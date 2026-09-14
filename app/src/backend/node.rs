use super::*;

/// The device-local settings facts: where this app points and what identity it
/// holds locally. Node status belongs to [`NodeFacts`].
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct SettingsFacts {
    pub generation: i64,
    pub key_path: String,
    pub key_state: String,
    /// This workspace's directory on this device — the Node overview's data dir.
    pub data_dir: String,
    /// THE VIEWER'S OWN KEY, full hex — the `me` every membership test needs.
    /// `ChatMember.key` is `member_id(..)` at full width, and the account card
    /// carries an account NUMBER, not a key, so neither the account card nor
    /// the node key can answer "is this row me". Empty on a device with no user
    /// key, which `post_gate` reads as "not seated" — the honest answer when
    /// there is no identity to seat.
    pub user_key: String,
}

/// The NETWORK card's Data dir row.
/// Load the settings facts: the local user key's location and state, and the
/// workspace directory.
pub async fn load_settings_facts(
    rpc: String,
    generation: i64,
) -> Result<SettingsFacts, HydrationError> {
    async {
        // the launch window's key-state reading, on the same file: one
        // classifier, so Settings and the wallet list cannot disagree about it.
        let (key_path, key_state) = match session_key_path(&rpc) {
            Err(_) => ("(unset)".to_string(), "unlocatable".to_string()),
            Ok(path) => (path.display().to_string(), key_state_of(&path)),
        };
        let data_dir = workspace_at(&rpc)
            .map(|(_, dir)| dir.display().to_string())
            .or_else(|| ducktape_home().map(|home| home.display().to_string()))
            .unwrap_or_default();
        Ok::<_, String>(SettingsFacts {
            generation,
            key_path,
            key_state,
            data_dir,
            user_key: local_user_key()
                .await
                .map(|key| hex_encode(&key))
                .unwrap_or_default(),
        })
    }
    .await
    .map_err(|message: String| HydrationError {
        generation,
        message: user_error(message),
    })
}

/// The node's consensus/storage facts — everything `/v1/status` publishes that
/// the two-field `Status` type drops, plus the mesh sample's live/total.
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct NodeFacts {
    /// The daemon identity, full hex so the operator surface can copy the key
    /// that membership and peer records actually carry.
    pub public_key: String,
    /// The daemon's build version, verbatim off `/v1/status` (its own
    /// `CARGO_PKG_VERSION`). A build/commit SHA is NOT published anywhere, so
    /// the version line carries the version alone.
    pub version: String,
    pub root_hash: String,
    /// The chain id every chain-scoped user proof (an `AddKey` consent) is
    /// minted for; "" on a daemon that serves no chain.
    pub chain_id: String,
    /// The three consensus facts are OPTION on purpose: `operations.consensus`
    /// is absent on a resident, a joiner and the embedded local daemon
    /// "rather than being filled with misleading zeroes", so a plain i64 would
    /// print a hard 0 as if it were measured.
    pub view: Option<i64>,
    pub quorum: Option<i64>,
    pub reachable_validators: Option<i64>,
    /// These two are under the SAME absent-on-a-resident `operations` object as
    /// the trio above, so they get the same honesty — carried as [`UNMEASURED`]
    /// rather than a plain `0`, which both renderers already print as `—`.
    pub last_finalized_at: i64,
    pub checkpoint_height: i64,
    /// THE HEAD FROM THE SAME DOCUMENT AS [`Self::checkpoint_height`]. A
    /// checkpoint means nothing except against the head it was sampled with, so
    /// the two travel together or not at all: Settings used to draw them from
    /// two separate `/v1/status` calls and printed CHECKPOINT h 422,563 above
    /// HEIGHT h 422,553 — an order no node is ever in. See [`served_height`]
    /// for why a wire `0` lands here as [`UNMEASURED`].
    pub height: i64,
    /// The node's own lifecycle phase — `starting`, `recovering`, `joining`,
    /// `syncing`, `validating`, `serving`, `draining`, `halted`.
    ///
    /// THE ONLY TRUSTWORTHY DISCRIMINANT for whether a sync is happening. The
    /// `sync` block beside it is written by `begin_sync` and never cleared, so
    /// a node that finished syncing hours ago still carries the last run.
    pub phase: String,
    /// Unix seconds the phase last changed; [`UNMEASURED`] when unpublished.
    pub phase_since: i64,
    /// The sync run's heights, [`UNMEASURED`] when the node has published none.
    pub sync_target: i64,
    pub sync_applied: i64,
    /// CUMULATIVE since boot and never reset, so these are a total rather than
    /// a state — which is why they belong on a detail surface and not on a
    /// badge. Absence really is zero here: a count of nothing IS zero.
    pub sync_retries: i64,
    pub sync_failures: i64,
    /// The last sync error, SELF-CLEARING: `record_sync_progress` puts it back
    /// to `None` the moment the node advances. Present therefore means "the
    /// most recent attempt failed and nothing has moved since", which is a
    /// fact about now rather than a scar.
    pub sync_last_error: String,
}

/// A DEFAULT IS A DOCUMENT NO NODE HAS PUBLISHED, so its three numbers are
/// [`UNMEASURED`] and not zero.
///
/// `derive(Default)` gave them `0`, which is the one value this whole file
/// exists to keep off the screen: a defaulted document must not report a
/// measured head and checkpoint for a node that has served neither.
/// It is inert today: both arms of `overview_from` construct a default (the
/// struct literal is evaluated before the status arm overwrites `facts`), but
/// only the peers frame's copy survives, and every one of the six `keep_i64` /
/// `keep_str` guards in `node_overview_sample` discards it on
/// `facts_answered == false`. Inert is not the same as right, which is why the
/// invariant is written here rather than left loaded on a public struct.
impl Default for NodeFacts {
    fn default() -> Self {
        Self {
            public_key: String::new(),
            version: String::new(),
            root_hash: String::new(),
            chain_id: String::new(),
            view: None,
            quorum: None,
            reachable_validators: None,
            last_finalized_at: UNMEASURED,
            checkpoint_height: UNMEASURED,
            height: UNMEASURED,
            phase: String::new(),
            phase_since: UNMEASURED,
            sync_target: UNMEASURED,
            sync_applied: UNMEASURED,
            sync_retries: 0,
            sync_failures: 0,
            sync_last_error: String::new(),
        }
    }
}

