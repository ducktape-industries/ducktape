//! The operator CLI's thin HTTP client for a node's `/v1` surface: one
//! `submit`, one `submit_frame` and one `query` primitive, shared by every
//! `user`/`account`/`agent` verb so the `{target, payload}` / `{target, query}`
//! shapes and the receipt/error handling live in exactly one place instead of
//! being re-inlined per verb.
//!
//! `/v1/submit` is the frameless lane: it stamps the NODE's key as the op
//! origin, so only node-authored ops (announces, node-level governance) go
//! there. Every USER-authored op — an identity, gateway or saga op that must be
//! attributed to an account — is a frame the user key signed, POSTed verbatim
//! to `/v1/submit/frame`; its verified signer is the op's origin. `/v1/query`
//! reads committed module state.

use commonware_cryptography::Signer as _;

/// ONE blocking client for this process's whole `/v1` lane.
///
/// `reqwest::blocking::Client::new()` is neither free nor infallible: each one
/// spawns its own tokio runtime on its own thread with its own connection pool,
/// and it PANICS when the build fails. Under `EMFILE` that is how a descriptor
/// shortage became a dead `announce-watch` thread — the watcher builds three
/// clients per 10 s tick, so it was the first thing in the process to ask the
/// kernel for a descriptor it could not have, and it asked through a `.expect`.
/// One client, built once and reused, also keeps the loopback keep-alive
/// connection instead of dialing a fresh socket per request.
///
/// A failed build is NOT cached: it is transient by nature (that is what EMFILE
/// is), and a poisoned lane would outlive the shortage that caused it. Two
/// racing builds are possible and harmless — the loser is dropped.
fn client() -> Result<&'static reqwest::blocking::Client, String> {
    static CLIENT: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let built = reqwest::blocking::Client::builder()
        .build()
        .map_err(|error| format!("could not build the http client: {error}"))?;
    Ok(CLIENT.get_or_init(|| built))
}

/// Submit one NODE-AUTHORED module op over `/v1/submit` `{target, payload}` and
/// return the commit height from the receipt. A non-2xx status carries the
/// node's rejection string.
///
/// `workspace` is the node's own directory, and reading this boot's operator
/// credential out of it is how the request authenticates: the route refuses a
/// caller that presents neither that nor a user signature, and an announce IS
/// the node's op — no user key would be the right actor for it. An unreadable
/// credential is not a reason to give up early; the node's own 401 names it
/// far better than a guess here would.
pub(crate) fn submit(
    base: &str,
    workspace: &std::path::Path,
    target: &str,
    payload: &serde_json::Value,
) -> Result<u64, Box<dyn std::error::Error>> {
    let operator = noded::admin::read_operator_token(workspace).ok();
    let body = post_with(
        base,
        "/v1/submit",
        &serde_json::json!({ "target": target, "payload": payload }),
        operator.as_deref(),
    )?;
    receipt_height(&body)
}

/// Submit one ALREADY-SIGNED op frame (the exact bytes `node::encode_frame`
/// produced — see `userkey_cli::user_frame`) over `/v1/submit/frame` and
/// return the commit height. The frame's verified signer becomes the op's
/// `Origin::External`, which is what lets an account's key act for itself.
pub(crate) fn submit_frame(base: &str, frame: &[u8]) -> Result<u64, Box<dyn std::error::Error>> {
    const PATH: &str = "/v1/submit/frame";
    let resp = client()?
        .post(format!("{base}{PATH}"))
        .header("content-type", "application/octet-stream")
        .body(frame.to_vec())
        .send()
        .map_err(|error| transport_failure(base, PATH, &error).to_string())?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("{PATH} rejected ({status}): {text}").into());
    }
    receipt_height(&text)
}

