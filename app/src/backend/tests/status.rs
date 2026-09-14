use super::*;

#[test]
fn machine_values_read_as_a_person_reads_them() {
    assert_eq!(mmss(0), "00:00");
    assert_eq!(mmss(4 * 60 + 7), "04:07");
    assert_eq!(initials_of("Kestrel Song"), "KS");
    assert_eq!(initials_of("triage"), "TR");
    assert_eq!(initials_of(""), "?");
    assert_eq!(optional_number(Some(4)), "4");
    // absent, not zero: a resident's status carries no consensus section.
    assert_eq!(optional_number(None), "—");
}

/// THE OTHER HALF OF THE SAME FACT: the reading has to ARRIVE as `UNMEASURED`.
///
/// The test above pins the renderers. This pins the producer, on the one path
/// where a real node publishes nothing: a resident, a joiner or the embedded
/// local daemon omits `operations` entirely (`skip_serializing_if` on the node
/// side), so the `unwrap_or` arms are what run. Every fixture in this file
/// supplies an `operations` object, so those arms had no coverage at all —
/// `.unwrap_or(UNMEASURED)` could be changed to `.unwrap_or(0)` with the whole
/// suite green, which is exactly the defect on exactly the lane this file
/// spends twenty lines forbidding.
#[test]
fn a_status_without_operations_produces_unmeasured_readings() {
    let resident = serde_json::json!({
        "version": "0.1.0",
        "root_hash": "abc",
        "height": 0,
    });
    let facts = node_facts(&resident);
    assert_eq!(
        facts.last_finalized_at, UNMEASURED,
        "an omitted `operations` must not read as a finalization at epoch zero"
    );
    assert_eq!(
        facts.checkpoint_height, UNMEASURED,
        "nor as a checkpoint at genesis"
    );
    assert_eq!(
        facts.height, UNMEASURED,
        "and a wire `0` head is the node's own no-boundary-served sentinel"
    );
    // the trio is `Option` for the same reason and stays absent.
    assert_eq!(facts.view, None);
    assert_eq!(facts.quorum, None);
    assert_eq!(facts.reachable_validators, None);
}

/// SYNC IS READ OFF `phase`, NEVER OFF THE PRESENCE OF `sync`.
///
/// `operations.sync` is set by `begin_sync` and never cleared — no writer in
/// `crates/noded/src/metrics.rs` puts it back to `None`. A node that finished
/// syncing hours ago still carries the last run's heights, so reading presence
/// as "is syncing" paints a progress bar that never goes away.
#[test]
fn a_finished_sync_is_not_a_sync_in_progress() {
    let caught_up = serde_json::json!({
        "operations": {
            "phase": "serving",
            "phase_since": 1_700_000_000,
            "sync": { "target_height": 900, "applied_height": 900, "retries": 3, "failures": 1 },
        },
    });
    let facts = node_facts(&caught_up);
    assert_eq!(facts.phase, "serving");
    assert!(
        !sync_in_progress(&facts.phase),
        "a stale sync block must not read as a sync in progress"
    );
    // the numbers still ride: Settings prints them as CUMULATIVE totals.
    assert_eq!(facts.sync_retries, 3);
    assert_eq!(facts.sync_failures, 1);
    assert_eq!(facts.phase_since, 1_700_000_000);

    let catching_up = serde_json::json!({
        "operations": {
            "phase": "syncing",
            "sync": { "target_height": 900, "applied_height": 412 },
        },
    });
    let facts = node_facts(&catching_up);
    assert!(sync_in_progress(&facts.phase));
    assert_eq!(facts.sync_applied, 412);
    assert_eq!(facts.sync_target, 900);

    // A node that has never synced publishes no `sync` at all. Heights are
    // UNMEASURED because the node published none; the counters are genuinely
    // zero, because a count of nothing IS zero.
    let fresh = serde_json::json!({ "operations": { "phase": "validating" } });
    let facts = node_facts(&fresh);
    assert_eq!(facts.sync_target, UNMEASURED);
    assert_eq!(facts.sync_applied, UNMEASURED);
    assert_eq!(facts.sync_retries, 0);
    assert_eq!(facts.sync_last_error, "");
    assert_eq!(facts.phase_since, UNMEASURED);
}

/// ONE STRING FOR ALL THREE SURFACES, so the titlebar, the explorer header and
/// Settings cannot disagree about what the node is doing.
#[test]
fn the_sync_label_shows_progress_only_while_catching_up() {
    assert_eq!(sync_label("syncing", 412, 900), "Syncing 412 / 900");

    // CAUGHT UP: the phase alone, however live the numbers beside it look.
    // `operations.sync` is never cleared, so a finished run's heights sit in
    // the document forever and must not reach a reader.
    assert_eq!(sync_label("serving", 900, 900), "Serving");
    assert_eq!(sync_label("validating", 900, 900), "Validating");

    // syncing with nothing published yet is still honest about the phase.
    assert_eq!(sync_label("syncing", UNMEASURED, UNMEASURED), "Syncing");
    assert_eq!(sync_label("syncing", 412, UNMEASURED), "Syncing");

    // and a node that published no phase says NOTHING rather than guessing.
    assert_eq!(sync_label("", 412, 900), "");
}

