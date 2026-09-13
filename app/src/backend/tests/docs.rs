use super::*;

/// The cheapest thing a view can be asked: zero targets, so the guest scans
/// nothing and answers an empty group list. `await_fold` reads only the
/// reply's watermark header, so the body should cost as little as the lane
/// allows. The kernel's `rpc.view` probes with the CALLER's own query — this
/// stands in for one.
fn empty_pages_probe() -> serde_json::Value {
    serde_json::json!({ "threads_for_targets": { "targets": [] } })
}

/// A PAGE HIT NAMES ITS PAGE, AND SAYS EACH THING ONCE. The index's hit row
/// carries a `page_id` and no title, so nothing downstream could name the page
/// a match came from: the Explorer set BOTH its row title and its snippet to
/// `hit.text` — the same sentence printed twice — and its only metadata was
/// the block kind (`pages · Text`), which is true of nearly every hit. The
/// palette printed that bare kind too, and the pages search panel printed the
/// raw `block_id`.
///
/// The title is now joined in at the producer, so all three surfaces agree —
/// the shape #997 used for the chat hit's room.
#[test]
fn a_page_search_hit_names_the_page_it_came_from() {
    let row = |page_id: &str, text: &str| pages::index::PageBlockRow {
        block_id: format!("block-{text}"),
        author: pages::Party::System,
        page_id: page_id.into(),
        parent: Some(page_id.into()),
        kind: BlockKind::Paragraph,
        text: text.into(),
        marks: Vec::new(),
        checked: false,
        children: Vec::new(),
        height: 1,
        time: 1,
    };
    let page = |id: &str, title: &str| PageRow {
        id: id.into(),
        title: title.into(),
        parent: None,
    };

    let hits = titled_page_hits(
        vec![
            row("page-1", "Tail paragraph after the list"),
            row("page-2", "second mention"),
            // A page the index does not carry, and one with no title at all.
            row("page-gone", "orphan mention"),
            row("page-3", "untitled mention"),
        ],
        vec![
            page("page-1", "Design QA"),
            page("page-2", "Team Runbook"),
            page("page-3", ""),
        ],
    );

    assert_eq!(hits[0].page_title, "Design QA");
    assert_eq!(hits[1].page_title, "Team Runbook");
    // The sidebar calls a titleless page "Untitled"; a hit must not read
    // differently, and a missing page must not read blank.
    assert_eq!(hits[2].page_title, "Untitled");
    assert_eq!(hits[3].page_title, "Untitled");
    // The join must not disturb what the row already carried.
    assert_eq!(hits[0].text, "Tail paragraph after the list");
    assert_eq!(hits[0].page_id, "page-1");
    assert_eq!(hits[0].kind, "Text");

    // Guest search surfaces verify their own rendered replies in their wire tests.
    const PALETTE: &str = include_str!("../../shell.rs");
    assert!(
        PALETTE.contains("hit.page_title"),
        "the palette's page hit names its page"
    );
}

/// A FAILED TITLE LOOKUP DEGRADES THE LABEL, NOT THE RESULTS. #1003 joined the
/// page index onto the hits with `?`, so a `ListPages` failure — a SECOND round
/// trip, made after the node had already answered the search — turned a
/// successful search into an `Err`. Both readers discard that silently: the
/// Explorer's `if let Ok(pages)` (backend/search.rs) drops every page hit from
/// a workspace search, and the palette keeps only whichever leg survived. A
/// decoration must never destroy the payload.
#[tokio::test(flavor = "current_thread")]
async fn a_failed_title_lookup_keeps_the_page_hits_it_could_not_name() {
    let rpc = node_with_a_broken_page_list().await;
    let data = search_pages(rpc, String::new(), "tail".into())
        .await
        .expect("a search the node answered must not fail on its title lookup");

    assert_eq!(data.hits.len(), 1, "the hit the search returned survives");
    assert_eq!(data.hits[0].text, "Tail paragraph after the list");
    assert_eq!(data.hits[0].page_id, "page-1");
    assert_eq!(data.hits[0].block_id, "block-1");
    // Only the LABEL degrades, onto the same fallback an unresolvable page id
    // already takes in `titled_page_hits`.
    assert_eq!(data.hits[0].page_title, "Untitled");
}

