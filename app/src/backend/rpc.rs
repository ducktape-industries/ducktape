use super::*;

/// Sign and submit one module op, answering the height of the block that
/// INCLUDED it.
///
/// The height is the coordinate a follow-up read waits on: the node answers
/// acceptance, the derived read models fold behind the block loop, and a
/// reload in between reads an index that does not have the write yet
/// (`await_fold`). Callers that need it by hand still take it; every caller
/// gets it for free through [`note_module_block`] below.
pub(crate) async fn signed_write(
    rpc: &RpcClient,
    target: &str,
    payload: Vec<u8>,
    password: String,
) -> Result<u64, String> {
    if payload.is_empty() || payload.len() > ::node::MAX_PAYLOAD_BYTES {
        return Err(format!(
            "{target} transaction exceeds the signed payload limit"
        ));
    }
    let frame = sign_frame(target, &payload, password).await?;
    submit_raw_frame(rpc, target, frame)
        .await
        .map_err(|failure| failure.to_string())
}

/// A refusal the NODE authored, in the shape a view gets it. The rpc client
/// already split the token from the sentence, so nothing here parses text:
/// this is the one place the two names line up.
pub(crate) fn refused(error: ducktape_rpc::Error) -> view_wire::Refusal {
    view_wire::Refusal::new(error.reason(), error.message())
}

/// The write lane's own two answers, which are NOT the same answer: a node
/// that said no has decided, and re-sending the same op cannot change it; an
/// exchange that never completed leaves the op's fate unknown, so the view
/// must re-read before it retries. That difference is the reason a token
/// exists, so it is the token.
fn submit_refused(failure: ducktape_rpc::SubmitFailure) -> view_wire::Refusal {
    match failure {
        ducktape_rpc::SubmitFailure::Refused(detail) => {
            view_wire::Refusal::new("rejected", detail)
        }
        ducktape_rpc::SubmitFailure::Unresolved(detail) => {
            view_wire::Refusal::new("unresolved", detail)
        }
    }
}

/// The seat is locked, as a view is told: its own token, because "come back
/// after you unlock" and "this will never work" are different answers and a
/// view decides what to draw on that difference.
pub(crate) fn locked_seat() -> view_wire::Refusal {
    view_wire::Refusal::new(
        "session_locked",
        "the local user key is locked; enter its password",
    )
}

/// Sign and submit one module op WITH THE KEY ALREADY SEATED — the seat a
/// password opened in the launch window or Settings — or refuse while the
/// seat is locked. The kernel's `op.submit` for a module view: the view
/// names the target and the message, and never carries a password.
pub(crate) async fn seated_write(
    rpc: &RpcClient,
    target: &str,
    payload: Vec<u8>,
    required_blob: Option<[u8; 32]>,
) -> Result<u64, view_wire::Refusal> {
    let limit = ::node::MAX_PAYLOAD_BYTES - required_blob.map_or(0, |_| 32);
    if payload.is_empty() || payload.len() > limit {
        return Err(view_wire::Refusal::new(
            "too_large",
            format!("{target} transaction exceeds the signed payload limit"),
        ));
    }
    let frame = {
        let session = SIGNER.lock().await;
        let Some(signer) = session.as_ref() else {
            return Err(locked_seat());
        };
        signer.sign_with_blob(target, next_sequence(), &payload, required_blob)
    };
    submit_raw_frame(rpc, target, frame)
        .await
        .map_err(submit_refused)
}

/// A data-plane signer over the key ALREADY SEATED, or the locked refusal:
/// the kernel's reader queries and raw-bytes writes for a module view, which
/// carries no password of its own.
pub(crate) async fn seated_data_plane_signer(
    rpc: &RpcClient,
) -> Result<ducktape_rpc::WriteAuth, view_wire::Refusal> {
    let status = rpc.status().await.map_err(refused)?;
    let node_key = hex_decode(&status.public_key)
        .map_err(|error| view_wire::Refusal::new("malformed_reply", error))?;
    let key = {
        let session = SIGNER.lock().await;
        let Some(signer) = session.as_ref() else {
            return Err(locked_seat());
        };
        signer.key.clone()
    };
    Ok(std::sync::Arc::new(
        move |method: &str, path: &str, digest: &[u8; 32]| {
            ::node::signed_req::request_headers_digest(&key, method, path, &node_key, digest)
                .into_iter()
                .map(|(name, value)| (name.to_string(), value))
                .collect()
        },
    ))
}

