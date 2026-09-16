//! The view driven natively through the wire. The kernel pushes session
//! facts and nothing else; the repo namespace, one repo's branches and
//! tracker, one item with its patch and reviews, and the discussion are all
//! read HERE through `rpc.query` / `rpc.view`, re-read on every `rpc.live`
//! hit. A review and a merge leave as `op.submit`.

use ducktape_view_guest::testing::{answer, has_text, item, press, refuse, texts, type_into};
use ducktape_view_guest::wire::{Event, Frame, Node, Request};
use forge_view::host::Session;
use forge_view::{boot_native, tick_native};

fn node_ending(frame: &Frame, suffix: &str) -> Node {
    fn find(node: &Node, suffix: &str) -> Option<Node> {
        if node.key().is_some_and(|key| key.ends_with(suffix)) {
            return Some(node.clone());
        }
        node.children().iter().find_map(|child| find(child, suffix))
    }
    find(frame.root.as_ref().unwrap(), suffix).expect("node exists")
}

fn kinds(requests: &[Request]) -> Vec<&str> {
    requests
        .iter()
        .map(|request| request.kind.as_str())
        .collect()
}

fn request<'a>(frame: &'a Frame, kind: &str) -> &'a Request {
    frame
        .requests
        .iter()
        .find(|request| request.kind == kind)
        .unwrap_or_else(|| panic!("no `{kind}` request in {:?}", kinds(&frame.requests)))
}

/// Every node read in the frame, paired with the module-level ask it
/// carries — the tag is what says WHICH read this is.
fn reads(frame: &Frame) -> Vec<(u64, String)> {
    frame
        .requests
        .iter()
        .filter(|request| request.kind == "rpc.query" || request.kind == "rpc.view")
        .map(|request| {
            let ask: serde_json::Value =
                serde_json::from_slice(&request.payload).expect("an ask decodes");
            let tag = match &ask["query"] {
                serde_json::Value::String(word) => word.clone(),
                object => object
                    .as_object()
                    .and_then(|fields| fields.keys().next().cloned())
                    .unwrap_or_default(),
            };
            (request.id, tag)
        })
        .collect()
}

/// The view under the native driver, with every read it has asked for and
/// not yet been answered. A read the view opens in one frame is answered in
/// a later one, so the outstanding set — not one frame — is what a test
/// replies to.
struct Drive {
    frame: Frame,
    open: Vec<(u64, String)>,
    /// Every frame so far, for the reads they opened.
    frames: Vec<Frame>,
}

impl Drive {
    fn boot() -> Self {
        boot_native();
        let mut drive = Drive {
            frame: Frame::default(),
            open: Vec::new(),
            frames: Vec::new(),
        };
        drive.tick(Vec::new());
        drive
    }

    fn tick(&mut self, events: Vec<Event>) {
        self.frame = tick_native(events);
        self.open.extend(reads(&self.frame));
        self.frames.push(self.frame.clone());
    }

    /// The id of the outstanding read whose ask names `tag`, consumed.
    fn take(&mut self, tag: &str) -> u64 {
        let at = self
            .open
            .iter()
            .position(|(_, named)| named == tag)
            .unwrap_or_else(|| panic!("no open `{tag}` read in {:?}", self.open));
        self.open.remove(at).0
    }

    fn answer(&mut self, tag: &str, payload: &[u8]) {
        let id = self.take(tag);
        self.tick(vec![answer(id, payload)]);
    }

    /// Answer the newest outstanding tree read for `path` — the view opens
    /// one per (revision, directory), and a repo opens with a read at no
    /// revision whose subscription is gone by the time the head is known.
    fn answer_tree(&mut self, path: &str, payload: &[u8]) {
        let id = self
            .frames
            .iter()
            .flat_map(|frame| &frame.requests)
            .filter(|request| request.kind == "rpc.query")
            .filter(|request| self.open.iter().any(|(open, _)| *open == request.id))
            .filter_map(|request| {
                let ask: serde_json::Value = serde_json::from_slice(&request.payload).ok()?;
                (ask["query"]["tree"]["path"].as_str()? == path).then_some(request.id)
            })
            .next_back()
            .unwrap_or_else(|| panic!("no open tree read for {path:?} in {:?}", self.open));
        self.open.retain(|(open, _)| *open != id);
        self.tick(vec![answer(id, payload)]);
    }
}