/// A WRITE'S OWN RELOAD WAITS FOR THE FOLD THAT CARRIES IT. `submit_frame`
/// answers ACCEPTANCE; the pages read model is folded behind the block loop,
/// so the reload `create_page`/`delete_page` fire immediately afterwards used
/// to read an index that predated the write — the new page absent from the
/// list it was supposed to land on, the deleted one still sitting in the
/// sidebar. The view lane now answers how far the fold has consumed the op
/// feed, and this is the wait that reads it: stale, stale, then caught up.
#[tokio::test(flavor = "current_thread")]
async fn a_post_write_reload_waits_for_the_fold_to_reach_its_block() {
    let (origin, served) =
        node_scripting_its_fold_watermark(vec![Some("6:0"), Some("6:3"), Some("9:0")]).await;
    let rpc = rpc_client(&origin).expect("stub client");

    let arrived = await_fold(&rpc, "pages", &empty_pages_probe(), 9).await;

    assert!(arrived, "the third probe's watermark reaches block 9");
    assert_eq!(
        served.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "it stops at the probe that answered, not at the budget"
    );
}

/// THE WAIT IS BOUNDED, AND ITS FALLBACK IS THE CALLER'S OWN CORRECTION. A
/// fold that never reaches the block — a wedged guest, a module whose feed
/// went quiet below the write, a node that is simply slow — must not hold a
/// page create open. The budget runs out and the answer is "no", which is what
/// puts `create_page`/`delete_page` back on the hand-correction they have
/// always carried.
#[tokio::test(flavor = "current_thread")]
async fn a_fold_that_never_arrives_gives_up_inside_its_budget() {
    let (origin, served) = node_scripting_its_fold_watermark(vec![Some("6:0")]).await;
    let rpc = rpc_client(&origin).expect("stub client");

    let arrived = await_fold(&rpc, "pages", &empty_pages_probe(), 9).await;

    assert!(!arrived, "the fold never reached block 9");
    assert_eq!(
        served.load(std::sync::atomic::Ordering::SeqCst),
        FOLD_WAIT_PROBES as usize,
        "bounded: it probes its budget and stops, never forever"
    );
}

/// ABSENT IS UNKNOWN, NOT "NOT YET". A module with no index guest, a fresh
/// database, or one a boundary stamp wiped reports no watermark at all —
/// forever. Reading that as "still folding" would spend the whole budget on
/// every write against such a module, so it stops on the FIRST reply.
#[tokio::test(flavor = "current_thread")]
async fn an_unstamped_reply_stops_the_wait_instead_of_spending_it() {
    let (origin, served) = node_scripting_its_fold_watermark(vec![None]).await;
    let rpc = rpc_client(&origin).expect("stub client");

    let arrived = await_fold(&rpc, "pages", &empty_pages_probe(), 9).await;

    assert!(!arrived);
    assert_eq!(
        served.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "an unknown watermark is answered once, never waited on"
    );
}

/// A READ WAITS FOR WHAT THIS CLIENT ALREADY KNOWS, AND FOR NOTHING ELSE.
///
/// The height a read owes itself is learned by whoever learned it — the write
/// that was signed, the op the stream delivered — and asked for at the read,
/// because the two are routinely different callers with several layers between
/// them. Three facts, and the middle one is the whole mechanism:
///
/// - a read with nothing outstanding pays NO probe, which is the ordinary page
///   open and the reason this costs the app's highest-frequency read nothing;
/// - a read behind a known block waits for it;
/// - once the fold is SEEN past that block the requirement is retired, because
///   the tip is monotonic and a later read cannot fall behind it again.
#[tokio::test(flavor = "current_thread")]
async fn a_read_waits_out_a_block_this_client_already_knows_about() {
    use std::sync::atomic::Ordering::SeqCst;
    let (origin, served) = node_scripting_its_fold_watermark(vec![Some("6:0"), Some("9:0")]).await;
    let rpc = rpc_client(&origin).expect("stub client");

    assert!(
        await_seen_fold(&rpc, "pages", &empty_pages_probe()).await,
        "nothing outstanding is not a stale read — it is nothing to wait for"
    );
    assert_eq!(served.load(SeqCst), 0, "and it costs no request at all");

    note_module_block(&rpc, "pages", 9);
    assert!(await_seen_fold(&rpc, "pages", &empty_pages_probe()).await);
    assert_eq!(
        served.load(SeqCst),
        2,
        "stale, then caught up: it waited for the fold to carry block 9"
    );

    assert!(await_seen_fold(&rpc, "pages", &empty_pages_probe()).await);
    assert_eq!(
        served.load(SeqCst),
        2,
        "the fold was observed past 9 — every later read is free"
    );
}

