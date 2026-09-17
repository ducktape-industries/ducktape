//! the committed data-plane lane table, as this node's planes read it.
//!
//! A lane's id is consensus state (`modules`' `ModulesQuery::Lanes`), and it
//! decides two overlay ports, so a plane cannot bind until the node can read
//! the table. That read is LATE: a validator has a host before its planes
//! spawn, but a resident awaiting redemption has no committed state at all
//! while its presence and gateway planes are already coming up. So the table is
//! a watch, not a boot constant — the node fills it from the registry
//! whenever the committed table changes, and every plane waits for its own
//! lane exactly as it already waits for its overlay `/128`.
//!
//! An absent key is a WAIT, never a default: a plane that guessed a port
//! would bind a socket some other lane owns on the nodes that did read the
//! table. It says which key it is waiting for, on the forever-retry cadence.

use data_plane::{BulkPacer, PlaneConfig, Service, StreamPacing, StreamPolicy};

use crate::overlay_book::{LaneKey, LaneSource};

/// what a plane needs out of the table to bind: the id its ports derive from,
/// and the stream half's shape when the lane declares one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneBinding {
    pub service: Service,
    pub stream: Option<modules::LaneStream>,
}

impl LaneBinding {
    /// the stream half as the plane binder takes it, or `None` for a lane
    /// that declared none.
    ///
    /// The pacing choice is the DECLARATION's, not the caller's: a lane that
    /// says `Shared` joins this process's one link budget, and one that names
    /// its own rate gets exactly that. A caller passing a local config to a
    /// shared-paced plane used to have it silently ignored — here the lane
    /// decides and there is nothing to ignore.
    pub fn stream_spec(&self, shared: &BulkPacer) -> Option<(StreamPacing, StreamPolicy)> {
        let stream = self.stream.as_ref()?;
        let pacing = match stream.pacing {
            modules::LanePacing::Shared => StreamPacing::Shared(shared.clone()),
            modules::LanePacing::Local {
                bulk_bytes_per_sec,
                bulk_burst_bytes,
            } => StreamPacing::Local(PlaneConfig {
                bulk_bytes_per_sec,
                bulk_burst_bytes,
            }),
        };
        let policy = StreamPolicy {
            accept_backlog: stream.accept_backlog as usize,
        };
        Some((pacing, policy))
    }
}

/// the forever-wait voice: attempt 1, then every Nth. The counter IS the
/// diagnosis — a line per poll would evict the ring it is evidence in.
const WAIT_REPORT_EVERY: u64 = 20;

/// THE lane table of this process — one per node, like the reachability
/// plane's execution watch beside it.
///
/// A singleton rather than a threaded-through handle because the readers are
/// plane bring-ups scattered across the validator, the replica park and the
/// promotion seat, and the one writer is a pump on the drain. Threading a
/// handle through all of them would add a parameter to a dozen functions to
/// say what "the committed lane table" already says once.
pub(crate) fn lane_table() -> &'static LaneTable {
    static TABLE: std::sync::OnceLock<LaneTable> = std::sync::OnceLock::new();
    TABLE.get_or_init(LaneTable::new)
}

/// the node's live view of the committed lane table. Cloneable handle; the
/// registry pump `publish`es, every plane `resolve`s.
#[derive(Clone)]
pub struct LaneTable {
    lanes: tokio::sync::watch::Sender<Vec<modules::LaneRecord>>,
}

impl Default for LaneTable {
    fn default() -> Self {
        Self::new()
    }
}

impl LaneTable {
    pub fn new() -> Self {
        Self {
            lanes: tokio::sync::watch::Sender::new(Vec::new()),
        }
    }

    /// install the committed table. Sends only on a CHANGE, so a plane
    /// waiting on the watch is not woken by every block that left the table
    /// alone (the pump runs per tick).
    pub fn publish(&self, lanes: Vec<modules::LaneRecord>) {
        self.lanes.send_if_modified(|current| {
            let changed = *current != lanes;
            if changed {
                *current = lanes;
            }
            changed
        });
    }

    /// the lane `key` names, if the table already answers for it. Tests
    /// only: production always WAITS, because a plane that read the table
    /// once and found nothing would bind nothing forever.
    #[cfg(test)]
    pub fn get(&self, key: LaneKey) -> Option<LaneBinding> {
        binding_for(&self.lanes.borrow(), key)
    }

