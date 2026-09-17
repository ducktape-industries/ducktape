// ============================================================================
// the local rpc: json-lines over tcp, bridged from blocking threads.
// ============================================================================

/// one rpc request, parsed from a json line.
#[derive(serde::Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub(crate) enum RpcRequest {
    /// submit an op into the ordered lane (accepted != finalized — poll status).
    Submit { target: String, payload_hex: String },
    /// read-only query against a module's committed+staged projection.
    Query { target: String, req_hex: String },
    /// node status: latest applied boundary + every module root.
    Status,
    /// the verified join requests parked joiners announced to THIS member —
    /// the queue the approve button (or `node resident accept`) settles.
    JoinRequests,
    /// the node-owned join state: the ONE authoritative source the
    /// app renders instead of parsing daemon.log markers. derived from gate
    /// progress + committed chain state, never a scattered guess.
    JoinState,
    /// the direct-peer sample: mesh-tracked connections plus per-peer
    /// traffic counters and statesync progression (see [`noded::peers`]).
    Peers,
    /// graceful stop: replies ok, then exits 0 after the current pump turn.
    Shutdown,
}

/// the node-owned join-state projection. `phase` uses the app's
/// onboarding vocabulary so the console renders it verbatim:
/// `parked | admitted | synced | promoted`.
#[derive(serde::Serialize)]
pub(crate) struct JoinStateView {
    pub(crate) phase: String,
    pub(crate) detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) height: Option<u64>,
}

/// one verified, unapproved join announce (node-local, in-memory; the parked
/// joiner re-announces every few seconds, so nothing here is durable state).
pub(crate) struct JoinRequestRecord {
    pub(crate) issuer: Vec<u8>,
    pub(crate) first_seen_ms: u64,
    pub(crate) last_seen_ms: u64,
}

/// Insert a freshly-forwarded join request, or refresh `last_seen_ms` on a
/// retransmit — mirrors [`crate::reachability_plane::insert_gate_outcome`],
/// capped at the same [`crate::reachability_plane::MAX_TRACKED_JOINERS`],
/// oldest-by-`last_seen_ms` evicted first. The map is keyed on the
/// attacker-chosen joiner key with no other size limit, so an unbounded
/// stream of never-approved joiners must not grow it forever.
pub(crate) fn insert_join_request(
    map: &mut std::collections::BTreeMap<Vec<u8>, JoinRequestRecord>,
    joiner: Vec<u8>,
    issuer: Vec<u8>,
    now_ms: u64,
) {
    if let Some(existing) = map.get_mut(&joiner) {
        existing.last_seen_ms = now_ms;
        return;
    }
    if map.len() >= crate::reachability_plane::MAX_TRACKED_JOINERS
        && let Some(oldest) = map
            .iter()
            .min_by_key(|(_, r)| r.last_seen_ms)
            .map(|(k, _)| k.clone())
    {
        map.remove(&oldest);
    }
    map.insert(
        joiner,
        JoinRequestRecord {
            issuer,
            first_seen_ms: now_ms,
            last_seen_ms: now_ms,
        },
    );
}

/// Sweep every join request last seen more than `window_ms` ago — mirrors
/// [`crate::reachability_plane::sweep_gate_outcomes`]. The only other prune
/// is `on_rpc`'s read-time retain for a joiner that GAINED standing, so a
/// joiner whose Redeem was rejected (and never retries) would otherwise sit
/// here forever.
pub(crate) fn sweep_join_requests(
    map: &mut std::collections::BTreeMap<Vec<u8>, JoinRequestRecord>,
    now_ms: u64,
    window_ms: u64,
) {
    map.retain(|_, r| now_ms.saturating_sub(r.last_seen_ms) <= window_ms);
}

/// the rpc/console projection of one [`JoinRequestRecord`].
#[derive(serde::Serialize)]
pub(crate) struct JoinRequestView {
    /// the key asking to join, hex.
    pub(crate) joiner: String,
    /// the member whose invite token authorized the announce, hex.
    pub(crate) issuer: String,
    pub(crate) first_seen_ms: u64,
    pub(crate) last_seen_ms: u64,
}

#[derive(serde::Serialize)]
pub(crate) struct RpcReply {
    pub(crate) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reply_hex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<RpcStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) join_requests: Option<Vec<JoinRequestView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) join_state: Option<JoinStateView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) peers: Option<noded::peers::PeersView>,
}

#[derive(serde::Serialize)]
pub(crate) struct RpcStatus {
    pub(crate) height: Option<u64>,
    pub(crate) root_hash: String,
    pub(crate) modules: std::collections::BTreeMap<String, String>,
    /// the WHOLE operations projection `/v1/status` serves, not a chosen slice
    /// of it. `node status` is the verb the getting-started screen points an
    /// operator at, and a slice is what let it print two numbers that are
    /// identical on a healthy chain and a halted one. The node computes this
    /// either way; carrying all of it costs one clone and leaves nothing for
    /// the next question to have to re-plumb.
    pub(crate) operations: noded::OperationalStatus,
}

