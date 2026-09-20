//! the shared spawn/drive harness behind the sim integration suites: a REAL
//! `ducktape-simnode` child process driven over its /v1 + /sim wires.
//! transport is the same deliberately raw std-TCP http/1.1 as noded's
//! daemon_e2e: any plain http client must be a full citizen of this wire.
//!
//! each tests/*.rs file is its own crate, so unused helpers per binary are
//! expected — hence the file-wide dead_code allow.
#![allow(dead_code)]

use std::io::{BufRead as _, BufReader};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Sim {
    child: Child,
    stdout: BufReader<ChildStdout>,
    port: u16,
    /// this sim's operator credential, minted 0600 into its storage dir at
    /// boot. EVERY mutating `/v1` route wants either it or a user signature,
    /// and a harness driving a sim it spawned IS that sim's local operator.
    operator: String,
}

impl Drop for Sim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Sim {
    /// spawn a sim child against `storage`. a FRESH dir is the norm — the
    /// determinism scenarios need one, since reused module state defeats the
    /// same-script reproducibility the tool exists for — but reusing a dir is a
    /// SUPPORTED restart: the sim resumes height above the index watermark
    /// (`index.resume_height()`) and every qmdb-backed module reloads its
    /// committed state, so respawning on the same dir CONTINUES the chain rather
    /// than restarting at 0 (exercised by reactor_seams.rs's restart scenario;
    /// `Drop` kills+waits the prior child, so a plain drop-then-respawn is safe).
    pub fn spawn(storage: &Path, extra_args: &[&str]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ducktape-simnode"));
        cmd.arg("--listen")
            .arg("127.0.0.1:0")
            .arg("--storage")
            .arg(storage)
            .args(extra_args)
            .stdout(Stdio::piped())
            // startup failures land on stderr — keep it visible or they read
            // as an opaque closed readiness pipe.
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().expect("spawn ducktape-simnode");
        let stdout = BufReader::new(child.stdout.take().expect("pipe sim stdout"));
        let mut sim = Self {
            child,
            stdout,
            port: 0,
            operator: String::new(),
        };
        sim.port = sim.read_listen_port();
        // the credential is written before the listener binds, so a sim that
        // announced its port has already minted it.
        sim.operator = noded::admin::read_operator_token(storage)
            .expect("the sim minted an operator credential");
        sim
    }

    /// this sim's operator credential — for a route the helpers below do not
    /// wrap.
    pub fn operator_token(&self) -> &str {
        &self.operator
    }

    /// Wait on the child's listener-bound event and return its OS-selected port.
    /// This is an event handoff, not a probe-and-drop reservation or readiness
    /// poll: once the line arrives, this exact child owns the reported listener.
    fn read_listen_port(&mut self) -> u16 {
        let mut line = String::new();
        let read = self
            .stdout
            .read_line(&mut line)
            .expect("read sim readiness");
        if read == 0 {
            let status = self.child.wait().expect("wait for failed sim startup");
            panic!("sim exited during startup ({status}) — see stderr above");
        }
        let addr: SocketAddr = line
            .strip_prefix("DUCKTAPE_SIMNODE_LISTEN=")
            .unwrap_or_else(|| panic!("unexpected sim readiness event: {line:?}"))
            .trim_end()
            .parse()
            .unwrap_or_else(|error| panic!("invalid sim readiness address {line:?}: {error}"));
        assert!(addr.ip().is_loopback(), "sim reported non-loopback {addr}");
        assert_ne!(addr.port(), 0, "sim reported unresolved listen port");
        addr.port()
    }

