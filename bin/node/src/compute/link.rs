//! the daemon's live ws attachment to its node: work-intake hints in, run
//! output out, on ONE connection.
//!
//! Two seams share it, because both are "this daemon and its node, continuously":
//!
//! - **hints.** The node sends a `heartbeat` frame on every block wake AND on a
//!   timer, so an unsubscribed connection is already the changed-hint feed —
//!   no topic, no cursor, no catch-up. Each hint wakes one intake pass; the
//!   daemon's own slower timer is the backstop that makes a dropped socket a
//!   DELAY rather than a stall.
//! - **run output.** The in-process `OutputSink` that used to feed the node's
//!   run-output ring cannot reach it across a process boundary; a `run_output`
//!   client frame is that sink, on the connection already open.
//!
//! A dropped socket is ordinary (the node restarts, the operator upgrades): the
//! task reconnects forever, logging attempt 1 and every Nth with an `attempts`
//! field. An unconditional warn here would be a log bomb on a node that stays
//! down — and the counter IS the diagnosis.

use futures::{SinkExt as _, StreamExt as _};
use tokio::sync::{Notify, mpsc};

/// one line of a run's live output, on its way to the node's ring.
pub(crate) struct OutputLine {
    pub(crate) run_key: String,
    pub(crate) stderr: bool,
    pub(crate) line: String,
}

/// how many reconnect failures pass between log lines after the first.
const LOG_EVERY: u64 = 30;
/// how long to wait before redialing a node that is not answering.
const REDIAL: std::time::Duration = std::time::Duration::from_secs(2);
/// the output lane's depth. Lines are display-only, so a burst that outruns the
/// socket is dropped at the sink rather than back-pressuring a provider's
/// stdout — a chatty run must never be able to stall its own execution.
pub(crate) const OUTPUT_LANE: usize = 1024;

