use super::*;

/// Does the pane `tab` mounts actually read `plane`'s rows?
///
/// THE TAB-SWITCH GATE. Every tab move used to refetch members, governance,
/// agents and account — four `/v1/query` round trips per click, on the way into
/// panes that render none of them. The refetch is not what keeps those planes
/// fresh either: `plane_live_hit` already refetches each one when ITS module
/// commits (`live.rs`), which is both cheaper and earlier. So a tab move only
/// needs the plane its destination screen is about to draw.
///
/// The titlebar chips (approvals, agent dot, account name) read from state,
/// and state is what the connect load and the live-plane lane fill — no chip
/// depends on a tab click.
pub fn tab_reads_plane(tab: crate::ShellTab, plane: String) -> bool {
    // a registry-listed view reads its own planes through `rpc.live`; no
    // app-side load is on its screen's path
    if let crate::ShellTab::Registered(_) = tab {
        return false;
    }
    match plane.as_str() {
        "governance" => tab == crate::ShellTab::Governance,
        "agents" => tab == crate::ShellTab::Agents,
        // Settings draws the account card; Forge draws the org "about".
        "account" => matches!(tab, crate::ShellTab::Settings | crate::ShellTab::Forge),
        _ => false,
    }
}

/// `$DUCKTAPE_HOME`, else `~/.ducktape` — the directory that holds every
/// workspace on this device and nothing else: [`ducktape_home::root`], the
/// same resolution the node lists its workspaces through.
pub(crate) fn ducktape_home() -> Option<PathBuf> {
    ducktape_home::root().ok()
}

/// Every workspace under the ducktape home as `(chain id, directory)` — the
/// CLI's own directory walk (`workspace_config::list_workspaces`), read per
/// call, so membership and the id agree with what `node init`/`node join`
/// wrote and what `-n` resolves.
pub(crate) fn workspaces() -> Vec<(String, PathBuf)> {
    let Some(root) = ducktape_home() else {
        return Vec::new();
    };
    workspaces_in(&root)
}

pub(crate) fn workspaces_in(root: &Path) -> Vec<(String, PathBuf)> {
    workspace_config::list_workspaces_in(root)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(chain_id, node_toml)| Some((chain_id, node_toml.parent()?.to_path_buf())))
        .collect()
}

/// This workspace's app endpoint, from its `http_listen` — a wildcard bind is
/// rewritten to loopback, the same as the CLI dials it.
pub(crate) fn workspace_endpoint(dir: &Path) -> Option<String> {
    workspace_config::http_base_in(dir).ok()
}

/// The endpoint an EMPTY `rpc` means: the one workspace under the home, when
/// the home holds exactly one — the rung the CLI's `-n` falls to as well.
/// Two workspaces are a pick the launch window makes, never a default.
pub(crate) fn lone_workspace_endpoint() -> Option<String> {
    let listed = workspaces();
    let [(_, dir)] = listed.as_slice() else {
        return None;
    };
    workspace_endpoint(dir)
}

/// The workspace on this device that serves an endpoint, matched on the
/// endpoint the app is actually connected to. `None` is a remote.
pub(crate) fn workspace_at(rpc: &str) -> Option<(String, PathBuf)> {
    let endpoint = canonical_endpoint(rpc.to_string());
    workspaces()
        .into_iter()
        .find(|(_, dir)| workspace_endpoint(dir).as_deref() == Some(endpoint.as_str()))
}

/// What a join hands back: the network's id, where it materialized, and the
/// endpoint this app should connect to.
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct WorkspaceInit {
    pub chain_id: String,
    pub workspace: String,
    pub rpc: String,
}

/// Materialize this device's workspace from an invite blob, in this process.
///
/// Joining is the one node operation with no daemon to ask — it is what BRINGS
/// a workspace into existence — so it is a library call
/// ([`workspace_config::join_workspace`]) rather than an endpoint. It writes a
/// directory, mints two keys and runs argon2-free but still blocking file work,
/// hence `spawn_blocking`.
pub async fn join_network(blob: crate::secret::Secret) -> Result<WorkspaceInit, AppError> {
    async {
        let blob = blob.expose().trim().to_string();
        let valid = !blob.is_empty()
            && blob.len() <= 64 * 1024
            && !blob.chars().any(|character| character == '\0');
        if !valid {
            return Err("invite must be between 1 and 65536 bytes".into());
        }
        let joining = tokio::task::spawn_blocking(move || {
            workspace_config::join_workspace(&blob, None, &Default::default())
        });
        let joined = joining
            .await
            .map_err(|_| "joining this network did not finish".to_string())??;
        let rpc = workspace_endpoint(&joined.dir)
            .ok_or_else(|| "the new workspace has no node.toml http_listen".to_string())?;
        Ok(WorkspaceInit {
            chain_id: joined.chain_id,
            workspace: joined.dir.display().to_string(),
            rpc,
        })
    }
    .await
    .map_err(app_error)
}

