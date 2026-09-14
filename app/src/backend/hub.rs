use super::*;

pub fn ceremony_retirement(
    welcome_closed: bool,
    account_closed: bool,
) -> crate::CeremonyRetirement {
    match (welcome_closed, account_closed) {
        (true, _) => crate::CeremonyRetirement::Welcome,
        (false, true) => crate::CeremonyRetirement::Account,
        (false, false) => crate::CeremonyRetirement::Keep,
    }
}

/// One row of the launch window's network list. `id` is the row's stable
/// device-local key: the chain id for a materialized network (the same id the
/// CLI registry lists), the canonical endpoint for a saved remote. `chain_id`
/// is the network's own identity out of `network.toml` (empty for a remote the
/// device holds no descriptor for) — it is also the exact `-n` selector later
/// CLI calls need.
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct HubNetwork {
    pub id: String,
    pub chain_id: String,
    pub name: String,
    pub endpoint: String,
    pub kind: String,
    pub last_used: i64,
    /// The liveness reading, merged in by `probe_known_networks`. `probed`
    /// distinguishes "not answered yet" from a measured dead node — the row
    /// must not claim either before the probe returns.
    pub probed: bool,
    pub live: bool,
    pub height: i64,
}

/// One probe answer. Never an error: a node that does not answer IS the
/// reading (`live == false`), not a failure to hide behind a banner.
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct HubProbe {
    pub id: String,
    pub live: bool,
    pub height: i64,
}

/// One wallet row the launch window lists, straight off `keystore::wallet`.
/// `state` is the key file's own reading (`encrypted` | `unreadable`), so a
/// row says whether it can sign before anyone types a password into it.
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct WalletInfo {
    pub name: String,
    pub pubkey: String,
    pub state: String,
    pub active: bool,
}

/// The launch window's boot read: the known-network list and the row it
/// opens on. No wallets here — a wallet is an identity ON a network, kept in
/// that network's workspace, so the keystore is read once a network is picked
/// ([`load_wallets`]).
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct HubState {
    pub networks: Vec<HubNetwork>,
    pub preselect: String,
}

/// A picked network's keystore: its wallet rows, why the listing is empty when
/// it FAILED rather than being empty, and whether the keystore could be
/// NAMED at all — a remote whose node never answered which network it serves
/// has no keystore to open, and the pick stays where it is with that error.
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct WalletList {
    pub wallets: Vec<WalletInfo>,
    pub error: String,
    pub keystore: bool,
}

/// Which step a picked network's keystore sends the launch window to, as the
/// discriminant the handler branches on once: rows are the unlock surface, an
/// empty keystore mints the device key, and a keystore that could not be named
/// (the remote never answered) keeps the pick on screen with its error. There
/// is no silent read-only door: every way into the console goes past a key,
/// and "Continue read-only" is a button the person presses.
pub fn wallet_door(list: &WalletList) -> crate::WalletDoor {
    match (list.keystore, list.wallets.is_empty()) {
        (false, _) => crate::WalletDoor::Unreached,
        (true, true) => crate::WalletDoor::Password,
        (true, false) => crate::WalletDoor::Wallets,
    }
}

/// The keystore's own reading of a key file (`absent` | `encrypted` |
/// `unreadable`) — the same classification the wallet listing shows, computed
/// in-process (no subprocess, no password).
pub(crate) fn key_state_of(path: &Path) -> String {
    keystore::userkey::key_file_state(path).as_str().into()
}

/// A network's display name is the human half of its chain id: `demo#a1b2`
/// reads `demo`. A remote row falls back to its endpoint sans scheme.
fn display_name(chain_id: &str, fallback: &str) -> String {
    let named = chain_id.split('#').next().unwrap_or_default();
    if !named.is_empty() {
        return named.to_string();
    }
    fallback
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .to_string()
}

/// The known-network list: every workspace directory under the ducktape home
/// plus every saved remote endpoint, most recently used first. A directory IS
/// a network on this device — there is no device-side forgetting of one;
/// deleting the directory is how it leaves.
pub(crate) fn known_networks() -> Vec<HubNetwork> {
    let prefs = read_prefs();
    let stamps = &prefs["networks"];
    let mut rows: Vec<HubNetwork> = workspaces()
        .into_iter()
        .map(|(chain_id, dir)| {
            let endpoint = workspace_endpoint(&dir).unwrap_or_default();
            HubNetwork {
                name: display_name(&chain_id, &chain_id),
                endpoint,
                kind: "local".into(),
                last_used: stamps[&chain_id]["last_used"].as_i64().unwrap_or(0),
                id: chain_id.clone(),
                chain_id,
                probed: false,
                live: false,
                height: -1,
            }
        })
        .collect();
    let remotes = prefs["saved_remotes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for remote in remotes {
        let Some(endpoint) = remote["endpoint"].as_str() else {
            continue;
        };
        let already_local = rows.iter().any(|row| row.endpoint == endpoint);
        if already_local {
            continue;
        }
        rows.push(HubNetwork {
            id: endpoint.to_string(),
            chain_id: String::new(),
            name: display_name("", endpoint),
            endpoint: endpoint.to_string(),
            kind: "remote".into(),
            last_used: stamps[endpoint]["last_used"].as_i64().unwrap_or(0),
            probed: false,
            live: false,
            height: -1,
        });
    }
    rows.sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.id.cmp(&b.id)));
    rows
}