/// Submit a frame signed ELSEWHERE — by a passkey or a wallet in the browser
/// (`authpage`), never this device's key — through the same funnel, so the
/// block it lands in is noted for the reads that follow.
pub(crate) async fn submit_raw_frame(
    rpc: &RpcClient,
    target: &str,
    frame: Vec<u8>,
) -> Result<u64, ducktape_rpc::SubmitFailure> {
    // no blob: an app frame carries its payload inline, so the wait is an
    // ordinary RPC's. Only a pack-bearing push needs a budget sized by bytes.
    let height = rpc.submit_frame(frame, 0).await?;
    note_module_block(rpc, target, height);
    Ok(height)
}

/// Every block this client KNOWS a module took and has not yet seen folded.
///
/// A view read must never answer behind something this client already knows
/// happened, and the caller that reads is generally not the one that learned
/// it: the autosave plans tick N+1 against a tree tick N wrote, a live resync
/// reloads on behalf of a push three layers up, and a save that fails halfway
/// leaves committed ops behind with nobody left holding the receipt. So the
/// height is kept where it is LEARNED — the two funnels every such fact passes
/// through, [`signed_write`] for this client's writes and `folded_update` for
/// the stream's — instead of being threaded through each seam in between,
/// which is one parameter per seam and a stale read wherever one is missed.
///
/// Keyed by ORIGIN as well as module because a height is a coordinate on ONE
/// chain, and this app points at as many chains as the user has networks —
/// the same reason `rpc_client`'s own cache is keyed that way.
///
/// An entry leaves only when a fold is OBSERVED at or past it: the tip is
/// monotonic, so a read after that can never fall behind it again, which makes
/// the steady state zero probes rather than one per read.
// ponytail: entries leave ONLY on an observed fold, so a node whose chain
// restarts under a running app (dev-box reset — heights come back below the
// recorded floor) charges each read of that module one bounded, failing wait
// until the app restarts. Comparing `/v1/status` tips is the upgrade if a
// reset chain ever stops being a dev-box-only event.
static SEEN_BLOCKS: Mutex<BTreeMap<(String, String), u64>> = Mutex::new(BTreeMap::new());

fn seen_blocks() -> std::sync::MutexGuard<'static, BTreeMap<(String, String), u64>> {
    SEEN_BLOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Record that `module` took a block at `height` — a write this client signed,
/// or an op its live stream delivered.
pub(crate) fn note_module_block(rpc: &RpcClient, module: &str, height: u64) {
    let key = (rpc.origin().to_string(), module.to_string());
    let mut seen = seen_blocks();
    let known = seen.entry(key).or_default();
    *known = (*known).max(height);
}

/// The block a read of `module`'s view owes itself — the newest one this client
/// knows about and has not seen folded. `None` = nothing outstanding.
fn unfolded_block(rpc: &RpcClient, module: &str) -> Option<u64> {
    seen_blocks()
        .get(&(rpc.origin().to_string(), module.to_string()))
        .copied()
}

/// Retire what the fold has now been seen to carry.
fn note_folded_through(rpc: &RpcClient, module: &str, height: u64) {
    let mut seen = seen_blocks();
    let key = (rpc.origin().to_string(), module.to_string());
    // ONLY if it is still the height that was waited on: a write landing
    // during the wait raises the floor, and dropping the entry outright would
    // discard a requirement nobody has met yet.
    if seen.get(&key).is_some_and(|known| *known <= height) {
        seen.remove(&key);
    }
}