/// Mint a single-use bearer invite for a workspace — the `🦆…` paste blob.
///
/// The RUNNING node mints it (`POST /v1/invite`), because minting is a WRITE
/// to that node's own files: it folds this member's dial hint into the network
/// descriptor and saves it, and reads the persisted mesh state for the member
/// fronts the blob carries. Asking the daemon that owns those files is the
/// difference between one writer and two racing ones.
///
/// The cost of that choice, stated: a workspace whose node is stopped can no
/// longer mint from here. An invite names paths a joiner must be able to reach,
/// so a network nobody is serving has nothing useful to hand out anyway.
///
/// The app takes no TTL: it mints the ONE default every other door mints
/// (`workspace_config::DEFAULT_INVITE_TTL_DAYS`).
pub async fn mint_invite(workspace: String) -> Result<String, AppError> {
    let minted: Result<String, String> = async {
        let endpoint = workspace_rpc(&workspace)?;
        let ttl = workspace_config::DEFAULT_INVITE_TTL_DAYS;
        Ok(rpc_client(&endpoint)?.mint_invite(ttl).await?)
    }
    .await;
    minted.map_err(app_error)
}

/// The endpoint serving a workspace named by directory OR by chain id — the
/// same two spellings the CLI's `-n` selector takes, because the callers that
/// used to pass one to `-n` now need a URL instead.
fn workspace_rpc(selector: &str) -> Result<String, String> {
    let selector = selector.trim();
    let matches_selector = |chain_id: &str, dir: &Path| {
        chain_id == selector || dir.file_name().is_some_and(|name| name == selector)
    };
    workspaces()
        .into_iter()
        .find(|(chain_id, dir)| matches_selector(chain_id, dir))
        .and_then(|(_, dir)| workspace_endpoint(&dir))
        .ok_or_else(|| format!("no local workspace named {selector:?} to mint an invite from"))
}

/// One provisioning step. `state` is `done` | `running` | `pending` | `blocked`.
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct ProvisionStep {
    pub index: i64,
    pub label: String,
    pub state: String,
    /// `state == "done"`, as a Copy field. The onboarding handler has to decide
    /// whether the phase advances BEFORE it moves the step into the reading,
    /// and reading `state` there would move the String out from under it.
    pub settled: bool,
}

/// The five provisioning steps. Steps 1-3 are facts of the materialized
/// workspace; steps 4-5 are a REAL `/v1/status` poll, because the app attaches
/// to a node it does not supervise — when nothing answers, the step goes
/// `blocked` and its label says which command starts it.
pub fn provision_progress(
    workspace: String,
    rpc: String,
) -> futures::stream::BoxStream<'static, ProvisionStep> {
    struct State {
        dir: Option<PathBuf>,
        chain_id: String,
        rpc: String,
        step: usize,
        attempts: u32,
    }
    let found = workspaces()
        .into_iter()
        .find(|(chain_id, dir)| *chain_id == workspace || dir.display().to_string() == workspace);
    let (chain_id, dir) = match found {
        Some((chain_id, dir)) => (chain_id, Some(dir)),
        None => (workspace, None),
    };
    Box::pin(futures::stream::unfold(
        State {
            dir,
            chain_id,
            rpc,
            step: 0,
            attempts: 0,
        },
        |mut state| async move {
            // the workspace's own facts, then the node's own answer.
            match state.step {
                0 => {
                    state.step = 1;
                    let home = ducktape_home()
                        .map(|home| home.display().to_string())
                        .unwrap_or_else(|| "~/.ducktape".into());
                    Some((
                        registered_step(
                            1,
                            &format!("Workspace on disk · {home}"),
                            state.dir.is_some(),
                        ),
                        state,
                    ))
                }
                1 => {
                    state.step = 2;
                    let key = state
                        .dir
                        .as_deref()
                        .and_then(workspace_identity)
                        .unwrap_or_default();
                    let known = !key.is_empty();
                    Some((
                        registered_step(2, &format!("Admin keypair · {key}"), known),
                        state,
                    ))
                }
                2 => {
                    state.step = 3;
                    let ready = state
                        .dir
                        .as_ref()
                        .is_some_and(|dir| dir.join("network.toml").is_file());
                    // No tail: this step proves only that `network.toml` exists,
                    // and what a member later copies is an opaque invite blob with
                    // no URI form — the artifact's "invite links available" promised
                    // a link nothing in this flow mints.
                    Some((registered_step(3, "Workspace ready", ready), state))
                }
                3 => {
                    // the app attaches to a node it does not supervise: the
                    // only honest readiness signal is the node answering.
                    let up = match rpc_client(&state.rpc) {
                        Ok(client) => client.status().await.is_ok(),
                        Err(_) => false,
                    };
                    if up {
                        state.step = 4;
                        return Some((registered_step(4, "Local node starting", true), state));
                    }
                    state.attempts += 1;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let stalled = state.attempts >= PROVISION_PATIENCE;
                    let step = match stalled {
                        false => ProvisionStep {
                            index: 4,
                            label: "Local node starting".into(),
                            state: "running".into(),
                            settled: false,
                        },
                        true => ProvisionStep {
                            index: 4,
                            label: format!(
                                "Start the node · ducktape node run -n {}",
                                state.chain_id
                            ),
                            state: "blocked".into(),
                            settled: false,
                        },
                    };
                    Some((step, state))
                }
                4 => {
                    let listen = state
                        .dir
                        .as_deref()
                        .and_then(workspace_endpoint)
                        .unwrap_or_else(|| state.rpc.clone());
                    state.step = 5;
                    Some((
                        ProvisionStep {
                            index: 5,
                            label: format!("Node API listening · {listen}"),
                            state: "done".into(),
                            settled: true,
                        },
                        state,
                    ))
                }
                // every step has reported; the console takes over.
                _ => None,
            }
        },
    ))
}

