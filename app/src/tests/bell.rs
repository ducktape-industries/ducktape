//! What the APP still owns of the bell: a number on its own chrome, and the
//! surface the `inbox` view is seated in. The rows, their wording and the
//! unread rule are the view's — those are proved in `crates/views/inbox`.

use super::*;

#[test]
fn a_late_count_cannot_land_on_a_connection_or_account_it_was_not_asked_for() {
    let (mut app, _) = Ducktape::boot();
    app.connect_generation = 7;
    app.account_number = "4".into();
    app.bell_unread = 3;
    let _ = app.update(AppMessage::BellUnreadLoaded(6, "4".into(), Some(0)));
    assert_eq!(app.bell_unread, 3, "a previous connection's count");
    let _ = app.update(AppMessage::BellUnreadLoaded(7, "5".into(), Some(0)));
    assert_eq!(app.bell_unread, 3, "another account's count");
    let _ = app.update(AppMessage::BellUnreadLoaded(7, "4".into(), Some(9)));
    assert_eq!(app.bell_unread, 9);
}

#[test]
fn an_unanswered_count_keeps_the_number_the_rail_has() {
    assert_eq!(backend::bell_count(Ok(4)), Some(4));
    assert_eq!(
        backend::bell_count(Err(backend::AppError {
            message: "Cannot reach the node".into(),
            committed: false,
        })),
        None,
        "a badge that blinks to zero on a dropped socket says everything is read"
    );
    let (mut app, _) = Ducktape::boot();
    app.connect_generation = 7;
    app.account_number = "4".into();
    app.bell_unread = 3;
    let _ = app.update(AppMessage::BellUnreadLoaded(7, "4".into(), None));
    assert_eq!(app.bell_unread, 3);
}

#[test]
fn opening_a_row_s_address_leaves_the_overlay_behind() {
    let (mut app, _) = Ducktape::boot();
    app.bell_open = true;
    app.network_chain_id = "dognet#0000".into();
    let _ = app.update(AppMessage::OpenMessageLink(
        "duck://page/page-a?net=0000".into(),
    ));
    assert!(!app.bell_open);
}

/// THE APP FOLDS NO INBOX. Which notifications exist, which of them are
/// noise, how each is worded and where its door leads are the `inbox`
/// view's, and a view swap must be able to change every one of them. The
/// only thing `app/src/backend` may do with the inbox is ask that view for
/// its own count — so the backend tree is parsed for every name the deleted
/// host fold was built out of.
#[test]
fn the_backend_builds_no_inbox_fold() {
    let backend = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/backend");
    let mut files = Vec::new();
    super::rooms::collect_rust_files(&backend, &mut files);
    assert!(!files.is_empty(), "the walk found no backend source at all");
    for file in files {
        let source = std::fs::read_to_string(&file).expect("read a backend source");
        for folded in [
            "BellItem",
            "BellDelta",
            "BellPresentation",
            "BellData",
            "bell_visible_items",
            "bell_unread_count",
            "bell_missing_items",
            "bell_presentation",
            "bell_openable",
            "apply_bell",
            "merge_bell_loaded",
            "merge_bell_presentations",
            "load_bell_presentations",
        ] {
            assert!(
                !source.contains(folded),
                "{} folds the inbox: {folded}",
                file.display()
            );
        }
    }
    assert!(
        !backend.join("bell.rs").exists(),
        "the host fold's file is back"
    );
}

#[gpui_kit::test]
async fn the_bell_overlay_seats_the_inbox_view_and_keeps_the_tab_s_own_seat(
    cx: &mut gpui_kit::TestAppContext,
) {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{VisualTestContext, px, size};
    let _guard = crate::module_view::tests::blocking_connection_turn();
    let (mut app, _) = Ducktape::boot();
    app.console_win = Some(crate::shell::WindowKey::unique());
    app.account_number = "4".into();
    app.shell_tab = ShellTab::Chat;
    app.bell_open = true;
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
    view.read_with(&native, |view, _| {
        let (tab, overlay) = view.test_seats();
        assert_eq!(tab, Some("chat"), "the tab underneath keeps its seat");
        assert!(overlay, "the bell overlay seats the inbox view");
    });
    // Closing returns the overlay's seat; the tab's is untouched.
    view.update(&mut native, |view, cx| {
        view.test_dispatch(AppMessage::CloseBell, cx)
    });
    native.update(|window, cx| window.render_frame(cx));
    view.read_with(&native, |view, _| {
        let (tab, overlay) = view.test_seats();
        assert_eq!(tab, Some("chat"));
        assert!(!overlay);
    });
}
