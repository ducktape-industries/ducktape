//! Raw operations received for one commitment, checked against its target before adoption.
use commonware_cryptography::sha256::Digest;
use commonware_runtime::Spawner;
use commonware_storage::{
    Context,
    journal::contiguous::{Contiguous as _, Many, variable},
    qmdb::{self, sync::Database as _},
};

use crate::{Commitment, Db, Error, Family, Op, Result, SyncTarget, Tuning};

pub struct Receiving<E: Context + Spawner> {
    context: E,
    name: String,
    target: SyncTarget,
    log: variable::Journal<E, Op>,
}

impl<E: Context + Spawner> Receiving<E> {
    pub(crate) async fn open(context: E, name: &str, target: SyncTarget) -> Result<Self> {
        let config = crate::commitment::config(&context, name, 0);
        let log = <variable::Journal<E, Op> as qmdb::sync::Journal<Family>>::new(
            context.child("log"),
            config.journal_config,
            target.range.clone(),
        )
        .await
        .map_err(qmdb::Error::<Family>::from)?;
        Ok(Self {
            context,
            name: name.to_owned(),
            target,
            log,
        })
    }

    pub fn size(&self) -> u64 {
        self.log.bounds().end
    }

    pub async fn append(mut self, ops: &[Op]) -> Result<Self> {
        let from = self.size();
        let count = ops.len() as u64;
        let start = *self.target.range.start();
        let end = *self.target.range.end();
        let outside = count == 0 || count > end.saturating_sub(from);
        if outside {
            return Err(Error::SyncRange {
                from,
                count,
                start,
                end,
            });
        }
        let (log, _) = self
            .log
            .append_many(Many::Flat(ops))
            .await
            .map_err(qmdb::Error::<Family>::from)?;
        self.log = log;
        Ok(self)
    }

    pub async fn sync(mut self) -> Result<Self> {
        self.log = self.log.sync().await.map_err(qmdb::Error::<Family>::from)?;
        Ok(self)
    }

    pub async fn finish(self, pinned: Vec<Digest>) -> Result<Commitment<E>> {
        let received = self.size();
        let end = *self.target.range.end();
        if received != end {
            return Err(Error::IncompleteSync { received, end });
        }
        let config = crate::commitment::config(&self.context, &self.name, 0);
        let log = self.log.sync().await.map_err(qmdb::Error::<Family>::from)?;
        let db = Db::<E>::from_sync_result(
            self.context,
            config,
            log,
            Some(pinned),
            self.target.range,
            Tuning::default().sync_apply_batch,
        )
        .await?;
        let actual = abi::Root(db.root().0);
        let expected = abi::Root(self.target.root.0);
        if actual != expected {
            return Err(Error::RootMismatch { expected, actual });
        }
        Commitment::received(self.name, db).await
    }
}
