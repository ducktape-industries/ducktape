//! The query context an authenticated read is answered in: WHO is asking, and
//! WHAT TIME the network agrees it is.
//!
//! Both are the module's only inputs for a protected read. `collaboration`
//! refuses `Origin::System` outright (the unauthenticated `/v1/query` lane
//! lands there). Deadline-sensitive readers also need the committed
//! `consensus_time`: a wrong clock makes a past deadline look future. This
//! harness probes that context; it does not implement collaboration eligibility.
//!
//! The clock is the subtler half. It advances ONLY at a committed block, and it
//! has to survive the two ways a host reaches a boundary without applying one:
//! a checkpoint reopen and a statesync install. Left at genesis after a restart
//! at height 40 000, every `now < expires_at` check reads as "nothing has
//! expired yet" — fail-open, in exactly the window after a restart.

use futures::executor::block_on;
use host::{BlockContext, Host};
use sdk::{Ctx, Error, Module, Msg, Origin, StateRoot};

/// A module that answers a query with the context it was given, so a test can
/// assert the `Env` the host built rather than a module's opinion of it.
struct EnvProbe;

#[async_trait::async_trait(?Send)]
impl Module for EnvProbe {
    fn id(&self) -> String {
        "probe".into()
    }

    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }

    async fn execute(&mut self, _ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
        Ok(())
    }

    async fn query_with(&self, ctx: &dyn Ctx, _req: &[u8]) -> Result<Vec<u8>, Error> {
        let env = ctx.env();
        let origin = match &env.origin {
            Origin::System => "system".to_string(),
            Origin::External(key) => format!("external:{}", hex(key)),
            other => format!("{other:?}"),
        };
        Ok(format!("{origin}|{}|{}", env.height, env.consensus_time).into_bytes())
    }
}

/// local, so this test pulls no crate in just to print a key.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A module that always fails its commit, so a test can drive a block that
/// reaches the boundary and does not survive it.
struct CommitFails;

#[async_trait::async_trait(?Send)]
impl Module for CommitFails {
    fn id(&self) -> String {
        "fails".into()
    }

    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }

    async fn execute(&mut self, _ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
        Ok(())
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        Err(Error::module("never_commits", "this module never commits"))
    }
}

fn probe(host: &Host) -> (String, u64, u64) {
    read(host, Origin::System)
}

fn read(host: &Host, origin: Origin) -> (String, u64, u64) {
    let raw = block_on(host.query_as("probe", b"", origin)).expect("the probe answers");
    let text = String::from_utf8(raw).expect("utf8");
    let mut parts = text.split('|');
    let who = parts.next().unwrap().to_string();
    let height = parts.next().unwrap().parse().unwrap();
    let time = parts.next().unwrap().parse().unwrap();
    (who, height, time)
}

fn host_with(modules: Vec<Box<dyn Module>>) -> Host {
    Host::genesis(modules).expect("genesis")
}

fn block(height: u64, consensus_time: u64) -> BlockContext {
    BlockContext {
        height,
        consensus_time,
        origin: Origin::System,
    }
}

/// The open lane's reader is `System`, and a module that serves protected
/// content is expected to refuse exactly that. `Host::query` must keep passing
/// it — an authenticated lane that accidentally widened the anonymous one would
/// be worse than having neither.
#[test]
fn the_plain_query_lane_still_reads_as_system() {
    let host = host_with(vec![Box::new(EnvProbe)]);
    let raw = block_on(host.query("probe", b"")).expect("the probe answers");
    assert!(
        String::from_utf8(raw).unwrap().starts_with("system|"),
        "the unauthenticated lane must stay System"
    );
}

/// ...and the authenticated lane carries the verified key, unchanged.
#[test]
fn query_as_carries_the_readers_key_to_the_module() {
    let host = host_with(vec![Box::new(EnvProbe)]);
    let key = vec![0xab; 32];
    let (who, ..) = read(&host, Origin::External(key.clone()));
    assert_eq!(who, format!("external:{}", hex(&key)));
}

/// The clock advances with a COMMITTED block and reads the block's own values,
/// not a wall clock — two nodes answering the same eligibility question off the
/// same committed state must agree.
#[test]
fn a_committed_block_advances_the_clock_a_read_is_answered_against() {
    let mut host = host_with(vec![Box::new(EnvProbe)]);
    assert_eq!(probe(&host), ("system".into(), 0, 0), "genesis is zero");

    block_on(host.submit_block(block(7, 1_700_000_000), Vec::new())).expect("block commits");

    let (_, height, time) = probe(&host);
    assert_eq!((height, time), (7, 1_700_000_000));
}