impl RpcReply {
    pub(crate) fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            reply_hex: None,
            status: None,
            join_requests: None,
            join_state: None,
            peers: None,
        }
    }
    pub(crate) fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            reply_hex: None,
            status: None,
            join_requests: None,
            join_state: None,
            peers: None,
        }
    }
}

/// a parsed request plus the blocking thread's reply slot.
pub(crate) struct RpcJob {
    pub(crate) req: RpcRequest,
    pub(crate) reply: std::sync::mpsc::Sender<RpcReply>,
    /// resolves once the connection thread has WRITTEN this job's reply to the
    /// socket — `reply.send` only hands it over. The shutdown arm waits on this
    /// before `process::exit`, or the exit races the write and the caller sees
    /// its connection close with no reply line. It also resolves (as an `Err`)
    /// when the connection thread is gone, so a dead client cannot park the
    /// exit.
    pub(crate) written: futures::channel::oneshot::Receiver<()>,
}

/// serve json-lines rpc on `listener`, one OS thread per connection (local,
/// low-volume — an operator console, a script). each line becomes an [`RpcJob`]
/// pushed into the pump's bounded queue; the pump answers between drains, so
/// every reply reflects a block boundary. this runs on PLAIN OS THREADS: it
/// must never touch the async runtime, only the mpsc bridge.
pub(crate) fn spawn_rpc_listener(
    listener: std::net::TcpListener,
    bridge: futures::channel::mpsc::Sender<RpcJob>,
) {
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let mut bridge = bridge.clone();
            std::thread::spawn(move || {
                use std::io::{BufRead as _, BufReader, Write as _};
                let reader = BufReader::new(conn.try_clone().expect("clone rpc conn"));
                let mut conn = conn;
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if line.trim().is_empty() {
                        continue;
                    }
                    // handed to the pump with the job and signalled after the
                    // write below: a shutdown must not exit before that.
                    let (written, written_rx) = futures::channel::oneshot::channel();
                    let reply = match serde_json::from_str::<RpcRequest>(&line) {
                        Ok(req) => {
                            let (tx, rx) = std::sync::mpsc::channel();
                            if bridge
                                .try_send(RpcJob {
                                    req,
                                    reply: tx,
                                    written: written_rx,
                                })
                                .is_err()
                            {
                                RpcReply::err("node busy (rpc queue full)")
                            } else {
                                // the pump answers within a tick; a stuck node
                                // must not park the operator's console forever.
                                rx.recv_timeout(std::time::Duration::from_secs(10))
                                    .unwrap_or_else(|_| RpcReply::err("node unresponsive"))
                            }
                        }
                        Err(e) => RpcReply::err(format!("bad request: {e}")),
                    };
                    let mut out = serde_json::to_string(&reply).expect("reply serializes");
                    out.push('\n');
                    if conn.write_all(out.as_bytes()).is_err() {
                        break;
                    }
                    let _ = written.send(());
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;
    use std::io::{Read as _, Write as _};

    /// The shutdown arm exits the process the moment `written` resolves, so
    /// that signal has to mean "the reply is on the socket" and not "the reply
    /// is queued for the connection thread" — the race this pins cost a
    /// `connection closed before a reply line` on the caller.
    #[test]
    fn a_job_reports_written_only_after_the_reply_reaches_the_socket() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind rpc listener");
        let addr = listener.local_addr().expect("listener address");
        let (jobs, mut pump) = futures::channel::mpsc::channel(1);
        spawn_rpc_listener(listener, jobs);

        let mut client = std::net::TcpStream::connect(addr).expect("dial the rpc listener");
        client
            .write_all(b"{\"cmd\":\"shutdown\"}\n")
            .expect("send the request");

        let job = futures::executor::block_on(pump.next()).expect("the listener pushed the job");
        assert!(matches!(job.req, RpcRequest::Shutdown));
        job.reply
            .send(RpcReply::ok())
            .expect("the conn thread waits");
        futures::executor::block_on(job.written).expect("the conn thread reports the write");

        // where the pump would call `process::exit`: the line is already
        // readable, with nothing left to wait for.
        client.set_nonblocking(true).expect("nonblocking client");
        let mut buf = [0u8; 128];
        let read = client
            .read(&mut buf)
            .expect("the reply is already on the socket");
        let reply: serde_json::Value =
            serde_json::from_slice(&buf[..read]).expect("one json reply line");
        assert_eq!(reply["ok"], serde_json::Value::Bool(true));
    }
}
