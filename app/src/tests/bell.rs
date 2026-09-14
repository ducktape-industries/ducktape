use super::*;

fn row(seq: i64) -> backend::BellItem {
    backend::BellItem {
        seq,
        reason: "mention".into(),
        actor: "account:9".into(),
        kind: "added".into(),
        ..Default::default()
    }
}

#[test]
fn bell_acknowledgement_reads_only_the_admitted_watermark() {
    let (mut app, _) = Ducktape::boot();
    app.connect_generation = 7;
    app.account_number = "4".into();
    app.bell_items = vec![row(3), row(2), row(1)];
    app.bell_unread = 3;
    app.bell_marking = true;
    let _ = app.update(AppMessage::BellMarked(
        7,
        "4".into(),
        backend::BellDelta {
            kind: "read".into(),
            up_to_seq: 2,
            ..Default::default()
        },
    ));
    assert!(!app.bell_marking);
    assert_eq!(app.bell_unread, 1);
    assert!(!app.bell_items[0].read);
    assert!(app.bell_items[1].read);
    let _ = app.update(AppMessage::BellLoaded(
        7,
        "4".into(),
        backend::BellData {
            unread: 2,
            items: vec![row(2), row(1)],
            presentations: vec![],
        },
    ));
    assert_eq!(app.bell_items[0].seq, 3);
    assert_eq!(app.bell_unread, 1);
}

#[test]
fn bell_failed_read_is_visible_and_stale_accounts_cannot_finish_it() {
    let (mut app, _) = Ducktape::boot();
    app.connect_generation = 7;
    app.account_number = "4".into();
    app.bell_items = vec![row(1)];
    app.bell_unread = 1;
    app.bell_marking = true;
    let delta = backend::BellDelta {
        kind: "read".into(),
        up_to_seq: 1,
        ..Default::default()
    };
    let _ = app.update(AppMessage::BellMarked(6, "4".into(), delta.clone()));
    let _ = app.update(AppMessage::BellMarked(7, "5".into(), delta));
    assert!(app.bell_marking);
    assert_eq!(app.bell_unread, 1);
    let _ = app.update(AppMessage::BellMarkFailed(
        7,
        "4".into(),
        backend::AppError {
            message: "Cannot reach the node".into(),
            committed: false,
        },
    ));
    assert!(!app.bell_marking);
    assert_eq!(app.bell_error, "Cannot reach the node");
    assert_eq!(app.bell_unread, 1);
    assert!(!app.bell_items[0].read);
}

#[test]
fn bell_page_navigation_cannot_outlive_its_connection_or_account() {
    use futures::StreamExt as _;
    for change in ["none", "connection", "account"] {
        let (mut app, _) = Ducktape::boot();
        app.connect_generation = 7;
        app.account_generation = 1;
        app.account_number = "4".into();
        app.shell_tab = ShellTab::Chat;
        app.loading = false;
        let task = app.update(AppMessage::BellOpenItem(
            7,
            "4".into(),
            backend::BellPresentation {
                target: BellTarget::Page,
                object: "page-a".into(),
                anchor: "block-a".into(),
                ..Default::default()
            },
        ));
        let queued = futures::executor::block_on(task.into_stream().collect::<Vec<_>>());
        assert_eq!(
            queued.len(),
            1,
            "hold the actual queued navigation, not an invented message"
        );
        match change {
            "connection" => {
                let _ = app.update(AppMessage::ConnectFailed(backend::HydrationError {
                    generation: 7,
                    message: "disconnected".into(),
                }));
            }
            "account" => {
                let _ = app.update(AppMessage::AccountLoaded(backend::AccountData {
                    generation: 1,
                    exists: true,
                    number: "5".into(),
                    name: "Bob".into(),
                    bio: String::new(),
                }));
            }
            _ => {}
        }
        for message in queued {
            let _ = app.update(message);
        }
        assert_eq!(
            app.shell_tab == ShellTab::Pages,
            change == "none",
            "queued page navigation after {change}"
        );
    }
}