    /// this sim's http port — for the routes the helpers below do not wrap,
    /// namely `/v1/admin/*`, which needs a credential header no json helper
    /// carries.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// block until the child exits ON ITS OWN — the event a graceful
    /// `/v1/admin/shutdown` produces. `Drop`'s kill is the backstop for a test
    /// that never reaches here; this is what a shutdown assertion waits on, and
    /// it waits on the process's own exit rather than on a duration.
    pub fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        self.child.wait().expect("wait for sim exit")
    }

    pub fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        credentialed_request(self.port, &self.operator, method, path, body).expect("sim reachable")
    }

    pub fn status(&self) -> serde_json::Value {
        let (status, reply) = self.request("GET", "/v1/status", None);
        assert_eq!(status, 200, "status failed: {reply}");
        reply
    }

    pub fn sim_state(&self) -> serde_json::Value {
        let (status, reply) = self.request("GET", "/sim/state", None);
        assert_eq!(status, 200, "sim state failed: {reply}");
        reply
    }

    pub fn step(&self) -> serde_json::Value {
        let (status, reply) = self.request("POST", "/sim/step", None);
        assert_eq!(status, 200, "step failed: {reply}");
        reply
    }

    pub fn set_auto(&self, enabled: bool) {
        let (status, reply) = self.request(
            "POST",
            "/sim/auto",
            Some(&serde_json::json!({ "enabled": enabled })),
        );
        assert_eq!(status, 200, "set auto failed: {reply}");
    }

    pub fn query(&self, target: &str, query: serde_json::Value) -> serde_json::Value {
        let (status, reply) = self.request(
            "POST",
            "/v1/query",
            Some(&serde_json::json!({ "target": target, "query": query })),
        );
        assert_eq!(status, 200, "query {target} failed: {reply}");
        reply
    }

    /// the canonical chat point read: the channel's committed record, or `None`
    /// when nothing is committed under that id.
    ///
    /// there is no list-all read to count any more, and that is the product's
    /// position, not a gap: the read-model cutover moved every UI-shaped
    /// iteration into the index guests (`POST /v1/index/chat/view`) and left
    /// canonical state answering point reads ALONE. so a "did this commit?"
    /// assertion has to name the channel it means — which is also the stronger
    /// claim, since a count of 2 never said WHICH two.
    pub fn channel(&self, channel_id: &str) -> Option<serde_json::Value> {
        let reply = self.query(
            "chat",
            serde_json::json!({ "channel": { "channel_id": channel_id } }),
        );
        match reply["channel"] {
            serde_json::Value::Null => None,
            _ => Some(reply["channel"].clone()),
        }
    }

    /// an inline submit — only sound in auto mode (or for an op the module
    /// rejects at once), where the reply does not wait on a step.
    pub fn submit(
        &self,
        target: &str,
        payload: serde_json::Value,
        origin: Option<&str>,
    ) -> (u16, serde_json::Value) {
        let mut body = serde_json::json!({ "target": target, "payload": payload });
        if let Some(origin) = origin {
            body["origin"] = serde_json::json!(origin);
        }
        credentialed_request(self.port, &self.operator, "POST", "/v1/submit", Some(&body))
            .expect("submit reachable")
    }

    /// inline submit that must COMMIT — returns the receipt.
    pub fn submit_ok(
        &self,
        target: &str,
        payload: serde_json::Value,
        origin: Option<&str>,
    ) -> serde_json::Value {
        let (code, reply) = self.submit(target, payload, origin);
        assert_eq!(code, 200, "submit to {target} failed: {reply}");
        reply
    }

    /// inline submit that the module must REJECT — returns the error text.
    pub fn submit_rejected(
        &self,
        target: &str,
        payload: serde_json::Value,
        origin: Option<&str>,
    ) -> String {
        let (code, reply) = self.submit(target, payload, origin);
        assert_eq!(code, 400, "submit to {target} was not rejected: {reply}");
        reply["error"]
            .as_str()
            .unwrap_or_else(|| panic!("rejection carries no error text: {reply}"))
            .to_string()
    }

    /// spawn a submit on its own thread — in hold mode the http reply hangs
    /// until a step releases it, so the caller must not block on it inline.
    pub fn submit_in_background(
        &self,
        target: &str,
        payload: serde_json::Value,
        origin: Option<&str>,
    ) -> std::thread::JoinHandle<(u16, serde_json::Value)> {
        let port = self.port;
        let operator = self.operator.clone();
        let target = target.to_string();
        let mut body = serde_json::json!({ "target": target, "payload": payload });
        if let Some(origin) = origin {
            body["origin"] = serde_json::json!(origin);
        }
        std::thread::spawn(move || {
            credentialed_request(port, &operator, "POST", "/v1/submit", Some(&body))
                .expect("held submit reachable")
        })
    }

    /// POST raw signed-frame bytes to `/v1/submit/frame` — the authenticated
    /// lane. the body is `application/octet-stream` (the exact bytes
    /// `node::encode_frame` produced), not json, so it needs its own sender.
    /// only sound in auto mode (the reply commits without a step).
    pub fn submit_frame(&self, frame: &[u8]) -> (u16, serde_json::Value) {
        post_raw(
            self.port,
            "/v1/submit/frame",
            "application/octet-stream",
            frame,
        )
        .expect("frame submit reachable")
    }

    /// a `/sim/peer-block` batch: N ops committed as ONE block. returns the
    /// (status, reply) so a test can assert per-member verdicts (the reply
    /// carries `members: [{applied|rejected}]`) and the single committed height.
    pub fn peer_batch(&self, ops: serde_json::Value) -> (u16, serde_json::Value) {
        self.request(
            "POST",
            "/sim/peer-block",
            Some(&serde_json::json!({ "ops": ops })),
        )
    }

    /// commit a concurrent writer's one-op block, independent of the held
    /// queue — a one-element `/sim/peer-block` batch whose single member must
    /// apply. returns the `BatchInfo` reply (`height`, `root_hash`, `members`).
    pub fn peer_block(
        &self,
        target: &str,
        payload: serde_json::Value,
        origin: &str,
    ) -> serde_json::Value {
        let (status, reply) = self.request(
            "POST",
            "/sim/peer-block",
            Some(&serde_json::json!({
                "ops": [{
                    "target": target,
                    "payload": payload,
                    "origin": origin,
                }],
            })),
        );
        assert_eq!(status, 200, "peer block failed: {reply}");
        assert_eq!(
            reply["members"][0]["disposition"], "applied",
            "peer op rejected: {reply}"
        );
        reply
    }

    /// poll /sim/state until `field` reaches `want` — the held queue is fed by
    /// another thread's in-flight request, so arrival is asynchronous.
    pub fn await_sim_state(&self, field: &str, want: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if self.sim_state()[field] == want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "sim state {field} never reached {want}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

/// json request/response against the sim's /v1 + /sim wires — the shared raw
/// http/1.1 client, `io::Result` so `await_status` can poll a not-yet-up sim.
/// `embed.rs` drives the embedded server through this directly.
// each tests/*.rs is its own crate, so a suite that only makes CREDENTIALED
// requests never names this one.
#[allow(unused_imports)]
pub use nettest::try_http_json as try_request;

/// [`try_request`] carrying the sim's operator credential — what a MUTATING
/// `/v1` route wants from a caller acting as the node's own operator.
pub fn credentialed_request(
    port: u16,
    operator: &str,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> std::io::Result<(u16, serde_json::Value)> {
    let bytes = body
        .map(|b| serde_json::to_vec(b).expect("request body serializes"))
        .unwrap_or_default();
    let (status, raw) = nettest::try_http_bytes_with(
        port,
        method,
        path,
        "application/json",
        &[(noded::admin::ADMIN_TOKEN_HEADER, operator)],
        &bytes,
    )?;
    Ok((
        status,
        serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null),
    ))
}

/// POST arbitrary body bytes with an explicit content-type — the raw-bytes
/// twin of [`try_request`] (which is json-only), for the octet-stream frame
/// lane.
fn post_raw(
    port: u16,
    path: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<(u16, serde_json::Value)> {
    let (status, raw) = nettest::try_http_bytes(port, "POST", path, content_type, body)?;
    Ok((
        status,
        serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null),
    ))
}

pub fn create_channel(channel: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "create_channel": { "channel_id": channel, "name": name, "post_policy": "open" }
    })
}