/// Wait, briefly, for `module`'s fold to carry everything this client knows it
/// took ([`SEEN_BLOCKS`]) — the read-your-own-writes wait, at the READ rather
/// than at each of the writes.
///
/// Answers on the same terms as [`await_fold`]: `true` = the view is not
/// behind anything this client knows, which includes the ordinary case of
/// knowing nothing outstanding and waiting for nothing.
pub(crate) async fn await_seen_fold<Q: serde::Serialize>(
    rpc: &RpcClient,
    module: &str,
    probe: &Q,
) -> bool {
    let Some(height) = unfolded_block(rpc, module) else {
        return true;
    };
    await_fold(rpc, module, probe, height).await
}

/// How many times a post-write reload probes the module's fold before giving
/// up, and how long it waits between probes.
///
/// Bounded on purpose and short: the watermark can legitimately never arrive
/// (a boundary stamp wipes the tip, a module with no index guest never has
/// one), so this narrows the stale-read window — it never guarantees closing
/// it, and the caller's own correction stays the guarantee.
pub(crate) const FOLD_WAIT_PROBES: u32 = 5;
const FOLD_WAIT_STEP: Duration = Duration::from_millis(60);

/// Wait, briefly, for `module`'s fold to reach block `height` — the block that
/// accepted the caller's own write — so the reload that follows reads a view
/// containing it instead of one that predates it.
///
/// `submit_frame` answers ACCEPTANCE; the derived read models fold behind the
/// block loop. Everything upstream of this reload used to paper over that gap
/// by hand, and the hand-patch could only fix the one field it knew about.
///
/// `probe` is the module's cheapest view request: this reads the reply's fold
/// watermark and throws the body away, so the smallest arm is the right one.
///
/// Answers whether the fold got there. `false` covers three different facts —
/// the budget ran out, the node refused, or the module reports no tip at all
/// (no index guest, a fresh database, a boundary stamp wiped it) — and they
/// all mean the same thing to a caller: do not trust the reload, apply your own
/// correction. Unknown is never "not yet": waiting on it would spend the whole
/// budget for nothing.
pub(crate) async fn await_fold<Q: serde::Serialize>(
    rpc: &RpcClient,
    module: &str,
    probe: &Q,
    height: u64,
) -> bool {
    for probe_number in 0..FOLD_WAIT_PROBES {
        if probe_number > 0 {
            tokio::time::sleep(FOLD_WAIT_STEP).await;
        }
        let Ok((_reply, folded)) = rpc
            .view_folded::<Q, serde::de::IgnoredAny>(module, probe)
            .await
        else {
            return false;
        };
        let Some(folded) = folded else {
            return false;
        };
        if folded.reached_block(height) {
            note_folded_through(rpc, module, height);
            return true;
        }
    }
    false
}

/// THE session signer: this device's key, opened ONCE into this process, then
/// used to sign a frame per write.
///
/// Opening it runs argon2id over 64 MiB (`crates/keystore/src/userkey.rs`), so
/// the per-op process this replaced charged every reaction tap ~200-400 ms of
/// memory-hard KDF plus a spawn — and five taps in a row (reactions skip
/// `mutation_phase` on purpose) fanned out five concurrent 64 MiB jobs against
/// the render thread.
///
/// The decrypted key now lives in THIS address space rather than a child's.
/// That is a deliberate trade and it buys the app back its independence: the
/// old shape could not open a key at all without finding `ducktape` on disk,
/// so a fresh checkout, a build still linking, or a bundle shipped without its
/// sibling binary all surfaced as "check your install" while someone was
/// trying to unlock their own wallet. The app already holds the password in
/// state for the session, so the window in which a key is extractable from
/// this process was never the child's to protect.
static SIGNER: tokio::sync::Mutex<Option<Signer>> = tokio::sync::Mutex::const_new(None);

pub(super) struct Signer {
    /// The password this seat was opened with. The seat is taken where a
    /// wallet is unlocked — the launch window, Settings — and a signing verb
    /// only presents the password again: a different one is not this seat.
    password: Zeroizing<String>,
    key: ed25519::PrivateKey,
}