fn session(link: &str) -> Vec<u8> {
    serde_json::to_vec(&Session {
        connected: true,
        dark: false,
        org: "duckhouse".into(),
        about: "a pond".into(),
        tier: "validator".into(),
        network_chain_id: "mynet#d0cdf950".into(),
        connected_rpc: "http://127.0.0.1:1".into(),
        link: link.into(),
        link_tick: i64::from(!link.is_empty()),
    })
    .expect("session encodes")
}

fn repos() -> Vec<u8> {
    serde_json::json!({ "repos": [{ "name": "core", "head": "1111222233334444" }] })
        .to_string()
        .into_bytes()
}

fn accounts() -> Vec<u8> {
    serde_json::json!({ "accounts": [
        { "number": 1, "name": "Mallard", "control": "key",
          "keys": [{ "pubkey": "aa" }] }
    ]})
    .to_string()
    .into_bytes()
}

fn refs() -> Vec<u8> {
    serde_json::json!({ "refs": [{ "name": "main", "head": "1111222233334444" }] })
        .to_string()
        .into_bytes()
}

fn items() -> Vec<u8> {
    serde_json::json!({ "items": [{
        "number": 7, "kind": "pr", "state": "open", "title": "Bound every list",
        "author": { "account": 1 }
    }]})
    .to_string()
    .into_bytes()
}

fn detail() -> Vec<u8> {
    serde_json::json!({ "item": {
        "number": 7, "kind": "pr", "state": "open", "title": "Bound every list",
        "author": { "account": 1 }, "body": "why this lands",
        "channel_id": "forge-core-7", "source_branch": "work",
        "target_branch": "main", "merge_oid": "", "reviews": []
    }})
    .to_string()
    .into_bytes()
}

fn pr_diff() -> Vec<u8> {
    serde_json::json!({ "pr_diff": {
        "source_oid": "aaaabbbbccccdddd", "target_oid": "1111222233334444",
        "patch": "--- a/main.rs\n+++ b/main.rs\n@@ -1 +1 @@\n-old\n+new\n",
        "truncated": false, "files_changed": 1, "additions": 1, "deletions": 1
    }})
    .to_string()
    .into_bytes()
}

/// Boots, hands the view a connected session, and answers the repo-list
/// read: the view with the namespace on screen, and the live id.
fn namespace(link: &str) -> (Drive, u64) {
    let mut drive = Drive::boot();
    let props = request(&drive.frame, "forge.props").id;
    drive.tick(vec![item(props, &session(link))]);
    let live = request(&drive.frame, "rpc.live").id;
    drive.answer("list_repos", &repos());
    (drive, live)
}

/// The open item on screen: the repo the link named, its tracker, and the
/// item with its patch and the name directory behind its author.
fn open_item(link: &str) -> Drive {
    let (mut drive, _) = namespace(link);
    // the repo: its refs, its tracker, and the roster the tracker's authors
    // are named through
    drive.answer("list_refs", &refs());
    drive.answer("list_items", &items());
    drive.answer("all", &accounts());
    // then the item the link parked, with its patch and its own name read
    drive.answer("get_item", &detail());
    drive.answer("pr_diff", &pr_diff());
    drive.answer("all", &accounts());
    drive
}

