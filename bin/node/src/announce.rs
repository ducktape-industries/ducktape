//! The capability announce: what this node tells the network it can execute.
//!
//! Two facts feed it, and they are deliberately NOT the same fact:
//!
//! - **consent** — the operator approved kind K on this node (`services.toml`,
//!   written by `service enable`/`disable`). Durable, and it changes only when
//!   a human runs a verb.
//! - **liveness** — K's daemon is signaling right now (the volatile
//!   [`noded::services::ServiceCatalog`], 30 s TTL). Ephemeral, and known only
//!   to this node.
//!
//! The announce is their intersection, so it has two triggers and ONE writer:
//!
//! - the **verb** submits on a consent change, synchronously, and reports the
//!   commit height to the human who ran it ([`crate::services::enable`]);
//! - the **watcher** below submits on a liveness transition ([`spawn`]).
//!
//! Both go through the SAME door — `POST /v1/submit`, which frames the op with
//! the node's own key and answers only once consensus has settled. Nothing here
//! submits through `OrderedNode::submit`, and that is the whole design: that
//! call is fire-and-forget by construction and lives inside the drain loop that
//! produces the very commits it would have to await, which is why the pump this
//! module replaces needed a frame latch, a two-tier give-up budget and an
//! outcome route back from the drain. A blocking POST from outside the loop
//! needs none of them.
//!
//! No daemon ever submits an announce: N daemons each doing a declarative
//! replace of one shared tag set would clobber each other. One writer, always
//! the node.

use std::collections::{BTreeMap, BTreeSet};

use crate::services::ServiceGrant;

/// what one announce carries.
///
/// The pair travels as one value because the module couples them: resources
/// without tags is a consensus-level reject, so they must never be decided in
/// two places that could disagree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AnnounceSet {
    pub(crate) capabilities: Vec<String>,
    pub(crate) resources: BTreeMap<String, u64>,
}

/// why a set cannot be announced.
///
/// A closed domain that belongs to the consent boundary — it is what
/// `plan_enable` refuses, so that an operator is never asked to approve a
/// consent screen listing tags this node will never announce.
///
/// It is unreachable from the watcher, because it is a property of the FILE,
/// not of whichever code path wrote it. `Services::validate` runs this very
/// function over [`widest`] on every `load`, so no `services.toml` this node
/// will read can carry an illegal tag. Every live derivation is a subset of
/// that set.
///
/// The consequence is deliberate and worth knowing before it surprises someone:
/// an ill-tagged file fails the node's BOOT, not merely its announce. That is
/// the louder half of the trade. The state it replaces is the one this
/// campaign has hit repeatedly — a node that boots, looks healthy, and
/// silently announces nothing behind a warn throttled to one line per five
/// minutes.
///
/// So if it ever fires on the watcher path, it is not a set to trim here — it
/// means `validate` was bypassed, and the fix is upstream.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// tags the registry's own grammar rejects. The hello boundary's item rule
    /// is LOOSER than `capability::validate_tag` (it admits any printable ascii
    /// plus a space), so a third-party daemon or an operator spec dir can
    /// signal `"Claude Sonnet"` and reach the consent screen intact.
    IllegalTags(Vec<String>),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::IllegalTags(tags) => write!(
                f,
                "the capability registry refuses {} (a tag is 1..64 bytes of [a-z0-9._-]) — \
                 fix the daemon's capability spec, then signal again",
                tags.join(", ")
            ),
        }
    }
}