pub fn post_message(channel: &str, message_id: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "post_message": {
            "channel_id": channel,
            "message_id": message_id,
            "blocks": [{ "paragraph": [{ "text": text, "marks": [] }] }],
            "thread": null,
        }
    })
}

/// Provision the model's real keyless account and its program under the first
/// account in a fresh scenario. The caller submits all three operations as the
/// same 32-byte controller key.
pub fn model_setup(agent_id: &str, capability: &str) -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("identity", create("model-controller")),
        (
            "agent",
            serde_json::json!({ "provision": {
                "request_id": format!("model-{agent_id}"),
                "name": agent_id,
                "program": runs::model_program(agent_id),
            }}),
        ),
        (
            "runs",
            serde_json::json!({ "configure_model": { "operation": { "register_model": {
                "account": 2,
                "agent_id": agent_id,
                "display_name": agent_id,
                "capability": capability,
            }}}}),
        ),
    ]
}

pub fn found_account(sim: &Sim, name: &str, seed: u64) -> String {
    use commonware_cryptography::Signer as _;
    let origin = key_origin(&commonware_cryptography::ed25519::PrivateKey::from_seed(
        seed,
    ));
    sim.submit_ok("identity", create(name), Some(&origin));
    origin
}

/// the sim's identity chain id — the composer's `Bindings { chain_id }` seeded
/// into the identity guest's genesis `__config`, so every add-key consent
/// signs over it (the same value the gateway guest scopes routes to).
pub const IDENTITY_CHAIN: &str = noded::LOCAL_CHAIN_ID;