/// Load the node facts from the raw status document.
/// A section the node omits for its role stays `None` — the status projection
/// leaves it out rather than filling it with misleading numbers, and so do we.
/// The facts a `/v1/status` document carries — the ONE reader, shared by the
/// HTTP load and the pushed `status` snapshot, for the same reason
/// [`peer_rows`] is shared.
pub(crate) fn node_facts(status: &serde_json::Value) -> NodeFacts {
    let operations = &status["operations"];
    let consensus = &operations["consensus"];
    let sync = &operations["sync"];
    NodeFacts {
        public_key: status["public_key"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        version: status["version"].as_str().unwrap_or_default().to_string(),
        root_hash: status["root_hash"].as_str().unwrap_or_default().to_string(),
        chain_id: status["chain_id"].as_str().unwrap_or_default().to_string(),
        view: consensus["view"].as_i64(),
        quorum: consensus["quorum"].as_i64(),
        reachable_validators: consensus["reachable_validators"].as_i64(),
        last_finalized_at: operations["last_finalized_at"]
            .as_i64()
            .unwrap_or(UNMEASURED),
        checkpoint_height: operations["storage"]["checkpoint_height"]
            .as_i64()
            .unwrap_or(UNMEASURED),
        height: served_height(&status["height"]),
        phase: operations["phase"].as_str().unwrap_or_default().to_string(),
        phase_since: operations["phase_since"].as_i64().unwrap_or(UNMEASURED),
        sync_target: sync["target_height"].as_i64().unwrap_or(UNMEASURED),
        sync_applied: sync["applied_height"].as_i64().unwrap_or(UNMEASURED),
        sync_retries: sync["retries"].as_i64().unwrap_or(0),
        sync_failures: sync["failures"].as_i64().unwrap_or(0),
        sync_last_error: sync["last_error"].as_str().unwrap_or_default().to_string(),
    }
}

/// The one sentence all three surfaces print for what the node is doing.
///
/// Progress rides ONLY while `sync_in_progress`. `operations.sync` is never
/// cleared, so printing it whenever it exists leaves a finished run's numbers
/// on screen for good — and a reader cannot tell a live count from a fossil.
pub fn sync_label(phase: &str, applied: i64, target: i64) -> String {
    if phase.is_empty() {
        return String::new();
    }
    let name = capitalized(phase);
    let measured = applied >= 0 && target >= 0;
    if !sync_in_progress(phase) || !measured {
        return name;
    }
    format!(
        "{name} {} / {}",
        grouped_digits(applied),
        grouped_digits(target)
    )
}

/// The node spells its phases lowercase on the wire; a reader reads prose.
fn capitalized(word: &str) -> String {
    let mut letters = word.chars();
    match letters.next() {
        Some(first) => first.to_uppercase().chain(letters).collect(),
        None => String::new(),
    }
}

/// Whether the node is catching up RIGHT NOW.
///
/// The phase, and only the phase. `operations.sync` is never cleared, so its
/// presence says a sync once happened — not that one is happening.
pub(crate) fn sync_in_progress(phase: &str) -> bool {
    phase == "syncing"
}

pub async fn load_node_facts(rpc: String) -> Result<NodeFacts, AppError> {
    async {
        let client = rpc_client(&rpc)?;
        let status = client.status_json().await?;
        Ok(node_facts(&status))
    }
    .await
    .map_err(app_error)
}

/// The head a status document actually serves, or [`UNMEASURED`] when it
/// serves none.
///
/// **A wire `0` is not a measurement — it is the node's own "no boundary
/// served" sentinel**, written at three independent sites: `NodeStatus`'s
/// `Default` ("zeroed boundary facts are the honest answer before any boundary
/// is served"), the validator's `node.finalized().map(|f| f.height)
/// .unwrap_or(0)`, and the replica's `None => (0, String::new(), Vec::new())`.
///
/// That last one is why this matters beside a checkpoint. A resident takes it
/// whenever it stops serving — a range-pruned backfill, an unresolvable pruned
/// view, an epoch cutover — and each of those three sites sets `serving = None`
/// and republishes while passing the LIVE `replica_prev_ckpt`, which only ever
/// climbs. Read as a measurement, one honest document then renders
/// `HEIGHT h 0` above `CHECKPOINT h 425,981`: a measured zero AND the very
/// inversion the pair is supposed to make impossible. Read as absence, it
/// renders `HEIGHT h —` — which is exactly what a node serving no boundary
/// knows about the head.
fn served_height(height: &serde_json::Value) -> i64 {
    match height.as_i64() {
        Some(head) if head > 0 => head,
        _ => UNMEASURED,
    }
}

/// What an `operations` reading the node did not publish carries.
///
/// An absent reading must display `—`, never a measured `0`: zero is a
/// legal height and timestamp.
///
/// A negative sentinel distinguishes absence from every valid measurement.
pub const UNMEASURED: i64 = -1;

/// A consensus fact the node did not publish for this role reads `—`, never a
/// zero. The view has no way to branch on an absent value itself.
pub fn optional_number(value: Option<i64>) -> String {
    match value {
        Some(number) => grouped_digits(number),
        None => "—".into(),
    }
}

/// THE NODE'S OWN STATUS, PUSHED, ON EVERY TAB.
///
/// Cheap to hold anywhere the console is standing: the node answers `status`
/// from a cell it publishes at each boundary, and the snapshot debounce means
/// one read per heartbeat. That is what lets a sync reading follow the reader
/// around instead of living on one tab — the node's phase is a fact about the
/// node, not about the surface you happen to have open.
///
/// Reconnects with backoff, parsed with the SAME reader the HTTP load uses. A
/// dropped socket is not a reason to blank the surface: the facts on screen
/// were true when they were sampled, so the subscription is rebuilt and they
/// stand until a fresher document replaces them.
pub fn node_status_live(rpc: String) -> futures::stream::BoxStream<'static, NodeFacts> {
    struct State {
        rpc: String,
        stream:
            Option<futures::stream::BoxStream<'static, ducktape_rpc::Result<serde_json::Value>>>,
        retry_attempt: u32,
    }
    futures::stream::unfold(
        State {
            rpc,
            stream: None,
            retry_attempt: 0,
        },
        move |mut state| async move {
            loop {
                if state.stream.is_none() && state.retry_attempt > 0 {
                    tokio::time::sleep(retry_delay(state.retry_attempt)).await;
                }
                if state.stream.is_none() {
                    let Ok(client) = rpc_client(&state.rpc) else {
                        state.retry_attempt = state.retry_attempt.saturating_add(1);
                        continue;
                    };
                    match client.status_events().await {
                        Ok(stream) => state.stream = Some(stream),
                        Err(_) => {
                            state.retry_attempt = state.retry_attempt.saturating_add(1);
                            continue;
                        }
                    }
                }
                match state
                    .stream
                    .as_mut()
                    .expect("stream initialized")
                    .next()
                    .await
                {
                    Some(Ok(document)) => {
                        state.retry_attempt = 0;
                        return Some((node_facts(&document), state));
                    }
                    Some(Err(_)) | None => {
                        state.stream = None;
                        state.retry_attempt = state.retry_attempt.saturating_add(1);
                    }
                }
            }
        },
    )
    .boxed()
}

/// One curated skill as the record carries it: a duckfs subtree, pinned at a
/// snapshot or tracking the committed head (an empty `source_snapshot`), and
/// whether its body is the agent's persona (`always`) or read on demand.
#[derive(Clone, Debug, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AgentSkill {
    pub name: String,
    pub source_prefix: String,
    pub source_snapshot: String,
    pub always: bool,
}

