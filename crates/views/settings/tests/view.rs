//! The view driven natively through the wire, on the KERNEL CONTRACT: the
//! kernel pushes session facts, the view reads this node's standing and the
//! seat's key associations for itself (`rpc.status` / `rpc.query`, re-read on
//! every `rpc.live` hit), and every act leaves as an intent the kernel signs
//! — the password crosses out and never back.

use std::collections::BTreeMap;

use ducktape_view_guest::testing::{answer, has_text, item, press, submit, texts, type_into};
use ducktape_view_guest::wire::{ButtonContent, Frame, Node, Request};
use settings_view::host::{KeyAdd, Name, Session, Tab, Unlock};
use settings_view::{boot_native, tick_native};

const SEAT: &str = "8c4fa211";

fn facts() -> Session {
    Session {
        dark: false,
        connected: true,
        loading: false,
        status: "Connected".into(),
        busy: false,
        recovering: false,
        appearance: "system".into(),
        desktop_notifications: true,
        desktop_notifications_host: "ready".into(),
        unlocked: true,
        seat_key: SEAT.into(),
        account_name: "duck".into(),
        account_number: "42".into(),
        account_exists: true,
        network_name: "testnet".into(),
        connected_rpc: "http://127.0.0.1:1".into(),
        account_ceremony_phase: String::new(),
        account_ceremony_qr: String::new(),
        account_ceremony_detail: String::new(),
        account_ceremony_left: String::new(),
        settings_key_state: "sealed".into(),
        settings_key_path: "/keys/user.key".into(),
        account_busy: false,
        account_ticket: String::new(),
        update_state: "unavailable".into(),
        update_current: String::new(),
        update_previous: String::new(),
        update_staged_display: String::new(),
        update_channel: "stable".into(),
        update_checked: String::new(),
        update_note: String::new(),
        update_busy: false,
    }
}

fn encoded(session: &Session) -> Vec<u8> {
    serde_json::to_vec(session).expect("session encodes")
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
        .unwrap_or_else(|| panic!("no `{kind}` request in {:?}", frame.requests))
}

/// Whether the button named `name` — by key, label or its own words, the
/// three ways `press` finds one — is in the tree with no press to send.
fn button_disabled(frame: &Frame, name: &str) -> bool {
    let mut root = frame.root.clone().expect("a tree");
    let mut disabled = None;
    root.for_each_mut(&mut |node| {
        let Node::Button {
            key,
            content,
            label,
            on_press,
            ..
        } = node
        else {
            return;
        };
        let named = key == name
            || label.as_deref() == Some(name)
            || matches!(content, ButtonContent::Label(words) if words == name);
        if named {
            disabled = Some(on_press.is_none());
        }
    });
    disabled.expect("the button is in the tree")
}

// ---------- what a node would answer ----------

/// This device IS the chain's one validator, so the card reads `validator`.
fn status() -> Vec<u8> {
    serde_json::json!({ "public_key": SEAT })
        .to_string()
        .into_bytes()
}

fn seats(name: &str, keys: &[[u8; 4]]) -> Vec<u8> {
    serde_json::json!({ name: keys }).to_string().into_bytes()
}

fn agents() -> Vec<u8> {
    serde_json::json!({ "model": { "agents": [{ "id": "a" }] } })
        .to_string()
        .into_bytes()
}

/// The identity module's `Account(Some)` reply for the seat's account.
fn account(keys: usize) -> Vec<u8> {
    let rows: Vec<serde_json::Value> = [
        serde_json::json!({
            "scheme": "ed25519", "pubkey": [0x8c, 0x4f, 0xa2, 0x11], "label": "laptop"
        }),
        serde_json::json!({
            "scheme": "secp256r1", "pubkey": [0xff, 0x00], "label": "phone"
        }),
    ]
    .into_iter()
    .take(keys)
    .collect();
    serde_json::json!({ "account": { "number": 42, "keys": rows } })
        .to_string()
        .into_bytes()
}

