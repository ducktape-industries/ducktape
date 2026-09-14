//! The native menu bar mirrors session facts; choosing a row reuses the
//! desktop's ordinary messages. macOS owns the status item and menu loop.

use crate::{AppMessage as Message, Appearance, Ducktape, backend};
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use gpui_kit::App;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    icon: usize,
    badge: String,
    tooltip: String,
    labels: [String; 22],
    visible: [bool; 22],
}

impl Snapshot {
    fn of(state: &Ducktape) -> Self {
        let icon = icon_index(state.connected, state.bell_unread);
        let mut labels = std::array::from_fn(|_| String::new());
        for (row, label) in [
            (3, "Open Ducktape"),
            (5, "Go to"),
            (6, "Chat"),
            (7, "Pages"),
            (8, "Node"),
            (9, "Settings"),
            (13, "Leave huddle"),
            (14, "Appearance"),
            (18, "Copy node key"),
            (19, "Reconnect"),
            (21, "Quit Ducktape"),
        ] {
            labels[row] = label.into();
        }
        labels[0] = match state.network_name.is_empty() {
            true => "No network".into(),
            false => state.network_name.clone(),
        };
        labels[1] = state.status.clone();
        labels[4] = backend::tray_bell_row(state.bell_unread);
        labels[11] =
            backend::tray_huddle_row(state.huddle_joined, state.huddle_channel_name.clone());
        labels[12] = match state.call_muted {
            true => "Unmute".into(),
            false => "Mute".into(),
        };
        labels[15] =
            backend::tray_choice_row("Light".into(), state.appearance == Appearance::Light);
        labels[16] = backend::tray_choice_row("Dark".into(), state.appearance == Appearance::Dark);
        let mut visible = [true; 22];
        for row in [4, 5, 18, 19] {
            visible[row] = state.console_win.is_some();
        }
        visible[11] = state.huddle_joined;
        Self {
            icon,
            badge: backend::tray_badge(state.bell_unread),
            tooltip: backend::tray_tooltip(state.network_name.clone(), state.status.clone()),
            labels,
            visible,
        }
    }
}

fn icon_index(connected: bool, unread: i64) -> usize {
    [!connected, unread > 0]
        .iter()
        .position(|selected| *selected)
        .unwrap_or(2)
}

pub fn message(row: usize) -> Option<Message> {
    match row {
        3 => Some(Message::TrayOpen),
        4 => Some(Message::TrayOpenBell),
        6 => Some(Message::TrayGoChat),
        7 => Some(Message::TrayGoPages),
        8 => Some(Message::TrayGoNode),
        9 => Some(Message::TrayGoSettings),
        12 => Some(Message::ToggleCallMute),
        13 => Some(Message::LeaveHuddleHere),
        15 => Some(Message::SetAppearanceLight),
        16 => Some(Message::SetAppearanceDark),
        18 => Some(Message::TrayCopyNodeKey),
        19 => Some(Message::TrayReconnect),
        21 => Some(Message::TrayQuit),
        _ => None,
    }
}

/// Rows that sit directly in the status-item menu, in menu order. The rest
/// live inside their submenu for the menu's whole life (5 → 6..=9, 11 → 12..=13,
/// 14 → 15..=16) and never attach or detach on their own.
const TOP_LEVEL: [usize; 14] = [0, 1, 2, 3, 4, 5, 10, 11, 14, 17, 18, 19, 20, 21];

/// Rows the layout never shows a label for (separators).
const SEPARATORS: [usize; 4] = [2, 10, 17, 20];

/// Each submenu row with the child rows it holds, in menu order.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const SUBMENUS: [(usize, &[usize]); 3] = [(5, &[6, 7, 8, 9]), (11, &[12, 13]), (14, &[15, 16])];

/// Where `row` sits once every top-level row `next` shows is attached: the
/// count of shown top-level rows before it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn attached_position(next: &Snapshot, row: usize) -> usize {
    TOP_LEVEL
        .iter()
        .filter(|candidate| **candidate < row && next.visible[**candidate])
        .count()
}