/// the `hex:` origin escape naming a REAL ed25519 key as the submit origin —
/// the only way a json-string origin lane can found an account whose member
/// can later sign (an ASCII origin like `"a"*32` is well-formed for ed25519
/// but holds no secret, so it can found but never consent).
pub fn key_origin(key: &commonware_cryptography::ed25519::PrivateKey) -> String {
    use commonware_cryptography::Signer as _;
    let hex: String = key
        .public_key()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("hex:{hex}")
}

/// the `Create` op founding an account for the submit ORIGIN (declared
/// ed25519, so a 32-byte origin — an ASCII stand-in or a real key via
/// [`key_origin`] — founds; anything else is refused as malformed). the
/// message shape lives ONCE in `identity::testkit`; this wraps it back to the
/// untyped JSON the sim's `/v1/submit` lane takes.
pub fn create(name: &str) -> serde_json::Value {
    serde_json::to_value(identity::testkit::create(name)).expect("Create serializes")
}

/// the `AddKey` op admitting `new_key` (the op's ORIGIN) into `account`,
/// ed25519 `member`'s, consented to at `generation` on the sim's chain. the
/// consent is single-use (acceptance advances `new_key`'s generation) and
/// dies at [`CONSENT_EXPIRES`].
pub fn add_ed25519_key(
    member: &commonware_cryptography::ed25519::PrivateKey,
    new_key: &[u8],
    generation: u64,
    account: u64,
) -> serde_json::Value {
    serde_json::to_value(identity::testkit::add_ed25519_key(
        member,
        IDENTITY_CHAIN,
        new_key,
        generation,
        None,
        account,
        CONSENT_EXPIRES,
    ))
    .expect("AddKey serializes")
}

/// the expiry every sim consent carries: the sim's logical clock is
/// `SIM_EPOCH_MS + height * SIM_BLOCK_MS`, so this is 500 blocks past its
/// epoch — past every height a sim test drives, inside
/// `identity::MAX_CONSENT_TTL` of each.
pub const CONSENT_EXPIRES: u64 = simnode::SIM_EPOCH_MS + 500 * simnode::SIM_BLOCK_MS;
