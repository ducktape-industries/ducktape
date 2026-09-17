//! Where a network view comes from: the deployed artifact of the registry
//! entry it belongs to — a module's, whose frame may embed a view, or a
//! view-only entry's, whose frame IS the view. The registry names the
//! entry's kind and its ACTIVE code hash (`ModulesQuery::ModuleStatus`), the
//! artifact under that hash is fetched and verified (`view_artifact`), and
//! its view — component bytes plus the assets shipped beside them — is
//! handed over as one unit. The view drawn is the view of the code that
//! runs — unless the member is TASTING a proposed one: the taste set
//! ([`taste_set`]) is every `(module, hash)` an open `UpdateModule` /
//! `RegisterModule` proposal or a scheduled swap names, and a seat asked
//! for one of those hashes ([`resolve`] with a `wanted`) reads that frame
//! instead of the active one. Nothing on-chain moves for it.
//!
//! Every reading here is strict. A registry reply of another shape, a module
//! the registry does not list or lists twice, a hash that is not 32 bytes,
//! a fetch that fails or a body that does not hash to what was asked for is
//! an [`Error`], never a fallback to a desktop resource. The only quiet
//! outcomes are the two the network itself asserts: a module admitted but
//! not yet activated ([`ViewSource::NotActivated`]) and a verified
//! deployment that ships no view ([`ViewSource::Missing`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use ducktape_rpc::Client;
use serde::Deserialize;

use super::view_artifact::{self, Frame};

/// Why a frame could not be fetched and verified, as [`frame`] tells it.
pub use super::view_artifact::Error as FetchError;

/// The BUILT-IN surfaces whose view is drawn by the module's own artifact:
/// each has a props builder of its own and is asked of the connected node at
/// connect and again at every block that moves its deployment. Every other
/// view off the node is a registry-listed `Kind::View` entry, seated from
/// `module_status` alone.
///
/// Most are tabs, with a `ShellTab` arm and an intent decoder each. `inbox`
/// is not: it is the bell overlay's body, seated in the overlay's own slot
/// and speaking the two generic doors. It is asked for here rather than
/// lazily, because the count beside the rail is a headless run of this same
/// view and it is wanted at connect.
pub const MODULE_OWNED: [&str; 6] = ["governance", "files", "pages", "chat", "forge", "inbox"];

/// The desktop's own views, staged beside the binary and asked for at boot.
/// Every view that is not one of these comes off the connected node.
pub const DESKTOP_OWNED: [&str; 6] =
    ["members", "agents", "node", "explorer", "settings", "palette"];

pub fn desktop_owned(module: &str) -> bool {
    DESKTOP_OWNED.contains(&module)
}

/// The assets a deployment ships beside its view, by canonical relative
/// path. Shared between the guest and the host surfaces that paint them,
/// and swapped with the guest as one unit.
pub type Assets = BTreeMap<String, Vec<u8>>;

#[derive(Debug)]
pub enum Error {
    /// The registry could not be read as `module_status`, or does not name
    /// this module exactly once with a 32-byte active hash.
    Status(String),
    Artifact(view_artifact::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status(error) => write!(formatter, "module status: {error}"),
            Self::Artifact(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, PartialEq)]
pub enum ViewSource {
    /// Admitted, but no activation has reached its boundary yet.
    NotActivated,
    /// The active deployment verified, and it ships no view.
    Missing { hash: [u8; 32] },
    Ready {
        hash: [u8; 32],
        component: Vec<u8>,
        assets: Arc<Assets>,
    },
}

// `ModulesReply::ModuleStatus`, as `/v1/query` serializes it — mirrored
// field for field from crates/modules/system/modules/src/interface.rs
// (`ModuleCode`, `Kind`, `ScheduledSwap`, `Activation`) and refused on any drift:
// a field this reader does not know is a registry it does not understand.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusReply {
    module_status: ModuleStatus,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModuleStatus {
    modules: Vec<ModuleCode>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModuleCode {
    module_id: String,
    kind: Kind,
    active_code_hash: Vec<u8>,
    pending: Option<ScheduledSwap>,
    #[allow(dead_code, reason = "read for shape only")]
    history: Vec<Activation>,
}

pub use super::view_artifact::Kind;

/// One registry entry as the seat set reads it: what it is, its active
/// code hash — `None` for an admission that has not reached its boundary —
/// and the swap scheduled after it, if one is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub hash: Option<[u8; 32]>,
    pub pending: Option<Scheduled>,
}

/// A swap the registry holds for its height: the post-pass canary record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scheduled {
    pub hash: [u8; 32],
    pub activation_height: u64,
    /// The height the readiness quorum latched at, once it has.
    pub ready_at: Option<u64>,
}