impl From<runs::SkillRef> for AgentSkill {
    fn from(skill: runs::SkillRef) -> Self {
        Self {
            name: skill.name,
            source_prefix: skill.source_prefix,
            source_snapshot: skill.source_snapshot.unwrap_or_default(),
            always: matches!(skill.load, runs::LoadMode::Always),
        }
    }
}

impl From<AgentSkill> for runs::SkillRef {
    fn from(skill: AgentSkill) -> Self {
        let pinned = !skill.source_snapshot.is_empty();
        Self {
            name: skill.name,
            source_prefix: skill.source_prefix,
            source_snapshot: pinned.then_some(skill.source_snapshot),
            load: if skill.always {
                runs::LoadMode::Always
            } else {
                runs::LoadMode::OnDemand
            },
        }
    }
}

/// One configured model: its record, whole, with its live-run fact.
#[derive(Clone, Debug, Hash, PartialEq, serde::Serialize)]
pub struct AgentRow {
    pub id: String,
    pub name: String,
    pub initials: String,
    pub capability: String,
    pub status: String,
    /// The current controller of the model's programmable account, by name.
    pub owner_handle: String,
    /// That controller's account number, decimal: the one principal whose
    /// signature may change this record.
    pub controller: String,
    /// this agent holds a RUN in flight right now — the runs module's pending
    /// register, NOT `status`. `ModelStatus` is only Active|Paused and Active
    /// is the registration default, so it says "not paused", never "working".
    pub live: bool,
    pub skills: Vec<AgentSkill>,
}

#[derive(Clone, Debug, Hash, PartialEq)]
pub struct AgentsData {
    pub generation: i64,
    pub agents: Vec<AgentRow>,
}

