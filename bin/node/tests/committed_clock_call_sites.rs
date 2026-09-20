//! Every path that composes a Host from a MANIFEST must set its committed
//! clock, guarded at the source.
//!
//! `Host::query_as` answers an authenticated read against the last committed
//! block's height and consensus time. Replay advances that clock by re-applying
//! blocks; the two manifest paths — a checkpoint reopen (`restore_host`) and a
//! statesync install (`sync_all_modules`) — jump straight to a boundary and
//! apply nothing, so each has to set it explicitly.
//!
//! Miss one and the node answers its first authenticated reads with
//! `consensus_time = 0`. At zero, every `now < expires_at` check reads as
//! "nothing has expired yet" — fail-open, for as long as it takes the next
//! block to commit after a restart.
//!
//! Why a source lint and not a behavioural test: `restore_host` and
//! `sync_all_modules` are `pub(super)`, so a test crate cannot call them, and
//! exercising them for real means booting a node against a checkpoint — a full
//! e2e. The unit tests in `crates/kernel/host/tests/query_reader_context.rs`
//! prove what `restore_committed` DOES; only this proves it is CALLED. Deleting
//! either call site fails this test, which is the mutation that matters.
//!
//! `genesis_host` is deliberately absent from the rule: genesis has applied
//! nothing, so `(0, 0)` is the truth there rather than a missing call.

use std::path::Path;

/// The 1-based lines carrying `needle`. An empty hit list is a DEFECT, not a
/// pass — a lint that matches nothing guards nothing, and this file's whole job
/// is to keep guarding after the code moves.
fn lines_of(src: &str, needle: &str) -> Vec<usize> {
    let hits: Vec<usize> = src
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains(needle))
        .map(|(n, _)| n + 1)
        .collect();
    assert!(
        !hits.is_empty(),
        "`{needle}` is gone from host_state.rs — this lint no longer guards anything"
    );
    hits
}

fn host_state() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/host_state.rs");
    std::fs::read_to_string(&path).expect("read the host composition module")
}

/// Both manifest paths set the clock, and they set it on the way out — after
/// the host is fully composed and wired, immediately before it is returned.
#[test]
fn every_manifest_composed_host_sets_its_committed_clock_before_returning() {
    let src = host_state();

    let restores = lines_of(&src, "host.restore_committed(");
    assert_eq!(
        restores.len(),
        2,
        "exactly two paths compose a Host from a manifest (the checkpoint reopen \
         and the statesync install), so exactly two set the clock — found {restores:?}"
    );

    // the three `Ok(host)` returns are genesis_host, restore_host and
    // sync_all_modules. The last two must each be preceded by a restore.
    let returns = lines_of(&src, "    Ok(host)");
    assert_eq!(
        returns.len(),
        3,
        "expected genesis_host, restore_host and sync_all_modules to return a \
         composed host — found {returns:?}"
    );

    for restore in &restores {
        let returned_right_after = returns.iter().any(|ret| *ret == restore + 1);
        assert!(
            returned_right_after,
            "the clock is set on the way out, immediately before `Ok(host)`; \
             line {restore} is followed by something else"
        );
    }
}

/// The clock is set from the BOUNDARY the path just proved, never from a wall
/// clock. A `SystemTime` here would make two nodes answering the same
/// eligibility question off the same committed state disagree.
#[test]
fn the_restored_clock_comes_from_the_manifest_and_never_from_a_wall_clock() {
    let src = host_state();

    for line in src
        .lines()
        .filter(|l| l.contains("host.restore_committed("))
    {
        let from_a_height = line.contains("height");
        assert!(
            from_a_height,
            "a restored clock must come from the boundary's height: {}",
            line.trim()
        );
        for wall_clock in ["SystemTime", "Instant", "now()", "UNIX_EPOCH"] {
            assert!(
                !line.contains(wall_clock),
                "a restored clock must not read a local clock ({wall_clock}): {}",
                line.trim()
            );
        }
    }
}

/// Genesis is not a missing call. It has applied nothing, so `(0, 0)` IS its
/// committed clock — this pins that the omission is deliberate, so nobody
/// "fixes" it by inventing a height for a chain that has no blocks.
#[test]
fn genesis_deliberately_sets_no_clock() {
    let src = host_state();
    let genesis_at = lines_of(&src, "pub(super) async fn genesis_host")[0];
    let genesis_returns = *lines_of(&src, "    Ok(host)")
        .iter()
        .find(|line| **line > genesis_at)
        .expect("genesis_host returns a host");

    let body = src
        .lines()
        .skip(genesis_at)
        .take(genesis_returns - genesis_at);
    assert!(
        !body
            .into_iter()
            .any(|line| line.contains("restore_committed")),
        "genesis has applied no block, so (0, 0) is the truth and not an omission"
    );
}
