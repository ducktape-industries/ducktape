//! The Approvals screen as a module-owned view: every decision the network
//! is being asked to make, and the ones it has settled, rendered from a
//! wasm component the desktop app loads from a file.
//!
//! The kernel pushes session facts only (`governance.props`: connected,
//! admin, dark). The view reads its own register through the kernel's
//! `rpc.query` / `rpc.blocks`, re-reads it on every `rpc.live` hit for the
//! governance plane, and a vote or a settle leaves as `op.submit` — the
//! governance message the kernel signs with the seated key. Signing secrets
//! and passwords stay in the host; public proposal data belongs to the guest.
pub mod host;
#[derive(serde::Serialize, serde::Deserialize)]
pub struct GovernanceView {
    pub(crate) rows: Vec<crate::host::ProposalRow>,
    pub(crate) voting: String,
    pub(crate) admin: bool,
    pub(crate) connected: bool,
    pub(crate) connection_serial: i64,
    pub(crate) answered: bool,
    pub(crate) host_error: String,
    #[serde(default)]
    pub(crate) dark: bool,
}
impl ::std::fmt::Debug for GovernanceView {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("GovernanceView")
    }
}
#[derive(Clone)]
pub enum Message {
    SessionArrived(crate::host::SessionItem),
    RegisterArrived(crate::host::RegisterItem),
    ActDone(crate::host::ActItem),
    GovVote(String, bool),
    GovExecute(String),
}
impl ::std::fmt::Debug for Message {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("Message")
    }
}
impl GovernanceView {
    fn state() -> Self {
        Self {
            rows: Vec::new(),
            voting: "".to_owned(),
            admin: false,
            connected: false,
            connection_serial: 0,
            answered: false,
            host_error: "".to_owned(),
            dark: false,
        }
    }
    pub(crate) fn boot() -> (Self, ::ducktape_view_guest::Task<Message>) {
        (Self::state(), ::ducktape_view_guest::Task::none())
    }
    pub(crate) const PREFERRED_WINDOW_SIZE: &'static str = "none";
    pub(crate) const SNAPSHOT_SCHEMA: &'static str =
        "7c12db27b05b027805b40f4d493f95bcbf83f7b71fb9a350d90ef241043cbc72";
}
impl GovernanceView {
    pub(crate) fn snapshot(&self) -> Result<Vec<u8>, String> {
        use ducktape_view_guest::wire;
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
            return Err("invalid Governance snapshot schema".into());
        }
        let wire::SnapshotValue::Bytes(state) = snapshot.state else {
            return Err("invalid Governance snapshot".into());
        };
        wire::decode(&state)
    }
    fn subscription(&self) -> ::ducktape_view_guest::Subscription<Message> {
        ::ducktape_view_guest::Subscription::batch([
            crate::host::session().map(Message::SessionArrived),
            if self.connected {
                ::ducktape_view_guest::Subscription::batch([crate::host::register(
                    self.connection_serial,
                )
                .map(Message::RegisterArrived)])
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
    fn an_empty_open_register_makes_no_claim_about_network_history() {
        let (mut view, _) = GovernanceView::boot();
        view.connected = true;
        view.answered = true;
        let mut message = None;
        view.view().for_each_mut(&mut |node| {
            if let ducktape_view_guest::wire::Node::Text { key, content, .. } = node
                && key == "governance/empty"
            {
                message = Some(content.clone());
            }
        });
        assert_eq!(message.as_deref(), Some("No proposals waiting."));
    }
    #[test]
    fn disconnected_view_hides_retained_proposals_and_actions() {
        let (mut view, _) = GovernanceView::boot();
        view.rows.push(host::ProposalRow {
            id: "stale-proposal".into(),
            open: true,
            ..Default::default()
        });
        let mut tree = view.view();
        tree.for_each_mut(&mut |node| {
            assert!(!node.key().is_some_and(|key| key.contains("stale-proposal")));
            assert!(!matches!(
                node,
                ducktape_view_guest::wire::Node::Button {
                    on_press: Some(_),
                    ..
                }
            ));
        });
        assert_eq!(
            view.rows.len(),
            1,
            "disconnect does not discard snapshot state"
        );
    }
    #[test]
    fn snapshot_preserves_proposals_without_theme_bookkeeping() {
        let (mut view, _) = GovernanceView::boot();
        view.rows.push(host::ProposalRow {
            id: "proposal".into(),
            approvals: 2,
            required_yes: 3,
            open: true,
            ..Default::default()
        });
        view.connected = true;
        let bytes = view.snapshot().unwrap();
        let restored = GovernanceView::restore(&bytes).unwrap();
        assert_eq!(restored.rows, view.rows);
        assert_eq!(restored.snapshot().unwrap(), bytes);
    }
    #[test]
    fn view_fits_default_stack() {
        ::std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(|| {
                let (app, _) = GovernanceView::boot();
                let _ = app.view();
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
impl GovernanceView {
    pub(crate) fn update(&mut self, message: Message) -> ::ducktape_view_guest::Task<Message> {
        match message {
            Message::SessionArrived(item) => self.on_session_arrived(item),
            Message::RegisterArrived(item) => self.on_register_arrived(item),
            Message::ActDone(item) => self.on_act_done(item),
            Message::GovVote(proposal_id, approve) => self.on_gov_vote(proposal_id, approve),
            Message::GovExecute(proposal_id) => self.on_gov_execute(proposal_id),
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
    fn on_register_arrived(
        &mut self,
        item: crate::host::RegisterItem,
    ) -> ::ducktape_view_guest::Task<Message> {
        {
            self.host_error = item.error.to_owned();
            self.answered = true;
            if !(item.error).is_empty() {
                return ::ducktape_view_guest::Task::none();
            }
            self.rows = item.rows.clone();
            crate::host::badge(crate::host::open_proposals(&self.rows));
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_act_done(&mut self, item: crate::host::ActItem) -> ::ducktape_view_guest::Task<Message> {
        {
            self.voting = "".to_owned();
            self.host_error = item.error.to_owned();
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_gov_vote(
        &mut self,
        proposal_id: String,
        approve: bool,
    ) -> ::ducktape_view_guest::Task<Message> {
        {
            if (!self.connected) || (!(self.voting).is_empty()) {
                return ::ducktape_view_guest::Task::none();
            }
            self.voting = proposal_id.to_owned();
            let _sent = crate::host::vote(proposal_id.to_owned(), approve);
            ::ducktape_view_guest::Task::none()
        }
    }
    fn on_gov_execute(&mut self, proposal_id: String) -> ::ducktape_view_guest::Task<Message> {
        {
            if (!self.connected) || (!(self.voting).is_empty()) {
                return ::ducktape_view_guest::Task::none();
            }
            self.voting = proposal_id.to_owned();
            let _sent = crate::host::execute(proposal_id.to_owned());
            ::ducktape_view_guest::Task::none()
        }
    }
}
impl GovernanceView {
    pub(crate) fn view(&self) -> ducktape_view_guest::wire::Node {
        use ducktape_view_guest::{
            kit::{self, Tone},
            wire,
        };
        kit::set_dark(self.dark);
        let mut content = vec![kit::centered_row(
            "governance/header",
            [
                kit::sized(
                    kit::container(
                        "governance/seal",
                        wire::Node::Surface {
                            key: "governance/seal/svg".into(),
                            name: "artifact_svg".into(),
                            args: vec![wire::SurfaceValue::Str("icons/seal.svg".into())],
                            on_event: None,
                        },
                    ),
                    Some(wire::Length::Fixed(24.)),
                    Some(wire::Length::Fixed(24.)),
                ),
                kit::sized(
                    kit::title("governance/title", "Approvals"),
                    Some(wire::Length::Fill),
                    None,
                ),
                kit::nowrap(kit::secondary(
                    "governance/summary",
                    host::proposals_summary(self.connected, &self.rows),
                )),
            ],
        )];
        if !self.host_error.is_empty() {
            content.push(kit::notice(
                "governance/error",
                kit::wrapping(kit::text("governance/error-text", &self.host_error)),
                Tone::Danger,
            ));
        }
        if !self.connected {
            content.push(kit::empty_state(
                "governance/disconnected",
                "Not connected",
                "Choose a network from the sidebar to read what it is deciding.",
            ));
            return kit::reading_page("governance", content);
        }
        if !self.admin {
            content.push(kit::notice(
                "governance/standing",
                kit::wrapping(kit::text(
                    "governance/standing-text",
                    "Approval votes are cast by this network's validators, and this node does not hold validator standing. You can still read every proposal and follow its tally while it runs.",
                )),
                Tone::Neutral,
            ));
        }
        let open = host::open_proposals(&self.rows);
        if open > 0 {
            content.push(kit::heading(
                "governance/pending",
                host::pending_label(&self.rows),
            ));
        } else if self.answered {
            content.push(kit::empty_state(
                "governance/empty-state",
                "No proposals waiting.",
                "Membership and module changes show up here when a validator opens one.",
            ));
            if let wire::Node::Linear { children, .. } = content.last_mut().unwrap()
                && let Some(wire::Node::Text { key, .. }) = children.first_mut()
            {
                *key = "governance/empty".into();
            }
        }
        for proposal in self.rows.iter().filter(|proposal| proposal.open) {
            content.push(self.proposal(proposal));
        }
        let settled = host::settled_proposals(&self.rows);
        if !settled.is_empty() {
            content.push(kit::heading("governance/finalized", "Recently finalized"));
            let mut rows = Vec::new();
            for proposal in settled {
                let key = format!("governance/settled/{}", proposal.id);
                let passed = proposal.status.eq_ignore_ascii_case("passed")
                    || proposal.status.eq_ignore_ascii_case("executed");
                let mut details = vec![
                    kit::sized(
                        kit::spaced(
                            kit::column(
                                format!("{key}/lines"),
                                [
                                    kit::nowrap(kit::mono(format!("{key}/id"), &proposal.id)),
                                    kit::wrapping(kit::secondary(
                                        format!("{key}/action"),
                                        &proposal.action,
                                    )),
                                ],
                            ),
                            2.,
                        ),
                        Some(wire::Length::Fill),
                        None,
                    ),
                    kit::badge(
                        format!("{key}/status"),
                        &proposal.status,
                        if passed { Tone::Success } else { Tone::Neutral },
                    ),
                ];
                if proposal.settled_height > 0 {
                    details.push(kit::nowrap(kit::caption(
                        format!("{key}/height"),
                        host::height_label_short(proposal.settled_height),
                    )));
                }
                rows.push(kit::divider(format!("{key}/rule")));
                rows.push(kit::centered_row(key, details));
            }
            rows.remove(0);
            content.push(kit::card(
                "governance/settled",
                kit::spaced(kit::column("governance/settled/rows", rows), 8.),
            ));
        }
        kit::reading_page("governance", content)
    }
    fn proposal(&self, proposal: &host::ProposalRow) -> ducktape_view_guest::wire::Node {
        use ducktape_view_guest::{
            kit::{self, Tone},
            slots, wire,
        };
        let key = format!("governance/proposal/{}", proposal.id);
        let available = self.voting.is_empty();
        let met = proposal.approvals >= proposal.required_yes;
        let mut content = vec![
            kit::centered_row(
                format!("{key}/head"),
                [
                    kit::sized(
                        kit::wrapping(kit::heading(format!("{key}/action"), &proposal.action)),
                        Some(wire::Length::Fill),
                        None,
                    ),
                    kit::badge(
                        format!("{key}/tally"),
                        host::tally_label(proposal.approvals, proposal.required_yes),
                        if met { Tone::Success } else { Tone::Accent },
                    ),
                ],
            ),
            kit::spaced(
                kit::centered_row(
                    format!("{key}/meta"),
                    [
                        kit::nowrap(kit::mono(format!("{key}/id"), &proposal.id)),
                        kit::caption(
                            format!("{key}/proposer"),
                            format!(
                                "proposed by @{} · expires at h{}",
                                proposal.proposer, proposal.deadline
                            ),
                        ),
                    ],
                ),
                10.,
            ),
        ];
        if !proposal.detail.is_empty() {
            content.push(kit::wrapping(kit::text(format!("{key}/detail"), &proposal.detail)));
        }
        let mut tally = vec![kit::secondary(
            format!("{key}/quorum"),
            host::tally_note(proposal.approvals, proposal.required_yes),
        )];
        if proposal.rejections > 0 {
            tally.push(kit::tone_text(
                format!("{key}/rejections"),
                format!("{} against", proposal.rejections),
                Tone::Danger,
            ));
        }
        content.push(kit::spaced(kit::centered_row(format!("{key}/votes"), tally), 10.));
        let reject = kit::button(
            format!("{key}/reject"),
            "Reject",
            available.then(|| slots::message(Message::GovVote(proposal.id.clone(), false))),
            wire::ButtonPreset::Danger,
        );
        let mut approval = if met {
            kit::button(
                format!("{key}/settle"),
                "Settle",
                available.then(|| slots::message(Message::GovExecute(proposal.id.clone()))),
                wire::ButtonPreset::Primary,
            )
        } else {
            kit::button(
                format!("{key}/approve"),
                host::approve_label(proposal.approvals, proposal.required_yes),
                available.then(|| slots::message(Message::GovVote(proposal.id.clone(), true))),
                wire::ButtonPreset::Primary,
            )
        };
        if let wire::Node::Button { label, .. } = &mut approval {
            *label = Some(if met { "Settle" } else { "Approve" }.into());
        }
        content.push(kit::divider(format!("{key}/rule")));
        content.push(kit::spaced(
            kit::row(format!("{key}/actions"), [kit::spacer(), reject, approval]),
            8.,
        ));
        kit::card(&key, kit::spaced(kit::column(format!("{key}/body"), content), 10.))
    }
}
ducktape_view_guest::export_app!(
    GovernanceView,
    "Approvals",
    "Every decision this network is being asked to make, and the ones it has settled.",
    ["governance"]
);