impl Scheduled {
    /// Past its height and never latched: the registry keeps it until it
    /// is cancelled, but it will never activate — `ScheduledSwap::stale_at`
    /// in the registry's own words.
    pub fn stale_at(&self, height: u64) -> bool {
        let past_due = self.activation_height <= height;
        let never_latched = self.ready_at.is_none();
        past_due && never_latched
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduledSwap {
    #[allow(dead_code, reason = "read for shape only")]
    name: String,
    activation_height: u64,
    code_hash: Vec<u8>,
    #[allow(dead_code, reason = "read for shape only")]
    readiness: Vec<Vec<u8>>,
    ready_at: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code, reason = "read for shape only")]
struct Activation {
    height: u64,
    code_hash: Vec<u8>,
}

/// Every registry entry — its kind, and its active code hash (`None` for an
/// admission that has not reached its boundary) — in one registry read,
/// id-ordered as the registry lists them.
pub async fn active_hashes(client: &Client) -> Result<BTreeMap<String, Entry>, Error> {
    let reply: StatusReply = client
        .query("modules", &serde_json::json!("module_status"))
        .await
        .map_err(|error| Error::Status(error.to_string()))?;
    let mut entries = BTreeMap::new();
    for entry in reply.module_status.modules {
        let module = entry.module_id;
        let hash = if entry.active_code_hash.is_empty() {
            None
        } else {
            Some(hash_of(&module, "active", &entry.active_code_hash)?)
        };
        let pending = match entry.pending {
            None => None,
            Some(swap) => Some(Scheduled {
                hash: hash_of(&module, "pending", &swap.code_hash)?,
                activation_height: swap.activation_height,
                ready_at: swap.ready_at,
            }),
        };
        let entry = Entry {
            kind: entry.kind,
            hash,
            pending,
        };
        if entries.insert(module.clone(), entry).is_some() {
            return Err(Error::Status(format!(
                "module {module:?} is registered more than once"
            )));
        }
    }
    Ok(entries)
}

fn hash_of(module: &str, which: &str, bytes: &[u8]) -> Result<[u8; 32], Error> {
    bytes.try_into().map_err(|_| {
        Error::Status(format!(
            "module {module:?}: {which} code hash is {} bytes, not 32",
            bytes.len()
        ))
    })
}

/// The entry's active code hash, or `None` for an admission that has not
/// reached its boundary.
pub async fn active_hash(client: &Client, module: &str) -> Result<Option<[u8; 32]>, Error> {
    active_hashes(client)
        .await?
        .remove(module)
        .map(|entry| entry.hash)
        .ok_or_else(|| Error::Status(format!("module {module:?} is not registered")))
}

/// How long the node took over each question a resolve asks it.
#[derive(Default)]
pub struct Asked {
    /// the registry (`module_status`)
    pub status: Duration,
    /// the artifact blob
    pub fetch: Duration,
}

/// The module's view as the frame under `wanted` ships it — the tasted
/// hash — or, with none wanted, as its active deployment does; `asked`
/// takes the time each question of the node took.
pub async fn resolve(
    client: &Client,
    module: &str,
    wanted: Option<[u8; 32]>,
    asked: &mut Asked,
) -> Result<ViewSource, Error> {
    let hash = match wanted {
        Some(hash) => hash,
        None => {
            let started = Instant::now();
            let active = active_hash(client, module).await;
            asked.status = started.elapsed();
            let Some(hash) = active? else {
                return Ok(ViewSource::NotActivated);
            };
            hash
        }
    };
    let started = Instant::now();
    let loaded = frame(client, hash).await;
    asked.fetch = started.elapsed();
    match loaded.map_err(Error::Artifact)?.view.clone() {
        None => Ok(ViewSource::Missing { hash }),
        Some(view) => Ok(ViewSource::Ready {
            hash,
            component: view.component,
            assets: Arc::new(view.assets),
        }),
    }
}

// ---------- the frames held ----------

/// The frames the taste set last walked — every tasteable one and the
/// active one beside it — so a taste, and the way back, is a swap and
/// not a fetch. Content-addressed: a hash names one frame forever, so an
/// entry is never stale, only unneeded; [`retain_frames`] drops those.
fn frames() -> &'static Mutex<HashMap<[u8; 32], Arc<Frame>>> {
    static FRAMES: OnceLock<Mutex<HashMap<[u8; 32], Arc<Frame>>>> = OnceLock::new();
    FRAMES.get_or_init(Mutex::default)
}

/// The verified frame under `hash`: the one held, else fetched off the
/// node and verified — and NOT held: only the taste walk decides what is
/// worth holding.
pub async fn frame(client: &Client, hash: [u8; 32]) -> Result<Arc<Frame>, view_artifact::Error> {
    let held = frames().lock().expect("held frames").get(&hash).cloned();
    if let Some(frame) = held {
        return Ok(frame);
    }
    view_artifact::fetch(client, hash).await.map(Arc::new)
}

/// The verified frame under `hash`, held from now on.
pub async fn hold_frame(
    client: &Client,
    hash: [u8; 32],
) -> Result<Arc<Frame>, view_artifact::Error> {
    let frame = frame(client, hash).await?;
    frames()
        .lock()
        .expect("held frames")
        .insert(hash, frame.clone());
    Ok(frame)
}