/// The reply a node would give this read, or `None` when the request is not
/// a read at all (a live subscription, an intent, a badge).
fn canned(request: &Request, keys: usize) -> Option<Vec<u8>> {
    if request.kind == "rpc.status" {
        return Some(status());
    }
    if request.kind != "rpc.query" {
        return None;
    }
    let ask: serde_json::Value = serde_json::from_slice(&request.payload).expect("a query decodes");
    let target = ask["target"].as_str().expect("a target");
    let validators = ask["query"] == serde_json::json!("validators");
    match (target, validators) {
        ("valset", true) => Some(seats("validators", &[[0x8c, 0x4f, 0xa2, 0x11]])),
        ("valset", false) => Some(seats("residents", &[[1, 2, 3, 4], [5, 6, 7, 8]])),
        ("runs", _) => Some(agents()),
        ("identity", _) => Some(account(keys)),
        _ => panic!("unexpected query target {target}"),
    }
}

/// Answer every read the view has outstanding until it asks for nothing
/// more, recording the `rpc.live` planes it subscribed to on the way.
fn settle(mut frame: Frame, planes: &mut BTreeMap<String, u64>, keys: usize) -> Frame {
    loop {
        for request in &frame.requests {
            if request.kind == "rpc.live" {
                let plane = String::from_utf8(request.payload.clone()).expect("a plane name");
                planes.insert(plane, request.id);
            }
        }
        let answers: Vec<_> = frame
            .requests
            .iter()
            .filter_map(|request| canned(request, keys).map(|reply| answer(request.id, &reply)))
            .collect();
        if answers.is_empty() {
            return frame;
        }
        frame = tick_native(answers);
    }
}

/// Boots, pushes the session, and answers every read it starts: the settled
/// frame, the session subscription, and the live planes it watches.
fn connected(session: &Session, keys: usize) -> (Frame, u64, BTreeMap<String, u64>) {
    boot_native();
    let frame = tick_native(Vec::new());
    assert_eq!(
        kinds(&frame.requests),
        ["settings.props"],
        "only the session at boot: {:?}",
        frame.requests
    );
    let props = request(&frame, "settings.props").id;
    let mut planes = BTreeMap::new();
    let frame = settle(
        tick_native(vec![item(props, &encoded(session))]),
        &mut planes,
        keys,
    );
    (frame, props, planes)
}

fn one_intent(frame: &Frame) -> &Request {
    let [intent] = frame.requests.as_slice() else {
        panic!("one intent, got {:?}", frame.requests);
    };
    intent
}

// ---------- the reads ----------

/// Connected, the view reads its own standing and its own key rows: the
/// valset and the runs registry through `rpc.query`, this node's key off
/// `rpc.status`, and the account the seat belongs to off `identity`. Every
/// one of them folds onto the screen.
#[test]
fn a_connected_view_reads_its_own_standing_and_key_rows() {
    let (frame, _, planes) = connected(&facts(), 2);
    assert_eq!(
        planes.keys().collect::<Vec<_>>(),
        ["identity", "valset"],
        "one live plane per read: {planes:?}"
    );

    let frame = tick_native(press(&frame, "Network"));
    assert!(
        has_text(&frame, "3 humans · 1 agent"),
        "the headcount is folded off the valset: {:?}",
        texts(&frame)
    );

    let frame = tick_native(press(&frame, "Account"));
    for expected in [
        "Validator",
        "laptop",
        "Device key",
        "8c4fa211",
        "phone",
        "Passkey",
        "ff00",
    ] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
}