/// ONE CARD, ONE SAMPLE — AND THE SAMPLE IS THE WHOLE PAIR.
///
/// A checkpoint carries no meaning alone; it only ever says how far the durable
/// snapshot trails the head. So the head printed beside it has to come from the
/// same read, or the pair can render an order no node is ever in. The app did
/// exactly that: `HEIGHT h 422,553` under `CHECKPOINT h 422,563`, from a live
/// register and a facts document sampled seconds apart on a chain moving
/// several blocks a second.
///
/// The stub serves `/v1/status` once and refuses the rest, so this fails on a
/// second read no matter what the fields are called.
#[tokio::test(flavor = "current_thread")]
async fn the_node_tile_prints_a_head_and_a_checkpoint_from_one_status_read() {
    // Shape copied from the live demo node, whose checkpoint trails its head by
    // 118 blocks — the direction a healthy node is always in.
    const SERVING: &str = r#"{"version":"0.1.0","root_hash":"20c53b","height":426099,"modules":[],"public_key":"ab","operations":{"last_finalized_at":426099,"storage":{"checkpoint_height":425981}}}"#;
    let rpc = node_that_serves_its_status_once(SERVING).await;

    let facts = load_node_facts(rpc)
        .await
        .expect("one read fills the whole card; a second one is the defect");

    assert_eq!(facts.height, 426_099);
    assert_eq!(facts.checkpoint_height, 425_981);
    assert_eq!(facts.public_key, "ab");
    assert!(
        facts.checkpoint_height <= facts.height,
        "a durable snapshot cannot be ahead of the head it was taken from"
    );
}

/// A NODE SERVING NO BOUNDARY HAS NO HEAD — AND ITS CHECKPOINT KEEPS CLIMBING.
///
/// One read is necessary and not sufficient: the replica lane publishes a
/// document that is itself two instants. `publish_replica_status` fills `height`
/// from `serving`, which is `None` — and so `0` — through a range-pruned
/// backfill, an unresolvable pruned view and an epoch cutover, while it hands
/// the same call the LIVE `replica_prev_ckpt`, which only ever climbs. Read as
/// a measurement, that one honest document renders `HEIGHT h 0` above
/// `CHECKPOINT h 425,981`: a measured zero AND a starker inversion than the
/// ten-block skew this change set out to remove.
///
/// `0` is the node's own word for "no boundary served" — `NodeStatus::default`,
/// the validator's `unwrap_or(0)` and the replica's `None` arm all write it —
/// so the app reads it as absence, and the pair prints `h —` over the
/// checkpoint that is genuinely on disk.
#[tokio::test(flavor = "current_thread")]
async fn a_resyncing_replica_has_no_head_to_print_a_checkpoint_against() {
    const RESYNCING: &str = r#"{"version":"0.1.0","root_hash":"","height":0,"modules":[],"public_key":"ab","operations":{"last_finalized_at":0,"storage":{"checkpoint_height":425981}}}"#;
    let rpc = node_that_serves_its_status_once(RESYNCING).await;

    let facts = load_node_facts(rpc)
        .await
        .expect("one read fills the whole card; a second one is the defect");

    assert_eq!(
        facts.checkpoint_height, 425_981,
        "the checkpoint on disk is real and stays printed"
    );
    assert_eq!(
        facts.height, UNMEASURED,
        "a node serving no boundary reports height 0; that is absence, not a measurement"
    );
    assert!(
        facts.height < 0 || facts.checkpoint_height <= facts.height,
        "the pair may say `h —`, but it may never say a checkpoint above its head"
    );
}

/// A DEFAULTED STATUS DOCUMENT MUST NOT READ AS MEASURED.
///
/// Asserting the RENDERED strings, not the integers: `-1` is only correct
/// because the renderers turn it into an em dash, and a future change to
/// either end that broke the pairing would leave an integer assertion green
/// while the screen went back to printing a head no node ever served.
#[test]
fn a_defaulted_node_facts_prints_as_unserved_everywhere() {
    let facts = NodeFacts::default();
    assert_eq!(facts.height, UNMEASURED);
    assert_eq!(facts.checkpoint_height, UNMEASURED);
    assert_eq!(facts.last_finalized_at, UNMEASURED);

    // What these render as is pinned once, by
    // `an_unpublished_operations_reading_renders_as_unknown` above — including
    // that a real `0` still prints `h 0`. Repeating it here would be
    // duplication wearing the costume of defence in depth.
}

/// A dropped file's target is composed under the directory the view stands
/// in, and the duckfs root is `/`, never "": the module's path check is
/// `starts_with('/')`, so a "" root answered every root write with a 400.
#[test]
fn a_dropped_file_lands_under_its_directory() {
    assert_eq!(fs_child("/".into(), "notes".into()), "/notes");
    assert_eq!(fs_child("/shared".into(), "notes".into()), "/shared/notes");
}

/// THE WRITE GATE IS THE FILES MODULE'S OWN RULE, asked before the round
/// trip: the roots refuse, and every home and `/shared` answer with nothing.
/// A device without a key has nothing to check; its refusal comes from the
/// signer.
#[test]
fn the_files_write_gate_answers_in_the_modules_words() {
    let me = "ab".repeat(32);
    let other = "cd".repeat(32);
    let gate = |dir: &str| files_write_gate(dir.into(), me.clone());
    assert_eq!(gate("/"), "path is outside /home and /shared");
    assert_eq!(gate("/home"), "home root is not writable");
    assert_eq!(gate("/shared"), "");
    assert_eq!(gate("/shared/reports"), "");
    assert_eq!(gate(&format!("/home/ext:{me}")), "");
    assert_eq!(gate(&format!("/home/ext:{me}/notes")), "");
    assert_eq!(gate(&format!("/home/ext:{other}")), "");
    assert_eq!(files_write_gate("/".into(), String::new()), "");
}
