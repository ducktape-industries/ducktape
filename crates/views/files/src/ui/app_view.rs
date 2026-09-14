//! A shared toolbar above the sidebar, directory and inspector. Dialogs
//! cover the browser; narrowing a pane never hides its navigation controls.

use super::*;
use ducktape_view_guest::{kit::Tone, slots};

/// What is over the browser: nothing, the delete confirm, or a name prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Modal {
    None,
    Deleting,
    Naming,
}

impl FilesView {
    pub(crate) fn view(&self) -> wire::Node {
        native::set_dark(self.dark);
        let viewport = || {
            slots::handler::<(f32, f32), Message>(Box::new(|(width, height)| {
                Some(Message::ViewportChanged(width.into(), height.into()))
            }))
        };
        // The split runs to the edges of the content area: the screen owns
        // its own chrome, and the sensor is layout-transparent over it.
        wire::Node::Sensor {
            key: "FilesView/viewport".into(),
            reset: None,
            on_show: Some(viewport()),
            on_resize: Some(viewport()),
            on_hide: None,
            anticipate: None,
            delay: None,
            child: Box::new(self.screen("FilesView/screen".into())),
        }
    }

    fn screen(&self, key: String) -> wire::Node {
        if !self.connected {
            return self.disconnected(format!("{key}/disconnected"));
        }
        let mut panes = Vec::new();
        if self.sidebar_open {
            panes.push(self.sidebar(format!("{key}/sidebar")));
            panes.push(kit::resize(
                format!("{key}/sidebar-resize"),
                Message::SidebarResized,
            ));
        }
        panes.push(self.main_pane(format!("{key}/main")));
        if self.inspector_open {
            panes.push(kit::resize(
                format!("{key}/inspector-resize"),
                Message::InspectorResized,
            ));
            panes.push(self.inspector(format!("{key}/inspector")));
        }
        let content = kit::filled_column(
            key.clone(),
            vec![
                self.toolbar(format!("{key}/toolbar")),
                native::divider(format!("{key}/toolbar-rule")),
                native::sized(
                    native::spaced(native::row(format!("{key}/panes"), panes), 0.),
                    Some(wire::Length::Fill),
                    Some(wire::Length::Fill),
                ),
            ],
        );
        match self.modal() {
            Modal::Deleting => kit::modal(
                format!("{key}/delete-dialog"),
                content,
                self.confirm_delete(format!("{key}/confirm-delete")),
                Message::DisarmDelete,
            ),
            Modal::Naming => kit::modal(
                format!("{key}/name-dialog"),
                content,
                self.name_dialog(format!("{key}/name-prompt")),
                Message::Prompt(NamePrompt::Closed),
            ),
            Modal::None => content,
        }
    }

    /// The layer over the browser, if any. A delete confirm outranks a name
    /// prompt: the prompt closes when a delete arms, never the reverse.
    pub(super) fn modal(&self) -> Modal {
        let deleting = !self.delete_target.is_empty();
        let naming = self.name_prompt.is_open();
        match (deleting, naming) {
            (true, _) => Modal::Deleting,
            (false, true) => Modal::Naming,
            (false, false) => Modal::None,
        }
    }

    /// NOT CONNECTED IS NOT EMPTY. Every reading arrives over the node; with
    /// it down there is no directory to draw and no write to offer.
    fn disconnected(&self, key: String) -> wire::Node {
        native::sized(
            native::container(
                key.clone(),
                native::empty_state(
                    format!("{key}/plate"),
                    "Not connected",
                    "Click the network name in the titlebar to pick or reconnect a network.",
                ),
            ),
            Some(wire::Length::Fill),
            Some(wire::Length::Fill),
        )
    }