    /// the lane a plane serves. A kernel lane answers at once — its id is in
    /// the binary. A declared one WAITS until the committed table names it;
    /// a plane calls this before it binds a single socket.
    pub async fn resolve(&self, source: LaneSource, node: &str) -> LaneBinding {
        match source {
            LaneSource::Kernel(service) => LaneBinding {
                service,
                stream: None,
            },
            LaneSource::Declared(key) => self.await_declared(key, node).await,
        }
    }

    /// WAIT until the committed table stops naming `bound` for `source` —
    /// the lane was withdrawn, or renumbered onto another id.
    ///
    /// A plane selects on this beside its serve loop, because its sockets are
    /// bound to ports DERIVED from the id it resolved. Once the network says
    /// that id is no longer this lane's, the ports are no longer this plane's
    /// either: another module may declare the id, bind the same ports on its
    /// nodes, and a plane that kept serving would answer for a lane it does
    /// not own. Losing the select tears the plane down, which closes the
    /// sockets — the only way to release a port this process holds.
    ///
    /// A kernel lane never changes (its id is in the binary), so this waits
    /// forever for one, which is exactly right in a `select!`.
    pub async fn await_lane_change(&self, source: LaneSource, bound: Service) {
        let LaneSource::Declared(key) = source else {
            std::future::pending::<()>().await;
            return;
        };
        let mut watch = self.lanes.subscribe();
        loop {
            let still_ours = binding_for(&watch.borrow_and_update(), key)
                .is_some_and(|binding| binding.service == bound);
            if !still_ours {
                return;
            }
            if watch.changed().await.is_err() {
                // shutting down: the table will not change again, and the
                // plane is going away with the process regardless.
                std::future::pending::<()>().await;
            }
        }
    }

    async fn await_declared(&self, key: LaneKey, node: &str) -> LaneBinding {
        let mut watch = self.lanes.subscribe();
        let mut attempts: u64 = 0;
        loop {
            if let Some(binding) = binding_for(&watch.borrow_and_update(), key) {
                return binding;
            }
            attempts += 1;
            report_wait(node, key, attempts);
            if watch.changed().await.is_err() {
                // the node is shutting down: the sender is gone, so no table
                // will ever arrive. Park instead of spinning on a dead watch.
                std::future::pending::<()>().await;
            }
        }
    }
}

/// run `serving` until the committed table stops naming `bound` for this
/// plane's lane, then stop.
///
/// Stopping IS the handling: dropping `serving` drops the plane, which closes
/// the sockets bound to that id's ports — a plane cannot keep a port it no
/// longer owns. The node then has no plane for that lane, which is correct:
/// the network says the lane is gone. A lane that comes back brings its plane
/// back on the next node start, and a KERNEL lane never changes, so this is
/// just the serve loop for those.
pub(crate) async fn serve_until_lane_changes(
    source: LaneSource,
    bound: Service,
    node: &str,
    serving: impl std::future::Future<Output = ()>,
) {
    tokio::select! {
        () = serving => {}
        () = lane_table().await_lane_change(source, bound) => {
            tracing::warn!(
                target: "ducktape::dataplane",
                node = %node,
                reason = "lane_withdrawn",
                lane = %source,
                was = bound.lane_id(),
                "plane stopped: the committed lane table no longer names this lane at this id"
            );
        }
    }
}

/// re-read the committed lane table into [`lane_table`].
///
/// Called wherever a node has just folded blocks, because that is the only
/// thing that can move the table — and it is a no-op when the table did not
/// move, so a plane is woken by a DECLARATION, never by a tick. A registry
/// that will not answer (a baseline net with no `modules` module) leaves the
/// table as it was: the planes keep waiting and say which lane they want.
pub(crate) async fn pump(host: &host::Host) {
    let req = modules::encode_query(&modules::ModulesQuery::Lanes);
    let Ok(bytes) = host.query(host::MODULES_ID, &req).await else {
        return;
    };
    let Ok(modules::ModulesReply::Lanes { lanes }) = modules::decode_reply(&bytes) else {
        return;
    };
    lane_table().publish(lanes);
}

fn binding_for(table: &[modules::LaneRecord], key: LaneKey) -> Option<LaneBinding> {
    let record = table
        .iter()
        .find(|lane| lane.module_id == key.module_id && lane.name == key.name)?;
    Some(LaneBinding {
        service: Service::from_lane_id(record.id),
        stream: record.stream.clone(),
    })
}

