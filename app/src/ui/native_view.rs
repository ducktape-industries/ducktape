use super::*;

impl Ducktape {
    /// The seated view's props and the route its events take home.
    ///
    /// ONE dispatch, on the tab's ID. Every arm below is a view that still
    /// needs the app to hand it facts it does not read itself yet; the
    /// fallthrough is the SESSION block — who is connected, to what, as whom —
    /// which is all a view needs once its own reads live in the guest
    /// (`#2303`). So an id this build has never heard of, listed by the
    /// connected node's registry, seats and routes with no app change, and a
    /// view that finishes its migration loses its arm here rather than
    /// gaining one.
    pub(crate) fn native_view(
        &self,
    ) -> (
        crate::module_view::ViewSpec,
        fn(crate::module_view::ModuleViewEvent) -> AppMessage,
    ) {
        let ShellTab::View(tab) = self.shell_tab;
        match tab {
            "chat" => (
                crate::module_view::chat_view(
                    self.is_dark(),
                    self.connected,
                    &self.connected_rpc,
                    &self.network_name,
                    &self.network_chain_id,
                    &self.status,
                    self.block_height,
                    &self.account_number,
                    &self.settings_user_key,
                    self.dm_peers_generation,
                    &self.active_channel,
                    &self.chat_dm_peer,
                    self.chat_dm_serial,
                    self.chat_land_seq,
                    self.mutation_phase,
                    self.loading,
                    self.huddle_joined,
                    &self.huddle_channel,
                    &self.huddle_channel_name,
                    self.huddle_joined_at,
                    self.huddle_now,
                    self.call_muted,
                    self.call_speaking,
                    &self.call_peers,
                    self.shift_held,
                    self.chat_copy_chord_serial,
                ),
                AppMessage::ChatViewEvent,
            ),
            "pages" => (
                crate::module_view::pages_view(
                    self.is_dark(),
                    self.connected,
                    &self.network_chain_id,
                    &self.page_route,
                    self.page_route_serial,
                ),
                AppMessage::PagesViewEvent,
            ),
            "forge" => (
                crate::module_view::forge_view(
                    self.is_dark(),
                    self.connected,
                    &self.network_name,
                    &self.account_bio,
                    &self.network_chain_id,
                    &self.connected_rpc,
                    &self.forge_link,
                    self.forge_link_tick,
                ),
                AppMessage::ForgeViewEvent,
            ),
            "agents" => (
                crate::module_view::agents_view(
                    self.is_dark(),
                    self.connected,
                    &self.account_number,
                    &self.agents_open_run,
                    self.agents_opened,
                ),
                AppMessage::AgentsViewEvent,
            ),
            "files" => (
                crate::module_view::files_view(
                    self.is_dark(),
                    self.connected,
                    &self.network_chain_id,
                    &self.account_number,
                    &self.fs_route,
                    self.fs_route_serial,
                ),
                AppMessage::FilesViewEvent,
            ),
            "explorer" => (
                crate::module_view::explorer_view(
                    self.is_dark(),
                    self.connected,
                    self.block_height,
                    &crate::backend::sync_label(
                        &self.node_phase,
                        self.node_sync_applied,
                        self.node_sync_target,
                    ),
                ),
                AppMessage::ExplorerViewEvent,
            ),
            "node" => (
                crate::module_view::node_view(
                    self.is_dark(),
                    self.connected,
                    &self.status,
                    &self.node_data_dir,
                    self.wall_now,
                ),
                AppMessage::NodeViewEvent,
            ),
            "members" => (
                crate::module_view::members_view(self.is_dark(), self.connected),
                AppMessage::MembersViewEvent,
            ),
            "governance" => (
                crate::module_view::governance_view(self.is_dark(), self.connected),
                AppMessage::GovernanceViewEvent,
            ),
            "settings" => (
                crate::module_view::settings_view(
                    self.is_dark(),
                    self.connected,
                    self.loading,
                    &self.status,
                    self.mutation_phase,
                    self.appearance,
                    self.desktop_notifications,
                    crate::backend::desktop_notifications_host().token(),
                    &self.password,
                    &self.settings_user_key,
                    &self.account_name,
                    &self.network_name,
                    &self.connected_rpc,
                    &self.account_ceremony_phase,
                    &self.account_ceremony_qr,
                    &self.account_ceremony_detail,
                    &self.account_ceremony_left,
                    &self.settings_key_state,
                    &self.settings_key_path,
                    &self.account_number,
                    self.account_exists,
                    self.account_busy,
                    &self.account_ticket,
                    &self.update_facts(),
                ),
                AppMessage::SettingsViewEvent,
            ),
            // the session block: every fact that is true of the connection
            // rather than of one view. An id with no arm above — a view the
            // registry lists and this build never heard of — lands here.
            module => (
                crate::module_view::registered_view(
                    module,
                    self.is_dark(),
                    self.connected,
                    &self.network_chain_id,
                    &self.account_number,
                ),
                AppMessage::RegisteredViewEvent,
            ),
        }
    }
}