/// Resolve a signed op's assigned stamp from its applied index row, never a
/// module's mutable latest state. Matching is scoped to the receipt's block,
/// verified external signer and complete decoded payload. Identical duplicate
/// dispatches are ambiguous and fail closed rather than choosing a newer row.
/// An unavailable/lagging index is an error; callers may reconcile and retry.
pub(crate) fn submit_frame_assigned(
    base: &str,
    frame: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let (origin, msg) = node::decode_frame(frame)?;
    let expected_origin = noded::index_origin(&origin);
    let payload: Option<serde_json::Value> = serde_json::from_slice(&msg.payload).ok();
    let payload_hex = payload.is_none().then(|| noded::hex_bytes(&msg.payload));
    let height = submit_frame(base, frame)?;
    // This prefix sorts before sequence zero at exactly the committed height.
    let mut after = format!("op/{height:016x}/");
    let mut assigned = None;
    loop {
        let response = client()?
            .get(format!("{base}/v1/index/{}/ops", msg.target))
            .query(&[("after", after.as_str()), ("limit", "128")])
            .send()?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "applied receipt index unavailable ({status}); retry to reconcile"
            )
            .into());
        }
        let page: serde_json::Value = response.json()?;
        let rows = page["ops"]
            .as_array()
            .ok_or("unexpected applied receipt page")?;
        for row in rows {
            let row_height = row["height"]
                .as_u64()
                .ok_or("missing applied receipt height")?;
            if row_height > height {
                return assigned.ok_or_else(|| {
                    "signed op has no indexed assigned receipt; retry to reconcile".into()
                });
            }
            let matching = row_height == height
                && row["origin"] == serde_json::to_value(&expected_origin)?
                && row.get("payload") == payload.as_ref()
                && row.get("payload_hex").and_then(serde_json::Value::as_str)
                    == payload_hex.as_deref();
            if !matching {
                continue;
            }
            if assigned.is_some() {
                return Err("signed op has ambiguous applied receipts".into());
            }
            let stamp = match (
                row.get("assigned"),
                row.get("assigned_hex").and_then(serde_json::Value::as_str),
            ) {
                (Some(value), None) => serde_json::to_vec(value)?,
                (None, Some(value)) => hex::decode(value)?,
                _ => return Err("signed op has no assigned stamp".into()),
            };
            assigned = Some(stamp);
        }
        if page["has_more"] == false {
            return assigned.ok_or_else(|| {
                "signed op has no indexed assigned receipt; retry to reconcile".into()
            });
        }
        let next = page["next_after"]
            .as_str()
            .ok_or("missing applied receipt cursor")?;
        if next <= after.as_str() {
            return Err("applied receipt cursor did not advance".into());
        }
        after = next.to_string();
    }
}

/// the `height` of a `SubmitReceipt` body — both submit lanes answer with one.
fn receipt_height(body: &str) -> Result<u64, Box<dyn std::error::Error>> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["height"].as_u64())
        .ok_or_else(|| format!("unexpected submit receipt: {body}").into())
}

/// Read committed module state over `/v1/query` `{target, query}` and return
/// the module's reply as JSON for the caller to deserialize.
pub(crate) fn query(
    base: &str,
    target: &str,
    query: serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let body = post(
        base,
        "/v1/query",
        &serde_json::json!({ "target": target, "query": query }),
    )?;
    Ok(serde_json::from_str(&body)?)
}

/// the authenticated read lane's path — the one spelling, shared by the client
/// here and the signature it mints (the bytes bind the path, so a second
/// spelling is a signature that verifies against nothing).
pub(crate) const QUERY_READER_PATH: &str = "/v1/query/reader";

/// Read committed module state over `POST /v1/query/reader` AS `signer`.
///
/// [`query`]'s authenticated sibling. The plain lane proves nothing about who
/// is asking, so a module answering it sees `Origin::System` and must refuse
/// protected content; this lane carries the signer's verified key to the module
/// as its `Env::origin`, which is how a mailbox or a conversation page can be
/// served at all.
///
/// `node_key` MUST come from [`pinned_node_key`], never [`node_public_key`]:
/// the signature binds to it, so letting the dialled endpoint choose it is
/// letting a proxy choose what the operator signed for (#1824).
pub(crate) fn query_as_reader(
    base: &str,
    signer: &commonware_cryptography::ed25519::PrivateKey,
    node_key: &[u8],
    target: &str,
    query: serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    // serialized ONCE: the bytes that are signed are the bytes that are sent.
    // Re-serializing for the wire would let map ordering or float formatting
    // differ from what the digest covered, and the node would reject a request
    // this operator did in fact authorize.
    let body = serde_json::to_vec(&serde_json::json!({ "target": target, "query": query }))?;
    let mut request = client()?
        .post(format!("{base}{QUERY_READER_PATH}"))
        .header("content-type", "application/json")
        .body(body.clone());
    for (name, value) in
        noded::signed_req::request_headers(signer, "POST", QUERY_READER_PATH, node_key, &body)
    {
        request = request.header(name, value);
    }
    let resp = request
        .send()
        .map_err(|error| transport_failure(base, QUERY_READER_PATH, &error).to_string())?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("{QUERY_READER_PATH} rejected ({status}): {text}").into());
    }
    Ok(serde_json::from_str(&text)?)
}

