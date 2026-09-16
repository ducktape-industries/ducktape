use super::*;

#[test]
fn a_chat_load_answers_for_the_huddle_only_when_it_loaded_the_huddles_channel() {
    let member = |is_you: bool| HuddleParticipant {
        key: "aa".into(),
        label: "aa".into(),
        initials: "A".into(),
        is_agent: false,
        is_you,
        joined_at: 0,
        node: "aa11".into(),
    };
    let idle = HuddleAfterLoad::default();

    // Not in a huddle: the loaded channel's roster is the whole answer.
    let joined = huddle_after_load(
        true,
        idle.joined,
        idle.channel.clone(),
        idle.channel_name.clone(),
        idle.roster.clone(),
        "eng".into(),
        "Engineering".into(),
        vec![member(true)],
    );
    assert!(joined.joined);
    assert_eq!(joined.channel, "eng");
    assert_eq!(joined.channel_name, "Engineering");
    assert_eq!(joined.roster.len(), 1);

    // NOW CLICK ANOTHER ROOM. Its roster is a different conversation's, and
    // reading the huddle off it used to cut the call's media (the session is
    // subscribed on `joined`) and blank the channel `leave_huddle_here` needs.
    let switched = huddle_after_load(
        true,
        joined.joined,
        joined.channel.clone(),
        joined.channel_name.clone(),
        joined.roster.clone(),
        "general".into(),
        "General".into(),
        Vec::new(),
    );
    assert_eq!(switched, joined, "another room's load is not the huddle's");

    // Back on the huddle's own channel, a roster without her ends it.
    let left = huddle_after_load(
        true,
        joined.joined,
        joined.channel.clone(),
        joined.channel_name.clone(),
        joined.roster.clone(),
        "eng".into(),
        "Engineering".into(),
        vec![member(false)],
    );
    assert_eq!(left, idle);

    // And a resync that carried no chat at all says nothing either way.
    let quiet = huddle_after_load(
        false,
        joined.joined,
        joined.channel.clone(),
        joined.channel_name.clone(),
        joined.roster.clone(),
        "eng".into(),
        "Engineering".into(),
        Vec::new(),
    );
    assert_eq!(quiet, joined);
}