#[test]
fn each_tab_selects_only_its_own_groups_inside_the_shared_scroll_root() {
    let panes = ["General", "Network", "Account", "Security"];
    let groups = [
        ("settings/appearance", "General"),
        ("settings/notifications", "General"),
        ("settings/updates", "General"),
        ("settings/network", "Network"),
        ("settings/identity-title", "Account"),
        ("settings/keys-title", "Account"),
        ("settings/security-title", "Security"),
    ];
    let (mut frame, props, _) = connected(&facts(), 2);
    for selected in panes {
        tick_native(press(&frame, selected));
        // Unrelated incoming facts must not select a different pane.
        frame = tick_native(vec![item(props, &encoded(&facts()))]);
        let mut root = frame.root.clone().expect("Settings tree");
        assert!(matches!(&root, Node::Scroll { key, .. } if key == "settings"));
        let mut visible_keys = Vec::new();
        let mut tabs = Vec::new();
        root.for_each_mut(&mut |node| {
            if let Some(key) = node.key() {
                visible_keys.push(key.to_owned());
            }
            if let Node::Button {
                key,
                checked,
                on_press,
                ..
            } = node
                && let Some(pane) = key.strip_prefix("settings/tab/")
            {
                assert!(on_press.is_some());
                tabs.push((pane.to_owned(), *checked));
            }
        });
        assert_eq!(tabs.len(), panes.len());
        for pane in panes {
            assert!(tabs.contains(&(pane.to_lowercase(), Some(pane == selected))));
        }
        for (group, owner) in groups {
            assert_eq!(
                visible_keys.iter().filter(|key| *key == group).count(),
                usize::from(owner == selected),
                "{group} while {selected} selected"
            );
        }
    }
}

/// A view with no connection reads nothing — the session is the only thing
/// it is waiting on.
#[test]
fn an_unconnected_view_asks_the_kernel_for_nothing() {
    let offline = Session {
        connected: false,
        status: "Connecting…".into(),
        ..facts()
    };
    boot_native();
    let frame = tick_native(Vec::new());
    let props = request(&frame, "settings.props").id;
    let frame = tick_native(vec![item(props, &encoded(&offline))]);
    assert!(
        frame.requests.is_empty(),
        "nothing to read with no node: {:?}",
        frame.requests
    );
}

#[test]
fn disconnect_hides_retained_account_and_network_claims_without_losing_drafts() {
    let (frame, props, _) = connected(&facts(), 2);
    let frame = tick_native(press(&frame, "Account"));
    let frame = tick_native(type_into(&frame, "rename account…", "kept draft"));
    assert!(has_text(&frame, "Account keys"));
    let offline = Session {
        connected: false,
        ..facts()
    };
    let frame = tick_native(vec![item(props, &encoded(&offline))]);
    assert!(has_text(&frame, "Not connected"));
    for hidden in [
        "duck",
        "42",
        "Account keys",
        "Rename",
        "Mint ticket",
        "Remove",
    ] {
        assert!(
            !has_text(&frame, hidden),
            "stale {hidden}: {:?}",
            texts(&frame)
        );
    }
    let frame = tick_native(press(&frame, "Network"));
    assert!(has_text(&frame, "Not connected"));
    assert!(!has_text(&frame, "Connected"));
    assert!(!has_text(&frame, "Members"));
    assert!(!button_disabled(&frame, "Reconnect"));
    let frame = tick_native(vec![item(props, &encoded(&facts()))]);
    let frame = tick_native(press(&frame, "Account"));
    assert!(
        !button_disabled(&frame, "Rename"),
        "the draft survives disconnect"
    );
    let frame = tick_native(press(&frame, "Rename"));
    assert_eq!(
        serde_json::from_slice::<Name>(&request(&frame, "settings.rename").payload)
            .unwrap()
            .name,
        "kept draft"
    );
}