/// The row the list preselects: the most recently used (the list is sorted
/// by it), else the first row.
fn preselect_id(rows: &[HubNetwork]) -> String {
    rows.first().map(|row| row.id.clone()).unwrap_or_default()
}

/// Which step the launch window opens on: an empty keystore means the
/// password step (the device key is minted under it), anything else lands on
/// the wallet list — the unlock surface. An unreadable key renders its refusal
/// plate on its own row rather than on a separate screen.
pub fn hub_entry_step(wallets: Vec<WalletInfo>) -> crate::HubStep {
    if wallets.is_empty() {
        crate::HubStep::Password
    } else {
        crate::HubStep::Wallets
    }
}

/// The name an auto-minted device key gets: this host's name, in the
/// keystore's grammar; `device` when the host has none to give. Asked of
/// the kernel (`gethostname`), not `/etc/hostname` — macOS has no such file
/// and a GUI app inherits no `HOSTNAME`.
pub fn device_key_name() -> String {
    let host = gethostname::gethostname().to_string_lossy().into_owned();
    let name = keystore::wallet::sanitize_name(host.trim());
    if name.is_empty() {
        return "device".to_string();
    }
    name
}

/// The row the wallet list preselects: the active wallet, else the first.
pub fn preselect_wallet(wallets: Vec<WalletInfo>) -> String {
    wallets
        .iter()
        .find(|row| row.active)
        .or_else(|| wallets.first())
        .map(|row| row.name.clone())
        .unwrap_or_default()
}

/// A refreshed list keeps the user's selection when its row survived, else
/// falls back to the fresh preselection.
pub fn refreshed_hub_selection(
    networks: Vec<HubNetwork>,
    current: String,
    preselect: String,
) -> String {
    let survives = networks.iter().any(|row| row.id == current);
    match survives {
        true => current,
        false => preselect,
    }
}

/// The selected row's endpoint, or empty when the selection no longer names
/// a row (a forget can race the click).
pub fn selected_network_endpoint(networks: Vec<HubNetwork>, id: String) -> String {
    networks
        .into_iter()
        .find(|row| row.id == id)
        .map(|row| row.endpoint)
        .unwrap_or_default()
}

/// The selected row's display name — what the wallet screens call the
/// network whose keystore they show — or empty when the selection no longer
/// names a row.
pub fn selected_network_name(networks: Vec<HubNetwork>, id: String) -> String {
    networks
        .into_iter()
        .find(|row| row.id == id)
        .map(|row| row.name)
        .unwrap_or_default()
}

/// The create ceremony's refusal, or empty when the pair is acceptable —
/// the same floor the CLI enforces (8 scalar chars).
pub fn password_problem(password: &str, confirm: &str) -> String {
    if password.chars().count() < 8 {
        return "The password needs at least 8 characters.".into();
    }
    if password != confirm {
        return "The passwords do not match.".into();
    }
    String::new()
}

/// The close/focus target for a window that may not be open. `Some(id)` names
/// it; `None` yields a fresh id that names no window. The native window
/// manager ignores commands targeting an id it does not hold.
pub fn window_target(current: Option<crate::shell::WindowKey>) -> crate::shell::WindowKey {
    current.unwrap_or_else(crate::shell::WindowKey::unique)
}

/// [`window_target`] gated on a bool: while `keep` holds, yields a fresh id
/// (a no-op close); once it does not, names the window. How a branch-free
/// fold spells "close the huddle window only if the huddle ended".
pub fn window_target_unless(
    keep: bool,
    current: Option<crate::shell::WindowKey>,
) -> crate::shell::WindowKey {
    if keep {
        crate::shell::WindowKey::unique()
    } else {
        window_target(current)
    }
}

/// What the status item's "Open" row has to do, as the discriminant the
/// handler branches on once. Closing a window no longer ends the process, so
/// the daemon can be running connected with nothing tracked — an ordinary
/// state, not "never signed in". Routing that case through the launch window
/// (`onboarding_opened` always re-runs `hub_state()`, which resets `hub_step`
/// to the network picker) sends a connected user back to network selection
/// for merely closing the console; routing it through the console instead
/// reconnects from `rpc` the way a fresh pick does (#1782). A window already
/// tracked is always raised regardless of connection state — [`window_target`]
/// on an empty slot names a FRESH id, whose focus is a no-op, so a raise-only
/// row would do nothing with both slots empty.
///
/// Its own type: a third arm here would force a meaningless one onto
/// [`huddle_summon`]'s `WindowSummon` match, which has no console to reconnect.
pub fn tray_open_action(network_open: bool, window_tracked: bool) -> crate::TrayOpen {
    match (network_open, window_tracked) {
        (true, true) => crate::TrayOpen::Raise,
        (false, true) => crate::TrayOpen::Raise,
        (true, false) => crate::TrayOpen::Console,
        (false, false) => crate::TrayOpen::Launch,
    }
}

/// Put the call's window in front of you: open one when none is up, raise the
/// one that is. The LIVE pill means exactly this, and does not know or care
/// which case it is in.
///
/// Closing that window is NOT leaving, so this is the ordinary way back into a
/// call that is still running behind your work.
pub fn huddle_summon(huddle: Option<crate::shell::WindowKey>) -> crate::WindowSummon {
    match huddle.is_none() {
        true => crate::WindowSummon::Open,
        false => crate::WindowSummon::Raise,
    }
}