/// Load model configurations with current account controllers and run activity.
/// The model's registration origin does not change when control transfers.
pub async fn load_agents(rpc: String, generation: i64) -> Result<AgentsData, HydrationError> {
    async {
        let client = rpc_client(&rpc)?;
        let reply: runs::RunsReply = client
            .query(
                "runs",
                &runs::RunsQuery::Model {
                    query: runs::ModelQuery::Agents,
                },
            )
            .await?;
        let runs::RunsReply::Model(runs::ModelReply::Agents(records)) = reply else {
            return Err("the runs module returned the wrong model roster reply".into());
        };
        let (accounts, working) =
            tokio::join!(read_accounts(&client), agents_with_a_run_in_flight(&client));
        let controllers: BTreeMap<u64, u64> = accounts?
            .into_iter()
            .filter_map(|account| match account.control {
                identity::Control::Program { controller, .. }
                | identity::Control::Revoked { controller } => Some((account.number, controller)),
                identity::Control::Keys => None,
            })
            .collect();
        let names = names();
        let agents = records
            .into_iter()
            .map(|record| {
                let status = match record.status {
                    runs::ModelStatus::Active => "active",
                    runs::ModelStatus::Paused => "paused",
                }
                .to_string();
                let controller = controllers
                    .get(&record.account)
                    .ok_or_else(|| "the model account has no program controller".to_string())?;
                let owner_handle = author_display(&format!("acct:{controller}"), &names);
                Ok(AgentRow {
                    live: working.contains(&record.agent_id),
                    initials: initials_of(&record.display_name),
                    capability: record.capability,
                    id: record.agent_id,
                    name: record.display_name,
                    status,
                    owner_handle,
                    controller: controller.to_string(),
                    skills: record.skills.into_iter().map(AgentSkill::from).collect(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(AgentsData { generation, agents })
    }
    .await
    .map_err(|message: String| HydrationError {
        generation,
        message: user_error(message),
    })
}

/// The agents holding a run in flight, from the runs module's pending
/// register — the ONLY place in the product that knows an agent is working.
/// A node that cannot answer the query reports nobody working, never everybody.
async fn agents_with_a_run_in_flight(rpc: &RpcClient) -> BTreeSet<String> {
    let Ok(reply) = rpc
        .query::<_, serde_json::Value>("runs", &serde_json::json!("pending_runs"))
        .await
    else {
        return BTreeSet::new();
    };
    let Some(pending) = reply["pending_runs"].as_array() else {
        return BTreeSet::new();
    };
    pending
        .iter()
        .filter_map(|run| run["agent_id"].as_str().map(str::to_string))
        .collect()
}

/// The editor's record as the Agents view hands it back: every field the
/// controller may set, in one piece. The view holds the drafts; this is what
/// leaves it with the save.
#[derive(Debug, serde::Deserialize)]
pub struct AgentDraft {
    pub agent_id: String,
    pub display_name: String,
    pub capability: String,
    pub skills: Vec<AgentSkill>,
}

impl AgentDraft {
    fn decode(draft: &str) -> Result<Self, String> {
        serde_json::from_str(draft)
            .map_err(|error| format!("the agent draft does not decode: {error}"))
    }
}

/// Bring a new agent into the register: provision its keyless program account
/// under the signing account (`controller`, the wallet's own account number),
/// read that account back, and register the draft against it. Two committed
/// writes and one read, in order; the first write is a full block, so the
/// read never runs ahead of it.
pub async fn register_agent(
    rpc: String,
    password: String,
    controller: String,
    draft: String,
) -> Result<bool, AppError> {
    async {
        let draft = AgentDraft::decode(&draft)?;
        let controller: u64 = controller.parse().map_err(|_| {
            "registering an agent needs an account to control it — create one in Settings first"
                .to_string()
        })?;
        runs::validate_agent_id(&draft.agent_id)?;
        let display_name = draft.display_name.trim().to_owned();
        if display_name.is_empty() {
            return Err("give the agent a display name".to_string());
        }
        let rpc = rpc_client(&rpc)?;
        signed_write(
            &rpc,
            "agent",
            ::agent::encode_msg(&::agent::AgentMsg::Provision {
                name: display_name.clone(),
                program: runs::model_program(&draft.agent_id),
            }),
            password.clone(),
        )
        .await?;
        let account = newest_program_account(&rpc, controller, &display_name).await?;
        let operation = runs::ModelMsg::RegisterModel {
            account,
            agent_id: draft.agent_id,
            display_name,
            capability: draft.capability,
            recipe_hash: None,
            skills: Some(draft.skills.into_iter().map(runs::SkillRef::from).collect()),
        };
        let payload = runs::encode_msg(&runs::RunsMsg::ConfigureModel { operation });
        signed_write(&rpc, "runs", payload, password).await
    }
    .await
    .map_err(app_error)?;
    Ok(true)
}

/// The highest-numbered agent-executed program account named `name` under
/// `controller`. Accounts are numbered upward with no gaps, so after a
/// provision the newest match IS the account it minted, whatever older
/// accounts share the name.
async fn newest_program_account(
    rpc: &RpcClient,
    controller: u64,
    name: &str,
) -> Result<u64, String> {
    let page_limit =
        usize::try_from(identity::MAX_QUERY_LIMIT).expect("the identity page cap fits a usize");
    let mut newest = None;
    let mut from: identity::AccountNumber = 0;
    loop {
        let reply: identity::IdentityReply = rpc
            .query(
                "identity",
                &identity::IdentityQuery::Controlled {
                    by: controller,
                    from,
                    limit: identity::MAX_QUERY_LIMIT,
                },
            )
            .await?;
        let identity::IdentityReply::Accounts(page) = reply else {
            return Err("the identity module returned the wrong reply".to_string());
        };
        let page_is_last = page.len() < page_limit;
        let Some(last) = page.last().map(|account| account.number) else {
            break;
        };
        let runs_agent_program = |account: &identity::AccountView| {
            matches!(
                &account.control,
                identity::Control::Program { executor, .. } if executor == "agent"
            )
        };
        newest = page
            .iter()
            .filter(|account| account.name == name && runs_agent_program(account))
            .map(|account| account.number)
            .max()
            .or(newest);
        if page_is_last {
            break;
        }
        from = last + 1;
    }
    newest
        .ok_or_else(|| format!("the program account for {name:?} was not found after provisioning"))
}

/// The local account picture: whether the local user key belongs to an
/// account, and that account's public face — what the rail, the bell, the
/// titlebar and the agents view all read. `number` is the decimal account
/// number — "" when there is none. The key ASSOCIATIONS are not here: the
/// settings view reads those for itself through the kernel.
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct AccountData {
    pub generation: i64,
    pub exists: bool,
    pub number: String,
    pub name: String,
    pub bio: String,
}

impl AccountData {
    fn none(generation: i64) -> Self {
        Self {
            generation,
            exists: false,
            number: String::new(),
            name: String::new(),
            bio: String::new(),
        }
    }
}

/// Load the account the local user key belongs to (via the canonical
/// resolver, `OfKey`). A device with no user key has no account to load.
pub async fn load_account(rpc: String, generation: i64) -> Result<AccountData, HydrationError> {
    async {
        let Some(key) = local_user_key().await else {
            return Ok(AccountData::none(generation));
        };
        let client = rpc_client(&rpc)?;
        let reply: identity::IdentityReply = client
            .query("identity", &identity::IdentityQuery::OfKey { key })
            .await?;
        let account = match reply {
            identity::IdentityReply::Account(account) => account,
            identity::IdentityReply::Accounts(_)
            | identity::IdentityReply::Resolved(_)
            | identity::IdentityReply::Gen(_) => {
                return Err("the identity module returned the wrong reply".to_string());
            }
        };
        let Some(account) = account else {
            return Ok(AccountData::none(generation));
        };
        Ok(AccountData {
            generation,
            exists: true,
            number: account.number.to_string(),
            name: account.name,
            bio: account.bio.unwrap_or_default(),
        })
    }
    .await
    .map_err(|message: String| HydrationError {
        generation,
        message: user_error(message),
    })
}

/// The chain a network names, read once off `/v1/status` — the welcome step
/// runs before the console's status stream exists, and every key consent is
/// chain-scoped.
pub async fn chain_id_of(rpc: String) -> Result<String, AppError> {
    async {
        let client = rpc_client(&rpc)?;
        let status = client.status_json().await?;
        named_chain(node_facts(&status).chain_id)
    }
    .await
    .map_err(app_error)
}

/// The probe's answer as the discriminant the launch window branches on.
pub fn account_probe(found: bool) -> crate::AccountProbe {
    match found {
        true => crate::AccountProbe::Found,
        false => crate::AccountProbe::Missing,
    }
}

/// Rename the account the local user key belongs to (origin-gated: any member
/// key is the authority).
pub async fn set_account_name(
    rpc: String,
    password: String,
    name: String,
) -> Result<bool, AppError> {
    async {
        let name = bounded_text(name, "account name", identity::MAX_NAME_LEN)?;
        let client = rpc_client(&rpc)?;
        signed_write(
            &client,
            "identity",
            identity::encode_msg(&identity::IdentityMsg::SetName { name }),
            password,
        )
        .await
    }
    .await
    .map_err(app_error)?;
    Ok(true)
}

/// Found an account with this device's key as its first member. The frame
/// signature is the key's possession proof; the name is display-only.
pub async fn create_account(rpc: String, password: String, name: String) -> Result<bool, AppError> {
    async {
        let name = bounded_text(name, "account name", identity::MAX_NAME_LEN)?;
        let client = rpc_client(&rpc)?;
        signed_write(
            &client,
            "identity",
            identity::encode_msg(&identity::IdentityMsg::Create {
                name,
                scheme: identity::KeyScheme::Ed25519,
            }),
            password,
        )
        .await
    }
    .await
    .map_err(app_error)?;
    Ok(true)
}

/// Mint the `AddKey` ticket that admits another device's pasted ed25519 key
/// to this device's account: this device (a member) consents to the key at
/// its CURRENT generation on `chain_id`, and the other device submits the
/// ticket verbatim ([`join_with_ticket`], or `ducktape account key join`).
/// The consent is single-use — the module advances the generation on
/// admission.
pub async fn mint_key_ticket(
    rpc: String,
    password: String,
    chain_id: String,
    pubkey: String,
    label: String,
) -> Result<String, AppError> {
    async {
        let chain_id = named_chain(chain_id)?;
        let new_key = hex_decode(pubkey.trim())?;
        let wellformed = identity::KeyScheme::Ed25519.pubkey_wellformed(&new_key);
        if !wellformed {
            return Err("that is not a well-formed ed25519 public key".to_string());
        }
        let label = optional_label(label)?;
        let client = rpc_client(&rpc)?;
        let msg = consented_add_key(
            &client,
            password,
            &chain_id,
            identity::KeyScheme::Ed25519,
            &new_key,
            label,
        )
        .await?;
        Ok(add_key_ticket(&msg))
    }
    .await
    .map_err(app_error)
}

/// The ticket text: ONE json line, exactly the `AddKey` payload the joining
/// key signs into its frame.
fn add_key_ticket(msg: &identity::IdentityMsg) -> String {
    String::from_utf8(identity::encode_msg(msg)).expect("json is utf-8")
}

/// An `AddKey` is chain-scoped; a node that has not named its chain yet
/// cannot be consented on.
fn named_chain(chain_id: String) -> Result<String, String> {
    if chain_id.is_empty() {
        return Err(
            "the connected node has not named its chain yet — a key consent is chain-scoped"
                .to_string(),
        );
    }
    Ok(chain_id)
}

fn optional_label(label: String) -> Result<Option<String>, String> {
    match label.trim() {
        "" => Ok(None),
        text => Ok(Some(bounded_text(
            text.to_string(),
            "key label",
            identity::MAX_LABEL_LEN,
        )?)),
    }
}

/// A key's current generation — what a consent signs, so it is single-use.
async fn key_generation(client: &RpcClient, key: &[u8]) -> Result<u64, String> {
    let reply: identity::IdentityReply = client
        .query(
            "identity",
            &identity::IdentityQuery::KeyGen { key: key.to_vec() },
        )
        .await?;
    match reply {
        identity::IdentityReply::Gen(generation) => Ok(generation),
        identity::IdentityReply::Account(_)
        | identity::IdentityReply::Accounts(_)
        | identity::IdentityReply::Resolved(_) => {
            Err("the identity module returned the wrong reply".to_string())
        }
    }
}

/// How long a consent this app mints stays spendable, in blocks —
/// `consensus_time` is a block height and a validator network heartbeats about
/// once a second, so this is roughly a day. There is no revoke op: this window
/// IS how a mis-issued ticket dies.
const CONSENT_TTL: u64 = 86_400;

/// The `AddKey` this device consents to for `new_key` (of `scheme`) at its
/// current generation, into THIS device's account, spendable for
/// [`CONSENT_TTL`] blocks.
async fn consented_add_key(
    client: &RpcClient,
    password: String,
    chain_id: &str,
    scheme: identity::KeyScheme,
    new_key: &[u8],
    label: Option<String>,
) -> Result<identity::IdentityMsg, String> {
    let generation = key_generation(client, new_key).await?;
    let account = own_account(client).await?.number;
    let expires_at = consent_expiry(client).await?;
    let authorizer = sign_add_key_consent(
        password, chain_id, scheme, new_key, generation, account, expires_at,
    )
    .await?;
    Ok(identity::IdentityMsg::AddKey {
        scheme,
        label,
        authorizer,
    })
}

/// The `expires_at` a consent minted right now carries.
async fn consent_expiry(client: &RpcClient) -> Result<u64, String> {
    Ok(client
        .status()
        .await
        .map_err(|error| error.to_string())?
        .height
        + CONSENT_TTL)
}

/// The account this device's key belongs to, by the canonical resolver.
async fn own_account(client: &RpcClient) -> Result<identity::AccountView, String> {
    let Some(key) = local_user_key().await else {
        return Err("this device has no user key".to_string());
    };
    account_reply(
        client
            .query("identity", &identity::IdentityQuery::OfKey { key })
            .await?,
    )?
    .ok_or_else(|| "this device's key belongs to no account yet".to_string())
}

fn account_reply(reply: identity::IdentityReply) -> Result<Option<identity::AccountView>, String> {
    match reply {
        identity::IdentityReply::Account(account) => Ok(account),
        identity::IdentityReply::Accounts(_)
        | identity::IdentityReply::Resolved(_)
        | identity::IdentityReply::Gen(_) => {
            Err("the identity module returned the wrong reply".to_string())
        }
    }
}

fn identity_msg(msg: &identity::IdentityMsg) -> sdk::Msg {
    sdk::Msg {
        target: "identity".into(),
        payload: identity::encode_msg(msg),
    }
}

// ============================================================================
// browser ceremonies (`authpage`)
// ============================================================================

/// How long a browser touch may take before the app gives up on it.
const CEREMONY_TIMEOUT: Duration = Duration::from_secs(300);

/// The ceremony owns its callback socket. Cancelling the UI task or timing out
/// drops the listener and any partial request along with the wait.
async fn browser_ceremony(request: authpage::Request) -> Result<authpage::Outcome, String> {
    let listener = authpage::Listener::bind()
        .await
        .map_err(|e| format!("auth callback: {e}"))?;
    let callback = listener.callback_url();
    let url = authpage::request_url(authpage::AUTH_PAGE, &request, &callback);
    let op = request_op(&request);
    let opened = authpage::open_browser(&url);
    if !opened {
        tracing::warn!(target: "ducktape::auth", event = "ceremony_failed", surface = "browser", op, reason = "no_browser_opener");
        return Err("no browser opener on this machine (xdg-open / open)".to_string());
    }
    tracing::info!(target: "ducktape::auth", event = "ceremony_shown", surface = "browser", op);
    let outcome = tokio::time::timeout(CEREMONY_TIMEOUT, listener.wait())
        .await
        .map_err(|_| "the browser did not answer in time".to_string())
        .and_then(|outcome| outcome);
    match &outcome {
        Ok(_) => {
            tracing::info!(target: "ducktape::auth", event = "ceremony_answered", surface = "browser", op)
        }
        Err(reason) => {
            tracing::warn!(target: "ducktape::auth", event = "ceremony_failed", surface = "browser", op, reason)
        }
    }
    outcome
}

/// Register a NEW passkey on this device's account: ceremony 1 creates it
/// (the page hands back its key), this device consents, ceremony 2 has the
/// passkey sign its own `AddKey` frame — possession proven by the assertion.
pub async fn register_passkey(
    rpc: String,
    password: String,
    chain_id: String,
    label: String,
) -> Result<bool, AppError> {
    async {
        let chain_id = named_chain(chain_id)?;
        let label = optional_label(label)?;
        require_password(&password)?;
        let client = rpc_client(&rpc)?;
        let account = own_account(&client).await?;
        let registered = browser_ceremony(authpage::Request::Create {
            chain_id: chain_id.to_string(),
            challenge: authpage::create_challenge(),
            user: account.number,
            name: account.name,
        })
        .await?;
        let authpage::Outcome::Create { public_key, .. } = registered else {
            return Err("expected a passkey registration".to_string());
        };
        let msg = consented_add_key(
            &client,
            password,
            &chain_id,
            identity::KeyScheme::Secp256r1,
            &public_key,
            label,
        )
        .await?;
        let (request, preimage) =
            authpage::passkey_frame_request(&public_key, next_sequence(), &identity_msg(&msg));
        let signed = browser_ceremony(request).await?;
        submit_raw_frame(
            &client,
            "identity",
            authpage::passkey_frame(preimage, &signed)?,
        )
        .await
    }
    .await
    .map_err(app_error)?;
    Ok(true)
}

/// Link an Ethereum wallet to this device's account: touch 1 reveals its
/// key, this device consents, touch 2 has the wallet sign its own `AddKey`
/// frame.
pub async fn link_wallet(
    rpc: String,
    password: String,
    chain_id: String,
    label: String,
) -> Result<bool, AppError> {
    async {
        let chain_id = named_chain(chain_id)?;
        let label = optional_label(label)?;
        require_password(&password)?;
        let client = rpc_client(&rpc)?;
        own_account(&client).await?;
        let reveal = authpage::reveal_message();
        let touch = browser_ceremony(authpage::Request::Eth {
            message: reveal.clone(),
        })
        .await?;
        let pubkey = authpage::wallet_pubkey(&reveal, &touch)?;
        let msg = consented_add_key(
            &client,
            password,
            &chain_id,
            identity::KeyScheme::Secp256k1,
            &pubkey,
            label,
        )
        .await?;
        let (request, preimage) =
            authpage::wallet_frame_request(&pubkey, next_sequence(), &identity_msg(&msg));
        let touch = browser_ceremony(request).await?;
        submit_raw_frame(
            &client,
            "identity",
            authpage::wallet_frame(preimage, &touch)?,
        )
        .await
    }
    .await
    .map_err(app_error)?;
    Ok(true)
}

/// Admit THIS device into an account by a passkey's consent. TWO browser
/// touches: a consent names the account it admits into, and only the passkey
/// knows which that is — touch 1 asks (`userHandle`), touch 2 is the assertion
/// over this key's `AddKey` preimage for that account. This device signs the
/// frame (the key being admitted).
pub async fn login_with_passkey(
    rpc: String,
    password: String,
    chain_id: String,
    label: String,
) -> Result<bool, AppError> {
    async {
        let chain_id = named_chain(chain_id)?;
        let label = optional_label(label)?;
        require_password(&password)?;
        let Some(device_key) = local_user_key().await else {
            return Err("this device has no user key".to_string());
        };
        let client = rpc_client(&rpc)?;
        let generation = key_generation(&client, &device_key).await?;
        let number = authpage::assertion_account(
            &chain_id,
            &browser_ceremony(authpage::account_request()).await?,
        )?;
        let account = account_reply(
            client
                .query("identity", &identity::IdentityQuery::Get { number })
                .await?,
        )?
        .ok_or_else(|| format!("the passkey names account {number}, unknown to this node"))?;
        let expires_at = consent_expiry(&client).await?;
        let consent = browser_ceremony(authpage::login_request(
            &chain_id,
            &device_key,
            generation,
            number,
            expires_at,
        ))
        .await?;
        let (_, proof) = authpage::login_consent(&chain_id, &consent)?;
        let msg = authpage::login_add_key(
            &chain_id,
            &device_key,
            generation,
            &account,
            label,
            proof,
            expires_at,
        )?;
        signed_write(&client, "identity", identity::encode_msg(&msg), password).await
    }
    .await
    .map_err(app_error)?;
    Ok(true)
}

// ============================================================================
// QR ceremonies — the browser is a phone that scanned the app's screen
// ============================================================================

/// One reading of a ceremony the launch window (or the Settings card) is
/// showing: `show_qr` carries the URL to render, `working` a line of what
/// the app is doing between touches, `done`/`failed` close the stream.
#[derive(Clone, Debug, Hash, PartialEq)]
pub struct CeremonyStep {
    pub phase: String,
    pub qr: String,
    pub detail: String,
    /// `show_qr` only: how long the code stays good, `m:ss`, re-sent every
    /// second; empty on every other phase.
    pub left: String,
}

impl CeremonyStep {
    fn working(detail: &str) -> Self {
        Self {
            phase: "working".into(),
            qr: String::new(),
            detail: detail.into(),
            left: String::new(),
        }
    }

    fn show_qr(url: String, detail: &str, left: Duration) -> Self {
        Self {
            phase: "show_qr".into(),
            qr: url,
            detail: detail.into(),
            left: authpage::countdown(left),
        }
    }

    fn done() -> Self {
        Self {
            phase: "done".into(),
            qr: String::new(),
            detail: String::new(),
            left: String::new(),
        }
    }

    fn failed(message: String) -> Self {
        Self {
            phase: "failed".into(),
            qr: String::new(),
            detail: message,
            left: String::new(),
        }
    }
}

/// Which welcome door a ceremony came through: a name was typed only on the
/// create path.
pub fn welcome_door(name_draft: &str) -> crate::WelcomeDoor {
    match name_draft.trim().is_empty() {
        true => crate::WelcomeDoor::Login,
        false => crate::WelcomeDoor::Create,
    }
}

/// The step's phase as the discriminant the handlers branch on.
pub fn ceremony_phase(step: &CeremonyStep) -> crate::CeremonyPhase {
    match step.phase.as_str() {
        "show_qr" => crate::CeremonyPhase::ShowQr,
        "working" => crate::CeremonyPhase::Working,
        "done" => crate::CeremonyPhase::Done,
        _ => crate::CeremonyPhase::Failed,
    }
}

type StepSender = futures::channel::mpsc::Sender<CeremonyStep>;

/// Hand one reading to the UI; a closed receiver means the lane was
/// invalidated (a cancel), which ends the ceremony as an error nobody reads.
async fn step(tx: &mut StepSender, step: CeremonyStep) -> Result<(), String> {
    use futures::SinkExt as _;
    tx.send(step)
        .await
        .map_err(|_| "the ceremony was cancelled".to_string())
}

/// The page op a request asks for — the `op` field of its fragment, for logs.
fn request_op(request: &authpage::Request) -> &'static str {
    match request {
        authpage::Request::Create { .. } => "create",
        authpage::Request::Get { .. } => "get",
        authpage::Request::Eth { .. } => "eth",
    }
}

/// One browser ceremony run ON A PHONE: mint a relay slot, hand the URL to
/// the UI as a QR under `detail` (the line the screen shows beside it), then
/// wait for the phone's answer under the same ceiling the desktop path uses.
/// `relay_base` is the auth host (tests point it at a fake).
pub(crate) async fn qr_ceremony(
    relay_base: &str,
    request: authpage::Request,
    detail: &str,
    tx: &mut StepSender,
) -> Result<authpage::Outcome, String> {
    let relay = authpage::Relay::at(relay_base);
    let url = authpage::request_url(authpage::AUTH_PAGE, &request, &relay.callback_url());
    let op = request_op(&request);
    tracing::info!(target: "ducktape::auth", event = "ceremony_shown", surface = "phone", op, relay = %relay.id);
    step(
        tx,
        CeremonyStep::show_qr(url.clone(), detail, CEREMONY_TIMEOUT),
    )
    .await?;
    let started = std::time::Instant::now();
    let waiting = relay.wait(CEREMONY_TIMEOUT);
    tokio::pin!(waiting);
    // The countdown: the same QR re-sent each second with the time it has
    // left, so the screen can show it. The first tick is a second away —
    // the reading above already carries the full ceiling.
    let second = Duration::from_secs(1);
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + second, second);
    let outcome = loop {
        tokio::select! {
            answered = &mut waiting => {
                break answered;
            }
            _ = ticks.tick() => {
                let left = CEREMONY_TIMEOUT.saturating_sub(started.elapsed());
                step(tx, CeremonyStep::show_qr(url.clone(), detail, left)).await?;
            }
        }
    };
    match &outcome {
        Ok(_) => {
            tracing::info!(target: "ducktape::auth", event = "ceremony_answered", surface = "phone", op)
        }
        Err(reason) => {
            tracing::warn!(target: "ducktape::auth", event = "ceremony_failed", surface = "phone", op, reason)
        }
    }
    outcome
}

/// Run `body` as a step stream: every `Err` becomes a `failed` step, `Ok` a
/// `done` one. The body is driven BY the stream's own polls (no spawn, so
/// no runtime handle is assumed), and every reading — the closing one too —
/// travels the one channel, so the UI sees them in order. Dropping the
/// stream (a lane invalidation) drops the body mid-await: the cancel.
fn ceremony_stream<F, Fut>(body: F) -> futures::stream::BoxStream<'static, CeremonyStep>
where
    F: FnOnce(StepSender) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    use futures::{SinkExt as _, StreamExt as _};
    let (tx, rx) = futures::channel::mpsc::channel::<CeremonyStep>(8);
    let mut closing = tx.clone();
    let driving = async move {
        let last = match body(tx).await {
            Ok(()) => {
                tracing::info!(target: "ducktape::auth", event = "ceremony_stream_done");
                CeremonyStep::done()
            }
            Err(message) => {
                tracing::warn!(target: "ducktape::auth", event = "ceremony_stream_failed", reason = %message);
                CeremonyStep::failed(message)
            }
        };
        let _ = closing.send(last).await;
    };
    let driver = futures::stream::once(driving).filter_map(|()| async { None });
    futures::stream::select(rx, driver).boxed()
}

/// Create the account with this device's key (no touch), then register a
/// passkey from the phone: QR 1 creates it, this device consents, QR 2 has
/// the passkey sign its own admission.
pub fn create_account_by_qr(
    rpc: String,
    password: String,
    chain_id: String,
    name: String,
) -> futures::stream::BoxStream<'static, CeremonyStep> {
    ceremony_stream(move |mut tx| async move {
        let chain_id = named_chain(chain_id)?;
        require_password(&password)?;
        step(&mut tx, CeremonyStep::working("Creating the account…")).await?;
        create_account(rpc.clone(), password.clone(), name)
            .await
            .map_err(|e| e.message)?;
        add_passkey_steps(&mut tx, &rpc, password, &chain_id, None).await
    })
}

/// Register a passkey on the account this device's key already belongs to.
pub fn add_passkey_by_qr(
    rpc: String,
    password: String,
    chain_id: String,
    label: String,
) -> futures::stream::BoxStream<'static, CeremonyStep> {
    ceremony_stream(move |mut tx| async move {
        let chain_id = named_chain(chain_id)?;
        let label = optional_label(label)?;
        require_password(&password)?;
        add_passkey_steps(&mut tx, &rpc, password, &chain_id, label).await
    })
}