/// THE fail-open window root named: a host that reopens a checkpoint or lands a
/// statesync install has applied no block, so nothing advanced the clock. Its
/// first authenticated read must not answer at genesis time — at 0 an expiry
/// check says "nothing has expired yet" for a network that has been running for
/// months.
#[test]
fn a_nonzero_boundary_restores_the_clock_before_any_read() {
    let mut host = host_with(vec![Box::new(EnvProbe)]);
    assert_eq!(probe(&host), ("system".into(), 0, 0));

    // what `restore_host` / `sync_all_modules` do after composing at a
    // manifest's boundary. `consensus_time` IS the height on this lane.
    host.restore_committed(40_000, 40_000);

    let (_, height, time) = probe(&host);
    assert_eq!(
        (height, time),
        (40_000, 40_000),
        "a read right after a restart is answered at the restored boundary"
    );
}

/// A restore is a SET, not a max: state and clock move together, so a resident
/// that re-syncs backward to an older boundary must not keep a clock belonging
/// to state it no longer holds.
#[test]
fn a_backward_resync_moves_the_clock_back_with_the_state() {
    let mut host = host_with(vec![Box::new(EnvProbe)]);
    host.restore_committed(40_000, 40_000);
    host.restore_committed(12_000, 12_000);

    let (_, height, time) = probe(&host);
    assert_eq!((height, time), (12_000, 12_000));
}

/// A block that does not survive its boundary must not move the clock. The
/// commit is the single boundary for the whole block, so a read between blocks
/// sees the last block that actually landed — never one that was rolled back.
#[test]
fn a_failed_commit_does_not_advance_the_clock() {
    let mut host = host_with(vec![Box::new(EnvProbe), Box::new(CommitFails)]);
    block_on(host.submit_block(block(3, 300), Vec::new())).expect("a clean block commits");
    assert_eq!(probe(&host), ("system".into(), 3, 300));

    // touch the module whose commit fails, so the boundary runs and faults.
    let doomed = Msg {
        target: "fails".into(),
        payload: b"{}".to_vec(),
    };
    let outcome = block_on(host.submit_block(block(4, 400), vec![(Origin::System, doomed)]));
    assert!(outcome.is_err(), "a failing commit boundary is fatal");

    let (_, height, time) = probe(&host);
    assert_eq!(
        (height, time),
        (3, 300),
        "the clock must still name the last block that COMMITTED"
    );
}

/// Reopening uses bytes written at the previous host's committed boundary,
/// not constants supplied only to the new instance. This exercises the host
/// clock and authenticated query context; it does not boot `node::restore_host`
/// or claim a collaboration delivery-eligibility API exists.
#[test]
fn a_fresh_host_reads_at_the_persisted_committed_boundary() {
    use std::io::Write as _;

    for (height, consensus_time, deadline) in [
        (40_000, 40_000, 39_999),
        (7, 1_700_000_000_000, 1_699_999_999_999),
    ] {
        let directory = tempfile::tempdir().expect("isolated checkpoint directory");
        let checkpoint = directory.path().join("committed-clock.json");
        let reader = Origin::External(vec![0xab; 32]);
        let mut previous = host_with(vec![Box::new(EnvProbe)]);
        block_on(previous.submit_block(block(height, consensus_time), Vec::new()))
            .expect("the boundary commits");
        let (_, committed_height, committed_time) = read(&previous, reader.clone());
        let encoded = serde_json::to_vec(&(committed_height, committed_time)).unwrap();
        let mut file = std::fs::File::create(&checkpoint).unwrap();
        file.write_all(&encoded).unwrap();
        file.sync_all().unwrap();
        drop(file);
        drop(previous);

        let persisted: (u64, u64) =
            serde_json::from_slice(&std::fs::read(&checkpoint).unwrap()).unwrap();
        let mut reopened = host_with(vec![Box::new(EnvProbe)]);
        assert_eq!(probe(&reopened), ("system".into(), 0, 0));
        reopened.restore_committed(persisted.0, persisted.1);
        // No new block is applied: this is the first authenticated read after
        // restoration, exactly when a missing restore otherwise exposes zero.
        let (who, restored_height, restored_time) = read(&reopened, reader);
        assert_eq!(who, format!("external:{}", hex(&[0xab; 32])));
        assert!(
            restored_time >= deadline,
            "a past deadline must not look future after reopening the committed clock"
        );
        assert_eq!(
            (restored_height, restored_time),
            (height, consensus_time),
            "the first read must use the persisted boundary without another block"
        );
    }
}
