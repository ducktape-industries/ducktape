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
    #[serde(default)]
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
        "c5b4c71dda09d5a068e1b5197b676ac214130791d9b62a6629ac8f67428df93e";
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
            self.host_error = item.error.to_owned();
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
            self.host_error = item.error.to_owned();
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
            self.host_error = item.error.to_owned();
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
            let tabs = filters.into_iter().map(|(key, label, description, filter)| {
                let count = host::filter_members(&self.rows, filter).len();
                let mut button = kit::button(
                    format!("members/filter/{key}"),
                    format!("{label}  {count}"),
                    Some(slots::message(Message::PickFilter(filter))),
                    wire::ButtonPreset::Subtle,
                );
                if let wire::Node::Button { label, checked, .. } = &mut button {
                    *label = Some(description.into());
                    *checked = Some(self.filter == filter);
                }
                button
            });
            roster.push(kit::spaced(kit::row("members/filters", tabs), 2.));
            let members = host::filter_members(&self.rows, self.filter);
            if members.is_empty() && self.answered {
                roster.push(kit::empty_state(
                    "members/empty",
                    "No members here yet",
                    "Validators, residents and registered agents appear as they join.",
                ));
            }
            let mut rows = Vec::new();
            for member in members {
                let key = format!("members/row/{}", member.key);
                let tone = if member.is_agent {
                    Tone::Agent
                } else {
                    Tone::Neutral
                };
                let mut name_line = vec![kit::nowrap(kit::strong(
                    format!("{key}/name"),
                    &member.label,
                ))];
                if member.is_this_node {
                    name_line.push(kit::badge(format!("{key}/local"), "this node", Tone::Accent));
                }
                let mut meta = vec![kit::badge(format!("{key}/role"), &member.role, tone)];
                if !member.model.is_empty() {
                    meta.push(kit::caption(format!("{key}/model"), &member.model));
                }
                let live_tone = if member.live {
                    Tone::Success
                } else {
                    Tone::Neutral
                };
                let content = kit::centered_row(
                    format!("{key}/details"),
                    [
                        kit::avatar(format!("{key}/avatar"), kit::initials(&member.label), tone),
                        kit::sized(
                            kit::spaced(
                                kit::column(
                                    format!("{key}/lines"),
                                    [
                                        kit::spaced(
                                            kit::centered_row(format!("{key}/name-line"), name_line),
                                            6.,
                                        ),
                                        kit::spaced(
                                            kit::centered_row(format!("{key}/meta"), meta),
                                            6.,
                                        ),
                                    ],
                                ),
                                3.,
                            ),
                            Some(wire::Length::Fill),
                            None,
                        ),
                        kit::badge(
                            format!("{key}/live"),
                            host::presence_label(&member),
                            live_tone,
                        ),
                    ],
                );
                let mut button = kit::list_row(
                    &key,
                    content,
                    self.selected == member.key,
                    Some(slots::message(Message::OpenMember(member.key.clone()))),
                );
                if let wire::Node::Button { label, .. } = &mut button {
                    *label = Some(member.label.clone());
                }
                rows.push(button);
            }
            roster.push(kit::spaced(kit::column("members/rows", rows), 2.));
        }
        let mut panes = vec![kit::scroll(
            "members/roster",
            kit::page("members/list", roster),
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
                content: Box::new(kit::sized(
                    kit::container(
                        "members/divider",
                        wire::Node::Space {
                            width: None,
                            height: None,
                        },
                    ),
                    Some(wire::Length::Fixed(6.)),
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
    fn member_record(&self, member: &host::MemberRow) -> ducktape_view_guest::wire::Node {
        use ducktape_view_guest::{
            kit::{self, Tone},
            slots, wire,
        };
        let key = format!("members/record/{}", member.key);
        let tone = if member.is_agent {
            Tone::Agent
        } else {
            Tone::Neutral
        };
        let presence_tone = if member.live {
            Tone::Success
        } else {
            Tone::Neutral
        };
        let mut details = vec![
            kit::centered_row(
                format!("{key}/header"),
                [
                    kit::sized(
                        kit::label(format!("{key}/title"), "Member"),
                        Some(wire::Length::Fill),
                        None,
                    ),
                    kit::button(
                        format!("{key}/close"),
                        "Close member",
                        Some(slots::message(Message::OpenMember(String::new()))),
                        wire::ButtonPreset::Subtle,
                    ),
                ],
            ),
            kit::spaced(
                kit::centered_row(
                    format!("{key}/identity"),
                    [
                        kit::avatar(format!("{key}/avatar"), kit::initials(&member.label), tone),
                        kit::wrapping(kit::heading(format!("{key}/name"), &member.label)),
                    ],
                ),
                10.,
            ),
            kit::spaced(
                kit::centered_row(
                    format!("{key}/badges"),
                    [
                        kit::badge(format!("{key}/role"), &member.role, tone),
                        kit::badge(
                            format!("{key}/status"),
                            host::presence_label(member),
                            presence_tone,
                        ),
                    ],
                ),
                6.,
            ),
            kit::divider(format!("{key}/rule")),
            kit::spaced(
                kit::column(
                    format!("{key}/key-block"),
                    [
                        kit::label(
                            format!("{key}/key-label"),
                            if member.is_agent {
                                "agent id"
                            } else {
                                "public key"
                            },
                        ),
                        kit::wrapping(kit::mono(format!("{key}/key"), &member.key)),
                    ],
                ),
                4.,
            ),
        ];
        if !member.model.is_empty() {
            details.push(kit::spaced(
                kit::column(
                    format!("{key}/capability"),
                    [
                        kit::label(format!("{key}/capability-label"), "capability"),
                        kit::text(format!("{key}/capability-value"), &member.model),
                    ],
                ),
                4.,
            ));
        }
        if member.is_this_node {
            details.push(kit::button(
                format!("{key}/copy"),
                "Copy this node's key",
                Some(slots::message(Message::CopyKey(
                    member.key.clone(),
                    "Node key copied".into(),
                ))),
                wire::ButtonPreset::Secondary,
            ));
        }
        if member.is_agent {
            details.push(kit::button(
                format!("{key}/status-change"),
                if member.live {
                    "Pause agent"
                } else {
                    "Resume agent"
                },
                self.acting.is_empty().then(|| {
                    slots::message(Message::SetAgentStatus(member.key.clone(), member.live))
                }),
                wire::ButtonPreset::Secondary,
            ));
            details.push(kit::wrapping(kit::caption(
                format!("{key}/owner-gate"),
                "Pause and resume are owner-gated writes. The model accepts changes from its program account or current controller.",
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
            details.push(kit::button(
                format!("{key}/ballot"),
                label,
                self.acting.is_empty().then(|| {
                    slots::message(Message::OpenBallot(action.into(), member.key.clone()))
                }),
                wire::ButtonPreset::Primary,
            ));
            details.push(kit::caption(format!("{key}/quorum"), "Needs quorum to pass."));
        }
        if !self.admin && !member.is_this_node && !member.is_agent {
            details.push(kit::wrapping(kit::caption(
                format!("{key}/validator-gate"),
                "Only a validator node may open a membership proposal.",
            )));
        }
        kit::spaced(
            kit::padded(kit::column(key, details), wire::Edges::all(20.)),
            14.,
        )
    }
}
ducktape_view_guest::export_app!(
    MembersView,
    "Members",
    "Who may act on this network: validators, residents and registered agents.",
    ["members"]
);
