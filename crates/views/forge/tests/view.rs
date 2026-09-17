//! The view driven natively through the wire. The kernel pushes session
//! facts and nothing else; the repo namespace, one repo's branches and
//! tracker, one item with its patch and reviews, and the discussion are all
//! read HERE through `rpc.query` / `rpc.view`, re-read on every `rpc.live`
//! hit. Opening an issue, a review and a merge leave as `op.submit`.

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

/// Every button in the frame, by the name a press would find it under —
/// its accessible label, else its key. A row that offers a control is in
/// here; a row drawn as text is not.
fn buttons(frame: &Frame) -> Vec<String> {
    fn walk(node: &Node, found: &mut Vec<String>) {
        if let Node::Button { key, label, .. } = node {
            found.push(label.clone().unwrap_or_else(|| key.clone()));
        }
        for child in node.children() {
            walk(child, found);
        }
    }
    let mut found = Vec::new();
    if let Some(root) = frame.root.as_ref() {
        walk(root, &mut found);
    }
    found
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

    /// Answer the NEWEST outstanding read whose ask names `tag`. An item
    /// left and opened again asks the same read twice, and the one its
    /// screen is waiting on is the last.
    fn answer_latest(&mut self, tag: &str, payload: &[u8]) {
        let id = *self
            .open
            .iter()
            .filter(|(_, named)| named == tag)
            .map(|(id, _)| id)
            .next_back()
            .unwrap_or_else(|| panic!("no open `{tag}` read in {:?}", self.open));
        self.open.retain(|(open, _)| *open != id);
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

/// A patch that changes one text file and one BINARY, as git writes one: a
/// binary delta carries no `---`/`+++` pair and no hunks, only the line
/// saying the two sides differ. Every pull request the closed loop opens
/// looks like this — a rebuilt `component.wasm` beside its source.
fn pr_diff_with_binary() -> Vec<u8> {
    serde_json::json!({ "pr_diff": {
        "source_oid": "aaaabbbbccccdddd", "target_oid": "1111222233334444",
        "patch": concat!(
            "diff --git a/main.rs b/main.rs\n",
            "index 94c9d14..c2e5965 100644\n",
            "--- a/main.rs\n+++ b/main.rs\n@@ -1 +1 @@\n-old\n+new\n",
            "diff --git a/logo.png b/logo.png\n",
            "index 22a565b..f34c953 100644\n",
            "Binary files a/logo.png and b/logo.png differ\n",
        ),
        "truncated": false, "files_changed": 2, "additions": 1, "deletions": 1
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

/// The files screen of a pull request that carries a binary.
fn open_files_with_binary(link: &str) -> Drive {
    let (mut drive, _) = namespace(link);
    drive.answer("list_refs", &refs());
    drive.answer("list_items", &items());
    drive.answer("all", &accounts());
    drive.answer("get_item", &detail());
    drive.answer("pr_diff", &pr_diff_with_binary());
    drive.answer("all", &accounts());
    drive.tick(press(&drive.frame, "forge/item-tab/files"));
    drive
}

/// The open item's FILES screen: a pull request's changes, and the review
/// composed over them, are one tab press from its conversation.
fn open_files(link: &str) -> Drive {
    let mut drive = open_item(link);
    drive.tick(press(&drive.frame, "forge/item-tab/files"));
    drive
}

fn issue_detail() -> Vec<u8> {
    serde_json::json!({ "item": {
        "number": 8, "kind": "issue", "state": "open", "title": "the pond is cold",
        "author": { "account": 1 }, "body": "every morning",
        "channel_id": "forge-core-8", "source_branch": "",
        "target_branch": "", "merge_oid": "", "reviews": []
    }})
    .to_string()
    .into_bytes()
}

/// An open ISSUE: no patch is read for one, so its screen is the detail and
/// the roster its author is named through.
fn open_issue(link: &str) -> Drive {
    let (mut drive, _) = namespace(link);
    drive.answer("list_refs", &refs());
    drive.answer("list_items", &items());
    drive.answer("all", &accounts());
    drive.answer("get_item", &issue_detail());
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

/// AN ITEM HAS TWO SCREENS AND OPENS ON THE FIRST. What a pull request says
/// — its body, its merge box, its reviews and the discussion — is its
/// conversation; the patch is a screen of its own, one press away, and the
/// press is the only thing that moves between them. Nothing is read again:
/// both screens are drawn from the reads the item already made.
#[test]
fn an_item_opens_on_its_conversation_with_its_files_one_press_away() {
    let mut drive = open_item("duck://forge/core/7");
    assert!(
        has_text(&drive.frame, "why this lands"),
        "the body is on the conversation: {:?}",
        texts(&drive.frame)
    );
    assert!(
        has_text(&drive.frame, "Discussion"),
        "and so is the discussion: {:?}",
        texts(&drive.frame)
    );
    assert!(
        !has_text(&drive.frame, "@@ -1 +1 @@"),
        "the patch is not: {:?}",
        texts(&drive.frame)
    );
    // the strip wears the count the patch reported
    assert!(
        has_text(&drive.frame, "Files changed 1"),
        "{:?}",
        texts(&drive.frame)
    );

    drive.tick(press(&drive.frame, "forge/item-tab/files"));
    assert!(
        reads(&drive.frame).is_empty(),
        "a tab press reads nothing: {:?}",
        kinds(&drive.frame.requests)
    );
    assert!(
        has_text(&drive.frame, "@@ -1 +1 @@"),
        "the patch is on the files screen: {:?}",
        texts(&drive.frame)
    );
    assert!(
        !has_text(&drive.frame, "Discussion"),
        "the conversation is not: {:?}",
        texts(&drive.frame)
    );
    // the review is composed here, beside the lines it anchors to
    assert!(
        has_text(&drive.frame, "Submit review"),
        "{:?}",
        texts(&drive.frame)
    );

    drive.tick(press(&drive.frame, "forge/item-tab/conversation"));
    assert!(
        has_text(&drive.frame, "Discussion"),
        "and the press goes back: {:?}",
        texts(&drive.frame)
    );
}

/// The merge box stays where a pull request is READ, not where its lines
/// are: a reader deciding whether to merge is on the conversation.
#[test]
fn the_merge_box_and_the_reviews_stay_on_the_conversation() {
    let drive = open_item("duck://forge/core/7");
    for expected in ["Merge pull request", "Reviews"] {
        assert!(
            has_text(&drive.frame, expected),
            "missing {expected:?} in {:?}",
            texts(&drive.frame)
        );
    }
    let files = open_files("duck://forge/core/7");
    for absent in ["Merge pull request", "Reviews"] {
        assert!(
            !has_text(&files.frame, absent),
            "{absent:?} followed the patch: {:?}",
            texts(&files.frame)
        );
    }
}

/// An ISSUE has one screen. It changes no files, so it wears no strip —
/// a tab that can only ever be empty is a tab that lies.
#[test]
fn an_issue_wears_no_files_tab() {
    let drive = open_issue("duck://forge/core/8");
    assert!(
        has_text(&drive.frame, "the pond is cold"),
        "the issue is open: {:?}",
        texts(&drive.frame)
    );
    for absent in ["Conversation", "Files changed"] {
        assert!(
            !has_text(&drive.frame, absent),
            "{absent:?} is on an issue: {:?}",
            texts(&drive.frame)
        );
    }
}

/// The screen is not a preference that outlives the item. Leaving a pull
/// request from its files and opening it again lands on the conversation,
/// which is where a reader starts.
#[test]
fn leaving_an_item_forgets_which_screen_it_was_left_on() {
    let mut drive = open_files("duck://forge/core/7");
    assert!(has_text(&drive.frame, "@@ -1 +1 @@"));
    drive.tick(press(&drive.frame, "Back to tracker"));
    drive.tick(press(&drive.frame, "Bound every list"));
    drive.answer_latest("get_item", &detail());
    drive.answer_latest("pr_diff", &pr_diff());
    drive.answer_latest("all", &accounts());
    assert!(
        has_text(&drive.frame, "Discussion"),
        "the item opens on its conversation: {:?}",
        texts(&drive.frame)
    );
}

/// The patch is drawn per file — one row naming the file, then its hunks —
/// never git's own `---`/`+++`/`index` bookkeeping lines.
#[test]
fn a_patch_opens_each_file_with_one_named_row() {
    let drive = open_files("duck://forge/core/7");
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

/// The MODULE'S WHOLE SENTENCE REACHES THE SCREEN. The host hands a view a
/// refusal already split — a token to branch on, the refusing module's own
/// words to show — so there is no transport envelope to peel here and no
/// clipping that could drop the ending. This is the real refusal a 9 MiB
/// patch draws, ending included: if anything ever wraps a refusal in a status
/// line and JSON again, this fails instead of the view quietly re-growing a
/// parser for it.
#[test]
fn the_whole_sentence_of_a_long_refusal_reaches_the_screen() {
    let said = concat!(
        "forge: pull request #7 diff is too large to serve ",
        "(target 8b4ba7efd7caa4e4f3d8106ff11579d26df4c0ab, ",
        "source 564ea02b5b094f225ebab8fcf09a1a782d24fffb): ",
        "diff is too large: 1 changed files / 8388609 materialized blob bytes",
    );
    let (mut drive, _) = namespace("duck://forge/core/7");
    drive.answer("list_refs", &refs());
    drive.answer("list_items", &items());
    drive.answer("all", &accounts());
    drive.answer("get_item", &detail());
    let diff = drive.take("pr_diff");
    drive.tick(vec![refuse(diff, said)]);
    drive.answer("all", &accounts());
    drive.tick(press(&drive.frame, "forge/item-tab/files"));
    let shown = texts(&drive.frame);
    assert!(
        shown
            .iter()
            .any(|text| text == &format!("These changes cannot be shown: {said}")),
        "{shown:?}"
    );
}

/// A CHANGED BINARY IS A ROW, NOT A CONTROL. The patch says only that the
/// two sides differ, so the row names the file, says it is not shown, and
/// offers nothing to press: no fold (there are no hunks to put away) and no
/// line to comment on. Git's own `diff --git` and `index` bookkeeping never
/// reaches the screen either.
#[test]
fn a_changed_binary_is_named_and_offers_nothing_to_press() {
    let drive = open_files_with_binary("duck://forge/core/7");
    let shown = texts(&drive.frame);
    assert!(
        shown
            .iter()
            .any(|text| text == "logo.png (binary file, not shown)"),
        "{shown:?}"
    );
    assert!(
        !shown
            .iter()
            .any(|text| text.starts_with("diff --git") || text.starts_with("index ")),
        "git's bookkeeping is not a row: {shown:?}"
    );
    let pressable = buttons(&drive.frame);
    assert!(
        pressable.iter().any(|name| name == "main.rs"),
        "a text file's header still folds: {pressable:?}"
    );
    assert!(
        !pressable.iter().any(|name| name.contains("logo.png")),
        "nothing about the binary is pressable: {pressable:?}"
    );
}

/// Folding the text file beside a binary puts ITS hunks away and leaves the
/// binary's row where it was: each header opens its own block, so a binary
/// cannot end up hidden inside the file above it.
#[test]
fn folding_a_text_file_leaves_the_binary_row_alone() {
    let mut drive = open_files_with_binary("duck://forge/core/7");
    assert!(has_text(&drive.frame, "@@ -1 +1 @@"));
    drive.tick(press(&drive.frame, "main.rs"));
    assert!(
        !has_text(&drive.frame, "@@ -1 +1 @@"),
        "the hunks are away: {:?}",
        texts(&drive.frame)
    );
    assert!(
        has_text(&drive.frame, "logo.png (binary file, not shown)"),
        "the binary is still listed: {:?}",
        texts(&drive.frame)
    );
}

/// A refused patch read leaves the merge and review doors shut, and the
/// Changes section says why IN THE MODULE'S OWN WORDS — a diff over a size
/// ceiling never becomes readable, so a reader who is told to retry is being
/// sent back for nothing. The tally is not drawn at all: the zeroes a
/// missing reply decodes to would claim the pull request changes nothing.
#[test]
fn a_refused_patch_read_is_said_under_changes() {
    let (mut drive, _) = namespace("duck://forge/core/7");
    drive.answer("list_refs", &refs());
    drive.answer("list_items", &items());
    drive.answer("all", &accounts());
    drive.answer("get_item", &detail());
    let diff = drive.take("pr_diff");
    drive.tick(vec![refuse(
        diff,
        "forge: pull request #7 diff is too large to serve",
    )]);
    drive.answer("all", &accounts());
    drive.tick(press(&drive.frame, "forge/item-tab/files"));
    let shown = texts(&drive.frame);
    assert!(
        shown.iter().any(|text| text
            == "These changes cannot be shown: forge: pull request #7 diff is too large to serve"),
        "{shown:?}"
    );
    assert!(
        !shown.iter().any(|text| text.starts_with("0 files")),
        "a refused patch has no tally to draw: {shown:?}"
    );
    assert!(
        !shown.iter().any(|text| text.contains("retry")),
        "nothing promises a retry that cannot succeed: {shown:?}"
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
    let mut drive = open_files("duck://forge/core/7");
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
    let mut drive = open_files("duck://forge/core/7");
    // the mark reads `+` and carries the line it belongs to as its name
    drive.tick(press(&drive.frame, "main.rs:1"));
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

/// The tracker with the Issues tab open: the repo the link named, its
/// refs, its items and the roster behind their authors.
fn open_issues_tab() -> Drive {
    let (mut drive, _) = namespace("duck://forge/core");
    drive.answer("list_refs", &refs());
    drive.answer("list_items", &items());
    drive.answer("all", &accounts());
    drive.tick(press(&drive.frame, "Issues"));
    drive
}

/// A forge block, as every read the view holds sees it: each subscription
/// opens its OWN `rpc.live` on the plane, so one block is a hit on each.
fn forge_block(drive: &Drive) -> Vec<Event> {
    let mut ids: Vec<u64> = Vec::new();
    for request in drive.frames.iter().flat_map(|frame| &frame.requests) {
        if request.kind == "rpc.live" && !ids.contains(&request.id) {
            ids.push(request.id);
        }
    }
    ids.into_iter().map(|id| item(id, b"{}")).collect()
}

/// The value the input under `suffix` is carrying.
fn input_value(frame: &Frame, suffix: &str) -> String {
    let Node::Input { value, .. } = node_ending(frame, suffix) else {
        panic!("{suffix} is not an input");
    };
    value
}

/// Opening an issue leaves as one `op.submit` carrying the module's own
/// `open_issue`, signed by the kernel with the seated key. The block that
/// lands it moves the live subscription, so the tracker re-reads itself and
/// the issue is on the list without a restart.
#[test]
fn an_issue_opens_from_the_tracker_and_the_live_hit_lists_it() {
    let mut drive = open_issues_tab();
    drive.tick(type_into(&drive.frame, "Title", "the pond is cold"));
    drive.tick(type_into(&drive.frame, "Describe it…", "every morning"));
    drive.tick(press(&drive.frame, "Open issue"));

    let submit = request(&drive.frame, "op.submit");
    let op: serde_json::Value = serde_json::from_slice(&submit.payload).expect("an op decodes");
    assert_eq!(op["target"], "forge");
    assert_eq!(
        op["payload"]["open_issue"],
        serde_json::json!({
            "repo": "core", "title": "the pond is cold", "body": "every morning"
        })
    );

    // the write lands: the composer empties, and the forge plane's live hit
    // re-reads the tracker the issue is now in.
    let id = submit.id;
    drive.tick(vec![answer(id, b"{}")]);
    assert_eq!(input_value(&drive.frame, "new-issue-title"), "");
    assert_eq!(input_value(&drive.frame, "new-issue-body"), "");
    let block = forge_block(&drive);
    drive.tick(block);
    // the tracker re-reads itself in the order `read_repo` asks: the refs,
    // then the items the issue is now one of.
    drive.answer("list_refs", &refs());
    let listed = serde_json::json!({ "items": [{
        "number": 8, "kind": "issue", "state": "open", "title": "the pond is cold",
        "author": { "account": 1 }
    }]});
    drive.answer("list_items", listed.to_string().as_bytes());
    drive.answer("all", &accounts());
    assert!(
        has_text(&drive.frame, "the pond is cold"),
        "{:?}",
        texts(&drive.frame)
    );
}

/// Escape is the way out of every forge screen, and a started issue is a
/// screen too: the draft goes first, and only a second press leaves the
/// repository. One press that did both would take the words with it.
#[test]
fn escape_clears_a_started_issue_before_it_leaves_the_repository() {
    let mut drive = open_issues_tab();
    drive.tick(type_into(&drive.frame, "Title", "the pond is cold"));
    drive.tick(key_press("Escape"));
    assert_eq!(input_value(&drive.frame, "new-issue-title"), "");
    assert!(
        has_text(&drive.frame, "Issues"),
        "the repository stays open: {:?}",
        texts(&drive.frame)
    );
    drive.tick(key_press("Escape"));
    assert!(
        has_text(&drive.frame, "duckhouse"),
        "the namespace is back: {:?}",
        texts(&drive.frame)
    );
}

/// The module refuses a blank title, so the composer never spends a
/// transaction on one: the button is dead until a title is typed.
#[test]
fn a_blank_title_opens_nothing() {
    let mut drive = open_issues_tab();
    let Node::Button { on_press, .. } = node_ending(&drive.frame, "open-issue") else {
        panic!("no open-issue button");
    };
    assert!(on_press.is_none(), "{:?}", texts(&drive.frame));
    drive.tick(type_into(&drive.frame, "Title", "   "));
    let Node::Button { on_press, .. } = node_ending(&drive.frame, "open-issue") else {
        panic!("no open-issue button");
    };
    assert!(on_press.is_none(), "{:?}", texts(&drive.frame));
    assert!(
        !drive
            .frame
            .requests
            .iter()
            .any(|request| request.kind == "op.submit"),
        "{:?}",
        kinds(&drive.frame.requests)
    );
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

/// The surface a file is drawn on, by name — `code`, `markdown`, `picture`.
fn surface_named(frame: &Frame) -> String {
    match node_ending(frame, "forge/file-text") {
        Node::Surface { name, .. } => name,
        node => panic!("the reader draws a surface: {node:?}"),
    }
}

/// A repository opens on what it says about itself: the root listing's
/// README is read and drawn without anyone pressing anything, the way every
/// forge lands.
#[test]
fn a_repository_opens_on_its_readme() {
    let (mut drive, _) = namespace("duck://forge/core");
    drive.answer("list_refs", &refs());
    drive.answer("list_items", &items());
    drive.answer("all", &accounts());
    drive.answer_tree(
        "",
        &listing(&[
            ("LICENSE", "file"),
            ("README.md", "file"),
            ("src", "dir"),
        ]),
    );
    assert!(
        has_text(&drive.frame, "Loading file…"),
        "the README is read on landing: {:?}",
        texts(&drive.frame)
    );
    let blob = drive.take("blob");
    drive.tick(vec![answer(
        blob,
        serde_json::json!({ "blob": {
            "path": "README.md", "rev": "1111222233334444",
            "text": "# core\n\nwhat this repository is", "binary": false, "truncated": false
        }})
        .to_string()
        .as_bytes(),
    )]);
    // a markdown body parks its inline pictures with the host before it
    // reaches the reader
    let parked = request(&drive.frame, "picture.inline").id;
    drive.tick(vec![answer(parked, b"{}")]);
    assert_eq!(surface_named(&drive.frame), "markdown");
    assert!(has_text(&drive.frame, "README.md"), "{:?}", texts(&drive.frame));
}

/// A repository without a README leaves the reader's own empty state on
/// screen — the landing never invents a file to open.
#[test]
fn a_repository_without_a_readme_lands_on_its_empty_state() {
    let (mut drive, _) = namespace("duck://forge/core");
    drive.answer("list_refs", &refs());
    drive.answer("list_items", &items());
    drive.answer("all", &accounts());
    drive.answer_tree("", &listing(&[("src", "dir")]));
    assert!(
        has_text(&drive.frame, "Choose a file from the tree."),
        "{:?}",
        texts(&drive.frame)
    );
    assert!(
        !drive.open.iter().any(|(_, tag)| tag == "blob"),
        "nothing was read: {:?}",
        drive.open
    );
}

/// A link into a file beats the landing README: the reader holds what the
/// link named, and the README is never read over it.
#[test]
fn a_deep_link_beats_the_landing_readme() {
    let (mut drive, _) = namespace("duck://forge/core/blob/src/main.rs");
    drive.answer("list_refs", &refs());
    drive.answer("list_items", &items());
    drive.answer("all", &accounts());
    drive.answer_tree("src", &listing(&[("src/main.rs", "file")]));
    drive.answer_tree("", &listing(&[("README.md", "file"), ("src", "dir")]));
    let blob = drive.take("blob");
    drive.tick(vec![answer(
        blob,
        serde_json::json!({ "blob": {
            "path": "src/main.rs", "rev": "1111222233334444",
            "text": "fn main() {}", "binary": false, "truncated": false
        }})
        .to_string()
        .as_bytes(),
    )]);
    assert_eq!(surface_named(&drive.frame), "code");
    assert!(
        !drive.open.iter().any(|(_, tag)| tag == "blob"),
        "the README was not read as well: {:?}",
        drive.open
    );
}

/// The reader's header is the path as crumbs: each directory above the file
/// presses to show that directory, and pressing one already unfolded leaves
/// it unfolded — a crumb names where to be, not a fold to flip.
#[test]
fn a_crumb_shows_its_directory_and_never_folds_it() {
    let (mut drive, _) = namespace("duck://forge/core/blob/src/util/mod.rs");
    drive.answer("list_refs", &refs());
    drive.answer("list_items", &items());
    drive.answer("all", &accounts());
    drive.answer_tree("src/util", &listing(&[("src/util/mod.rs", "file")]));
    drive.answer_tree("", &listing(&[("src", "dir")]));
    drive.answer_tree("src", &listing(&[("src/util", "dir")]));
    let blob = drive.take("blob");
    drive.tick(vec![answer(
        blob,
        serde_json::json!({ "blob": {
            "path": "src/util/mod.rs", "rev": "1111222233334444",
            "text": "pub fn greeting() {}", "binary": false, "truncated": false
        }})
        .to_string()
        .as_bytes(),
    )]);
    assert!(has_key_ending(&drive.frame, "forge/tree/src/util/mod.rs"));
    let asked = tree_asks(&drive.frames);
    drive.tick(press(&drive.frame, "forge/crumb/0"));
    assert!(
        has_key_ending(&drive.frame, "forge/tree/src/util/mod.rs"),
        "the tree stayed unfolded: {:?}",
        texts(&drive.frame)
    );
    assert_eq!(tree_asks(&drive.frames), asked, "a crumb reads nothing");
}

#[test]
fn the_landing_file_is_the_readme_whatever_it_is_called() {
    use forge_view::host::{readme_of, TreeEntry};
    let entry = |path: &str, kind: &str| TreeEntry {
        name: path.rsplit('/').next().unwrap().to_owned(),
        path: path.to_owned(),
        kind: kind.to_owned(),
    };
    // `.md` wins over every other spelling, whatever order they arrive in
    assert_eq!(
        readme_of(&[entry("readme", "file"), entry("README.md", "file")]),
        "README.md"
    );
    assert_eq!(readme_of(&[entry("Readme.markdown", "file")]), "Readme.markdown");
    assert_eq!(readme_of(&[entry("README", "file")]), "README");
    assert_eq!(readme_of(&[entry("README.txt", "file")]), "README.txt");
    // a directory called README is not a file to open, and nothing else is a README
    assert_eq!(readme_of(&[entry("README.md", "dir")]), "");
    assert_eq!(readme_of(&[entry("src/readme.md", "file")]), "src/readme.md");
    assert_eq!(readme_of(&[entry("READMEISH.md", "file")]), "");
    assert_eq!(readme_of(&[]), "");
}

#[test]
fn a_crumb_trail_presses_every_directory_but_the_file() {
    use forge_view::host::crumbs_of;
    assert_eq!(
        crumbs_of("src/util/mod.rs"),
        vec![
            ("src".to_owned(), "src".to_owned()),
            ("src/util".to_owned(), "util".to_owned()),
            (String::new(), "mod.rs".to_owned()),
        ]
    );
    assert_eq!(
        crumbs_of("README.md"),
        vec![(String::new(), "README.md".to_owned())]
    );
    assert_eq!(crumbs_of(""), Vec::new());
}

/// The events the host sends when a key is pressed with nothing focused.
fn key_press(named: &str) -> Vec<Event> {
    use ducktape_view_guest::wire::keyboard::{
        Event as Key, Key as Pressed, KeyState, Location, Modifiers, NativeCode, Named, Physical,
    };
    let key = match named {
        "Escape" => Pressed::Named(Named::Escape),
        character => Pressed::Character(character.into()),
    };
    vec![Event::Keyboard {
        event: Key::Press {
            state: KeyState {
                key: key.clone(),
                modified_key: key,
                physical_key: Physical::Unidentified(NativeCode::Unidentified),
                location: Location::Standard,
                modifiers: Modifiers::default(),
            },
            text: None,
            repeat: false,
        },
        captured: false,
    }]
}

/// Three issues and two pull requests, mixed open and closed: what a
/// tracker's two sides are read off.
fn mixed_items() -> Vec<u8> {
    serde_json::json!({ "items": [
        { "number": 1, "kind": "issue", "state": "open", "title": "The landing shows no README",
          "author": { "account": 1 } },
        { "number": 2, "kind": "issue", "state": "closed", "title": "Tabs miss their counts",
          "author": { "account": 1 } },
        { "number": 3, "kind": "issue", "state": "open", "title": "Breadcrumbs are one string",
          "author": { "account": 1 } },
        { "number": 7, "kind": "pr", "state": "open", "title": "Bound every list",
          "author": { "account": 1 } },
        { "number": 8, "kind": "pr", "state": "closed", "title": "Abandoned rewrite",
          "author": { "account": 1 } }
    ]})
    .to_string()
    .into_bytes()
}

/// The repo open on its issue tracker, with both sides populated.
fn tracker() -> Drive {
    let (mut drive, _) = namespace("duck://forge/core");
    drive.answer("list_refs", &refs());
    drive.answer("list_items", &mixed_items());
    drive.answer("all", &accounts());
    drive.answer_tree("", &listing(&[("src", "dir")]));
    drive.tick(press(&drive.frame, "forge/tab/issues"));
    drive
}

/// A tracker opens on its open side — a closed issue is one press away, and
/// never mixed into the open list.
#[test]
fn a_tracker_opens_on_the_open_side_and_the_closed_side_is_a_press_away() {
    let mut drive = tracker();
    assert!(has_text(&drive.frame, "The landing shows no README"));
    assert!(has_text(&drive.frame, "Breadcrumbs are one string"));
    assert!(
        !has_text(&drive.frame, "Tabs miss their counts"),
        "the closed issue is not on the open side: {:?}",
        texts(&drive.frame)
    );
    // each side wears its own count
    assert!(has_text(&drive.frame, "Open 2"), "{:?}", texts(&drive.frame));
    assert!(has_text(&drive.frame, "Closed 1"));
    drive.tick(press(&drive.frame, "forge/tracker-side/closed"));
    assert!(has_text(&drive.frame, "Tabs miss their counts"));
    assert!(!has_text(&drive.frame, "The landing shows no README"));
}

/// The filter keeps the rows that match its text or their number, and says
/// so when none do.
#[test]
fn a_tracker_filter_keeps_what_matches_and_names_the_empty_case() {
    let mut drive = tracker();
    drive.tick(type_into(&drive.frame, "Filter by title or number", "readme"));
    assert!(has_text(&drive.frame, "The landing shows no README"));
    assert!(!has_text(&drive.frame, "Breadcrumbs are one string"));
    drive.tick(type_into(&drive.frame, "Filter by title or number", "#3"));
    assert!(has_text(&drive.frame, "Breadcrumbs are one string"));
    drive.tick(type_into(&drive.frame, "Filter by title or number", "zzz"));
    assert!(
        has_text(&drive.frame, "Nothing matches"),
        "{:?}",
        texts(&drive.frame)
    );
}

/// Escape leaves what is open, one step at a time: the item, then the
/// repository — and a key a native field took is that field's, not ours.
#[test]
fn escape_walks_back_out_of_the_item_then_the_repository() {
    let mut drive = open_item("duck://forge/core/7");
    assert!(has_text(&drive.frame, "Bound every list"));
    let held = {
        let mut events = key_press("Escape");
        if let Some(Event::Keyboard { captured, .. }) = events.first_mut() {
            *captured = true;
        }
        events
    };
    drive.tick(held);
    assert!(
        has_text(&drive.frame, "Bound every list"),
        "a captured key belongs to the field that took it"
    );
    drive.tick(key_press("Escape"));
    assert!(!has_text(&drive.frame, "Back to tracker"));
    drive.tick(key_press("Escape"));
    assert!(
        has_text(&drive.frame, "duckhouse"),
        "the namespace is back: {:?}",
        texts(&drive.frame)
    );
}

/// `/` asks the host to put the keyboard in the tracker filter, and asks
/// for nothing while the code browse is what is on screen.
#[test]
fn a_slash_reaches_for_the_filter_only_where_a_tracker_is() {
    let mut drive = tracker();
    let before = focus_asks(&drive).len();
    drive.tick(key_press("/"));
    let focused: Vec<String> = focus_asks(&drive).split_off(before);
    assert_eq!(focused.len(), 1, "one focus ask: {focused:?}");
    assert!(
        focused[0].contains("tracker-filter"),
        "it names the filter: {focused:?}"
    );
    // the tab press takes the keyboard to the page (that is what makes the
    // key arrive at all), so what is counted here is the FILTER ask
    drive.tick(press(&drive.frame, "forge/tab/code"));
    let before = focus_asks(&drive).len();
    drive.tick(key_press("/"));
    let after: Vec<String> = focus_asks(&drive).split_off(before);
    assert!(
        !after.iter().any(|ask| ask.contains("tracker-filter")),
        "the code browse reaches for no filter: {after:?}"
    );
}

/// The window's keys reach a view only while something inside it holds the
/// focus, so every press that moves the forge screen asks for the page.
#[test]
fn a_move_between_screens_takes_the_keyboard_to_the_page() {
    let mut drive = tracker();
    let takes_the_page = |asks: Vec<String>| asks.iter().any(|ask| ask.contains("ForgeView/page"));
    let opened = focus_asks(&drive);
    assert!(takes_the_page(opened), "opening the repository takes it");
    for step in ["forge/tracker-side/closed", "forge/tab/code"] {
        let before = focus_asks(&drive).len();
        drive.tick(press(&drive.frame, step));
        let asks = focus_asks(&drive).split_off(before);
        assert!(takes_the_page(asks.clone()), "{step} takes it: {asks:?}");
    }
    // and leaving the repository, which is where Escape lands
    let before = focus_asks(&drive).len();
    drive.tick(key_press("Escape"));
    let asks = focus_asks(&drive).split_off(before);
    assert!(takes_the_page(asks.clone()), "Escape keeps it: {asks:?}");
}

/// Every focus the view has asked the host for, oldest first.
fn focus_asks(drive: &Drive) -> Vec<String> {
    drive
        .frames
        .iter()
        .flat_map(|frame| &frame.requests)
        .filter(|request| request.kind == "host.widget")
        .map(|request| String::from_utf8_lossy(&request.payload).into_owned())
        .collect()
}

/// A changed file folds its hunks away and brings them back, and folding one
/// file leaves the others open.
#[test]
fn a_changed_file_folds_its_hunks_away_and_back() {
    let mut drive = open_files("duck://forge/core/7");
    let hunk = "@@ -1 +1 @@";
    assert!(has_text(&drive.frame, hunk), "{:?}", texts(&drive.frame));
    drive.tick(press(&drive.frame, "main.rs"));
    assert!(
        !has_text(&drive.frame, hunk),
        "the hunks are away: {:?}",
        texts(&drive.frame)
    );
    assert!(has_text(&drive.frame, "main.rs"), "the header stays");
    drive.tick(press(&drive.frame, "main.rs"));
    assert!(has_text(&drive.frame, hunk), "and they come back");
}
