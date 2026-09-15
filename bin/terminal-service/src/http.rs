//! Gateway-attested attachment to service-owned terminal sessions.
use crate::{
    runtime::Runtime,
    state::{Caller, Replay},
};
use axum::{
    Router,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio::sync::{Semaphore, watch};

pub struct Route {
    pub account: u64,
    pub label: String,
}
struct Service {
    route: Route,
    token: [u8; 64],
    runtime: Runtime,
    streams: Arc<Semaphore>,
}

pub fn router(route: Route, token: [u8; 64], runtime: Runtime) -> Result<Router, String> {
    if route.label.is_empty() {
        return Err("empty terminal route".into());
    }
    let service = Arc::new(Service {
        route,
        token,
        runtime,
        streams: Arc::new(Semaphore::new(64)),
    });
    Ok(Router::new()
        .route("/sessions/{session}", get(upgrade))
        .with_state(service))
}

fn caller(headers: &HeaderMap, token: &[u8; 64], config: &Route) -> Option<Caller> {
    let field = |name| {
        let mut values = headers.get_all(name).iter();
        let value = values.next()?.to_str().ok()?;
        values.next().is_none().then_some(value)
    };
    let presented = field("x-duck-upstream-token")?;
    let authenticated = bool::from(presented.as_bytes().ct_eq(token));
    if !authenticated {
        return None;
    }
    let route_account: u64 = field("x-duck-route-account")?.parse().ok()?;
    let revision: u64 = field("x-duck-route-revision")?.parse().ok()?;
    let installed_route = route_account == config.account
        && field("x-duck-route-label")? == config.label
        && revision > 0;
    if !installed_route {
        return None;
    }
    let account = field("x-duck-caller-account")?.parse().ok()?;
    let node_text = field("x-duck-caller-node")?;
    let mut node = [0; 32];
    let canonical_key = node_text.len() == 64
        && node_text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !canonical_key {
        return None;
    }
    for (out, digits) in node.iter_mut().zip(node_text.as_bytes().chunks_exact(2)) {
        *out = u8::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()?;
    }
    Some(Caller { account, node })
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    #[serde(default)]
    after: u64,
    #[serde(default)]
    after_command: u64,
}

async fn upgrade(
    State(service): State<Arc<Service>>,
    Path(session): Path<String>,
    Query(cursor): Query<Cursor>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    let owner = caller(&headers, &service.token, &service.route).ok_or(StatusCode::UNAUTHORIZED)?;
    let permit = service
        .streams
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    // Subscribe before the snapshot so output arriving during replay is observed.
    let changes = service.runtime.changes();
    let replay = service
        .runtime
        .replay(
            session.clone(),
            owner.clone(),
            cursor.after,
            cursor.after_command,
        )
        .await
        .map_err(|_| StatusCode::FORBIDDEN)?;
    Ok(ws
        .max_message_size(128 * 1024)
        .max_frame_size(128 * 1024)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            let _ = attached(socket, &service.runtime, session, owner, replay, changes).await;
        }))
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Input { data_b64: String },
    Resize { cols: u16, rows: u16 },
    Close,
}

async fn command(
    runtime: &Runtime,
    session: &str,
    owner: &Caller,
    bytes: &[u8],
) -> Result<(), String> {
    let command: Command = serde_json::from_slice(bytes).map_err(|_| "invalid terminal command")?;
    match command {
        Command::Input { data_b64 } => {
            runtime
                .input(
                    session.into(),
                    owner.clone(),
                    STANDARD
                        .decode(data_b64)
                        .map_err(|_| "invalid terminal input")?,
                )
                .await
        }
        Command::Resize { cols, rows } => {
            runtime
                .resize(session.into(), owner.clone(), cols, rows)
                .await
        }
        Command::Close => runtime.close(session.into(), owner.clone()).await,
    }
}

async fn send(socket: &mut WebSocket, value: Value) -> Result<(), ()> {
    tokio::time::timeout(
        Duration::from_secs(5),
        socket.send(Message::Text(value.to_string().into())),
    )
    .await
    .map_err(|_| ())?
    .map_err(|_| ())
}

// Replay heads are snapshot bounds. Clients resume from the last frames they
// consumed, not from these bounds before consuming the following frames.
async fn replay(socket: &mut WebSocket, snapshot: Replay) -> Result<Cursor, ()> {
    send(socket, json!({"event":"replay", "first":snapshot.first, "head":snapshot.head, "ended":snapshot.ended, "command_first":snapshot.command_first, "command_head":snapshot.command_head})).await?;
    for command in snapshot.commands {
        send(socket, json!({"event":"command", "seq":command.seq, "origin":command.origin, "text":command.text})).await?;
    }
    for chunk in snapshot.chunks {
        send(
            socket,
            json!({"event":"output", "seq":chunk.seq, "data_b64":STANDARD.encode(chunk.bytes)}),
        )
        .await?;
    }
    Ok(Cursor {
        after: snapshot.head,
        after_command: snapshot.command_head,
    })
}

async fn attached(
    mut socket: WebSocket,
    runtime: &Runtime,
    session: String,
    owner: Caller,
    initial: Replay,
    mut changes: watch::Receiver<()>,
) -> Result<(), ()> {
    let mut after = replay(&mut socket, initial).await?;
    loop {
        tokio::select! {
            changed = changes.changed() => {
                changed.map_err(|_| ())?;
                let snapshot = runtime.replay(session.clone(), owner.clone(), after.after, after.after_command).await.map_err(|_| ())?;
                after = replay(&mut socket, snapshot).await?;
            }
            message = socket.recv() => {
                let Some(Ok(message)) = message else { return Ok(()); };
                match message {
                    Message::Text(text) => {
                        let result = command(runtime, &session, &owner, text.as_bytes()).await;
                        send(&mut socket, json!({"event":"result", "result":result})).await?;
                    }
                    Message::Close(_) => return Ok(()),
                    Message::Ping(_) | Message::Pong(_) => {},
                    Message::Binary(_) => return Err(()),
                }
            }
        }
    }
}