/// This node's own consensus key, read from a plain, unauthenticated
/// `GET /v1/status`.
///
/// Every data-plane signature is BOUND to whatever key this returns
/// ([`noded::signed_req`]), so trusting it verbatim lets whatever answers on
/// `base` — including a proxy sitting in front of the real node, or one
/// substituting another node's key — choose what a caller's signature binds
/// to. [`pinned_node_key`] is the one thing that may still call this: the
/// first-contact read that seeds a trust-on-first-use pin. Nothing else
/// should sign against this value directly (#1824).
pub(crate) fn node_public_key(base: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let status = get_json(base, "/v1/status").map_err(|failure| failure.to_string())?;
    Ok(crate::config::unhex(
        status["public_key"].as_str().unwrap_or_default(),
    )?)
}

/// The node identity a signature may safely bind to — pinned from a source
/// the dialled endpoint does not control, never [`node_public_key`]'s plain
/// claim.
///
/// Two cases:
/// - `base` is one of the operator's OWN registered workspaces
///   ([`crate::cli_args::workspace_for_base`]): its `node.toml` already names
///   the key, read locally with no network round trip — nothing an answer on
///   `base` says can change it.
/// - anything else: trust-on-first-use, pinned beside the signing key
///   ([`crate::known_nodes`]). The first answer this CLI ever sees for `base`
///   is trusted and remembered; every answer after that must match it, or the
///   request is refused (reason: `node_key_mismatch`) unless the caller passed
///   `trust_node` to re-pin.
///
/// `key_path` is the user key about to sign: its pins live in its directory,
/// so the identity that trusts a url and the identity that signs against it
/// are one and the same file's neighbours.
pub(crate) fn pinned_node_key(
    key_path: &std::path::Path,
    base: &str,
    trust_node: bool,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let keys = key_path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", key_path.display()))?;
    Ok(pinned_node_key_in(keys, base, trust_node)?)
}

/// [`pinned_node_key`] over an explicit keys dir — split out so a test can
/// drive it against a temp dir instead of the operator's real keystore.
fn pinned_node_key_in(
    keys: &std::path::Path,
    base: &str,
    trust_node: bool,
) -> Result<Vec<u8>, String> {
    if let Ok(dir) = crate::cli_args::workspace_for_base(base) {
        let resolved = crate::config::resolve(&dir.join("node.toml"))?;
        return Ok(resolved.signer.public_key().as_ref().to_vec());
    }
    let reported = node_public_key(base).map_err(|error| error.to_string())?;
    match crate::known_nodes::pinned(keys, base)? {
        None => {
            crate::known_nodes::trust(keys, base, &reported)?;
            Ok(reported)
        }
        Some(pinned) if pinned == reported => Ok(pinned),
        Some(_) if trust_node => {
            crate::known_nodes::trust(keys, base, &reported)?;
            Ok(reported)
        }
        Some(_) => Err(format!(
            "{base} answered with a different node key than the one pinned for it \
             (reason: node_key_mismatch) — if you trust this change, re-run with --trust-node"
        )),
    }
}

