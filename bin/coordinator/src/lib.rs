//! Coordinator policy selection — the ONLY decision the untrusted coordinator
//! makes at boot: which [`nat_traversal::AuthPolicy`] to serve, and in private
//! mode which node's validator set it follows.
//!
//! Factored out of `main.rs` so it is unit-testable without spawning the
//! process. The coordinator stays keyless: `--genesis-set` reads ONLY the
//! PUBLIC validator pubkeys out of a `network.toml` (never a secret, never
//! written back), `--valset-node` reads only the PUBLIC current validator set
//! off a node's open `/v1/query` lane, and every other input is a bare CLI
//! flag.

use std::time::Duration;

use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519;
use nat_traversal::LiveValset;
use serde::Deserialize;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// This process's CPU time across every thread, user and system, in
/// nanoseconds — the kernel's own accounting, asked the POSIX way
/// (`getrusage`) so the reading is the same call on every host.
pub fn process_cpu_ns() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: `getrusage` writes one `rusage` into the pointer it is handed,
    // and `RUSAGE_SELF` is always a valid subject.
    let filled = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } == 0;
    if !filled {
        return None;
    }
    // SAFETY: the call returned 0, so the struct is filled.
    let usage = unsafe { usage.assume_init() };
    timeval_ns(usage.ru_utime)?.checked_add(timeval_ns(usage.ru_stime)?)
}

fn timeval_ns(time: libc::timeval) -> Option<u64> {
    let seconds = u64::try_from(time.tv_sec).ok()?;
    let microseconds = u64::try_from(time.tv_usec).ok()?;
    seconds
        .checked_mul(1_000_000_000)?
        .checked_add(microseconds.checked_mul(1_000)?)
}

/// This process's resident set, in bytes. The kernel keeps it where the host
/// keeps process facts: `/proc` on Linux, the task info call on macOS.
#[cfg(target_os = "linux")]
pub fn process_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    kib.checked_mul(1024)
}

#[cfg(target_os = "macos")]
pub fn process_rss_bytes() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::uninit();
    let size = i32::try_from(std::mem::size_of::<libc::proc_taskinfo>()).ok()?;
    let pid = i32::try_from(std::process::id()).ok()?;
    // SAFETY: `proc_pidinfo` writes at most `size` bytes into the buffer it
    // is handed, which is exactly one `proc_taskinfo`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: the call wrote the whole struct.
    let info = unsafe { info.assume_init() };
    Some(info.pti_resident_size)
}

/// The one field of `network.toml` the coordinator cares about: the genesis
/// validators, as hex ed25519 public keys. Every other key (chain_id,
/// bootstrap, reach, coordination, …) is ignored — serde drops unknown fields —
/// so a full descriptor parses here without dragging in `bin/node`.
#[derive(Debug, Deserialize)]
struct GenesisPin {
    #[serde(default)]
    validators: Vec<String>,
}

/// Select the authorization policy from CLI flags:
/// `--genesis-set <path>` => Private (the genesis set of that network.toml is
///                           the floor; `--valset-node` adds the live set);
/// otherwise              => public with proof-of-possession.
pub fn select_policy(args: &[String]) -> std::io::Result<nat_traversal::AuthPolicy> {
    let follows_a_node = args.iter().any(|a| a == VALSET_NODE_FLAG);
    let private = args.iter().any(|a| a == "--genesis-set");
    if follows_a_node && !private {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--valset-node follows a private network's validators; it needs --genesis-set",
        ));
    }
    // `--genesis-set` presence is detected SEPARATELY from its value: a present
    // but value-less flag (bare `--genesis-set`, `--genesis-set` as the final
    // token, or immediately followed by another `--flag` — e.g. an unset shell
    // variable that collapses to nothing) is a HARD error, never a silent
    // fall-through to the weaker public policy. Downgrading a
    // Private (genesis/cap-gated) coordinator to public-PoP on a typo'd path
    // would admit any node with a valid proof-of-possession.
    if let Some(i) = args.iter().position(|a| a == "--genesis-set") {
        let path = args
            .get(i + 1)
            .filter(|v| !v.starts_with("--"))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "--genesis-set requires a <network.toml> path",
                )
            })?;
        let genesis_set = load_genesis_pubkeys(path)?;
        return Ok(nat_traversal::AuthPolicy::Private {
            genesis_set,
            live: LiveValset::default(),
        });
    }
    Ok(nat_traversal::AuthPolicy::Public)
}