/// What the live menu must do to move from `previous` to `next`, per row.
/// The menu object itself is never replaced: a relabel is `set_text` on the
/// row's handle, a show/hide is an insert/remove of that same handle. With no
/// `previous` (first sync) every row is relabelled and every shown top-level
/// row is attached.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct MenuDiff {
    relabelled: Vec<usize>,
    hidden: Vec<usize>,
    shown: Vec<usize>,
}

impl MenuDiff {
    fn between(previous: Option<&Snapshot>, next: &Snapshot) -> Self {
        let relabelled = (0..next.labels.len())
            .filter(|row| !SEPARATORS.contains(row))
            .filter(|row| previous.is_none_or(|old| old.labels[*row] != next.labels[*row]))
            .collect();
        let hidden = TOP_LEVEL
            .into_iter()
            .filter(|row| !next.visible[*row])
            .filter(|row| previous.is_some_and(|old| old.visible[*row]))
            .collect();
        let shown = TOP_LEVEL
            .into_iter()
            .filter(|row| next.visible[*row])
            .filter(|row| previous.is_none_or(|old| !old.visible[*row]))
            .collect();
        Self {
            relabelled,
            hidden,
            shown,
        }
    }
}

pub struct Tray {
    snapshot: Option<Snapshot>,
    #[cfg(target_os = "macos")]
    native: Option<native::StatusItem>,
}

/// Call once on GPUI's foreground thread after its native event loop starts.
pub fn init(_: &mut App) -> (Tray, UnboundedReceiver<usize>) {
    let (send, receive) = unbounded();
    #[cfg(target_os = "macos")]
    let native = match native::StatusItem::new(send) {
        Ok(tray) => Some(tray),
        Err(error) => {
            tracing::warn!(target:"ducktape::app",reason="tray_init_failed",%error,"native status item unavailable");
            None
        }
    };
    #[cfg(not(target_os = "macos"))]
    drop(send);
    (
        Tray {
            snapshot: None,
            #[cfg(target_os = "macos")]
            native,
        },
        receive,
    )
}

impl Tray {
    #[cfg(test)]
    pub(crate) fn without_status_item() -> Self {
        Self {
            snapshot: None,
            #[cfg(target_os = "macos")]
            native: None,
        }
    }

    pub fn sync(&mut self, state: &Ducktape) {
        let next = Snapshot::of(state);
        if self.snapshot.as_ref() == Some(&next) {
            return;
        }
        #[cfg(target_os = "macos")]
        if let Some(native) = &mut self.native {
            if let Err(error) = native.sync(self.snapshot.as_ref(), &next) {
                tracing::warn!(target:"ducktape::app",reason="tray_update_failed",%error,"native status item update refused");
                return;
            }
        }
        self.snapshot = Some(next);
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::{MenuDiff, SEPARATORS, SUBMENUS, Snapshot, TOP_LEVEL, attached_position};
    use futures::channel::mpsc::UnboundedSender;
    use tray_icon::{
        Icon, TrayIcon, TrayIconBuilder,
        menu::{IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    };

    /// One row's live handle. Built once and owned for the tray's lifetime;
    /// every later change mutates it in place. Replacing the `NSMenu` is
    /// wrong twice over: AppKit dismisses the open menu, and muda stores a raw
    /// `*const MenuChild` inside each `NSMenuItem` that it dereferences on
    /// click — a click landing on an item whose `Menu` was dropped reads freed
    /// memory inside a non-unwinding objc callback and aborts the process.
    enum Row {
        Item(MenuItem),
        Submenu(Submenu),
        Separator(PredefinedMenuItem),
    }

    impl Row {
        fn handle(&self) -> &dyn IsMenuItem {
            match self {
                Row::Item(item) => item,
                Row::Submenu(submenu) => submenu,
                Row::Separator(separator) => separator,
            }
        }

        fn set_text(&self, text: &str) {
            match self {
                Row::Item(item) => item.set_text(text),
                Row::Submenu(submenu) => submenu.set_text(text),
                Row::Separator(_) => {}
            }
        }
    }

    pub(super) struct StatusItem {
        tray: TrayIcon,
        icons: [Icon; 3],
        menu: Menu,
        rows: [Row; 22],
    }

    impl StatusItem {
        pub(super) fn new(send: UnboundedSender<usize>) -> Result<Self, String> {
            let pixels: [&[u8]; 3] = [
                include_bytes!("../assets/tray-offline.rgba"),
                include_bytes!("../assets/tray-unread.rgba"),
                include_bytes!("../assets/tray.rgba"),
            ];
            let mut icons = pixels
                .into_iter()
                .map(|bytes| Icon::from_rgba(bytes.to_vec(), 128, 128));
            let icons = [
                icons.next().unwrap().map_err(|error| error.to_string())?,
                icons.next().unwrap().map_err(|error| error.to_string())?,
                icons.next().unwrap().map_err(|error| error.to_string())?,
            ];
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                let Some(row) = event
                    .id
                    .as_ref()
                    .strip_prefix("ducktape-tray-")
                    .and_then(|row| row.parse::<usize>().ok())
                else {
                    return;
                };
                let _ = send.unbounded_send(row);
            }));
            let rows = rows().map_err(|error| error.to_string())?;
            let menu = Menu::new();
            let tray = TrayIconBuilder::new()
                .with_id("ducktape")
                .with_icon(icons[0].clone())
                .with_icon_as_template(false)
                .with_menu_on_left_click(true)
                .with_menu(Box::new(menu.clone()))
                .build()
                .map_err(|error| error.to_string())?;
            Ok(Self {
                tray,
                icons,
                menu,
                rows,
            })
        }

