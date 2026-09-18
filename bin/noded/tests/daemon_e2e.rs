//! daemon e2e: a REAL spawned `ducktape-noded` process driven over its actual
//! http/ws surface — the seam every app build (web, desktop sidecar) dials.
//! `tests/router.rs` covers the axum wiring against a FAKE actor; this suite
//! is the other half: real genesis, real `Host::submit_at` blocks, real
//! broadcast fan-out, real storage persistence across a restart.
//!
//! transport is deliberately raw std-TCP http/1.1 (plus a minimal ws client):
//! the daemon's whole point is that ANY plain http client is a full citizen —
//! if this file needs a feature a hand-rolled client can't express, the wire
//! has drifted from that promise.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest as _, Sha256};

/// a running daemon, killed on drop so failures never leak an orphan (the
/// REAL orphan lifecycle — outliving a client — is the desktop shell's
/// contract with a detached spawn; this harness owns its child instead).
struct Daemon {
    child: Child,
    port: u16,
    /// the operator credential the daemon minted 0600 into its storage root.
    /// `/v1/admin/*` requires it: loopback presence is not authority.
    admin_token: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    /// every spawn gets an EXPLICIT storage dir: the daemon's default is
    /// temp_dir()/ducktape-noded-{pid}, which the process never cleans up —
    /// a leaked dir plus a recycled pid would reopen stale qmdb state and
    /// fail this suite spuriously.
    fn spawn(storage: &Path) -> Self {
        let port = free_port();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ducktape-noded"));
        cmd.arg("--listen")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--storage")
            .arg(storage)
            // the daemon composes its wasm tenants and index guests from a
            // founding set; this suite names the one the build staged beside
            // the test executable, so an operator's $DUCKTAPE_MODULES_DIR
            // cannot redirect it.
            .arg("--modules")
            .arg(
                workspace_config::modules_dir(noded::services::STAGED_SET)
                    .expect("cargo build stages the founding set beside the test executable"),
            )
            .stdout(Stdio::null())
            // startup failures (port stolen in the free_port window, bad
            // storage) land on stderr — keep it visible or they read as an
            // opaque readiness timeout.
            .stderr(Stdio::inherit());
        let child = cmd.spawn().expect("spawn ducktape-noded");
        let mut daemon = Self {
            child,
            port,
            admin_token: String::new(),
        };
        // readiness = a status answer, never the listen line: the daemon binds
        // its listener only AFTER the node actor publishes its boot snapshot,
        // so the first answer is genesis (or the resumed height) — never the
        // empty default.
        daemon.await_status();
        // the credential is written before the listener binds, so a daemon that
        // answers /v1/status has already minted it.
        daemon.admin_token =
            noded::admin::read_operator_token(storage).expect("daemon minted an operator token");
        daemon
    }

    /// a duckfs transport whose writes this daemon admits.
    fn files(&self) -> duckfs_client::http::HttpNode {
        use commonware_cryptography::{Signer as _, ed25519};
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let signer = ed25519::PrivateKey::from_seed(700);
        duckfs_client::http::HttpNode::new(format!("http://127.0.0.1:{}", self.port))
            .with_frame_signer(std::sync::Arc::new(move |target, payload| {
                node::encode_frame(
                    &signer,
                    SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                    &sdk::Msg {
                        target: target.into(),
                        payload,
                    },
                )
            }))
    }

    /// POST the graceful-exit route with this daemon's operator credential —
    /// the ONLY thing that may drive it.
    fn admin_shutdown(&self) -> Option<u16> {
        nettest::http_status_with(
            self.port,
            "POST",
            "/v1/admin/shutdown",
            &[(noded::admin::ADMIN_TOKEN_HEADER, &self.admin_token)],
        )
    }

    fn await_status(&mut self) {
        // generous because genesis COMPILES: every tenant is a wasm component
        // the daemon cranelift-compiles at boot, and `cargo test` runs this
        // suite's daemons in parallel — one per test, each compiling the whole
        // set at once. The bound is only a hang-catcher; the loop below exits
        // on the daemon's own readiness answer, never on the clock.
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            // liveness BEFORE the probe, never after. If our child lost a race
            // for the port and exited, something else is listening on it — and
            // a probe-first loop would take that stranger's 200 as our own
            // readiness and silently drive ANOTHER test's daemon for the whole
            // test. That failure surfaces far downstream as an impossible
            // assertion (a height that moved on a daemon we never submitted
            // to), which is exactly the kind of ghost that costs a day.
            if let Some(status) = self.child.try_wait().expect("poll daemon") {
                panic!(
                    "daemon for port {} exited during startup ({status}) — see stderr above. \
                     If something still answers that port, it is NOT ours.",
                    self.port
                );
            }
            if let Ok((200, _)) = self.try_request("GET", "/v1/status", None) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "daemon on port {} never answered /v1/status",
                self.port
            );
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    fn try_request(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> std::io::Result<(u16, serde_json::Value)> {
        let bytes = body
            .map(|b| serde_json::to_vec(b).expect("request body serializes"))
            .unwrap_or_default();
        let (status, raw) = self.try_bytes(method, path, "application/json", &bytes)?;
        Ok((
            status,
            serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null),
        ))
    }

    /// every request this harness makes carries the daemon's own operator
    /// credential. The harness OWNS this daemon, so it is the local operator
    /// the credential names — and every mutating `/v1` route now refuses a
    /// caller holding neither it nor a user signature.
    fn try_bytes(
        &self,
        method: &str,
        path: &str,
        content_type: &str,
        body: &[u8],
    ) -> std::io::Result<(u16, Vec<u8>)> {
        nettest::try_http_bytes_with(
            self.port,
            method,
            path,
            content_type,
            &[(noded::admin::ADMIN_TOKEN_HEADER, &self.admin_token)],
            body,
        )
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        self.try_request(method, path, body)
            .expect("daemon reachable")
    }

    fn submit(
        &self,
        target: &str,
        payload: serde_json::Value,
        origin: Option<&str>,
    ) -> (u16, serde_json::Value) {
        self.request(
            "POST",
            "/v1/submit",
            Some(&serde_json::json!({
                "target": target,
                "payload": payload,
                "origin": origin,
            })),
        )
    }

    /// POST a module view and return `(status, x-ducktape-folded, reply)` —
    /// the fold watermark rides a response HEADER, so the json helper alone
    /// cannot see the half of this route's contract that answers "is my op in
    /// this snapshot".
    fn view(
        &self,
        module: &str,
        query: serde_json::Value,
    ) -> (u16, Option<String>, serde_json::Value) {
        let body = serde_json::to_vec(&query).expect("view query serializes");
        let (status, head, reply) = nettest::try_http_headed(
            self.port,
            "POST",
            &format!("/v1/index/{module}/view"),
            "application/json",
            &[(noded::admin::ADMIN_TOKEN_HEADER, &self.admin_token)],
            &body,
        )
        .expect("daemon reachable");
        (
            status,
            nettest::header_of(&head, "x-ducktape-folded"),
            serde_json::from_slice(&reply).unwrap_or(serde_json::Value::Null),
        )
    }

    fn query(&self, target: &str, query: serde_json::Value) -> serde_json::Value {
        let (status, reply) = self.request(
            "POST",
            "/v1/query",
            Some(&serde_json::json!({ "target": target, "query": query })),
        );
        assert_eq!(status, 200, "query {target} failed: {reply}");
        reply
    }

    fn status(&self) -> serde_json::Value {
        let (status, reply) = self.request("GET", "/v1/status", None);
        assert_eq!(status, 200, "status failed: {reply}");
        reply
    }

    /// GET /metrics as raw OpenMetrics text (not json — the scrape body is a
    /// text exposition, so reuse the byte lane and utf-8 decode it).
    fn metrics(&self) -> String {
        let (status, body) = self.request_bytes("GET", "/metrics", &[]);
        assert_eq!(status, 200, "metrics failed");
        String::from_utf8(body).expect("metrics body is utf-8")
    }

    /// raw-byte request for the blob lane: returns status + the response body
    /// BYTES exactly as received. the json helpers above lossy-decode the
    /// whole response as utf-8, which would corrupt binary chunk bodies.
    fn request_bytes(&self, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
        self.try_bytes(method, path, "application/octet-stream", body)
            .expect("daemon reachable")
    }