#[test]
fn copy_actions_send_complete_account_number_and_ticket() {
    let number = "18446744073709551615";
    let ticket = "fixture-ticket-with-a-long-capability-that-must-not-be-truncated";
    let session = Session {
        account_number: number.into(),
        account_ticket: ticket.into(),
        ..facts()
    };
    let (frame, _, _) = connected(&session, 2);
    let frame = tick_native(press(&frame, "Account"));
    assert!(has_text(&frame, number));
    let frame = tick_native(press(&frame, "Copy number"));
    let copied: settings_view::host::Copy =
        serde_json::from_slice(&request(&frame, "settings.copy").payload).unwrap();
    assert_eq!(copied.text, number);
    let frame = tick_native(press(&frame, "Copy ticket"));
    let copied: settings_view::host::Copy =
        serde_json::from_slice(&request(&frame, "settings.copy").payload).unwrap();
    assert_eq!(copied.text, ticket);
}

/// A valset block re-reads the standing; an identity block re-reads the key
/// rows. Neither costs the other a query.
#[test]
fn a_block_on_a_plane_reads_only_that_plane_again() {
    let (_, _, planes) = connected(&facts(), 2);

    let frame = tick_native(vec![item(planes["valset"], b"{}")]);
    assert_eq!(
        kinds(&frame.requests),
        ["rpc.status"],
        "the standing read starts at this node's key: {:?}",
        frame.requests
    );

    let frame = tick_native(vec![item(planes["identity"], b"{}")]);
    let query = request(&frame, "rpc.query");
    let ask: serde_json::Value = serde_json::from_slice(&query.payload).expect("decodes");
    assert_eq!(ask["target"], "identity");
    assert_eq!(
        ask["query"]["of_key"]["key"],
        serde_json::json!([0x8c, 0x4f, 0xa2, 0x11]),
        "the seat's PUBLIC key is what resolves the account"
    );
}

// ---------- the acts ----------

#[test]
fn working_browser_ceremony_can_be_cancelled_without_an_account() {
    let working = Session {
        account_exists: false,
        account_ceremony_phase: "working".into(),
        account_ceremony_detail: "Waiting for browser".into(),
        ..facts()
    };
    let (frame, _, _) = connected(&working, 0);
    let frame = tick_native(press(&frame, "Account"));
    assert!(!button_disabled(&frame, "Cancel"));
    let frame = tick_native(press(&frame, "Cancel"));
    assert_eq!(one_intent(&frame).kind, "settings.ceremony_cancel");
}

/// The seat's password leaves as an `unlock` intent and never comes back:
/// what the kernel pushes is the FLAG.
#[test]
fn the_password_leaves_only_as_an_unlock_and_never_comes_back() {
    let locked = Session {
        unlocked: false,
        ..facts()
    };
    let (frame, props, _) = connected(&locked, 2);
    let frame = tick_native(press(&frame, "Security"));
    let frame = tick_native(type_into(&frame, "unlock signing…", "hunter2"));
    assert!(frame.requests.is_empty(), "typing runs no handler");
    let frame = tick_native(submit(&frame, "unlock signing…"));
    let intent = one_intent(&frame);
    assert_eq!(intent.kind, "settings.unlock");
    assert_eq!(
        serde_json::from_slice::<Unlock>(&intent.payload).expect("decodes"),
        Unlock {
            password: "hunter2".into()
        }
    );
    let frame = tick_native(vec![item(props, &encoded(&facts()))]);
    assert!(
        has_text(&frame, "Signing unlocked for this session."),
        "{:?}",
        texts(&frame)
    );
    assert!(!has_text(&frame, "hunter2"));
}

