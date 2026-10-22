// A test's `/v1/logs` holds its own nodes' lines alone, whatever else the binary runs: the two
// tests below each start one node, and each sees one start.

use super::*;

fn one_node_reads_one_start() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let runtime = client_runtime();
    runtime.block_on(async {
        let lines = live.client.logs().await.unwrap();
        let started: Vec<&String> = lines
            .iter()
            .filter(|line| line.contains("node_started"))
            .collect();
        assert_eq!(
            started.len(),
            1,
            "the one node this test started: {started:?}"
        );
    });
    shut_down(&runtime, &[&seat], vec![live]);
}

#[test]
fn a_test_reads_only_its_own_nodes_lines() {
    apart!();
    one_node_reads_one_start();
}

#[test]
fn another_test_reads_only_its_own_nodes_lines() {
    apart!();
    one_node_reads_one_start();
}