/// Does this close end the process? Only where the daemon has nowhere else to
/// live: on a Mac it goes on in the status item with no window at all, but off
/// macOS the native tray has no status item,
/// so a window is the only handle on the process and closing the last one
/// must leave — a daemon nobody can reach is a leak, not a menu-bar app. The
/// huddle window is deliberately not a survivor: a lone call window never
/// keeps the daemon alive after its console is gone.
pub fn last_window_closed_exits(
    console: Option<crate::shell::WindowKey>,
    onboarding: Option<crate::shell::WindowKey>,
) -> bool {
    let has_status_item = cfg!(target_os = "macos");
    let a_window_remains = console.is_some() || onboarding.is_some();
    !has_status_item && !a_window_remains
}

/// Clear a tracked window id when it is the one that closed.
pub fn without_window(
    current: Option<crate::shell::WindowKey>,
    closed: crate::shell::WindowKey,
) -> Option<crate::shell::WindowKey> {
    match current == Some(closed) {
        true => None,
        false => current,
    }
}

/// the synthetic row a `DUCKTAPE_USER_KEY` override renders as.
const ENV_WALLET: &str = "env";

/// Is the keystore bypassed? The `DUCKTAPE_USER_KEY` override is what
/// SYNTHESIZES the `env` row, so it is also the only condition under which the
/// name `env` means that row. Without this test a keystore wallet legitimately
/// named `env` would resolve to the override's file — the app would show one
/// identity and sign as another.
fn env_key_override() -> bool {
    std::env::var_os("DUCKTAPE_USER_KEY").is_some()
}

/// A picked network's keystore rows. `DUCKTAPE_USER_KEY` bypasses the
/// keystore with one synthetic row so rigs and huddle lanes get the same
/// single screen, on a remote as much as on a workspace. A failure is
/// returned, never flattened to an empty list: "no wallets" sends the launch
/// window to the create ceremony, and sending someone who HAS wallets there
/// because a directory would not read is a lie with no way back.
fn wallet_rows(rpc: &str) -> Result<WalletList, String> {
    if let Some(path) = env_user_key() {
        return Ok(WalletList {
            wallets: vec![WalletInfo {
                name: ENV_WALLET.into(),
                pubkey: String::new(),
                state: key_state_of(&path),
                active: true,
            }],
            error: String::new(),
            keystore: true,
        });
    }
    let listed = keystore::wallet::list(&keystore_root(rpc)?)?;
    Ok(WalletList {
        wallets: listed
            .into_iter()
            .map(|row| WalletInfo {
                name: row.name,
                pubkey: row.pubkey,
                state: row.state.to_string(),
                active: row.active,
            })
            .collect(),
        error: String::new(),
        keystore: true,
    })
}

/// The named wallet's key file in a workspace — `env` names the override
/// path, and only while the override is what put that row on screen.
fn wallet_key_path(rpc: &str, name: &str) -> Result<PathBuf, String> {
    match (env_user_key(), name) {
        (Some(path), ENV_WALLET) => Ok(path),
        (_, name) => keystore_key_path(&keystore_root(rpc)?, name),
    }
}

/// Whose password the console's Settings re-unlock is about: the override,
/// else the connected workspace's active wallet.
fn active_or_env_wallet(rpc: &str) -> Result<String, String> {
    if env_key_override() {
        return Ok(ENV_WALLET.to_string());
    }
    let name = active_wallet_name(&keystore_root(rpc)?);
    if name.is_empty() {
        return Err("no active wallet — pick one in the launch window".to_string());
    }
    Ok(name)
}

pub async fn hub_state() -> HubState {
    let networks = known_networks();
    HubState {
        preselect: preselect_id(&networks),
        networks,
    }
}

/// The picked network's keystore, read the moment a network is picked. Also
/// what settles the session's identity for that network: the active wallet's
/// pubkey, read without a password, so the console knows who it is about to
/// sign as before — and without — an unlock. A network with no active wallet
/// is an identity of nobody. The read is a directory listing, never a
/// subprocess: nothing on the key path execs anything.
///
/// A REMOTE's keystore is named by the network it serves, so its node is
/// asked first (`/v1/status`); a node that cannot be reached, or serves no
/// chain yet, has no keystore to open and the launch window stays on the pick
/// with that error. A workspace on this device names its own keystore and is
/// not asked.
pub async fn load_wallets(rpc: String) -> WalletList {
    if let Err(cause) = name_remote_keystore(&rpc).await {
        set_local_user_key(None).await;
        return WalletList {
            wallets: Vec::new(),
            error: user_error(cause),
            keystore: false,
        };
    }
    let list = match wallet_rows(&rpc) {
        Ok(list) => list,
        Err(cause) => {
            // The detail can name a path — it reaches the screen, never the
            // log ring. The token is the fact.
            tracing::warn!(
                target: "ducktape::app",
                reason = "wallet_list_failed",
                "the keystore listing failed; the launch window shows the refusal"
            );
            WalletList {
                wallets: Vec::new(),
                error: user_error(cause),
                keystore: true,
            }
        }
    };
    let identity = session_key_path(&rpc)
        .ok()
        .and_then(|path| pubkey_of_key_file(&path));
    set_local_user_key(identity).await;
    list
}