/// THE decision: what this node may truthfully announce, given what the operator
/// consented to and what is signaling right now.
///
/// For every granted kind whose daemon is live, the kind's own tag rides beside
/// the grant's executor tags intersected with what that SAME kind's hello offers
/// — so neither side can widen the other. A grant with no live daemon
/// contributes nothing; a live daemon with no grant contributes nothing.
///
/// The kind tag rides even when the executor intersection is empty: a daemon
/// that spawns nothing (an airlock plug) still IS that kind, and that is
/// precisely what placement asks about.
///
/// PURE — no clock, no disk, no logging, no `self`. Both triggers call it, which
/// is what keeps the verb's consent screen and the watcher's op describing the
/// same node.
pub(crate) fn announced_set(
    grants: &[ServiceGrant],
    signaling: &[noded::services::Signaling],
    capacity: &BTreeMap<String, u64>,
) -> Result<AnnounceSet, Refusal> {
    let mut tags: BTreeSet<String> = BTreeSet::new();
    for live in signaling {
        // signaling WITHOUT consent offers nothing at all — not its executors
        // and not its kind. Enable is the switch.
        let Some(grant) = grants.iter().find(|grant| grant.kind == live.kind) else {
            continue;
        };
        let offers_now: BTreeSet<&str> = live.capabilities.iter().map(String::as_str).collect();
        tags.insert(live.kind.clone());
        tags.extend(
            grant
                .capabilities
                .iter()
                .filter(|tag| offers_now.contains(tag.as_str()))
                .cloned(),
        );
    }

    // the registry's own rule, applied before anything is submitted. The KIND
    // tags need no separate check: the hello's kind grammar (1..32 of
    // [a-z0-9-]) is a strict subset of the tag rule.
    let illegal: Vec<String> = tags
        .iter()
        .filter(|tag| capability::validate_tag(tag).is_err())
        .cloned()
        .collect();
    if !illegal.is_empty() {
        return Err(Refusal::IllegalTags(illegal));
    }
    // empty tags force empty resources — the module rejects the
    // resources-without-tags shape, so it is never emitted.
    let resources = match tags.is_empty() {
        true => BTreeMap::new(),
        false => capacity.clone(),
    };
    // sorted and deduplicated by construction, and that is load-bearing rather
    // than tidy: the committed registry answers a `BTreeSet` rendered in order,
    // so an unsorted set could never compare equal to it and the watcher would
    // submit forever.
    Ok(AnnounceSet {
        capabilities: tags.into_iter().collect(),
        resources,
    })
}

/// the WIDEST set `grants` could ever announce: every granted kind signaling
/// everything it was granted.
///
/// This is what the consent boundary must bound, because the live derivation is
/// always a subset of it. Bounding the live one instead makes the check
/// order-dependent — enabling `compute` while `agent`'s daemon happens to be
/// down would pass, and the union would cross the cap later when `agent`
/// started, at which point no verb is running to refuse it.
///
/// Expressed as a synthetic signaling set fed through [`announced_set`] rather
/// than as a second union, so there is exactly one definition of what this node
/// announces and the bound cannot drift from the thing it bounds.
pub(crate) fn widest(grants: &[ServiceGrant]) -> Vec<noded::services::Signaling> {
    grants
        .iter()
        .map(|grant| noded::services::Signaling {
            kind: grant.kind.clone(),
            version: String::new(),
            build: String::new(),
            capabilities: grant.capabilities.clone(),
            scopes: Vec::new(),
            needs: Vec::new(),
        })
        .collect()
}

/// Submit one announce through this node's own `/v1/submit` and return the
/// height it committed at.
///
/// The node re-frames the op with ITS key, which is the registry's identity
/// (`capability` takes the announcing node from the verified submit origin,
/// never from payload data). That is why no caller here holds a signing key —
/// and why a user-signed announce could never have worked.
pub(crate) fn submit(
    base: &str,
    workspace: &std::path::Path,
    set: &AnnounceSet,
) -> Result<u64, String> {
    let msg = capability::CapabilityMsg::Announce {
        capabilities: set.capabilities.clone(),
        resources: set.resources.clone(),
    };
    let payload = serde_json::to_value(&msg).map_err(|error| error.to_string())?;
    crate::node_http::submit(base, workspace, "capability", &payload)
        .map_err(|error| error.to_string())
}

/// This node's committed announce.
///
/// TWO `/v1/query` round trips, not one — the registry answers tags and
/// resources through separate query variants. Both cross the node's command
/// lane, so a host busy with a catch-up stage can leave this unanswered for as
/// long as that stage runs; the watcher then reports `Unknown` and waits, which
/// is the safe direction (a read failure can never retract a live set).
fn committed(base: &str, node_key: &[u8]) -> Option<AnnounceSet> {
    use capability::{CapabilityQuery, CapabilityReply};
    let ask = |query: CapabilityQuery| -> Option<CapabilityReply> {
        let value = serde_json::to_value(&query).ok()?;
        serde_json::from_value(crate::node_http::query(base, "capability", value).ok()?).ok()
    };
    let node = node_key.to_vec();
    let CapabilityReply::Node(capabilities) = ask(CapabilityQuery::Node { node: node.clone() })?
    else {
        return None;
    };
    let CapabilityReply::Resources(resources) = ask(CapabilityQuery::Resources { node })? else {
        return None;
    };
    Some(AnnounceSet {
        capabilities,
        resources,
    })
}