impl Signer {
    /// Open the key — the session's one argon2id pass, on a blocking thread
    /// because 64 MiB of memory-hard KDF on an async worker stalls every other
    /// task sharing it (the render thread's `Task` results included).
    ///
    /// The key path is an argument: the wallet is the WORKSPACE's, so whoever
    /// picked the network names the file, and a session can be driven against
    /// a fixture without touching this process's environment.
    pub(super) async fn unlock(key: PathBuf, password: Zeroizing<String>) -> Result<Self, String> {
        require_password(&password)?;
        let opening = {
            let password = password.clone();
            tokio::task::spawn_blocking(move || {
                keystore::userkey::open_user_key_at(&key, &password)
            })
        };
        let key = opening
            .await
            .map_err(|_| "opening this device's key did not finish".to_string())??;
        Ok(Self { password, key })
    }

    /// One signed op frame, ready for `/v1/submit/frame`.
    pub(super) fn sign(&self, target: &str, seq: u64, payload: &[u8]) -> Vec<u8> {
        self.sign_with_blob(target, seq, payload, None)
    }

    fn sign_with_blob(
        &self,
        target: &str,
        seq: u64,
        payload: &[u8],
        required_blob: Option<[u8; 32]>,
    ) -> Vec<u8> {
        // `::node`, not `node` — this backend has a module of its own by that
        // name, and it is the sibling that wins the bare path.
        ::node::encode_frame_with_blob(
            &self.key,
            seq,
            &sdk::Msg {
                target: target.to_string(),
                payload: payload.to_vec(),
            },
            required_blob,
        )
    }
}

async fn sign_frame(target: &str, payload: &[u8], password: String) -> Result<Vec<u8>, String> {
    let session = seated_signer(password).await?;
    let signer = session.as_ref().expect("the session was seated above");
    Ok(signer.sign(target, next_sequence(), payload))
}

/// The member consent an identity `AddKey` ticket carries: this device's key
/// signs the module's own `add_key_preimage` for `new_key` at its current
/// `generation` on `chain_id`, naming `account` and dying at `expires_at` —
/// the same seat [`sign_frame`] uses, so a ticket costs no argon2 pass of its
/// own.
pub(crate) async fn sign_add_key_consent(
    password: String,
    chain_id: &str,
    scheme: identity::KeyScheme,
    new_key: &[u8],
    generation: u64,
    account: u64,
    expires_at: u64,
) -> Result<identity::Authorizer, String> {
    let session = seated_signer(password).await?;
    let signer = session.as_ref().expect("the session was seated above");
    Ok(workspace_config::ed25519_authorizer(
        &signer.key,
        chain_id,
        scheme,
        new_key,
        generation,
        account,
        expires_at,
    ))
}

/// Take the session seat: the key at `path`, opened under `password`. THE
/// one place a key is opened for signing — the wallet ceremonies call it with
/// the workspace's key file, having just proved the password on it — so a
/// signing verb never has to know which workspace it is signing for, and a
/// network switch that drops the seat ([`lock_signer`]) leaves nothing that
/// could sign as the previous network's wallet. Answers the public key it
/// opened, in hex: the identity the session now signs as.
pub(crate) async fn seat_signer(
    path: PathBuf,
    password: Zeroizing<String>,
) -> Result<String, String> {
    let signer = Signer::unlock(path, password).await?;
    let pubkey = hex_encode(signer.key.public_key().as_ref());
    *SIGNER.lock().await = Some(signer);
    Ok(pubkey)
}