/// Why a node-local read did not produce an answer.
///
/// The distinction is the whole point: "the node is not running" is an
/// ordinary state a read verb must render calmly, while "the node answered
/// something unexpected" must be surfaced. Collapsing both into one error is
/// how a 404 or a changed body shape comes to look like "nothing is there".
pub(crate) enum ReadFailure {
    /// nothing is listening on the node's HTTP surface, and what the operator
    /// should do about that — which is not one answer, see [`NotRunning`].
    Unreachable(NotRunning),
    /// the node was reached but the exchange failed (status or body).
    Rejected(String),
}

impl std::fmt::Display for ReadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadFailure::Unreachable(next) => write!(f, "{next}"),
            ReadFailure::Rejected(detail) => write!(f, "{detail}"),
        }
    }
}

/// What an operator should DO about a node that did not answer — the whole
/// content of the sentence, and it is not one sentence.
///
/// "start it with `ducktape node run`" was right for exactly one of these
/// three worlds. Under a launcher it is wrong twice over: that node is ALREADY
/// being started, in a restart loop, so a hand-typed `node run` is a second
/// process racing it for the port; and the reason the launcher's own attempt
/// keeps failing is in the launcher's log, which nothing used to mention.
pub(crate) enum NotRunning {
    /// no launcher has ever installed against this workspace — starting the
    /// node is the operator's own to do.
    Unsupervised,
    /// a launcher owns this workspace and its output is in a file that is
    /// there right now.
    Supervised(std::path::PathBuf),
    /// a launcher owns this workspace, but its output was never redirected
    /// into the workspace: it went wherever the unit that runs it sends stderr.
    SupervisedElsewhere,
}

impl std::fmt::Display for NotRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Both supervised arms open the same way and differ only in WHERE the
        // launcher's own reason can be read, so that clause is written once —
        // two copies of a sentence are two things to keep in agreement.
        const SUPERVISED: &str = "a launcher owns this workspace and is already restarting it, \
                                  so do not start a second one; its own reason is ";
        // Every arm opens on the same clause too: whatever follows it, the fact
        // a reader came here for is that nothing answered.
        write!(f, "the node is not running — ")?;
        match self {
            NotRunning::Unsupervised => write!(f, "start it with `ducktape node run`"),
            NotRunning::Supervised(log) => {
                write!(f, "{SUPERVISED}the last FATAL line in {}", log.display())
            }
            NotRunning::SupervisedElsewhere => write!(
                f,
                "{SUPERVISED}on the launcher's stderr, wherever the unit that runs it sends that"
            ),
        }
    }
}

/// Where an operator conventionally tees the launcher's stderr. The launcher
/// does not open this file itself, which is why its existence is checked and
/// never assumed: naming a path that is not there sends a reader to an empty
/// `cat` and costs more trust than saying less.
const LAUNCHER_LOG: &str = "launcher.log";

/// Classify a node that did not answer, by reading the launcher's OWN state on
/// disk — [`app_update::workspace::launcher_state_path`], the same function the
/// launcher composes that path with, so the two cannot drift apart.
///
/// Never a process scan: a pattern match over the process table finds an
/// editor with the word in its command line, and finds NOTHING at all in the
/// window between a launcher's restarts — which is precisely the window in
/// which someone is reading this sentence.
///
/// `None` is an address this CLI could not tie back to a registered
/// workspace; there is then nothing to have a launcher, so it is the plain
/// case.
pub(crate) fn not_running_in(workspace: Option<&std::path::Path>) -> NotRunning {
    let Some(workspace) = workspace else {
        return NotRunning::Unsupervised;
    };
    let supervised = app_update::workspace::launcher_state_path(workspace).is_file();
    if !supervised {
        return NotRunning::Unsupervised;
    }
    let log = workspace.join(LAUNCHER_LOG);
    match log.is_file() {
        true => NotRunning::Supervised(log),
        false => NotRunning::SupervisedElsewhere,
    }
}

/// [`not_running_in`] for a caller holding the node's http base rather than
/// its directory — every `/v1` lane, which dials a url and never knew the
/// workspace behind it.
///
/// The registry can answer "none" (a `--node` url pointing off this box) or
/// "several" (two networks both left on the default `http_listen`), and both
/// fall back to the plain sentence: a wrong launcher's log is worse than no
/// launcher's. A caller that HAS the directory should pass it to
/// [`not_running_in`] and skip this — `services::catalog_now` does.
fn not_running_at(base: &str) -> NotRunning {
    let workspace = crate::cli_args::workspace_for_base(base).ok();
    not_running_in(workspace.as_deref())
}