/// how often the watcher re-derives the announce.
///
/// THE DAEMON'S OWN BEAT, not a copy of the formula behind it. Sampling faster
/// than daemons report would only re-read a catalog that cannot have changed
/// conclusion, and sampling slower would miss a beat — so this is not "the same
/// number as `HEARTBEAT`", it IS `HEARTBEAT`, and re-deriving it from
/// `HELLO_TTL` here is how the two silently drift apart when one is tuned.
const TICK: std::time::Duration = crate::services::HEARTBEAT;

/// how long after this thread starts before it may submit anything.
///
/// The signaling catalog lives in THIS process, so a node restart does not age
/// it out — it starts EMPTY. A daemon that never stopped running re-registers on
/// its own beat ([`crate::services::HEARTBEAT`]), and its beats fail while the node's `/v1` is
/// down, so its first post-boot hello can land after this thread's first tick.
/// Acting then would read "nothing is signaling" off a catalog that has simply
/// not been filled yet and RETRACT a healthy daemon — pulling a live node out of
/// every placement pool for a tick and costing two consensus writes per node
/// restart.
///
/// One full TTL is the tight bound: every live daemon beats at least three times
/// inside it, so by the time this expires an empty catalog means the daemon is
/// really gone. The cost is that a genuinely stale registry is corrected up to
/// `SETTLE` later at boot, which is fine — nothing places work on a node faster
/// than the operator can start its daemon.
///
/// KNOWN CEILING, named rather than handled: the clock is per-thread, so a node
/// crash-looping faster than `SETTLE` never reaches a tick that may submit, and
/// a stale announce of its stands until it stays up for one full window. That is
/// the right trade — a node that cannot stay up for 30 s has a worse problem
/// than a stale tag, and the alternative (persisting the settle deadline across
/// restarts) would let a fast restart retract a daemon that never stopped, which
/// is the failure this constant exists to prevent.
const SETTLE: std::time::Duration = noded::services::HELLO_TTL;

/// a failure is reported on the FIRST occurrence and every Nth after it,
/// carrying the count. The doctrine's rule for a forever-retry loop: an
/// unconditional `warn!` on a 10 s tick evicts the whole 4096-line ring in
/// half a day and destroys the evidence someone came to read — and the counter
/// IS the diagnosis.
const REPORT_EVERY: u64 = 32;

/// everything the watcher needs. Owned, because it outlives the boot frame it
/// is built in.
pub(crate) struct Watch {
    /// this node's own `/v1` base — the only transport it uses.
    pub(crate) base: String,
    /// this node's consensus public key, to seed from the committed registry.
    pub(crate) node_key: Vec<u8>,
    /// the workspace whose `services.toml` carries the operator's consent.
    pub(crate) workspace: std::path::PathBuf,
    /// the volatile signaling catalog — the live half.
    pub(crate) services: noded::services::ServiceCatalog,
    /// the capacity announced beside the tags: probed host totals from the
    /// `[sandbox]` table, EMPTY on a node that configures no isolation.
    pub(crate) capacity: BTreeMap<String, u64>,
}

/// Start the liveness watcher on its own OS thread.
///
/// A plain thread with a blocking client, exactly like the daemons' own
/// heartbeat: the node's host must never leave the commonware runner thread,
/// and this wants to BLOCK on a settling submit — which is the one thing a task
/// on that thread must not do.
pub(crate) fn spawn(watch: Watch) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("announce-watch".into())
        .spawn(move || run(watch))
        .map(|_| ())
}