    /// open /v1/ws with a minimal rfc6455 client handshake and return the
    /// stream positioned after the 101 response.
    fn ws_connect(&self) -> BufReader<TcpStream> {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).expect("ws connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("ws read timeout");
        let mut stream = stream;
        let req = "GET /v1/ws HTTP/1.1\r\nhost: 127.0.0.1\r\nupgrade: websocket\r\nconnection: upgrade\r\nsec-websocket-key: ZHVja3RhcGUtZTJlLXdzLWtleQ==\r\nsec-websocket-version: 13\r\n\r\n".to_string();
        stream
            .write_all(req.as_bytes())
            .expect("ws handshake write");
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("ws handshake status");
        assert!(line.contains("101"), "ws upgrade rejected: {line}");
        // drain the rest of the handshake headers.
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).expect("ws handshake header");
            if header == "\r\n" {
                break;
            }
        }
        reader
    }

    /// read one server->client text frame (unfragmented, unmasked — what the
    /// daemon sends for stream frames).
    fn ws_read_text(reader: &mut BufReader<TcpStream>) -> String {
        let mut head = [0u8; 2];
        reader.read_exact(&mut head).expect("ws frame head");
        assert_eq!(head[0] & 0x0f, 0x1, "expected a text frame");
        let mut len = (head[1] & 0x7f) as u64;
        if len == 126 {
            let mut ext = [0u8; 2];
            reader.read_exact(&mut ext).expect("ws extended len");
            len = u16::from_be_bytes(ext) as u64;
        } else if len == 127 {
            let mut ext = [0u8; 8];
            reader.read_exact(&mut ext).expect("ws extended len");
            len = u64::from_be_bytes(ext);
        }
        let mut payload = vec![0u8; len as usize];
        reader.read_exact(&mut payload).expect("ws frame payload");
        String::from_utf8(payload).expect("ws text frame is utf-8")
    }

    /// send one client->server text frame. RFC6455 requires client frames to
    /// be masked; the static key is fine for a deterministic test client.
    fn ws_send_text(reader: &mut BufReader<TcpStream>, text: &str) {
        let payload = text.as_bytes();
        let mut frame = Vec::new();
        frame.push(0x81);
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else if payload.len() <= u16::MAX as usize {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        let mask = [0x11, 0x22, 0x33, 0x44];
        frame.extend_from_slice(&mask);
        for (i, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[i % mask.len()]);
        }
        reader.get_mut().write_all(&frame).expect("ws frame write");
    }

    fn ws_read_json(reader: &mut BufReader<TcpStream>) -> serde_json::Value {
        serde_json::from_str(&Self::ws_read_text(reader)).expect("ws frame json")
    }

    fn ws_read_type(reader: &mut BufReader<TcpStream>, want: &str) -> serde_json::Value {
        loop {
            let frame = Self::ws_read_json(reader);
            if frame["type"] == want {
                return frame;
            }
        }
    }

    /// a websocket-upgrade GET that expects an http REFUSAL, not a 101: sends
    /// the full rfc6455 handshake so the request reaches the handler body
    /// (axum's extractor stops a plain GET before the handler can say why),
    /// then returns whatever status + raw response text the daemon answers.
    /// a refusal leaves the connection open (keep-alive), so the read is
    /// timeout-bounded instead of read-to-close.
    fn ws_upgrade_refusal(&self, path: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).expect("daemon reachable");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        let req = format!(
            "GET {path} HTTP/1.1\r\nhost: 127.0.0.1\r\nupgrade: websocket\r\nconnection: upgrade\r\nsec-websocket-key: ZHVja3RhcGUtZTJlLXdzLWtleQ==\r\nsec-websocket-version: 13\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).expect("write request");
        let mut raw = Vec::new();
        // read_to_end keeps what arrived before the timeout error — exactly
        // the refusal head + body on a connection the server holds open.
        let _ = stream.read_to_end(&mut raw);
        let text = String::from_utf8_lossy(&raw).into_owned();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, text)
    }
}

use nettest::free_port;

fn post_message(channel: &str, message_id: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "post_message": {
            "channel_id": channel,
            "message_id": message_id,
            "blocks": [{ "paragraph": [{ "text": text, "marks": [] }] }],
            "thread": null,
        }
    })
}

fn post_mention(channel: &str, message_id: &str, account: u64) -> serde_json::Value {
    serde_json::json!({
        "post_message": {
            "channel_id": channel,
            "message_id": message_id,
            "blocks": [{
                "paragraph": [
                    { "text": "hey ", "marks": [] },
                    {
                        "text": format!("@{account}"),
                        "marks": [{
                            "mention": {
                                "account": account
                            }
                        }]
                    },
                    { "text": " can you handle this?", "marks": [] }
                ]
            }],
            "thread": null,
        }
    })
}

/// an INCOMPLETE modules dir is refused at argv time, naming the component it
/// could not read plus the remedy. `main` opens the code source itself, so this
/// is the ONE completeness decision — before a storage root exists, and never a
/// stack trace from the actor thread.
#[test]
fn an_incomplete_modules_dir_is_refused_before_boot() {
    let storage = tempfile::TempDir::new().expect("storage dir");
    // a directory that EXISTS and holds no components: a hand-assembled
    // --modules, or a staged set a `make wasm-modules` left half-refreshed.
    let modules = tempfile::TempDir::new().expect("modules dir");

    let out = Command::new(env!("CARGO_BIN_EXE_ducktape-noded"))
        .arg("--storage")
        .arg(storage.path())
        .arg("--modules")
        .arg(modules.path())
        .output()
        .expect("spawn ducktape-noded");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "an empty modules dir composes no genesis: {stderr}"
    );
    // a path INSIDE the dir, i.e. the component it could not read — never just
    // the directory. WHICH id comes first is `hash_bundle`'s sorted-by-id walk
    // and not this test's business.
    let names_a_component = format!("{}{}", modules.path().display(), std::path::MAIN_SEPARATOR);
    assert!(
        stderr.contains(&names_a_component),
        "refusal names the component it could not read: {stderr}"
    );
    assert!(
        stderr.contains("`cargo build` stages the founding set"),
        "refusal carries the remedy: {stderr}"
    );
}

/// Calls are served by a published application process; Pages presence
/// requires the node's overlay runtime.
#[test]
fn media_product_route_is_absent_and_presence_without_overlay_refuses() {
    let storage = tempfile::TempDir::new().expect("storage dir");
    let daemon = Daemon::spawn(storage.path());
    let (status, _) = daemon.ws_upgrade_refusal("/v1/call/ws?channel=general");
    assert_eq!(status, 404);

    let (status, raw) = daemon.ws_upgrade_refusal("/v1/presence/ws?page=page-1");
    assert_eq!(status, 503, "no realtime hub → presence refused: {raw}");
    assert!(
        raw.contains("no mesh realtime hub"),
        "refusal says WHY: {raw}"
    );

    let (status, _raw) = daemon.ws_upgrade_refusal("/v1/voice/ws?channel=general");
    assert_eq!(status, 404, "the old voice route is unrouted, not refused");
}