/// QR 1 (create) → this device consents → QR 2 (the passkey signs its own
/// `AddKey`) → submit. The phone half of `register_passkey`.
async fn add_passkey_steps(
    tx: &mut StepSender,
    rpc: &str,
    password: String,
    chain_id: &str,
    label: Option<String>,
) -> Result<(), String> {
    let client = rpc_client(rpc)?;
    let account = own_account(&client).await?;
    let registered = qr_ceremony(
        authpage::AUTH_PAGE,
        authpage::Request::Create {
            chain_id: chain_id.to_string(),
            challenge: authpage::create_challenge(),
            user: account.number,
            name: account.name,
        },
        "Scan 1 of 2 — your phone creates the passkey.",
        tx,
    )
    .await?;
    let authpage::Outcome::Create { public_key, .. } = registered else {
        return Err("expected a passkey registration".to_string());
    };
    step(tx, CeremonyStep::working("Consenting to the new key…")).await?;
    let msg = consented_add_key(
        &client,
        password,
        chain_id,
        identity::KeyScheme::Secp256r1,
        &public_key,
        label,
    )
    .await?;
    let (request, preimage) =
        authpage::passkey_frame_request(&public_key, next_sequence(), &identity_msg(&msg));
    let signed = qr_ceremony(
        authpage::AUTH_PAGE,
        request,
        "Scan 2 of 2 — confirm with the passkey you just made.",
        tx,
    )
    .await?;
    step(tx, CeremonyStep::working("Submitting…")).await?;
    submit_raw_frame(
        &client,
        "identity",
        authpage::passkey_frame(preimage, &signed)?,
    )
    .await?;
    Ok(())
}

