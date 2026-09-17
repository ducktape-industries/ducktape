//! Opt-in proof driven by the external deployment harness, with this host test
//! binary compiled before any example artifact is built or installed.
use super::*;

fn node(guest: &Guest, key: &str) -> wire::Node {
    let mut root = guest.frame.root.clone().expect("view tree");
    let mut found = None;
    root.for_each_mut(&mut |node| {
        if node.key() == Some(key) {
            found = Some(node.clone())
        }
    });
    found.unwrap_or_else(|| panic!("view lacks {key}"))
}
fn press(guest: &mut Guest, key: &str) {
    let wire::Node::Button {
        on_press: Some(message),
        ..
    } = node(guest, key)
    else {
        panic!("enabled button")
    };
    guest.pending.push(wire::Event::Message(message));
}
async fn output_matching(guest: &mut Guest, accept: impl Fn(&str) -> bool) {
    let mut changes = guest.replies.changes();
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let again = guest.redraw(&None);
            assert!(guest.fault.is_none(), "{:?}", guest.fault);
            let wire::Node::Text { content, .. } = node(guest, "output") else {
                panic!("output text")
            };
            if accept(&content) {
                return;
            }
            if !again {
                changes.changed().await.expect("reply event")
            }
        }
    })
    .await
    .expect("live service response");
}

async fn output(guest: &mut Guest, expected: &str) {
    output_matching(guest, |text| text == expected).await;
}

#[tokio::test]
#[ignore = "run through the frozen-host external extension harness"]
async fn file_loaded_view_uses_live_signed_http_and_bidirectional_service() {
    let _turn = tests::connection_turn().await;
    let rpc = std::env::var("DUCK_EXTENSION_RPC").expect("harness RPC");
    let key = std::env::var("DUCK_EXTENSION_KEY").expect("harness wallet");
    let path = std::env::var("DUCK_EXTENSION_VIEW").expect("deployed view file");
    let expected = std::env::var("DUCK_EXTENSION_EXPECT").expect("service reply");
    let snapshot = std::env::var("DUCK_EXTENSION_SNAPSHOT").expect("view state file");
    crate::backend::seat_signer(
        key.into(),
        zeroize::Zeroizing::new("extension-test-password".into()),
    )
    .await
    .unwrap();
    connection().lock().unwrap().client = Some(crate::backend::rpc_client(&rpc).unwrap());
    let mut guest = Guest::load_from("extension-policy", std::path::Path::new(&path)).unwrap();
    let saved = std::path::Path::new(&snapshot);
    if saved.exists() {
        guest
            .restore(&std::fs::read(saved).unwrap(), "replacement probe")
            .unwrap();
    }
    guest.redraw(&None);
    let wire::Node::Text { content: title, .. } = node(&guest, "title") else {
        panic!("view title")
    };
    let expected_title = match expected.as_str() {
        "#HELLO" => "Extension probe",
        "#hello" => "Extension updated",
        _ => panic!("unknown fixture response"),
    };
    assert_eq!(title, expected_title);
    let wire::Node::Input {
        on_input, value, ..
    } = node(&guest, "draft")
    else {
        panic!("draft input")
    };
    if saved.exists() {
        assert_eq!(
            value, "#Socket",
            "replacement must restore the previous draft"
        );
    }
    guest.pending.push(wire::Event::Input {
        handler: on_input,
        text: "#Hello".into(),
    });
    guest.redraw(&None);
    press(&mut guest, "request");
    output(&mut guest, &expected).await;
    press(&mut guest, "record");
    output_matching(&mut guest, |text| text.parse::<u64>().is_ok()).await;
    press(&mut guest, "read");
    output_matching(&mut guest, |text| {
        serde_json::from_str::<serde_json::Value>(text).is_ok_and(|state| state["last"] == "#Hello")
    })
    .await;
    press(&mut guest, "connect");
    guest.redraw(&None);
    let wire::Node::Input { on_input, .. } = node(&guest, "draft") else {
        panic!("draft input")
    };
    guest.pending.push(wire::Event::Input {
        handler: on_input,
        text: "#Socket".into(),
    });
    guest.redraw(&None);
    press(&mut guest, "send");
    let socket_expected = match expected.as_str() {
        "#HELLO" => "#SOCKET",
        "#hello" => "#socket",
        _ => panic!("unknown fixture response"),
    };
    output(&mut guest, socket_expected).await;
    std::fs::write(saved, guest.snapshot().unwrap()).unwrap();
    press(&mut guest, "disconnect");
    guest.redraw(&None);
    drop(guest);
    crate::backend::lock_signer().await;
}