/// The flag naming the node a private coordinator follows.
pub const VALSET_NODE_FLAG: &str = "--valset-node";

/// The node's open read lane over committed module state.
pub const QUERY_PATH: &str = "/v1/query";

/// The read asked of it: the valset module's `Validators` query, whose reply
/// is `{"validators": [<32 key bytes>, ...]}` — every CURRENT validator.
const VALIDATORS_QUERY: &str = r#"{"target":"valset","query":"validators"}"#;

/// How often the coordinator re-reads its node's validator set: well inside
/// [`nat_traversal::LIVE_VALSET_TTL_SECS`], so a few failed reads in a row
/// never lapse a healthy reading.
pub const VALSET_REFRESH: Duration = Duration::from_secs(10);

/// One whole read — connect, request, reply — against the node.
const VALSET_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The largest reply read: a set of 2 000 validators is under 140 KiB.
const MAX_VALSET_REPLY_BYTES: u64 = 1 << 20;

/// A failing read logs its first attempt, then every Nth.
const FAILED_READ_LOG_EVERY: u64 = 30;

/// The node a private coordinator follows, as `host:port` — reached over plain
/// HTTP, so it is a node the operator runs or trusts, over a path they trust
/// (the same host, or a private link).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValsetNode {
    authority: String,
}

impl ValsetNode {
    /// Parse `http://<host>:<port>` (a trailing `/` is allowed): no default
    /// port, no path, no TLS.
    pub fn parse(raw: &str) -> std::io::Result<Self> {
        let authority = raw
            .strip_prefix("http://")
            .map(|rest| rest.strip_suffix('/').unwrap_or(rest));
        let names_only_a_host = |a: &&str| !a.contains(['/', '?', '#', '@', ' ']);
        let has_port = |a: &&str| {
            a.rsplit_once(':')
                .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok())
        };
        let Some(authority) = authority.filter(names_only_a_host).filter(has_port) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{VALSET_NODE_FLAG} {raw:?} is not http://<host>:<port>"),
            ));
        };
        Ok(Self {
            authority: authority.to_string(),
        })
    }
}

/// `--valset-node <http://host:port>`: the node whose validator set a private
/// coordinator follows. Absent: the coordinator admits against the genesis set
/// alone. Present without a value: a hard error, like `--genesis-set`.
pub fn valset_node(args: &[String]) -> std::io::Result<Option<ValsetNode>> {
    let Some(i) = args.iter().position(|a| a == VALSET_NODE_FLAG) else {
        return Ok(None);
    };
    let raw = args
        .get(i + 1)
        .filter(|v| !v.starts_with("--"))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{VALSET_NODE_FLAG} requires an http://<host>:<port> URL"),
            )
        })?;
    ValsetNode::parse(raw).map(Some)
}

/// The valset module's `Validators` reply, as `/v1/query` carries it.
#[derive(Deserialize)]
struct ValidatorsReply {
    validators: Vec<Vec<u8>>,
}

/// One read of `node`'s committed validator set, recorded into `live` when it
/// succeeds. A failure leaves the last reading in place to lapse on its own
/// TTL, and answers a snake_case reason.
pub async fn refresh(
    node: &ValsetNode,
    live: &LiveValset,
) -> Result<Vec<ed25519::PublicKey>, &'static str> {
    let raw = tokio::time::timeout(VALSET_READ_TIMEOUT, get_validators(node))
        .await
        .map_err(|_| "node_timeout")??;
    let validators = parse_validators_reply(&raw)?;
    live.record(validators.clone(), nat_traversal::now_secs());
    Ok(validators)
}

