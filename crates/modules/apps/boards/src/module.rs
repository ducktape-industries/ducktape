use crate::*;
use sdk::{
    Ctx, Error, MerkleStore, Module, ModuleId, Msg, Origin, ResolverSyncTarget, StagedStore,
    StateRoot, StateSyncHandle,
};
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;

pub struct Boards {
    staged: StagedStore,
}
impl Boards {
    pub fn new(store: Box<dyn MerkleStore>) -> Self {
        Self {
            staged: StagedStore::new(store),
        }
    }
    async fn read<T: DeserializeOwned>(&self, key: &[u8]) -> Result<Option<T>, Error> {
        self.staged
            .get(key)
            .await?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|e| Error::Module(e.to_string())))
            .transpose()
    }
    async fn create(&mut self, id: String, title: String, owner: String) -> Result<(), Error> {
        if !valid_id(&id) {
            return Err(Error::Module("Invalid board id.".into()));
        }
        let mut catalog: BTreeMap<String, String> =
            self.read(b"catalog").await?.unwrap_or_default();
        if catalog.contains_key(&id) {
            let existing: Board = self
                .read(&board_key(&id))
                .await?
                .ok_or_else(|| Error::Module("Board catalog is inconsistent.".into()))?;
            let same_create = existing.owner == owner && existing.title == title;
            return if same_create {
                Ok(())
            } else {
                Err(Error::Module("Board id is already in use.".into()))
            };
        }
        if catalog.len() >= MAX_BOARDS {
            return Err(Error::Module("Board limit reached.".into()));
        }
        let board = Board::new(title.clone(), owner).map_err(Error::Module)?;
        catalog.insert(id.clone(), title);
        let writes = [
            (board_key(&id), sdk::wire::encode(&board)),
            (b"catalog".to_vec(), sdk::wire::encode(&catalog)),
        ];
        self.write(writes);
        Ok(())
    }
    async fn edit(&mut self, board: String, changes: Vec<Change>) -> Result<(), Error> {
        if !valid_id(&board) {
            return Err(Error::Module("Invalid board id.".into()));
        }
        let key = board_key(&board);
        let current: Board = self
            .read(&key)
            .await?
            .ok_or_else(|| Error::Module("Board no longer exists.".into()))?;
        let next = current.changed_many(&changes).map_err(Error::Module)?;
        self.write([(key, sdk::wire::encode(&next))]);
        Ok(())
    }
    fn write(&mut self, writes: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>) {
        for (key, value) in writes {
            self.staged.stage(key, value);
        }
    }
}
fn board_key(id: &str) -> Vec<u8> {
    format!("board/{id}").into_bytes()
}
fn actor(origin: &Origin) -> Result<String, Error> {
    match origin {
        Origin::External(key) => {
            let supported_key = matches!(key.len(), 32 | 33);
            if !supported_key {
                return Err(Error::Module("A signing key is required.".into()));
            }
            Ok(origin.actor_string())
        }
        Origin::Program(_) => Ok(origin.actor_string()),
        Origin::Module(_) | Origin::System => Err(Error::Module(
            "Boards require an authenticated user or program account.".into(),
        )),
    }
}
#[async_trait::async_trait(?Send)]
impl Module for Boards {
    fn id(&self) -> ModuleId {
        "boards".into()
    }
    fn root(&self) -> StateRoot {
        self.staged.root()
    }
    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        self.staged.state_sync_handle()
    }
    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.staged.serve_sync(req).await
    }
    async fn resolver_sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        self.staged.sync_target().await
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let owner = actor(&ctx.env().origin)?;
        let operation: Operation = sdk::wire::decode(&msg.payload).map_err(Error::Module)?;
        match operation {
            Operation::Create { id, title } => self.create(id, title, owner).await,
            Operation::Edit { board, change } => self.edit(board, vec![change]).await,
            Operation::Batch { board, changes } => self.edit(board, changes).await,
        }
    }
    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let query: Query = sdk::wire::decode(req).map_err(Error::Module)?;
        let reply = match query {
            Query::List => Reply::List(self.read(b"catalog").await?.unwrap_or_default()),
            Query::Get { id } => Reply::Board(self.read(&board_key(&id)).await?),
        };
        Ok(sdk::wire::encode(&reply))
    }
    async fn commit_block(&mut self) -> Result<(), Error> {
        self.staged.commit().await
    }
    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged.abort();
        Ok(())
    }
}
