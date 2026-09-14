//! The folds the screen is drawn from, read straight: the module's write
//! rule, the listing and history folds, the path bar's segments, the
//! browser's trail, and the sort, filter and keyboard readings.

use files_view::browse::{
    BrowseKey, Navigation, Sort, SortKey, browse_key, kind_label, neighbour, row_key, tally,
    visible_rows,
};
use files_view::host::{
    FsEntry, crumbs, fold_author, fold_entries, fold_history, fs_child, fs_name, fs_parent,
    home_of, readable, write_refusal,
};

/// duckfs's `check_authority` restated in the view's own words: a home tree
/// needs its label AND an entry under it, `/shared/**` is every member's, and
/// both namespace roots are nobody's.
#[test]
fn the_write_rule_names_the_directories_nothing_can_be_written_in() {
    for writable in [
        "/shared",
        "/shared/docs",
        "/home/alice",
        "/home/alice/notes",
    ] {
        assert_eq!(write_refusal(writable), "", "{writable}");
    }
    assert_eq!(write_refusal("/home"), "home root is not writable");
    assert_eq!(write_refusal("/"), "path is outside /home and /shared");
    assert_eq!(write_refusal("/etc"), "path is outside /home and /shared");
}

/// The root is `/`, never "" — duckfs refuses an empty path.
#[test]
fn paths_split_and_join_around_the_root() {
    assert_eq!(fs_parent("/shared/docs/a.md"), "/shared/docs");
    assert_eq!(fs_parent("/shared"), "/");
    assert_eq!(fs_parent("/"), "/");
    assert_eq!(fs_child("/shared", "  notes  "), "/shared/notes");
    assert_eq!(fs_child("/", "notes"), "/notes");
    assert_eq!(fs_name("/shared/docs/a.md"), "a.md");
    assert_eq!(fs_name("/"), "/");
    assert_eq!(home_of("7"), "/home/acct:7");
    assert_eq!(home_of(""), "");
}

/// The path bar has one segment per directory, each carrying the directory
/// it opens, root first.
#[test]
fn the_path_bar_has_one_segment_per_directory() {
    let segments: Vec<(String, String)> = crumbs("/shared/docs/plans")
        .into_iter()
        .map(|crumb| (crumb.name, crumb.path))
        .collect();
    assert_eq!(
        segments,
        [
            ("/".to_string(), "/".to_string()),
            ("shared".to_string(), "/shared".to_string()),
            ("docs".to_string(), "/shared/docs".to_string()),
            ("plans".to_string(), "/shared/docs/plans".to_string()),
        ]
    );
    assert_eq!(crumbs("/").len(), 1);
}

/// The rows the browser draws come out of the `ls` reply, and a path is the
/// same row across every re-read, so the virtual list keeps its place.
#[test]
fn a_listing_folds_into_rows_keyed_by_path() {
    let reply = serde_json::json!({ "entries": [
        { "path": "/shared/docs", "kind": "dir", "size": 2, "object": "aa" },
        { "path": "/shared/a.md", "kind": "file", "size": 7, "object": "bb" },
    ]});
    let rows = fold_entries(&reply);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "docs");
    assert!(rows[0].is_dir());
    assert_eq!(rows[1].name, "a.md");
    assert_eq!(rows[1].size, 7);
    assert_eq!(
        row_key(&rows[1].path),
        row_key("/shared/a.md"),
        "a path keeps its row identity across re-reads"
    );
    assert_ne!(row_key(&rows[0].path), row_key(&rows[1].path));
}

/// A snapshot's author crosses as duckfs's `Actor` — an externally tagged
/// enum, not a display string — and reads into the module's own label.
#[test]
fn history_reads_the_real_author_shape() {
    assert_eq!(fold_author(&serde_json::json!({ "Account": 7 })), "acct:7");
    assert_eq!(fold_author(&serde_json::json!("System")), "system");
    assert_eq!(
        fold_author(&serde_json::json!({ "Module": "chat" })),
        "module:chat"
    );
    assert_eq!(
        fold_author(&serde_json::json!({ "Key": [0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67] })),
        "ext:abcdef012345…"
    );
    assert_eq!(
        fold_author(&serde_json::json!("ext:aa")),
        "",
        "a display string is not an actor"
    );

    let reply = serde_json::json!({ "snapshots": [
        { "id": "a".repeat(64), "parent": "p".repeat(64), "author": { "Account": 9 }, "height": 12, "message": "first" },
    ]});
    let rows = fold_history(&reply);
    assert_eq!(rows[0].short_id, format!("{}…", "a".repeat(12)));
    assert_eq!(rows[0].parent, "p".repeat(64));
    assert_eq!(rows[0].author, "acct:9");
    assert_eq!(rows[0].height, 12);
}

/// A page that ends inside a multi-byte character is still text: the cut
/// is the page's, not the file's. A control byte is still binary.
#[test]
fn a_page_cut_inside_a_character_still_reads_as_text() {
    let mut bytes = "한글 문서 ".repeat(3).into_bytes();
    let whole = bytes.len();
    bytes.extend_from_slice(&"한".as_bytes()[..2]);
    let (text, binary) = readable(bytes.clone(), false);
    assert!(!binary, "a trailing partial character is the page's cut");
    assert_eq!(text.len(), whole);

    let (_, binary) = readable(bytes, true);
    assert!(binary, "the same bytes at the file's end are malformed");

    let (_, binary) = readable(b"plain\0bytes".to_vec(), true);
    assert!(binary, "a control byte is binary");
    let (text, binary) = readable("plain\ttext\n".as_bytes().to_vec(), true);
    assert!(!binary);
    assert_eq!(text, "plain\ttext\n");
}