/// what one tick concludes. ONE discriminant, so every path the watcher can
/// take is a named value a test can assert on rather than a branch buried in a
/// loop that only runs against a live node.
#[derive(Debug, PartialEq, Eq)]
enum Tick {
    /// inside the settle window: the catalog's emptiness means nothing yet.
    Wait,
    /// the committed registry did not answer, so there is nothing to compare
    /// against. Never a submit — a read failure must not be able to retract a
    /// live set.
    Unknown,
    /// the desired set could not be formed (unreadable grants, a refusal).
    Undecidable(String),
    /// the registry already says what this node offers.
    Quiet,
    /// the registry disagrees; announce this.
    Announce(AnnounceSet),
}

/// THE decision, pure: no clock, no I/O, no logging, no `self`.
///
/// Every rule the watcher has lives here and only here, which is what lets the
/// loop below be a plain executor — and what makes deleting one of these rules
/// fail a test instead of shipping.
fn tick(settled: bool, committed: Option<AnnounceSet>, want: Result<AnnounceSet, String>) -> Tick {
    if !settled {
        return Tick::Wait;
    }
    let Some(committed) = committed else {
        return Tick::Unknown;
    };
    let want = match want {
        Ok(want) => want,
        Err(reason) => return Tick::Undecidable(reason),
    };
    if want == committed {
        return Tick::Quiet;
    }
    Tick::Announce(want)
}

fn run(watch: Watch) {
    let started = std::time::Instant::now();
    let mut failures: u64 = 0;
    tracing::info!(
        target: "ducktape::service",
        settle_secs = SETTLE.as_secs(),
        tick_secs = TICK.as_secs(),
        "announce watcher started; quiet until the signaling catalog has settled"
    );
    loop {
        std::thread::sleep(TICK);
        // BOTH inputs are re-read every tick, never cached. The committed set is
        // the only thing that says what the network actually believes, and a
        // snapshot taken once at boot is not the same information: a node whose
        // host was still catching up when this started would hold an empty
        // snapshot forever, agree with an empty desired set, and never notice a
        // dead daemon's tags standing on chain. Comparing against FRESH
        // committed state is what makes this converge rather than merely track
        // — the old pump's per-tick read was the guarantee, not just its cost.
        // Two loopback queries per tick — `committed` issues one for tags and
        // one for resources — is not a price worth trading it for.
        let settled = started.elapsed() >= SETTLE;
        let decision = tick(
            settled,
            committed(&watch.base, &watch.node_key),
            desired(&watch, std::time::Instant::now()),
        );
        match decision {
            Tick::Wait => tracing::debug!(
                target: "ducktape::service",
                "announce watcher waiting for the signaling catalog to settle"
            ),
            Tick::Unknown => {
                failures += 1;
                report(failures, "the committed capability registry did not answer");
            }
            Tick::Undecidable(reason) => {
                failures += 1;
                report(failures, &reason);
            }
            Tick::Quiet => failures = 0,
            Tick::Announce(want) => match submit(&watch.base, &watch.workspace, &want) {
                Ok(height) => {
                    failures = 0;
                    tracing::info!(
                        target: "ducktape::service",
                        height,
                        capabilities = ?want.capabilities,
                        "capabilities announced"
                    );
                }
                // nothing is latched on failure, so the next tick simply
                // re-derives and retries — which is what a node not yet admitted
                // to its network needs. Bounded by the tick plus the submit's
                // own hold, so ~one attempt per 20 s.
                Err(reason) => {
                    failures += 1;
                    report(failures, &reason);
                }
            },
        }
    }
}

/// the announce this node would emit at `now`. Reads the two live inputs and
/// delegates the decision; it decides nothing itself.
///
/// `now` is threaded in rather than read from the clock so the catalog's TTL
/// expiry — i.e. retraction, the whole reason this thread exists — is reachable
/// from a test without sleeping.
fn desired(watch: &Watch, now: std::time::Instant) -> Result<AnnounceSet, String> {
    let signaling = watch.services.live(now);
    // consent that cannot be read is not consent — but silently retracting a
    // live node's whole announce because someone corrupted a toml is exactly
    // the failure that must not be quiet, so this is an error, not an empty set.
    let services = crate::services::load(&watch.workspace)?;
    announced_set(&services.grants, &signaling, &watch.capacity)
        .map_err(|refusal| refusal.to_string())
}