/// Admit THIS device by a passkey's consent given on the phone: two QRs, one
/// per touch — the first asks the passkey which account it speaks for, the
/// second is the consent, bound to that account. The phone half of
/// `login_with_passkey`.
pub fn login_by_qr(
    rpc: String,
    password: String,
    chain_id: String,
) -> futures::stream::BoxStream<'static, CeremonyStep> {
    ceremony_stream(move |mut tx| async move {
        let chain_id = named_chain(chain_id)?;
        require_password(&password)?;
        let Some(device_key) = local_user_key().await else {
            return Err("this device has no user key".to_string());
        };
        let client = rpc_client(&rpc)?;
        let generation = key_generation(&client, &device_key).await?;
        let named = qr_ceremony(
            authpage::AUTH_PAGE,
            authpage::account_request(),
            "Confirm with the passkey that belongs to your account.",
            &mut tx,
        )
        .await?;
        let number = authpage::assertion_account(&chain_id, &named)?;
        step(&mut tx, CeremonyStep::working("Reading the account…")).await?;
        let account = account_reply(
            client
                .query("identity", &identity::IdentityQuery::Get { number })
                .await?,
        )?
        .ok_or_else(|| format!("the passkey names account {number}, unknown to this node"))?;
        let expires_at = consent_expiry(&client).await?;
        let consent = qr_ceremony(
            authpage::AUTH_PAGE,
            authpage::login_request(&chain_id, &device_key, generation, number, expires_at),
            "Confirm once more to admit this device to the account.",
            &mut tx,
        )
        .await?;
        let (_, proof) = authpage::login_consent(&chain_id, &consent)?;
        step(&mut tx, CeremonyStep::working("Joining the account…")).await?;
        let msg = authpage::login_add_key(
            &chain_id,
            &device_key,
            generation,
            &account,
            None,
            proof,
            expires_at,
        )?;
        signed_write(&client, "identity", identity::encode_msg(&msg), password).await?;
        Ok(())
    })
}