/// `POST /v1/query` for the validator set as HTTP/1.0, so the node answers
/// with a plain body and closes: the reply is everything read to EOF.
async fn get_validators(node: &ValsetNode) -> Result<Vec<u8>, &'static str> {
    let mut stream = tokio::net::TcpStream::connect(&node.authority)
        .await
        .map_err(|_| "node_unreachable")?;
    let request = format!(
        "POST {QUERY_PATH} HTTP/1.0\r\nHost: {}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{VALIDATORS_QUERY}",
        node.authority,
        VALIDATORS_QUERY.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|_| "node_unreachable")?;
    let mut raw = Vec::new();
    stream
        .take(MAX_VALSET_REPLY_BYTES)
        .read_to_end(&mut raw)
        .await
        .map_err(|_| "node_unreachable")?;
    Ok(raw)
}

/// The validator keys out of a raw HTTP reply. A set can never be empty on a
/// live chain, so an empty one (a node that has not published yet) is refused
/// like any other bad reading rather than recorded.
fn parse_validators_reply(raw: &[u8]) -> Result<Vec<ed25519::PublicKey>, &'static str> {
    let text = std::str::from_utf8(raw).map_err(|_| "reply_malformed")?;
    let (head, body) = text.split_once("\r\n\r\n").ok_or("reply_malformed")?;
    let answered_ok = head
        .lines()
        .next()
        .is_some_and(|status| status.split_whitespace().nth(1) == Some("200"));
    if !answered_ok {
        return Err("reply_not_ok");
    }
    let reply: ValidatorsReply = serde_json::from_str(body).map_err(|_| "reply_malformed")?;
    let validators: Vec<ed25519::PublicKey> = reply
        .validators
        .iter()
        .map(|key| ed25519::PublicKey::decode(key.as_slice()))
        .collect::<Result<_, _>>()
        .map_err(|_| "reply_malformed")?;
    if validators.is_empty() {
        return Err("valset_empty");
    }
    Ok(validators)
}

/// Follow `node`'s validator set forever: read it every [`VALSET_REFRESH`] into
/// `live`. A reading moves the set only when it succeeds, so a node outage
/// leaves the last one to lapse after its TTL — from then on a validator
/// only that reading named admits nothing (fail closed), while the genesis
/// set keeps admitting.
pub async fn follow_valset(node: ValsetNode, live: LiveValset) {
    let mut ticker = tokio::time::interval(VALSET_REFRESH);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last: Vec<ed25519::PublicKey> = Vec::new();
    let mut attempts = 0u64;
    loop {
        ticker.tick().await;
        match refresh(&node, &live).await {
            Ok(validators) => {
                let recovered = attempts > 0;
                let moved = validators != last;
                if recovered || moved {
                    tracing::info!(
                        target: "ducktape::reachability",
                        event = "coordinator_valset_read",
                        node = %node.authority,
                        validators = validators.len(),
                        failed_before = attempts,
                        "following the node's current validator set"
                    );
                }
                attempts = 0;
                last = validators;
            }
            Err(reason) => {
                attempts += 1;
                let logs = attempts == 1 || attempts.is_multiple_of(FAILED_READ_LOG_EVERY);
                if logs {
                    tracing::warn!(
                        target: "ducktape::reachability",
                        event = "coordinator_valset_read_failed",
                        reason,
                        node = %node.authority,
                        attempts,
                        lapsed = live.expired(nat_traversal::now_secs()),
                        "could not read the node's validator set; once the last reading \
                         lapses, only genesis validators and their caps admit"
                    );
                }
            }
        }
    }
}