fn report_wait(node: &str, key: LaneKey, attempts: u64) {
    let due = attempts == 1 || attempts.is_multiple_of(WAIT_REPORT_EVERY);
    if !due {
        return;
    }
    // the KEY is the whole diagnosis: an operator reading this knows whether
    // the module is missing, the declaration never landed, or the name drifted.
    // same plane, same target as the socket-bind retry beside it: an
    // operator turning up `ducktape::dataplane` sees BOTH reasons a plane is
    // not up, in one filter.
    tracing::warn!(
        target: "ducktape::dataplane",
        node = %node,
        reason = "lane_undeclared",
        lane = %key,
        attempts,
        "plane unbound: the committed lane table names no such lane"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const VOICE: LaneKey = LaneKey {
        module_id: "chat",
        name: "voice",
    };

    fn record(id: u8, module_id: &str, name: &str) -> modules::LaneRecord {
        modules::LaneRecord {
            id,
            module_id: module_id.into(),
            name: name.into(),
            stream: None,
        }
    }

    #[test]
    fn a_key_resolves_by_module_and_name_never_by_id_or_position() {
        let table = LaneTable::new();
        // chat's two lanes are distinguishable ONLY by name — the whole point
        // of the key. Declared back-to-front to catch a positional lookup.
        table.publish(vec![
            record(3, "chat", "video"),
            record(2, "chat", "voice"),
            record(2, "agent", "voice"),
        ]);
        assert_eq!(table.get(VOICE).unwrap().service.lane_id(), 2);
        assert_eq!(
            table
                .get(LaneKey {
                    module_id: "chat",
                    name: "video"
                })
                .unwrap()
                .service
                .lane_id(),
            3
        );
        // another module's identically named lane is a different lane.
        assert!(
            table
                .get(LaneKey {
                    module_id: "gateway",
                    name: "voice"
                })
                .is_none()
        );
    }

    #[test]
    fn an_undeclared_key_resolves_to_nothing_rather_than_a_default() {
        let table = LaneTable::new();
        assert!(table.get(VOICE).is_none(), "an empty table binds nothing");
        table.publish(vec![record(4, "gateway", "gateway")]);
        assert!(
            table.get(VOICE).is_none(),
            "a table that names other lanes still binds nothing for this key"
        );
    }

    #[tokio::test]
    async fn a_withdrawn_or_renumbered_lane_releases_the_plane_that_bound_it() {
        // the sockets sit on ports derived from the id, so "this id is no
        // longer yours" and "this lane is gone" have to mean the same thing
        // to a running plane: both end it. Either way the ports go back.
        for table_after in [
            vec![record(9, "agent", "telemetry")], // withdrawn
            vec![record(7, "chat", "voice")],      // renumbered
        ] {
            let table = LaneTable::new();
            table.publish(vec![record(2, "chat", "voice")]);
            let bound = table.get(VOICE).unwrap().service;
            let watcher = {
                let table = table.clone();
                tokio::spawn(async move {
                    table
                        .await_lane_change(LaneSource::Declared(VOICE), bound)
                        .await;
                })
            };
            // a table that still names it at the same id leaves the plane alone
            table.publish(vec![
                record(2, "chat", "voice"),
                record(4, "gateway", "gateway"),
            ]);
            assert!(
                !watcher.is_finished(),
                "an unrelated declaration ended a plane"
            );
            table.publish(table_after);
            watcher.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_kernel_lane_never_changes_so_its_plane_is_never_released() {
        // its id is in the binary and the registry refuses to declare it, so
        // there is no table edit that could take it away.
        let table = LaneTable::new();
        let kernel = LaneSource::Kernel(Service::from_lane_id(1));
        let watcher = {
            let table = table.clone();
            tokio::spawn(async move {
                table
                    .await_lane_change(kernel, Service::from_lane_id(1))
                    .await;
            })
        };
        table.publish(vec![record(1, "impostor", "statesync")]);
        tokio::task::yield_now().await;
        assert!(
            !watcher.is_finished(),
            "a kernel plane must not be releasable"
        );
        watcher.abort();
    }

    #[tokio::test]
    async fn resolve_returns_as_soon_as_the_lane_is_declared() {
        let table = LaneTable::new();
        let waiter = {
            let table = table.clone();
            tokio::spawn(async move { table.resolve(LaneSource::Declared(VOICE), "node").await })
        };
        // a table that does not name the lane leaves the waiter waiting; the
        // one that does releases it. No sleep: the watch IS the event.
        table.publish(vec![record(9, "agent", "telemetry")]);
        table.publish(vec![
            record(9, "agent", "telemetry"),
            record(2, "chat", "voice"),
        ]);
        let binding = waiter.await.unwrap();
        assert_eq!(binding.service.lane_id(), 2);
        assert_eq!(binding.service.overlay_datagram_port(), 45902);
    }
}