    /// The path, notices, directory as rows or columns, and status bar.
    fn main_pane(&self, key: String) -> wire::Node {
        let mut children = vec![
            self.path_bar(format!("{key}/path-bar")),
            native::divider(format!("{key}/toolbar-rule")),
        ];
        let entry = self.selected_entry();
        let actions_here = !self.inspector_open && !entry.path.is_empty();
        if actions_here {
            children.push(kit::bar(
                format!("{key}/selection"),
                vec![self.row_actions(format!("{key}/selection/actions"), &entry)],
            ));
        }
        let notices = self.notices(&key);
        if !notices.is_empty() {
            children.push(native::padded(
                native::spaced(native::column(format!("{key}/notices"), notices), 8.),
                wire::Edges {
                    top: 8.,
                    right: 10.,
                    bottom: 0.,
                    left: 10.,
                },
            ));
        }
        children.push(match self.view_mode {
            ViewMode::List => self.list_pane(format!("{key}/list")),
            ViewMode::Columns => self.columns_pane(format!("{key}/columns")),
        });
        children.push(native::divider(format!("{key}/status-rule")));
        children.push(self.status_bar(format!("{key}/status")));
        kit::filled_column(key, children)
    }

    fn toolbar(&self, key: String) -> wire::Node {
        let busy = self.loading();
        let can_write = !busy && self.refusal().is_empty();
        let mut children = vec![
            kit::quiet(
                format!("{key}/sidebar-toggle"),
                "☰",
                "Toggle sidebar",
                (self.viewport_width >= app_update::SIDEBAR_MIN).then_some(Message::ToggleSidebar),
                self.sidebar_open,
            ),
            kit::quiet(
                format!("{key}/back"),
                "‹",
                "Back",
                self.nav.can_back().then_some(Message::Back),
                false,
            ),
            kit::quiet(
                format!("{key}/forward"),
                "›",
                "Forward",
                self.nav.can_forward().then_some(Message::Forward),
                false,
            ),
            kit::quiet(
                format!("{key}/up"),
                "↑",
                "Up",
                (!self.nav.at_root()).then_some(Message::Parent),
                false,
            ),
            kit::quiet(
                format!("{key}/refresh"),
                "↻",
                "Refresh",
                Some(Message::Refresh),
                false,
            ),
            native::gap(4.),
            kit::quiet(
                format!("{key}/list-mode"),
                "☰ List",
                "List view",
                Some(Message::SetViewMode(ViewMode::List)),
                self.view_mode == ViewMode::List,
            ),
            kit::quiet(
                format!("{key}/columns-mode"),
                "▥ Columns",
                "Column view",
                Some(Message::SetViewMode(ViewMode::Columns)),
                self.view_mode == ViewMode::Columns,
            ),
            native::gap(4.),
            kit::action(
                format!("{key}/new-folder"),
                "New folder",
                Message::Prompt(NamePrompt::NewFolder),
                !can_write,
            ),
            kit::action(
                format!("{key}/new-file"),
                "New file",
                Message::Prompt(NamePrompt::NewFile),
                !can_write,
            ),
        ];
        if busy {
            let wait = match self.writing {
                Writing::Busy(Act::Save) => "Saving…",
                Writing::Busy(_) => "Writing…",
                Writing::Idle => "Loading…",
            };
            children.push(native::secondary(format!("{key}/wait"), wait));
        }
        let mut filter = native::input(
            format!("{key}/filter"),
            "Filter by name",
            &self.filter,
            slots::handler::<String, Message>(Box::new(|value| {
                Some(Message::FilterChanged(value))
            })),
            None,
        );
        if let wire::Node::Input { options, width, .. } = &mut filter {
            options.label = "Filter the listing by name".into();
            *width = Some(wire::Length::Fixed(180.));
        }
        children.push(filter);
        children.push(kit::quiet(
            format!("{key}/inspector-toggle"),
            "Info",
            "Toggle inspector",
            (self.viewport_width >= app_update::INSPECTOR_MIN).then_some(Message::ToggleInspector),
            self.inspector_open,
        ));
        native::padded(
            native::spaced(native::wrapped_row(key, children), 6.),
            wire::Edges::all(8.),
        )
    }

    /// One button per directory on the way here; the last is where the
    /// reader stands and does not press.
    fn path_bar(&self, key: String) -> wire::Node {
        let crumbs = crate::host::crumbs(&self.nav.path);
        let last = crumbs.len() - 1;
        let mut children = Vec::new();
        for (index, crumb) in crumbs.into_iter().enumerate() {
            if index > 0 {
                children.push(native::caption(format!("{key}/sep/{index}"), "›"));
            }
            let here = index == last;
            let mut button = native::button(
                format!("{key}/crumb/{}", crumb.path),
                crumb.name.clone(),
                (!here).then(|| slots::message(Message::Navigate(crumb.path.clone()))),
                wire::ButtonPreset::Text,
            );
            if let wire::Node::Button { label, checked, .. } = &mut button {
                *label = Some(format!("Go to {}", crumb.path));
                *checked = Some(here);
            }
            children.push(button);
        }
        let mut bar = native::sized(
            native::padded(
                native::spaced(native::centered_row(key, children), 2.),
                wire::Edges {
                    top: 0.,
                    right: 10.,
                    bottom: 4.,
                    left: 10.,
                },
            ),
            Some(wire::Length::Fill),
            Some(wire::Length::Fixed(30.)),
        );
        if let wire::Node::Linear { clip, .. } = &mut bar {
            *clip = true;
        }
        bar
    }