/// Why a request never produced a response — ONE discriminant over the only
/// three things that can be wrong at this boundary, so the sentence a person
/// reads is a `match` and not a ladder of `is_*()` booleans.
///
/// `is_connect()` alone was the old rule and it is NOT this question. It is
/// false for a connection dropped mid-exchange — precisely what a node
/// DRAINING does to an in-flight read — so the calm sentence was bypassed at
/// the one moment someone is most likely to be watching, and the same stopped
/// node answered `service list` with "the node is not running" and `user cred
/// list` with `error sending request for url (…)`.
///
/// The honest reading of a failed `send()` is that we never heard back. What a
/// person can DO about that splits three ways, and no further.
enum Unanswered {
    /// we could not even form the request: our own bug, or an input that got
    /// past its boundary check. Never "the node is down".
    Malformed,
    /// something IS on that port and did not answer in time — a wedged node is
    /// a different problem from a stopped one, and must not be reported as it.
    TimedOut,
    /// nothing usable answered: refused, reset, hung up mid-exchange.
    NoAnswer,
}

/// Decide which of the three a failed `send()` was. Pure.
fn why_unanswered(error: &reqwest::Error) -> Unanswered {
    let we_built_it_wrong = error.is_builder() || error.is_redirect();
    if we_built_it_wrong {
        return Unanswered::Malformed;
    }
    if error.is_timeout() {
        return Unanswered::TimedOut;
    }
    Unanswered::NoAnswer
}

/// Turn a failed `send()` into what to tell the operator. The one `match`.
///
/// `base` is carried only to answer "and what do I do about it" — the url
/// itself is never echoed (see below); it is the handle this CLI has on which
/// workspace the caller was dialing, and therefore on whether a launcher is
/// already doing the thing we would otherwise tell them to do.
pub(crate) fn transport_failure(base: &str, path: &str, error: &reqwest::Error) -> ReadFailure {
    match why_unanswered(error) {
        Unanswered::NoAnswer => ReadFailure::Unreachable(not_running_at(base)),
        // the url is deliberately not echoed: it adds nothing a reader can act
        // on, and every base here is one this CLI resolved itself.
        Unanswered::Malformed => {
            ReadFailure::Rejected(format!("{path} could not be requested: {error}"))
        }
        Unanswered::TimedOut => ReadFailure::Rejected(format!(
            "{path} timed out — the node is up but not answering"
        )),
    }
}

/// Read one node-local JSON surface over GET (the `/v1` read routes that are
/// not module queries, e.g. the volatile service catalog).
pub(crate) fn get_json(base: &str, path: &str) -> Result<serde_json::Value, ReadFailure> {
    let resp = client()
        .map_err(ReadFailure::Rejected)?
        .get(format!("{base}{path}"))
        .send()
        .map_err(|error| transport_failure(base, path, &error))?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(ReadFailure::Rejected(format!(
            "{path} rejected ({status}): {text}"
        )));
    }
    serde_json::from_str(&text).map_err(|error| {
        ReadFailure::Rejected(format!("{path} returned undecodable JSON: {error}"))
    })
}

/// POST one node-local JSON surface and return the decoded reply (the `/v1`
/// routes that are not module submits, e.g. service signaling).
pub(crate) fn post_json(
    base: &str,
    path: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    Ok(serde_json::from_str(&post(base, path, body)?)?)
}

/// One blocking POST of a JSON body, returning the response text or the node's
/// rejection string on a non-success status.
fn post(
    base: &str,
    path: &str,
    body: &serde_json::Value,
) -> Result<String, Box<dyn std::error::Error>> {
    post_with(base, path, body, None)
}