/// Parse the PUBLIC genesis validator pubkeys out of a `network.toml`. This is
/// the ONLY new input the coordinator reads — public data, never a secret.
/// Mirrors `NetworkDescriptor::validator_keys` (bin/node/src/config.rs) without
/// depending on the node crate: decode each hex entry to an ed25519 pubkey and
/// reject a duplicate (a repeat would otherwise be a silently smaller valset).
fn load_genesis_pubkeys(path: &str) -> std::io::Result<Vec<ed25519::PublicKey>> {
    let text = std::fs::read_to_string(path)?;
    let pin: GenesisPin = toml::from_str(&text).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("network.toml: {e}"),
        )
    })?;
    let invalid = |msg: String| std::io::Error::new(std::io::ErrorKind::InvalidData, msg);

    let keys: Vec<ed25519::PublicKey> = pin
        .validators
        .iter()
        .map(|h| decode_key(h))
        .collect::<Result<_, _>>()
        .map_err(invalid)?;

    let mut seen = std::collections::BTreeSet::new();
    for k in &keys {
        if !seen.insert(k.as_ref().to_vec()) {
            return Err(invalid(format!(
                "duplicate validator {} in genesis set",
                hex_bytes(k.as_ref())
            )));
        }
    }
    Ok(keys)
}