/// A WAIT THAT GAVE UP LEAVES THE NEXT READ STILL OWING IT — the autosave's
/// duplicate line, pinned.
///
/// The document tick reads the tree, diffs the buffer against it, writes, and
/// reads back. The NEXT tick reads again, and `document_plan` pairs the
/// disturbed middle POSITIONALLY: a tree still missing the line the previous
/// tick inserted is not merely stale, it makes the plan emit a second
/// `InsertBlock` for a line that is already on chain. So the requirement is
/// retired by an OBSERVED fold and by nothing else — never by the budget
/// running out, which is precisely the case where the read cannot be trusted.
#[tokio::test(flavor = "current_thread")]
async fn a_wait_that_gave_up_leaves_the_next_read_still_owing_it() {
    use std::sync::atomic::Ordering::SeqCst;
    let (origin, served) = node_scripting_its_fold_watermark(vec![Some("6:0")]).await;
    let rpc = rpc_client(&origin).expect("stub client");
    note_module_block(&rpc, "pages", 9);

    assert!(!await_seen_fold(&rpc, "pages", &empty_pages_probe()).await);
    assert_eq!(served.load(SeqCst), FOLD_WAIT_PROBES as usize);

    assert!(!await_seen_fold(&rpc, "pages", &empty_pages_probe()).await);
    assert_eq!(
        served.load(SeqCst),
        2 * FOLD_WAIT_PROBES as usize,
        "the next tick owes the same block: a spent budget is not a fold"
    );
}

/// AN OP THE STREAM DELIVERED IS WAITED OUT BY THE RELOAD BEHIND IT.
///
/// A push reports APPLICATION — the acceptance gap closed, the FOLD gap still
/// open, since the node's block loop writes the op feed and the index folds
/// behind it on its own runner. A pages op reaches the pages view as a plane
/// hit, and the `rpc.view` the view answers it with would otherwise read a
/// snapshot predating the push — the deleted line still there, the inserted
/// one missing — with no further op coming to correct it.
///
/// The height rides in the push, so it is recorded where the push is decoded
/// rather than threaded down through the update and the kernel's request.
#[tokio::test(flavor = "current_thread")]
async fn an_op_the_stream_delivered_is_waited_out_by_the_reload_behind_it() {
    let _names = crate::backend::seed_names(crate::backend::NameDirectory::empty());
    use std::sync::atomic::Ordering::SeqCst;
    let (origin, served) = node_scripting_its_fold_watermark(vec![Some("12:0")]).await;
    let rpc = rpc_client(&origin).expect("stub client");

    let moved = folded_update(
        &origin,
        "pages",
        ducktape_rpc::StreamOp {
            height: 12,
            seq: 0,
            time: 0,
            origin: ducktape_rpc::StreamOrigin {
                kind: ducktape_rpc::StreamOriginKind::External,
                id: None,
            },
            payload: Some(serde_json::json!({
                "move_block": { "block_id": "b1", "parent": "page", "after": null }
            })),
            payload_hex: None,
            assigned: None,
            assigned_hex: None,
        },
    )
    .await
    .expect("a pages op is visible to the shell");
    assert_eq!(
        moved.kind,
        crate::LiveKind::Plane,
        "a pages op is a plane hit: the view holding `rpc.live pages` is told"
    );
    assert_eq!(moved.module, "pages");
    assert_eq!(
        served.load(SeqCst),
        0,
        "recording the height is bookkeeping, not a request"
    );

    assert!(await_seen_fold(&rpc, "pages", &empty_pages_probe()).await);
    assert_eq!(
        served.load(SeqCst),
        1,
        "the reload waited for the fold to carry the pushed block"
    );
}

/// THE WAIT SITS BETWEEN A VIEW'S WRITE AND ITS NEXT READ, AND IT SITS IN THE
/// KERNEL. Every module view reads through `rpc.view`, so one placement covers
/// all of them — and it must come BEFORE the read, or it outlasts nothing. The
/// pages document save is the case that made it load-bearing: `document_plan`
/// pairs the disturbed middle POSITIONALLY, so a tree still missing the line
/// the previous tick inserted does not merely read stale, it emits a SECOND
/// insert for a line that is already on chain.
#[test]
fn the_kernels_view_read_waits_for_the_fold_before_it_reads() {
    const KERNEL: &str = include_str!("../../module_view/kernel.rs");
    let body = backend_fn(KERNEL, "fn view(");
    let wait = body
        .find("await_seen_fold(")
        .expect("`rpc.view` waits for the module's fold");
    let read = body.find(".view(&target").expect("it then reads");
    assert!(wait < read, "`rpc.view` waits BEFORE it reads");
}