/// Learn which network a remote endpoint serves, so its keystore has a name
/// ([`keystore_root`]). A workspace on this device, or the key override, needs
/// no asking. The status read is the only network round trip on the key path.
async fn name_remote_keystore(rpc: &str) -> Result<(), String> {
    let names_itself = env_user_key().is_some() || workspace_at(rpc).is_some();
    if names_itself {
        return Ok(());
    }
    let status = rpc_client(rpc)?
        .status_json()
        .await
        .map_err(|error| error.to_string())?;
    let chain_id = super::node::node_facts(&status).chain_id;
    if chain_id.is_empty() {
        return Err(
            "this node serves no network yet, so there is no identity to hold for it".into(),
        );
    }
    note_remote_chain(rpc, &chain_id);
    Ok(())
}

/// Merge one probe answer into the list by row id.
pub fn apply_network_probe(networks: Vec<HubNetwork>, probe: HubProbe) -> Vec<HubNetwork> {
    networks
        .into_iter()
        .map(|mut row| {
            if row.id == probe.id {
                row.probed = true;
                row.live = probe.live;
                row.height = probe.height;
            }
            row
        })
        .collect()
}

/// Probe every known network's endpoint, emitting one reading per row as it
/// answers. Bounded: one `/v1/status` with a short timeout per endpoint.
pub fn probe_known_networks() -> futures::stream::BoxStream<'static, HubProbe> {
    use futures::StreamExt;
    let probes = known_networks().into_iter().map(move |row| async move {
        let reading = probe_endpoint(&row.endpoint).await;
        HubProbe {
            id: row.id,
            live: reading.is_some(),
            height: reading.unwrap_or(-1),
        }
    });
    futures::stream::iter(probes).buffer_unordered(8).boxed()
}

/// One bounded status read: the height when the node answers, `None` when it
/// does not. 3s — a liveness dot must not hang the list.
async fn probe_endpoint(endpoint: &str) -> Option<i64> {
    if endpoint.is_empty() {
        return None;
    }
    let client = rpc_client(endpoint).ok()?;
    let status = tokio::time::timeout(Duration::from_secs(3), client.status())
        .await
        .ok()?
        .ok()?;
    Some(status.height as i64)
}

/// Stamp a network's last-used time and — for an endpoint no workspace
/// directory serves — remember it as a saved remote. Called on every
/// successful console connect; best-effort, a failed write costs only the
/// next boot's sort order.
pub async fn remember_network(rpc: String) -> bool {
    let endpoint = canonical_endpoint(rpc);
    if endpoint.is_empty() {
        return false;
    }
    let now = unix_now();
    let mut prefs = read_prefs();
    let is_remote = workspace_at(&endpoint).is_none();
    if is_remote {
        let mut remotes = prefs["saved_remotes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let known = remotes
            .iter()
            .any(|remote| remote["endpoint"].as_str() == Some(endpoint.as_str()));
        if !known {
            remotes.push(serde_json::json!({ "endpoint": endpoint }));
            prefs["saved_remotes"] = serde_json::json!(remotes);
        }
    }
    prefs["networks"][network_key(&endpoint)]["last_used"] = serde_json::json!(now);
    write_prefs(&prefs)
}

/// Drop a saved remote from the list, with the readings kept about it. Only a
/// remote: a local network is a directory under the ducktape home, and this
/// app does not delete those.
pub async fn forget_network(id: String) -> bool {
    let mut prefs = read_prefs();
    let remotes = prefs["saved_remotes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let kept: Vec<_> = remotes
        .into_iter()
        .filter(|remote| remote["endpoint"].as_str() != Some(id.as_str()))
        .collect();
    prefs["saved_remotes"] = serde_json::json!(kept);
    if let Some(networks) = prefs["networks"].as_object_mut() {
        networks.remove(&id);
    }
    write_prefs(&prefs)
}

/// Run one keystore ceremony OFF the async runtime.
///
/// Every one of them spends an argon2id pass over 64 MiB — 200-400 ms of
/// memory-hard work. On a tokio worker that is 400 ms during which the UI's
/// other tasks do not run, and this app's tasks are what repaint it.
async fn in_the_keystore<T: Send + 'static>(
    ceremony: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(ceremony)
        .await
        .map_err(|_| "the keystore operation did not finish".to_string())?
}

/// BEGIN this device's key: pick its name and its 24 words, and write
/// NOTHING. The seed lives only in [`MINTED_PHRASE`] until
/// [`confirm_recovery_phrase`] reads three of the words back and seals the
/// key file — so a key file that exists is a key whose only backup someone
/// has confirmed they hold, and a ceremony abandoned halfway leaves no
/// half-founded identity behind for the account screens to find.
///
/// The password is checked HERE (an 8-char floor is not worth learning after
/// writing 24 words down) and the name is claimed here too — after the host
/// (`-2`… on a collision) — in the PICKED network's keystore. Returns the
/// wallet name the seal will use.
pub async fn create_device_key(rpc: String, password: String) -> Result<String, AppError> {
    async {
        require_password(&password)?;
        let workspace = keystore_root(&rpc)?;
        let base = device_key_name();
        let candidates =
            std::iter::once(base.clone()).chain((2..10).map(|n| format!("{base}-{n}")));
        let name = candidates
            .into_iter()
            .find(|name| !keystore::wallet::key_file(&workspace, name).exists())
            .ok_or_else(|| {
                "this host already holds nine device keys — pick one in the wallet list".to_string()
            })?;
        let mut seed = Zeroizing::new([0u8; 32]);
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, seed.as_mut_slice());
        hold_minted_phrase(
            name.clone(),
            Zeroizing::new(keystore::userkey::mnemonic_of_seed(&seed)),
        )?;
        Ok(name)
    }
    .await
    .map_err(app_error)
}