/// A rename leaves as the intent carrying the TRIMMED name, and the draft is
/// spent when the account carries that name back — the kernel signs the op,
/// so the account it reports IS the acknowledgement. Nothing else is spent.
#[test]
fn a_rename_is_spent_when_the_account_carries_its_name_back() {
    let (frame, props, _) = connected(&facts(), 2);
    let frame = tick_native(press(&frame, "Account"));
    let frame = tick_native(type_into(&frame, "rename account…", "  mallard  "));
    let frame = tick_native(type_into(&frame, "paste its ed25519 key (hex)…", "ff00"));
    let frame = tick_native(press(&frame, "Rename"));
    let intent = one_intent(&frame);
    assert_eq!(intent.kind, "settings.rename");
    assert_eq!(
        serde_json::from_slice::<Name>(&intent.payload).expect("decodes"),
        Name {
            name: "mallard".into()
        }
    );

    // the same name pushed back spends the name draft …
    let renamed = Session {
        account_name: "mallard".into(),
        ..facts()
    };
    let frame = tick_native(vec![item(props, &encoded(&renamed))]);
    assert!(
        button_disabled(&frame, "Rename"),
        "an empty name draft offers no rename: {:?}",
        texts(&frame)
    );

    // … and only it: the pasted key still mints
    let frame = tick_native(press(&frame, "Mint ticket"));
    let intent = one_intent(&frame);
    assert_eq!(intent.kind, "settings.key_add");
    assert_eq!(
        serde_json::from_slice::<KeyAdd>(&intent.payload).expect("decodes"),
        KeyAdd {
            pubkey: "ff00".into(),
            label: String::new()
        }
    );
}

/// A minted ticket is the answer to the key that was pasted: the ticket the
/// kernel reports spends the key and its label, and nothing else.
#[test]
fn a_minted_ticket_spends_the_key_it_was_minted_for() {
    let (frame, props, _) = connected(&facts(), 2);
    let frame = tick_native(press(&frame, "Account"));
    let frame = tick_native(type_into(&frame, "paste its ed25519 key (hex)…", "ff00"));
    let frame = tick_native(press(&frame, "Mint ticket"));
    assert_eq!(one_intent(&frame).kind, "settings.key_add");

    let minted = Session {
        account_ticket: r#"{"add_key":{}}"#.into(),
        ..facts()
    };
    let frame = tick_native(vec![item(props, &encoded(&minted))]);
    assert!(
        button_disabled(&frame, "Mint ticket"),
        "an empty key draft mints nothing: {:?}",
        texts(&frame)
    );
}

/// THE LAST KEY IS NEVER OFFERED FOR REMOVAL — consensus refuses it ("cannot
/// remove the last key of an account"), and the rows the view reads are what
/// say how many are left.
#[test]
fn the_last_key_is_never_offered_for_removal() {
    let (frame, _, _) = connected(&facts(), 1);
    let frame = tick_native(press(&frame, "Account"));
    assert!(
        button_disabled(&frame, "Remove"),
        "one association is the last one: {:?}",
        texts(&frame)
    );

    let (frame, _, _) = connected(&facts(), 2);
    let frame = tick_native(press(&frame, "Account"));
    assert!(
        !button_disabled(&frame, "Remove"),
        "two associations, one is removable: {:?}",
        texts(&frame)
    );
}

/// No raw token reaches the screen: the keystore's `encrypted` reads as a
/// sentence, an unanswered roster reads as a wait rather than a blank badge,
/// and the QR countdown says what it counts.
#[test]
fn raw_tokens_read_as_words_and_a_pending_read_says_so() {
    let session = Session {
        settings_key_state: "encrypted".into(),
        account_ceremony_phase: "show_qr".into(),
        account_ceremony_qr: "https://auth.example/c".into(),
        account_ceremony_detail: "Scan this with your phone.".into(),
        account_ceremony_left: "1:07".into(),
        ..facts()
    };
    boot_native();
    let frame = tick_native(Vec::new());
    let props = request(&frame, "settings.props").id;
    // the session is in, the standing and key reads are still out
    let frame = tick_native(vec![item(props, &encoded(&session))]);
    let frame = tick_native(press(&frame, "Account"));
    assert!(has_text(&frame, "Reading…"), "{:?}", texts(&frame));
    assert!(!has_text(&frame, "validator"), "{:?}", texts(&frame));
    assert!(
        has_text(&frame, "This code expires in 1:07"),
        "{:?}",
        texts(&frame)
    );
    let frame = tick_native(press(&frame, "Security"));
    assert!(has_text(&frame, "Encrypted on disk"), "{:?}", texts(&frame));
    assert!(!has_text(&frame, "encrypted"), "{:?}", texts(&frame));
}