/// Decode one hex-encoded ed25519 public key. Dependency-free hex (the
/// coordinator does not pull in bin/node's `unhex`); strict digits, even length.
fn decode_key(hex: &str) -> Result<ed25519::PublicKey, String> {
    let raw = unhex(hex.trim())?;
    ed25519::PublicKey::decode(raw.as_slice())
        .map_err(|e| format!("{hex:?} is not an ed25519 public key: {e}"))
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("hex string contains non-hex characters".into());
    }
    if !s.len().is_multiple_of(2) {
        return Err("hex string has odd length".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod valset_reads {
    use commonware_cryptography::Signer as _;

    use super::*;

    /// a key, and the json the valset module's reply carries it as.
    fn key_json(seed: u64) -> (ed25519::PublicKey, String) {
        let key = ed25519::PrivateKey::from_seed(seed).public_key();
        let json = serde_json::to_string(key.as_ref()).unwrap();
        (key, json)
    }

    fn reply(status: &str, body: &str) -> Vec<u8> {
        format!("HTTP/1.0 {status}\r\ncontent-type: application/json\r\n\r\n{body}").into_bytes()
    }

    #[test]
    fn a_reply_parses_to_the_validator_keys() {
        let ((a, a_json), (b, b_json)) = (key_json(1), key_json(2));
        let body = format!(r#"{{"validators":[{a_json},{b_json}]}}"#);
        assert_eq!(
            parse_validators_reply(&reply("200 OK", &body)),
            Ok(vec![a, b])
        );
    }

    #[test]
    fn a_bad_reply_is_refused_by_reason_never_recorded() {
        let (_, a_json) = key_json(1);
        let good = format!(r#"{{"validators":[{a_json}]}}"#);
        let cases = [
            (reply("400 Bad Request", &good), "reply_not_ok"),
            (reply("200 OK", r#"{"validators":[]}"#), "valset_empty"),
            (
                reply("200 OK", r#"{"validators":[[1,2,3]]}"#),
                "reply_malformed",
            ),
            (reply("200 OK", r#"{"residents":[]}"#), "reply_malformed"),
            (reply("200 OK", "not json"), "reply_malformed"),
            (b"HTTP/1.0 200 OK".to_vec(), "reply_malformed"),
        ];
        for (raw, reason) in cases {
            assert_eq!(parse_validators_reply(&raw), Err(reason));
        }
    }

    #[test]
    fn a_valset_node_is_an_explicit_http_host_and_port() {
        for good in [
            "http://127.0.0.1:8844",
            "http://node.lan:8844/",
            "http://[::1]:8844",
        ] {
            assert!(ValsetNode::parse(good).is_ok(), "{good} should parse");
        }
        for bad in [
            "127.0.0.1:8844",
            "https://node.lan:8844",
            "http://node.lan",
            "http://node.lan:port",
            "http://:8844",
            "http://node.lan:8844/v1/query",
            "http://user@node.lan:8844",
        ] {
            assert!(ValsetNode::parse(bad).is_err(), "{bad} should be refused");
        }
    }

    #[tokio::test]
    async fn a_refresh_records_what_the_node_serves() {
        let (a, a_json) = key_json(1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let node =
            ValsetNode::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let body = format!(r#"{{"validators":[{a_json}]}}"#);
        let served = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            // the whole request: the client holds its half open for the reply.
            let mut request = Vec::new();
            while !request.ends_with(VALIDATORS_QUERY.as_bytes()) {
                let mut chunk = [0u8; 512];
                let n = conn.read(&mut chunk).await.unwrap();
                assert!(n > 0, "the client closed before its query was sent");
                request.extend_from_slice(&chunk[..n]);
            }
            conn.write_all(&reply("200 OK", &body)).await.unwrap();
            String::from_utf8(request).unwrap()
        });

        let live = LiveValset::default();
        assert_eq!(refresh(&node, &live).await, Ok(vec![a]));
        assert!(!live.expired(nat_traversal::now_secs()));
        let request = served.await.unwrap();
        assert!(
            request.starts_with("POST /v1/query HTTP/1.0\r\n"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn an_unreachable_node_past_the_window_fails_closed_by_name() {
        // a port nothing listens on: bind, learn it, close it.
        let port = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let node = ValsetNode::parse(&format!("http://127.0.0.1:{port}")).unwrap();
        let (genesis, promoted, joiner) = (
            ed25519::PrivateKey::from_seed(10),
            ed25519::PrivateKey::from_seed(11),
            ed25519::PrivateKey::from_seed(20),
        );
        let live = LiveValset::default();
        let policy = nat_traversal::AuthPolicy::Private {
            genesis_set: vec![genesis.public_key()],
            live: live.clone(),
        };
        // the last good reading named the promoted validator, one window ago.
        let now = nat_traversal::now_secs();
        live.record(
            vec![genesis.public_key(), promoted.public_key()],
            now - nat_traversal::LIVE_VALSET_TTL_SECS,
        );

        // the node does not answer, so nothing replaces it...
        assert_eq!(refresh(&node, &live).await, Err("node_unreachable"));
        assert!(live.expired(now));
        // ...and a cap the promoted validator signed is refused by name, while
        // the genesis floor still admits.
        let subject = nat_traversal::NodeKey(joiner.public_key().as_ref().try_into().unwrap());
        for (issuer, verdict) in [
            (&promoted, Err(nat_traversal::AuthError::ValsetStale)),
            (&genesis, Ok(())),
        ] {
            let cap = nat_traversal::mint_coord_cap(issuer, subject, now + 3600);
            let auth = nat_traversal::sign_authenticator(&joiner, b"bind", now, Some(cap));
            assert_eq!(
                nat_traversal::verify_request(&policy, now, 30, subject, b"bind", &auth),
                verdict
            );
        }
    }
}

#[cfg(test)]
mod readings {
    use super::*;

    /// Both readings answer on this host, and CPU time only grows: a process
    /// that just did work has spent more of it than before.
    #[test]
    fn the_process_readings_answer_and_cpu_time_only_grows() {
        let before = process_cpu_ns().expect("cpu time reads on this host");
        let mut spent = 0u64;
        for step in 0..2_000_000u64 {
            spent = spent.wrapping_mul(31).wrapping_add(step);
        }
        assert_ne!(spent, 1, "the loop ran");
        let after = process_cpu_ns().expect("cpu time reads on this host");
        assert!(
            after >= before,
            "cpu time went backwards: {before} → {after}"
        );
        let rss = process_rss_bytes().expect("resident size reads on this host");
        assert!(rss > 0, "a running process is resident");
    }
}