/// A step whose fact is either established or missing.
fn registered_step(index: i64, label: &str, established: bool) -> ProvisionStep {
    ProvisionStep {
        index,
        label: label.to_string(),
        state: match established {
            true => "done".into(),
            false => "blocked".into(),
        },
        settled: established,
    }
}

/// The workspace's own node identity, short — `network.toml` seats it as the
/// founding validator, so a fresh network's admin key is readable there.
pub(crate) fn workspace_identity(dir: &Path) -> Option<String> {
    let descriptor = workspace_config::NetworkDescriptor::load(&dir.join("network.toml")).ok()?;
    let key = descriptor.validators.first()?;
    Some(short_label(key))
}

/// The titlebar's network label: the NAME PART of the connected node's chain
/// id (`name#hash`), the one fact every member of a network shares. Until the
/// node has said which chain it serves, the endpoint's host stands in, and
/// with no endpoint the product name does.
///
/// Nothing device-local feeds this: the ducktape home only knows the
/// workspaces this machine holds, and an account name is one person's,
/// not the network's.
pub fn network_label(chain_id: impl AsRef<str>, rpc: impl AsRef<str>) -> String {
    let chain_id = chain_id.as_ref().trim();
    let named = chain_id.split('#').next().unwrap_or_default();
    if !named.is_empty() {
        return named.to_string();
    }
    if !chain_id.is_empty() {
        return chain_id.to_string();
    }
    let host = rpc
        .as_ref()
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    if host.is_empty() {
        return "Ducktape".into();
    }
    host.to_string()
}

// Shared status-item labels, alongside the titlebar labels.

/// The count beside the menu-bar icon: nothing at all while the bell is empty.
pub fn tray_badge(unread: i64) -> String {
    match unread > 0 {
        true => unread.to_string(),
        false => String::new(),
    }
}

pub fn tray_tooltip(network: String, status: String) -> String {
    match network.is_empty() {
        true => format!("Ducktape — {status}"),
        false => format!("{network} — {status}"),
    }
}

pub fn tray_bell_row(unread: i64) -> String {
    match unread > 0 {
        true => format!("Notifications · {unread} unread"),
        false => "Notifications".into(),
    }
}

pub fn tray_huddle_row(joined: bool, channel: String) -> String {
    match joined {
        true => format!("Huddle · #{channel}"),
        false => "Huddle".into(),
    }
}

/// A radio row: the chosen one wears the check.
pub fn tray_choice_row(label: String, chosen: bool) -> String {
    match chosen {
        true => format!("✓ {label}"),
        false => label,
    }
}

/// A non-negative count with thousands separators: `84,912`.
pub(crate) fn grouped_digits(value: i64) -> String {
    let digits = value.max(0).to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        let boundary = index > 0 && (digits.len() - index).is_multiple_of(3);
        if boundary {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// The rail's foot names who is signed in: the account by its name, else by
/// its number; no account reads as a sign-in. The second line is the short
/// prefix of the key this desktop signs with, or that nothing signs.
pub fn rail_identity(
    account_exists: bool,
    account_name: &str,
    account_number: &str,
    signer_key: &str,
) -> (String, String) {
    let who = match (account_exists, account_name.is_empty()) {
        (true, false) => account_name.to_owned(),
        (true, true) => format!("Account #{account_number}"),
        (false, _) => "Sign in".to_owned(),
    };
    let whose_key = match signer_key.is_empty() {
        true => "No signing key".to_owned(),
        false => format!("key {}", signer_key.chars().take(8).collect::<String>()),
    };
    (who, whose_key)
}

/// TWO uppercase letters for a 28px+ avatar plate: the initials of the first
/// two words, else the first two alphanumerics of one word.
pub fn initials_of(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().take(2).collect();
    if words.len() == 2 {
        let letters: String = words
            .iter()
            .filter_map(|word| word.chars().find(char::is_ascii_alphanumeric))
            .collect();
        if letters.chars().count() == 2 {
            return letters.to_uppercase();
        }
    }
    let letters: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(2)
        .collect();
    match letters.is_empty() {
        true => "?".into(),
        false => letters.to_uppercase(),
    }
}

/// Elapsed `mm:ss` for the huddle pills and panel.
pub fn mmss(seconds: i64) -> String {
    let seconds = seconds.max(0);
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

/// The wall clock, unix seconds.
pub(crate) fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub fn current_wall_seconds() -> i64 {
    now_seconds()
}