// ============================================================================
// the recovery-phrase ceremony — show the 24 words once, then confirm three
// ============================================================================

/// THE ONLY COPY of a phrase whose key does not exist yet, alive between
/// [`create_device_key`] and the confirm that seals it. It is deliberately
/// NOT app state: the screen asks for its rows, the confirm asks whether
/// three typed words match, and neither answer hands the phrase to anything
/// that could log, snapshot or persist it. The rows the screen draws are the
/// one copy that leaves — a render-time `String` per word, gone with the
/// step. Sealing drops the phrase: the words are shown once, and this app
/// has no verb that shows them again (`ducktape user key reveal` reads them
/// back off the key file, which is the thing the phrase is a backup FOR).
static MINTED_PHRASE: std::sync::Mutex<Option<MintedPhrase>> = std::sync::Mutex::new(None);

struct MintedPhrase {
    /// the wallet name the confirm will seal these words under.
    name: String,
    words: Zeroizing<String>,
    /// the three 1-based positions this ceremony asks back, ascending.
    asked: [usize; 3],
}

/// One row of the phrase screen's two-column grid. Pairing (`1`/`13`,
/// `2`/`14`, …) yields twelve rows that fit the
/// launch window without a scroll, twenty-four do not.
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct PhraseRow {
    pub left_number: String,
    pub left_word: String,
    pub right_number: String,
    pub right_word: String,
}

/// Stash a phrase for the ceremony and pick the words it will ask back.
///
/// A phrase too short to ask three distinct words out of is refused rather
/// than padded: `mnemonic_of_seed` is always 24, so the only way here is a
/// caller that has stopped handing over a mnemonic, and a prompt that asks
/// for position 1 three times would hide that instead of showing it.
fn hold_minted_phrase(name: String, words: Zeroizing<String>) -> Result<(), String> {
    use rand::seq::SliceRandom as _;
    let count = words.split_whitespace().count();
    let mut positions: Vec<usize> = (1..=count).collect();
    positions.shuffle(&mut rand::rngs::OsRng);
    let [first, second, third, ..] = positions[..] else {
        return Err(format!("a recovery phrase is 24 words, not {count}"));
    };
    let mut asked = [first, second, third];
    asked.sort_unstable();
    let mut held = MINTED_PHRASE.lock().expect("minted recovery phrase");
    *held = Some(MintedPhrase { name, words, asked });
    Ok(())
}

/// The phrase screen's rows, or none when no ceremony is in flight — the
/// phrase step is reachable only straight off a mint.
pub fn phrase_rows() -> Vec<PhraseRow> {
    let held = MINTED_PHRASE.lock().expect("minted recovery phrase");
    held.as_ref()
        .map(|phrase| phrase_rows_of(&phrase.words))
        .unwrap_or_default()
}

/// The pairing itself, over any phrase — the screen's own test drives this
/// with a FIXED mnemonic so a capture never carries a real one.
pub fn phrase_rows_of(words: &str) -> Vec<PhraseRow> {
    let words: Vec<&str> = words.split_whitespace().collect();
    let half = words.len().div_ceil(2);
    (0..half)
        .map(|row| PhraseRow {
            left_number: format!("{}", row + 1),
            left_word: words[row].to_string(),
            right_number: format!("{}", row + half + 1),
            right_word: words.get(row + half).copied().unwrap_or("").to_string(),
        })
        .collect()
}

/// "5, 12 and 20" — the positions named by both ceremony screens.
fn asked_label(asked: &[usize; 3]) -> String {
    format!("{}, {} and {}", asked[0], asked[1], asked[2])
}

/// What the confirm step asks for, or empty when no ceremony is in flight.
pub fn recovery_prompt() -> String {
    let held = MINTED_PHRASE.lock().expect("minted recovery phrase");
    let Some(phrase) = held.as_ref() else {
        return String::new();
    };
    format!(
        "Type words {} — in that order, separated by spaces.",
        asked_label(&phrase.asked)
    )
}

/// THE CEREMONY'S ONE GATE, decided and nothing else: the three words back,
/// at the positions [`recovery_prompt`] named, case-insensitively. Hands the
/// seal below the wallet to write; the refusal names the positions and never
/// a word.
fn confirmed_phrase(answer: &str) -> Result<(String, Zeroizing<String>), String> {
    let held = MINTED_PHRASE.lock().expect("minted recovery phrase");
    let Some(phrase) = held.as_ref() else {
        return Err("there is no recovery phrase waiting to be confirmed".to_string());
    };
    let words: Vec<&str> = phrase.words.split_whitespace().collect();
    let typed: Vec<&str> = answer.split_whitespace().collect();
    let all_three_typed = typed.len() == phrase.asked.len();
    let every_word_matches = phrase.asked.iter().zip(typed.iter()).all(|(at, typed)| {
        words
            .get(at - 1)
            .is_some_and(|word| word.eq_ignore_ascii_case(typed))
    });
    let confirmed = all_three_typed && every_word_matches;
    if !confirmed {
        return Err(format!(
            "Those are not words {}. Check the phrase you wrote down.",
            asked_label(&phrase.asked)
        ));
    }
    Ok((phrase.name.clone(), phrase.words.clone()))
}

