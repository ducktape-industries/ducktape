use ducktape_view_guest::{kit as native, wire};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) enum SearchPhase {
    Idle,
    Searching,
    Done,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) enum MessageAction {
    Toolbar,
    More,
    Reactions,
    Editing,
    Delete,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum CopySurface {
    Nowhere,
    Timeline,
    Thread,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RowPlate {
    Plain,
    Selected,
    Ranged,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RoomMove {
    Stayed,
    Moved,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SearchOutcome {
    Answered,
    Refused,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum LandingThread {
    Absent,
    Seated,
}
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ChatView {
    pub(crate) endpoint: String,
    pub(crate) network_name: String,
    pub(crate) network_chain_id: String,
    pub(crate) status: String,
    pub(crate) block_height: i64,
    pub(crate) connected: bool,
    pub(crate) session_loading: bool,
    pub(crate) session_busy: bool,
    pub(crate) rooms: Vec<crate::host::ChatSidebarRow>,
    pub(crate) dm_rows: Vec<crate::host::DmSidebarRow>,
    pub(crate) channel_create_open: bool,
    pub(crate) active_channel: String,
    pub(crate) active_dm_peer: String,
    pub(crate) active_dm: crate::host::DmPeer,
    pub(crate) huddle_joined: bool,
    pub(crate) huddle_channel: String,
    pub(crate) huddle_channel_name: String,
    pub(crate) huddle_joined_at: i64,
    pub(crate) huddle_now: i64,
    pub(crate) call_muted: bool,
    pub(crate) call_speaking: bool,
    pub(crate) speaking_peers: Vec<String>,
    pub(crate) unread_boundary: i64,
    pub(crate) live_agents: Vec<crate::host::LiveRunHint>,
    pub(crate) shift_held: bool,
    pub(crate) copy_chord_serial: i64,
    pub(crate) sent_serial: i64,
    pub(crate) pending_sends: Vec<crate::host::PendingSend>,
    pub(crate) me: String,
    pub(crate) me_key: String,
    pub(crate) names_serial: i64,
    pub(crate) land_seq: i64,
    pub(crate) connection_serial: i64,
    pub(crate) room_serial: i64,
    pub(crate) history_pages: i64,
    pub(crate) room_key: crate::host::RoomKey,
    pub(crate) room_channel: String,
    pub(crate) room_messages: Vec<crate::host::ChatMessage>,
    pub(crate) messages: Vec<crate::host::ChatMessage>,
    pub(crate) channel_members: Vec<crate::host::ChatMember>,
    pub(crate) active_channel_name: String,
    pub(crate) active_channel_archived: bool,
    pub(crate) active_channel_members_only: bool,
    pub(crate) post_refusal: String,
    pub(crate) has_older_history: bool,
    /// the window on screen runs up to the room's head (see `RoomItem`)
    pub(crate) window_reaches_head: bool,
    pub(crate) loading: bool,
    pub(crate) busy: bool,
    pub(crate) thread_pages: i64,
    pub(crate) thread_key: crate::host::ThreadKey,
    pub(crate) active_thread_seq: i64,
    pub(crate) thread_target_seq: i64,
    pub(crate) thread_reveal_key: i64,
    pub(crate) stream_reveal_key: i64,
    pub(crate) thread_messages: Vec<crate::host::ChatMessage>,
    pub(crate) thread_has_more: bool,
    pub(crate) thread_next_reply_seq: i64,
    pub(crate) thread_loading: bool,
    pub(crate) search_key: crate::host::SearchKey,
    pub(crate) search_phase: SearchPhase,
    pub(crate) search_query: String,
    pub(crate) search_hits: Vec<crate::host::ChatSearchHit>,
    pub(crate) history_view: bool,
    pub(crate) at_live_tail: bool,
    pub(crate) history_loading: bool,
    pub(crate) unread_marker_seq: i64,
    pub(crate) selected_message_seq: i64,
    pub(crate) selected_message_rev: i64,
    pub(crate) message_action: MessageAction,
    pub(crate) channel_settings_open: bool,
    pub(crate) thread_selected_seq: i64,
    pub(crate) thread_selected_rev: i64,
    pub(crate) thread_message_action: MessageAction,
    pub(crate) copy_anchor_seq: i64,
    pub(crate) copy_head_seq: i64,
    pub(crate) copy_surface: CopySurface,
    pub(crate) chat_viewport_width: f64,
    pub(crate) chat_viewport_height: f64,
    /// The pictures the host has decoded for this view, by their duck link:
    /// the drawn size, or (0, 0) for one that did not decode and stays a
    /// file card. Not a snapshot's to keep: the host's store is not.
    #[serde(skip)]
    pub(crate) pictures: ::std::collections::BTreeMap<String, (i64, i64)>,
    /// The pictures asked for and not yet answered.
    #[serde(skip)]
    pub(crate) pictures_pending: ::std::collections::BTreeSet<String>,
    /// The attachment open in the preview card over the screen, by its duck
    /// link; "" is no card. The serial moves on every open so the read
    /// subscription re-runs even for the same file.
    pub(crate) preview_link: String,
    pub(crate) preview_serial: i64,
    /// What that read answered. Not a snapshot's to keep: the card re-reads.
    #[serde(skip)]
    pub(crate) preview: crate::host::PreviewItem,
    /// Where the pointer last pressed, in chat-screen pixels: a message menu
    /// opens there.
    pub(crate) press_x: f64,
    pub(crate) press_y: f64,
    /// Where the open menu was anchored when it opened.
    pub(crate) menu_x: f64,
    pub(crate) menu_y: f64,
    pub(crate) sidebar_width: f64,
    pub(crate) details_width: f64,
    pub(crate) thread_width: f64,
    pub(crate) search_draft: String,
    pub(crate) message_edit_draft: String,
    pub(crate) channel_name_draft: String,
    pub(crate) member_key_draft: String,
    pub(crate) thread_edit_draft: String,
    pub(crate) host_error: String,
    pub(crate) sent: bool,
    pub(crate) dark: bool,
}
impl ::std::fmt::Debug for ChatView {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("ChatView")
    }
}
#[derive(Clone)]
pub enum Message {
    SidebarResized(f64, f64),
    DetailsResized(f64, f64),
    ThreadResized(f64, f64),
    ChatViewportChanged(f64, f64),
    PressedAt(f64, f64),
    /// The host decoded (or refused) the picture behind a duck link.
    PictureLoaded(String, Result<(i64, i64), String>),
    /// A press on a message's file: the preview card opens over the screen.
    OpenAttachment(String),
    ClosePreview,
    PreviewArrived(crate::host::PreviewItem),
    /// Boxed: the session item dwarfs every other variant.
    SessionArrived(Box<crate::host::SessionItem>),
    SessionSettled(bool),
    SnapStream(bool),
    RevealStream(i64),
    RevealThread(i64),
    RoomArrived(crate::host::RoomItem),
    ThreadArrived(crate::host::ThreadItem),
    SearchArrived(crate::host::SearchItem),
    ActDone(crate::host::ActItem),
    SearchChatSubmit,
    ClearChatSearch,
    OpenChatSearchHit(String, i64, i64),
    ToggleChannelCreate,
    ChooseChannel(String),
    ChooseDm(String),
    ToggleChannelSettings,
    ShowHuddle,
    LeaveHuddleHere,
    JoinHuddleSubmit,
    JoinVoice(String),
    OpenMessageLink(String),
    CopyToClipboard(String, String),
    CopyMessageLink(String),
    CancelRun(String),
    OpenRun(String),
    ChatScrolled(f64, f64, f64, f64),
    LoadMoreHistory,
    OpenMessageActions(i64, String, i64),
    OpenMessageReactions(i64, String, i64),
    BeginMessageEdit(i64, String, i64),
    ArmMessageDelete(i64, String, i64),
    ClearMessageSelection,
    OpenThreadMessageActions(i64, String, i64),
    OpenThreadMessageReactions(i64, String, i64),
    BeginThreadMessageEdit(i64, String, i64),
    ArmThreadMessageDelete(i64, String, i64),
    ClearThreadMessageSelection,
    OpenThreadFor(i64),
    CloseThread,
    LoadMoreThread,
    AddReactionSubmit(String),
    AddReactionAt(i64, String),
    RemoveReactionAt(i64, String),
    DeleteMessageSubmit,
    DeleteThreadMessageSubmit,
    RenameChannelSubmit,
    ArchiveChannelSubmit,
    UnarchiveChannelSubmit,
    AddChannelMemberSubmit,
    RemoveChannelMemberSubmit(String),
    PressMessage(i64, CopySurface),
    ClearCopyRange,
    CopySelectedMessages,
    CopyChord(bool),
    SearchDraftChanged(String),
    ChannelNameDraftChanged(String),
    MemberKeyDraftChanged(String),
}
impl ::std::fmt::Debug for Message {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("Message")
    }
}
impl ChatView {
    fn state() -> Self {
        Self {
            endpoint: "".to_owned(),
            network_name: "".to_owned(),
            network_chain_id: "".to_owned(),
            status: "".to_owned(),
            block_height: 0,
            connected: false,
            session_loading: false,
            session_busy: false,
            rooms: Vec::new(),
            dm_rows: Vec::new(),
            channel_create_open: false,
            active_channel: "".to_owned(),
            active_dm_peer: "".to_owned(),
            active_dm: crate::host::no_dm_peer(),
            huddle_joined: false,
            huddle_channel: "".to_owned(),
            huddle_channel_name: "".to_owned(),
            huddle_joined_at: 0,
            huddle_now: 0,
            call_muted: false,
            call_speaking: false,
            speaking_peers: Vec::new(),
            unread_boundary: 0,
            live_agents: Vec::new(),
            shift_held: false,
            copy_chord_serial: 0,
            sent_serial: 0,
            pending_sends: Vec::new(),
            me: "".to_owned(),
            me_key: "".to_owned(),
            names_serial: 0,
            land_seq: 0,
            connection_serial: 0,
            room_serial: 0,
            history_pages: 0,
            room_key: crate::host::room_key(0, 0, ::std::convert::AsRef::as_ref(&("")), 0, 0),
            room_channel: "".to_owned(),
            room_messages: Vec::new(),
            messages: Vec::new(),
            channel_members: Vec::new(),
            active_channel_name: "".to_owned(),
            active_channel_archived: false,
            active_channel_members_only: false,
            post_refusal: "".to_owned(),
            has_older_history: false,
            window_reaches_head: true,
            loading: false,
            busy: false,
            thread_pages: 0,
            thread_key: crate::host::thread_key(
                0,
                0,
                ::std::convert::AsRef::as_ref(&("")),
                0,
                0,
                0,
            ),
            active_thread_seq: 0,
            thread_target_seq: 0,
            thread_reveal_key: 0,
            stream_reveal_key: 0,
            thread_messages: Vec::new(),
            thread_has_more: false,
            thread_next_reply_seq: 0,
            thread_loading: false,
            search_key: crate::host::search_key(0, 0, ::std::convert::AsRef::as_ref(&(""))),
            search_phase: SearchPhase::Idle,
            search_query: "".to_owned(),
            search_hits: Vec::new(),
            history_view: false,
            at_live_tail: true,
            history_loading: false,
            unread_marker_seq: 0,
            selected_message_seq: 0,
            selected_message_rev: 0,
            message_action: MessageAction::Toolbar,
            channel_settings_open: false,
            thread_selected_seq: 0,
            thread_selected_rev: 0,
            thread_message_action: MessageAction::Toolbar,
            copy_anchor_seq: 0,
            copy_head_seq: 0,
            copy_surface: CopySurface::Nowhere,
            chat_viewport_width: 1280.0,
            chat_viewport_height: 800.0,
            pictures: ::std::collections::BTreeMap::new(),
            pictures_pending: ::std::collections::BTreeSet::new(),
            preview_link: "".to_owned(),
            preview_serial: 0,
            preview: crate::host::PreviewItem::default(),
            press_x: 0.0,
            press_y: 0.0,
            menu_x: 0.0,
            menu_y: 0.0,
            sidebar_width: 236.0,
            details_width: 320.0,
            thread_width: 330.0,
            search_draft: "".to_owned(),
            message_edit_draft: "".to_owned(),
            channel_name_draft: "".to_owned(),
            member_key_draft: "".to_owned(),
            thread_edit_draft: "".to_owned(),
            host_error: "".to_owned(),
            sent: false,
            dark: false,
        }
    }
    pub(crate) fn boot() -> (Self, ::ducktape_view_guest::Task<Message>) {
        (Self::state(), ::ducktape_view_guest::Task::none())
    }
    pub(crate) const PREFERRED_WINDOW_SIZE: &'static str = "none";
    pub(crate) const SNAPSHOT_SCHEMA: &'static str =
        "9ea6f38a08808cd55a30e2275cefb28fa11cabf11be9f5ea5b2d04664c7d1bc6";
    pub(crate) fn snapshot(&self) -> Result<Vec<u8>, String> {
        self.validate_snapshot()?;
        wire::Snapshot {
            schema: Self::SNAPSHOT_SCHEMA.into(),
            state: wire::SnapshotValue::Bytes(wire::encode(self)),
        }
        .encode()
    }

    pub(crate) fn restore(bytes: &[u8]) -> Result<Self, String> {
        let snapshot = wire::Snapshot::decode(bytes)?;
        if snapshot.schema != Self::SNAPSHOT_SCHEMA {
            return Err("invalid Chat snapshot schema".into());
        }
        let wire::SnapshotValue::Bytes(state) = snapshot.state else {
            return Err("invalid Chat snapshot".into());
        };
        let state: Self = wire::decode(&state)?;
        state.validate_snapshot()?;
        Ok(state)
    }

    fn validate_snapshot(&self) -> Result<(), String> {
        let widths = [
            self.chat_viewport_width,
            self.chat_viewport_height,
            self.press_x,
            self.press_y,
            self.menu_x,
            self.menu_y,
            self.sidebar_width,
            self.details_width,
            self.thread_width,
        ];
        if widths.into_iter().all(f64::is_finite) {
            Ok(())
        } else {
            Err("invalid Chat snapshot geometry".into())
        }
    }
}
impl ChatView {
    pub(crate) fn subscription(&self) -> ::ducktape_view_guest::Subscription<Message> {
        ::ducktape_view_guest::Subscription::batch([
            crate::host::session().map(|item| Message::SessionArrived(Box::new(item))),
            if self.connected {
                ::ducktape_view_guest::Subscription::batch([crate::host::room(
                    self.room_key.clone(),
                )
                .map(Message::RoomArrived)])
            } else {
                ::ducktape_view_guest::Subscription::none()
            },
            if self.connected && (self.active_thread_seq > 0) {
                ::ducktape_view_guest::Subscription::batch([crate::host::thread(
                    self.thread_key.clone(),
                )
                .map(Message::ThreadArrived)])
            } else {
                ::ducktape_view_guest::Subscription::none()
            },
            if self.connected && (!(self.search_query).is_empty()) {
                ::ducktape_view_guest::Subscription::batch([crate::host::search(
                    self.search_key.clone(),
                )
                .map(Message::SearchArrived)])
            } else {
                ::ducktape_view_guest::Subscription::none()
            },
            if self.preview_reads() {
                ::ducktape_view_guest::Subscription::batch([crate::host::preview(
                    self.preview_serial,
                    crate::host::attachment_file_path(&self.preview_link),
                )
                .map(Message::PreviewArrived)])
            } else {
                ::ducktape_view_guest::Subscription::none()
            },
            crate::host::acts().map(Message::ActDone),
        ])
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_composition_retains_timeline_identity_unread_and_action_admission() {
        let mut state = ChatView::state();
        state.connected = true;
        state.active_channel = "room".into();
        state.active_channel_name = "Room".into();
        state.unread_boundary = 1;
        state.unread_marker_seq = 2;
        state.rooms = vec![crate::host::ChatSidebarRow {
            channel: crate::host::ChatChannel {
                id: "room".into(),
                name: "Room".into(),
                ..Default::default()
            },
            unread: true,
        }];
        state.messages = vec![
            crate::host::ChatMessage {
                seq: 1,
                view_key: 11,
                ..Default::default()
            },
            crate::host::ChatMessage {
                seq: 2,
                view_key: 22,
                ..Default::default()
            },
            crate::host::ChatMessage {
                seq: -1,
                view_key: -1,
                pending: true,
                ..Default::default()
            },
            crate::host::ChatMessage {
                seq: 3,
                view_key: 33,
                deleted: true,
                ..Default::default()
            },
        ];
        let mut tree = state.view();
        let mut scrolls = 0;
        let mut rows = 0;
        let mut unread = 0;
        let mut threads = 0;
        let mut reactions = 0;
        let mut menus = 0;
        let mut unread_rooms = 0;
        let mut action_rows = 0;
        tree.for_each_mut(&mut |node| match node {
            wire::Node::Linear {
                key,
                axis,
                wrap,
                children,
                ..
            } if key.ends_with("/actions/bar") => {
                assert_eq!(*axis, wire::Axis::Row);
                assert!(wrap.is_none(), "the floating bar keeps one line");
                assert_eq!(children.len(), 4);
                assert!(
                    children
                        .iter()
                        .all(|child| matches!(child, wire::Node::Button { .. }))
                );
                action_rows += 1;
            }
            wire::Node::Scroll {
                key,
                virtual_rows,
                anchor_y,
                ..
            } if key.ends_with("/message-stream") => {
                assert!(*virtual_rows);
                assert_eq!(*anchor_y, wire::ScrollAnchor::End);
                scrolls += 1;
            }
            wire::Node::KeyedColumn {
                key,
                keys,
                virtual_row,
                ..
            } if key.ends_with("/message-stream/rows") => {
                assert_eq!(
                    keys.as_ref().unwrap(),
                    &[11, 22, -1, 33].map(wire::ListKey::Integer)
                );
                assert!(virtual_row.is_some_and(|height| height > 0.));
                rows += 1;
            }
            wire::Node::Text { content, .. } if content == "New messages" => unread += 1,
            wire::Node::Container { key, .. }
                if key.ends_with("/unread") && key.contains("/channel/") =>
            {
                unread_rooms += 1;
            }
            wire::Node::Button {
                label: Some(label),
                on_press,
                key,
                ..
            } if ["Open thread", "React with 👍", "More message actions"]
                .contains(&label.as_str()) =>
            {
                assert!(on_press.is_some());
                assert!(!key.contains("/message/-1/") && !key.contains("/message/33/"));
                match label.as_str() {
                    "Open thread" => threads += 1,
                    "React with 👍" => reactions += 1,
                    _ => menus += 1,
                }
            }
            _ => {}
        });
        assert_eq!(
            (scrolls, rows, unread, threads, reactions, menus),
            (1, 1, 1, 2, 2, 2)
        );
        assert_eq!(unread_rooms, 1);
        assert_eq!(action_rows, 2);
    }

    #[test]
    fn dm_header_fills_the_title_slot_and_menus_focus_real_content() {
        let mut state = ChatView::state();
        state.connected = true;
        state.active_channel = "room".into();
        state.active_dm_peer = "peer".into();
        state.active_dm = crate::host::DmPeer {
            name: "Peer".into(),
            ..Default::default()
        };
        state.selected_message_seq = 1;
        state.message_action = MessageAction::More;
        let mut tree = state.view();
        let mut headers = 0;
        let mut menus = 0;
        tree.for_each_mut(&mut |node| match node {
            wire::Node::Linear { key, width, .. } if key.ends_with("/dm-header") => {
                assert_eq!(*width, Some(wire::Length::Fill));
                headers += 1;
            }
            wire::Node::Container { key, .. } if key.ends_with("/message-action-focus") => {
                menus += 1;
            }
            wire::Node::Input { key, .. } => assert!(!key.ends_with("-focus")),
            _ => {}
        });
        assert_eq!((headers, menus), (1, 1));
        state.active_dm.name.clear();
        let mut tree = state.view();
        tree.for_each_mut(&mut |node| {
            assert!(!node.key().is_some_and(|key| key.ends_with("/dm-header")))
        });
    }

    /// The stream opens on the room's beginning — and only when the beginning
    /// is what it is showing. The intro rides inside the scroll, which keeps
    /// the end anchor either way.
    #[test]
    fn the_stream_opens_with_its_intro_only_once_the_whole_history_shows() {
        let mut state = ChatView::state();
        state.connected = true;
        state.active_channel = "room".into();
        state.active_channel_name = "design".into();
        state.messages = vec![crate::host::ChatMessage {
            seq: 1,
            view_key: 11,
            ..Default::default()
        }];
        let intros = |state: &ChatView| {
            let mut tree = state.view();
            let (mut intros, mut anchored) = (0, 0);
            tree.for_each_mut(&mut |node| match node {
                wire::Node::Text { content, .. }
                    if content.starts_with("This is the very beginning of #design.") =>
                {
                    intros += 1;
                }
                wire::Node::Scroll { key, anchor_y, .. } if key.ends_with("/message-stream") => {
                    assert_eq!(*anchor_y, wire::ScrollAnchor::End);
                    anchored += 1;
                }
                _ => {}
            });
            assert_eq!(anchored, 1, "the stream keeps one end-anchored scroll");
            intros
        };
        assert_eq!(intros(&state), 1);
        state.has_older_history = true;
        assert_eq!(intros(&state), 0, "older history is not a beginning");
    }

    #[test]
    fn reaction_menu_keeps_every_choice_in_four_native_grid_rows() {
        let mut state = ChatView::state();
        state.connected = true;
        state.active_channel = "room".into();
        state.selected_message_seq = 1;
        state.message_action = MessageAction::Reactions;
        let mut tree = state.view();
        let mut grids = 0;
        tree.for_each_mut(&mut |node| {
            if let wire::Node::Grid {
                key,
                columns,
                children,
                ..
            } = node
                && key.ends_with("/message-reaction-grid")
            {
                assert_eq!(*columns, Some(8));
                assert_eq!(children.len(), 32);
                for (node, emoji) in children.iter().zip(crate::host::reaction_palette()) {
                    let wire::Node::Button {
                        on_press,
                        description,
                        content,
                        ..
                    } = node
                    else {
                        panic!("reaction choice must be a native button");
                    };
                    assert!(on_press.is_some());
                    assert_eq!(description.as_ref(), Some(&emoji));
                    // the glyph is a text node whose line box is the cell,
                    // so the host's button clips nothing off the emoji
                    let wire::ButtonContent::Child(glyph) = content else {
                        panic!("the cell's glyph is a text node");
                    };
                    let wire::Node::Text {
                        content, options, ..
                    } = glyph.as_ref()
                    else {
                        panic!("the cell's glyph is a text node");
                    };
                    assert_eq!(content, &emoji);
                    assert_eq!(
                        options.line_height,
                        Some(wire::LineHeight::Absolute(super::chat::PICKER_CELL))
                    );
                }
                grids += 1;
            }
        });
        assert_eq!(grids, 1);
    }

    #[test]
    fn every_thread_menu_mount_matches_its_focus_target() {
        let mut state = ChatView::state();
        state.connected = true;
        state.active_channel = "room".into();
        state.active_thread_seq = 1;
        for (message, suffix) in [
            (
                Message::OpenThreadMessageActions(1, "body".into(), 0),
                "thread-action-focus",
            ),
            (
                Message::OpenThreadMessageReactions(1, "body".into(), 0),
                "thread-reaction-focus",
            ),
            (
                Message::ArmThreadMessageDelete(1, "body".into(), 0),
                "thread-delete-focus",
            ),
        ] {
            let _ = state.update(message);
            let target = format!("ChatView/chat/thread-pane/{suffix}");
            let mut tree = state.view();
            let mut matches = 0;
            tree.for_each_mut(&mut |node| {
                if let wire::Node::Container { key, .. } = node
                    && key == &target
                {
                    matches += 1;
                }
            });
            assert_eq!(matches, 1, "one real menu owns {target}");
        }
    }

    /// The "…" menu is a dropdown floated where the pointer pressed: one
    /// item a row, full-width, no close row (the backdrop closes it), and
    /// nothing of it in the stream's flow. The timeline's menu adds Reply;
    /// the thread's has no thread to open. One menu floats at a time.
    #[test]
    fn the_message_menu_floats_at_the_press_as_a_dropdown() {
        let mut state = ChatView::state();
        state.connected = true;
        state.active_channel = "room".into();
        state.active_thread_seq = 1;
        state.press_x = 900.0;
        state.press_y = 300.0;
        let menu_of = |state: &ChatView, focus: &str| -> Vec<String> {
            let mut tree = state.view();
            let mut floats = 0;
            let mut items = Vec::new();
            tree.for_each_mut(&mut |node| match node {
                wire::Node::Float { key, content, .. } => {
                    assert!(key.ends_with("/floating-menu"), "{key}");
                    let mut frames = 0;
                    content.for_each_mut(&mut |inner| {
                        if let wire::Node::Container { key, .. } = inner
                            && key.ends_with(focus)
                        {
                            frames += 1;
                        }
                    });
                    assert_eq!(frames, 1, "the float carries the {focus} frame");
                    floats += 1;
                }
                wire::Node::Linear { key, children, .. } if key.ends_with("menu-actions") => {
                    for child in children {
                        let wire::Node::Button {
                            content: wire::ButtonContent::Child(_),
                            label: Some(accessible),
                            width: Some(wire::Length::Fill),
                            ..
                        } = child
                        else {
                            panic!("a full-width labelled row, got {child:?}");
                        };
                        items.push(accessible.clone());
                    }
                }
                wire::Node::Linear { key, .. } => assert!(!key.ends_with("close-row")),
                _ => {}
            });
            assert_eq!(floats, 1, "one menu floats");
            items
        };
        let _ = state.update(Message::OpenMessageActions(1, "body".into(), 0));
        assert_eq!(
            menu_of(&state, "message-action-focus"),
            [
                "Reply in thread",
                "Add reaction",
                "Copy link",
                "Edit message",
                "Delete message"
            ]
        );
        // a press on one of its items does not move the open menu
        let _ = state.update(Message::PressedAt(10.0, 10.0));
        let mut tree = state.view();
        tree.for_each_mut(&mut |node| {
            if let wire::Node::Float { x, .. } = node {
                assert_eq!(x.ops, vec![wire::FloatOp::Number(900.0)]);
            }
        });
        let _ = state.update(Message::ClearMessageSelection);
        let _ = state.update(Message::OpenThreadMessageActions(1, "body".into(), 0));
        assert_eq!(
            menu_of(&state, "thread-action-focus"),
            [
                "Add reaction",
                "Copy link",
                "Edit message",
                "Delete message"
            ]
        );
        // a press at the right edge (the "…" of a row) opens leftward
        let (x, y) = crate::host::menu_origin(
            (1200.0, 300.0),
            (220.0, 100.0),
            (state.chat_viewport_width, state.chat_viewport_height),
        );
        assert_eq!((x, y), (980.0, 304.0));
    }

    /// A pressed attachment previews in a modal card over the screen — a
    /// picture from the host's slot, a file from a read the card keys —
    /// and Files is a button inside it, never where the press lands.
    #[test]
    fn attachment_press_previews_in_place_and_files_is_one_press_away() {
        let doc = "duck://files/shared/attachments/u1/notes.txt".to_owned();
        let shot = "duck://files/shared/attachments/u1/shot.png".to_owned();
        let mut state = ChatView::state();
        state.connected = true;
        state.active_channel = "room".into();
        state.messages = vec![crate::host::ChatMessage {
            seq: 1,
            view_key: 11,
            blocks: vec![
                crate::host::ChatBlock {
                    kind: "attachment".into(),
                    text: "notes.txt".into(),
                    link: doc.clone(),
                    ..Default::default()
                },
                crate::host::ChatBlock {
                    kind: "attachment".into(),
                    text: "shot.png".into(),
                    link: shot.clone(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }];
        state.pictures.insert(shot.clone(), (800, 600));
        let overlays = |state: &ChatView| {
            let mut found = Vec::new();
            state.view().for_each_mut(&mut |node| {
                if let wire::Node::Overlay {
                    key, on_dismiss, ..
                } = node
                {
                    assert!(on_dismiss.is_some(), "a press outside the card closes it");
                    found.push(key.clone());
                }
            });
            found
        };
        let buttons = |state: &ChatView| {
            let mut labels = Vec::new();
            state.view().for_each_mut(&mut |node| {
                if let wire::Node::Button {
                    label: Some(label),
                    on_press: Some(_),
                    ..
                } = node
                {
                    labels.push(label.clone());
                }
            });
            labels
        };
        assert!(overlays(&state).is_empty());
        assert!(buttons(&state).contains(&"Open notes.txt".to_owned()));

        let _ = state.update(Message::OpenAttachment(doc.clone()));
        assert!(state.preview_reads(), "a text file is read for its card");
        assert_eq!(overlays(&state).len(), 1);
        let labels = buttons(&state);
        assert!(labels.contains(&"Open in Files".to_owned()));
        assert!(labels.contains(&"Close preview".to_owned()));

        let _ = state.update(Message::PreviewArrived(crate::host::PreviewItem {
            path: "/shared/attachments/u1/notes.txt".into(),
            read: true,
            binary: true,
            text: crate::host::BINARY_PLATE.into(),
            ..Default::default()
        }));
        let mut plates = 0;
        state.view().for_each_mut(&mut |node| {
            if let wire::Node::Linear { key, .. } = node
                && key.ends_with("/preview/binary")
            {
                plates += 1;
            }
        });
        assert_eq!(plates, 1, "a binary file shows the no-preview plate");

        // a text file paints through the host's code surface, a Markdown
        // file through its Markdown surface with a link handler
        let surfaces = |state: &ChatView| {
            let mut found = Vec::new();
            state.view().for_each_mut(&mut |node| {
                if let wire::Node::Surface {
                    key,
                    name,
                    on_event,
                    ..
                } = node
                    && key.contains("/preview/")
                {
                    found.push((name.clone(), on_event.is_some()));
                }
            });
            found
        };
        let _ = state.update(Message::PreviewArrived(crate::host::PreviewItem {
            path: "/shared/attachments/u1/notes.txt".into(),
            read: true,
            text: "fn main() {}".into(),
            ..Default::default()
        }));
        assert_eq!(surfaces(&state), vec![("forge_code".to_owned(), false)]);
        let readme = "duck://files/shared/attachments/u1/README.md".to_owned();
        let _ = state.update(Message::OpenAttachment(readme));
        let _ = state.update(Message::PreviewArrived(crate::host::PreviewItem {
            path: "/shared/attachments/u1/README.md".into(),
            read: true,
            text: "# hi".into(),
            ..Default::default()
        }));
        assert_eq!(surfaces(&state), vec![("agent_markdown".to_owned(), true)]);

        let _ = state.update(Message::OpenAttachment(shot.clone()));
        assert!(
            !state.preview_reads(),
            "a decoded picture draws from the slot"
        );
        let mut pictures = 0;
        state.view().for_each_mut(&mut |node| {
            if let wire::Node::Surface { key, name, .. } = node
                && name == "picture"
                && key.ends_with("/preview/picture")
            {
                pictures += 1;
            }
        });
        assert_eq!(pictures, 1);

        let _ = state.update(Message::OpenMessageLink(shot));
        assert!(
            overlays(&state).is_empty(),
            "leaving for Files closes the card"
        );
        let _ = state.update(Message::OpenAttachment(doc));
        let _ = state.update(Message::ClosePreview);
        assert!(overlays(&state).is_empty());
    }

    #[test]
    fn snapshot_preserves_drafts_selection_and_subscription_identity() {
        let mut state = ChatView::state();
        state.search_draft = "unsent search".into();
        state.message_edit_draft = "unfinished message".into();
        state.thread_edit_draft = "unfinished reply".into();
        state.channel_name_draft = "new channel".into();
        state.member_key_draft = "member key".into();
        state.copy_surface = CopySurface::Thread;
        state.copy_anchor_seq = 12;
        state.copy_head_seq = 18;
        state.message_action = MessageAction::Editing;
        state.thread_message_action = MessageAction::Reactions;
        state.search_phase = SearchPhase::Searching;
        state.room_key = crate::host::room_key(7, 8, "room", 12, 2);
        state.thread_key = crate::host::thread_key(7, 8, "room", 12, 1, 0);
        state.search_key = crate::host::search_key(7, 8, "query");
        state.sidebar_width = 278.5;
        let bytes = state.snapshot().unwrap();
        let restored = ChatView::restore(&bytes).unwrap();
        assert_eq!(wire::encode(&state), wire::encode(&restored));
        assert_eq!(bytes, restored.snapshot().unwrap());
    }

    #[test]
    fn snapshot_rejects_wrong_envelope_corrupt_state_and_nonfinite_geometry() {
        let mut envelope = wire::Snapshot::decode(&ChatView::state().snapshot().unwrap()).unwrap();
        envelope.schema = "0".repeat(64);
        assert!(ChatView::restore(&envelope.encode().unwrap()).is_err());
        envelope.schema = ChatView::SNAPSHOT_SCHEMA.into();
        envelope.state = wire::SnapshotValue::Bytes(vec![255]);
        assert!(ChatView::restore(&envelope.encode().unwrap()).is_err());
        let mut state = ChatView::state();
        state.thread_width = f64::NAN;
        assert!(state.snapshot().is_err());
        envelope.state = wire::SnapshotValue::Bytes(wire::encode(&state));
        assert!(ChatView::restore(&envelope.encode().unwrap()).is_err());
    }

    #[test]
    fn view_fits_default_stack() {
        ::std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(|| {
                let (app, _) = ChatView::boot();
                let _ = app.view();
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
mod app_update;
mod app_view;
mod chat;
mod components;
mod dm;
mod kit;