/// Run the daemon's ws attachment until the process stops.
///
/// `hint` is notified once per node heartbeat; `lines` carries run output the
/// other way. Both are best-effort by design: the chain is the source of truth
/// for work, and the output ring is a display buffer.
pub(crate) async fn attach(
    ws_url: String,
    hint: std::sync::Arc<Notify>,
    mut lines: mpsc::Receiver<OutputLine>,
    mut records: mpsc::UnboundedReceiver<serde_json::Value>,
    token: String,
    resync: std::sync::Arc<Notify>,
) {
    let mut failures: u64 = 0;
    loop {
        match tokio_tungstenite::connect_async(&ws_url).await {
            Ok((mut socket, _)) => {
                let attach = serde_json::json!({"op":"compute_attach","token":token.trim()});
                if socket
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        attach.to_string(),
                    ))
                    .await
                    .is_err()
                {
                    tokio::time::sleep(REDIAL).await;
                    continue;
                }
                for (run, line) in provider_host::run_session::active() {
                    let frame = serde_json::json!({"op":"run_output","id":run,"stream":"stdout","line":line.to_string()});
                    if socket
                        .send(tokio_tungstenite::tungstenite::Message::Text(
                            frame.to_string(),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                if failures > 0 {
                    tracing::info!(
                        target: "ducktape::service",
                        "compute daemon reattached to its node"
                    );
                }
                failures = 0;
                tokio::select! {
                    _ = pump(socket, &hint, &mut lines, &mut records) => {},
                    _ = resync.notified() => {},
                }
                // a dropped link redials on the same pace as a failed dial:
                // pump can return immediately (the node restarting mid-accept),
                // and an unpaced success path dials at connect latency — the
                // storm that exhausted macOS's ephemeral port range.
                tokio::time::sleep(REDIAL).await;
            }
            Err(error) => {
                failures += 1;
                if failures == 1 || failures.is_multiple_of(LOG_EVERY) {
                    tracing::warn!(
                        target: "ducktape::service",
                        attempts = failures,
                        reason = "ws_attach_failed",
                        "compute daemon cannot reach its node's stream surface: {error}"
                    );
                }
                tokio::time::sleep(REDIAL).await;
            }
        }
    }
}

/// One connection's lifetime: hints out of it, output lines into it. Returns
/// when the socket closes, so the caller redials.
async fn pump<S>(
    socket: S,
    hint: &Notify,
    lines: &mut mpsc::Receiver<OutputLine>,
    records: &mut mpsc::UnboundedReceiver<serde_json::Value>,
) where
    S: futures::Sink<
            tokio_tungstenite::tungstenite::Message,
            Error = tokio_tungstenite::tungstenite::Error,
        > + futures::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use tokio_tungstenite::tungstenite::Message;
    let (mut tx, mut rx) = socket.split();
    let mut pending = futures::stream::FuturesUnordered::new();
    loop {
        tokio::select! {
            biased;
            record = records.recv() => {
                let Some(record) = record else { continue };
                let frame = match record["record"].as_str() {
                    Some("start") => serde_json::json!({
                        "op": "run_record_start",
                        "summary": record["summary"].clone(),
                    }),
                    Some("event") => serde_json::json!({
                        "op": "run_record_event",
                        "session_id": record["session_id"].clone(),
                        "event": record["event"].clone(),
                    }),
                    Some("snapshot") => serde_json::json!({
                        "op": "run_record_snapshot",
                        "summary": record["summary"].clone(),
                    }),
                    _ => continue,
                };
                if tx.send(Message::Text(frame.to_string())).await.is_err() {
                    return;
                }
            }
            frame = rx.next() => {
                let command = frame.as_ref().and_then(|frame| frame.as_ref().ok())
                    .and_then(|frame| frame.to_text().ok())
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                    .filter(|frame| frame["type"] == "run_control")
                    .and_then(|frame| serde_json::from_value::<noded::run_control::Command>(frame["command"].clone()).ok());
                if let Some(command) = command {
                    if pending.len() >= 64 {
                        let result: Result<serde_json::Value,String> = Err("Run control is busy.".into());
                        let frame = serde_json::json!({"op":"run_control_reply","id":command.id,"result":result});
                        if tx.send(Message::Text(frame.to_string())).await.is_err() { return; }
                        continue;
                    }
                    pending.push(async move {
                        let result = provider_host::run_session::control(&command.run,command.input).await;
                        (command.id,result)
                    });
                    continue;
                }
                if !read_frame(frame, hint) {
                    return;
                }
            }
            Some((id,result)) = pending.next() => {
                let frame = serde_json::json!({"op":"run_control_reply","id":id,"result":result});
                if tx.send(Message::Text(frame.to_string())).await.is_err() { return; }
            }
            line = lines.recv() => {
                // a closed lane ends the LANE, not the link: nothing holds a
                // sink (zero discovered providers drops every sender), but the
                // hint feed is still this daemon's work intake. Returning here
                // made every successful dial drop instantly — an unpaced
                // redial storm.
                let Some(line) = line else { break };
                let obsolete = serde_json::from_str::<serde_json::Value>(&line.line)
                    .ok()
                    .is_some_and(|event| !provider_host::run_session::current_control_event(&line.run_key, &event));
                if obsolete { continue; }
                let stream = if line.stderr { "stderr" } else { "stdout" };
                let frame = serde_json::json!({
                    "op": "run_output",
                    "id": line.run_key,
                    "stream": stream,
                    "line": line.line,
                })
                .to_string();
                if tx.send(Message::Text(frame)).await.is_err() {
                    return;
                }
            }
        }
    }
    // the output lane is closed for the daemon's lifetime; hints in, nothing
    // out, until the socket itself ends.
    while read_frame(rx.next().await, hint) {}
}

