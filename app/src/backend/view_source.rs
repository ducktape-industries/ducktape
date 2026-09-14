//! Where a network view comes from: the deployed artifact of the registry
//! entry it belongs to — a module's, whose frame may embed a view, or a
//! view-only entry's, whose frame IS the view. The registry names the
//! entry's kind and its ACTIVE code hash (`ModulesQuery::ModuleStatus`), the
//! artifact under that hash is fetched and verified (`view_artifact`), and
//! its view — component bytes plus the assets shipped beside them — is
//! handed over as one unit. A pending swap is never read: the view drawn is
//! the view of the code that runs.
//!
//! Every reading here is strict. A registry reply of another shape, a module
//! the registry does not list or lists twice, a hash that is not 32 bytes,
//! a fetch that fails or a body that does not hash to what was asked for is
//! an [`Error`], never a fallback to a desktop resource. The only quiet
//! outcomes are the two the network itself asserts: a module admitted but
//! not yet activated ([`ViewSource::NotActivated`]) and a verified
//! deployment that ships no view ([`ViewSource::Missing`]).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ducktape_rpc::Client;
use serde::Deserialize;

use super::view_artifact;

/// The BUILT-IN tabs whose view is drawn by the module's own artifact: each
/// has a `ShellTab` arm, a props builder and an intent decoder of its own,
/// and is asked of the connected node at connect and again at every block
/// that moves its deployment. Every other view off the node is a
/// registry-listed `Kind::View` entry, seated from `module_status` alone.
pub const MODULE_OWNED: [&str; 5] = ["governance", "files", "pages", "chat", "forge"];

/// The desktop's own views, staged beside the binary and asked for at boot.
/// Every view that is not one of these comes off the connected node.
pub const DESKTOP_OWNED: [&str; 5] = ["members", "agents", "node", "explorer", "settings"];

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
    #[allow(
        dead_code,
        reason = "read for shape only: a pending swap is never loaded"
    )]
    pending: Option<ScheduledSwap>,
    #[allow(dead_code, reason = "read for shape only")]
    history: Vec<Activation>,
}

/// What the entry's artifact is: a module (whose frame may embed a view) or
/// a view alone — the registry's word, fixed at admission.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Module,
    View,
}

/// One registry entry as the seat set reads it: what it is, and its active
/// code hash — `None` for an admission that has not reached its boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub hash: Option<[u8; 32]>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code, reason = "read for shape only")]
struct ScheduledSwap {
    name: String,
    activation_height: u64,
    code_hash: Vec<u8>,
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
            Some(entry.active_code_hash.as_slice().try_into().map_err(|_| {
                Error::Status(format!(
                    "module {module:?}: active code hash is {} bytes, not 32",
                    entry.active_code_hash.len()
                ))
            })?)
        };
        let entry = Entry {
            kind: entry.kind,
            hash,
        };
        if entries.insert(module.clone(), entry).is_some() {
            return Err(Error::Status(format!(
                "module {module:?} is registered more than once"
            )));
        }
    }
    Ok(entries)
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

