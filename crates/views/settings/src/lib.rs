//! Settings as a module-owned view: this device's preferences and the account
//! this key speaks for, from the facts the desktop app pushes. Every act — a
//! theme, a rename, a minted ticket, the signing seat — leaves as an intent
//! the app signs.
pub mod host;
use ducktape_view_guest::{Subscription, Task, wire};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SettingsPane {
    General,
    Network,
    Account,
    Security,
}
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SettingsView {
    pub(crate) connected: bool,
    pub(crate) loading: bool,
    pub(crate) status: String,
    pub(crate) busy: bool,
    pub(crate) recovering: bool,
    pub(crate) appearance: String,
    pub(crate) desktop_notifications: bool,
    pub(crate) unlocked: bool,
    pub(crate) seat_key: String,
    pub(crate) account_name: String,
    pub(crate) network_name: String,
    pub(crate) connected_rpc: String,
    pub(crate) account_ceremony_phase: String,
    pub(crate) account_ceremony_qr: String,
    pub(crate) account_ceremony_detail: String,
    pub(crate) account_ceremony_left: String,
    pub(crate) settings_key_state: String,
    pub(crate) settings_key_path: String,
    pub(crate) account_number: String,
    pub(crate) account_exists: bool,
    pub(crate) account_busy: bool,
    pub(crate) account_ticket: String,
    pub(crate) connection_serial: i64,
    pub(crate) tier: String,
    pub(crate) admin: bool,
    pub(crate) members_line: String,
    pub(crate) members_answered: bool,
    pub(crate) account_key_rows: Vec<crate::host::AccountKeyRow>,
    pub(crate) renaming_to: String,
    pub(crate) account_name_draft: String,
    pub(crate) account_create_draft: String,
    pub(crate) account_key_draft: String,
    pub(crate) account_key_label_draft: String,
    pub(crate) account_join_draft: String,
    pub(crate) host_error: String,
    key_password: String,
    settings_pane: SettingsPane,
    pub(crate) dark: bool,
}
impl ::std::fmt::Debug for SettingsView {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("SettingsView")
    }
}
#[derive(Clone)]
pub enum Message {
    SessionArrived(Box<crate::host::SessionItem>),
    StandingArrived(crate::host::StandingItem),
    KeysArrived(crate::host::KeysItem),
    ShowTab(String),
    Reconnect,
    SwitchNetwork,
    SettingsUnlockSubmit(String),
    LockSession,
    AccountRenameSubmit,
    AccountCreateSubmit,
    AccountKeyAddSubmit,
    AccountKeyJoinSubmit,
    AccountKeyRemove(String),
    AccountPasskeySubmit,
    AccountPasskeyDesktop,
    AccountCeremonyCancel,
    AccountWalletSubmit,
    AccountLoginSubmit,
    CopyToClipboard(String, String),
    SetAppearanceLight,
    SetAppearanceDark,
    SetDesktopNotifications(bool),
    PickSettingsPane(SettingsPane),
    EditSettingsPassword(String),
    BindAccountNameDraft(String),
    BindAccountCreateDraft(String),
    BindAccountJoinDraft(String),
    BindAccountKeyDraft(String),
    BindAccountKeyLabelDraft(String),
}
impl ::std::fmt::Debug for Message {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("Message")
    }
}
impl SettingsView {
    fn state() -> Self {
        Self {
            connected: false,
            loading: false,
            status: "".to_owned(),
            busy: false,
            recovering: false,
            appearance: "system".to_owned(),
            desktop_notifications: true,
            unlocked: false,
            seat_key: "".to_owned(),
            account_name: "".to_owned(),
            network_name: "".to_owned(),
            connected_rpc: "".to_owned(),
            account_ceremony_phase: "".to_owned(),
            account_ceremony_qr: "".to_owned(),
            account_ceremony_detail: "".to_owned(),
            account_ceremony_left: "".to_owned(),
            settings_key_state: "".to_owned(),
            settings_key_path: "".to_owned(),
            account_number: "".to_owned(),
            account_exists: false,
            account_busy: false,
            account_ticket: "".to_owned(),
            connection_serial: 0,
            tier: "".to_owned(),
            admin: false,
            members_line: "".to_owned(),
            members_answered: false,
            account_key_rows: Vec::new(),
            renaming_to: "".to_owned(),
            account_name_draft: "".to_owned(),
            account_create_draft: "".to_owned(),
            account_key_draft: "".to_owned(),
            account_key_label_draft: "".to_owned(),
            account_join_draft: "".to_owned(),
            host_error: "".to_owned(),
            key_password: String::new(),
            settings_pane: SettingsPane::General,
            dark: false,
        }
    }
    pub(crate) fn boot() -> (Self, Task<Message>) {
        (Self::state(), Task::none())
    }
    pub(crate) const PREFERRED_WINDOW_SIZE: &'static str = "none";
    pub(crate) const SNAPSHOT_SCHEMA: &'static str =
        "59a2e96a909e916d315084c8325ec77a1202d06a2dae9576077cd16ffec89cd2";
    pub(crate) fn snapshot(&self) -> Result<Vec<u8>, String> {
        wire::Snapshot {
            schema: Self::SNAPSHOT_SCHEMA.into(),
            state: wire::SnapshotValue::Bytes(wire::encode(self)),
        }
        .encode()
    }
    pub(crate) fn restore(bytes: &[u8]) -> Result<Self, String> {
        let snapshot = wire::Snapshot::decode(bytes)?;
        if snapshot.schema != Self::SNAPSHOT_SCHEMA {
            return Err("snapshot schema mismatch".into());
        }
        let wire::SnapshotValue::Bytes(state) = snapshot.state else {
            return Err("snapshot state mismatch".into());
        };
        wire::decode(&state)
    }
}
impl SettingsView {
    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            crate::host::session().map(|value| Message::SessionArrived(Box::new(value))),
            if self.connected {
                crate::host::standing(self.connection_serial).map(Message::StandingArrived)
            } else {
                Subscription::none()
            },
            if self.connected && !self.seat_key.is_empty() {
                crate::host::account_keys(self.connection_serial, self.seat_key.to_owned())
                    .map(Message::KeysArrived)
            } else {
                Subscription::none()
            },
        ])
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_preserves_account_drafts_and_selected_settings_pane() {
        let (mut view, _) = SettingsView::boot();
        view.account_name_draft = "새 이름".into();
        view.account_key_draft = "aabb".into();
        view.account_key_label_draft = "휴대폰".into();
        view.account_create_draft = "new account".into();
        view.account_join_draft = "ticket".into();
        view.account_key_rows = vec![host::AccountKeyRow {
            scheme: "ed25519".into(),
            pubkey: "public-key".into(),
            label: "Laptop".into(),
        }];
        view.account_ceremony_phase = "show_qr".into();
        view.account_ceremony_qr = "ceremony payload".into();
        view.connection_serial = 17;
        view.settings_pane = SettingsPane::Account;
        view.key_password = "retained draft".into();
        let bytes = view.snapshot().unwrap();
        let restored = SettingsView::restore(&bytes).unwrap();
        assert_eq!(restored.account_name_draft, "새 이름");
        assert_eq!(restored.account_key_draft, "aabb");
        assert_eq!(restored.settings_pane, SettingsPane::Account);
        assert_eq!(restored.key_password, "retained draft");
        assert_eq!(restored.snapshot().unwrap(), bytes);
    }

    #[test]
    fn malformed_snapshot_is_rejected_without_partial_state() {
        let (view, _) = SettingsView::boot();
        let bytes = view.snapshot().unwrap();
        assert!(SettingsView::restore(&bytes[..bytes.len() / 2]).is_err());
        let mut snapshot = wire::Snapshot::decode(&bytes).unwrap();
        snapshot.state = wire::SnapshotValue::Bytes(vec![0xff]);
        assert!(SettingsView::restore(&snapshot.encode().unwrap()).is_err());
    }

    #[test]
    fn pane_changes_keep_the_same_password_draft() {
        let (mut view, _) = SettingsView::boot();
        drop(view.update(Message::EditSettingsPassword("draft".into())));
        for pane in [
            SettingsPane::Security,
            SettingsPane::General,
            SettingsPane::Security,
        ] {
            drop(view.update(Message::PickSettingsPane(pane)));
            assert_eq!(view.settings_pane, pane);
            assert_eq!(view.key_password, "draft");
        }
    }
    #[test]
    fn view_fits_default_stack() {
        ::std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(|| {
                let (app, _) = SettingsView::boot();
                let _ = app.view();
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
impl SettingsView {
    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SessionArrived(item) => self.on_session_arrived(item),
            Message::StandingArrived(item) => self.on_standing_arrived(item),
            Message::KeysArrived(item) => self.on_keys_arrived(item),
            Message::ShowTab(tab) => self.on_show_tab(tab),
            Message::Reconnect => self.on_reconnect(),
            Message::SwitchNetwork => self.on_switch_network(),
            Message::SettingsUnlockSubmit(pw) => self.on_settings_unlock_submit(pw),
            Message::LockSession => self.on_lock_session(),
            Message::AccountRenameSubmit => self.on_account_rename_submit(),
            Message::AccountCreateSubmit => self.on_account_create_submit(),
            Message::AccountKeyAddSubmit => self.on_account_key_add_submit(),
            Message::AccountKeyJoinSubmit => self.on_account_key_join_submit(),
            Message::AccountKeyRemove(pubkey) => self.on_account_key_remove(pubkey),
            Message::AccountPasskeySubmit => self.on_account_passkey_submit(),
            Message::AccountPasskeyDesktop => self.on_account_passkey_desktop(),
            Message::AccountCeremonyCancel => self.on_account_ceremony_cancel(),
            Message::AccountWalletSubmit => self.on_account_wallet_submit(),
            Message::AccountLoginSubmit => self.on_account_login_submit(),
            Message::CopyToClipboard(text, label) => self.on_copy_to_clipboard(text, label),
            Message::SetAppearanceLight => self.on_set_appearance_light(),
            Message::SetAppearanceDark => self.on_set_appearance_dark(),
            Message::SetDesktopNotifications(enabled) => self.on_set_desktop_notifications(enabled),
            Message::PickSettingsPane(picked) => self.on_pick_settings_pane(picked),
            Message::EditSettingsPassword(value) => self.on_edit_settings_password(value),
            Message::BindAccountNameDraft(value) => self.on_bind_account_name_draft(value),
            Message::BindAccountCreateDraft(value) => self.on_bind_account_create_draft(value),
            Message::BindAccountJoinDraft(value) => self.on_bind_account_join_draft(value),
            Message::BindAccountKeyDraft(value) => self.on_bind_account_key_draft(value),
            Message::BindAccountKeyLabelDraft(value) => self.on_bind_account_key_label_draft(value),
        }
    }
    fn on_session_arrived(&mut self, item: Box<crate::host::SessionItem>) -> Task<Message> {
        self.host_error = item.error.to_owned();
        if !(item.error).is_empty() {
            return Task::none();
        }
        let next = item.next.clone();
        self.connection_serial = crate::host::connection_serial_after(
            self.connected,
            next.connected,
            self.connection_serial,
        );
        self.connected = next.connected;
        self.dark = next.dark;
        self.loading = next.loading;
        self.status = next.status.to_owned();
        self.busy = next.busy;
        self.recovering = next.recovering;
        self.appearance = next.appearance.to_owned();
        self.desktop_notifications = next.desktop_notifications;
        self.unlocked = next.unlocked;
        self.seat_key = next.seat_key.to_owned();
        self.account_name = next.account_name.to_owned();
        self.network_name = next.network_name.to_owned();
        self.connected_rpc = next.connected_rpc.to_owned();
        self.account_ceremony_phase = next.account_ceremony_phase.to_owned();
        self.account_ceremony_qr = next.account_ceremony_qr.to_owned();
        self.account_ceremony_detail = next.account_ceremony_detail.to_owned();
        self.account_ceremony_left = next.account_ceremony_left.to_owned();
        self.settings_key_state = next.settings_key_state.to_owned();
        self.settings_key_path = next.settings_key_path.to_owned();
        self.account_busy = next.account_busy;
        self.account_ticket = next.account_ticket.to_owned();
        let renamed = crate::host::renamed_to(&(next.account_name), &(self.renaming_to));
        self.renaming_to = crate::host::keep_draft(renamed, &(self.renaming_to));
        self.account_name_draft = crate::host::keep_draft(renamed, &(self.account_name_draft));
        let founded = next.account_exists && (!self.account_exists);
        self.account_exists = next.account_exists;
        self.account_number = next.account_number.to_owned();
        self.account_create_draft = crate::host::keep_draft(founded, &(self.account_create_draft));
        self.account_join_draft = crate::host::keep_draft(founded, &(self.account_join_draft));
        let minted = !(next.account_ticket).is_empty();
        self.account_key_draft = crate::host::keep_draft(minted, &(self.account_key_draft));
        self.account_key_label_draft =
            crate::host::keep_draft(minted, &(self.account_key_label_draft));
        Task::none()
    }
    fn on_standing_arrived(&mut self, item: crate::host::StandingItem) -> Task<Message> {
        self.host_error = item.error.to_owned();
        self.members_answered = item.answered;
        if !(item.error).is_empty() {
            return Task::none();
        }
        self.tier = item.next.tier.to_owned();
        self.admin = item.next.admin;
        self.members_line = item.next.members_line.to_owned();
        Task::none()
    }
    fn on_keys_arrived(&mut self, item: crate::host::KeysItem) -> Task<Message> {
        self.host_error = item.error.to_owned();
        if !(item.error).is_empty() {
            return Task::none();
        }
        self.account_key_rows = item.rows.clone();
        Task::none()
    }
    fn on_show_tab(&mut self, tab: String) -> Task<Message> {
        crate::host::open_tab(&(tab));
        Task::none()
    }
    fn on_reconnect(&mut self) -> Task<Message> {
        crate::host::reconnect_network();
        Task::none()
    }
    fn on_switch_network(&mut self) -> Task<Message> {
        crate::host::switch_workspace();
        Task::none()
    }
    fn on_settings_unlock_submit(&mut self, pw: String) -> Task<Message> {
        if self.busy || (pw).is_empty() {
            return Task::none();
        }
        crate::host::unlock(&(pw));
        Task::none()
    }
    fn on_lock_session(&mut self) -> Task<Message> {
        crate::host::lock();
        Task::none()
    }
    fn on_account_rename_submit(&mut self) -> Task<Message> {
        if !self.connected {
            return Task::none();
        }
        if self.account_busy || ((self.account_name_draft).trim().to_owned()).is_empty() {
            return Task::none();
        }
        self.renaming_to = (self.account_name_draft).trim().to_owned();
        crate::host::rename_account((self.account_name_draft).trim());
        Task::none()
    }
    fn on_account_create_submit(&mut self) -> Task<Message> {
        if !self.connected {
            return Task::none();
        }
        if (self.account_busy || (!self.unlocked))
            || ((self.account_create_draft).trim().to_owned()).is_empty()
        {
            return Task::none();
        }
        crate::host::create_account((self.account_create_draft).trim());
        Task::none()
    }
    fn on_account_key_add_submit(&mut self) -> Task<Message> {
        if !self.connected {
            return Task::none();
        }
        if (self.account_busy || (!self.unlocked))
            || ((self.account_key_draft).trim().to_owned()).is_empty()
        {
            return Task::none();
        }
        crate::host::mint_ticket(
            (self.account_key_draft).trim(),
            (self.account_key_label_draft).trim(),
        );
        Task::none()
    }
    fn on_account_key_join_submit(&mut self) -> Task<Message> {
        if !self.connected {
            return Task::none();
        }
        if (self.account_busy || (!self.unlocked))
            || ((self.account_join_draft).trim().to_owned()).is_empty()
        {
            return Task::none();
        }
        crate::host::join_account((self.account_join_draft).trim());
        Task::none()
    }
    fn on_account_key_remove(&mut self, pubkey: String) -> Task<Message> {
        if !self.connected {
            return Task::none();
        }
        if self.account_busy || !self.unlocked || self.account_key_rows.len() <= 1 {
            return Task::none();
        }
        crate::host::remove_key(&(pubkey));
        Task::none()
    }
    fn on_account_passkey_submit(&mut self) -> Task<Message> {
        if !self.connected {
            return Task::none();
        }
        crate::host::add_passkey((self.account_key_label_draft).trim());
        Task::none()
    }
    fn on_account_passkey_desktop(&mut self) -> Task<Message> {
        if !self.connected {
            return Task::none();
        }
        crate::host::add_passkey_here((self.account_key_label_draft).trim());
        Task::none()
    }
    fn on_account_ceremony_cancel(&mut self) -> Task<Message> {
        crate::host::cancel_ceremony();
        Task::none()
    }
    fn on_account_wallet_submit(&mut self) -> Task<Message> {
        if !self.connected {
            return Task::none();
        }
        crate::host::link_wallet((self.account_key_label_draft).trim());
        Task::none()
    }
    fn on_account_login_submit(&mut self) -> Task<Message> {
        if !self.connected {
            return Task::none();
        }
        crate::host::login();
        Task::none()
    }
    fn on_copy_to_clipboard(&mut self, text: String, label: String) -> Task<Message> {
        crate::host::copy(&(text), &(label));
        Task::none()
    }
    fn on_set_appearance_light(&mut self) -> Task<Message> {
        crate::host::set_light();
        Task::none()
    }
    fn on_set_appearance_dark(&mut self) -> Task<Message> {
        crate::host::set_dark();
        Task::none()
    }
    fn on_set_desktop_notifications(&mut self, enabled: bool) -> Task<Message> {
        crate::host::set_notifications(enabled);
        Task::none()
    }
    fn on_pick_settings_pane(&mut self, picked: SettingsPane) -> Task<Message> {
        self.settings_pane = picked;
        Task::none()
    }
    fn on_edit_settings_password(&mut self, value: String) -> Task<Message> {
        self.key_password = value;
        Task::none()
    }
    fn on_bind_account_name_draft(&mut self, value: String) -> Task<Message> {
        self.account_name_draft = value;
        Task::none()
    }
    fn on_bind_account_create_draft(&mut self, value: String) -> Task<Message> {
        self.account_create_draft = value;
        Task::none()
    }
    fn on_bind_account_join_draft(&mut self, value: String) -> Task<Message> {
        self.account_join_draft = value;
        Task::none()
    }
    fn on_bind_account_key_draft(&mut self, value: String) -> Task<Message> {
        self.account_key_draft = value;
        Task::none()
    }
    fn on_bind_account_key_label_draft(&mut self, value: String) -> Task<Message> {
        self.account_key_label_draft = value;
        Task::none()
    }
}
fn settings_action(
    key: impl Into<String>,
    label: &str,
    message: Message,
    enabled: bool,
) -> wire::Node {
    use ducktape_view_guest::{kit, slots, wire};
    kit::button(
        key,
        label,
        enabled.then(|| slots::message(message)),
        wire::ButtonPreset::Secondary,
    )
}
fn settings_primary(
    key: impl Into<String>,
    label: &str,
    message: Message,
    enabled: bool,
) -> wire::Node {
    use ducktape_view_guest::{kit, slots, wire};
    kit::button(
        key,
        label,
        enabled.then(|| slots::message(message)),
        wire::ButtonPreset::Primary,
    )
}
fn settings_subtle(
    key: impl Into<String>,
    label: &str,
    message: Message,
    enabled: bool,
) -> wire::Node {
    use ducktape_view_guest::{kit, slots, wire};
    kit::button(
        key,
        label,
        enabled.then(|| slots::message(message)),
        wire::ButtonPreset::Subtle,
    )
}
/// Every settings field is the same width: the reading stays a column of
/// labels on the left and controls on the right, whatever the value is.
const FIELD_WIDTH: f32 = 280.;
/// The band a settings row takes above and below its content.
const ROW_BAND: f32 = 8.;
fn settings_input(
    key: &str,
    placeholder: &str,
    value: &str,
    route: fn(String) -> Message,
) -> wire::Node {
    use ducktape_view_guest::{kit, slots};
    kit::sized(
        kit::input(
            key,
            placeholder,
            value,
            slots::handler::<String, Message>(Box::new(move |text| Some(route(text)))),
            None,
        ),
        Some(wire::Length::Fixed(FIELD_WIDTH)),
        None,
    )
}
/// A choice between a few values: the chosen one is checked.
fn settings_choice(
    key: &str,
    choices: impl IntoIterator<Item = (String, &'static str, bool, Message)>,
) -> wire::Node {
    use ducktape_view_guest::{kit, slots};
    kit::tabs(
        key,
        choices.into_iter().map(|(id, label, chosen, message)| {
            (id, label.to_owned(), chosen, Some(slots::message(message)))
        }),
    )
}
/// One setting on its own row: what it is and why on the left, the control
/// that changes it on the right.
fn setting_row(key: &str, name: &str, detail: &str, note: &str, control: wire::Node) -> wire::Node {
    use ducktape_view_guest::kit;
    let mut lines = vec![kit::strong(format!("{key}/name"), name)];
    if !detail.is_empty() {
        lines.push(kit::wrapping(kit::secondary(
            format!("{key}/detail"),
            detail,
        )));
    }
    if !note.is_empty() {
        lines.push(kit::wrapping(kit::caption(format!("{key}/note"), note)));
    }
    kit::centered_row(
        key,
        [
            kit::sized(
                kit::spaced(kit::column(format!("{key}/text"), lines), 2.),
                Some(wire::Length::Fill),
                None,
            ),
            kit::sized(control, Some(wire::Length::Shrink), None),
        ],
    )
}
/// A run of settings rows: a hairline between them, every one in the same
/// band, so the tab reads as one list and not as a stack of cards.
fn setting_list(key: &str, rows: impl IntoIterator<Item = wire::Node>) -> wire::Node {
    use ducktape_view_guest::kit;
    let band = wire::Edges {
        top: ROW_BAND,
        right: 0.,
        bottom: ROW_BAND,
        left: 0.,
    };
    let mut children = Vec::new();
    for (index, row) in rows.into_iter().enumerate() {
        if index > 0 {
            children.push(kit::divider(format!("{key}/rule/{index}")));
        }
        children.push(kit::padded(row, band));
    }
    kit::spaced(kit::column(key, children), 0.)
}
/// A group inside a tab: its heading, a line about it, then its body.
fn settings_section(
    key: &str,
    title_key: &str,
    title: &str,
    help: &str,
    body: wire::Node,
) -> wire::Node {
    use ducktape_view_guest::kit;
    let mut head = vec![kit::heading(title_key, title)];
    if !help.is_empty() {
        head.push(kit::wrapping(kit::secondary(format!("{key}/help"), help)));
    }
    kit::spaced(
        kit::column(
            key,
            [
                kit::spaced(kit::column(format!("{key}/head"), head), 2.),
                body,
            ],
        ),
        6.,
    )
}
impl SettingsView {
    pub(crate) fn view(&self) -> wire::Node {
        use ducktape_view_guest::{kit, slots, wire};
        kit::set_dark(self.dark);
        let tabs = [
            ("general", "General", SettingsPane::General),
            ("network", "Network", SettingsPane::Network),
            ("account", "Account", SettingsPane::Account),
            ("security", "Security", SettingsPane::Security),
        ];
        let mut content = vec![
            kit::title("settings/title", "Settings"),
            kit::spaced(
                kit::column(
                    "settings/tabs",
                    [
                        kit::sized(
                            kit::tabs(
                                "settings/tab",
                                tabs.into_iter().map(|(key, label, pane)| {
                                    (
                                        key.to_owned(),
                                        label.to_owned(),
                                        self.settings_pane == pane,
                                        Some(slots::message(Message::PickSettingsPane(pane))),
                                    )
                                }),
                            ),
                            Some(wire::Length::Fill),
                            Some(wire::Length::Fixed(28.)),
                        ),
                        kit::divider("settings/tab-rule"),
                    ],
                ),
                6.,
            ),
        ];
        if !self.host_error.is_empty() {
            content.push(kit::notice(
                "settings/error",
                kit::wrapping(kit::text("settings/error-text", &self.host_error)),
                kit::Tone::Danger,
            ));
        }
        content.push(match self.settings_pane {
            SettingsPane::General => self.general_settings(),
            SettingsPane::Network => self.network_settings(),
            SettingsPane::Account => self.account_settings(),
            SettingsPane::Security => self.security_settings(),
        });
        let mut page = kit::page("settings/content", content);
        if let wire::Node::Linear {
            max_width, height, ..
        } = &mut page
        {
            *max_width = Some(760.);
            *height = None;
        }
        kit::scroll("settings", page)
    }
    fn general_settings(&self) -> wire::Node {
        let appearance = settings_choice(
            "settings/appearance",
            [
                (
                    "light".to_owned(),
                    "Light",
                    self.appearance == "light",
                    Message::SetAppearanceLight,
                ),
                (
                    "dark".to_owned(),
                    "Dark",
                    self.appearance == "dark",
                    Message::SetAppearanceDark,
                ),
            ],
        );
        let notifications = settings_choice(
            "settings/notifications",
            [
                (
                    "true".to_owned(),
                    "On",
                    self.desktop_notifications,
                    Message::SetDesktopNotifications(true),
                ),
                (
                    "false".to_owned(),
                    "Off",
                    !self.desktop_notifications,
                    Message::SetDesktopNotifications(false),
                ),
            ],
        );
        let source = if self.appearance == "system" {
            "Following the system appearance."
        } else {
            "Pinned for this device."
        };
        let banner = if self.desktop_notifications {
            "A desktop banner when you are named, or written to directly."
        } else {
            "Silent — the bell is the only notice."
        };
        setting_list(
            "settings/general",
            [
                setting_row(
                    "settings/theme",
                    "Appearance",
                    "How this device draws the app.",
                    source,
                    appearance,
                ),
                setting_row(
                    "settings/notification",
                    "Mentions and direct messages",
                    banner,
                    "",
                    notifications,
                ),
            ],
        )
    }
    fn network_settings(&self) -> wire::Node {
        use ducktape_view_guest::kit;
        let actions = kit::row(
            "settings/network-actions",
            [
                settings_action(
                    "settings/reconnect",
                    "Reconnect",
                    Message::Reconnect,
                    !self.loading && (!self.busy || self.recovering),
                ),
                settings_subtle(
                    "settings/switch",
                    "Switch network",
                    Message::SwitchNetwork,
                    !self.busy,
                ),
            ],
        );
        if !self.connected {
            return kit::column(
                "settings/disconnected-network",
                [
                    kit::empty_state(
                        "settings/disconnected",
                        "Not connected",
                        "Reconnect to this network, or open another one.",
                    ),
                    actions,
                ],
            );
        }
        settings_section(
            "settings/network",
            "settings/network-name",
            &self.network_name,
            "",
            kit::spaced(
                kit::column(
                    "settings/network-body",
                    [
                        setting_list(
                            "settings/network-rows",
                            [
                                kit::kv(
                                    "settings/network-status-row",
                                    "Status",
                                    kit::text("settings/network-status", &self.status),
                                ),
                                kit::kv(
                                    "settings/network-rpc-row",
                                    "Endpoint",
                                    kit::centered_row(
                                        "settings/network-rpc-value",
                                        [
                                            kit::wrapping(kit::mono(
                                                "settings/network-rpc",
                                                &self.connected_rpc,
                                            )),
                                            settings_subtle(
                                                "settings/copy-rpc",
                                                "Copy endpoint",
                                                Message::CopyToClipboard(
                                                    self.connected_rpc.clone(),
                                                    "Endpoint copied".into(),
                                                ),
                                                !self.connected_rpc.is_empty(),
                                            ),
                                        ],
                                    ),
                                ),
                                kit::kv(
                                    "settings/members",
                                    "Members",
                                    kit::centered_row(
                                        "settings/members-row",
                                        [
                                            kit::nowrap(kit::text(
                                                "settings/members-count",
                                                &self.members_line,
                                            )),
                                            settings_subtle(
                                                "settings/manage-members",
                                                "manage",
                                                Message::ShowTab("members".into()),
                                                true,
                                            ),
                                        ],
                                    ),
                                ),
                                kit::kv(
                                    "settings/node",
                                    "Node",
                                    kit::centered_row(
                                        "settings/node-row",
                                        [settings_subtle(
                                            "settings/view-node",
                                            "view",
                                            Message::ShowTab("node".into()),
                                            true,
                                        )],
                                    ),
                                ),
                            ],
                        ),
                        actions,
                    ],
                ),
                12.,
            ),
        )
    }
    fn account_settings(&self) -> wire::Node {
        use ducktape_view_guest::{kit, wire};
        if !self.connected {
            return kit::empty_state(
                "settings/disconnected-account",
                "Not connected",
                "Reconnect to read or change your account on this network.",
            );
        }
        let available = !self.account_busy && self.unlocked;
        let standing = if self.tier.is_empty() && self.members_answered {
            "standing unknown".to_owned()
        } else {
            self.tier.clone()
        };
        let mut identity = vec![
            kit::kv(
                "settings/account-name-row",
                "Name",
                kit::text(
                    "settings/account-name",
                    if self.account_name.is_empty() {
                        "(unnamed)"
                    } else {
                        &self.account_name
                    },
                ),
            ),
            kit::kv(
                "settings/standing-row",
                "Standing",
                kit::badge("settings/standing", standing, kit::Tone::Neutral),
            ),
            kit::kv(
                "settings/account-number-row",
                "Number",
                kit::centered_row(
                    "settings/number-row",
                    [
                        kit::mono("settings/account-number", &self.account_number),
                        settings_subtle(
                            "settings/copy-number",
                            "Copy number",
                            Message::CopyToClipboard(
                                self.account_number.clone(),
                                "Number copied".into(),
                            ),
                            !self.account_number.is_empty(),
                        ),
                    ],
                ),
            ),
            kit::kv(
                "settings/seat-row",
                "Key on this device",
                kit::wrapping(kit::mono("settings/seat", &self.seat_key)),
            ),
        ];
        if self.account_exists {
            identity.push(setting_row(
                "settings/rename-row",
                "Account name",
                "What this network calls you; every device you add answers to it.",
                "",
                kit::row(
                    "settings/rename-controls",
                    [
                        settings_input(
                            "settings/rename-draft",
                            "rename account…",
                            &self.account_name_draft,
                            Message::BindAccountNameDraft,
                        ),
                        settings_primary(
                            "settings/rename",
                            "Rename",
                            Message::AccountRenameSubmit,
                            !self.account_busy && !self.account_name_draft.trim().is_empty(),
                        ),
                    ],
                ),
            ));
        }
        let identity = setting_list("settings/identity-rows", identity);
        let mut content = Vec::new();
        if !self.account_exists {
            content.push(settings_section(
                "settings/identity",
                "settings/identity-title",
                "Your identity",
                "This device holds a key but no account yet.",
                identity,
            ));
            content.push(settings_section(
                "settings/enrol",
                "settings/enrol-title",
                "Create or join an account",
                "An account is what the network knows you as; every device you add signs for it.",
                setting_list(
                    "settings/enrol-rows",
                    [
                        setting_row(
                            "settings/create-row",
                            "New account",
                            "This device founds it and holds its first key.",
                            "",
                            kit::row(
                                "settings/create-controls",
                                [
                                    settings_input(
                                        "settings/create-draft",
                                        "name your account…",
                                        &self.account_create_draft,
                                        Message::BindAccountCreateDraft,
                                    ),
                                    settings_primary(
                                        "settings/create",
                                        "Create account",
                                        Message::AccountCreateSubmit,
                                        available && !self.account_create_draft.trim().is_empty(),
                                    ),
                                ],
                            ),
                        ),
                        setting_row(
                            "settings/join-row",
                            "Join with a ticket",
                            "A device that already signs for the account mints it.",
                            "",
                            kit::row(
                                "settings/join-controls",
                                [
                                    settings_input(
                                        "settings/join-draft",
                                        "paste a ticket from a member device…",
                                        &self.account_join_draft,
                                        Message::BindAccountJoinDraft,
                                    ),
                                    settings_action(
                                        "settings/join",
                                        "Join",
                                        Message::AccountKeyJoinSubmit,
                                        available && !self.account_join_draft.trim().is_empty(),
                                    ),
                                ],
                            ),
                        ),
                        setting_row(
                            "settings/login-row",
                            "Passkey",
                            "Or let a passkey from another device admit this one.",
                            "",
                            settings_action(
                                "settings/login",
                                "Log in with a passkey",
                                Message::AccountLoginSubmit,
                                available,
                            ),
                        ),
                    ],
                ),
            ));
        } else {
            content.push(settings_section(
                "settings/identity",
                "settings/identity-title",
                "Your identity",
                "",
                identity,
            ));
            let mut keys = Vec::new();
            for row in &self.account_key_rows {
                let key = format!("settings/key/{}/{}", row.scheme, row.pubkey);
                keys.push(kit::centered_row(
                    &key,
                    [
                        kit::sized(
                            kit::spaced(
                                kit::column(
                                    format!("{key}/text"),
                                    [
                                        kit::centered_row(
                                            format!("{key}/head"),
                                            [
                                                kit::nowrap(kit::strong(
                                                    format!("{key}/label"),
                                                    if row.label.is_empty() {
                                                        "(unlabeled)"
                                                    } else {
                                                        &row.label
                                                    },
                                                )),
                                                kit::badge(
                                                    format!("{key}/scheme"),
                                                    &row.scheme,
                                                    kit::Tone::Neutral,
                                                ),
                                            ],
                                        ),
                                        kit::wrapping(kit::colored(
                                            kit::mono(format!("{key}/value"), &row.pubkey),
                                            kit::palette().muted,
                                        )),
                                    ],
                                ),
                                2.,
                            ),
                            Some(wire::Length::Fill),
                            None,
                        ),
                        settings_subtle(
                            format!("{key}/remove"),
                            "Remove",
                            Message::AccountKeyRemove(row.pubkey.clone()),
                            available && self.account_key_rows.len() > 1,
                        ),
                    ],
                ));
            }
            content.push(settings_section(
                "settings/keys",
                "settings/keys-title",
                "Account keys",
                &format!(
                    "{} — each one signs for this account.",
                    kit_plural(self.account_key_rows.len(), "key", "keys")
                ),
                setting_list("settings/keys-rows", keys),
            ));
            let mut add = vec![
                kit::field(
                    "settings/key-field",
                    "Its public key",
                    settings_input(
                        "settings/key-draft",
                        "paste its ed25519 key (hex)…",
                        &self.account_key_draft,
                        Message::BindAccountKeyDraft,
                    ),
                ),
                kit::field(
                    "settings/key-label-field",
                    "Label",
                    kit::row(
                        "settings/key-label-row",
                        [
                            settings_input(
                                "settings/key-label-draft",
                                "label…",
                                &self.account_key_label_draft,
                                Message::BindAccountKeyLabelDraft,
                            ),
                            settings_primary(
                                "settings/mint",
                                "Mint ticket",
                                Message::AccountKeyAddSubmit,
                                available && !self.account_key_draft.trim().is_empty(),
                            ),
                        ],
                    ),
                ),
            ];
            if !self.account_ticket.is_empty() {
                add.push(kit::notice(
                    "settings/ticket",
                    kit::centered_row(
                        "settings/ticket-row",
                        [
                            kit::wrapping(kit::text(
                                "settings/ticket-help",
                                "Ticket minted — paste it on the other device.",
                            )),
                            settings_action(
                                "settings/copy-ticket",
                                "Copy ticket",
                                Message::CopyToClipboard(
                                    self.account_ticket.clone(),
                                    "Ticket copied".into(),
                                ),
                                true,
                            ),
                        ],
                    ),
                    kit::Tone::Success,
                ));
            }
            add.push(kit::divider("settings/add-rule"));
            add.push(kit::label("settings/passkey-help", "Or a passkey"));
            add.push(kit::wrapped_row(
                "settings/passkey-row",
                [
                    settings_action(
                        "settings/passkey-phone",
                        "On your phone",
                        Message::AccountPasskeySubmit,
                        available,
                    ),
                    settings_action(
                        "settings/passkey-browser",
                        "In this browser",
                        Message::AccountPasskeyDesktop,
                        available,
                    ),
                    settings_action(
                        "settings/wallet",
                        "Link a wallet",
                        Message::AccountWalletSubmit,
                        available,
                    ),
                ],
            ));
            content.push(settings_section(
                "settings/add",
                "settings/add-device",
                "Add a device",
                "A ticket admits one more key to this account.",
                kit::spaced(kit::column("settings/add-body", add), 12.),
            ));
        }
        match self.account_ceremony_phase.as_str() {
            "show_qr" => {
                content.push(kit::card(
                    "settings/ceremony",
                    kit::column(
                        "settings/ceremony-body",
                        [
                            wire::Node::Qr {
                                key: "settings/ceremony-qr".into(),
                                code: wire::Qr {
                                    payload: Some(self.account_ceremony_qr.as_bytes().to_vec()),
                                    correction: Some(wire::QrCorrection::Medium),
                                    size: Some(wire::QrSize::Cell(3.)),
                                    version: None,
                                    cell: None,
                                    background: None,
                                },
                            },
                            kit::wrapping(kit::text(
                                "settings/ceremony-detail",
                                &self.account_ceremony_detail,
                            )),
                            kit::caption("settings/ceremony-left", &self.account_ceremony_left),
                            settings_subtle(
                                "settings/ceremony-cancel",
                                "Cancel",
                                Message::AccountCeremonyCancel,
                                true,
                            ),
                        ],
                    ),
                ));
            }
            "working" => {
                content.push(kit::notice(
                    "settings/ceremony",
                    kit::centered_row(
                        "settings/ceremony-row",
                        [
                            kit::wrapping(kit::text(
                                "settings/ceremony-detail",
                                &self.account_ceremony_detail,
                            )),
                            settings_subtle(
                                "settings/ceremony-cancel",
                                "Cancel",
                                Message::AccountCeremonyCancel,
                                true,
                            ),
                        ],
                    ),
                    kit::Tone::Accent,
                ));
            }
            _ => {}
        }
        kit::spaced(kit::column("settings/account", content), 20.)
    }
    fn security_settings(&self) -> wire::Node {
        use ducktape_view_guest::{kit, slots, wire};
        let state = if self.settings_key_state.is_empty() {
            "unknown"
        } else {
            &self.settings_key_state
        };
        let tone = match self.settings_key_state.as_str() {
            "encrypted" => kit::Tone::Success,
            "absent" => kit::Tone::Warning,
            "unreadable" => kit::Tone::Danger,
            _ => kit::Tone::Neutral,
        };
        let mut content = vec![
            kit::kv(
                "settings/key-state-row",
                "Key state",
                kit::badge("settings/key-state", state, tone),
            ),
            kit::kv(
                "settings/key-path-row",
                "Key path",
                kit::wrapping(kit::mono("settings/key-path", &self.settings_key_path)),
            ),
        ];
        if self.unlocked {
            content.push(kit::centered_row(
                "settings/signing-row",
                [
                    kit::sized(
                        kit::spaced(
                            kit::column(
                                "settings/signing-text",
                                [
                                    kit::strong("settings/signing-name", "Signing seat"),
                                    kit::wrapping(kit::tone_text(
                                        "settings/signing",
                                        "Signing unlocked for this session.",
                                        kit::Tone::Success,
                                    )),
                                ],
                            ),
                            2.,
                        ),
                        Some(wire::Length::Fill),
                        None,
                    ),
                    settings_action("settings/lock", "Lock", Message::LockSession, true),
                ],
            ));
        } else {
            let mut password = kit::input(
                "settings/password",
                "unlock signing…",
                &self.key_password,
                slots::handler::<String, Message>(Box::new(|text| {
                    Some(Message::EditSettingsPassword(text))
                })),
                Some(slots::message(Message::SettingsUnlockSubmit(
                    self.key_password.clone(),
                ))),
            );
            if let wire::Node::Input { secure, .. } = &mut password {
                *secure = true;
            }
            content.push(setting_row(
                "settings/unlock-row",
                "Wallet password",
                "Unlock this device's key for this session.",
                "",
                kit::row(
                    "settings/unlock-controls",
                    [
                        kit::sized(password, Some(wire::Length::Fixed(FIELD_WIDTH)), None),
                        settings_action(
                            "settings/unlock",
                            "Unlock",
                            Message::SettingsUnlockSubmit(self.key_password.clone()),
                            !self.busy && !self.key_password.is_empty(),
                        ),
                    ],
                ),
            ));
        }
        settings_section(
            "settings/security",
            "settings/security-title",
            "Identity key",
            "The key this device signs with, and whether it is unlocked now.",
            setting_list("settings/security-rows", content),
        )
    }
}
fn kit_plural(count: usize, one: &str, many: &str) -> String {
    match count {
        1 => format!("1 {one}"),
        n => format!("{n} {many}"),
    }
}
ducktape_view_guest::export_app!(
    SettingsView,
    "Settings",
    "This device's preferences, the account this key speaks for, and the workspace's lifecycle.",
    ["settings"]
);