/// [`post`] carrying the node's operator credential — what a MUTATING route
/// wants from a caller that acts as the node rather than as a person.
fn post_with(
    base: &str,
    path: &str,
    body: &serde_json::Value,
    operator_token: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    // the SAME classifier the read lane uses: `submit`/`query` are how every
    // `user`/`agent`/`cred` verb reaches the node, and a down node used to
    // surface here as a raw `POST http://…: error sending request for url (…)`
    // while `service list` — one function away — said "the node is not running".
    let mut request = client()?.post(format!("{base}{path}")).json(body);
    if let Some(token) = operator_token {
        request = request.header(noded::admin::ADMIN_TOKEN_HEADER, token);
    }
    let resp = request
        .send()
        .map_err(|error| transport_failure(base, path, &error).to_string())?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("{path} rejected ({status}): {text}").into());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a listener bound only to learn a port nothing is on. Dropping it
    /// closes it, so the connect that follows is REFUSED — no sleep, no
    /// guessed port number.
    fn a_dead_port() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        format!("http://127.0.0.1:{port}")
    }

    /// BOTH lanes must speak the same sentence about a node that is not up.
    ///
    /// The read lane always did; the submit/query lane did not, so `user cred
    /// list` and `agent sched` answered a stopped node with a raw
    /// `POST http://127.0.0.1:8844/v1/query: error sending request for url (…)`
    /// while `service list` — one function away — said "the node is not
    /// running". This is the test that would have failed.
    #[test]
    fn both_lanes_say_the_node_is_not_running() {
        let base = a_dead_port();

        let read = get_json(&base, "/v1/status").expect_err("nothing listens");
        assert!(
            matches!(read, ReadFailure::Unreachable(_)),
            "the read lane must classify a refused connect: {read}"
        );

        let write = post(&base, "/v1/query", &serde_json::json!({}))
            .expect_err("nothing listens")
            .to_string();
        assert_eq!(
            write,
            read.to_string(),
            "the submit lane must say the same thing, not a near-miss of it"
        );
        assert!(
            write.starts_with("the node is not running"),
            "and it must say it first: {write}"
        );
        assert!(
            !write.contains("http://"),
            "a person is not helped by the url they did not type: {write}"
        );
    }

    /// A workspace nothing supervises, which is the world the old single
    /// sentence was written for. Kept as a MUST-PASS case: without it the two
    /// tests below would still pass against a renderer that had simply stopped
    /// saying `node run` to everybody.
    #[test]
    fn an_unsupervised_workspace_is_still_told_to_start_the_node() {
        let dir = tempfile::tempdir().expect("tempdir");
        let said = not_running_in(Some(dir.path())).to_string();
        assert_eq!(
            said, "the node is not running — start it with `ducktape node run`",
            "nothing here starts this node, so the operator must"
        );
    }

    /// What `ducktape-node-launcher install` leaves behind, built through the
    /// same composer the launcher writes it with — so a test cannot keep
    /// passing against a tree the launcher has stopped using.
    fn install_launcher_state(workspace: &std::path::Path) {
        let state = app_update::workspace::launcher_state_path(workspace);
        std::fs::create_dir_all(state.parent().expect("the state file is in a directory"))
            .expect("updates dir");
        std::fs::write(&state, "{}").expect("state.json");
    }

    /// THE BUG (#2531). A launcher is already restarting this node in a loop,
    /// so `node run` is a second process racing it for the port — and the
    /// reason its own attempts keep failing is in a file nothing used to name.
    #[test]
    fn a_launcher_managed_workspace_is_never_told_to_run_a_second_node() {
        let dir = tempfile::tempdir().expect("tempdir");
        install_launcher_state(dir.path());
        std::fs::write(dir.path().join(LAUNCHER_LOG), "FATAL: nope\n").expect("launcher.log");

        let said = not_running_in(Some(dir.path())).to_string();
        assert!(
            !said.contains("ducktape node run"),
            "a supervised node must not be handed a second `node run`: {said}"
        );
        assert!(
            said.contains(&dir.path().join(LAUNCHER_LOG).display().to_string()),
            "and it must name the log by a path the reader can open: {said}"
        );
    }

    /// The launcher does not open `launcher.log` itself — an operator tees its
    /// stderr there, and plenty do not. Naming a file that is not there sends
    /// the reader to an empty `cat`, which costs more than saying less.
    #[test]
    fn a_launcher_whose_output_went_elsewhere_names_no_file_at_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        install_launcher_state(dir.path());

        let said = not_running_in(Some(dir.path())).to_string();
        assert!(
            !said.contains("ducktape node run"),
            "it is still supervised: {said}"
        );
        assert!(
            !said.contains(LAUNCHER_LOG),
            "it must not invent a file it did not find: {said}"
        );
        assert!(
            said.contains("stderr"),
            "it must still say where to look instead: {said}"
        );
    }

    /// An address this CLI cannot tie back to a workspace has nothing to have
    /// a launcher — the plain sentence, never a guess.
    #[test]
    fn an_address_with_no_workspace_behind_it_gets_the_plain_sentence() {
        let said = not_running_in(None).to_string();
        assert_eq!(
            said,
            "the node is not running — start it with `ducktape node run`"
        );
    }

    /// The SHUTDOWN WINDOW, which `is_connect()` alone gets wrong: the socket
    /// accepts and is then dropped without a response — exactly what a node
    /// draining does to an in-flight read. `is_connect()` is false for that,
    /// so this used to degrade into the raw reqwest string at the one moment a
    /// person is most likely to be looking.
    ///
    /// Synchronized on the accept itself, not on a clock. It deliberately does
    /// not pin WHICH failure the stack reports — a hang-up races the request
    /// write, so it is a reset on one run and an incomplete message on the
    /// next, and BOTH are the same thing to the person reading the line. That
    /// race is exactly what made the previous `std::io::ErrorKind` rule flake:
    /// only one of the two shapes carries an io error at all.
    #[test]
    fn a_connection_dropped_mid_exchange_is_still_a_node_that_is_not_running() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let draining = std::thread::spawn(move || {
            // accept, then hang up without writing a byte.
            let (conn, _) = listener.accept().expect("the client connects");
            drop(conn);
        });

        let failure = get_json(&base, "/v1/status").expect_err("no response");
        draining.join().expect("the drain thread finishes");
        assert!(
            matches!(failure, ReadFailure::Unreachable(_)),
            "a hang-up during drain is the same operator condition: {failure}"
        );
    }

    /// The three ways a `send()` can fail are three DIFFERENT operator
    /// problems, and a wedged node must never be reported as a stopped one:
    /// "start it with `ducktape node run`" is wrong advice for a process that
    /// is already running and not answering.
    #[test]
    fn a_wedged_node_and_a_bad_url_do_not_borrow_the_stopped_nodes_sentence() {
        let timed_out = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_millis(1))
            .build()
            .expect("client")
            // TEST-NET-1 (RFC 5737): routable-looking and guaranteed dark, so
            // the connect stalls rather than being refused.
            .get("http://192.0.2.1:9/v1/status")
            .send()
            .expect_err("nothing answers on a reserved address");
        assert!(
            matches!(why_unanswered(&timed_out), Unanswered::TimedOut),
            "a timeout is not a stopped node"
        );

        // THE MISSING SCHEME IS THE WHOLE TEST, and it is also the only thing
        // keeping this offline: reqwest rejects a relative url at parse, so no
        // name is resolved and nothing is dialed. Do NOT "fix" this into
        // `http://…` — a bare word is not guaranteed to be unresolvable
        // (a search domain or an NXDOMAIN-hijacking resolver will happily hand
        // back an address for any label), so a scheme here turns a pure parse
        // test into a live request against whatever the run's DNS invents.
        // The word itself carries no meaning; it just has to not look like a
        // host somebody would try to reach.
        let malformed = reqwest::blocking::Client::new()
            .get("not-a-url/v1/status")
            .send()
            .expect_err("a bare word is not a url");
        assert!(
            matches!(why_unanswered(&malformed), Unanswered::Malformed),
            "an unbuildable request is not a stopped node"
        );
        assert!(
            !transport_failure("http://127.0.0.1:1", "/v1/status", &malformed)
                .to_string()
                .contains("not running"),
            "and it must not say so"
        );
    }

    /// ...and anything that is NOT that condition keeps its own words. A node
    /// that answers is never "not running", however unhappy the answer.
    #[test]
    fn a_node_that_answers_is_never_reported_as_down() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let serving = std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let (mut conn, _) = listener.accept().expect("the client connects");
            let mut scratch = [0u8; 1024];
            let _ = conn.read(&mut scratch);
            let _ = conn
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\n\r\nnope");
        });

        let failure = get_json(&base, "/v1/status").expect_err("a 500 is an error");
        serving.join().expect("the serve thread finishes");
        let ReadFailure::Rejected(detail) = failure else {
            panic!("a served 500 must not be reported as a stopped node");
        };
        assert!(detail.contains("500"), "{detail}");
    }

    /// A fake `GET /v1/status` responder whose reported `public_key` can be
    /// changed between requests (`set`) — the shape a real proxy takes when it
    /// substitutes another node's key mid-conversation. `Connection: close` so
    /// the shared client dials fresh each time instead of reusing a socket
    /// this thread already answered on.
    struct FakeStatus {
        key: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl FakeStatus {
        fn spawn(initial_key: Vec<u8>) -> (String, Self) {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            let base = format!("http://{}", listener.local_addr().expect("addr"));
            let key = std::sync::Arc::new(std::sync::Mutex::new(initial_key));
            let served = key.clone();
            std::thread::spawn(move || {
                use std::io::{BufRead as _, BufReader, Write as _};
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let mut reader = BufReader::new(stream.try_clone().expect("clone socket"));
                    let mut line = String::new();
                    // drain the request head; the body this endpoint reads is
                    // never more than headers.
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) if line == "\r\n" || line.is_empty() => break,
                            Ok(_) => {}
                        }
                    }
                    let hex = duckfs_core::to_hex(&served.lock().unwrap());
                    let body = format!("{{\"public_key\":\"{hex}\"}}");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
            });
            (base, Self { key })
        }

        fn set(&self, key: Vec<u8>) {
            *self.key.lock().unwrap() = key;
        }
    }

    /// the first answer this CLI ever sees for an unregistered node's url is
    /// trusted and remembered, and signs with it.
    #[test]
    fn a_first_contact_pin_signs_with_what_the_node_reports() {
        let duck = tempfile::TempDir::new().unwrap();
        let real_key = vec![0x11; 32];
        let (base, _server) = FakeStatus::spawn(real_key.clone());

        let pinned = pinned_node_key_in(duck.path(), &base, false).expect("first contact pins");
        assert_eq!(pinned, real_key);
    }

    /// the vulnerability #1824 fixes: a proxy or a substituted node answering
    /// with a DIFFERENT key than the one already trusted for this url must be
    /// refused, never silently re-signed against.
    #[test]
    fn a_later_mismatch_is_refused_without_trust_node() {
        let duck = tempfile::TempDir::new().unwrap();
        let real_key = vec![0x22; 32];
        let (base, server) = FakeStatus::spawn(real_key.clone());

        pinned_node_key_in(duck.path(), &base, false).expect("first contact pins");
        server.set(vec![0x99; 32]); // a proxy substitutes another node's key.

        let refused =
            pinned_node_key_in(duck.path(), &base, false).expect_err("a changed key is refused");
        assert!(refused.contains("node_key_mismatch"), "{refused}");
        // the pin itself must not have moved.
        assert_eq!(
            crate::known_nodes::pinned(duck.path(), &base).unwrap(),
            Some(real_key)
        );
    }

    /// `--trust-node` is the ONLY way an already-pinned key changes.
    #[test]
    fn trust_node_re_pins_to_whatever_is_reported_now() {
        let duck = tempfile::TempDir::new().unwrap();
        let real_key = vec![0x33; 32];
        let (base, server) = FakeStatus::spawn(real_key.clone());

        pinned_node_key_in(duck.path(), &base, false).expect("first contact pins");
        let rotated_key = vec![0x44; 32];
        server.set(rotated_key.clone());

        let pinned = pinned_node_key_in(duck.path(), &base, true).expect("--trust-node re-pins");
        assert_eq!(pinned, rotated_key);
        assert_eq!(
            crate::known_nodes::pinned(duck.path(), &base).unwrap(),
            Some(rotated_key)
        );
    }
}