/// Drops every held frame but those under `kept`.
pub fn retain_frames(kept: &BTreeSet<[u8; 32]>) {
    frames()
        .lock()
        .expect("held frames")
        .retain(|hash, _| kept.contains(hash));
}

// ---------- the taste set ----------

/// Where a tasteable hash stands in governance: on an open ballot, or
/// scheduled by a passed one for its height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Open,
    Scheduled { activation_height: u64 },
}

/// One `(module, hash)` a member may taste: what an open proposal would
/// install, or a scheduled swap will.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tasteable {
    pub module: String,
    pub hash: [u8; 32],
    /// The open proposal naming it; none for a scheduled swap, which the
    /// registry names on its own.
    pub proposal: Option<String>,
    pub stage: Stage,
}

/// The node's word on where its chain stands, read beside the taste set:
/// the chain the taste preference is keyed by and the height a scheduled
/// swap is judged stale against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    pub id: String,
    pub height: u64,
}

/// The taste set: `{ (module, hash) : open UpdateModule / RegisterModule
/// proposal } ∪ { (module, pending.hash) : a scheduled swap not stale at
/// the height }`, module-ordered then hash-ordered, one row per pair.
/// `entries` is the registry as [`active_hashes`] read it on the same
/// tick.
pub async fn taste_set(
    client: &Client,
    entries: &BTreeMap<String, Entry>,
) -> Result<(Chain, Vec<Tasteable>), Error> {
    let chain = chain(client).await?;
    let proposals = open_proposals(client).await?;
    let mut rows: BTreeMap<(String, [u8; 32]), Tasteable> = BTreeMap::new();
    for (module, entry) in entries {
        let Some(scheduled) = entry.pending else {
            continue;
        };
        if scheduled.stale_at(chain.height) {
            continue;
        }
        rows.insert(
            (module.clone(), scheduled.hash),
            Tasteable {
                module: module.clone(),
                hash: scheduled.hash,
                proposal: None,
                stage: Stage::Scheduled {
                    activation_height: scheduled.activation_height,
                },
            },
        );
    }
    for proposal in proposals {
        // the registry's word outranks the ballot's: a hash scheduled by
        // one passed proposal and still named by another open one is
        // scheduled
        rows.entry((proposal.module.clone(), proposal.hash))
            .or_insert(Tasteable {
                module: proposal.module,
                hash: proposal.hash,
                proposal: Some(proposal.id),
                stage: Stage::Open,
            });
    }
    Ok((chain, rows.into_values().collect()))
}

async fn chain(client: &Client) -> Result<Chain, Error> {
    let status = client
        .status_json()
        .await
        .map_err(|error| Error::Status(format!("status: {error}")))?;
    let id = status["chain_id"].as_str().unwrap_or_default().to_owned();
    let height = status["height"]
        .as_u64()
        .ok_or_else(|| Error::Status("status: no height".to_owned()))?;
    Ok(Chain { id, height })
}

/// A module-code proposal off the governance register: only the fields
/// the taste set reads. The register is the governance view's own to
/// fold; this reader takes the module and hash a code ballot names and
/// nothing else, and reads an action that is not a code ballot as none.
struct CodeProposal {
    id: String,
    module: String,
    hash: [u8; 32],
}

#[derive(Deserialize)]
struct ProposalsReply {
    proposals: Vec<ProposalView>,
}