/// THE ROSTER IS NOT THE HOST'S. Who a network calls a validator, who it
/// calls a resident, and whether THIS node may act on either are folds of
/// `valset` — and every screen that draws one reads it for itself through
/// the kernel. A host-side row type is how those folds diverge: the app
/// would decide the gate while the view decided the word, and a view swap
/// could no longer change either. The directory beneath is walked rather
/// than a fixed file list, so a new backend file cannot reintroduce one.
#[test]
fn the_backend_builds_no_roster_of_its_own() {
    let backend = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/backend");
    let mut offenders = Vec::new();
    let mut pending = vec![backend.clone()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("the backend directory is readable") {
            let path = entry.expect("a backend entry").path();
            if path.is_dir() {
                // this walk names the banned symbols, and it lives here
                let is_the_suite = path.file_name().is_some_and(|name| name == "tests");
                if !is_the_suite {
                    pending.push(path);
                }
                continue;
            }
            if path.extension().is_none_or(|kind| kind != "rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("a backend file is readable");
            let named = [
                "MemberRow",
                "MembersData",
                "load_members",
                "members_is_admin",
            ]
            .into_iter()
            .filter(|name| source.contains(name))
            .collect::<Vec<_>>();
            if !named.is_empty() {
                offenders.push(format!("{}: {named:?}", path.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the roster belongs to the members view: {offenders:?}"
    );
}

/// The shell owns ONE transient layer now — the bell — and Escape names it
/// only when it is up. Every other layer is a view's own: the palette claims
/// its chord and answers its own Escape, like the chat menus and the pages
/// cards before it.
#[test]
fn escape_ladder_names_the_topmost_transient_layer_only() {
    assert_eq!(escape_target("x".into(), true), "");
    assert_eq!(escape_target("escape".into(), true), "bell");
    assert_eq!(escape_target("escape".into(), false), "");
}

#[test]
fn the_two_ladder_readers_enumerate_the_same_layers() {
    assert_eq!(topmost_overlay(true), "bell");
    assert_eq!(topmost_overlay(false), "");
}

#[test]
fn files_base64_round_trips() {
    for sample in [
        b"".as_slice(),
        b"a".as_slice(),
        b"ab".as_slice(),
        b"abc".as_slice(),
        b"hello duckfs \xf0\x9f\xa6\x86".as_slice(),
    ] {
        let encoded = base64_encode(sample);
        assert_eq!(
            base64_decode(&encoded).as_deref(),
            Some(sample),
            "{encoded}"
        );
    }
    assert_eq!(base64_encode(b"abc"), "YWJj");
    assert_eq!(base64_encode(b"ab"), "YWI=");
    // MALFORMED IS A REFUSAL, NOT EMPTY BYTES. The handwritten decoder this
    // replaced stopped at padding per quartet (`Zg==Zg==` read as `ff`) and
    // read a lone `Z` as nothing; a read page that decodes to `None` fails
    // the read upstream instead of showing an empty file.
    for malformed in [
        "Zg==Zg==", "Z", "Zg", "Zg=", "Zg===", "Zh==", "Y*Jj", "YWJj\n",
    ] {
        assert_eq!(base64_decode(malformed), None, "{malformed}");
    }
}

/// THE TAB-SWITCH GATE. Four planes used to refetch on every tab move —
/// members, governance, agents, account — regardless of the destination, so a
/// click into Files paid four `/v1/query` round trips for rows nothing on
/// screen reads.
#[test]
fn a_tab_move_only_refetches_what_its_destination_draws() {
    let tabs = [
        ShellTab::Chat,
        ShellTab::Pages,
        ShellTab::Forge,
        ShellTab::Agents,
        ShellTab::Files,
        ShellTab::Explorer,
        ShellTab::Node,
        ShellTab::Members,
        ShellTab::Governance,
        ShellTab::Settings,
    ];

    // every plane left is one pane's: Settings draws the account card, Forge
    // the org "about", and proposals and agent rows belong to one pane each.
    // The ROSTER is on no line here at all — the members, governance, node,
    // forge and settings views each read the valset for themselves, so no tab
    // click loads one.
    for (plane, drawn) in [
        ("members", &[][..]),
        ("governance", &[ShellTab::Governance][..]),
        ("agents", &[ShellTab::Agents][..]),
        ("account", &[ShellTab::Forge, ShellTab::Settings][..]),
        // an unknown plane name is nobody's — a typo must not silently reopen
        // the storm by answering true.
        ("explorer", &[][..]),
    ] {
        let readers: Vec<ShellTab> = tabs
            .iter()
            .copied()
            .filter(|tab| tab_reads_plane(*tab, plane.into()))
            .collect();
        assert_eq!(readers, drawn, "exactly these tabs draw {plane}");
    }
}

/// The launch window reads the workspace files through the crate that wrote
/// them, never a line parser: the chain id keeps its `#hex` half, a
/// two-validator descriptor (the multi-line array `node admit` writes) still
/// yields the founding key, and a wildcard `http_listen` dials loopback.
#[test]
fn workspace_facts_come_from_the_crate_that_wrote_them() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("mynet-dir");
    std::fs::create_dir_all(&dir).unwrap();
    let founder = "aa".repeat(32);
    let admitted = "bb".repeat(32);
    workspace_config::NetworkDescriptor {
        chain_id: "mynet#a1b2c3d4".into(),
        validators: vec![founder.clone(), admitted],
        bootstrap: vec![],
        reach: vec![],
        coordination: None,
        block_time_ms: workspace_config::DEFAULT_BLOCK_TIME_MS,
        modules: vec![],
        genesis: String::new(),
    }
    .save(&dir.join("network.toml"))
    .unwrap();
    let descriptor = std::fs::read_to_string(dir.join("network.toml")).unwrap();
    assert!(
        descriptor.contains("validators = [\n"),
        "two validators serialize as a multi-line array:\n{descriptor}"
    );
    std::fs::write(
        dir.join("node.toml"),
        r#"network = "network.toml"
key_file = "node.key"
listen = "0.0.0.0:52200"
advertised = "overlay"
storage_dir = "data"
http_listen = "0.0.0.0:8844"
gateway_listen = "127.0.0.1:0"
rpc_listen = "127.0.0.1:8845"
wireguard_listen = "0.0.0.0:51820"
invite_listen = "0.0.0.0:51821"
wireguard_advertised = "auto"
primary_coordinator = "none"
coordinator_relay = "none"
checkpoint_blocks = 32
"#,
    )
    .unwrap();

    assert_eq!(
        workspaces_in(root.path()),
        vec![("mynet#a1b2c3d4".to_string(), dir.clone())]
    );
    assert_eq!(workspace_identity(&dir), Some(short_label(&founder)));
    assert_eq!(
        workspace_endpoint(&dir).as_deref(),
        Some("http://127.0.0.1:8844")
    );
}