    /// What the browser has to say before its rows: a draft whose network
    /// moved under it, the last answer from a write, and why nothing can be
    /// written here.
    fn notices(&self, key: &str) -> Vec<wire::Node> {
        let mut children = Vec::new();
        if self.draft_parked() {
            children.push(native::notice(
                format!("{key}/parked-draft"),
                native::spaced(
                    native::centered_row(
                        format!("{key}/parked-draft/row"),
                        [
                            native::sized(
                                native::spaced(
                                    native::column(
                                        format!("{key}/parked-draft/lines"),
                                        [
                                            native::strong(
                                                format!("{key}/draft-label"),
                                                "Unsaved changes to:",
                                            ),
                                            native::wrapping(native::mono(
                                                format!("{key}/draft-path"),
                                                self.draft_path.clone(),
                                            )),
                                            native::caption(
                                                format!("{key}/draft-hint"),
                                                "Return to this file to continue editing.",
                                            ),
                                        ],
                                    ),
                                    2.,
                                ),
                                Some(wire::Length::Fill),
                                None,
                            ),
                            native::button(
                                format!("{key}/discard-draft"),
                                "Discard unsaved changes",
                                Some(slots::message(Message::DiscardDraft(self.draft_id))),
                                wire::ButtonPreset::Secondary,
                            ),
                        ],
                    ),
                    12.,
                ),
                Tone::Warning,
            ));
        }
        // under a dialog the refusal is said in the dialog, where the reader is
        let notice_here = !self.notice.is_empty() && self.modal() == Modal::None;
        if notice_here {
            children.push(native::notice(
                format!("{key}/notice-box"),
                native::wrapping(native::text(format!("{key}/notice"), self.notice.clone())),
                Tone::Danger,
            ));
        }
        let refusal = self.refusal();
        if !refusal.is_empty() {
            children.push(native::notice(
                format!("{key}/refusal-box"),
                native::wrapping(native::secondary(
                    format!("{key}/refusal"),
                    format!("Nothing can be written here: {refusal}."),
                )),
                Tone::Neutral,
            ));
        }
        children
    }

    /// `12 items, 3 folders` and the chosen row, as Finder's bar reads; a
    /// filter says how many of the rows it kept.
    fn status_bar(&self, key: String) -> wire::Node {
        let all = self.listing.entries();
        let rows = self.rows();
        let filtered = !self.filter.trim().is_empty();
        let tally = match (self.listing.is_pending(), filtered) {
            (true, _) => String::new(),
            (false, true) => format!("{} of {}", rows.len(), browse::tally(all)),
            (false, false) => browse::tally(all),
        };
        let mut children = vec![native::nowrap(native::caption(
            format!("{key}/tally"),
            tally,
        ))];
        if self.listing.has_more() {
            children.push(native::caption(format!("{key}/more"), "· more not shown"));
        }
        if !self.selected.is_empty() {
            children.push(native::wrapping(native::caption(
                format!("{key}/selected"),
                self.selected.clone(),
            )));
        }
        children.push(native::nowrap(native::caption(
            format!("{key}/drop-hint"),
            "Drop files here to upload",
        )));
        let mut bar = native::sized(
            native::padded(
                native::spaced(native::wrapped_row(key, children), 8.),
                wire::Edges {
                    top: 4.,
                    right: 10.,
                    bottom: 4.,
                    left: 10.,
                },
            ),
            Some(wire::Length::Fill),
            None,
        );
        if let wire::Node::Linear {
            clip, background, ..
        } = &mut bar
        {
            *clip = true;
            *background = Some(native::rgba(native::palette().surface));
        }
        bar
    }
}
