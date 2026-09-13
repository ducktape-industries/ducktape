//! The Members roster as a module-owned view: who may act on this network.
//!
//! The kernel pushes session facts only (`members.props`: connected, admin,
//! dark). The view reads the roster itself through the kernel's
//! `rpc.status` / `rpc.peers` / `rpc.query`, re-reads it on every `rpc.live`
//! hit for the valset plane, and a row opens its record. Pausing an agent
//! and opening a membership ballot leave as `op.submit` — the module
//! message the kernel signs with the seated key; copying a key stays an
//! intent, because the clipboard is an OS door the kernel has not opened.
//! Public member keys are view data; signing secrets and passwords stay in the host.
pub mod host;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum MembersFilter {
    All,
    Humans,
    Agents,
    Validators,
}
#[derive(serde::Serialize, serde::Deserialize)]
pub struct MembersView {
    pub(crate) rows: Vec<crate::host::MemberRow>,
    pub(crate) admin: bool,
    pub(crate) connected: bool,
    pub(crate) connection_serial: i64,
    pub(crate) answered: bool,
    pub(crate) host_error: String,
    pub(crate) filter: MembersFilter,
    pub(crate) selected: String,
    pub(crate) height: i64,
    pub(crate) acting: String,
    pub(crate) viewport_width: f64,
    pub(crate) member_width: f64,
    pub(crate) dark: bool,
}
impl ::std::fmt::Debug for MembersView {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("MembersView")
    }
}
#[derive(Clone)]
pub enum Message {
    SessionArrived(crate::host::SessionItem),
    RosterArrived(crate::host::RosterItem),
    ActDone(crate::host::ActItem),
    PickFilter(MembersFilter),
    OpenMember(String),
    CopyKey(String, String),
    MemberResized(f64, f64),
    ViewportChanged(f64, f64),
    SetAgentStatus(String, bool),
    OpenBallot(String, String),
}
impl ::std::fmt::Debug for Message {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("Message")
    }
}
impl MembersView {
    fn state() -> Self {
        Self {
            rows: Vec::new(),
            admin: false,
            connected: false,
            connection_serial: 0,
            answered: false,
            host_error: "".to_owned(),
            filter: MembersFilter::All,
            selected: "".to_owned(),
            height: 0,
            acting: "".to_owned(),
            viewport_width: 1280.0,
            member_width: 312.0,
            dark: false,
        }
    }
    pub(crate) fn boot() -> (Self, ::ducktape_view_guest::Task<Message>) {
        (Self::state(), ::ducktape_view_guest::Task::none())
    }
    pub(crate) const PREFERRED_WINDOW_SIZE: &'static str = "none";
    pub(crate) const SNAPSHOT_SCHEMA: &'static str =
        "2e83a4474a83cd1cec8a9b8c5c55db557002ca8afe7872c545a6e15969be41c1";
}
impl MembersView {
    pub(crate) fn snapshot(&self) -> Result<Vec<u8>, String> {
        use ducktape_view_guest::wire;
        self.validate_snapshot()?;
        wire::Snapshot {
            schema: Self::SNAPSHOT_SCHEMA.into(),
            state: wire::SnapshotValue::Bytes(wire::encode(self)),
        }
        .encode()
    }
    pub(crate) fn restore(bytes: &[u8]) -> Result<Self, String> {
        use ducktape_view_guest::wire;
        let snapshot = wire::Snapshot::decode(bytes)?;
        if snapshot.schema != Self::SNAPSHOT_SCHEMA {
            return Err("invalid Members snapshot schema".into());
        }
        let wire::SnapshotValue::Bytes(state) = snapshot.state else {
            return Err("invalid Members snapshot".into());
        };
        let state: Self = wire::decode(&state)?;
        state.validate_snapshot()?;
        Ok(state)
    }
    fn validate_snapshot(&self) -> Result<(), String> {
        let finite = self.viewport_width.is_finite() && self.member_width.is_finite();
        if finite {
            Ok(())
        } else {
            Err("snapshot number must be finite".into())
        }
    }
    fn subscription(&self) -> ::ducktape_view_guest::Subscription<Message> {
        ::ducktape_view_guest::Subscription::batch([
            crate::host::session().map(Message::SessionArrived),
            if self.connected {
                ::ducktape_view_guest::Subscription::batch([crate::host::roster(
                    self.connection_serial,
                )
                .map(Message::RosterArrived)])
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
    fn disconnected_view_hides_retained_members_and_actions() {
        let (mut view, _) = MembersView::boot();
        view.selected = "stale-member".into();
        view.rows.push(host::MemberRow {
            key: "stale-member".into(),
            ..Default::default()
        });
        let mut tree = view.view();
        tree.for_each_mut(&mut |node| {
            assert!(!node.key().is_some_and(|key| key.contains("stale-member")));
            assert!(!matches!(
                node,
                ducktape_view_guest::wire::Node::Button {
                    on_press: Some(_),
                    ..
                }
            ));
        });
        assert_eq!(view.rows.len(), 1);
    }
    #[test]
    fn snapshot_preserves_member_selection_filter_and_width() {
        let (mut view, _) = MembersView::boot();
        view.filter = MembersFilter::Agents;
        view.selected = "reviewer".into();
        view.member_width = 360.;
        view.rows.push(host::MemberRow {
            key: "reviewer".into(),
            ..Default::default()
        });
        let bytes = view.snapshot().unwrap();
        let restored = MembersView::restore(&bytes).unwrap();
        assert_eq!(restored.filter, MembersFilter::Agents);
        assert_eq!(restored.selected, "reviewer");
        assert_eq!(restored.member_width, 360.);
        assert_eq!(restored.snapshot().unwrap(), bytes);
    }
    #[test]
    fn snapshot_rejects_nonfinite_dimensions_on_both_sides() {
        use ducktape_view_guest::wire;
        let (mut view, _) = MembersView::boot();
        view.member_width = f64::INFINITY;
        assert!(view.snapshot().is_err());
        let bytes = wire::Snapshot {
            schema: MembersView::SNAPSHOT_SCHEMA.into(),
            state: wire::SnapshotValue::Bytes(wire::encode(&view)),
        }
        .encode()
        .unwrap();
        assert!(MembersView::restore(&bytes).is_err());
    }
    /// A tab the roster has nobody under names the tab, not the network.
    #[test]
    fn an_empty_tab_names_itself() {
        let (mut view, _) = MembersView::boot();
        view.connected = true;
        view.answered = true;
        view.filter = MembersFilter::Validators;
        view.rows.push(host::MemberRow {
            key: "reviewer".into(),
            is_agent: true,
            role: "agent".into(),
            ..Default::default()
        });
        let mut shown = Vec::new();
        view.view().for_each_mut(&mut |node| {
            if let ducktape_view_guest::wire::Node::Text { content, .. } = node {
                shown.push(content.clone());
            }
        });
        assert!(
            shown.iter().any(|text| text == "No validators"),
            "{shown:?}"
        );
        assert!(
            !shown.iter().any(|text| text == "No members yet"),
            "{shown:?}"
        );
    }
    #[test]
    fn view_fits_default_stack() {
        let (app, _) = MembersView::boot();
        let _ = app.view();
    }
}
impl MembersView {
    pub(crate) fn update(&mut self, message: Message) -> ::ducktape_view_guest::Task<Message> {
        match message {
            Message::SessionArrived(item) => self.on_session_arrived(item),
            Message::RosterArrived(item) => self.on_roster_arrived(item),
            Message::ActDone(item) => self.on_act_done(item),
            Message::PickFilter(next) => self.on_pick_filter(next),
            Message::OpenMember(key) => self.on_open_member(key),
            Message::CopyKey(text, label) => self.on_copy_key(text, label),
            Message::MemberResized(dx, _dy) => self.on_member_resized(dx, _dy),
            Message::ViewportChanged(width, _height) => self.on_viewport_changed(width, _height),
            Message::SetAgentStatus(agent_id, paused) => self.on_set_agent_status(agent_id, paused),
            Message::OpenBallot(action, key) => self.on_open_ballot(action, key),
        }
    }
    fn on_session_arrived(
        &mut self,
        item: crate::host::SessionItem,
    ) -> ::ducktape_view_guest::Task<Message> {
        {
            self.host_error = sentence("Couldn't read the session", &item.error);
            if !(item.error).is_empty() {
                return ::ducktape_view_guest::Task::none();
            }
            let next = item.next.clone();
            self.connection_serial = crate::host::connection_serial_after(
                self.connected,
                next.connected,
                self.connection_serial,
            );
            self.admin = next.admin;
            self.connected = next.connected;
            self.dark = next.dark;
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_roster_arrived(
        &mut self,
        item: crate::host::RosterItem,
    ) -> ::ducktape_view_guest::Task<Message> {
        {
            self.host_error = sentence("Couldn't read the roster", &item.error);
            self.answered = true;
            if !(item.error).is_empty() {
                return ::ducktape_view_guest::Task::none();
            }
            self.rows = item.rows.clone();
            self.height = item.height;
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_act_done(&mut self, item: crate::host::ActItem) -> ::ducktape_view_guest::Task<Message> {
        {
            self.acting = "".to_owned();
            self.host_error = sentence("The node refused the change", &item.error);
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_pick_filter(&mut self, next: MembersFilter) -> ::ducktape_view_guest::Task<Message> {
        {
            self.filter = next;
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_open_member(&mut self, key: String) -> ::ducktape_view_guest::Task<Message> {
        {
            self.selected = key.to_owned();
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_copy_key(&mut self, text: String, label: String) -> ::ducktape_view_guest::Task<Message> {
        {
            crate::host::copy(&text, &label);
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_member_resized(&mut self, dx: f64, _dy: f64) -> ::ducktape_view_guest::Task<Message> {
        {
            self.member_width =
                crate::host::member_width_after_delta(self.member_width, -dx, self.viewport_width);
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_viewport_changed(
        &mut self,
        width: f64,
        _height: f64,
    ) -> ::ducktape_view_guest::Task<Message> {
        {
            self.viewport_width = width;
            self.member_width =
                crate::host::member_width_after_delta(self.member_width, 0.0, width);
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_set_agent_status(
        &mut self,
        agent_id: String,
        paused: bool,
    ) -> ::ducktape_view_guest::Task<Message> {
        {
            if (!self.connected) || (!(self.acting).is_empty()) {
                return ::ducktape_view_guest::Task::none();
            }
            self.acting = agent_id.to_owned();
            let _sent =
                crate::host::agent_status(::std::convert::AsRef::as_ref(&(agent_id)), paused);
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_open_ballot(
        &mut self,
        action: String,
        key: String,
    ) -> ::ducktape_view_guest::Task<Message> {
        {
            if ((!self.connected) || (!self.admin)) || (!(self.acting).is_empty()) {
                return ::ducktape_view_guest::Task::none();
            }
            self.acting = key.to_owned();
            let _sent = crate::host::propose(
                ::std::convert::AsRef::as_ref(&(action)),
                ::std::convert::AsRef::as_ref(&(key)),
                self.height,
            );
            ::ducktape_view_guest::Task::none()
        }
    }
}
impl MembersView {
    pub(crate) fn view(&self) -> ducktape_view_guest::wire::Node {
        use ducktape_view_guest::{
            kit::{self, Tone},
            slots, wire,
        };
        kit::set_dark(self.dark);
        let mut roster = vec![kit::centered_row(
            "members/head",
            [
                kit::sized(
                    kit::title("members/title", "Members"),
                    Some(wire::Length::Fill),
                    None,
                ),
                kit::nowrap(kit::secondary(
                    "members/summary",
                    host::members_summary(self.connected, &self.rows),
                )),
            ],
        )];
        if !self.host_error.is_empty() {
            roster.push(kit::notice(
                "members/error",
                kit::wrapping(kit::text("members/error-text", &self.host_error)),
                Tone::Danger,
            ));
        }
        if !self.connected {
            roster.push(kit::empty_state(
                "members/disconnected",
                "Not connected",
                "Choose a network from the sidebar to read who may act on it.",
            ));
        } else {
            let filters = [
                ("all", "All", "Show all members", MembersFilter::All),
                (
                    "humans",
                    "Humans",
                    "Show humans only",
                    MembersFilter::Humans,
                ),
                (
                    "agents",
                    "Agents",
                    "Show agents only",
                    MembersFilter::Agents,
                ),
                (
                    "validators",
                    "Validators",
                    "Show validators only",
                    MembersFilter::Validators,
                ),
            ];
            let choices = filters.map(|(id, label, _, filter)| {
                let count = host::filter_members(&self.rows, filter).len();
                (
                    id.to_owned(),
                    format!("{label}  {count}"),
                    self.filter == filter,
                    Some(slots::message(Message::PickFilter(filter))),
                )
            });
            let mut strip = kit::tabs("members/filter", choices);
            let wire::Node::Linear { children, .. } = &mut strip else {
                unreachable!("a tab strip is a row")
            };
            // the strip reads as a switch, not a sentence: the name a
            // reader hears is the filter it applies, not its count.
            for (node, (_, _, description, _)) in children.iter_mut().zip(filters) {
                let wire::Node::Button { label, height, .. } = node else {
                    unreachable!("a tab is a button")
                };
                *label = Some(description.into());
                *height = Some(wire::Length::Fixed(28.));
            }
            roster.push(strip);
            let members = host::filter_members(&self.rows, self.filter);
            let waiting = !self.answered;
            if waiting {
                roster.push(kit::secondary("members/loading", "Reading the roster…"));
            }
            // a refused read is the notice above, not an empty network
            let nobody = members.is_empty() && self.answered && self.host_error.is_empty();
            if nobody {
                let (title, detail) = host::empty_words(self.filter);
                roster.push(kit::empty_state("members/empty", title, detail));
            }
            let mut rows = Vec::new();
            for member in &members {
                // a hairline between rows, and none above the first: the
                // rule separates two members, it does not frame the list.
                if !rows.is_empty() {
                    rows.push(kit::divider(format!("members/rule/{}", member.key)));
                }
                rows.push(self.member_row(member));
            }
            roster.push(kit::spaced(kit::column("members/rows", rows), 0.));
        }
        // the page's Fill height would pin the content to the viewport and
        // leave nothing to scroll; the list is as tall as its rows
        let mut panes = vec![kit::scroll(
            "members/roster",
            kit::sized(
                kit::page("members/list", roster),
                Some(wire::Length::Fill),
                None,
            ),
        )];
        if let Some(member) = self
            .rows
            .iter()
            .find(|member| self.connected && member.key == self.selected)
        {
            panes.push(wire::Node::ResizeHandle {
                key: "members/member-resize".into(),
                on_press: None,
                on_release: None,
                on_drag: Some(slots::handler::<(f64, f64), Message>(Box::new(
                    |(dx, dy)| Some(Message::MemberResized(dx, dy)),
                ))),
                cursor: Some(wire::mouse::Cursor::ResizingHorizontally),
                // a 10px grip around the hairline: the handle is as wide
                // as its child, and a 1px rule is nothing to grab
                content: Box::new(kit::sized(
                    grip(
                        "members/divider",
                        kit::vertical_divider("members/divider/rule"),
                    ),
                    Some(wire::Length::Fixed(10.)),
                    Some(wire::Length::Fill),
                )),
            });
            panes.push(kit::pane(
                "members/member",
                kit::scroll("members/record-scroll", self.member_record(member)),
                wire::Length::Fixed(self.member_width as f32),
            ));
        }
        let observe = || {
            slots::handler::<(f32, f32), Message>(Box::new(|(width, height)| {
                Some(Message::ViewportChanged(width.into(), height.into()))
            }))
        };
        wire::Node::Sensor {
            key: "members/viewport".into(),
            reset: None,
            on_show: Some(observe()),
            on_resize: Some(observe()),
            on_hide: None,
            anticipate: None,
            delay: None,
            child: Box::new(kit::sized(
                kit::row("members", panes),
                Some(wire::Length::Fill),
                Some(wire::Length::Fill),
            )),
        }
    }
    /// One roster row on one line: who, what they are, and whether they are
    /// here. The whole row presses open their record.
    fn member_row(&self, member: &host::MemberRow) -> ducktape_view_guest::wire::Node {
        use ducktape_view_guest::{
            kit::{self, Tone},
            slots, wire,
        };
        let key = format!("members/row/{}", member.key);
        let mut line = vec![
            kit::avatar(
                format!("{key}/avatar"),
                kit::initials(&member.label),
                avatar_tone(member),
            ),
            kit::nowrap(kit::strong(format!("{key}/name"), &member.label)),
        ];
        if member.is_this_node {
            line.push(kit::badge(
                format!("{key}/local"),
                "This node",
                Tone::Accent,
            ));
        }
        line.push(kit::badge(
            format!("{key}/role"),
            host::sentence_case(&member.role),
            Tone::Neutral,
        ));
        if !member.model.is_empty() {
            line.push(kit::nowrap(kit::caption(
                format!("{key}/model"),
                &member.model,
            )));
        }
        line.push(kit::space(Some(wire::Length::Fill), None));
        line.push(presence_chip(format!("{key}/live"), member));
        let mut button = kit::list_row(
            &key,
            kit::spaced(kit::centered_row(format!("{key}/details"), line), 6.),
            self.selected == member.key,
            Some(slots::message(Message::OpenMember(member.key.clone()))),
        );
        if let wire::Node::Button { label, .. } = &mut button {
            *label = Some(member.label.clone());
        }
        kit::sized(
            button,
            Some(wire::Length::Fill),
            Some(wire::Length::Fixed(32.)),
        )
    }
    fn member_record(&self, member: &host::MemberRow) -> ducktape_view_guest::wire::Node {
        use ducktape_view_guest::{
            kit::{self, Tone},
            slots, wire,
        };
        let key = format!("members/record/{}", member.key);
        let (key_name, copy_name, copied) = key_words(member);
        let mut close = kit::button(
            format!("{key}/close"),
            "Close",
            Some(slots::message(Message::OpenMember(String::new()))),
            wire::ButtonPreset::Subtle,
        );
        if let wire::Node::Button { label, .. } = &mut close {
            *label = Some("Close member".into());
        }
        let header = kit::sized(
            kit::padded(
                kit::centered_row(
                    format!("{key}/header"),
                    [
                        kit::avatar(
                            format!("{key}/avatar"),
                            kit::initials(&member.label),
                            avatar_tone(member),
                        ),
                        kit::sized(
                            kit::nowrap(kit::strong(format!("{key}/name"), &member.label)),
                            Some(wire::Length::Fill),
                            None,
                        ),
                        close,
                    ],
                ),
                wire::Edges {
                    top: 0.,
                    right: 8.,
                    bottom: 0.,
                    left: 12.,
                },
            ),
            Some(wire::Length::Fill),
            Some(wire::Length::Fixed(40.)),
        );
        let mut copy = kit::button(
            format!("{key}/copy"),
            "Copy",
            Some(slots::message(Message::CopyKey(
                member.key.clone(),
                copied.into(),
            ))),
            wire::ButtonPreset::Subtle,
        );
        if let wire::Node::Button { label, .. } = &mut copy {
            *label = Some(copy_name.into());
        }
        let mut facts = vec![
            kit::kv(
                format!("{key}/role"),
                "role",
                kit::badge(
                    format!("{key}/role-badge"),
                    host::sentence_case(&member.role),
                    Tone::Neutral,
                ),
            ),
            kit::kv(
                format!("{key}/presence"),
                "presence",
                presence_chip(format!("{key}/status"), member),
            ),
            // a 64-hex key has no room beside a 140px label in a 312px
            // pane: the name and its copy control share a line, the key
            // wraps at full width under them
            kit::spaced(
                kit::column(
                    format!("{key}/key-row"),
                    [
                        kit::centered_row(
                            format!("{key}/key-cell"),
                            [
                                kit::sized(
                                    kit::nowrap(kit::secondary(
                                        format!("{key}/key-label"),
                                        key_name,
                                    )),
                                    Some(wire::Length::Fill),
                                    None,
                                ),
                                copy,
                            ],
                        ),
                        kit::wrapping(kit::mono(format!("{key}/key"), &member.key)),
                    ],
                ),
                4.,
            ),
        ];
        if !member.model.is_empty() {
            facts.push(kit::kv(
                format!("{key}/capability"),
                "capability",
                kit::wrapping(kit::text(format!("{key}/capability-value"), &member.model)),
            ));
        }
        let mut actions = Vec::new();
        // the record waits on its own write: the button says so, and no
        // second press leaves until the kernel answers the first
        let sending = self.acting == member.key;
        let idle = self.acting.is_empty();
        if member.is_agent {
            let word = match (sending, member.live) {
                (true, _) => "Sending…",
                (false, true) => "Pause agent",
                (false, false) => "Resume agent",
            };
            actions.push(kit::button(
                format!("{key}/status-change"),
                word,
                idle.then(|| {
                    slots::message(Message::SetAgentStatus(member.key.clone(), member.live))
                }),
                wire::ButtonPreset::Secondary,
            ));
            actions.push(kit::wrapping(kit::caption(
                format!("{key}/owner-gate"),
                "Only the agent's owner or its current controller can pause or resume it.",
            )));
        }
        let ballot = match (
            self.admin,
            member.is_agent,
            member.role.as_str(),
            member.is_this_node,
        ) {
            (true, false, "resident", _) => Some(("add_validator", "Promote to validator")),
            (true, false, "validator", false) => {
                Some(("remove_validator", "Remove from the validator set"))
            }
            _ => None,
        };
        if let Some((action, label)) = ballot {
            let word = match sending {
                true => "Sending…",
                false => label,
            };
            actions.push(kit::button(
                format!("{key}/ballot"),
                word,
                idle.then(|| {
                    slots::message(Message::OpenBallot(action.into(), member.key.clone()))
                }),
                wire::ButtonPreset::Secondary,
            ));
            actions.push(kit::wrapping(kit::caption(
                format!("{key}/quorum"),
                "Opens a ballot the validators vote on under Governance.",
            )));
        }
        let reads_only = !self.admin && !member.is_this_node && !member.is_agent;
        if reads_only {
            actions.push(kit::wrapping(kit::caption(
                format!("{key}/validator-gate"),
                "Only a validator node may open a membership proposal.",
            )));
        }
        // this node's own seat: nothing to press, and the reason why
        let own_seat = self.admin && member.is_this_node && member.role == "validator";
        if own_seat {
            actions.push(kit::wrapping(kit::caption(
                format!("{key}/own-seat"),
                "This node holds a validator seat. Another validator opens the ballot to remove it.",
            )));
        }
        if !actions.is_empty() {
            facts.push(kit::divider(format!("{key}/rule")));
            facts.push(kit::spaced(
                kit::column(format!("{key}/actions"), actions),
                8.,
            ));
        }
        kit::spaced(
            kit::column(
                key.clone(),
                [
                    header,
                    kit::divider(format!("{key}/header-rule")),
                    kit::padded(
                        kit::spaced(kit::column(format!("{key}/facts"), facts), 10.),
                        wire::Edges::all(12.),
                    ),
                ],
            ),
            0.,
        )
    }
}
/// A container that centres its child: the grip a hairline sits in.
fn grip(key: &str, child: ducktape_view_guest::wire::Node) -> ducktape_view_guest::wire::Node {
    use ducktape_view_guest::{kit, wire};
    let mut node = kit::container(key, child);
    if let wire::Node::Container { align_x, .. } = &mut node {
        *align_x = Some(wire::AlignX::Center);
    }
    node
}

/// A kernel refusal as a person reads it: a verb, then the reason. An empty
/// reason is no error at all.
fn sentence(verb: &str, reason: &str) -> String {
    match reason.is_empty() {
        true => String::new(),
        false => format!("{verb}: {reason}"),
    }
}

/// The agent tone is the one mark a machine carries in the roster: it sits
/// on the avatar, so a row reads as a person or a program at a glance.
fn avatar_tone(member: &host::MemberRow) -> ducktape_view_guest::kit::Tone {
    use ducktape_view_guest::kit::Tone;
    match member.is_agent {
        true => Tone::Agent,
        false => Tone::Neutral,
    }
}

/// Presence, in the vocabulary `host::presence_label` chose: a live member
/// is a success badge, an agent's registration state a neutral one, and
/// absence a faint caption — offline is not a state worth a colour.
fn presence_chip(key: String, member: &host::MemberRow) -> ducktape_view_guest::wire::Node {
    use ducktape_view_guest::kit::{self, Tone};
    let word = host::sentence_case(host::presence_label(member));
    match (member.is_agent, member.live) {
        (true, _) => kit::badge(key, word, Tone::Neutral),
        (false, true) => kit::badge(key, word, Tone::Success),
        (false, false) => kit::nowrap(kit::caption(key, word)),
    }
}

/// What a member's identifier is called, what its copy control is named,
/// and what the toast says once the clipboard has it. An agent holds no
/// node key, so its cell is an agent id.
fn key_words(member: &host::MemberRow) -> (&'static str, &'static str, &'static str) {
    match (member.is_this_node, member.is_agent) {
        (true, _) => ("public key", "Copy this node's key", "Node key copied"),
        (false, true) => ("agent id", "Copy agent id", "Agent id copied"),
        (false, false) => ("public key", "Copy public key", "Public key copied"),
    }
}

ducktape_view_guest::export_app!(
    MembersView,
    "Members",
    "Who may act on this network: validators, residents and registered agents.",
    ["members"]
);