/// A pasted ticket is an `AddKey` or it is refused HERE, before any signature
/// — the module would refuse a stray `SetName` too, but under a name that
/// says nothing about tickets.
fn add_key_ticket_bytes(ticket: &str) -> Result<Vec<u8>, String> {
    let ticket = ticket.trim();
    let is_add_key = matches!(
        identity::decode_msg(ticket.as_bytes())?,
        identity::IdentityMsg::AddKey { .. }
    );
    if !is_add_key {
        return Err(
            "that is not an add-key ticket (mint one on a device that is already a member)"
                .to_string(),
        );
    }
    Ok(ticket.as_bytes().to_vec())
}

/// Join the account a ticket names with THIS device's key: the ticket bytes
/// ride verbatim (the member's consent is over them), signed by the key being
/// admitted.
pub async fn join_with_ticket(
    rpc: String,
    password: String,
    ticket: String,
) -> Result<bool, AppError> {
    async {
        let payload = add_key_ticket_bytes(&ticket)?;
        let client = rpc_client(&rpc)?;
        signed_write(&client, "identity", payload, password).await
    }
    .await
    .map_err(app_error)?;
    Ok(true)
}

/// Remove one key from this device's account (member-gated; the module
/// refuses the last key).
pub async fn remove_account_key(
    rpc: String,
    password: String,
    pubkey: String,
) -> Result<bool, AppError> {
    async {
        let key = hex_decode(pubkey.trim())?;
        let client = rpc_client(&rpc)?;
        signed_write(
            &client,
            "identity",
            identity::encode_msg(&identity::IdentityMsg::RemoveKey { key }),
            password,
        )
        .await
    }
    .await
    .map_err(app_error)?;
    Ok(true)
}

#[cfg(test)]
mod account_ticket_tests {
    use super::*;

    fn member() -> ed25519::PrivateKey {
        ed25519::PrivateKey::from_seed(41)
    }