/// One server frame: forward a heartbeat hint, ignore everything else — the
/// hub's other frames are for subscribers, and this connection subscribes to
/// nothing. False when the socket is finished (a close or an error).
fn read_frame(
    frame: Option<
        Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>,
    >,
    hint: &Notify,
) -> bool {
    use tokio_tungstenite::tungstenite::Message;
    let Some(Ok(Message::Text(text))) = frame else {
        // a frame shape we do not read is a live socket; a close or an error
        // is not.
        return matches!(frame, Some(Ok(_)));
    };
    let is_hint = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| value["type"].as_str().map(|kind| kind == "heartbeat"))
        .unwrap_or(false);
    if is_hint {
        hint.notify_one();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::Role;

    #[tokio::test]
    async fn buffered_ready_for_a_closed_run_does_not_follow_a_reconnect_snapshot() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        let (sender, mut lines) = mpsc::channel(2);
        let (_record_sender, mut records) = mpsc::unbounded_channel();
        sender.send(OutputLine { run_key: "already-closed".into(), stderr: false, line: serde_json::json!({"type":"run_control","state":"ready","turn":"old","steers":true}).to_string() }).await.unwrap();
        sender
            .send(OutputLine {
                run_key: "already-closed".into(),
                stderr: false,
                line: "retained output".into(),
            })
            .await
            .unwrap();
        let task =
            tokio::spawn(
                async move { pump(client, &Notify::new(), &mut lines, &mut records).await },
            );
        let frame = server.next().await.unwrap().unwrap().into_text().unwrap();
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["line"], "retained output");
        server.close(None).await.unwrap();
        drop(server);
        task.await.unwrap();
    }

    /// the macOS bring-up storm: zero discovered providers drop every
    /// `OutputLine` sender before the link is even up, and a pump that treated
    /// the closed lane as a dead socket turned every successful dial into an
    /// instant drop — an unpaced redial loop that exhausted the ephemeral port
    /// range. The lane's end must leave the hint feed running until the socket
    /// itself closes.
    #[tokio::test]
    async fn a_closed_output_lane_keeps_the_link_alive() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;

        let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<OutputLine>(4);
        drop(line_tx);
        let (_record_tx, mut record_rx) = tokio::sync::mpsc::unbounded_channel();
        let hint = std::sync::Arc::new(Notify::new());
        let mut pumping = tokio::spawn({
            let hint = hint.clone();
            async move { pump(client, &hint, &mut line_rx, &mut record_rx).await }
        });

        // the lane was closed before the first frame; a heartbeat must still
        // come through as a hint.
        server
            .send(Message::Text(r#"{"type":"heartbeat","height":1}"#.into()))
            .await
            .expect("the server writes into a live socket");
        tokio::select! {
            () = hint.notified() => {}
            end = &mut pumping => panic!("pump ended on a closed output lane: {end:?}"),
        }

        // the socket closing is what ends the pump.
        server
            .close(None)
            .await
            .expect("the server closes its side");
        drop(server);
        pumping.await.expect("pump returns when the socket ends");
    }

    #[tokio::test]
    async fn durable_records_are_forwarded_before_display_output() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        let (line_sender, mut lines) = mpsc::channel(2);
        let (record_sender, mut records) = mpsc::unbounded_channel();
        record_sender
            .send(serde_json::json!({
                "record": "start",
                "summary": {"session_id": "session-a"}
            }))
            .unwrap();
        line_sender
            .send(OutputLine {
                run_key: "a".repeat(64),
                stderr: false,
                line: "display output".into(),
            })
            .await
            .unwrap();
        let task =
            tokio::spawn(
                async move { pump(client, &Notify::new(), &mut lines, &mut records).await },
            );
        let first: serde_json::Value =
            serde_json::from_str(&server.next().await.unwrap().unwrap().into_text().unwrap())
                .unwrap();
        assert_eq!(first["op"], "run_record_start");
        let second: serde_json::Value =
            serde_json::from_str(&server.next().await.unwrap().unwrap().into_text().unwrap())
                .unwrap();
        assert_eq!(second["op"], "run_output");
        server.close(None).await.unwrap();
        drop(server);
        task.await.unwrap();
    }
}