fn entry(path: &str, kind: &str, size: i64) -> FsEntry {
    FsEntry {
        path: path.into(),
        name: fs_name(path),
        kind: kind.into(),
        size,
        object: String::new(),
    }
}

/// Folders sort first whatever the column; the column orders within.
#[test]
fn rows_sort_folders_first_and_filter_by_name() {
    let entries = [
        entry("/s/zeta.md", "file", 30),
        entry("/s/beta", "dir", 1),
        entry("/s/alpha.txt", "file", 10),
        entry("/s/Gamma", "dir", 1),
        entry("/s/pic.png", "file", 20),
    ];
    let names =
        |rows: Vec<FsEntry>| -> Vec<String> { rows.into_iter().map(|row| row.name).collect() };
    assert_eq!(
        names(visible_rows(&entries, "", Sort::BY_NAME)),
        ["beta", "Gamma", "alpha.txt", "pic.png", "zeta.md"]
    );
    let by_size_desc = Sort::BY_NAME.toggled(SortKey::Size).toggled(SortKey::Size);
    assert!(!by_size_desc.ascending);
    assert_eq!(
        names(visible_rows(&entries, "", by_size_desc)),
        ["beta", "Gamma", "zeta.md", "pic.png", "alpha.txt"]
    );
    let by_kind = Sort::BY_NAME.toggled(SortKey::Kind);
    assert_eq!(
        names(visible_rows(&entries, "", by_kind)),
        ["beta", "Gamma", "zeta.md", "pic.png", "alpha.txt"]
    );
    assert_eq!(
        names(visible_rows(&entries, "AL", Sort::BY_NAME)),
        ["alpha.txt"]
    );
    assert_eq!(kind_label(&entries[0]), "Markdown");
    assert_eq!(kind_label(&entries[1]), "Folder");
    assert_eq!(kind_label(&entries[2]), "Text");
    assert_eq!(kind_label(&entries[4]), "Picture");
    assert_eq!(tally(&entries), "5 items, 2 folders");
}

/// Arrow keys step through the visible rows, clamped at the ends; with
/// nothing chosen the first row is next.
#[test]
fn the_selection_steps_through_visible_rows() {
    let rows = [entry("/s/a", "dir", 1), entry("/s/b.md", "file", 1)];
    assert_eq!(neighbour(&rows, "", 1).as_deref(), Some("/s/a"));
    assert_eq!(neighbour(&rows, "/s/a", 1).as_deref(), Some("/s/b.md"));
    assert_eq!(neighbour(&rows, "/s/b.md", 1).as_deref(), Some("/s/b.md"));
    assert_eq!(neighbour(&rows, "/s/b.md", -1).as_deref(), Some("/s/a"));
    assert_eq!(neighbour(&rows, "/s/a", -1).as_deref(), Some("/s/a"));
    assert_eq!(neighbour(&[], "", 1), None);
}

/// Back and Forward are a browser's trail: a fresh move clears what was
/// ahead, standing still is no move.
#[test]
fn the_trail_walks_back_and_forward() {
    let mut nav = Navigation::at("/shared");
    assert!(!nav.go("/shared"), "standing still is no move");
    assert!(nav.go("/shared/docs"));
    assert!(nav.go("/shared/docs/plans"));
    assert!(nav.back());
    assert_eq!(nav.path, "/shared/docs");
    assert!(nav.can_forward());
    assert!(nav.forward());
    assert_eq!(nav.path, "/shared/docs/plans");
    assert!(nav.back() && nav.back());
    assert_eq!(nav.path, "/shared");
    assert!(!nav.back(), "nothing behind the start");
    assert!(nav.go("/home"));
    assert!(!nav.can_forward(), "a fresh move clears what was ahead");
}

/// The keys the browser answers, and the ones it leaves to the host.
#[test]
fn keys_read_into_browser_moves() {
    use ducktape_view_guest::wire::keyboard::{
        Key, KeyState, Location, Modifiers, Named, NativeCode, Physical,
    };
    let state = |key: Key, modifiers: Modifiers| KeyState {
        key: key.clone(),
        modified_key: key,
        physical_key: Physical::Unidentified(NativeCode::Unidentified),
        location: Location::Standard,
        modifiers,
    };
    let plain = Modifiers::default();
    let command = Modifiers {
        control: true,
        ..Modifiers::default()
    };
    assert_eq!(
        browse_key(&state(Key::Named(Named::ArrowDown), plain)),
        BrowseKey::Down
    );
    assert_eq!(
        browse_key(&state(Key::Named(Named::ArrowUp), plain)),
        BrowseKey::Up
    );
    assert_eq!(
        browse_key(&state(Key::Named(Named::Enter), plain)),
        BrowseKey::Open
    );
    assert_eq!(
        browse_key(&state(Key::Named(Named::Backspace), plain)),
        BrowseKey::Parent
    );
    assert_eq!(
        browse_key(&state(Key::Named(Named::ArrowUp), command)),
        BrowseKey::Parent
    );
    assert_eq!(
        browse_key(&state(Key::Named(Named::ArrowLeft), command)),
        BrowseKey::Back
    );
    assert_eq!(
        browse_key(&state(Key::Named(Named::ArrowRight), command)),
        BrowseKey::Forward
    );
    assert_eq!(
        browse_key(&state(Key::Character("a".into()), plain)),
        BrowseKey::Ignored
    );
}