fn report(failures: u64, reason: &str) {
    let first = failures == 1;
    let every_nth = failures.is_multiple_of(REPORT_EVERY);
    if !first && !every_nth {
        return;
    }
    tracing::warn!(
        target: "ducktape::service",
        attempts = failures,
        reason = "announce_failed",
        "this node is not announcing what it offers; retrying: {reason}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn caps(cores: u64) -> BTreeMap<String, u64> {
        BTreeMap::from([("cores".to_string(), cores)])
    }

    fn grant(kind: &str, capabilities: &[&str]) -> ServiceGrant {
        ServiceGrant {
            kind: kind.into(),
            instance: "aa".repeat(32),
            nonce: "bb".repeat(16),
            granted_unix: 1,
            capabilities: tags(capabilities),
            scopes: Vec::new(),
        }
    }

    fn signal(kind: &str, capabilities: &[&str]) -> noded::services::Signaling {
        noded::services::Signaling {
            kind: kind.into(),
            version: "1".into(),
            build: "b".into(),
            capabilities: tags(capabilities),
            scopes: Vec::new(),
            needs: Vec::new(),
        }
    }

    #[test]
    fn the_set_is_the_grant_intersected_with_a_live_hello() {
        // a grant with NO daemon signaling offers nothing: the node discovers
        // nothing itself, so consent alone is not evidence anything can run.
        let grants = [grant("compute", &["claude", "codex"])];
        let set = announced_set(&grants, &[], &caps(8)).unwrap();
        assert!(
            set.capabilities.is_empty(),
            "a grant without a hello offers nothing"
        );
        assert!(set.resources.is_empty(), "empty tags force empty resources");

        // a daemon signals: the intersection appears, kind tag included, and
        // the daemon cannot widen the grant.
        let live = [signal("compute", &["claude", "codex", "extra"])];
        let set = announced_set(&grants, &live, &caps(8)).unwrap();
        assert_eq!(set.capabilities, tags(&["claude", "codex", "compute"]));
        assert_eq!(set.resources, caps(8));

        // ... and the grant cannot widen the daemon either.
        let grants = [grant("compute", &["claude", "never-offered"])];
        let live = [signal("compute", &["claude"])];
        let set = announced_set(&grants, &live, &caps(8)).unwrap();
        assert_eq!(set.capabilities, tags(&["claude", "compute"]));

        // signaling with NO grant announces nothing at all.
        let set = announced_set(&[], &live, &caps(8)).unwrap();
        assert!(set.capabilities.is_empty(), "no consent, no announce");
    }

    #[test]
    fn every_granted_and_signaling_kind_contributes_its_own_tag() {
        let grants = [grant("agent", &["claude"]), grant("compute", &["codex"])];
        let live = [signal("agent", &["claude"]), signal("compute", &["codex"])];
        let set = announced_set(&grants, &live, &caps(4)).unwrap();
        assert_eq!(
            set.capabilities,
            tags(&["agent", "claude", "codex", "compute"])
        );
    }

    #[test]
    fn a_kind_with_no_executors_still_announces_itself() {
        // the airlock plug: it spawns nothing, so its executor set is empty —
        // but "which nodes run airlock" is exactly what placement asks.
        let grants = [grant("airlock", &[])];
        let live = [signal("airlock", &[])];
        let set = announced_set(&grants, &live, &BTreeMap::new()).unwrap();
        assert_eq!(set.capabilities, tags(&["airlock"]));
    }

    #[test]
    fn an_absent_daemon_drops_only_its_own_kind() {
        let grants = [grant("agent", &["claude"]), grant("compute", &["codex"])];
        let live = [signal("agent", &["claude"])];
        let set = announced_set(&grants, &live, &caps(4)).unwrap();
        assert_eq!(
            set.capabilities,
            tags(&["agent", "claude"]),
            "compute's grant survives, but nothing of compute is announced"
        );
    }

    #[test]
    fn an_illegal_tag_refuses_the_whole_set() {
        // the hello boundary admits a space; the registry does not. Refusing
        // here is the point: the announce is all-or-nothing, so trimming would
        // hide it until an operator wondered why their node was in no pool.
        let grants = [grant("compute", &["Claude Sonnet", "codex"])];
        let live = [signal("compute", &["Claude Sonnet", "codex"])];
        assert_eq!(
            announced_set(&grants, &live, &caps(8)),
            Err(Refusal::IllegalTags(tags(&["Claude Sonnet"])))
        );
    }

    /// a workspace whose `services.toml` grants each `(kind, tags)` pair — the
    /// watcher reads consent off disk, so a test grant IS a file.
    fn granted_workspace(grants: &[(&str, &[&str])]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("scratch workspace");
        let mut body = String::new();
        let mut sorted = grants.to_vec();
        sorted.sort_by_key(|(kind, _)| *kind);
        for (kind, capabilities) in &sorted {
            let tags = capabilities
                .iter()
                .map(|tag| format!("    {tag:?},"))
                .collect::<Vec<_>>()
                .join("\n");
            body.push_str(&format!(
                "\n[[service]]\nkind = {kind:?}\ninstance = {:?}\nnonce = {:?}\n\
                 granted_unix = 1\ncapabilities = [\n{tags}\n]\nscopes = []\n",
                "aa".repeat(32),
                "bb".repeat(16),
            ));
        }
        std::fs::write(dir.path().join(crate::services::FILE_NAME), body).expect("write grants");
        dir
    }

    fn watcher(workspace: &tempfile::TempDir, services: noded::services::ServiceCatalog) -> Watch {
        Watch {
            base: String::new(),
            node_key: vec![7u8; 32],
            workspace: workspace.path().to_path_buf(),
            services,
            capacity: caps(4),
        }
    }

    /// register a live hello for `kind` at `at`.
    fn beat(
        catalog: &noded::services::ServiceCatalog,
        kind: &str,
        capabilities: &[&str],
        at: std::time::Instant,
    ) {
        catalog
            .hello(
                noded::services::Hello {
                    kind: kind.into(),
                    version: "1".into(),
                    build: noded::services::build_identity_or_unknown().to_string(),
                    capabilities: tags(capabilities),
                    scopes: Vec::new(),
                    needs: Vec::new(),
                },
                at,
            )
            .expect("a well-formed hello is admitted");
    }

    #[test]
    fn a_daemon_that_stops_signaling_retracts_the_announce() {
        // THE behaviour this whole thread exists for. Driven through the real
        // catalog with an explicit clock, so the TTL expiry that constitutes
        // "the daemon stopped" is actually exercised — and without sleeping.
        let catalog = noded::services::ServiceCatalog::default();
        let workspace = granted_workspace(&[("compute", &["claude"])]);
        let watch = watcher(&workspace, catalog.clone());
        let t0 = std::time::Instant::now();

        beat(&catalog, "compute", &["claude"], t0);
        assert_eq!(
            desired(&watch, t0).unwrap().capabilities,
            tags(&["claude", "compute"]),
            "a signaling grant announces its kind and its executors"
        );

        // the daemon dies; its entry ages out one TTL after its last beat.
        let dead = t0 + noded::services::HELLO_TTL + std::time::Duration::from_secs(1);
        let want = desired(&watch, dead).unwrap();
        assert!(
            want.capabilities.is_empty(),
            "an absent daemon retracts the tags it was serving"
        );
        assert!(
            want.resources.is_empty(),
            "and the capacity with them — resources without tags is a module-level reject"
        );
        // the grant is untouched on disk: consent survives the daemon.
        assert!(
            crate::services::load(workspace.path())
                .unwrap()
                .grant("compute")
                .is_some()
        );
    }

    #[test]
    fn an_absent_daemon_retracts_only_its_own_kind() {
        let catalog = noded::services::ServiceCatalog::default();
        let workspace = granted_workspace(&[("agent", &["claude"]), ("compute", &["codex"])]);
        let watch = watcher(&workspace, catalog.clone());
        let t0 = std::time::Instant::now();

        // compute beats once and stops; agent keeps beating.
        beat(&catalog, "compute", &["codex"], t0);
        let later = t0 + std::time::Duration::from_secs(20);
        beat(&catalog, "agent", &["claude"], later);
        assert_eq!(
            desired(&watch, later).unwrap().capabilities,
            tags(&["agent", "claude", "codex", "compute"]),
            "both kinds are live here"
        );

        // past compute's deadline but inside agent's: only compute drops.
        let gap = t0 + noded::services::HELLO_TTL + std::time::Duration::from_secs(1);
        assert_eq!(
            desired(&watch, gap).unwrap().capabilities,
            tags(&["agent", "claude"]),
            "the surviving daemon keeps announcing; only the dead kind is retracted"
        );
    }

    #[test]
    fn nothing_is_submitted_before_the_catalog_has_settled() {
        // the boot-retraction guard. The catalog is empty on a node restart, so
        // an unsettled tick would read `want` = {} against a live committed set
        // and RETRACT a daemon that never stopped. Deleting the gate makes this
        // return `Announce({})` — the retraction — instead of `Wait`.
        let committed = AnnounceSet {
            capabilities: tags(&["claude", "compute"]),
            resources: caps(4),
        };
        let empty = AnnounceSet::default();
        assert_eq!(
            tick(false, Some(committed.clone()), Ok(empty.clone())),
            Tick::Wait,
            "an unsettled tick must never act, least of all retract"
        );
        // and once settled, the same inputs DO retract — so the test above is
        // pinning the gate, not a set that happens to agree.
        assert_eq!(
            tick(true, Some(committed), Ok(empty.clone())),
            Tick::Announce(empty),
            "a settled tick with a genuinely empty catalog retracts"
        );
    }

    #[test]
    fn a_failed_registry_read_never_submits() {
        // a read failure is not evidence about anything. If `Unknown` ever
        // became a submit, a node whose host was busy would retract its whole
        // announce for the duration.
        let want = AnnounceSet {
            capabilities: tags(&["compute"]),
            resources: caps(4),
        };
        assert_eq!(tick(true, None, Ok(want)), Tick::Unknown);
    }

    #[test]
    fn a_matching_registry_is_quiet_and_an_undecidable_one_is_reported() {
        let set = AnnounceSet {
            capabilities: tags(&["compute"]),
            resources: caps(4),
        };
        // this is also what makes `enable`/`disable` cost no follow-up frame:
        // the verb's own submit is already committed by the next tick.
        assert_eq!(tick(true, Some(set.clone()), Ok(set.clone())), Tick::Quiet);
        assert_eq!(
            tick(true, Some(set), Err("grants unreadable".into())),
            Tick::Undecidable("grants unreadable".into()),
            "an unreadable grant is reported, never treated as an empty set"
        );
    }

    #[test]
    fn the_settle_window_outlasts_a_live_daemons_beat() {
        // the boot-retraction guard is only correct if a daemon that never
        // stopped is GUARANTEED to have re-registered before the window closes.
        // A daemon beats every HELLO_TTL/3, so it beats at least twice inside
        // SETTLE even if its first post-boot attempt is lost.
        // the beat the DAEMON actually sleeps on, not a re-derivation of it —
        // reading `HELLO_TTL / 3` here would keep this test green while someone
        // retuned `HEARTBEAT` out from under the property it claims to pin.
        let beat_period = crate::services::HEARTBEAT;
        assert!(
            SETTLE >= beat_period * 2,
            "SETTLE must outlast at least two daemon beats, else a node restart \
             retracts a healthy daemon"
        );
        assert!(
            TICK <= SETTLE,
            "the first tick must not fall outside the settle window it gates"
        );
    }

    #[test]
    fn the_widest_set_is_liveness_independent_and_bounds_every_live_one() {
        // what `plan_enable` bounds. It must not depend on who happens to be
        // signaling, and every live derivation must be a subset of it.
        let grants = [grant("agent", &["claude"]), grant("compute", &["codex"])];
        let bound = announced_set(&grants, &widest(&grants), &caps(4)).unwrap();
        assert_eq!(
            bound.capabilities,
            tags(&["agent", "claude", "codex", "compute"]),
            "every granted kind contributes everything it was granted"
        );
        // with only one daemon up, the live set is strictly smaller.
        let live = [signal("agent", &["claude"])];
        let now = announced_set(&grants, &live, &caps(4)).unwrap();
        assert!(
            now.capabilities
                .iter()
                .all(|tag| bound.capabilities.contains(tag)),
            "a live derivation is always a subset of the bound"
        );
        assert!(now.capabilities.len() < bound.capabilities.len());
    }

}