/// THE END OF THE CEREMONY: three right words seal the key they back.
/// The seal is `keystore::wallet::import` — the very call `ducktape wallet
/// import <name>` makes, so the phrase on screen restores this identity byte
/// for byte, here or on another machine.
///
/// A miss (or a seal that fails) keeps the phrase and the step, so a typo
/// costs a retry and not the account; a pass drops it, and nothing in this
/// app can show it again. The pointer write is not allowed to fail the call:
/// it degrades to a warning, and the user lands on a wallet that exists but
/// is not active, which the wallet list can still fix. The sealed key takes
/// the session seat: the password that sealed it is the one that signs.
pub async fn confirm_recovery_phrase(
    rpc: String,
    answer: String,
    password: String,
) -> Result<String, AppError> {
    let answer = Zeroizing::new(answer);
    let (name, words) = confirmed_phrase(&answer).map_err(app_error)?;
    let password = Zeroizing::new(password);
    let pubkey = async {
        let workspace = keystore_root(&rpc)?;
        let sealing = {
            let (workspace, name, password) = (workspace.clone(), name.clone(), password.clone());
            in_the_keystore(move || keystore::wallet::import(&workspace, &name, &words, &password))
        };
        let pubkey = sealing.await?;
        seat_signer(keystore::wallet::key_file(&workspace, &name), password).await?;
        Ok::<_, String>(pubkey)
    }
    .await
    .map_err(app_error)?;
    end_the_ceremony();
    if activate_wallet(&rpc, &name).await.is_err() {
        tracing::warn!(
            target: "ducktape::app",
            reason = "wallet_activate_failed",
            "the minted wallet is not the active one; pick it in the launch window"
        );
    }
    set_local_user_key(hex_decode(&pubkey).ok()).await;
    Ok(pubkey)
}

/// Let the words go, the moment the key they back exists.
fn end_the_ceremony() {
    let mut held = MINTED_PHRASE.lock().expect("minted recovery phrase");
    *held = None;
}

/// Re-seal an identity from its 24 words under a new password, into the
/// picked network's keystore. Returns the pubkey — the same identity those
/// words were minted as — and takes the session seat with it.
pub async fn restore_user_key(
    rpc: String,
    name: String,
    words: crate::secret::Secret,
    password: String,
) -> Result<String, AppError> {
    async {
        let workspace = keystore_root(&rpc)?;
        let normalized = Zeroizing::new(
            words
                .expose()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        );
        if normalized.split(' ').count() != 24 {
            return Err("a recovery phrase is exactly 24 words".to_string());
        }
        let password = Zeroizing::new(password);
        let importing = {
            let (workspace, name, password) = (workspace.clone(), name.clone(), password.clone());
            in_the_keystore(move || {
                keystore::wallet::import(&workspace, &name, &normalized, &password)
            })
        };
        let pubkey = importing.await?;
        seat_signer(keystore::wallet::key_file(&workspace, &name), password).await?;
        activate_wallet(&rpc, &name).await?;
        set_local_user_key(hex_decode(&pubkey).ok()).await;
        Ok(pubkey)
    }
    .await
    .map_err(app_error)
}

/// Unlock the NAMED wallet of the picked network: the one argon2id pass that
/// proves `password` opens it takes the session seat with the key it opened,
/// followed by the pointer write that makes it the wallet this device signs
/// with on that network. The pubkey the decrypt just proved seeds the
/// session's identity.
pub async fn unlock_wallet(
    rpc: String,
    name: String,
    password: String,
) -> Result<String, AppError> {
    async {
        let path = wallet_key_path(&rpc, &name)?;
        let password = Zeroizing::new(password);
        let pubkey = seat_signer(path, password).await?;
        activate_wallet(&rpc, &name).await?;
        set_local_user_key(hex_decode(&pubkey).ok()).await;
        Ok(pubkey)
    }
    .await
    .map_err(app_error)
}

/// The active-pointer write, in the picked network's keystore. The env
/// override names no keystore row, so it has no pointer to move.
async fn activate_wallet(rpc: &str, name: &str) -> Result<(), String> {
    keystore::wallet::valid_name(name)?;
    if env_key_override() && name == ENV_WALLET {
        return Ok(());
    }
    keystore::wallet::activate(&keystore_root(rpc)?, name)
}