#[derive(Deserialize)]
struct ProposalView {
    proposal_id: String,
    action: serde_json::Value,
    status: ProposalStatus,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ProposalStatus {
    Open,
    Passed,
    Rejected,
}

async fn open_proposals(client: &Client) -> Result<Vec<CodeProposal>, Error> {
    let reply: ProposalsReply = client
        .query("governance", &serde_json::json!("proposals"))
        .await
        .map_err(|error| Error::Status(format!("governance proposals: {error}")))?;
    let mut open = Vec::new();
    for proposal in reply.proposals {
        let is_open = match proposal.status {
            ProposalStatus::Open => true,
            ProposalStatus::Passed | ProposalStatus::Rejected => false,
        };
        if !is_open {
            continue;
        }
        let Some(code) = code_action(&proposal.action) else {
            continue;
        };
        let hash: Vec<u8> = code["code_hash"]
            .as_array()
            .map(|bytes| {
                bytes
                    .iter()
                    .filter_map(|byte| byte.as_u64().and_then(|byte| u8::try_from(byte).ok()))
                    .collect()
            })
            .unwrap_or_default();
        let module = code["module_id"].as_str().unwrap_or_default().to_owned();
        let hash = hash_of(&module, "proposed", &hash)?;
        open.push(CodeProposal {
            id: proposal.proposal_id,
            module,
            hash,
        });
    }
    Ok(open)
}

/// The payload of an `update_module` / `register_module` action; any other
/// action installs no code.
fn code_action(action: &serde_json::Value) -> Option<&serde_json::Value> {
    action
        .get("update_module")
        .or_else(|| action.get("register_module"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use module_artifact::{Artifact, ModuleArtifact, ViewArtifact};
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// A node that answers `module_status` with `status` and serves
    /// `artifact` under every blob digest asked for — so a registry naming
    /// a hash the bytes do not match is one test away. With `hold`, the
    /// blob is served only once it is notified.
    pub(crate) async fn node(
        status: serde_json::Value,
        artifact: Option<Artifact>,
        hold: Option<Arc<tokio::sync::Notify>>,
    ) -> Client {
        let deployment = FakeDeployment {
            status: Mutex::new(status),
            artifacts: Mutex::new(artifact.into_iter().collect()),
            by_digest: false,
            hold: Mutex::new(hold),
            hold_status: Mutex::new(None),
            held: tokio::sync::Notify::new(),
            queries: Mutex::new(BTreeMap::new()),
            files_lanes: Mutex::new(BTreeMap::new()),
            files: Mutex::new(BTreeMap::new()),
            file_reads: Mutex::new(Vec::new()),
            index_views: Mutex::new(BTreeMap::new()),
            chain: Mutex::new(chain_status(7)),
        };
        fake_node(Arc::new(deployment)).await
    }

    /// What a fake node serves, changeable under a running host: the
    /// registry reply, the artifacts (by their own digest when `by_digest`,
    /// else the first for any digest), and a one-shot hold on the next
    /// blob served.
    pub(crate) struct FakeDeployment {
        pub status: Mutex<serde_json::Value>,
        pub artifacts: Mutex<Vec<Artifact>>,
        pub by_digest: bool,
        /// One-shot holds: the next blob, or status, answer waits on it.
        pub hold: Mutex<Option<Arc<tokio::sync::Notify>>>,
        pub hold_status: Mutex<Option<Arc<tokio::sync::Notify>>>,
        /// Told each time an answer starts waiting on a hold.
        pub held: tokio::sync::Notify,
        /// What a module query answers, by target: the reads a view makes
        /// for itself through the kernel's `rpc.query`. A target not here
        /// answers the registry status, as every query did before views
        /// read the node.
        pub queries: Mutex<BTreeMap<String, serde_json::Value>>,
        /// What a `/v1/files/<lane>` read answers, by lane: the duckfs reads
        /// a view makes for itself through the kernel's `files.get`. A lane
        /// not here is not found.
        pub files_lanes: Mutex<BTreeMap<String, serde_json::Value>>,
        /// Whole duckfs files by path, served byte-ranged on the `read`
        /// lane (`path`, `offset`, `len` → `{b64, eof}`) the way the node
        /// does; a path not here falls through to `files_lanes`.
        pub files: Mutex<BTreeMap<String, Vec<u8>>>,
        /// Every `read`-lane page served out of `files`: `(path, offset,
        /// len)` — the pin that a resumed download asked only for what it
        /// lacked.
        pub file_reads: Mutex<Vec<(String, u64, u64)>>,
        /// What an index-tier read answers, by module then by the query's own
        /// first key — a view reads its register with several shapes down the
        /// one `rpc.view` door, and each shape wants its own reply. A module
        /// or a shape not here is not found.
        pub index_views: Mutex<BTreeMap<String, serde_json::Value>>,
        /// What `/v1/status` answers: the chain id the taste preference is
        /// keyed by and the height a scheduled swap is judged against.
        pub chain: Mutex<serde_json::Value>,
    }

    /// A `/v1/status` document at `height` on the fake chain.
    pub(crate) fn chain_status(height: u64) -> serde_json::Value {
        serde_json::json!({"height": height, "chain_id": "fake-chain", "public_key": "00"})
    }

    impl FakeDeployment {
        pub(crate) fn serving(module: &str, artifact: &Artifact) -> Arc<Self> {
            Arc::new(Self {
                status: Mutex::new(status_naming(module, &artifact.hash())),
                artifacts: Mutex::new(vec![artifact.clone()]),
                by_digest: true,
                hold: Mutex::new(None),
                hold_status: Mutex::new(None),
                held: tokio::sync::Notify::new(),
                queries: Mutex::new(BTreeMap::new()),
                files_lanes: Mutex::new(BTreeMap::new()),
                files: Mutex::new(BTreeMap::new()),
                file_reads: Mutex::new(Vec::new()),
                index_views: Mutex::new(BTreeMap::new()),
                chain: Mutex::new(chain_status(7)),
            })
        }

        /// The duckfs file at `path` is `bytes` from now on.
        pub(crate) fn publish_file(&self, path: &str, bytes: Vec<u8>) {
            self.files.lock().unwrap().insert(path.to_owned(), bytes);
        }

        /// The duckfs file at `path` is gone.
        pub(crate) fn withdraw_file(&self, path: &str) {
            self.files.lock().unwrap().remove(path);
        }

        /// Every `rpc.query` for `target` answers `reply` from now on.
        pub(crate) fn answer_query(&self, target: &str, reply: serde_json::Value) {
            self.queries
                .lock()
                .unwrap()
                .insert(target.to_owned(), reply);
        }

        /// Every `files.get` on `lane` answers `reply` from now on.
        pub(crate) fn answer_files(&self, lane: &str, reply: serde_json::Value) {
            self.files_lanes
                .lock()
                .unwrap()
                .insert(lane.to_owned(), reply);
        }

        /// Every `rpc.view` on `module` answers out of `shapes`: an object
        /// whose keys are the query keys the view asks with.
        pub(crate) fn answer_view(&self, module: &str, shapes: serde_json::Value) {
            self.index_views
                .lock()
                .unwrap()
                .insert(module.to_owned(), shapes);
        }

        /// The registry now names `artifact` as `module`'s active code,
        /// and the blob store has it.
        pub(crate) fn deploy(&self, module: &str, artifact: &Artifact) {
            *self.status.lock().unwrap() = status_naming(module, &artifact.hash());
            self.artifacts.lock().unwrap().push(artifact.clone());
        }

        /// The blob store has `artifact`; the registry says nothing of it —
        /// the bytes a proposal fanned out ahead of its ballot.
        pub(crate) fn stage(&self, artifact: &Artifact) {
            self.artifacts.lock().unwrap().push(artifact.clone());
        }

        /// The registry names `active` as `module`'s active code and
        /// `scheduled` as the swap pending for `activation_height`.
        pub(crate) fn schedule(
            &self,
            module: &str,
            active: &Artifact,
            scheduled: &Artifact,
            activation_height: u64,
        ) {
            let (active, next) = (active.hash(), scheduled.hash());
            *self.status.lock().unwrap() = serde_json::json!({"module_status": {"modules": [
                {"module_id": module, "kind": "module", "active_code_hash": active,
                 "pending": {"name": "next", "activation_height": activation_height,
                             "code_hash": next, "readiness": [], "ready_at": null},
                 "history": [{"height": 7, "code_hash": active}]}
            ]}});
        }

        /// The governance register lists one open code ballot per
        /// `(module, hash)` in `open`, ids `prop-<n>` in order.
        pub(crate) fn propose(&self, open: &[(&str, [u8; 32])]) {
            let proposals: Vec<serde_json::Value> = open
                .iter()
                .enumerate()
                .map(|(index, (module, hash))| {
                    serde_json::json!({
                        "proposal_id": format!("prop-{index}"),
                        "action": { "update_module": {
                            "name": "next", "module_id": module,
                            "activation_lead": 10, "code_hash": hash.to_vec(),
                        }},
                        "proposer": [1, 2, 3], "created_at": 1, "deadline": 4200,
                        "status": "open", "votes": [], "voter_kind": "validator_node",
                        "electorate": [[[1], 1]],
                        "voting_rule": { "threshold": { "required_yes": 1 } }
                    })
                })
                .collect();
            self.answer_query("governance", serde_json::json!({ "proposals": proposals }));
        }
    }

    pub(crate) fn status_naming(module: &str, hash: &[u8]) -> serde_json::Value {
        serde_json::json!({"module_status": {"modules": [
            {"module_id": module, "kind": "module", "active_code_hash": hash, "pending": null,
             "history": [{"height": 7, "code_hash": hash}]}
        ]}})
    }

    pub(crate) async fn fake_node(deployment: Arc<FakeDeployment>) -> Client {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                // Keep connections alive as the real node does. Closing every
                // response would hide clients reusing a parked runtime's socket.
                let deployment = deployment.clone();
                tokio::spawn(async move {
                    loop {
                        let request = read_request(&mut socket).await;
                        if request.is_empty() {
                            break;
                        }
                        let head = String::from_utf8_lossy(&request).into_owned();
                        let route = head.split(' ').nth(1).unwrap_or("").to_owned();
                        let (status_line, body) = if route == "/v1/status" {
                            let chain = deployment.chain.lock().unwrap().clone();
                            ("200 OK", chain.to_string().into_bytes())
                        } else if route == "/v1/query" {
                            let hold = deployment.hold_status.lock().unwrap().take();
                            if let Some(hold) = hold {
                                deployment.held.notify_one();
                                hold.notified().await;
                            }
                            // a view's own read names its module; anything
                            // else is the app's registry read
                            let ask = head
                                .rsplit("\r\n\r\n")
                                .next()
                                .and_then(|body| {
                                    serde_json::from_str::<serde_json::Value>(body).ok()
                                })
                                .unwrap_or_default();
                            let target = ask["target"].as_str().unwrap_or_default();
                            let answered = match target {
                                "files" => files_reply(&deployment, &ask["query"]),
                                _ => deployment.queries.lock().unwrap().get(target).cloned(),
                            };
                            let reply = answered
                                .unwrap_or_else(|| deployment.status.lock().unwrap().clone());
                            ("200 OK", reply.to_string().into_bytes())
                        } else if let Some(digest) = route.strip_prefix("/v1/files/blob/") {
                            let hold = deployment.hold.lock().unwrap().take();
                            if let Some(hold) = hold {
                                deployment.held.notify_one();
                                hold.notified().await;
                            }
                            let artifacts = deployment.artifacts.lock().unwrap();
                            let served = if deployment.by_digest {
                                artifacts.iter().find(|artifact| {
                                    crate::backend::hex_encode(&artifact.hash()) == digest
                                })
                            } else {
                                artifacts.first()
                            };
                            match served {
                                Some(artifact) => ("200 OK", artifact.encode()),
                                None => ("404 Not Found", Vec::new()),
                            }
                        } else if let Some(module) = route
                            .strip_prefix("/v1/index/")
                            .and_then(|rest| rest.strip_suffix("/view"))
                        {
                            // a view's own index-tier read: the route names the
                            // module, the body's first key names the shape
                            let shape = head
                                .rsplit("\r\n\r\n")
                                .next()
                                .and_then(|body| {
                                    serde_json::from_str::<serde_json::Value>(body).ok()
                                })
                                .and_then(|ask| ask.as_object()?.keys().next().cloned());
                            let answered = shape.and_then(|shape| {
                                let views = deployment.index_views.lock().unwrap();
                                views.get(module)?.get(&shape).cloned()
                            });
                            match answered {
                                Some(reply) => ("200 OK", reply.to_string().into_bytes()),
                                None => ("404 Not Found", Vec::new()),
                            }
                        } else {
                            ("404 Not Found", Vec::new())
                        };
                        let response = format!(
                            "HTTP/1.1 {status_line}\r\nConnection: keep-alive\r\nContent-Length: {}\r\n\r\n",
                            body.len()
                        );
                        socket.write_all(response.as_bytes()).await.unwrap();
                        let _ = socket.write_all(&body).await;
                    }
                });
            }
        });
        Client::new(&origin).unwrap()
    }