#[test]
fn full_surface_blocks_authorship_and_ws() {
    let storage = tempfile::TempDir::new().expect("storage dir");
    let daemon = Daemon::spawn(storage.path());

    // status at genesis: build version, height 0, every registered module
    // root — the host's live set, sorted by id (a registry admission joins
    // it later, so no selection order could list it).
    let status = daemon.status();
    assert_eq!(status["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(status["height"], 0);
    let modules: Vec<&str> = status["modules"]
        .as_array()
        .expect("modules array")
        .iter()
        .map(|m| m["id"].as_str().expect("module id"))
        .collect();
    let mut genesis_set = topology::SIM_BASE.to_vec();
    genesis_set.sort_unstable();
    assert_eq!(modules, genesis_set);
    let genesis_hash = status["root_hash"].as_str().expect("root_hash").to_string();

    // connect before submitting: the stream heartbeats without a subscription,
    // then module events catch up from the subscribed cursor.
    let mut ws = daemon.ws_connect();
    let heartbeat = Daemon::ws_read_type(&mut ws, "heartbeat");
    assert_eq!(heartbeat["height"], 0);
    assert_eq!(heartbeat["interval_ms"], 3_000);

    Daemon::ws_send_text(&mut ws, r#"{"op":"subscribe","topics":["module:chat"]}"#);
    let subscribed = Daemon::ws_read_type(&mut ws, "subscribed");
    assert_eq!(
        subscribed["topics"]["module:chat"],
        "op/0000000000000000/ffffffff"
    );

    // one msg = one block; the summary echoes the new height + root-hash.
    let (code, block) = daemon.submit(
        "chat",
        serde_json::json!({
            "create_channel": { "channel_id": "general", "name": "General", "post_policy": "open" }
        }),
        None,
    );
    assert_eq!(code, 200, "create channel failed: {block}");
    assert_eq!(block["height"], 1);
    assert_ne!(block["root_hash"].as_str(), Some(genesis_hash.as_str()));

    let (code, block) = daemon.submit(
        "chat",
        post_message("general", "m1", "hello from e2e"),
        Some("alice"),
    );
    assert_eq!(code, 200, "post failed: {block}");
    assert_eq!(block["height"], 2);

    // the ws stream carries index-backed op rows, not payload-free block
    // ticks. each event cursor is the same `after` token the HTTP op log uses.
    let event1 = Daemon::ws_read_type(&mut ws, "event");
    let event2 = Daemon::ws_read_type(&mut ws, "event");
    assert_eq!(event1["topic"], "module:chat");
    assert_eq!(event2["topic"], "module:chat");
    assert_eq!(event1["cursor"], "op/0000000000000001/00000000");
    assert_eq!(event2["cursor"], "op/0000000000000002/00000000");

    let (code, ops) = daemon.request("GET", "/v1/index/chat/ops?limit=10", None);
    assert_eq!(code, 200, "ops failed: {ops}");
    let rows = ops["ops"].as_array().expect("ops array");
    assert_eq!(rows.len(), 2, "create and post rows: {ops}");
    assert_eq!(event1["op"], rows[0]);
    assert_eq!(event2["op"], rows[1]);

    let cursor1 = event1["cursor"].as_str().expect("event cursor");
    let (code, paged) = daemon.request(
        "GET",
        &format!("/v1/index/chat/ops?after={cursor1}&limit=10"),
        None,
    );
    assert_eq!(code, 200, "paged ops failed: {paged}");
    let paged_rows = paged["ops"].as_array().expect("paged ops array");
    assert_eq!(paged_rows.as_slice(), &rows[1..], "cursor pages to row 2");

    drop(ws);
    let mut ws = daemon.ws_connect();
    let _heartbeat = Daemon::ws_read_type(&mut ws, "heartbeat");
    Daemon::ws_send_text(
        &mut ws,
        &format!(
            r#"{{"op":"subscribe","topics":["module:chat"],"resume":{{"module:chat":"{cursor1}"}}}}"#
        ),
    );
    let _subscribed = Daemon::ws_read_type(&mut ws, "subscribed");
    let replay = Daemon::ws_read_type(&mut ws, "event");
    assert_eq!(replay["cursor"], event2["cursor"]);
    assert_eq!(replay["op"], rows[1]);

    // committed state reads back; authorship derived from the submit origin.
    let reply = daemon.query(
        "chat",
        serde_json::json!({ "messages_range": { "channel_id": "general", "from_seq": 1, "limit": 16 } }),
    );
    let messages = reply["messages"].as_array().expect("Messages reply");
    assert_eq!(messages.len(), 1);
    let head = &messages[0]["head"];
    assert_eq!(head["message_id"], "m1");
    assert_eq!(head["blocks"][0]["paragraph"][0]["text"], "hello from e2e");
    let author_bytes: Vec<u8> = head["author"]["key"]
        .as_array()
        .expect("Key author")
        .iter()
        .map(|v| v.as_u64().expect("byte") as u8)
        .collect();
    assert_eq!(
        author_bytes, b"alice",
        "authorship must come from the submit origin"
    );

    // a deterministic rejection is a clean 400, not a dead daemon.
    let (code, err) = daemon.submit("no-such-module", serde_json::json!({"Nope": {}}), None);
    assert_eq!(code, 400, "unknown target must reject: {err}");
    daemon.status(); // still alive, still answering.
}

/// read frames until block `height`'s module event and require a heartbeat
/// carrying that height to have arrived FIRST — the per-block tip push, not
/// the interval beat (which the loop tolerates at other heights).
fn assert_tip_precedes_event(ws: &mut BufReader<TcpStream>, height: u64) {
    let mut tip_seen = false;
    loop {
        let frame = Daemon::ws_read_json(ws);
        if frame["type"] == "heartbeat" && frame["height"] == height {
            tip_seen = true;
            continue;
        }
        if frame["type"] == "event" {
            assert_eq!(frame["op"]["height"], height, "event for block {height}");
            assert!(
                tip_seen,
                "no tip heartbeat at height {height} arrived before its event"
            );
            return;
        }
    }
}

/// the tip rides the block wake itself: every committed block pushes a
/// heartbeat frame with the new height BEFORE that block's module events, so
/// a console's height ticks per block instead of waiting out the 3s timer
/// beat. asserting the ordering on TWO consecutive blocks makes a
/// coincidental timer beat unable to false-pass the test — two timer beats
/// are 3s apart and cannot both land inside one test's submit window.
#[test]
fn block_commits_push_tip_heartbeats_before_their_events() {
    let storage = tempfile::TempDir::new().expect("storage dir");
    let daemon = Daemon::spawn(storage.path());

    let mut ws = daemon.ws_connect();
    let heartbeat = Daemon::ws_read_type(&mut ws, "heartbeat");
    assert_eq!(heartbeat["height"], 0);

    Daemon::ws_send_text(&mut ws, r#"{"op":"subscribe","topics":["module:chat"]}"#);
    let _subscribed = Daemon::ws_read_type(&mut ws, "subscribed");

    let (code, block) = daemon.submit(
        "chat",
        serde_json::json!({
            "create_channel": { "channel_id": "general", "name": "General", "post_policy": "open" }
        }),
        None,
    );
    assert_eq!(code, 200, "create channel failed: {block}");
    assert_eq!(block["height"], 1);
    assert_tip_precedes_event(&mut ws, 1);

    let (code, block) = daemon.submit("chat", post_message("general", "m1", "tick"), None);
    assert_eq!(code, 200, "post failed: {block}");
    assert_eq!(block["height"], 2);
    assert_tip_precedes_event(&mut ws, 2);
}

#[test]
fn programmable_user_calls_and_reports_failure_through_onchain_attribution() {
    use agent::{Continuation, Decode, Predicate, Program, Step, Value};
    use attribution::{Actor, AttributionReply, Reason};

    let storage = tempfile::TempDir::new().expect("storage dir");
    let daemon = Daemon::spawn(storage.path());
    macro_rules! map {
        ($($key:literal => $value:expr),* $(,)?) => {
            Value::Map(std::collections::BTreeMap::from([
                $(($key.to_owned(), $value)),*
            ]))
        };
    }
    let program = Program {
        steps: vec![
            Step::Branch {
                test: Predicate::Equals {
                    left: Value::Ref(vec!["change".into(), "reason".into()]),
                    right: Value::Text("mention".into()),
                },
                then: 1,
                or: 3,
            },
            Step::Call {
                module: "tasks".into(),
                msg: map! {
                    "task" => map! {
                        "create_task" => map! {
                            "task_id" => Value::Text("from-program".into()),
                            "title" => Value::Text("Created by a programmable user".into()),
                            "owner" => Value::Null,
                        },
                    },
                },
                bind: "task".into(),
                decode: Decode::Json,
                on_failure: Continuation::Step(2),
            },
            Step::Report {
                recipient: Value::Number(1),
                reason: Reason::Report,
                detail: Value::Ref(vec!["task".into()]),
            },
            Step::Finish,
        ],
    };
    let controller = "n".repeat(32);
    for (target, operation) in [
        (
            "chat",
            serde_json::json!({ "create_channel": {
                "channel_id": "general", "name": "General", "post_policy": "open",
            }}),
        ),
        (
            "identity",
            serde_json::json!({ "create": {
                "name": "controller", "scheme": "ed25519",
            }}),
        ),
        (
            "agent",
            serde_json::json!({ "provision": {
                "name": "taskbot", "program": program,
            }}),
        ),
    ] {
        let (code, reply) = daemon.submit(target, operation, Some(&controller));
        assert_eq!(code, 200, "program setup {target}: {reply}");
    }

    for message_id in ["first", "second"] {
        let (code, included) = daemon.submit(
            "chat",
            post_mention("general", message_id, 2),
            Some("alice"),
        );
        assert_eq!(code, 200, "mention: {included}");
        assert!(
            daemon.status()["height"].as_u64().unwrap() > included["height"].as_u64().unwrap(),
            "the script and call completion commit at later boundaries"
        );
    }

    let task = daemon.query(
        "tasks",
        serde_json::json!({
            "task": { "get": { "task_id": "from-program" } },
        }),
    );
    assert_eq!(
        task["task"]["task"]["owner"],
        serde_json::json!({"account": 2}),
        "the program owns the task and its later failed duplicate leaves it intact: {task}"
    );

    let changes = daemon.query(
        "attribution",
        serde_json::json!({
            "changes_for": { "recipient": 1, "after": 0, "limit": 64 },
        }),
    );
    let AttributionReply::Changes(entries) = serde_json::from_value(changes).unwrap() else {
        panic!("expected the controller's attributions");
    };
    let reports: Vec<_> = entries
        .iter()
        .map(|entry| &entry.change)
        .filter(|change| change.source.module == "agent" && change.reason == Reason::Report)
        .collect();
    assert_eq!(reports.len(), 2, "one chosen report for each invocation");
    for report in &reports {
        assert_eq!(report.actor, Actor::Account(2));
        assert_eq!(report.recipient, 1);
        assert!(
            matches!(
                report.cause,
                sdk::Cause::Chain {
                    hop: sdk::Hop::Completion(_),
                    ..
                }
            ),
            "the report preserves its call completion: {:?}",
            report.cause
        );
    }
    let applied: agent::CallResult = serde_json::from_slice(&reports[0].detail).unwrap();
    assert!(matches!(applied, agent::CallResult::Applied { .. }));
    let rejected: agent::CallResult = serde_json::from_slice(&reports[1].detail).unwrap();
    let agent::CallResult::Rejected { reason } = rejected else {
        panic!("duplicate task must report its target rejection");
    };
    assert!(
        reason.contains("task already exists: from-program"),
        "{reason}"
    );
}

#[test]
fn state_persists_across_restart() {
    let storage = tempfile::TempDir::new().expect("storage dir");

    {
        let daemon = Daemon::spawn(storage.path());
        let (code, _) = daemon.submit(
            "chat",
            serde_json::json!({
                "create_channel": { "channel_id": "durable", "name": "Durable", "post_policy": "open" }
            }),
            None,
        );
        assert_eq!(code, 200);
        let (code, _) = daemon.submit(
            "chat",
            post_message("durable", "m1", "written before restart"),
            Some("alice"),
        );
        assert_eq!(code, 200);

        // graceful retirement THROUGH the wire — the port is the daemon's
        // identity; a client that spawned it has no pid to signal.
        assert_eq!(daemon.admin_shutdown(), Some(200));
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut daemon = daemon;
        loop {
            match daemon.child.try_wait().expect("poll daemon") {
                Some(status) => {
                    assert!(status.success(), "shutdown must exit cleanly");
                    break;
                }
                None => {
                    assert!(
                        Instant::now() < deadline,
                        "daemon ignored /v1/admin/shutdown"
                    );
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    // a fresh daemon over the SAME storage root: qmdb state must survive, and
    // the local block counter resumes ABOVE the per-module index watermark
    // (two blocks were indexed) — a counter restarting at 0 would re-use
    // indexed heights and every new block would be silently skipped.
    let daemon = Daemon::spawn(storage.path());
    assert_eq!(daemon.status()["height"], 2);
    let reply = daemon.query(
        "chat",
        serde_json::json!({ "messages_range": { "channel_id": "durable", "from_seq": 1, "limit": 16 } }),
    );
    let messages = reply["messages"].as_array().expect("Messages reply");
    assert_eq!(messages.len(), 1, "chat state must survive a restart");
    assert_eq!(
        messages[0]["head"]["blocks"][0]["paragraph"][0]["text"],
        "written before restart"
    );

    // the explorer survives too: /v1/blocks reads the durable block index,
    // not an in-memory ring, so both pre-restart blocks are still served.
    let (code, blocks) = daemon.request("GET", "/v1/blocks", None);
    assert_eq!(code, 200, "blocks failed: {blocks}");
    let blocks = blocks["blocks"].as_array().expect("blocks array").clone();
    assert_eq!(blocks.len(), 2, "pre-restart blocks survive: {blocks:?}");
    assert_eq!(blocks[0]["height"], 1);
    let post = &blocks[1];
    assert_eq!(post["height"], 2);
    // a block now carries its member ops under `ops[]`; this lane is one op
    // per block.
    let op = &post["ops"][0];
    assert_eq!(op["target"], "chat");
    assert_eq!(op["disposition"], "applied");
    // this lane frames and signs nothing: the block hash is honestly empty, and
    // the op's proposer is the SUBMITTER's origin bytes as hex ("alice").
    assert_eq!(post["hash"], "");
    assert_eq!(op["proposer"], "616c696365");
    assert!(
        op["operations"]
            .as_array()
            .is_some_and(|ops| !ops.is_empty()),
        "the dispatch trace rides the op: {post}"
    );
}

/// block until `module`'s materialized view has caught up with everything the
/// op feed already carries.
///
/// the derived tier is asynchronous ON PURPOSE — the block loop writes the op
/// rows and the guest folds them into the views behind a background trigger, so
/// an index failure degrades the read models and never a block. that makes a
/// view read immediately after a submit a genuine race, and the daemon publishes
/// the backlog on `/v1/index/status` (`fold.<module>.pending`) precisely so a
/// reader can tell "caught up" from "still folding". this waits on THAT signal —
/// the daemon's own report that the fold drained — never on a duration.
fn await_view_folded(daemon: &Daemon, module: &str) {
    nettest::poll_until(
        &format!("{module}'s view fold to drain"),
        Duration::from_secs(30),
        || {
            let (code, status) = daemon.request("GET", "/v1/index/status", None);
            assert_eq!(code, 200, "index status failed: {status}");
            let fold = &status["fold"][module];
            // a module with no folding guest reports nothing — nothing to await.
            let drained = fold.is_null() || fold["pending"].as_u64() == Some(0);
            assert!(
                fold["lastError"].is_null(),
                "the {module} fold FAILED — its views can never catch up: {status}"
            );
            drained.then_some(())
        },
    );
}

#[test]
fn per_module_index_serves_ops_and_views() {
    let storage = tempfile::TempDir::new().expect("storage dir");
    let hits_of = |reply: &serde_json::Value| -> Vec<serde_json::Value> {
        reply["hits"].as_array().expect("hits reply").clone()
    };

    let pre_restart_height;
    {
        let daemon = Daemon::spawn(storage.path());
        let (code, _) = daemon.submit(
            "chat",
            serde_json::json!({
                "create_channel": { "channel_id": "eng", "name": "Eng", "post_policy": "open" }
            }),
            None,
        );
        assert_eq!(code, 200);
        let (code, _) = daemon.submit(
            "chat",
            post_message("eng", "m1", "fluent index demo"),
            Some("alice"),
        );
        assert_eq!(code, 200);
        // tasks owns TWO boards behind one module: the write wire is the
        // `WorkMsg` envelope that routes to the task board or the job board.
        let (code, task) = daemon.submit(
            "tasks",
            serde_json::json!({
                "task": { "create_task": { "task_id": "t1", "title": "wire the indexer" } }
            }),
            None,
        );
        assert_eq!(code, 200, "create task failed: {task}");

        // the raw op log: every applied chat op, oldest-first, json envelopes.
        let (code, ops) = daemon.request("GET", "/v1/index/chat/ops?limit=10", None);
        assert_eq!(code, 200, "ops failed: {ops}");
        let rows = ops["ops"].as_array().expect("ops array");
        assert_eq!(rows.len(), 2, "create-channel and post: {ops}");
        // the payload is the module op VERBATIM (chat's wire is snake_case);
        // the envelope itself (origin/height/seq) is the indexer's camelCase.
        assert_eq!(rows[1]["payload"]["post_message"]["message_id"], "m1");
        assert_eq!(rows[1]["origin"]["kind"], "external");
        assert_eq!(rows[1]["height"], 2);

        // chat's OWN endpoint: the materialized search view.
        await_view_folded(&daemon, "chat");
        let (code, folded, reply) = daemon.view(
            "chat",
            serde_json::json!({ "search": { "text": "fluent" } }),
        );
        assert_eq!(code, 200, "chat view failed: {reply}");
        let hits = hits_of(&reply);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["message_id"], "m1");
        assert_eq!(hits[0]["author"], "user:alice");
        // THE FOLD WATERMARK RIDES THE REPLY. It names the last op ROW the
        // fold consumed — the last row of the feed above, position and all —
        // which is how a caller that just wrote tells "my op is in this
        // snapshot" from "not yet". `meta/height` cannot: it reads 3 here
        // because every module's feed watermark tracks every block, folded or
        // not.
        let last = rows.last().expect("chat folded at least one op");
        assert_eq!(
            folded.as_deref(),
            Some(format!("{}:{}", last["height"], last["seq"]).as_str()),
            "the header names the last folded op row: {rows:?}"
        );

        // tasks' endpoint: the by-status partition.
        await_view_folded(&daemon, "tasks");
        let (code, reply) = daemon.request(
            "POST",
            "/v1/index/tasks/view",
            Some(&serde_json::json!({ "by_status": { "status": "open" } })),
        );
        assert_eq!(code, 200, "tasks view failed: {reply}");
        let tasks = reply["tasks"]["tasks"].as_array().expect("tasks array");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["title"], "wire the indexer");

        // a module with no materialized view answers 404 — forge's substrate
        // is already a queryable git repo; it never registers one. It never
        // folds either, so there is no watermark to stamp: ABSENT means
        // unknown, and a caller must never read it as height 0.
        let (code, folded, _) = daemon.view("forge", serde_json::json!({ "anything": {} }));
        assert_eq!(code, 404);
        assert_eq!(folded, None, "no fold, no watermark — not a zero one");

        // the watermark surface: all three blocks indexed, nothing poisoned.
        // EVERY module's watermark tracks the last applied block — chat reads
        // 3 even though its last op landed in block 2 — so a watermark below
        // the tip always means missing blocks, never a quiet module.
        let (code, status) = daemon.request("GET", "/v1/index/status", None);
        assert_eq!(code, 200);
        assert_eq!(status["poisoned"], false);
        assert_eq!(status["modules"]["chat"], 3);
        assert_eq!(status["modules"]["tasks"], 3);

        pre_restart_height = daemon.status()["height"].as_u64().expect("height");

        assert_eq!(daemon.admin_shutdown(), Some(200));
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut daemon = daemon;
        while daemon.child.try_wait().expect("poll daemon").is_none() {
            assert!(
                Instant::now() < deadline,
                "daemon ignored /v1/admin/shutdown"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    // restart over the same storage: the index survives, the block counter
    // resumes above its watermark, and NEW blocks keep indexing.
    let daemon = Daemon::spawn(storage.path());
    assert_eq!(
        daemon.status()["height"].as_u64().expect("height"),
        pre_restart_height
    );
    let (code, reply) = daemon.request(
        "POST",
        "/v1/index/chat/view",
        Some(&serde_json::json!({ "search": { "text": "fluent" } })),
    );
    assert_eq!(code, 200);
    assert_eq!(hits_of(&reply).len(), 1, "index survives a restart");

    let (code, _) = daemon.submit(
        "chat",
        post_message("eng", "m2", "fresh after restart"),
        Some("alice"),
    );
    assert_eq!(code, 200);
    await_view_folded(&daemon, "chat");
    let (code, reply) = daemon.request(
        "POST",
        "/v1/index/chat/view",
        Some(&serde_json::json!({ "search": { "text": "fresh" } })),
    );
    assert_eq!(code, 200);
    let hits = hits_of(&reply);
    assert_eq!(hits.len(), 1, "post-restart blocks keep indexing");
    assert_eq!(hits[0]["message_id"], "m2");
}

#[test]
fn blob_receipt_lane_round_trips_and_stays_off_consensus() {
    let storage = tempfile::TempDir::new().expect("storage dir");
    let daemon = Daemon::spawn(storage.path());
    let genesis_hash = daemon.status()["root_hash"]
        .as_str()
        .expect("root_hash")
        .to_string();

    // sha256 as 64-char lowercase hex — the digest rendering the lane returns.
    let digest_hex = |bytes: &[u8]| -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };

    // upload: binary, non-utf8 receipt bytes.
    let receipt: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
    let (code, body) = daemon.request_bytes("POST", "/v1/files/blob", &receipt);
    assert_eq!(
        code,
        200,
        "upload failed: {}",
        String::from_utf8_lossy(&body)
    );
    let reply: serde_json::Value = serde_json::from_slice(&body).expect("upload reply json");
    let digest = reply["digest"].as_str().expect("digest").to_string();
    assert_eq!(
        digest,
        digest_hex(&receipt),
        "the returned digest is sha256 of the exact uploaded bytes"
    );

    // fetch round-trips byte-identical.
    let (code, fetched) = daemon.request_bytes("GET", &format!("/v1/files/blob/{digest}"), &[]);
    assert_eq!(code, 200);
    assert_eq!(fetched, receipt, "fetched bytes must be byte-identical");

    // a well-formed digest nobody uploaded is a 404; a malformed digest
    // (uppercase hex included) is a 400, not a miss.
    let absent = digest_hex(b"never uploaded");
    let (code, _) = daemon.request_bytes("GET", &format!("/v1/files/blob/{absent}"), &[]);
    assert_eq!(code, 404, "absent receipt must be a 404");
    let upper = digest.to_uppercase();
    let (code, _) = daemon.request_bytes("GET", &format!("/v1/files/blob/{upper}"), &[]);
    assert_eq!(code, 400, "digest must be lowercase hex");

    // the receipt-lane body cap is 4 MiB inclusive: exactly 4 MiB lands...
    let max = vec![0xABu8; 4 * 1024 * 1024];
    let (code, _) = daemon.request_bytes("POST", "/v1/files/blob", &max);
    assert_eq!(code, 200, "a body of exactly the cap must land");
    // ...and one byte more is a 413 in the daemon's error envelope.
    let over = vec![0xCDu8; 4 * 1024 * 1024 + 1];
    let (code, body) = daemon.request_bytes("POST", "/v1/files/blob", &over);
    assert_eq!(
        code,
        413,
        "oversized body must be rejected: {}",
        String::from_utf8_lossy(&body)
    );
    let err: serde_json::Value = serde_json::from_slice(&body).expect("413 body is json");
    assert!(
        err["error"].is_string(),
        "413 uses the error envelope: {err}"
    );

    // the whole blob lane is off-consensus: no blocks, no root-hash movement.
    //
    // a non-zero height here means SOMETHING committed a block, and this test
    // submits no op at all — so the interesting evidence is what that block
    // holds, not the number. `/v1/blocks` names the op, which is what tells a
    // real "the blob lane started committing" regression apart from this
    // daemon not being ours at all.
    let status = daemon.status();
    let (_, blocks) = daemon.request("GET", "/v1/blocks", None);
    assert_eq!(
        status["height"], 0,
        "blob puts must not commit blocks — height moved to {}, blocks: {}",
        status["height"], blocks["blocks"]
    );
    assert_eq!(
        status["root_hash"].as_str(),
        Some(genesis_hash.as_str()),
        "blob puts must not move the root hash"
    );
}

// ============================================================================
// duckfs product surface: the stage -> commit -> read round trip against a real
// daemon. two chunks staged through signed module frames, a commit that references
// them (Chunks content) alongside an inline file, then ls/read/stat/history read
// it all back — read byte-exact. a rejected op (dangling chunk, oversized stage)
// is a clean 4xx, never a 500/panic. distinct from the op-receipt /v1/files/blob
// lane, which its own test above keeps green.
// ============================================================================

#[test]
fn duckfs_module_transport_stage_commit_and_reads_round_trip() {
    use duckfs_client::api::NodeApi as _;
    let storage = tempfile::TempDir::new().unwrap();
    let daemon = Daemon::spawn(storage.path());
    let files = daemon.files();
    let genesis = daemon.status()["root_hash"].clone();
    assert!(files.refs().unwrap().head.is_none());
    let chunk_a: Vec<u8> = (0..64u32).map(|i| (i * 7 % 256) as u8).collect();
    let chunk_b: Vec<u8> = (0..48u32).map(|i| (200 - i) as u8).collect();
    let digest_a = files.stage_chunk(&chunk_a).unwrap();
    let digest_b = files.stage_chunk(&chunk_b).unwrap();
    assert_eq!(daemon.status()["height"], 2);
    assert_ne!(daemon.status()["root_hash"], genesis);
    let changes = serde_json::from_value(serde_json::json!([
        {"put":{"path":"/shared/a.bin","exec":false,"meta":{},"content":{"chunks":{"size":chunk_a.len(),"chunks":[digest_a]}}}},
        {"put":{"path":"/shared/b.bin","exec":false,"meta":{},"content":{"chunks":{"size":chunk_b.len(),"chunks":[digest_b]}}}},
        {"put":{"path":"/shared/hello.txt","exec":false,"meta":{},"content":{"inline":{"b64":STANDARD.encode(b"hello duckfs")}}}}
    ])).unwrap();
    assert_eq!(
        files.commit(None, "seed duckfs", changes).unwrap().height,
        3
    );
    let (entries, next) = files.ls("/shared", None, None, 256).unwrap();
    assert!(next.is_none());
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        vec!["/shared/a.bin", "/shared/b.bin", "/shared/hello.txt"]
    );
    for (path, bytes) in [("/shared/a.bin", chunk_a), ("/shared/b.bin", chunk_b)] {
        assert_eq!(files.read(path, None, 0, 1024).unwrap(), (bytes, true));
    }
    assert!(files.stat("/shared/nope", None).unwrap().is_none());
    let history = files.history(8).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].message, "seed duckfs");
    let seed = history[0].id.clone();
    assert_eq!(files.refs().unwrap().head.as_deref(), Some(seed.as_str()));
    let absent = "22".repeat(32);
    assert_eq!(
        files.has_chunks(std::slice::from_ref(&absent)).unwrap(),
        vec![false]
    );
    let chunk_c = files.stage_chunk(b"third chunk").unwrap();
    assert_eq!(
        files.has_chunks(&[chunk_c, absent]).unwrap(),
        vec![true, false]
    );
    let changes = serde_json::from_value(serde_json::json!([
        {"put":{"path":"/shared/hello.txt","exec":false,"meta":{},"content":{"inline":{"b64":STANDARD.encode(b"HELLO AGAIN")}}}}
    ])).unwrap();
    files.commit(Some(&seed), "edit hello", changes).unwrap();
    let head = files.refs().unwrap().head.unwrap();
    let diff = files.diff(&seed, &head, "/shared").unwrap();
    assert_eq!(diff.len(), 1);
    assert_eq!(diff[0].path, "/shared/hello.txt");
    let bad_changes = serde_json::from_value(serde_json::json!([
        {"put":{"path":"/shared/dangling.bin","exec":false,"meta":{},"content":{"chunks":{"size":10,"chunks":["11".repeat(32)]}}}}
    ])).unwrap();
    assert!(
        files
            .commit(Some(&head), "dangling chunk", bad_changes)
            .is_err()
    );
    assert!(files.stage_chunk(&vec![0; 1024 * 1024 + 1]).is_err());
    daemon.status();
}

/// A SUBSCRIBED SESSION IS SENT BACK TO THE STORE ONCE PER FED BLOCK — AND ONLY
/// THEN.
///
/// The block wake is gated on the block having appended index rows, and until
/// this series existed nothing could observe whether that gate was wired at
/// all: inverting the one `sweep &&` that connects the decision to `catch_up`
/// left the whole suite green, because the 30s backstop delivered late and no
/// assertion measured promptness. This measures the sweep itself, so the gate
/// is pinned rather than inferred from a frame arriving eventually.
///
/// The counter is read BEFORE and AFTER, because `subscribe` runs its own
/// `Wake::All` catch-up that is not a block wake and must not be counted.
#[test]
fn a_fed_block_sweeps_a_subscribed_session_exactly_once() {
    let storage = tempfile::TempDir::new().expect("storage dir");
    let daemon = Daemon::spawn(storage.path());

    // BY CAUSE, because the total cannot tell the gate from its floor: the 30s
    // backstop sweeps too, and a test that waited for the total to move would
    // pass on a broken wake after half a minute. This asks whether the BLOCK
    // woke it.
    fn block_sweeps(text: &str) -> u64 {
        text.lines()
            .find_map(|line| {
                line.strip_prefix("ducktape_stream_index_sweeps_total{cause=\"block\"} ")
            })
            .map(|count| count.trim().parse().expect("counter is a number"))
            // absent until the first one — a family emits no series for a
            // label it has never seen.
            .unwrap_or(0)
    }

    let mut ws = daemon.ws_connect();
    Daemon::ws_send_text(&mut ws, r#"{"op":"subscribe","topics":["module:chat"]}"#);
    let subscribed = Daemon::ws_read_type(&mut ws, "subscribed");
    assert!(subscribed["topics"]["module:chat"].is_string());
    let before = block_sweeps(&daemon.metrics());

    let (code, block) = daemon.submit(
        "chat",
        serde_json::json!({
            "create_channel": { "channel_id": "general", "name": "General", "post_policy": "open" }
        }),
        None,
    );
    assert_eq!(code, 200, "create channel failed: {block}");

    // wait on the session's OWN delivery, never a clock: the event frame for
    // this block cannot arrive before the sweep that produced it.
    let event = Daemon::ws_read_type(&mut ws, "event");
    assert_eq!(event["topic"], "module:chat");

    assert_eq!(
        block_sweeps(&daemon.metrics()),
        before + 1,
        "one fed block, one sweep — not zero (the gate never fires) and not \
         several (something else is waking the index topics)"
    );
}

#[test]
fn metrics_endpoint_exposes_ducktape_and_runtime_series() {
    let storage = tempfile::TempDir::new().expect("storage dir");
    let daemon = Daemon::spawn(storage.path());

    // at genesis the ducktape series are registered but a block-derived series
    // like the height gauge has not been observed yet — commit one block first.
    let (code, block) = daemon.submit(
        "chat",
        serde_json::json!({
            "create_channel": { "channel_id": "general", "name": "General", "post_policy": "open" }
        }),
        None,
    );
    assert_eq!(code, 200, "create channel failed: {block}");

    let text = daemon.metrics();
    // the daemon's own series, registered into commonware's registry.
    assert!(
        text.contains("ducktape_blocks_total"),
        "blocks counter present: {text}"
    );
    assert!(
        text.contains("ducktape_block_height"),
        "height gauge present"
    );
    assert!(
        text.contains("ducktape_block_apply_latency_seconds"),
        "latency histogram present",
    );
    // the per-dispatch counter carries the low-cardinality labels, and the
    // block above dispatched chat as an external submit.
    assert!(
        text.contains("ducktape_dispatch_total") && text.contains("module=\"chat\""),
        "labelled dispatch counter present: {text}",
    );
    // the same encode() also carries commonware's runtime metrics — proof the
    // series share one registry — and closes with the OpenMetrics EOF sentinel.
    assert!(
        text.contains("runtime_"),
        "commonware runtime metrics present too"
    );
    assert!(
        text.trim_end().ends_with("# EOF"),
        "OpenMetrics EOF terminator"
    );
}

#[test]
fn metrics_stream_topic_pushes_the_scrape_over_ws() {
    let storage = tempfile::TempDir::new().expect("storage dir");
    let daemon = Daemon::spawn(storage.path());

    // commit one block so the ducktape series carry observed values.
    let (code, block) = daemon.submit(
        "chat",
        serde_json::json!({
            "create_channel": { "channel_id": "general", "name": "General", "post_policy": "open" }
        }),
        None,
    );
    assert_eq!(code, 200, "create channel failed: {block}");

    let mut ws = daemon.ws_connect();
    Daemon::ws_send_text(&mut ws, r#"{"op":"subscribe","topics":["metrics"]}"#);
    let subscribed = Daemon::ws_read_type(&mut ws, "subscribed");
    assert_eq!(
        subscribed["topics"]["metrics"], "0",
        "fresh snapshot cursor"
    );

    // the subscribe replay pushes the first sample immediately — no wait for
    // the next heartbeat tick — carrying the SAME exposition GET /metrics
    // serves, stamped with the server-side sample instant as its cursor.
    let tail = Daemon::ws_read_type(&mut ws, "tail");
    assert_eq!(tail["topic"], "metrics");
    let text = tail["item"]["text"].as_str().expect("scrape text");
    assert!(
        text.contains("ducktape_blocks_total"),
        "stream sample carries the block series: {text}"
    );
    assert!(
        text.trim_end().ends_with("# EOF"),
        "whole scrape body rides"
    );
    let time_ms = tail["item"]["time_ms"].as_u64().expect("sample instant");
    assert_eq!(tail["cursor"], time_ms.to_string());

    // the next sample arrives on the heartbeat tick without any block moving.
    let tail2 = Daemon::ws_read_type(&mut ws, "tail");
    assert_eq!(tail2["topic"], "metrics");
    assert!(
        tail2["item"]["time_ms"].as_u64().expect("second instant") >= time_ms,
        "tick samples advance monotonically"
    );
}

/// A SUBSCRIBE COSTS ONE SAMPLE PER TOPIC, NOT TWO.
///
/// `peers` composes its sample by encoding the whole metrics registry, so the
/// heartbeat's immediate first tick landing on the heels of the subscribe
/// replay meant every subscribe paid that twice — the second carrying nothing
/// the first did not. The scrape sits inside the ~3 s before the next real
/// beat, so the margin here is a whole heartbeat, not a hair.
///
/// ALL THREE SNAPSHOT TOPICS, because the guard is written at three call
/// sites. Asserting `peers` alone leaves deleting the other two copies green,
/// which is the same forget-a-topic failure `catch_up`'s exhaustive dispatch
/// was written to prevent.
#[test]
fn a_subscribe_composes_one_snapshot_per_topic_and_not_the_tick_behind_it() {
    let storage = tempfile::TempDir::new().expect("storage dir");
    let daemon = Daemon::spawn(storage.path());

    let mut ws = daemon.ws_connect();
    Daemon::ws_send_text(
        &mut ws,
        r#"{"op":"subscribe","topics":["peers","status","metrics"]}"#,
    );
    let _ = Daemon::ws_read_type(&mut ws, "subscribed");
    for _ in 0..3 {
        let _ = Daemon::ws_read_type(&mut ws, "tail");
    }

    let exposition = daemon.metrics();
    for topic in ["peers", "status", "metrics"] {
        assert_eq!(
            snapshot_samples(&exposition, topic),
            1,
            "{topic}: the subscribe replay composed the document; the immediate \
             heartbeat tick behind it must fold away rather than compose again"
        );
    }
}

/// DROPPING THE SOCKET MUST STOP THE SAMPLING — the whole cost argument for
/// gating the console's overview subscription rests on it, and until now
/// nothing observed it.
///
/// `peers` re-composes its sample by encoding the node's ENTIRE metrics
/// registry, per session, per heartbeat, for as long as a session holds the
/// topic. "Leaving the tab stops that at the source" is a claim about session
/// teardown that no test could see: a session that outlived its socket would
/// keep paying that cost forever with every existing test green.
///
/// THE SECOND SESSION IS THE CLOCK. Its frames mark heartbeat ticks, so the
/// test waits on the system's own events and never on a duration — a sleep
/// here would be a timeout wearing a disguise. If the closed session were
/// still sampling, the counter would advance by two per tick instead of one.
#[test]
fn a_closed_session_stops_costing_the_node_a_snapshot_sample() {
    let storage = tempfile::TempDir::new().expect("storage dir");
    let daemon = Daemon::spawn(storage.path());

    let mut leaving = daemon.ws_connect();
    Daemon::ws_send_text(&mut leaving, r#"{"op":"subscribe","topics":["peers"]}"#);
    let _ = Daemon::ws_read_type(&mut leaving, "subscribed");
    let _ = Daemon::ws_read_type(&mut leaving, "tail");

    let mut clock = daemon.ws_connect();
    Daemon::ws_send_text(&mut clock, r#"{"op":"subscribe","topics":["peers"]}"#);
    let _ = Daemon::ws_read_type(&mut clock, "subscribed");
    let _ = Daemon::ws_read_type(&mut clock, "tail");

    // the leaver goes away. Nothing else changes.
    drop(leaving);

    // ONE TICK OF SLACK so the close is observed before the window we measure.
    //
    // This read blocks for a real heartbeat beat now. It did not always: a
    // subscribe used to be sampled twice within milliseconds — the `Wake::All`
    // replay and the heartbeat's immediate first tick — and this read returned
    // instantly with that second, already-counted frame, ordering nothing.
    // `SNAPSHOT_MIN_INTERVAL_MS` folded the pair into one, which is what makes
    // the wait real and this comment true.
    let _ = Daemon::ws_read_type(&mut clock, "tail");
    let before = peers_samples(&daemon.metrics());

    // THREE ticks, counted by the surviving session's own frames.
    const TICKS: u64 = 3;
    for _ in 0..TICKS {
        let _ = Daemon::ws_read_type(&mut clock, "tail");
    }
    let observed = peers_samples(&daemon.metrics()) - before;

    // A RANGE, NOT AN EQUALITY, and the slack is exactly one tick.
    //
    // Upper bound: at most one further tick can fire between the last frame and
    // the scrape, so demanding equality would flake on it.
    //
    // Lower bound: the counter is incremented before its frame is sent, so
    // reading N frames proves at least N samples — but ONLY because the drain
    // above leaves no counted-but-unread frame behind at the `before` scrape.
    // The two steps buy that invariant together; neither alone is enough, and
    // the bottom of this range has no margin beyond it.
    //
    // The separation is what matters: ONE subscriber gives 3..=4, and a leaked
    // session gives 6 — which no slack of one tick can reach.
    assert!(
        (TICKS..=TICKS + 1).contains(&observed),
        "expected {TICKS}..={} samples for the ONE session still subscribed, got \
         {observed}. At roughly double, the closed session is still being \
         sampled and the console's subscription gate buys the node nothing; at \
         zero, the surviving session stopped sampling and the topic is dead.",
        TICKS + 1
    );
}

/// one arm of `ducktape_stream_snapshot_samples`, off a scrape.
fn snapshot_samples(exposition: &str, topic: &str) -> u64 {
    let series = format!("ducktape_stream_snapshot_samples_total{{topic=\"{topic}\"}}");
    exposition
        .lines()
        .find(|line| line.starts_with(&series))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse::<f64>().ok())
        .map(|value| value as u64)
        .unwrap_or_else(|| panic!("no {topic} sample counter in exposition"))
}

fn peers_samples(exposition: &str) -> u64 {
    snapshot_samples(exposition, "peers")
}

/// THE OVERVIEW TOPICS MUST DELIVER, OVER A REAL SOCKET, IN THE SHAPE THE
/// CONSOLE DECODES.
///
/// `files:watch` subscribed cleanly for months and never delivered a frame,
/// because every test stopped at the `subscribed` ack. So this reads the
/// items: `item.peers` must be the same document `GET /v1/peers` serves and
/// `item.status` the same one `GET /v1/status` serves, because
/// `ducktape_rpc::node_snapshots` routes on exactly those two keys and the
/// console then parses them with the readers it already had for the HTTP
/// routes. A rename on either side breaks this test rather than the console.
#[test]
fn overview_snapshot_topics_push_peers_and_status_over_ws() {
    let storage = tempfile::TempDir::new().expect("storage dir");
    let daemon = Daemon::spawn(storage.path());

    let mut ws = daemon.ws_connect();
    Daemon::ws_send_text(&mut ws, r#"{"op":"subscribe","topics":["peers","status"]}"#);
    let subscribed = Daemon::ws_read_type(&mut ws, "subscribed");
    assert_eq!(subscribed["topics"]["peers"], "0", "fresh snapshot cursor");
    assert_eq!(subscribed["topics"]["status"], "0", "fresh snapshot cursor");

    // BOTH arrive on the subscribe replay — no wait for a heartbeat tick, and
    // no block has to move: these planes have no op behind them, which is the
    // whole reason they needed a snapshot topic instead of a module stream.
    let mut peers = serde_json::Value::Null;
    let mut status = serde_json::Value::Null;
    for _ in 0..2 {
        let tail = Daemon::ws_read_type(&mut ws, "tail");
        let time_ms = tail["item"]["time_ms"].as_u64().expect("sample instant");
        assert_eq!(tail["cursor"], time_ms.to_string());
        match tail["topic"].as_str().expect("topic") {
            "peers" => peers = tail["item"]["peers"].clone(),
            "status" => status = tail["item"]["status"].clone(),
            other => panic!("unexpected topic {other}"),
        }
    }

    // the peers sample is the `/v1/peers` document: the envelope's own chain
    // coordinates plus the peer array. A solo daemon meshes with nobody, so an
    // EMPTY array is the correct answer — and an absent one is not.
    assert!(
        peers["peers"].is_array(),
        "peers sample carries the peer array: {peers}"
    );
    assert!(
        peers["sampled_at_ms"].as_u64().is_some(),
        "peers sample carries its own instant: {peers}"
    );

    // the status sample is the `/v1/status` document, read field-for-field by
    // the console's `load_node_facts`.
    assert!(
        status["version"].as_str().is_some(),
        "status sample carries the build: {status}"
    );
    assert!(
        status["root_hash"].as_str().is_some(),
        "status sample carries the app hash: {status}"
    );
    assert!(
        status["operations"].is_object(),
        "status sample carries the operations projection the overview reads: {status}"
    );

    // and it agrees with the HTTP route it mirrors — one node, one answer.
    let http = daemon.status();
    assert_eq!(
        status["version"], http["version"],
        "the pushed status must not be a second, drifting projection"
    );
}

// ============================================================================
// Operator-origin Git import through generic blob and module transport.
// Stock Git service protocol cases live in node-bin's real Gateway cluster.
// ============================================================================

/// a `git` invocation in `dir` with a hermetic config: no host global/system
/// config leaks in (gpg signing, aliases), the default branch is `main`, a fixed
/// identity, and no interactive credential/gpg prompts can hang the test.
fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "user.name=Ducktape Test",
            "-c",
            "user.email=test@ducktape.local",
            "-c",
            "commit.gpgsign=false",
        ]);
    cmd.args(args);
    cmd
}

/// run a git command, capturing stdout+stderr (git prints push progress and
/// rejections to stderr), WITHOUT asserting success — the caller decides.
fn git_capture(dir: &Path, args: &[&str]) -> std::process::Output {
    git_cmd(dir, args).output().expect("spawn git")
}

/// run a git command that must succeed.
fn git_ok(dir: &Path, args: &[&str]) {
    let out = git_capture(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}",
        render(&out)
    );
}

/// a legible dump of a git subprocess result for assertion messages / logs.
fn render(out: &std::process::Output) -> String {
    format!(
        "status: {}\n--- stdout ---\n{}--- stderr ---\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
}

/// stage a file with `content`, then commit it with `message`.
fn commit_file(dir: &Path, name: &str, content: &str, message: &str) {
    std::fs::write(dir.join(name), content).expect("write work file");
    git_ok(dir, &["add", name]);
    git_ok(dir, &["commit", "-m", message]);
}

/// this repo's current HEAD oid hex.
fn rev_parse_head(dir: &Path) -> String {
    let out = git_capture(dir, &["rev-parse", "HEAD"]);
    assert!(out.status.success(), "rev-parse failed:\n{}", render(&out));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// forge's committed HEAD oid hex for `repo` over /v1/query (`None` == unborn).
fn forge_head(daemon: &Daemon, repo: &str) -> Option<String> {
    let reply = daemon.query("forge", serde_json::json!({ "head_of": { "repo": repo } }));
    reply["head"].as_str().map(str::to_string)
}

#[test]
fn operator_git_import_uses_generic_blob_and_module_transport() {
    if nettest::skip_without("operator Git import", nettest::missing_tool("git")).is_some() {
        return;
    }
    let storage = tempfile::tempdir().unwrap();
    let daemon = Daemon::spawn(storage.path());
    let source = tempfile::tempdir().unwrap();
    git_ok(source.path(), &["init"]);
    commit_file(source.path(), "source.txt", "one\n", "first");
    let importer = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ops/forge-import.py");
    let publish = || {
        let output = Command::new("python3")
            .arg(&importer)
            .args([
                "push",
                "--node-url",
                &format!("http://127.0.0.1:{}", daemon.port),
                "--repo",
                "operator-import",
                "--branch",
                "main",
                "--token-file",
            ])
            .arg(storage.path().join("admin.token"))
            .current_dir(source.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            forge_head(&daemon, "operator-import"),
            Some(rev_parse_head(source.path()))
        );
    };
    publish();
    commit_file(source.path(), "source.txt", "one\ntwo\n", "second");
    publish();
    let height = daemon.status()["height"].as_u64().unwrap();
    publish();
    assert_eq!(daemon.status()["height"].as_u64().unwrap(), height);
}

// ============================================================================
// the FULL-STACK proof of the client transport: the `duckfs-client` checkout/
// commit engine driven through `HttpNode` against a real spawned daemon —
// checkout an empty prefix, write a small file AND a >1 MiB file (the stage
// path), commit, checkout again byte-identically, then force a same-path
// conflict and assert it surfaces a structured `ConflictReport` (never a silent
// merge). the hand-rolled `HttpNode` contract lives in the crate's
// `http_contract.rs`; this is the wire against the actual noded routes.
// ============================================================================

#[test]
fn duckfs_engine_round_trips_and_reports_conflict_through_http_node() {
    use duckfs_client::checkout::{CheckoutOptions, checkout_with};
    use duckfs_client::commit::{CommitError, commit};

    let storage = tempfile::TempDir::new().expect("storage dir");
    let daemon = Daemon::spawn(storage.path());
    let base_url = format!("http://127.0.0.1:{}", daemon.port);
    // the harness owns this daemon, so its duckfs writes carry the operator
    // credential the daemon minted at boot.
    let node = daemon.files();
    let opts = CheckoutOptions {
        node_url: base_url.clone(),
        ..Default::default()
    };

    // checkout the empty prefix: base is None (nothing committed yet).
    let dir_a = tempfile::TempDir::new().expect("checkout a");
    let idx = checkout_with(&node, dir_a.path(), "/shared/e2e", None, &opts)
        .expect("checkout empty prefix");
    assert!(idx.base_snapshot.is_none(), "empty checkout has no base");

    // a small (inline) file and a >1 MiB file — the latter forces the stage
    // path through real consensus (one signed module frame per chunk).
    std::fs::write(dir_a.path().join("small"), b"hello duckfs engine").expect("write small");
    let big: Vec<u8> = (0..(2 * 1024 * 1024 + 7))
        .map(|i| (i % 251) as u8)
        .collect();
    std::fs::write(dir_a.path().join("big"), &big).expect("write big");

    let summary = commit(&node, dir_a.path(), "seed via engine").expect("commit seed");
    assert!(!summary.rebased, "a first commit never rebases");

    // a fresh checkout elsewhere reads back byte-identical (the big file is
    // reassembled from staged chunks and verified against its object id).
    let dir_b = tempfile::TempDir::new().expect("checkout b");
    checkout_with(&node, dir_b.path(), "/shared/e2e", None, &opts).expect("checkout again");
    assert_eq!(
        std::fs::read(dir_b.path().join("small")).unwrap(),
        b"hello duckfs engine",
        "small file round-trips"
    );
    assert_eq!(
        std::fs::read(dir_b.path().join("big")).unwrap(),
        big,
        ">1 MiB file round-trips byte-identical"
    );

    // both checkouts edit the SAME path off the same base: A lands, B must
    // surface a ConflictReport naming the clashing path — no silent merge.
    std::fs::write(dir_a.path().join("small"), b"edit from A").expect("edit a");
    std::fs::write(dir_b.path().join("small"), b"edit from B").expect("edit b");
    commit(&node, dir_a.path(), "A wins").expect("A commits clean");
    let err = commit(&node, dir_b.path(), "B loses").expect_err("B must conflict");
    match err {
        CommitError::Conflict(report) => {
            assert!(
                report.clashing.iter().any(|p| p == "/shared/e2e/small"),
                "the conflicting path is named in the report: {report:?}"
            );
        }
        other => panic!("expected a structured conflict, got {other:?}"),
    }
}

// ============================================================================
// workspace RPC (the jobs/sandbox seam): the daemon manages a checkout under an
// injected root, driven entirely over http — create -> files on disk -> commit
// -> read back over the files surface -> delete. state lives on disk under
// `<storage>/duckfs-workspaces/<id>`, so this test reads/writes that path
// directly (same machine). a conflicting workspace commit is a 409 carrying the
// serialized ConflictReport.
// ============================================================================

#[test]
fn duckfs_workspace_rpc_maps_workspace_prefix_into_managed_namespace() {
    let storage = tempfile::TempDir::new().expect("storage dir");
    let daemon = Daemon::spawn(storage.path());

    // `/workspace` is the caller's local sandbox vocabulary. The workspace RPC
    // owns the duckfs namespace choice; it must not persist `/workspace` into
    // the .duckfs index and let commit fail later with the module's authority
    // error ("files: path is outside /home and /shared").
    let (code, ws) = daemon.request(
        "POST",
        "/v1/fs/workspaces",
        Some(&serde_json::json!({ "prefix": "/workspace" })),
    );
    assert_eq!(code, 200, "create managed workspace failed: {ws}");
    let id = ws["id"].as_str().expect("workspace id").to_string();
    let path = ws["path"].as_str().expect("workspace path").to_string();

    std::fs::write(std::path::Path::new(&path).join("hello.txt"), b"inside").unwrap();
    let (code, done) = daemon.request(
        "POST",
        &format!("/v1/fs/workspaces/{id}/commit"),
        Some(&serde_json::json!({ "message": "commit managed workspace" })),
    );
    assert_eq!(
        code, 200,
        "workspace commit should use a managed duckfs prefix: {done}"
    );

    let index = duckfs_client::index::Index::load(std::path::Path::new(&path)).unwrap();
    assert!(
        index.prefix.starts_with("/shared/workspaces/"),
        "the managed checkout records an internal writable prefix, got {}",
        index.prefix
    );
    let read_path = format!("{}/hello.txt", index.prefix);
    let (code, reply) = daemon.request("POST", "/v1/query", Some(&serde_json::json!({"target":"files","query":{"read":{"path":read_path,"snapshot":null,"offset":0,"len":1048576}}})));
    let read = &reply["read"];
    assert_eq!(code, 200, "read committed managed workspace file: {read}");
    let bytes = STANDARD
        .decode(read["b64"].as_str().expect("b64").as_bytes())
        .expect("decode b64");
    assert_eq!(bytes, b"inside");
}

#[test]
fn duckfs_workspace_rpc_lifecycle_and_conflict() {
    let storage = tempfile::TempDir::new().expect("storage dir");
    let daemon = Daemon::spawn(storage.path());

    // ---- create: an empty checkout. `/workspace` is the ONLY vocabulary this
    // RPC accepts; it maps to an id-scoped managed prefix the caller reads back
    // off the reply, so a job never has to know the module's writable roots ----
    let (code, ws) = daemon.request(
        "POST",
        "/v1/fs/workspaces",
        Some(&serde_json::json!({ "prefix": "/workspace" })),
    );
    assert_eq!(code, 200, "create workspace failed: {ws}");
    let id = ws["id"].as_str().expect("workspace id").to_string();
    let path = ws["path"].as_str().expect("workspace path").to_string();
    let prefix = ws["prefix"].as_str().expect("managed prefix").to_string();
    assert!(ws["snapshot"].is_null(), "empty checkout has no base: {ws}");
    // a duckfs path outside that vocabulary is a clean 400, not a commit that
    // fails later inside the module's authority check.
    let (code, refused) = daemon.request(
        "POST",
        "/v1/fs/workspaces",
        Some(&serde_json::json!({ "prefix": "/shared/job1" })),
    );
    assert_eq!(code, 400, "a non-/workspace prefix is refused: {refused}");
    // the managed checkout wrote its .duckfs index to disk at `path`.
    let index_json = std::path::Path::new(&path).join(".duckfs/index.json");
    assert!(
        index_json.exists(),
        "the workspace index must exist at {}",
        index_json.display()
    );

    // ---- edit on disk, then commit over rpc ----
    std::fs::write(
        std::path::Path::new(&path).join("hello.txt"),
        b"workspace bytes",
    )
    .expect("write into the workspace");
    let (code, done) = daemon.request(
        "POST",
        &format!("/v1/fs/workspaces/{id}/commit"),
        Some(&serde_json::json!({ "message": "commit from the workspace rpc" })),
    );
    assert_eq!(code, 200, "workspace commit failed: {done}");
    assert!(
        done["snapshot"].is_string(),
        "commit returns a snapshot id: {done}"
    );
    assert_eq!(done["rebased"], false, "a first commit never rebases");

    // ---- read the committed file back over the files surface ----
    let (code, reply) = daemon.request("POST", "/v1/query", Some(&serde_json::json!({"target":"files","query":{"read":{"path":format!("{prefix}/hello.txt"),"snapshot":null,"offset":0,"len":1048576}}})));
    let read = &reply["read"];
    assert_eq!(code, 200, "read the committed file: {read}");
    let bytes = STANDARD
        .decode(read["b64"].as_str().expect("b64").as_bytes())
        .expect("decode b64");
    assert_eq!(bytes, b"workspace bytes", "the committed bytes round-trip");

    // ---- delete: the workspace directory is gone ----
    let (code, gone) = daemon.request("DELETE", &format!("/v1/fs/workspaces/{id}"), None);
    assert_eq!(code, 200, "delete workspace failed: {gone}");
    assert_eq!(gone["ok"], true);
    assert!(
        !std::path::Path::new(&path).exists(),
        "the workspace dir is removed on delete"
    );

    // ---- conflict: a workspace loses a race on its OWN path. every managed
    // checkout owns an id-scoped prefix, so no two workspaces can collide; the
    // competing writer is whoever else commits into duckfs — here a direct
    // generic module submit that lands between this workspace's checkout and its
    // commit. same 409 lane, reachable the way production reaches it ----
    let (code, ws2) = daemon.request(
        "POST",
        "/v1/fs/workspaces",
        Some(&serde_json::json!({ "prefix": "/workspace" })),
    );
    assert_eq!(code, 200, "create conflict workspace: {ws2}");
    let id2 = ws2["id"].as_str().expect("workspace id").to_string();
    let path2 = ws2["path"].as_str().expect("workspace path").to_string();
    let prefix2 = ws2["prefix"].as_str().expect("managed prefix").to_string();
    let commit_ws = |id: &str, msg: &str| -> (u16, serde_json::Value) {
        daemon.request(
            "POST",
            &format!("/v1/fs/workspaces/{id}/commit"),
            Some(&serde_json::json!({ "message": msg })),
        )
    };

    std::fs::write(std::path::Path::new(&path2).join("f.txt"), b"v1").unwrap();
    let (code, seeded) = commit_ws(&id2, "seed");
    assert_eq!(code, 200, "seed commit lands: {seeded}");

    // a direct commit advances the SAME path off the seeded head — the
    // workspace's base snapshot is now stale.
    let (code, reply) = daemon.request(
        "POST",
        "/v1/query",
        Some(&serde_json::json!({"target":"files","query":{"refs":{}}})),
    );
    let refs = &reply["refs"];
    assert_eq!(code, 200, "refs failed: {refs}");
    let head = refs["head"].as_str().expect("seeded head").to_string();
    let (code, advanced) = daemon.request(
        "POST",
        "/v1/submit",
        Some(&serde_json::json!({"target":"files","payload":{"commit":{
            "base_snapshot": head,
            "message": "a competing writer takes the path",
            "changes": [
                { "put": { "path": format!("{prefix2}/f.txt"), "exec": false, "meta": {},
                    "content": { "inline": { "b64": STANDARD.encode(b"from the other writer") } } } },
            ],
        }}})),
    );
    assert_eq!(code, 200, "the competing commit lands: {advanced}");

    // ...so the workspace's same-path commit conflicts: a 409 naming the path.
    std::fs::write(
        std::path::Path::new(&path2).join("f.txt"),
        b"from the workspace",
    )
    .unwrap();
    let (code, report) = commit_ws(&id2, "loses");
    assert_eq!(
        code, 409,
        "an overlapping workspace commit is a 409: {report}"
    );
    let clashing = report["clashing"].as_array().expect("clashing array");
    assert!(
        clashing.iter().any(|p| p == &format!("{prefix2}/f.txt")),
        "the conflict report names the clashing path: {report}"
    );
}