/// The rail tabs the settings cards link to leave as the one `tab` intent —
/// the view never opens another tab itself.
#[test]
fn a_link_to_another_tab_leaves_as_an_intent() {
    let (frame, _, _) = connected(&facts(), 2);
    let frame = tick_native(press(&frame, "Network"));
    let frame = tick_native(press(&frame, "Open members"));
    let intent = one_intent(&frame);
    assert_eq!(intent.kind, "settings.tab");
    assert_eq!(
        serde_json::from_slice::<Tab>(&intent.payload).expect("decodes"),
        Tab {
            tab: "members".into()
        }
    );
}

// ---------- updates ----------

/// Without a launcher the Updates group says so and offers no control;
/// installed, its rows read the facts and each control leaves as its own
/// intent: `Check now` while idle, `Restart to update` only while staged,
/// `Roll back to <previous>` only while idle with a previous release.
#[test]
fn the_updates_group_reads_the_facts_and_each_control_is_one_intent() {
    let (frame, props, _) = connected(&facts(), 2);
    assert!(
        has_text(
            &frame,
            "Updates unavailable: not installed through the launcher."
        ),
        "{:?}",
        texts(&frame)
    );
    let mut root = frame.root.clone().expect("a tree");
    let mut update_buttons = 0;
    root.for_each_mut(&mut |node| {
        if let Node::Button { key, .. } = node
            && key.starts_with("settings/update-")
        {
            update_buttons += 1;
        }
    });
    assert_eq!(update_buttons, 0, "no control without a launcher");

    let idle = Session {
        update_state: "idle".into(),
        update_current: "1a2b3c4".into(),
        update_previous: "9f8e7d6".into(),
        update_checked: "5 min ago".into(),
        update_note: "Up to date.".into(),
        ..facts()
    };
    let frame = tick_native(vec![item(props, &encoded(&idle))]);
    for expected in ["1a2b3c4", "stable", "5 min ago", "Up to date."] {
        assert!(
            has_text(&frame, expected),
            "missing {expected:?} in {:?}",
            texts(&frame)
        );
    }
    assert!(!button_disabled(&frame, "Check now"));
    let pressed = tick_native(press(&frame, "Check now"));
    assert_eq!(one_intent(&pressed).kind, "settings.update_check");
    let pressed = tick_native(press(&frame, "Roll back to 9f8e7d6"));
    assert_eq!(one_intent(&pressed).kind, "settings.update_rollback");

    let staged = Session {
        update_state: "staged".into(),
        update_staged_display: "2026.09.2+abc1234".into(),
        ..idle.clone()
    };
    let frame = tick_native(vec![item(props, &encoded(&staged))]);
    assert!(has_text(&frame, "2026.09.2+abc1234"), "{:?}", texts(&frame));
    assert!(
        button_disabled(&frame, "Check now"),
        "staged: nothing to check for"
    );
    let pressed = tick_native(press(&frame, "Restart to update"));
    assert_eq!(one_intent(&pressed).kind, "settings.update_restart");
    let mut root = frame.root.clone().expect("a tree");
    let mut rollback_offered = false;
    root.for_each_mut(&mut |node| {
        if let Node::Button { key, .. } = node
            && key == "settings/update-rollback"
        {
            rollback_offered = true;
        }
    });
    assert!(!rollback_offered, "a rollback is offered only while idle");

    let busy = Session {
        update_busy: true,
        ..idle
    };
    let frame = tick_native(vec![item(props, &encoded(&busy))]);
    assert!(has_text(&frame, "Checking…"), "{:?}", texts(&frame));
    assert!(button_disabled(&frame, "Check now"));
}