    /// One HTTP request off the socket, head and body: the body arrives in
    /// its own write as often as not, and a query's target is in it.
    fn files_reply(
        deployment: &FakeDeployment,
        query: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        let (lane, params) = query.as_object()?.iter().next()?;
        let reply = match lane.as_str() {
            "read" => file_page(deployment, params),
            _ => deployment.files_lanes.lock().unwrap().get(lane).cloned(),
        }?;
        let value = match lane.as_str() {
            "history" => reply["snapshots"].clone(),
            _ => reply,
        };
        Some(serde_json::json!({lane:value}))
    }

    fn file_page(
        deployment: &FakeDeployment,
        params: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        let path = params["path"].as_str()?;
        let bytes = deployment.files.lock().unwrap().get(path)?.clone();
        let offset = params["offset"].as_u64().unwrap_or(0);
        let len = params["len"]
            .as_u64()
            .unwrap_or(1024 * 1024)
            .min(1024 * 1024);
        deployment
            .file_reads
            .lock()
            .unwrap()
            .push((path.to_owned(), offset, len));
        let start = (offset as usize).min(bytes.len());
        let end = (start + len as usize).min(bytes.len());
        let page = &bytes[start..end];
        use base64::Engine as _;
        Some(serde_json::json!({
            "b64": base64::engine::general_purpose::STANDARD.encode(page),
            "eof": end == bytes.len(),
        }))
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut chunk = vec![0u8; 4096];
        loop {
            let read = socket.read(&mut chunk).await.unwrap();
            if read == 0 {
                return request;
            }
            request.extend_from_slice(&chunk[..read]);
            let Some(head_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
            let content_length = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::to_owned)
                })
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let complete = request.len() >= head_end + 4 + content_length;
            if complete {
                return request;
            }
        }
    }

    fn status_of(hash: &[u8]) -> serde_json::Value {
        serde_json::json!({"module_status": {"modules": [
            {"module_id": "files", "kind": "module", "active_code_hash": hash, "pending": null,
             "history": [{"height": 7, "code_hash": hash}]},
            {"module_id": "chat", "kind": "module", "active_code_hash": [], "pending": null, "history": []}
        ]}})
    }

    fn with_view() -> Artifact {
        Artifact::Module(ModuleArtifact {
            component: vec![1, 2, 3],
            index: None,
            view: Some(ViewArtifact {
                component: vec![4, 5, 6],
                assets: [("icons/action.svg".to_owned(), b"<svg/>".to_vec())].into(),
            }),
            lanes: Vec::new(),
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_activated_deployment_with_a_view_is_ready() {
        let artifact = with_view();
        let client = node(status_of(&artifact.hash()), Some(artifact.clone()), None).await;
        assert_eq!(
            resolve(&client, "files", None, &mut Asked::default())
                .await
                .unwrap(),
            ViewSource::Ready {
                hash: artifact.hash(),
                component: vec![4, 5, 6],
                assets: Arc::new(artifact.view().unwrap().assets.clone()),
            }
        );
    }

    /// A `Kind::View` entry's frame IS its view: the registry lists it as a
    /// view, the artifact under its hash is the view frame, and the seat
    /// set reads its kind off the same status read.
    #[tokio::test(flavor = "current_thread")]
    async fn a_view_only_entry_is_ready_off_its_own_frame() {
        let artifact = Artifact::View(ViewArtifact {
            component: vec![4, 5, 6],
            assets: [("icons/tab.svg".to_owned(), b"<svg/>".to_vec())].into(),
        });
        let hash = artifact.hash();
        let status = serde_json::json!({"module_status": {"modules": [
            {"module_id": "files", "kind": "module", "active_code_hash": [], "pending": null, "history": []},
            {"module_id": "home", "kind": "view", "active_code_hash": hash, "pending": null,
             "history": [{"height": 0, "code_hash": hash}]}
        ]}});
        let client = node(status, Some(artifact.clone()), None).await;
        let entries = active_hashes(&client).await.unwrap();
        assert_eq!(
            entries["home"],
            Entry {
                kind: Kind::View,
                hash: Some(hash),
                pending: None,
            }
        );
        assert_eq!(
            entries["files"],
            Entry {
                kind: Kind::Module,
                hash: None,
                pending: None,
            }
        );
        assert_eq!(
            resolve(&client, "home", None, &mut Asked::default())
                .await
                .unwrap(),
            ViewSource::Ready {
                hash,
                component: vec![4, 5, 6],
                assets: Arc::new(artifact.view().unwrap().assets.clone()),
            }
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_verified_deployment_without_a_view_is_missing_not_a_fallback() {
        let artifact = Artifact::module(vec![1, 2, 3]);
        let client = node(status_of(&artifact.hash()), Some(artifact.clone()), None).await;
        assert_eq!(
            resolve(&client, "files", None, &mut Asked::default())
                .await
                .unwrap(),
            ViewSource::Missing {
                hash: artifact.hash()
            }
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bytes_that_do_not_hash_to_the_active_code_fail() {
        let client = node(status_of(&[7; 32]), Some(with_view()), None).await;
        assert!(matches!(
            resolve(&client, "files", None, &mut Asked::default()).await,
            Err(Error::Artifact(view_artifact::Error::HashMismatch))
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_fetch_that_fails_is_an_error() {
        let client = node(status_of(&[7; 32]), None, None).await;
        assert!(matches!(
            resolve(&client, "files", None, &mut Asked::default()).await,
            Err(Error::Artifact(view_artifact::Error::NotHeld))
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_admission_before_its_boundary_is_not_activated() {
        let client = node(status_of(&[7; 32]), Some(with_view()), None).await;
        assert_eq!(
            resolve(&client, "chat", None, &mut Asked::default())
                .await
                .unwrap(),
            ViewSource::NotActivated
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_registry_the_reader_does_not_understand_is_an_error() {
        for (case, status) in [
            ("wrong-length hash", status_of(&[7u8; 31])),
            (
                "unknown field",
                serde_json::json!({"module_status": {"modules": [
                    {"module_id": "files", "kind": "module", "active_code_hash": vec![7u8; 32], "pending": null,
                     "history": [], "extra": 1}
                ]}}),
            ),
            (
                "missing kind",
                serde_json::json!({"module_status": {"modules": [
                    {"module_id": "files", "active_code_hash": vec![7u8; 32], "pending": null,
                     "history": []}
                ]}}),
            ),
            (
                "unknown kind",
                serde_json::json!({"module_status": {"modules": [
                    {"module_id": "files", "kind": "surface", "active_code_hash": vec![7u8; 32],
                     "pending": null, "history": []}
                ]}}),
            ),
            (
                "another reply",
                serde_json::json!({"armed_at": {"swaps": []}}),
            ),
            (
                "registered twice",
                serde_json::json!({"module_status": {"modules": [
                    {"module_id": "files", "kind": "module", "active_code_hash": vec![7u8; 32], "pending": null, "history": []},
                    {"module_id": "files", "kind": "module", "active_code_hash": vec![8u8; 32], "pending": null, "history": []}
                ]}}),
            ),
            (
                "not registered",
                serde_json::json!({"module_status": {"modules": []}}),
            ),
        ] {
            let client = node(status, Some(with_view()), None).await;
            assert!(
                matches!(
                    resolve(&client, "files", None, &mut Asked::default()).await,
                    Err(Error::Status(_))
                ),
                "{case}"
            );
        }
    }

    /// The taste set is every `(module, hash)` an open code ballot names
    /// plus every scheduled swap not stale at the height: a passed or
    /// rejected ballot names nothing, a swap past its height that never
    /// latched names nothing, and the registry's word outranks the
    /// ballot's for a hash both name.
    #[tokio::test(flavor = "current_thread")]
    async fn the_taste_set_is_open_code_ballots_and_live_scheduled_swaps() {
        let active = with_view();
        let next = Artifact::module(vec![9, 9, 9]);
        let (active_hash, next_hash) = (active.hash(), next.hash());
        let node = FakeDeployment::serving("chat", &active);
        let client = fake_node(node.clone()).await;
        let entries = active_hashes(&client).await.unwrap();
        assert_eq!(entries["chat"].pending, None);
        // nothing open, nothing scheduled
        node.answer_query("governance", serde_json::json!({ "proposals": [] }));
        let (chain, rows) = taste_set(&client, &entries).await.unwrap();
        assert_eq!(
            chain,
            Chain {
                id: "fake-chain".into(),
                height: 7
            }
        );
        assert!(rows.is_empty());
        // an open update and a register, a passed one, a rejected one,
        // and a ballot that installs no code
        let ballot = |id: &str, action: serde_json::Value, status: &str| {
            serde_json::json!({
                "proposal_id": id, "action": action,
                "proposer": [1], "created_at": 1, "deadline": 4200, "status": status,
                "votes": [], "voter_kind": "validator_node", "electorate": [[[1], 1]],
                "voting_rule": { "threshold": { "required_yes": 1 } }
            })
        };
        let update = |module: &str, hash: &[u8; 32]| {
            serde_json::json!({ "update_module": {
                "name": "n", "module_id": module, "activation_lead": 10, "code_hash": hash.to_vec()
            }})
        };
        node.answer_query(
            "governance",
            serde_json::json!({ "proposals": [
                ballot("prop-chat", update("chat", &next_hash), "open"),
                ballot("prop-home", serde_json::json!({ "register_module": {
                    "name": "n", "module_id": "home", "kind": "view",
                    "activation_lead": 10, "code_hash": vec![5u8; 32]
                }}), "open"),
                ballot("prop-passed", update("chat", &[6u8; 32]), "passed"),
                ballot("prop-rejected", update("chat", &[7u8; 32]), "rejected"),
                ballot("prop-signal", serde_json::json!({ "signal": { "text": "hi" } }), "open"),
            ]}),
        );
        let (_, rows) = taste_set(&client, &entries).await.unwrap();
        assert_eq!(
            rows,
            [
                Tasteable {
                    module: "chat".into(),
                    hash: next_hash,
                    proposal: Some("prop-chat".into()),
                    stage: Stage::Open,
                },
                Tasteable {
                    module: "home".into(),
                    hash: [5; 32],
                    proposal: Some("prop-home".into()),
                    stage: Stage::Open,
                },
            ]
        );
        // the ballot passed and the swap is scheduled: the registry's word,
        // even while the (settled) ballot still lists the hash as open
        node.schedule("chat", &active, &next, 40);
        let entries = active_hashes(&client).await.unwrap();
        assert_eq!(
            entries["chat"],
            Entry {
                kind: Kind::Module,
                hash: Some(active_hash),
                pending: Some(Scheduled {
                    hash: next_hash,
                    activation_height: 40,
                    ready_at: None,
                }),
            }
        );
        let (_, rows) = taste_set(&client, &entries).await.unwrap();
        assert_eq!(
            rows[0].stage,
            Stage::Scheduled {
                activation_height: 40
            }
        );
        assert_eq!(rows[0].proposal, None);
        assert_eq!(rows.len(), 2);
        // past its height and never latched: stale, and nobody's to taste
        *node.chain.lock().unwrap() = chain_status(40);
        node.answer_query("governance", serde_json::json!({ "proposals": [] }));
        let (chain, rows) = taste_set(&client, &entries).await.unwrap();
        assert_eq!(chain.height, 40);
        assert!(rows.is_empty(), "{rows:?}");
        // a proposed hash that is not 32 bytes is a register this reader
        // does not understand
        node.answer_query(
            "governance",
            serde_json::json!({ "proposals": [ballot("short", update("chat", &[1u8; 32]), "open")] }),
        );
        let mut short = node.queries.lock().unwrap()["governance"].clone();
        short["proposals"][0]["action"]["update_module"]["code_hash"] = serde_json::json!([1, 2]);
        node.answer_query("governance", short);
        assert!(matches!(
            taste_set(&client, &entries).await,
            Err(Error::Status(_))
        ));
    }

    /// A frame held is served without a fetch; one not held is fetched
    /// and, held, kept until the taste walk retains without it.
    #[tokio::test(flavor = "current_thread")]
    async fn a_held_frame_is_a_swap_not_a_fetch() {
        let artifact = with_view();
        let hash = artifact.hash();
        let node = FakeDeployment::serving("chat", &artifact);
        let client = fake_node(node.clone()).await;
        let held = hold_frame(&client, hash).await.unwrap();
        assert_eq!(held.kind, Kind::Module);
        assert!(held.view.is_some());
        // the node forgets the bytes: the held frame still answers
        node.artifacts.lock().unwrap().clear();
        assert_eq!(frame(&client, hash).await.unwrap(), held);
        retain_frames(&BTreeSet::new());
        assert!(matches!(
            frame(&client, hash).await,
            Err(view_artifact::Error::NotHeld)
        ));
    }
}