/// Sign one data-plane request with the key ALREADY SEATED — the seat the
/// action before this one opened under the person's password — or `None`
/// while the seat is locked. For a caller that has no password of its own:
/// the huddle's call socket is a subscription the app runs `when
/// huddle_joined`, and the join that set that flag is what seated the key.
pub(crate) async fn seated_request_headers(
    method: &str,
    path_and_query: &str,
    node_key: &[u8],
    body: &[u8],
) -> Option<[(&'static str, String); 3]> {
    let session = SIGNER.lock().await;
    let signer = session.as_ref()?;
    Some(::node::signed_req::request_headers(
        &signer.key,
        method,
        path_and_query,
        node_key,
        body,
    ))
}

/// Mint gateway caller authority without exposing the seated key to a view.
pub(crate) async fn seated_gateway_proof(
    publisher: &[u8],
    head: &gateway::ProxyRequestHead,
    body: &[u8],
) -> Option<gateway::UserPop> {
    let session = SIGNER.lock().await;
    let signer = session.as_ref()?;
    let ts = ::node::signed_req::now_secs();
    let preimage = gateway::caller_pop_preimage(publisher, head, &gateway::body_digest(body), ts);
    Some(gateway::UserPop {
        key: signer.key.public_key().as_ref().to_vec(),
        ts,
        sig: signer
            .key
            .sign(gateway::GATEWAY_CALLER_NS, &preimage)
            .as_ref()
            .to_vec(),
    })
}

/// This node's own public key — the bytes a data-plane signature is bound to, so
/// a proof minted for one node cannot be replayed at another. Read off the
/// node's own `status`, which is where every other signing caller reads it.
pub(crate) async fn node_public_key(rpc: &str) -> Result<Vec<u8>, String> {
    let status = crate::backend::rpc_client(rpc)?
        .status()
        .await
        .map_err(|error| error.to_string())?;
    crate::backend::hex_decode(&status.public_key)
}

/// The session seat, when `password` is the one it was taken with. The lock
/// is what makes the seat singular — a burst of reactions shares one opened
/// key instead of racing five argon2 passes into it. No seat, or another
/// password, is the locked state: the launch window and Settings are where a
/// seat is taken, never a write that happened to carry a password.
/// Confirm the current user unlocked the signing seat for this action.
pub(crate) async fn require_seated_signer(password: String) -> Result<(), String> {
    drop(seated_signer(password).await?);
    Ok(())
}

async fn seated_signer(
    password: String,
) -> Result<tokio::sync::MutexGuard<'static, Option<Signer>>, String> {
    require_password(&password)?;
    let password = Zeroizing::new(password);
    let session = SIGNER.lock().await;
    let seated = session
        .as_ref()
        .is_some_and(|signer| signer.password == password);
    if !seated {
        return Err("the local user key is locked; enter its password".into());
    }
    Ok(session)
}

/// Retire the session signer — the `Lock` button's other half. Clearing
/// `password` in state stops the app from signing; this drops the opened KEY,
/// so nothing in this process can sign until a password re-opens it.
pub async fn lock_signer() -> bool {
    SIGNER.lock().await.take().is_some()
}

/// An empty password is the locked state, not a refusal to explain.
pub(crate) fn require_password(password: &str) -> Result<(), String> {
    if password.is_empty() {
        return Err("the local user key is locked; enter its password".into());
    }
    Ok(())
}

/// The identity this session is about: the pubkey of the picked network's
/// wallet, or `None` for a network picked with no key to sign as. Set where
/// the identity is decided — a network pick reads the workspace's active
/// wallet without a password ([`super::load_wallets`]), and the wallet
/// ceremonies replace it with the key they just opened or sealed — never read
/// lazily off a global path, because there is no global key: a wallet is an
/// identity ON a network, kept in that network's workspace.
static LOCAL_USER_KEY: tokio::sync::RwLock<Option<Vec<u8>>> = tokio::sync::RwLock::const_new(None);

pub(crate) async fn local_user_key() -> Option<Vec<u8>> {
    LOCAL_USER_KEY.read().await.clone()
}

/// Replace the session's identity — a network pick, a wallet ceremony.
pub(crate) async fn set_local_user_key(reading: Option<Vec<u8>>) {
    *LOCAL_USER_KEY.write().await = reading;
}

/// The identity WITHOUT waiting — the update thread's synchronous folds
/// cannot await. `None` covers a held write lock; by any reaction tap the
/// pick that set it is long done.
pub(crate) fn cached_user_key() -> Option<Vec<u8>> {
    LOCAL_USER_KEY
        .try_read()
        .ok()
        .and_then(|reading| reading.clone())
}

