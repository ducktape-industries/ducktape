use boards_wire::{Board, Operation, Query, Reply};
use ducktape_view_guest::{Subscription, host};
use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Session {
    pub connected: bool,
    pub dark: bool,
    #[serde(default)]
    pub chain: String,
}
#[derive(Clone, Debug)]
pub struct Reading {
    pub catalog: BTreeMap<String, String>,
    pub board: Option<Board>,
}
pub fn session() -> Subscription<Result<Session, String>> {
    Subscription::run(|| {
        host::subscribe("canvas.props", &[]).map(|answer| {
            answer
                .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|error| error.to_string()))
        })
    })
}
async fn query(query: Query) -> Result<Reply, String> {
    let request = serde_json::json!({ "target": "boards", "query": query });
    let bytes = host::request("rpc.query", &serde_json::to_vec(&request).unwrap()).await?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}
pub async fn read(id: &str) -> Result<Reading, String> {
    let Reply::List(catalog) = query(Query::List).await? else {
        return Err("Unexpected board list reply.".into());
    };
    let board = if id.is_empty() {
        None
    } else {
        let Reply::Board(board) = query(Query::Get { id: id.into() }).await? else {
            return Err("Unexpected board reply.".into());
        };
        board
    };
    Ok(Reading { catalog, board })
}
pub fn watch(id: String, epoch: u64) -> Subscription<(u64, String, Result<Reading, String>)> {
    Subscription::run_with((id, epoch), |(id, epoch)| {
        let id = id.clone();
        let epoch = *epoch;
        // Establish the resumable feed before the first read, as Pages does.
        let live = host::subscribe("rpc.live", b"boards");
        stream::once(std::future::ready(Ok(Vec::new())))
            .chain(live)
            .then(move |hit| {
                let id = id.clone();
                async move {
                    let result = match hit {
                        Ok(_) => read(&id).await,
                        Err(error) => Err(error),
                    };
                    (epoch, id, result)
                }
            })
    })
}
pub async fn mint() -> Result<String, String> {
    let bytes = host::request("host.id", b"board").await?;
    String::from_utf8(bytes).map_err(|error| error.to_string())
}
pub async fn submit(operation: Operation) -> Result<(), String> {
    let request = serde_json::json!({ "target": "boards", "payload": operation });
    host::request("op.submit", &serde_json::to_vec(&request).unwrap()).await?;
    Ok(())
}