/// The module's view as its active deployment ships it; `asked` takes the
/// time each question of the node took.
pub async fn resolve(
    client: &Client,
    module: &str,
    asked: &mut Asked,
) -> Result<ViewSource, Error> {
    let started = Instant::now();
    let active = active_hash(client, module).await;
    asked.status = started.elapsed();
    let Some(hash) = active? else {
        return Ok(ViewSource::NotActivated);
    };
    let started = Instant::now();
    let loaded = view_artifact::load(client, hash).await;
    asked.fetch = started.elapsed();
    match loaded.map_err(Error::Artifact)? {
        None => Ok(ViewSource::Missing { hash }),
        Some(view) => Ok(ViewSource::Ready {
            hash,
            component: view.component,
            assets: Arc::new(view.assets),
        }),
    }
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
            index_views: Mutex::new(BTreeMap::new()),
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
        /// What an index-tier read answers, by module then by the query's own
        /// first key — a view reads its register with several shapes down the
        /// one `rpc.view` door, and each shape wants its own reply. A module
        /// or a shape not here is not found.
        pub index_views: Mutex<BTreeMap<String, serde_json::Value>>,
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
                index_views: Mutex::new(BTreeMap::new()),
            })
        }

        /// Every `rpc.query` for `target` answers `reply` from now on.
        pub(crate) fn answer_query(&self, target: &str, reply: serde_json::Value) {
            self.queries.lock().unwrap().insert(target.to_owned(), reply);
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
                // each request on its own: a held blob leaves the status
                // answering, as a node does
                let deployment = deployment.clone();
                tokio::spawn(async move {
                    let request = read_request(&mut socket).await;
                    let head = String::from_utf8_lossy(&request).into_owned();
                    let route = head.split(' ').nth(1).unwrap_or("").to_owned();
                    let (status_line, body) = if route == "/v1/query" {
                        let hold = deployment.hold_status.lock().unwrap().take();
                        if let Some(hold) = hold {
                            deployment.held.notify_one();
                            hold.notified().await;
                        }
                        // a view's own read names its module; anything
                        // else is the app's registry read
                        let target = head
                            .rsplit("\r\n\r\n")
                            .next()
                            .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok())
                            .and_then(|ask| ask["target"].as_str().map(str::to_owned));
                        let answered = target
                            .and_then(|target| deployment.queries.lock().unwrap().get(&target).cloned());
                        let reply = answered.unwrap_or_else(|| deployment.status.lock().unwrap().clone());
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
                            .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok())
                            .and_then(|ask| {
                                ask.as_object()?.keys().next().cloned()
                            });
                        let answered = shape.and_then(|shape| {
                            let views = deployment.index_views.lock().unwrap();
                            views.get(module)?.get(&shape).cloned()
                        });
                        match answered {
                            Some(reply) => ("200 OK", reply.to_string().into_bytes()),
                            None => ("404 Not Found", Vec::new()),
                        }
                    } else if let Some(lane) = route.strip_prefix("/v1/files/") {
                        // a view's own duckfs read: the lane names it, the
                        // query string carries its params
                        let lane = lane.split('?').next().unwrap_or_default();
                        let answered = deployment.files_lanes.lock().unwrap().get(lane).cloned();
                        match answered {
                            Some(reply) => ("200 OK", reply.to_string().into_bytes()),
                            None => ("404 Not Found", Vec::new()),
                        }
                    } else {
                        ("404 Not Found", Vec::new())
                    };
                    let response = format!(
                        "HTTP/1.1 {status_line}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Client::new(&origin).unwrap()
    }

    /// One HTTP request off the socket, head and body: the body arrives in
    /// its own write as often as not, and a query's target is in it.
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
                .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(str::to_owned))
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
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_activated_deployment_with_a_view_is_ready() {
        let artifact = with_view();
        let client = node(status_of(&artifact.hash()), Some(artifact.clone()), None).await;
        assert_eq!(
            resolve(&client, "files", &mut Asked::default())
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
                hash: Some(hash)
            }
        );
        assert_eq!(
            entries["files"],
            Entry {
                kind: Kind::Module,
                hash: None
            }
        );
        assert_eq!(
            resolve(&client, "home", &mut Asked::default())
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
            resolve(&client, "files", &mut Asked::default())
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
            resolve(&client, "files", &mut Asked::default()).await,
            Err(Error::Artifact(view_artifact::Error::HashMismatch))
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_fetch_that_fails_is_an_error() {
        let client = node(status_of(&[7; 32]), None, None).await;
        assert!(matches!(
            resolve(&client, "files", &mut Asked::default()).await,
            Err(Error::Artifact(view_artifact::Error::Transport(_)))
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_admission_before_its_boundary_is_not_activated() {
        let client = node(status_of(&[7; 32]), Some(with_view()), None).await;
        assert_eq!(
            resolve(&client, "chat", &mut Asked::default())
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
                    resolve(&client, "files", &mut Asked::default()).await,
                    Err(Error::Status(_))
                ),
                "{case}"
            );
        }
    }
}