    /// The ticket the app mints IS the `AddKey` the CLI's `key join` submits:
    /// one line, decodes to the message, and the consent verifies under the
    /// module's own namespace at the minted generation — and at no other.
    #[test]
    fn a_ticket_is_the_add_key_the_cli_accepts() {
        let new_key = ed25519::PrivateKey::from_seed(42)
            .public_key()
            .as_ref()
            .to_vec();
        let authorizer = workspace_config::ed25519_authorizer(
            &member(),
            "chain-a",
            identity::KeyScheme::Ed25519,
            &new_key,
            3,
            11,
            900,
        );
        let ticket = add_key_ticket(&identity::IdentityMsg::AddKey {
            scheme: identity::KeyScheme::Ed25519,
            label: Some("phone".into()),
            authorizer,
        });
        assert_eq!(ticket.lines().count(), 1, "one json line, pasteable");
        let identity::IdentityMsg::AddKey {
            scheme,
            label,
            authorizer,
        } = identity::decode_msg(ticket.as_bytes()).unwrap()
        else {
            panic!("a ticket is an AddKey");
        };
        assert_eq!(scheme, identity::KeyScheme::Ed25519);
        assert_eq!(label.as_deref(), Some("phone"));
        assert_eq!(authorizer.key, member().public_key().as_ref());
        assert_eq!(authorizer.account, 11);
        assert_eq!(authorizer.expires_at, 900);
        let preimage = |generation, account, expires_at| {
            identity::add_key_preimage(
                "chain-a",
                identity::KeyScheme::Ed25519,
                &new_key,
                generation,
                account,
                expires_at,
            )
        };
        let verifies = |generation, account, expires_at| {
            identity::KeyScheme::Ed25519.verify(
                &authorizer.key,
                identity::IDENTITY_ADD_KEY_NS,
                &preimage(generation, account, expires_at),
                &authorizer.proof,
            )
        };
        assert!(verifies(3, 11, 900), "the consent is over the minted terms");
        assert!(!verifies(4, 11, 900), "and is single-use");
        assert!(!verifies(3, 12, 900), "account-bound");
        assert!(!verifies(3, 11, 901), "expiry-bound");
        assert_eq!(
            add_key_ticket_bytes(&format!("  {ticket}\n")).unwrap(),
            ticket.as_bytes(),
            "the joining frame carries the ticket bytes verbatim, whitespace trimmed"
        );
    }

    #[test]
    fn a_non_add_key_ticket_is_refused_before_any_signature() {
        let stray = String::from_utf8(identity::encode_msg(&identity::IdentityMsg::SetName {
            name: "x".into(),
        }))
        .unwrap();
        let err = add_key_ticket_bytes(&stray).unwrap_err();
        assert!(err.contains("not an add-key ticket"), "{err}");
        assert!(add_key_ticket_bytes("not json").is_err());
    }
}

#[cfg(test)]
mod qr_ceremony_tests {
    use super::*;
    use std::io::{BufRead as _, BufReader, Write as _};

    /// A relay that answers 204 `absent` times, then `json` once and exits.
    fn fake_relay(absent: usize, json: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for (served, stream) in listener.incoming().take(absent + 1).enumerate() {
                let mut stream = stream.unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let is_the_answer = served == absent;
                let response = match is_the_answer {
                    true => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{json}",
                        json.len()
                    ),
                    false => "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_string(),
                };
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        base
    }

    const ASSERTION: &str = r#"{"op":"get","credentialId":"AQ","authenticatorData":"AQ","clientDataJSON":"AQ","signature":"AQ","userHandle":"6zD6Woip0W_PPk0EWZGNZdwjPHgvY2dqMFHQVJ7xyIwqAAAAAAAAAA"}"#;

    /// Invalidating the UI stream must close the request already at the relay,
    /// even if that relay never sends a response or the next countdown tick.
    #[tokio::test]
    async fn cancelling_a_ceremony_stream_closes_the_pending_relay_request() {
        use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let mut stream = ceremony_stream(move |mut tx| async move {
            qr_ceremony(
                &base,
                authpage::Request::Get { challenge: [7; 32] },
                "Confirm with the passkey.",
                &mut tx,
            )
            .await?;
            Ok(())
        });
        let receive_request = async {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            loop {
                let mut line = String::new();
                let read = socket.read_line(&mut line).await.unwrap();
                assert_ne!(
                    read, 0,
                    "the request must reach the relay before cancellation"
                );
                let headers_complete = line == "\r\n";
                if headers_complete {
                    return socket;
                }
            }
        };
        let mut socket = {
            let consume = async { while stream.next().await.is_some() {} };
            tokio::select! {
                socket = receive_request => socket,
                () = consume => panic!("the unanswered ceremony ended before cancellation"),
            }
        };
        drop(stream);
        let mut byte = [0];
        assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
    }

    /// the first reading is the QR — the auth page URL carrying this relay's
    /// slot as its callback — and the outcome is the phone's answer.
    #[tokio::test(flavor = "current_thread")]
    async fn a_qr_ceremony_shows_the_url_then_yields_the_outcome() {
        let base = fake_relay(1, ASSERTION);
        let (mut tx, mut rx) = futures::channel::mpsc::channel::<CeremonyStep>(8);
        let outcome = qr_ceremony(
            &base,
            authpage::Request::Get {
                challenge: [7u8; 32],
            },
            "Confirm with the passkey.",
            &mut tx,
        )
        .await
        .unwrap();
        assert!(matches!(
            outcome,
            authpage::Outcome::Get {
                user_handle: Some(handle),
                ..
            } if handle == authpage::UserHandle::new("demo#a1b2c3d4", 42)
        ));
        let shown = rx.next().await.unwrap();
        assert_eq!(shown.phase, "show_qr");
        assert!(
            shown
                .qr
                .starts_with("https://auth.ducktape.industries/#op=get&challenge="),
            "{}",
            shown.qr
        );
        // the callback is percent-encoded into the fragment: `/r/` survives as %2Fr%2F
        assert!(
            shown.qr.contains("&cb=http%3A%2F%2F127.0.0.1"),
            "{}",
            shown.qr
        );
        assert!(shown.qr.contains("%2Fr%2F"), "{}", shown.qr);
    }

    /// the stream shape: readings in order, and the closing one last.
    #[tokio::test(flavor = "current_thread")]
    async fn a_ceremony_stream_ends_with_its_closing_step_in_order() {
        let steps: Vec<CeremonyStep> = ceremony_stream(|mut tx| async move {
            step(&mut tx, CeremonyStep::working("one")).await?;
            step(&mut tx, CeremonyStep::working("two")).await?;
            Err("boom".to_string())
        })
        .collect()
        .await;
        let phases: Vec<&str> = steps.iter().map(|s| s.phase.as_str()).collect();
        assert_eq!(phases, ["working", "working", "failed"]);
        assert_eq!(steps[1].detail, "two");
        assert_eq!(steps[2].detail, "boom");
        let done: Vec<CeremonyStep> = ceremony_stream(|_tx| async move { Ok(()) }).collect().await;
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].phase, "done");
    }
}