/// At boot the view asks the kernel for the session and nothing else; once
/// connected it subscribes to the forge plane and reads the repo namespace
/// itself.
#[test]
fn a_connected_view_reads_its_own_repo_namespace() {
    let drive = Drive::boot();
    assert_eq!(
        kinds(&drive.frame.requests),
        ["forge.props"],
        "only the session at boot: {:?}",
        kinds(&drive.frame.requests)
    );
    assert!(
        has_text(&drive.frame, "Not connected"),
        "{:?}",
        texts(&drive.frame)
    );

    let (drive, _live) = namespace("");
    assert!(has_text(&drive.frame, "core"), "{:?}", texts(&drive.frame));
    assert!(
        has_text(&drive.frame, "duckhouse"),
        "{:?}",
        texts(&drive.frame)
    );
}

/// A forge block moves the live subscription, and the view re-reads exactly
/// what it has open — here the namespace.
#[test]
fn a_live_hit_re_reads_what_is_open() {
    let (mut drive, live) = namespace("");
    drive.tick(vec![item(live, b"{}")]);
    assert_eq!(
        reads(&drive.frame)
            .into_iter()
            .map(|(_, tag)| tag)
            .collect::<Vec<_>>(),
        ["list_repos"],
        "{:?}",
        kinds(&drive.frame.requests)
    );
}

/// A refused read is said on the screen in the kernel's own words, not
/// swallowed into a blank listing.
#[test]
fn a_refused_read_is_shown_where_the_listing_would_be() {
    let mut drive = Drive::boot();
    let props = request(&drive.frame, "forge.props").id;
    drive.tick(vec![item(props, &session(""))]);
    let repo_list = drive.take("list_repos");
    drive.tick(vec![refuse(repo_list, "the node is not reachable")]);
    assert!(
        has_text(
            &drive.frame,
            "Could not read the repositories: the node is not reachable"
        ),
        "{:?}",
        texts(&drive.frame)
    );
}

/// A refused file read is a sentence in the file pane, never a blank
/// danger strip: the loader's note is empty on a refusal, so the refusal
/// itself is the line.
#[test]
fn a_refused_file_read_says_so_in_the_file_pane() {
    let (mut drive, _) = namespace("duck://forge/core/blob/main.rs");
    drive.answer("list_refs", &refs());
    drive.answer("list_items", &items());
    drive.answer("all", &accounts());
    let tree = serde_json::json!({ "tree": {
        "rev": "1111222233334444", "born": true, "truncated": false,
        "entries": [{ "name": "main.rs", "path": "main.rs", "kind": "file" }]
    }});
    drive.answer("tree", tree.to_string().as_bytes());
    // the file opens at the commit the tree answered with — one blob read
    assert!(
        has_text(&drive.frame, "Loading file…"),
        "{:?}",
        texts(&drive.frame)
    );
    let blob = drive.take("blob");
    drive.tick(vec![refuse(blob, "object missing")]);
    assert!(
        has_text(&drive.frame, "Could not load this file: object missing"),
        "{:?}",
        texts(&drive.frame)
    );
}

/// The patch is drawn per file — one row naming the file, then its hunks —
/// never git's own `---`/`+++`/`index` bookkeeping lines.
#[test]
fn a_patch_opens_each_file_with_one_named_row() {
    let drive = open_item("duck://forge/core/7");
    let shown = texts(&drive.frame);
    assert!(shown.iter().any(|text| text == "main.rs"), "{shown:?}");
    assert!(
        !shown
            .iter()
            .any(|text| text.starts_with("--- ") || text.starts_with("+++ ")),
        "{shown:?}"
    );
    assert!(
        shown.iter().any(|text| text == "1 file, +1 −1"),
        "{shown:?}"
    );
}

/// A refused patch read leaves the merge and review doors shut, and the
/// Changes section says why instead of vanishing.
#[test]
fn a_refused_patch_read_is_said_under_changes() {
    let (mut drive, _) = namespace("duck://forge/core/7");
    drive.answer("list_refs", &refs());
    drive.answer("list_items", &items());
    drive.answer("all", &accounts());
    drive.answer("get_item", &detail());
    let diff = drive.take("pr_diff");
    drive.tick(vec![refuse(diff, "the pack is gone")]);
    drive.answer("all", &accounts());
    assert!(
        has_text(
            &drive.frame,
            "The changes could not be loaded. Open the pull request again to retry."
        ),
        "{:?}",
        texts(&drive.frame)
    );
}