/// The parent drives real governance between acknowledgements. This process
/// keeps the ordinary registry seat alive throughout the deployment change.
#[test]
#[ignore = "run through the frozen-host external extension harness"]
fn registry_discovers_and_replaces_a_live_view() {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, HeadlessAppContext, px, size};
    use std::io::{BufRead as _, Write as _};
    const VIEW: &str = "extension-probe-view";
    const DRAFT: &str = "retained through registry replacement";
    fn render(native: &mut HeadlessAppContext, handle: AnyWindowHandle) {
        native.run_until_parked();
        native
            .update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        native.run_until_parked();
    }
    fn click(native: &mut HeadlessAppContext, handle: AnyWindowHandle, key: &str) {
        native
            .update_window(handle, |_, window, cx| window.click(key.to_owned(), cx))
            .unwrap();
        render(native, handle);
    }
    let _turn = tests::blocking_connection_turn();
    let rpc = std::env::var("DUCK_EXTENSION_RPC").expect("harness RPC");
    let client = crate::backend::rpc_client(&rpc).unwrap();
    runtime().block_on(connected(&client).settled());
    assert!(registered_views().contains(&VIEW), "ordinary tab discovery");
    let seat = mounted(VIEW);
    let (mut app, _) = crate::Ducktape::boot();
    app.console_win = Some(crate::shell::WindowKey::unique());
    app.connected = true;
    app.connected_rpc = rpc;
    let mut native = crate::frame_probe::headless_context();
    // Real node I/O wakes foreground tasks from the views-kernel runtime.
    native.allow_parking();
    let mut desktop = None;
    let handle = native
        .open_window(size(px(1120.), px(900.)), |window, cx| {
            let view =
                crate::shell::test_window(app, crate::shell::WindowKind::Console, window, cx);
            desktop = Some(view.clone());
            cx.new(|cx| gpui_kit::component::Root::new(view, window, cx))
        })
        .unwrap();
    let desktop = desktop.unwrap();
    let handle = handle.into();
    render(&mut native, handle);
    click(&mut native, handle, &format!("view:{VIEW}"));
    desktop.read_with(&native, |desktop, cx| {
        assert_eq!(
            desktop.test_state(cx).shell_tab,
            crate::ShellTab::View(VIEW)
        );
    });
    native
        .update_window(handle, |_, window, _| {
            assert!(
                window.find("draft").visible(),
                "registry view input rendered in the selected tab"
            );
            assert!(
                window.find("read").visible(),
                "registry view button rendered"
            );
        })
        .unwrap();
    click(&mut native, handle, "draft");
    native
        .update_window(handle, |_, window, cx| window.input(DRAFT, cx))
        .unwrap();
    render(&mut native, handle);
    let (old_hash, old_instance, replies) = {
        let mounted = seat.lock().unwrap();
        let Slot::Ready(guest) = &mounted.slot else {
            panic!("registered deployment did not mount");
        };
        let wire::Node::Text { content, .. } = node(guest, "title") else {
            panic!("title");
        };
        assert_eq!(content, "Extension probe");
        let wire::Node::Input { value, .. } = node(guest, "draft") else {
            panic!("draft");
        };
        assert_eq!(value, DRAFT, "native text input reaches the deployed guest");
        (
            guest.hash.expect("verified deployment hash"),
            Arc::downgrade(&guest.alive),
            guest.replies.clone(),
        )
    };
    click(&mut native, handle, "read");
    replies.wait_idle();
    render(&mut native, handle);
    {
        let mounted = seat.lock().unwrap();
        let Slot::Ready(guest) = &mounted.slot else {
            panic!("mounted guest");
        };
        let wire::Node::Text { content, .. } = node(guest, "output") else {
            panic!("output");
        };
        assert!(
            serde_json::from_str::<serde_json::Value>(&content).is_ok(),
            "native button receives a deployed module query response: {content}"
        );
    }
    // stdout is the test driver's event protocol, not application logging.
    println!("EXTENSION mounted");
    std::io::stdout().flush().unwrap();
    let mut command = String::new();
    std::io::stdin().lock().read_line(&mut command).unwrap();
    assert_eq!(command.trim(), "replace");
    runtime().block_on(async { deployments_checked().await.settled().await });
    assert!(registered_views().contains(&VIEW));
    assert!(Arc::ptr_eq(&seat, &mounted(VIEW)), "tab seat preserved");
    render(&mut native, handle);
    {
        let mounted = seat.lock().unwrap();
        let Slot::Ready(guest) = &mounted.slot else {
            panic!("replacement did not mount");
        };
        assert_ne!(guest.hash, Some(old_hash), "new registry bytes installed");
        assert!(
            old_instance.upgrade().is_none(),
            "previous instance retired"
        );
        let wire::Node::Text { content, .. } = node(guest, "title") else {
            panic!("title");
        };
        assert_eq!(content, "Extension updated");
        let wire::Node::Input { value, .. } = node(guest, "draft") else {
            panic!("draft");
        };
        assert_eq!(value, DRAFT);
    }
    native
        .update_window(handle, |_, window, _| {
            assert!(
                window.find("draft").visible(),
                "replacement stays rendered in the same registered tab"
            )
        })
        .unwrap();
    let replacement_output = {
        let mounted = seat.lock().unwrap();
        let Slot::Ready(guest) = &mounted.slot else {
            panic!("replacement guest");
        };
        assert!(
            !Arc::ptr_eq(&replies, &guest.replies),
            "replacement owns a new reply queue"
        );
        let wire::Node::Text { content, .. } = node(guest, "output") else {
            panic!("output");
        };
        content
    };
    // This is an event-driven reply-queue ownership check, not a socket race.
    let mut late = replies.changes();
    replies.inject_item(u64::MAX, Ok(b"retired-instance-only".to_vec()));
    runtime()
        .block_on(late.changed())
        .expect("retired queue arrival event");
    render(&mut native, handle);
    {
        let mounted = seat.lock().unwrap();
        let Slot::Ready(guest) = &mounted.slot else {
            panic!("replacement guest");
        };
        let wire::Node::Text { content, .. } = node(guest, "output") else {
            panic!("output");
        };
        assert_eq!(
            content, replacement_output,
            "retired reply cannot change replacement output"
        );
        let wire::Node::Input { value, .. } = node(guest, "draft") else {
            panic!("draft");
        };
        assert_eq!(value, DRAFT);
        assert!(
            old_instance.upgrade().is_none(),
            "late reply cannot revive retired guest"
        );
    }
    let mut retired = Vec::new();
    replies
        .drain_into(&mut retired)
        .expect("retired reply budget");
    assert!(
        matches!(retired.as_slice(), [wire::Event::Response { id: u64::MAX, result: Ok(bytes), done: true }] if bytes == b"retired-instance-only"),
        "replacement never drains its predecessor's queue"
    );
    println!("EXTENSION replaced");
    std::io::stdout().flush().unwrap();
}

#[tokio::test]
#[ignore = "run through the frozen-host external extension harness after node restart"]
async fn registry_mounts_current_view_after_node_restart() {
    let _turn = tests::connection_turn().await;
    let rpc = std::env::var("DUCK_EXTENSION_RPC").expect("restarted publisher RPC");
    let client = crate::backend::rpc_client(&rpc).unwrap();
    connected(&client).settled().await;
    assert!(registered_views().contains(&"extension-probe-view"));
    let seat = mounted("extension-probe-view");
    let mut mounted = seat.lock().unwrap();
    let Slot::Ready(guest) = &mut mounted.slot else {
        panic!("restarted node did not supply its current registered view");
    };
    guest.redraw(&None);
    let wire::Node::Text { content, .. } = node(guest, "title") else {
        panic!("current deployment title");
    };
    assert_eq!(content, "Extension updated");
    assert!(guest.hash.is_some(), "verified registry deployment");
}