/// A key file's pubkey, read WITHOUT its password — the pubkey rides in the
/// clear inside the encrypted line precisely so a pick can say who it is
/// about to sign as while the key is locked. A parse failure (an unreadable
/// or non-v1 file) is `None`, the same answer as no key at all.
pub(crate) fn pubkey_of_key_file(path: &Path) -> Option<Vec<u8>> {
    keystore::userkey::read_user_key_file(path)
        .ok()
        .map(|encrypted| encrypted.pubkey)
}

/// The app's own preferences file — `prefs.json` in the app's config
/// directory ([`super::app_dirs::config_dir`]), never under the ducktape
/// home: the home holds workspaces and nothing else. Never wire state: purely
/// this device's view preferences.
fn prefs_path() -> Option<PathBuf> {
    super::app_dirs::config_dir()
        .ok()
        .map(|dir| dir.join("prefs.json"))
}

pub(crate) fn read_prefs() -> serde_json::Value {
    let Some(path) = prefs_path() else {
        return serde_json::json!({});
    };
    std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

pub(crate) fn write_prefs(prefs: &serde_json::Value) -> bool {
    let Some(path) = prefs_path() else {
        return false;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(bytes) = serde_json::to_vec_pretty(prefs) else {
        return false;
    };
    std::fs::write(&path, bytes).is_ok()
}

/// The persisted appearance override. DEVICE-global, not per-endpoint:
/// appearance is a property of the person's eyes and room, not of a workspace.
pub async fn load_appearance() -> crate::Appearance {
    match read_prefs()["appearance"].as_str() {
        Some("light") => crate::Appearance::Light,
        Some("dark") => crate::Appearance::Dark,
        _ => crate::Appearance::System,
    }
}

/// `System` is the absence of an override: it clears the key rather than
/// storing a third value, so `load_appearance` needs no "system" spelling.
pub async fn save_appearance(mode: crate::Appearance) -> bool {
    let mut prefs = read_prefs();
    match mode {
        crate::Appearance::System => {
            if let Some(prefs) = prefs.as_object_mut() {
                prefs.remove("appearance");
            }
        }
        crate::Appearance::Light => prefs["appearance"] = serde_json::json!("light"),
        crate::Appearance::Dark => prefs["appearance"] = serde_json::json!("dark"),
    }
    write_prefs(&prefs)
}

/// The prefs key a network's per-network readings (`networks[<key>]`: its
/// last-used stamp) sit under: the chain id when the endpoint is served by a
/// workspace on this device — the one name that survives a port change —
/// else the canonical endpoint of a remote.
pub(crate) fn network_key(rpc: &str) -> String {
    match workspace_at(rpc) {
        Some((chain_id, _)) => chain_id,
        None => canonical_endpoint(rpc.to_string()),
    }
}

/// WHERE A NETWORK'S WALLETS LIVE ON THIS DEVICE. A wallet is an identity ON
/// a network, so its keystore is keyed by the network — and every network
/// this device talks to has one, whether or not it also runs that network's
/// node here:
///
/// - a network this device hosts keeps its keys in the node's workspace, as
///   the CLI does (`<workspace>/keys/`);
/// - a network reached only through a remote endpoint keeps them under
///   `<ducktape home>/remotes/<chain id>/keys/` — the chain id, not the
///   endpoint, because the same network dialled through a different tunnel or
///   port is the same identity.
///
/// The keystore used to be the workspace and nothing else, which made a device
/// that hosts no node a device that can hold no key: it dropped into the
/// console read-only, silently, with no way to create or unlock anything. The
/// launch window's key screens now open for a remote exactly as for a local
/// network.
///
/// A remote's chain id is what its `/v1/status` says; [`note_remote_chain`]
/// records it when the launch window loads the keystore, and until it has,
/// the root is a refusal — "not yet reached", never a guess.
pub(crate) fn keystore_root(rpc: &str) -> Result<PathBuf, String> {
    if let Some((_, workspace)) = workspace_at(rpc) {
        return Ok(workspace);
    }
    let endpoint = canonical_endpoint(rpc.to_string());
    let chain_id = remote_chains()
        .get(&endpoint)
        .cloned()
        .ok_or_else(|| "this node has not answered which network it serves yet".to_string())?;
    remote_keystore_root(&workspace_config::ducktape_home()?, &chain_id)
}

/// `<home>/remotes/<chain id>`. The chain id is a `--name` plus a hash and may
/// carry a path separator; it is made inert the way
/// `workspace_config::default_workspace_dir` does for workspaces.
pub(crate) fn remote_keystore_root(home: &Path, chain_id: &str) -> Result<PathBuf, String> {
    let name = chain_id.replace(std::path::MAIN_SEPARATOR, "-");
    if name.is_empty() || name == "." || name == ".." {
        return Err(format!("chain id {chain_id:?} is not a directory name"));
    }
    Ok(home.join("remotes").join(name))
}

/// Endpoint → chain id, for the remotes this session has reached. Learned
/// once per endpoint from the node's own status, at the keystore load.
static REMOTE_CHAINS: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

fn remote_chains() -> std::sync::MutexGuard<'static, BTreeMap<String, String>> {
    REMOTE_CHAINS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Record which network `rpc` serves, so [`keystore_root`] can name its
/// keystore. An empty chain id (a node serving no chain yet) records nothing.
pub(crate) fn note_remote_chain(rpc: &str, chain_id: &str) {
    if chain_id.is_empty() {
        return;
    }
    remote_chains().insert(canonical_endpoint(rpc.to_string()), chain_id.to_string());
}

/// One named wallet's key file inside a workspace's keystore — THE join, so
/// the charset check (`[a-z0-9][a-z0-9._-]*`, at most 41 chars) lives here and
/// every caller inherits it. A name is untrusted even when nobody typed it:
/// the `active` pointer is an ordinary file anyone can garble, and a `/` or
/// `..` in it would walk the key path straight out of the keystore.
pub(crate) fn keystore_key_path(workspace: &Path, name: &str) -> Result<PathBuf, String> {
    keystore::wallet::valid_name(name)?;
    Ok(keystore::wallet::key_file(workspace, name))
}

/// The `active` pointer's wallet NAME in a workspace — empty when its keystore
/// holds none. The pointer is the one place that decides which key signs on
/// that network.
pub(crate) fn active_wallet_name(workspace: &Path) -> String {
    keystore::wallet::active_name(workspace).unwrap_or_default()
}

/// The signing key file a session on `rpc` reads its identity off: the
/// explicit `DUCKTAPE_USER_KEY` override, else the active wallet of that
/// network's keystore ([`keystore_root`]). No active wallet is a refusal, not
/// a guess — the launch window is where one is picked.
pub(crate) fn session_key_path(rpc: &str) -> Result<PathBuf, String> {
    if let Some(path) = env_user_key() {
        return Ok(path);
    }
    let root = keystore_root(rpc)?;
    let name = active_wallet_name(&root);
    if name.is_empty() {
        return Err("no active wallet — pick one in the launch window".to_string());
    }
    keystore_key_path(&root, &name)
}

/// The rig override: `DUCKTAPE_USER_KEY` names a key file outright, and no
/// keystore is consulted while it is set.
pub(crate) fn env_user_key() -> Option<PathBuf> {
    keystore::wallet::env_user_key()
}

pub(crate) fn bounded_text(value: String, field: &str, limit: usize) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.len() > limit || value.chars().any(|character| character == '\0') {
        return Err(format!("{field} must be between 1 and {limit} bytes"));
    }
    Ok(value.to_string())
}

/// One voice at the surface. The global banner prints whatever reaches an
/// `AppError`/`HydrationError`, so the known developer diagnostics — CLI spawn
/// chatter, key paths, argv timeouts, serde parse positions — translate to a
/// user sentence here. A module's own sentence flows through untouched.
///
/// Cause before symptom. The AEAD cannot tell a wrong password from a damaged
/// file, so the password sentence has to serve both — but it must be REACHED,
/// and a broader test placed above it swallows the case it was written for.
///
/// THE ORDER IS THE CONTRACT, and the tests beside this pin every overlap that
/// decides one. It is not a hypothetical: a helper-name test used to sit on top
/// here, and because every refusal a helper reported arrived WRAPPED in text
/// naming the helper, it swallowed the two specific branches beneath it — a
/// wrong password was reported to the user as a broken install.
pub(crate) fn user_error(message: String) -> String {
    let password_refused = message.contains("corrupt or wrong password");
    if password_refused {
        return "That password did not open this device's key. Check it and try again.".into();
    }
    let key_unreadable = message.contains("local user key") || message.contains("wallet name");
    if key_unreadable {
        return "This device's user key is missing or unreadable. Check Settings.".into();
    }
    let node_slow = message.contains("timed out");
    if node_slow {
        return "The node did not answer in time. Retry in a moment.".into();
    }
    let reply_garbled = message.contains("invalid type")
        || message.contains("while parsing")
        || message.contains("at line ")
        || message.contains("non-UTF-8");
    if reply_garbled {
        return "The node sent a reply this app could not read. Reload and retry.".into();
    }
    message
}

pub(crate) fn app_error(message: String) -> AppError {
    message.into()
}

pub(crate) fn retry_delay(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(4);
    Duration::from_secs(1_u64 << exponent)
}

#[cfg(test)]
mod tests {
    use super::user_error;
    use commonware_cryptography::Signer as _;

    #[test]
    fn seated_signer_binds_the_declared_blob_for_an_arbitrary_module() {
        let signer = super::Signer {
            password: zeroize::Zeroizing::new(String::new()),
            key: commonware_cryptography::ed25519::PrivateKey::from_seed(73),
        };
        let frame =
            signer.sign_with_blob("independent-app", 7, b"opaque payload", Some([0xab; 32]));
        let (_, msg, digest) = ::node::decode_frame_with_blob(&frame).unwrap();
        assert_eq!(msg.target, "independent-app");
        assert_eq!(msg.payload, b"opaque payload");
        assert_eq!(digest, Some([0xab; 32]));
        let mut changed = frame;
        let digest_start = changed.len() - 64 - 32;
        changed[digest_start] ^= 1;
        assert!(::node::decode_frame_with_blob(&changed).is_err());
        let ordinary = signer.sign("independent-app", 8, b"ordinary");
        assert_eq!(::node::decode_frame_with_blob(&ordinary).unwrap().2, None);
    }

    /// EVERY refusal a helper reported used to arrive WRAPPED in text naming
    /// that helper — `Signer::reap` built "ducktape signer refused the
    /// transaction: {stderr}" precisely so the stderr survived the trip — so a
    /// helper-name test on top swallowed every specific cause beneath it, and
    /// the two branches written for exactly those causes were dead code. The
    /// signer is gone, but the shape that made this possible is not: anything
    /// that wraps a cause in context re-creates it. This pins the order.
    #[test]
    fn the_cause_outranks_the_context_that_carried_it() {
        assert_eq!(
            user_error(
                "could not start the ducktape agent terminal: FATAL: corrupt or wrong password"
                    .into()
            ),
            "That password did not open this device's key. Check it and try again."
        );
        assert_eq!(
            user_error(
                "the keystore operation did not finish: local user key is unreadable".into()
            ),
            "This device's user key is missing or unreadable. Check Settings."
        );
    }

    /// A node that went quiet has its own sentence, reached only after the key
    /// causes above it.
    #[test]
    fn a_slow_node_keeps_its_own_sentence() {
        assert_eq!(
            user_error("submit timed out awaiting finalization".into()),
            "The node did not answer in time. Retry in a moment."
        );
        assert_eq!(
            user_error("the node sent this at line 4: invalid type".into()),
            "The node sent a reply this app could not read. Reload and retry."
        );
    }

    /// The contract the whole function exists to keep: a module's own sentence
    /// is already addressed to this person, so it passes through untouched.
    #[test]
    fn a_module_sentence_flows_through_untouched() {
        let own = "That channel no longer exists.";
        assert_eq!(user_error(own.into()), own);
    }
}