/// A `duck://forge/<repo>/<n>` the app routed here opens the repo AND
/// its item without the app holding either: the view parses the address and
/// makes both reads itself. The author's display name is one of them — the
/// identity roster, read the way the app reads it.
#[test]
fn a_routed_link_opens_the_item_it_names() {
    let drive = open_item("duck://forge/core/7");
    for expected in ["Bound every list", "work into main", "Mallard"] {
        assert!(
            has_text(&drive.frame, expected),
            "missing {expected:?} in {:?}",
            texts(&drive.frame)
        );
    }
}

/// Submitting a review leaves as one `op.submit` carrying the forge message
/// the module's wire names, signed by the kernel with the seated key.
#[test]
fn a_review_leaves_as_a_signed_op() {
    let mut drive = open_item("duck://forge/core/7");
    let typed = type_into(&drive.frame, "Leave a review…", "reads well");
    drive.tick(typed);
    let events = press(&drive.frame, "Submit review");
    drive.tick(events);
    let submit = request(&drive.frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(op["target"], "forge");
    let review = &op["payload"]["submit_review"];
    assert_eq!(review["repo"], "core");
    assert_eq!(review["number"], 7);
    assert_eq!(review["verdict"], "comment");
    assert_eq!(review["body"], "reads well");
    assert_eq!(review["commit_oid"], "aaaabbbbccccdddd");
}

/// The deployment asset selects the service; its merge reply supplies the
/// pack for the guest's signed operation with both expected branch heads.
#[test]
fn a_merge_uses_its_deployed_service_then_submits_the_commit() {
    let mut drive = open_item("duck://forge/core/7");
    let events = press(&drive.frame, "Merge pull request");
    drive.tick(events);
    let config = request(&drive.frame, "asset.read");
    assert_eq!(config.payload, b"service.json");
    drive.tick(vec![answer(config.id, br#"{"account":42,"route":"git"}"#)]);
    let build = request(&drive.frame, "net.request");
    let envelope: serde_json::Value =
        serde_json::from_slice(&build.payload).expect("an ask decodes");
    assert_eq!(envelope["account"], 42);
    assert_eq!(envelope["route"], "git");
    assert_eq!(envelope["path"], "/merge");
    let body: Vec<u8> = serde_json::from_value(envelope["body"].clone()).unwrap();
    let ask: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(ask["repo"], "core");
    assert_eq!(ask["ours"], "1111222233334444", "the target tip");
    assert_eq!(ask["theirs"], "aaaabbbbccccdddd", "the source tip");

    let built = serde_json::json!({ "merge_oid": "99998888", "pack_b64": "UEFDSw==" });
    drive.tick(vec![answer(build.id, &service_reply(built))]);
    let upload = request(&drive.frame, "blob.put");
    assert_eq!(upload.payload, b"PACK");
    drive.tick(vec![answer(upload.id, "de".repeat(32).as_bytes())]);
    let submit = request(&drive.frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(op["required_blob"], "de".repeat(32));
    assert_eq!(
        op["payload"]["merge_pr"],
        serde_json::json!({
            "repo": "core", "number": 7,
            "prev_target_oid": "1111222233334444",
            "expected_source_oid": "aaaabbbbccccdddd",
            "merge_oid": "99998888", "pack_digest": "de".repeat(32)
        })
    );
}

/// A conflict the builder found writes NOTHING: the paths come back to the
/// screen and no op is submitted over a merge that does not exist.
#[test]
fn a_conflicting_merge_submits_nothing() {
    let mut drive = open_item("duck://forge/core/7");
    let events = press(&drive.frame, "Merge pull request");
    drive.tick(events);
    let config = request(&drive.frame, "asset.read").id;
    drive.tick(vec![answer(config, br#"{"account":42,"route":"git"}"#)]);
    let build = request(&drive.frame, "net.request").id;
    let conflicts = serde_json::json!({ "conflicts": ["main.rs"] });
    drive.tick(vec![answer(build, &service_reply(conflicts))]);
    assert!(
        !drive
            .frame
            .requests
            .iter()
            .any(|request| request.kind == "op.submit"),
        "{:?}",
        kinds(&drive.frame.requests)
    );
    assert!(
        has_text(
            &drive.frame,
            "Merge conflicts — resolve on the branch and push again:"
        ),
        "{:?}",
        texts(&drive.frame)
    );
}

#[test]
fn the_repository_tree_width_is_the_readers_and_its_edge_has_a_resize_cursor() {
    use ducktape_view_guest::wire::{Length, mouse};

    let (drive, _) = namespace("duck://forge/core");
    let width = |frame: &Frame| match node_ending(frame, "/tree-pane") {
        Node::Container {
            width: Some(Length::Fixed(width)),
            ..
        } => width,
        node => panic!("fixed tree pane: {node:?}"),
    };
    let Node::ResizeHandle {
        on_drag: Some(handler),
        cursor,
        ..
    } = node_ending(&drive.frame, "/tree-resize")
    else {
        panic!("tree resize handle")
    };
    assert_eq!(cursor, Some(mouse::Cursor::ResizingHorizontally));
    assert_eq!(width(&drive.frame), 260.0);
    let frame = tick_native(vec![Event::Drag {
        handler,
        dx: 42.0,
        dy: 0.0,
    }]);
    assert_eq!(width(&frame), 302.0);
}

/// The directories every tree read so far asked for, in order; a read opens
/// in one frame and is answered in a later one, so the history is what a
/// test checks.
fn tree_asks(frames: &[Frame]) -> Vec<String> {
    frames
        .iter()
        .flat_map(|frame| &frame.requests)
        .filter(|request| request.kind == "rpc.query")
        .filter_map(|request| {
            let ask: serde_json::Value = serde_json::from_slice(&request.payload).ok()?;
            Some(ask["query"]["tree"]["path"].as_str()?.to_owned())
        })
        .collect()
}

fn listing(entries: &[(&str, &str)]) -> Vec<u8> {
    let entries: Vec<_> = entries
        .iter()
        .map(|(path, kind)| {
            let name = path.rsplit('/').next().unwrap();
            serde_json::json!({ "name": name, "path": path, "kind": kind })
        })
        .collect();
    serde_json::json!({ "tree": {
        "rev": "1111222233334444", "born": true, "truncated": false, "entries": entries
    }})
    .to_string()
    .into_bytes()
}

fn row_inset(frame: &Frame, suffix: &str) -> f32 {
    match node_ending(frame, suffix) {
        Node::Button {
            padding: Some(edges),
            ..
        } => edges.left,
        node => panic!("padded tree row: {node:?}"),
    }
}

fn has_key_ending(frame: &Frame, suffix: &str) -> bool {
    fn walk(node: &Node, suffix: &str) -> bool {
        node.key().is_some_and(|key| key.ends_with(suffix))
            || node.children().iter().any(|child| walk(child, suffix))
    }
    walk(frame.root.as_ref().unwrap(), suffix)
}

/// The tree unfolds in place: pressing a directory reads its listing and
/// paints its rows under it, one step further in; pressing it again folds
/// them without another read.
#[test]
fn a_directory_unfolds_under_its_row_and_folds_again() {
    let (mut drive, _) = namespace("duck://forge/core");
    drive.answer("list_refs", &refs());
    drive.answer("list_items", &items());
    drive.answer("all", &accounts());
    assert_eq!(
        tree_asks(&drive.frames).last().map(String::as_str),
        Some("")
    );
    drive.answer_tree("", &listing(&[("src", "dir"), ("README.md", "file")]));
    assert!(!has_key_ending(&drive.frame, "forge/tree-root"));
    drive.tick(press(&drive.frame, "src"));
    let asked = tree_asks(&drive.frames);
    assert_eq!(asked.last().map(String::as_str), Some("src"));
    assert!(has_text(&drive.frame, "Loading…"));
    drive.answer_tree("src", &listing(&[("src/main.rs", "file")]));
    assert_eq!(
        row_inset(&drive.frame, "forge/tree/src/main.rs"),
        row_inset(&drive.frame, "forge/tree/src") + 14.
    );
    drive.tick(press(&drive.frame, "src"));
    assert!(!has_key_ending(&drive.frame, "forge/tree/src/main.rs"));
    assert_eq!(tree_asks(&drive.frames), asked, "a fold reads nothing");
}

/// A link into a file reads that file's directory first, then the root, and
/// leaves the tree unfolded down to the file.
#[test]
fn a_file_link_unfolds_the_tree_down_to_it() {
    let (mut drive, _) = namespace("duck://forge/core/blob/src/main.rs");
    drive.answer("list_refs", &refs());
    drive.answer("list_items", &items());
    drive.answer("all", &accounts());
    assert_eq!(
        tree_asks(&drive.frames).last().map(String::as_str),
        Some("src")
    );
    drive.answer_tree("src", &listing(&[("src/main.rs", "file")]));
    assert_eq!(
        tree_asks(&drive.frames).last().map(String::as_str),
        Some("")
    );
    drive.answer_tree("", &listing(&[("src", "dir")]));
    assert!(
        has_key_ending(&drive.frame, "forge/tree/src/main.rs"),
        "asks {:?} open {:?} texts {:?}",
        tree_asks(&drive.frames),
        drive.open,
        texts(&drive.frame)
    );
    assert!(has_text(&drive.frame, "Loading file…"));
}

#[test]
fn a_diff_line_comment_keeps_its_anchor_and_submits_without_a_review_body() {
    let mut drive = open_item("duck://forge/core/7");
    drive.tick(press(&drive.frame, "Comment on this line"));
    drive.tick(type_into(
        &drive.frame,
        "Comment on this line…",
        "Keep this guard",
    ));
    drive.tick(press(&drive.frame, "Add comment"));
    assert!(has_text(&drive.frame, "Not sent yet"));
    assert!(has_text(&drive.frame, "Keep this guard"));
    drive.tick(press(&drive.frame, "Pick approve verdict"));
    drive.tick(press(&drive.frame, "Submit review"));
    let op: serde_json::Value =
        serde_json::from_slice(&request(&drive.frame, "op.submit").payload).unwrap();
    let review = &op["payload"]["submit_review"];
    assert_eq!(review["verdict"], "approve");
    assert_eq!(review["body"], "");
    assert_eq!(review["comments"][0]["path"], "main.rs");
    assert_eq!(review["comments"][0]["line"], 1);
    assert_eq!(review["comments"][0]["body"], "Keep this guard");
}

fn service_reply(body: serde_json::Value) -> Vec<u8> {
    use base64::Engine as _;
    serde_json::to_vec(&serde_json::json!({"head":{"status":200,"headers":[]},"body_b64":base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&body).unwrap())})).unwrap()
}

#[test]
fn a_merge_without_its_deployment_service_asset_is_refused() {
    let mut drive = open_item("duck://forge/core/7");
    let events = press(&drive.frame, "Merge pull request");
    drive.tick(events);
    let config = request(&drive.frame, "asset.read").id;
    drive.tick(vec![refuse(config, "missing deployment asset")]);
    assert!(has_text(
        &drive.frame,
        "The merge did not go through: Forge merge service is not configured: missing deployment asset"
    ));
    assert!(
        !drive
            .frame
            .requests
            .iter()
            .any(|request| matches!(request.kind.as_str(), "net.request" | "op.submit"))
    );
}