#[gpui_kit::test]
async fn bell_controls_render_context_and_admit_read_from_the_real_button(
    cx: &mut gpui_kit::TestAppContext,
) {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{VisualTestContext, px, size};
    let _guard = crate::module_view::tests::blocking_connection_turn();
    let (mut app, _) = Ducktape::boot();
    app.console_win = Some(crate::shell::WindowKey::unique());
    app.account_number = "4".into();
    app.bell_open = true;
    app.bell_unread = 1;
    app.bell_items = vec![row(17)];
    app.bell_presentations = vec![backend::BellPresentation {
        seq: 17,
        title: "Alice mentioned you".into(),
        detail: "Please review the launch checklist.".into(),
        target: BellTarget::Message,
        object: "general".into(),
        number: 42,
        ..Default::default()
    }];
    cx.update(gpui_kit::init);
    let mut view = None;
    let handle = cx.open_window(size(px(1120.), px(720.)), |window, cx| {
        let native = crate::shell::test_window(app, crate::shell::WindowKind::Console, window, cx);
        view = Some(native.clone());
        gpui_kit::component::Root::new(native, window, cx)
    });
    let view = view.unwrap();
    let mut native = VisualTestContext::from_window(handle.into(), cx);
    native.update(|window, cx| window.render_frame(cx));
    native.update(|window, _| {
        let row = window.find("notification/17");
        assert!(row.visible());
        let label = row
            .label()
            .expect("the native button exposes its actual context");
        assert!(label.contains("Alice mentioned you") && label.contains("launch checklist"));
    });
    let click = |native: &mut VisualTestContext, key: &str| {
        native.update(|window, cx| {
            let position = window.find(key.to_owned()).bounds().center();
            window.dispatch_event(
                gpui_kit::PlatformInput::MouseDown(gpui_kit::MouseDownEvent {
                    position,
                    button: gpui_kit::MouseButton::Left,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
            window.dispatch_event(
                gpui_kit::PlatformInput::MouseUp(gpui_kit::MouseUpEvent {
                    position,
                    button: gpui_kit::MouseButton::Left,
                    modifiers: Default::default(),
                    click_count: 1,
                }),
                cx,
            );
        });
    };
    click(&mut native, "bell-mark-read");
    view.read_with(&native, |view, cx| {
        assert!(
            view.test_state(cx).bell_marking,
            "real native button admits the read"
        )
    });
    view.update(&mut native, |view, cx| {
        let generation = view.test_state(cx).bell_head_generation;
        // Deliver through the actual completion boundary: it retires the
        // admitted request before this UI-only test yields to its executor.
        // Injecting the inner BellMarked alone leaves a live RPC behind.
        view.test_dispatch(
            AppMessage::BellHeadReply(
                generation,
                Box::new(AppMessage::BellMarked(
                    0,
                    "4".into(),
                    backend::BellDelta {
                        kind: "read".into(),
                        up_to_seq: 17,
                        ..Default::default()
                    },
                )),
            ),
            cx,
        )
    });
    view.read_with(&native, |view, cx| {
        assert_eq!(view.test_state(cx).bell_unread, 0);
        assert!(view.test_state(cx).bell_items[0].read);
        assert!(view.test_state(cx).bell_head_task.is_none());
    });
    native.update(|window, cx| window.render_frame(cx));
    click(&mut native, "notification/17");
    cx.condition(&view, |view, cx| {
        view.test_state(cx).active_channel == "general"
    })
    .await;
    view.read_with(&native, |view, cx| {
        let app = view.test_state(cx);
        assert!(!app.bell_open);
        assert_eq!(app.active_channel, "general");
        assert_eq!(
            app.chat_land_seq, 42,
            "navigate to source message, not inbox sequence 17"
        );
    });
}