        pub(super) fn sync(
            &mut self,
            previous: Option<&Snapshot>,
            next: &Snapshot,
        ) -> Result<(), String> {
            if previous.is_none_or(|old| old.icon != next.icon) {
                self.tray
                    .set_icon(Some(self.icons[next.icon].clone()))
                    .map_err(|error| error.to_string())?;
            }
            if previous.is_none_or(|old| old.badge != next.badge) {
                self.tray.set_title(Some(&next.badge));
            }
            if previous.is_none_or(|old| old.tooltip != next.tooltip) {
                self.tray
                    .set_tooltip(Some(&next.tooltip))
                    .map_err(|error| error.to_string())?;
            }
            let diff = MenuDiff::between(previous, next);
            for row in diff.relabelled {
                self.rows[row].set_text(&next.labels[row]);
            }
            for row in diff.hidden {
                self.menu
                    .remove(self.rows[row].handle())
                    .map_err(|error| error.to_string())?;
            }
            for row in diff.shown {
                self.menu
                    .insert(self.rows[row].handle(), attached_position(next, row))
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        }
    }

    fn item(row: usize, enabled: bool) -> MenuItem {
        MenuItem::with_id(format!("ducktape-tray-{row}"), "", enabled, None)
    }

    /// Every row's handle, labels blank until the first sync fills them. Rows 0
    /// and 1 are facts, not actions, so they stay disabled for good. Children
    /// are attached to their submenu here and stay there; top-level rows are
    /// attached to the menu by `sync` as the snapshot shows them.
    fn rows() -> Result<[Row; 22], tray_icon::menu::Error> {
        let mut rows: [Option<Row>; 22] = std::array::from_fn(|_| None);
        for row in SEPARATORS {
            rows[row] = Some(Row::Separator(PredefinedMenuItem::separator()));
        }
        for (parent, children) in SUBMENUS {
            let submenu = Submenu::new("", true);
            for child in children {
                let child_item = item(*child, true);
                submenu.append(&child_item)?;
                rows[*child] = Some(Row::Item(child_item));
            }
            rows[parent] = Some(Row::Submenu(submenu));
        }
        for row in TOP_LEVEL {
            let is_fact = row < 2;
            let unbuilt = rows[row].is_none();
            if unbuilt {
                rows[row] = Some(Row::Item(item(row, !is_fact)));
            }
        }
        Ok(rows.map(|row| row.expect("every row in the 22-row layout is built")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_priority_and_menu_routes_match_the_desktop() {
        assert_eq!(icon_index(false, 3), 0);
        assert_eq!(icon_index(true, 3), 1);
        assert_eq!(icon_index(true, 0), 2);
        assert!(matches!(message(3), Some(Message::TrayOpen)));
        assert!(matches!(message(21), Some(Message::TrayQuit)));
        for row in [0, 1, 2, 5, 10, 11, 14, 17, 20, 22] {
            assert!(message(row).is_none());
        }
    }

    /// The status-item menu is built once and mutated in place. A second
    /// `Menu::new()` or any `set_menu` call would swap the `NSMenu` under an
    /// open menu (dismissing it) and free the `MenuChild`s its items still
    /// point at (aborting on the next click).
    #[test]
    fn the_status_item_menu_is_built_once_and_never_replaced() {
        let source = include_str!("tray.rs");
        let code: String = source
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .expect("the test module ends the file")
            .to_owned();
        assert_eq!(code.matches("Menu::new()").count(), 1);
        assert_eq!(code.matches(".set_menu(").count(), 0);
        assert_eq!(code.matches("Submenu::new(").count(), 1);
    }

    #[test]
    fn the_layout_places_every_row_exactly_once() {
        let children: Vec<usize> = SUBMENUS
            .iter()
            .flat_map(|(_, children)| children.iter().copied())
            .collect();
        for row in 0..22 {
            let top_level = TOP_LEVEL.contains(&row);
            let child = children.contains(&row);
            assert!(
                top_level != child,
                "row {row} must be top-level or a child, not both"
            );
        }
        for (parent, _) in SUBMENUS {
            assert!(TOP_LEVEL.contains(&parent));
            assert!(!SEPARATORS.contains(&parent));
        }
        for separator in SEPARATORS {
            assert!(TOP_LEVEL.contains(&separator));
        }
    }

    /// A new block only relabels the status row: the open menu keeps every
    /// handle it was built with, so it stays open.
    #[test]
    fn a_new_block_relabels_the_status_row_and_nothing_else() {
        let (mut app, _) = Ducktape::boot();
        app.status = "Live · block 41".into();
        let before = Snapshot::of(&app);
        app.status = "Live · block 42".into();
        let after = Snapshot::of(&app);

        assert_eq!(
            MenuDiff::between(Some(&before), &after),
            MenuDiff {
                relabelled: vec![1],
                hidden: vec![],
                shown: vec![],
            }
        );
        assert_eq!(MenuDiff::between(Some(&after), &after), MenuDiff::default());
    }

    #[test]
    fn the_first_sync_labels_every_row_and_attaches_the_shown_ones() {
        let (app, _) = Ducktape::boot();
        let snapshot = Snapshot::of(&app);

        let diff = MenuDiff::between(None, &snapshot);
        let labelled: Vec<usize> = (0..22).filter(|row| !SEPARATORS.contains(row)).collect();
        assert_eq!(diff.relabelled, labelled);
        assert!(diff.hidden.is_empty());
        assert_eq!(diff.shown, vec![0, 1, 2, 3, 10, 14, 17, 20, 21]);
        assert_eq!(attached_position(&snapshot, 0), 0);
        assert_eq!(attached_position(&snapshot, 21), 8);
    }

    #[test]
    fn console_and_huddle_rows_attach_and_detach_in_menu_order() {
        let (mut app, _) = Ducktape::boot();
        let closed = Snapshot::of(&app);
        app.console_win = Some(crate::shell::WindowKey::unique());
        app.huddle_joined = true;
        app.huddle_channel_name = "general".into();
        let open = Snapshot::of(&app);

        let opened = MenuDiff::between(Some(&closed), &open);
        assert_eq!(opened.relabelled, vec![11]);
        assert!(opened.hidden.is_empty());
        assert_eq!(opened.shown, vec![4, 5, 11, 18, 19]);
        assert_eq!(attached_position(&open, 4), 4);
        assert_eq!(attached_position(&open, 5), 5);
        assert_eq!(attached_position(&open, 11), 7);
        assert_eq!(attached_position(&open, 18), 10);
        assert_eq!(attached_position(&open, 19), 11);

        let shut = MenuDiff::between(Some(&open), &closed);
        assert_eq!(shut.relabelled, vec![11]);
        assert_eq!(shut.hidden, vec![4, 5, 11, 18, 19]);
        assert!(shut.shown.is_empty());
    }
}