/// The console's Settings re-unlock, which knows a password and nothing else:
/// it re-proves the wallet this session is already signing with on the
/// connected network.
pub async fn unlock_user_key(rpc: String, password: String) -> Result<String, AppError> {
    let name = active_or_env_wallet(&rpc).map_err(app_error)?;
    unlock_wallet(rpc, name, password).await
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(actives: &[(&str, bool)]) -> Vec<WalletInfo> {
        actives
            .iter()
            .map(|(name, active)| WalletInfo {
                name: name.to_string(),
                pubkey: String::new(),
                state: "encrypted".into(),
                active: *active,
            })
            .collect()
    }

    /// A device key is named after the host, in the keystore's grammar, and
    /// never comes out empty.
    #[test]
    fn a_device_key_is_named_after_the_host_and_never_empty() {
        let name = device_key_name();
        assert!(!name.is_empty());
        assert!(keystore::wallet::valid_name(&name).is_ok(), "{name}");
    }

    /// The keystore decides both the entry step and the preselected row: an
    /// empty one is the password step, and the active wallet is the row the
    /// list opens on.
    #[test]
    fn entry_step_and_preselect_follow_the_keystore() {
        assert!(matches!(hub_entry_step(vec![]), crate::HubStep::Password));
        assert!(matches!(
            hub_entry_step(rows(&[("a", false)])),
            crate::HubStep::Wallets
        ));
        assert_eq!(preselect_wallet(rows(&[("a", false), ("b", true)])), "b");
        assert_eq!(preselect_wallet(rows(&[("a", false)])), "a");
        assert_eq!(preselect_wallet(vec![]), "");
    }

    /// A wallet name is a path segment: `..` or `/` would walk the key file
    /// out of the keystore. The check lives in `keystore_key_path`, so a name
    /// that never passed through a person — the `active` pointer file's
    /// contents — is gated on the same terms.
    #[test]
    fn a_wallet_name_is_never_a_path() {
        let workspace = tempfile::tempdir().unwrap();
        for name in ["env", "default", "alice2", "a.b_c-d", "0"] {
            assert!(
                keystore::wallet::valid_name(name).is_ok(),
                "{name} should be valid"
            );
        }
        let refused = [
            "",
            "-h",
            "..",
            "../x",
            "a/b",
            "/etc/passwd",
            "Alice",
            ".hidden",
        ];
        for name in refused {
            let refusal = keystore_key_path(workspace.path(), name).expect_err("built a path");
            assert!(
                refusal.contains("wallet name"),
                "unnamed refusal: {refusal}"
            );
        }
        assert!(keystore::wallet::valid_name(&"a".repeat(41)).is_ok());
        assert!(keystore::wallet::valid_name(&"a".repeat(42)).is_err());
    }

    /// The wallet screens name the picked network by its row's display name,
    /// and a selection that no longer names a row (a forget can race the
    /// click) names nothing rather than the wrong network.
    #[test]
    fn the_selected_row_names_the_network_the_wallet_screens_show() {
        let rows = vec![HubNetwork {
            id: "demo#a1b2".into(),
            chain_id: "demo#a1b2".into(),
            name: "demo".into(),
            endpoint: "http://127.0.0.1:1".into(),
            kind: "local".into(),
            last_used: 0,
            probed: false,
            live: false,
            height: -1,
        }];
        assert_eq!(
            selected_network_name(rows.clone(), "demo#a1b2".into()),
            "demo"
        );
        assert_eq!(selected_network_name(rows, "gone".into()), "");
    }

    /// A picked network's keystore decides the next step: rows unlock, an
    /// empty keystore mints, and a keystore that could not be named (the
    /// remote never answered) keeps the pick on screen — whatever rows it
    /// claims. No door opens the console read-only on its own.
    #[test]
    fn the_wallet_door_follows_the_picked_keystore() {
        assert!(matches!(
            wallet_door(&WalletList {
                wallets: rows(&[("a", true)]),
                error: String::new(),
                keystore: true
            }),
            crate::WalletDoor::Wallets
        ));
        assert!(matches!(
            wallet_door(&WalletList {
                wallets: vec![],
                error: String::new(),
                keystore: true
            }),
            crate::WalletDoor::Password
        ));
        assert!(matches!(
            wallet_door(&WalletList {
                wallets: vec![],
                error: "unreachable".into(),
                keystore: false
            }),
            crate::WalletDoor::Unreached
        ));
    }

    /// A REMOTE HAS A KEYSTORE. An endpoint this device holds no workspace
    /// for used to have none — the door dropped it into the console read-only
    /// with no way to create or unlock a key. Its keystore is named by the
    /// network the node says it serves, under the ducktape home, and until
    /// the node has said so the root is a refusal rather than a guess.
    #[test]
    fn a_remote_keystore_is_named_by_its_chain_id_once_the_node_has_answered() {
        let rpc = "http://203.0.113.9:18844";
        assert!(
            keystore_root(rpc).is_err(),
            "an unreached remote names no keystore"
        );
        note_remote_chain(rpc, "team#c0ffee");
        let home = tempfile::tempdir().unwrap();
        let root = remote_keystore_root(home.path(), "team#c0ffee").unwrap();
        assert_eq!(root, home.path().join("remotes").join("team#c0ffee"));
        assert!(
            keystore_root(rpc).unwrap().ends_with("remotes/team#c0ffee"),
            "the named remote resolves under the ducktape home"
        );
        // and the keystore verbs work on it like on a workspace: nothing yet.
        assert!(keystore::wallet::list(&root).unwrap().is_empty());
        // a path separator in a chain id is made inert, never a directory walk.
        let root = remote_keystore_root(home.path(), "a/b#1").unwrap();
        assert_eq!(root.file_name().unwrap(), "a-b#1");
        assert!(remote_keystore_root(home.path(), "..").is_err());
        // a node serving no chain records nothing.
        note_remote_chain("http://203.0.113.11:1", "");
        assert!(keystore_root("http://203.0.113.11:1").is_err());
    }

    /// A FIXED phrase — never a minted one, so nothing here can leak a real
    /// key's backup into a test log.
    const TEST_PHRASE: &str = "abandon amount liar amount expire adjust cage candy arch gather drum bullet absurd math era live bid rhythm alien crouch range attend journey unaware";

    /// the entropy [`TEST_PHRASE`] encodes: `00 01 02 … 1f`. A fixture, not a
    /// key — every phrase in this file's tests and in the screen captures is
    /// this one, so no minted phrase can reach a log or a PNG.
    const TEST_SEED: [u8; 32] = {
        let mut seed = [0u8; 32];
        let mut byte = 0;
        while byte < 32 {
            seed[byte] = byte as u8;
            byte += 1;
        }
        seed
    };

    /// The grid pairs 1↔13, 2↔14 … 12↔24, which is what lets the launch
    /// window show all 24 words without a scroll.
    #[test]
    fn the_phrase_grid_pairs_the_halves() {
        let rows = phrase_rows_of(TEST_PHRASE);
        assert_eq!(rows.len(), 12);
        assert_eq!(rows[0].left_number, "1");
        assert_eq!(rows[0].left_word, "abandon");
        assert_eq!(rows[0].right_number, "13");
        assert_eq!(rows[0].right_word, "absurd");
        assert_eq!(rows[11].left_number, "12");
        assert_eq!(rows[11].left_word, "bullet");
        assert_eq!(rows[11].right_number, "24");
        assert_eq!(rows[11].right_word, "unaware");
        assert!(phrase_rows_of("").is_empty());
    }

    /// THE WHOLE CEREMONY, in one test because [`MINTED_PHRASE`] is a
    /// process-global slot and two tests racing over it prove nothing: the
    /// mint hands the words over, the prompt names three positions, a wrong
    /// answer is refused WITHOUT naming a word and keeps the phrase, the
    /// right answer hands back the wallet to seal, and the phrase is gone the
    /// moment the seal is done with it.
    ///
    /// The gate is driven, not [`confirm_recovery_phrase`] itself: the seal
    /// writes a key file into the real `~/.ducktape`, which is not a test's
    /// to touch. What the seal does with what the gate returns is asserted
    /// below instead — the phrase IS a `ducktape wallet import` phrase.
    #[test]
    fn the_confirm_gate_ends_the_ceremony() {
        hold_minted_phrase(
            "device-test".to_string(),
            Zeroizing::new(TEST_PHRASE.into()),
        )
        .expect("a 24-word fixture");
        assert!(
            hold_minted_phrase("too-short".to_string(), Zeroizing::new("one two".into())).is_err()
        );
        assert_eq!(phrase_rows().len(), 12);
        let asked = MINTED_PHRASE
            .lock()
            .expect("minted recovery phrase")
            .as_ref()
            .expect("a phrase is held")
            .asked;
        let prompt = recovery_prompt();
        assert!(prompt.contains(&asked_label(&asked)), "{prompt}");
        let words: Vec<&str> = TEST_PHRASE.split_whitespace().collect();
        let right = asked
            .iter()
            .map(|at| words[at - 1])
            .collect::<Vec<_>>()
            .join(" ");

        let refusal = confirmed_phrase("nope nope nope").expect_err("three wrong words pass");
        assert!(refusal.contains(&asked_label(&asked)), "{refusal}");
        for word in &words {
            assert!(
                !refusal.contains(word),
                "the refusal leaked a word: {refusal}"
            );
        }
        // a miss keeps the phrase: the retry is still possible.
        assert_eq!(phrase_rows().len(), 12);
        // and so is a short answer.
        assert!(confirmed_phrase(words[asked[0] - 1]).is_err());

        // CASE IS NOT THE TEST — the words are, in order.
        let (name, sealing) =
            confirmed_phrase(&right.to_uppercase()).expect("the right words in the right order");
        assert_eq!(name, "device-test");
        assert_eq!(sealing.as_str(), TEST_PHRASE);
        // WHAT THE SEAL IS HANDED is what `ducktape wallet import <name>`
        // eats — the same `keystore::wallet::import` call, so the words on
        // screen restore this identity — and it decodes to exactly the seed
        // the mint drew, which is the whole claim the phrase makes.
        let seed = keystore::userkey::seed_of_mnemonic(&sealing).expect("a bip39 phrase");
        assert_eq!(seed, TEST_SEED);
        assert_eq!(keystore::userkey::mnemonic_of_seed(&seed), TEST_PHRASE);

        end_the_ceremony();
        assert!(phrase_rows().is_empty(), "the phrase outlived its ceremony");
        assert_eq!(recovery_prompt(), "");
        assert!(confirmed_phrase(&right).is_err());
    }

    /// The tray's Open row (#1782): a window already tracked is always
    /// raised, whichever it is — and only once nothing is tracked does
    /// connection state decide between reopening the console (reconnect) and
    /// the launch window (fresh pick).
    #[test]
    fn tray_open_reconnects_the_console_only_when_untracked_and_connected() {
        use crate::TrayOpen;

        assert!(matches!(tray_open_action(false, false), TrayOpen::Launch));
        assert!(matches!(tray_open_action(true, false), TrayOpen::Console));
        assert!(matches!(tray_open_action(true, true), TrayOpen::Raise));
        assert!(matches!(tray_open_action(false, true), TrayOpen::Raise));
    }
}
