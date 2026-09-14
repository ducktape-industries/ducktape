use super::*;
impl Ducktape {
    pub(crate) fn update(&mut self, message: AppMessage) -> Task<AppMessage> {
        match message {
            AppMessage::AppearanceSaveReply(request_generation, reply_message) => {
                self.on_appearance_save_reply(request_generation, reply_message)
            }
            AppMessage::ConnectionReply(request_generation, reply_message) => {
                self.on_connection_reply(request_generation, reply_message)
            }
            AppMessage::DmPeersLoadReply(request_generation, reply_message) => {
                self.on_dm_peers_load_reply(request_generation, reply_message)
            }
            AppMessage::NodeFactsLoadReply(request_generation, reply_message) => {
                self.on_node_facts_load_reply(request_generation, reply_message)
            }
            AppMessage::BellLoadReply(request_generation, reply_message) => {
                self.on_bell_load_reply(request_generation, reply_message)
            }
            AppMessage::MembersLoadReply(request_generation, reply_message) => {
                self.on_members_load_reply(request_generation, reply_message)
            }
            AppMessage::SettingsLoadReply(request_generation, reply_message) => {
                self.on_settings_load_reply(request_generation, reply_message)
            }
            AppMessage::AccountLoadReply(request_generation, reply_message) => {
                self.on_account_load_reply(request_generation, reply_message)
            }
            AppMessage::LiveResyncReply(request_generation, reply_message) => {
                self.on_live_resync_reply(request_generation, reply_message)
            }
            AppMessage::BellPresentationsReply(request_generation, reply_message) => {
                self.on_bell_presentations_reply(request_generation, reply_message)
            }
            AppMessage::AccountQrAuthReply(request_generation, reply_message) => {
                self.on_account_qr_auth_reply(request_generation, reply_message)
            }
            AppMessage::AccountDesktopAuthReply(request_generation, reply_message) => {
                self.on_account_desktop_auth_reply(request_generation, reply_message)
            }
            AppMessage::NotificationsSaveReply(request_generation, reply_message) => {
                self.on_notifications_save_reply(request_generation, reply_message)
            }
            AppMessage::BellHeadReply(request_generation, reply_message) => {
                self.on_bell_head_reply(request_generation, reply_message)
            }
            AppMessage::BellNavigationReply(request_generation, reply_message) => {
                self.on_bell_navigation_reply(request_generation, reply_message)
            }
            AppMessage::PaletteSearchReply(request_generation, reply_message) => {
                self.on_palette_search_reply(request_generation, reply_message)
            }
            AppMessage::ChannelWindowLoadReply(request_generation, reply_message) => {
                self.on_channel_window_load_reply(request_generation, reply_message)
            }
            AppMessage::AppearanceLoadReply(request_generation, reply_message) => {
                self.on_appearance_load_reply(request_generation, reply_message)
            }
            AppMessage::NotificationsLoadReply(request_generation, reply_message) => {
                self.on_notifications_load_reply(request_generation, reply_message)
            }
            AppMessage::HubLoadReply(request_generation, reply_message) => {
                self.on_hub_load_reply(request_generation, reply_message)
            }
            AppMessage::NetworkProbeReply(request_generation, reply_message) => {
                self.on_network_probe_reply(request_generation, reply_message)
            }
            AppMessage::WelcomeAccountLoadReply(request_generation, reply_message) => {
                self.on_welcome_account_load_reply(request_generation, reply_message)
            }
            AppMessage::ChainIdentityLoadReply(request_generation, reply_message) => {
                self.on_chain_identity_load_reply(request_generation, reply_message)
            }
            AppMessage::WalletsLoadReply(request_generation, reply_message) => {
                self.on_wallets_load_reply(request_generation, reply_message)
            }
            AppMessage::WelcomeQrAuthReply(request_generation, reply_message) => {
                self.on_welcome_qr_auth_reply(request_generation, reply_message)
            }
            AppMessage::WelcomeDesktopAuthReply(request_generation, reply_message) => {
                self.on_welcome_desktop_auth_reply(request_generation, reply_message)
            }
            AppMessage::ProvisionProgressReply(request_generation, reply_message) => {
                self.on_provision_progress_reply(request_generation, reply_message)
            }
            AppMessage::AppearanceLoaded(mode) => self.on_appearance_loaded(mode),
            AppMessage::SetAppearanceLight => self.on_set_appearance_light(),
            AppMessage::SetAppearanceDark => self.on_set_appearance_dark(),
            AppMessage::AppearanceSaved(_written) => self.on_appearance_saved(_written),
            AppMessage::DesktopNotificationsLoaded(enabled) => {
                self.on_desktop_notifications_loaded(enabled)
            }
            AppMessage::DesktopNotificationsSaved(_written) => {
                self.on_desktop_notifications_saved(_written)
            }
            AppMessage::Reconnect => self.on_reconnect(),
            AppMessage::WorkspaceConnected(next) => self.on_workspace_connected(next),
            AppMessage::ConsoleEntryAnswered => self.on_console_entry_answered(),
            AppMessage::LiveUpdated(next) => self.on_live_updated(next),
            AppMessage::LiveResynced(next) => self.on_live_resynced(next),
            AppMessage::LiveResyncFailed(cause) => self.on_live_resync_failed(cause),
            AppMessage::SelectShellTab(next) => self.on_select_shell_tab(next),
            AppMessage::MembersLoadSelected(request) => self.on_members_load_selected(request),
            AppMessage::SettingsLoadSelected(request) => self.on_settings_load_selected(request),
            AppMessage::AccountLoadSelected(request) => self.on_account_load_selected(request),
            AppMessage::DmPeersLoadSelected(request) => self.on_dm_peers_load_selected(request),
            AppMessage::NamesMovedSelected(request) => self.on_names_moved_selected(request),
            AppMessage::Tick => self.on_tick(),
            AppMessage::WallTick => self.on_wall_tick(),
            AppMessage::WindowWasClosed(id) => self.on_window_was_closed(id),
            AppMessage::TrayOpen => self.on_tray_open(),
            AppMessage::TrayQuit => self.on_tray_quit(),
            AppMessage::ModifierStateChanged(mods) => self.on_modifier_state_changed(mods),
            AppMessage::CloseLaunchWindow => self.on_close_launch_window(),
            AppMessage::WindowFocused(id) => self.on_window_focused(id),
            AppMessage::WindowUnfocused(id) => self.on_window_unfocused(id),
            AppMessage::CommandChordPressed(event) => self.on_command_chord_pressed(event),
            AppMessage::TrayOpenBell => self.on_tray_open_bell(),
            AppMessage::TrayGoChat => self.on_tray_go_chat(),
            AppMessage::TrayGoPages => self.on_tray_go_pages(),
            AppMessage::TrayGoNode => self.on_tray_go_node(),
            AppMessage::TrayGoSettings => self.on_tray_go_settings(),
            AppMessage::TrayReconnect => self.on_tray_reconnect(),
            AppMessage::TrayCopyNodeKey => self.on_tray_copy_node_key(),
            AppMessage::MutationFailed(cause) => self.on_mutation_failed(cause),
            AppMessage::DismissError => self.on_dismiss_error(),
            AppMessage::ConnectFailed(cause) => self.on_connect_failed(cause),
            AppMessage::ForgeViewEvent(event) => self.on_forge_view_event(event),
            AppMessage::ForgeComposerEvent(scope, body) => {
                self.on_forge_composer_event(scope, body)
            }
            AppMessage::ForgeNoteSent(op, next) => self.on_forge_note_sent(op, next),
            AppMessage::ForgeNoteFailed(scope, op, cause) => {
                self.on_forge_note_failed(scope, op, cause)
            }
            AppMessage::FilesViewEvent(event) => self.on_files_view_event(event),
            AppMessage::FsFileDropped(path) => self.on_fs_file_dropped(path),
            AppMessage::AttachmentQueued(scope, id, path) => {
                self.on_attachment_queued(scope, id, path)
            }
            AppMessage::AttachmentUploaded(scope, id, outcome) => {
                self.on_attachment_uploaded(scope, id, outcome)
            }
            AppMessage::FsDropped(_result) => self.on_fs_dropped(_result),
            AppMessage::FsDropFailed(cause) => self.on_fs_drop_failed(cause),
            AppMessage::AccountLoaded(next) => self.on_account_loaded(next),
            AppMessage::AccountFailed(cause) => self.on_account_failed(cause),
            AppMessage::AccountRenamed(_result) => self.on_account_renamed(_result),
            AppMessage::AccountRenameFailed(cause) => self.on_account_rename_failed(cause),
            AppMessage::AccountTicketMinted(ticket) => self.on_account_ticket_minted(ticket),
            AppMessage::AccountCeremonyStepped(next) => self.on_account_ceremony_stepped(next),
            AppMessage::AccountChanged(_result) => self.on_account_changed(_result),
            AppMessage::AccountOpFailed(cause) => self.on_account_op_failed(cause),
            AppMessage::OpenRunPanel(dispatch_id) => self.on_open_run_panel(dispatch_id),
            AppMessage::GovernanceViewEvent(event) => self.on_governance_view_event(event),
            AppMessage::MembersViewEvent(event) => self.on_members_view_event(event),
            AppMessage::MembersLoaded(next) => self.on_members_loaded(next),
            AppMessage::MembersFailed(cause) => self.on_members_failed(cause),
            AppMessage::DmPeersLoaded(next) => self.on_dm_peers_loaded(next),
            AppMessage::DmPeersFailed(cause) => self.on_dm_peers_failed(cause),
            AppMessage::AgentsViewEvent(event) => self.on_agents_view_event(event),
            AppMessage::AgentStatusSet(_result) => self.on_agent_status_set(_result),
            AppMessage::NodeViewEvent(event) => self.on_node_view_event(event),
            AppMessage::NodeFactsLoaded(next) => self.on_node_facts_loaded(next),
            AppMessage::NodeFactsFailed(_cause) => self.on_node_facts_failed(_cause),
            AppMessage::NodeStatusPushed(next) => self.on_node_status_pushed(next),
            AppMessage::SettingsLoaded(next) => self.on_settings_loaded(next),
            AppMessage::SettingsFailed(cause) => self.on_settings_failed(cause),
            AppMessage::SettingsViewEvent(event) => self.on_settings_view_event(event),
            AppMessage::SettingsUnlocked(pubkey) => self.on_settings_unlocked(pubkey),
            AppMessage::SettingsUnlockFailed(cause) => self.on_settings_unlock_failed(cause),
            AppMessage::CopyToClipboard(text, label) => self.on_copy_to_clipboard(text, label),
            AppMessage::DismissToast => self.on_dismiss_toast(),
            AppMessage::ToastTick => self.on_toast_tick(),
            AppMessage::ExplorerViewEvent(event) => self.on_explorer_view_event(event),
            AppMessage::ClosePalette => self.on_close_palette(),
            AppMessage::ToggleBell => self.on_toggle_bell(),
            AppMessage::ReloadBell => self.on_reload_bell(),
            AppMessage::CloseBell => self.on_close_bell(),
            AppMessage::MarkBellReadSubmit => self.on_mark_bell_read_submit(),
            AppMessage::BellLoaded(generation, account, next) => {
                self.on_bell_loaded(generation, account, next)
            }
            AppMessage::BellContextLoaded(generation, account, next) => {
                self.on_bell_context_loaded(generation, account, next)
            }
            AppMessage::BellFailed(generation, account, cause) => {
                self.on_bell_failed(generation, account, cause)
            }
            AppMessage::BellMarked(generation, account, delta) => {
                self.on_bell_marked(generation, account, delta)
            }
            AppMessage::BellMarkFailed(generation, account, cause) => {
                self.on_bell_mark_failed(generation, account, cause)
            }
            AppMessage::BellOpenItem(generation, account, context) => {
                self.on_bell_open_item(generation, account, context)
            }
            AppMessage::GlobalKeyPressed(event) => self.on_global_key_pressed(event),
            AppMessage::PaletteChanged(next) => self.on_palette_changed(next),
            AppMessage::PaletteResults(next) => self.on_palette_results(next),
            AppMessage::PaletteSearchFailed(cause) => self.on_palette_search_failed(cause),
            AppMessage::OpenChatSearchHit(channel_id, target_seq) => {
                self.on_open_chat_search_hit(channel_id, target_seq)
            }
            AppMessage::ChooseChannel(id) => self.on_choose_channel(id),
            AppMessage::ChooseDm(peer_key) => self.on_choose_dm(peer_key),
            AppMessage::CreateChannelSubmit => self.on_create_channel_submit(),
            AppMessage::ToggleChannelCreateMembersOnly => {
                self.on_toggle_channel_create_members_only()
            }
            AppMessage::ToggleChannelCreate => self.on_toggle_channel_create(),
            AppMessage::JoinHuddleSubmit => self.on_join_huddle_submit(),
            AppMessage::HuddleJoinedAck(_result) => self.on_huddle_joined_ack(_result),
            AppMessage::ChatBeginEdit(scope, body, seq, rev) => {
                self.on_chat_begin_edit(scope, body, seq, rev)
            }
            AppMessage::ComposerSubmitted(kind, pending_body, pending_id, scope) => {
                self.on_composer_submitted(kind, pending_body, pending_id, scope)
            }
            AppMessage::EditMessageSubmit(text) => self.on_edit_message_submit(text),
            AppMessage::MessageSent(next) => self.on_message_sent(next),
            AppMessage::MessageSendFailed(cause) => self.on_message_send_failed(cause),
            AppMessage::ThreadReplySendFailed(cause) => self.on_thread_reply_send_failed(cause),
            AppMessage::ThreadReplySent(next) => self.on_thread_reply_sent(next),
            AppMessage::ChatUpdated(next) => self.on_chat_updated(next),
            AppMessage::ChatLoadFailed(cause) => self.on_chat_load_failed(cause),
            AppMessage::ChannelCreated(next) => self.on_channel_created(next),
            AppMessage::LiveAgentsEvent(next) => self.on_live_agents_event(next),
            AppMessage::LiveCancelAcked(_ok) => self.on_live_cancel_acked(_ok),
            AppMessage::ChatAcked(_result) => self.on_chat_acked(_result),
            AppMessage::CopyMessageLink(link) => self.on_copy_message_link(link),
            AppMessage::OpenMessageLink(url) => self.on_open_message_link(url),
            AppMessage::ChatScrolled(_absolute_x, _absolute_y, _relative_x, relative_y) => {
                self.on_chat_scrolled(_absolute_x, _absolute_y, _relative_x, relative_y)
            }
            AppMessage::CopyChordPressed(event) => self.on_copy_chord_pressed(event),
            AppMessage::ChatViewEvent(event) => self.on_chat_view_event(event),
            AppMessage::PagesViewEvent(event) => self.on_pages_view_event(event),
            AppMessage::OpenPageSearchHit(page_id, _block_id) => {
                self.on_open_page_search_hit(page_id, _block_id)
            }
            AppMessage::ExternalUrlFailed(cause) => self.on_external_url_failed(cause),
            AppMessage::OnboardingOpened(id) => self.on_onboarding_opened(id),
            AppMessage::HubBooted(state) => self.on_hub_booted(state),
            AppMessage::HubRefreshed(state) => self.on_hub_refreshed(state),
            AppMessage::NetworkProbed(probe) => self.on_network_probed(probe),
            AppMessage::PickWallet(name) => self.on_pick_wallet(name),
            AppMessage::UnlockSubmit(pw) => self.on_unlock_submit(pw),
            AppMessage::KeyUnlocked(pubkey) => self.on_key_unlocked(pubkey),
            AppMessage::LoginSkip => self.on_login_skip(),
            AppMessage::PasswordSubmit(pw) => self.on_password_submit(pw),
            AppMessage::DeviceKeyCreated(_name) => self.on_device_key_created(_name),
            AppMessage::PhraseWrittenDown => self.on_phrase_written_down(),
            AppMessage::ShowPhraseAgain => self.on_show_phrase_again(),
            AppMessage::ConfirmPhraseSubmit(answer) => self.on_confirm_phrase_submit(answer),
            AppMessage::PhraseConfirmed(pubkey) => self.on_phrase_confirmed(pubkey),
            AppMessage::PhraseConfirmFailed(cause) => self.on_phrase_confirm_failed(cause),
            AppMessage::GoRestore => self.on_go_restore(),
            AppMessage::GoLogin => self.on_go_login(),
            AppMessage::RestoreSubmit(name, pw) => self.on_restore_submit(name, pw),
            AppMessage::KeyRestored(pubkey) => self.on_key_restored(pubkey),
            AppMessage::LoginFailed(cause) => self.on_login_failed(cause),
            AppMessage::PickNetwork(id) => self.on_pick_network(id),
            AppMessage::OpenNetworkSubmit => self.on_open_network_submit(),
            AppMessage::ConnectRemoteSubmit(endpoint) => self.on_connect_remote_submit(endpoint),
            AppMessage::WalletsLoaded(list) => self.on_wallets_loaded(list),
            AppMessage::ChainNamed(id) => self.on_chain_named(id),
            AppMessage::ChainProbeFailed(_cause) => self.on_chain_probe_failed(_cause),
            AppMessage::AccountProbed(next) => self.on_account_probed(next),
            AppMessage::AccountProbeFailed(cause) => self.on_account_probe_failed(cause),
            AppMessage::WelcomeSkip => self.on_welcome_skip(),
            AppMessage::WelcomeCancel => self.on_welcome_cancel(),
            AppMessage::WelcomeCreateSubmit(name) => self.on_welcome_create_submit(name),
            AppMessage::WelcomeLoginSubmit => self.on_welcome_login_submit(),
            AppMessage::WelcomeDesktop => self.on_welcome_desktop(),
            AppMessage::WelcomeDesktopDone(_ok) => self.on_welcome_desktop_done(_ok),
            AppMessage::CeremonyStepped(next) => self.on_ceremony_stepped(next),
            AppMessage::WelcomeFailed(cause) => self.on_welcome_failed(cause),
            AppMessage::NetworkEntered => self.on_network_entered(),
            AppMessage::ConsoleOpened(id) => self.on_console_opened(id),
            AppMessage::ForgetNetworkSubmit(id) => self.on_forget_network_submit(id),
            AppMessage::NetworkForgotten(_written) => self.on_network_forgotten(_written),
            AppMessage::GoJoin => self.on_go_join(),
            AppMessage::GoNetworks => self.on_go_networks(),
            AppMessage::JoinNetworkSubmit => self.on_join_network_submit(),
            AppMessage::WorkspaceMaterialized(init) => self.on_workspace_materialized(init),
            AppMessage::ProvisionStepped(step) => self.on_provision_stepped(step),
            AppMessage::OnboardingInviteMinted(blob) => self.on_onboarding_invite_minted(blob),
            AppMessage::CopyOnboardingInvite => self.on_copy_onboarding_invite(),
            AppMessage::EnterConsole => self.on_enter_console(),
            AppMessage::OnboardingFailed(cause) => self.on_onboarding_failed(cause),
            AppMessage::SwitchNetwork => self.on_switch_network(),
            AppMessage::OnboardingReopened(id) => self.on_onboarding_reopened(id),
            AppMessage::DismissAccountBanner => self.on_dismiss_account_banner(),
            AppMessage::OpenAccountWelcome => self.on_open_account_welcome(),
            AppMessage::WelcomeReopened(id) => self.on_welcome_reopened(id),
            AppMessage::CallEvent(event) => self.on_call_event(event),
            AppMessage::ToggleCallMute => self.on_toggle_call_mute(),
            AppMessage::ToggleCallDeafen => self.on_toggle_call_deafen(),
            AppMessage::ToggleCallCamera => self.on_toggle_call_camera(),
            AppMessage::ToggleCallScreen => self.on_toggle_call_screen(),
            AppMessage::ShowHuddle => self.on_show_huddle(),
            AppMessage::HuddleOpened(id) => self.on_huddle_opened(id),
            AppMessage::HuddleGoChannel => self.on_huddle_go_channel(),
            AppMessage::LeaveHuddleHere => self.on_leave_huddle_here(),
            AppMessage::HuddleLeft(_result) => self.on_huddle_left(_result),
            AppMessage::SecretTyped(slot, text) => self.on_secret_typed(slot, text),
            AppMessage::ChannelDraftChanged(value) => self.on_channel_draft_changed(value),
        }
    }
    fn on_appearance_save_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.appearance_save_generation == request_generation {
            self.appearance_save_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_connection_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.connection_generation == request_generation {
            self.connection_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_dm_peers_load_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.dm_peers_load_generation == request_generation {
            self.dm_peers_load_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_node_facts_load_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.node_facts_load_generation == request_generation {
            self.node_facts_load_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_bell_load_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.bell_load_generation == request_generation {
            self.bell_load_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_members_load_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.members_load_generation == request_generation {
            self.members_load_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_settings_load_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.settings_load_generation == request_generation {
            self.settings_load_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_account_load_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.account_load_generation == request_generation {
            self.account_load_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_live_resync_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.live_resync_generation == request_generation {
            self.live_resync_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_bell_presentations_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.bell_presentations_generation == request_generation {
            self.bell_presentations_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_account_qr_auth_reply(
        &mut self,
        request_generation: u64,
        reply_message: Option<Box<AppMessage>>,
    ) -> Task<AppMessage> {
        if self.account_qr_auth_generation == request_generation {
            if let Some(reply_message) = reply_message {
                return self.update(*reply_message);
            }
            self.account_qr_auth_task = None;
        }
        Task::none()
    }
    fn on_account_desktop_auth_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.account_desktop_auth_generation == request_generation {
            self.account_desktop_auth_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_notifications_save_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.notifications_save_generation == request_generation {
            self.notifications_save_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_bell_head_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.bell_head_generation == request_generation {
            self.bell_head_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_bell_navigation_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.bell_navigation_generation == request_generation {
            self.bell_navigation_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_palette_search_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.palette_search_generation == request_generation {
            self.palette_search_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_channel_window_load_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.channel_window_load_generation == request_generation {
            self.channel_window_load_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_appearance_load_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.appearance_load_generation == request_generation {
            self.appearance_load_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_notifications_load_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.notifications_load_generation == request_generation {
            self.notifications_load_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_hub_load_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.hub_load_generation == request_generation {
            self.hub_load_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_network_probe_reply(
        &mut self,
        request_generation: u64,
        reply_message: Option<Box<AppMessage>>,
    ) -> Task<AppMessage> {
        if self.network_probe_generation == request_generation {
            if let Some(reply_message) = reply_message {
                return self.update(*reply_message);
            }
            self.network_probe_task = None;
        }
        Task::none()
    }
    fn on_welcome_account_load_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.welcome_account_load_generation == request_generation {
            self.welcome_account_load_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_chain_identity_load_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.chain_identity_load_generation == request_generation {
            self.chain_identity_load_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_wallets_load_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.wallets_load_generation == request_generation {
            self.wallets_load_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_welcome_qr_auth_reply(
        &mut self,
        request_generation: u64,
        reply_message: Option<Box<AppMessage>>,
    ) -> Task<AppMessage> {
        if self.welcome_qr_auth_generation == request_generation {
            if let Some(reply_message) = reply_message {
                return self.update(*reply_message);
            }
            self.welcome_qr_auth_task = None;
        }
        Task::none()
    }
    fn on_welcome_desktop_auth_reply(
        &mut self,
        request_generation: u64,
        reply_message: Box<AppMessage>,
    ) -> Task<AppMessage> {
        if self.welcome_desktop_auth_generation == request_generation {
            self.welcome_desktop_auth_task = None;
            return self.update(*reply_message);
        }
        Task::none()
    }
    fn on_provision_progress_reply(
        &mut self,
        request_generation: u64,
        reply_message: Option<Box<AppMessage>>,
    ) -> Task<AppMessage> {
        if self.provision_progress_generation == request_generation {
            if let Some(reply_message) = reply_message {
                return self.update(*reply_message);
            }
            self.provision_progress_task = None;
        }
        Task::none()
    }
    fn on_appearance_loaded(&mut self, mode: Appearance) -> Task<AppMessage> {
        self.appearance = mode;
        Task::none()
    }
    fn on_set_appearance_light(&mut self) -> Task<AppMessage> {
        self.appearance = Appearance::Light;
        let pending_task = Task::perform(
            crate::backend::save_appearance(self.appearance),
            AppMessage::AppearanceSaved,
        );
        self.appearance_save_generation = self.appearance_save_generation.wrapping_add(1);
        let request_generation = self.appearance_save_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .appearance_save_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::AppearanceSaveReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_set_appearance_dark(&mut self) -> Task<AppMessage> {
        self.appearance = Appearance::Dark;
        let pending_task = Task::perform(
            crate::backend::save_appearance(self.appearance),
            AppMessage::AppearanceSaved,
        );
        self.appearance_save_generation = self.appearance_save_generation.wrapping_add(1);
        let request_generation = self.appearance_save_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .appearance_save_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::AppearanceSaveReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_appearance_saved(&mut self, _written: bool) -> Task<AppMessage> {
        Task::none()
    }
    fn on_desktop_notifications_loaded(&mut self, enabled: bool) -> Task<AppMessage> {
        self.desktop_notifications = enabled;
        Task::none()
    }
    fn on_desktop_notifications_saved(&mut self, _written: bool) -> Task<AppMessage> {
        Task::none()
    }
    fn on_reconnect(&mut self) -> Task<AppMessage> {
        if self.loading
            || ((self.mutation_phase != MutationPhase::Idle)
                && (self.mutation_phase != MutationPhase::Recovering))
        {
            return Task::none();
        }
        self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_qr_auth_task.take() {
            previous_handle.abort();
        }
        self.account_desktop_auth_generation = self.account_desktop_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_desktop_auth_task.take() {
            previous_handle.abort();
        }
        self.account_busy = self.account_busy && (self.account_ceremony_phase).is_empty();
        self.account_ceremony_phase = "".to_owned();
        self.account_ceremony_qr = "".to_owned();
        self.account_ceremony_detail = "".to_owned();
        self.account_ceremony_left = "".to_owned();
        self.palette_search_generation = self.palette_search_generation.wrapping_add(1);
        if let Some(previous_handle) = self.palette_search_task.take() {
            previous_handle.abort();
        }
        self.channel_window_load_generation = self.channel_window_load_generation.wrapping_add(1);
        if let Some(previous_handle) = self.channel_window_load_task.take() {
            previous_handle.abort();
        }
        self.live_resync_generation = self.live_resync_generation.wrapping_add(1);
        if let Some(previous_handle) = self.live_resync_task.take() {
            previous_handle.abort();
        }
        self.hydration_generation += 1;
        self.hydration_retry_attempt = 0;
        self.mutation_phase = MutationPhase::Idle;
        self.loading = true;
        self.connected = false;
        self.channels = Vec::new();
        self.rooms = Vec::new();
        self.dm_rows = Vec::new();
        self.chat_at_tail = true;
        self.chat_land_seq = 0;
        self.chat_pending_sends = Vec::new();
        self.chat_edit_seq = 0;
        self.chat_edit_rev = 0;
        self.channel_reads = Vec::new();
        self.unread_boundary = 0;
        self.active_channel = "".to_owned();
        self.active_dm_peer = "".to_owned();
        self.active_dm = crate::backend::no_dm_peer();
        self.history_view = false;
        self.active_channel_name = "".to_owned();
        self.active_channel_archived = false;
        self.active_channel_members_only = false;
        self.channel_members = Vec::new();
        self.post_refusal = "".to_owned();
        self.pending_channel = "".to_owned();
        self.page_route = "".to_owned();
        self.palette_search_phase = SearchPhase::Idle;
        self.error = "".to_owned();
        self.status = "Connecting…".to_owned();
        self.bell_marking = false;
        self.bell_error = "".to_owned();
        self.bell_head_generation = self.bell_head_generation.wrapping_add(1);
        if let Some(previous_handle) = self.bell_head_task.take() {
            previous_handle.abort();
        }
        self.bell_navigation_generation = self.bell_navigation_generation.wrapping_add(1);
        if let Some(previous_handle) = self.bell_navigation_task.take() {
            previous_handle.abort();
        }
        self.bell_load_generation = self.bell_load_generation.wrapping_add(1);
        if let Some(previous_handle) = self.bell_load_task.take() {
            previous_handle.abort();
        }
        self.bell_presentations_generation = self.bell_presentations_generation.wrapping_add(1);
        if let Some(previous_handle) = self.bell_presentations_task.take() {
            previous_handle.abort();
        }
        self.bell_items = Vec::new();
        self.bell_presentations = Vec::new();
        self.bell_unread = 0;
        self.bell_read_through = 0;
        self.bell_clear_through = 0;
        self.connect_generation += 1;
        let pending_task = Task::perform(
            crate::backend::connect(
                self.connected_rpc.to_owned(),
                self.hydration_retry_attempt,
                self.connect_generation,
            ),
            |result| match result {
                Ok(value) => AppMessage::WorkspaceConnected(value),
                Err(error) => AppMessage::ConnectFailed(error),
            },
        );
        self.connection_generation = self.connection_generation.wrapping_add(1);
        let request_generation = self.connection_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self.connection_task.replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::ConnectionReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_workspace_connected(&mut self, next: crate::backend::WorkspaceData) -> Task<AppMessage> {
        if next.generation != self.connect_generation {
            return Task::none();
        }
        self.rpc = next.rpc.to_owned();
        self.connected_rpc = next.rpc.to_owned();
        self.network_name = crate::backend::network_label(
            &self.network_chain_id,
            &self.connected_rpc,
        );
        self.status = next.status.to_owned();
        self.block_height = next.height;
        self.channels = next.channels.clone();
        self.chat_chain_id = self.network_chain_id.to_owned();
        self.channel_reads = crate::backend::initial_channel_reads(
            next.channels.clone(),
            ::std::mem::take(&mut self.channel_reads),
        );
        self.rooms = crate::backend::chat_sidebar_rooms(
            self.channels.clone(),
            self.dm_peers.clone(),
            self.channel_reads.clone(),
        );
        self.dm_rows = crate::backend::chat_sidebar_dms(
            self.channels.clone(),
            self.dm_peers.clone(),
            self.channel_reads.clone(),
        );
        self.unread_boundary = 0;
        self.history_view = false;
        self.chat_at_tail = true;
        self.chat_land_seq = 0;
        self.active_channel = next.active_channel.to_owned();
        self.active_dm_peer = crate::backend::dm_peer_of_channel(
            self.active_dm_peer.to_owned(),
            self.dm_peers.clone(),
            self.active_channel.to_owned(),
        );
        self.active_dm =
            crate::backend::dm_peer_named(self.dm_peers.clone(), self.active_dm_peer.to_owned());
        self.active_channel_name = next.active_channel_name.to_owned();
        self.active_channel_archived = next.active_channel_archived;
        self.active_channel_members_only = next.active_channel_members_only;
        self.huddle_joined_at =
            crate::backend::keep_i64(self.huddle_joined, self.huddle_joined_at, self.huddle_now);
        let huddle = crate::backend::huddle_after_load(
            true,
            self.huddle_joined,
            self.huddle_channel.to_owned(),
            self.huddle_channel_name.to_owned(),
            self.huddle_roster.clone(),
            self.active_channel.to_owned(),
            self.active_channel_name.to_owned(),
            next.huddle_roster.clone(),
        );
        self.huddle_joined = huddle.joined;
        self.huddle_roster = huddle.roster.clone();
        self.huddle_rows = crate::call::huddle_tile_rows(
            self.huddle_roster.clone(),
            self.call_peers.clone(),
            self.call_muted,
            self.call_speaking,
        );
        self.huddle_channel = huddle.channel.to_owned();
        self.huddle_channel_name = huddle.channel_name.to_owned();
        self.channel_members = next.channel_members.clone();
        crate::module_view::chat_composer_roster(
            &(crate::backend::composer_scope(&self.connected_rpc, &self.active_channel)),
            &self.channel_members,
        );
        self.post_refusal = crate::backend::post_gate(
            self.active_channel_archived,
            self.active_channel_members_only,
            self.channel_members.clone(),
            self.settings_user_key.to_owned(),
        );
        self.connected = true;
        self.loading = false;
        self.mutation_phase = MutationPhase::Idle;
        self.hydration_retry_attempt = 0;
        self.error = "".to_owned();
        self.members_generation += 1;
        self.agents_open_run = "".to_owned();
        self.agents_live = false;
        self.account_generation += 1;
        self.settings_generation += 1;
        self.dm_peers_generation += 1;
        Task::batch([
            {
                let pending_task = Task::perform(
                    crate::backend::load_dm_peers(
                        self.connected_rpc.to_owned(),
                        self.dm_peers_generation,
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::DmPeersLoaded(value),
                        Err(error) => AppMessage::DmPeersFailed(error),
                    },
                );
                self.dm_peers_load_generation = self.dm_peers_load_generation.wrapping_add(1);
                let request_generation = self.dm_peers_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .dm_peers_load_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::DmPeersLoadReply(request_generation, Box::new(reply_message))
                })
            },
            {
                let pending_task = Task::perform(
                    crate::backend::load_node_facts(self.connected_rpc.to_owned()),
                    |result| match result {
                        Ok(value) => AppMessage::NodeFactsLoaded(value),
                        Err(error) => AppMessage::NodeFactsFailed(error),
                    },
                );
                self.node_facts_load_generation = self.node_facts_load_generation.wrapping_add(1);
                let request_generation = self.node_facts_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .node_facts_load_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::NodeFactsLoadReply(request_generation, Box::new(reply_message))
                })
            },
            {
                let pending_task = {
                    let reply_generation = self.connect_generation;
                    let reply_account = self.account_number.to_owned();
                    Task::perform(
                        crate::backend::load_bell(
                            self.connected_rpc.to_owned(),
                            self.account_number.to_owned(),
                        ),
                        move |result| match result {
                            Ok(value) => AppMessage::BellLoaded(
                                reply_generation,
                                reply_account.clone(),
                                value,
                            ),
                            Err(error) => AppMessage::BellFailed(
                                reply_generation,
                                reply_account.clone(),
                                error,
                            ),
                        },
                    )
                };
                self.bell_load_generation = self.bell_load_generation.wrapping_add(1);
                let request_generation = self.bell_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) =
                    self.bell_load_task.replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::BellLoadReply(request_generation, Box::new(reply_message))
                })
            },
            {
                let pending_task = Task::perform(
                    crate::backend::load_members(
                        self.connected_rpc.to_owned(),
                        self.members_generation,
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::MembersLoaded(value),
                        Err(error) => AppMessage::MembersFailed(error),
                    },
                );
                self.members_load_generation = self.members_load_generation.wrapping_add(1);
                let request_generation = self.members_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .members_load_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::MembersLoadReply(request_generation, Box::new(reply_message))
                })
            },
            {
                let pending_task = Task::perform(
                    crate::backend::load_settings_facts(
                        self.connected_rpc.to_owned(),
                        self.settings_generation,
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::SettingsLoaded(value),
                        Err(error) => AppMessage::SettingsFailed(error),
                    },
                );
                self.settings_load_generation = self.settings_load_generation.wrapping_add(1);
                let request_generation = self.settings_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .settings_load_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::SettingsLoadReply(request_generation, Box::new(reply_message))
                })
            },
            {
                let pending_task = Task::perform(
                    crate::backend::load_account(
                        self.connected_rpc.to_owned(),
                        self.account_generation,
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::AccountLoaded(value),
                        Err(error) => AppMessage::AccountFailed(error),
                    },
                );
                self.account_load_generation = self.account_load_generation.wrapping_add(1);
                let request_generation = self.account_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .account_load_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::AccountLoadReply(request_generation, Box::new(reply_message))
                })
            },
            crate::shell::close::<AppMessage>(crate::backend::window_target_unless(
                self.huddle_joined,
                self.huddle_win,
            )),
            Task::done(AppMessage::ConsoleEntryAnswered),
        ])
    }
    fn on_console_entry_answered(&mut self) -> Task<AppMessage> {
        match self.console_entry {
            ConsoleEntry::Entering => {
                self.console_entry = ConsoleEntry::Idle;
                let (_, pending_task) = crate::shell::open(crate::shell::WindowKind::Console);
                pending_task.map(AppMessage::ConsoleOpened)
            }
            ConsoleEntry::Idle => {
                self.console_entry = ConsoleEntry::Idle;
                Task::none()
            }
        }
    }
    fn on_live_updated(&mut self, next: crate::backend::LiveUpdate) -> Task<AppMessage> {
        self.status = next.status.to_owned();
        self.block_height =
            crate::backend::keep_i64(next.height >= 0, next.height, self.block_height);
        self.views_live_serial =
            crate::module_view::view_block_hit(self.block_height, self.views_live_serial);
        match next.kind {
            LiveKind::Retry => Task::none(),
            LiveKind::Tip => Task::none(),
            LiveKind::Ready => {
                self.hydration_generation += 1;
                self.hydration_retry_attempt = 0;
                let pending_task = Task::perform(
                    crate::backend::live_resync_load(
                        self.connected_rpc.to_owned(),
                        self.active_channel.to_owned(),
                        next.load_chat,
                        next.debounce,
                        self.hydration_generation,
                        0,
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::LiveResynced(value),
                        Err(error) => AppMessage::LiveResyncFailed(error),
                    },
                );
                self.live_resync_generation = self.live_resync_generation.wrapping_add(1);
                let request_generation = self.live_resync_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .live_resync_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::LiveResyncReply(request_generation, Box::new(reply_message))
                })
            }
            LiveKind::Chat => {
                self.views_live_serial =
                    crate::module_view::view_live_hit(&(next.module), self.views_live_serial);
                let folded_chat = crate::backend::fold_live_chat(
                    next.chat.clone(),
                    self.channels.clone(),
                    self.channel_members.clone(),
                    self.channel_reads.clone(),
                    self.dm_peers.clone(),
                    self.settings_user_key.to_owned(),
                    self.active_channel.to_owned(),
                    self.history_view,
                    self.shell_tab == ShellTab::Chat,
                    self.active_channel_name.to_owned(),
                    self.active_channel_archived,
                    self.active_channel_members_only,
                );
                self.channels = folded_chat.channels.clone();
                self.channel_members = folded_chat.channel_members.clone();
                crate::module_view::chat_composer_roster(
                    &(crate::backend::composer_scope(&self.connected_rpc, &self.active_channel)),
                    &self.channel_members,
                );
                self.channel_reads = folded_chat.channel_reads.clone();
                self.rooms = folded_chat.rooms.clone();
                self.dm_rows = folded_chat.dm_rows.clone();
                self.active_channel_name = folded_chat.active_channel_name.to_owned();
                self.active_channel_archived = folded_chat.active_channel_archived;
                self.active_channel_members_only = folded_chat.active_channel_members_only;
                self.post_refusal = folded_chat.post_refusal.to_owned();
                if !folded_chat.refresh_chat {
                    return Task::none();
                }
                self.hydration_generation += 1;
                self.hydration_retry_attempt = 0;
                let pending_task = Task::perform(
                    crate::backend::live_resync_load(
                        self.connected_rpc.to_owned(),
                        self.active_channel.to_owned(),
                        true,
                        false,
                        self.hydration_generation,
                        0,
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::LiveResynced(value),
                        Err(error) => AppMessage::LiveResyncFailed(error),
                    },
                );
                self.live_resync_generation = self.live_resync_generation.wrapping_add(1);
                let request_generation = self.live_resync_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .live_resync_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::LiveResyncReply(request_generation, Box::new(reply_message))
                })
            }
            LiveKind::Bell => {
                self.bell_read_through = crate::backend::keep_i64(
                    (next.bell.kind == "read") && (next.bell.up_to_seq > self.bell_read_through),
                    next.bell.up_to_seq,
                    self.bell_read_through,
                );
                self.bell_clear_through = crate::backend::keep_i64(
                    (next.bell.kind == "cleared")
                        && (next.bell.up_to_seq > self.bell_clear_through),
                    next.bell.up_to_seq,
                    self.bell_clear_through,
                );
                self.bell_items = crate::backend::merge_bell_loaded(
                    crate::backend::apply_bell(
                        ::std::mem::take(&mut self.bell_items),
                        next.bell.clone(),
                    ),
                    Vec::new(),
                    self.bell_read_through,
                    self.bell_clear_through,
                );
                self.bell_unread = crate::backend::bell_unread_count(
                    &self.bell_items,
                    &self.account_number,
                    &self.settings_user_key,
                );
                self.bell_presentations = crate::backend::merge_bell_presentations(
                    crate::backend::bell_visible_items(
                        &self.bell_items,
                        &self.account_number,
                        &self.settings_user_key,
                    ),
                    ::std::mem::take(&mut self.bell_presentations),
                    Vec::new(),
                );
                if next.bell.kind != "delivered" {
                    return Task::none();
                }
                let pending_task = {
                    let reply_generation = self.connect_generation;
                    let reply_account = self.account_number.to_owned();
                    Task::perform(
                        crate::backend::load_bell_presentations(
                            self.connected_rpc.to_owned(),
                            crate::backend::bell_missing_items(
                                crate::backend::bell_visible_items(
                                    &self.bell_items,
                                    &self.account_number,
                                    &(self.settings_user_key),
                                ),
                                &self.bell_presentations,
                            ),
                        ),
                        move |result| match result {
                            Ok(value) => AppMessage::BellContextLoaded(
                                reply_generation,
                                reply_account.clone(),
                                value,
                            ),
                            Err(error) => AppMessage::BellFailed(
                                reply_generation,
                                reply_account.clone(),
                                error,
                            ),
                        },
                    )
                };
                self.bell_presentations_generation =
                    self.bell_presentations_generation.wrapping_add(1);
                let request_generation = self.bell_presentations_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .bell_presentations_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::BellPresentationsReply(request_generation, Box::new(reply_message))
                })
            }
            LiveKind::Plane => {
                self.views_live_serial =
                    crate::module_view::view_live_hit(&(next.module), self.views_live_serial);
                self.members_generation = crate::backend::keep_i64(
                    crate::backend::plane_live_hit(
                        next.kind,
                        next.module.to_owned(),
                        "valset".to_owned(),
                    ),
                    self.members_generation + 1,
                    self.members_generation,
                );
                self.account_generation = crate::backend::keep_i64(
                    crate::backend::plane_live_hit(
                        next.kind,
                        next.module.to_owned(),
                        "identity".to_owned(),
                    ),
                    self.account_generation + 1,
                    self.account_generation,
                );
                self.dm_peers_generation = crate::backend::keep_i64(
                    crate::backend::plane_live_hit(
                        next.kind,
                        next.module.to_owned(),
                        "identity".to_owned(),
                    ),
                    self.dm_peers_generation + 1,
                    self.dm_peers_generation,
                );
                Task::batch([
                    Task::done(crate::backend::load_request(
                        crate::backend::plane_live_hit(
                            next.kind,
                            next.module.to_owned(),
                            "valset".to_owned(),
                        ),
                        self.connected_rpc.to_owned(),
                        "".to_owned(),
                        self.members_generation,
                    ))
                    .and_then(Task::done)
                    .map(AppMessage::MembersLoadSelected),
                    Task::done(crate::backend::load_request(
                        crate::backend::plane_live_hit(
                            next.kind,
                            next.module.to_owned(),
                            "identity".to_owned(),
                        ),
                        self.connected_rpc.to_owned(),
                        "".to_owned(),
                        self.account_generation,
                    ))
                    .and_then(Task::done)
                    .map(AppMessage::AccountLoadSelected),
                    Task::done(crate::backend::load_request(
                        crate::backend::plane_live_hit(
                            next.kind,
                            next.module.to_owned(),
                            "identity".to_owned(),
                        ),
                        self.connected_rpc.to_owned(),
                        "".to_owned(),
                        self.dm_peers_generation,
                    ))
                    .and_then(Task::done)
                    .map(AppMessage::DmPeersLoadSelected),
                    Task::done(crate::backend::load_request(
                        crate::backend::plane_live_hit(
                            next.kind,
                            next.module.to_owned(),
                            "identity".to_owned(),
                        ),
                        self.connected_rpc.to_owned(),
                        "".to_owned(),
                        self.hydration_generation,
                    ))
                    .and_then(Task::done)
                    .map(AppMessage::NamesMovedSelected),
                ])
            }
            LiveKind::Resync => {
                self.views_live_serial =
                    crate::module_view::view_live_hit(&(next.module), self.views_live_serial);
                if !next.load_chat {
                    return Task::none();
                }
                self.hydration_generation += 1;
                self.hydration_retry_attempt = 0;
                let pending_task = Task::perform(
                    crate::backend::live_resync_load(
                        self.connected_rpc.to_owned(),
                        self.active_channel.to_owned(),
                        next.load_chat,
                        next.debounce,
                        self.hydration_generation,
                        0,
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::LiveResynced(value),
                        Err(error) => AppMessage::LiveResyncFailed(error),
                    },
                );
                self.live_resync_generation = self.live_resync_generation.wrapping_add(1);
                let request_generation = self.live_resync_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .live_resync_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::LiveResyncReply(request_generation, Box::new(reply_message))
                })
            }
        }
    }
    fn on_live_resynced(&mut self, next: crate::backend::LiveRefresh) -> Task<AppMessage> {
        if next.generation != self.hydration_generation {
            return Task::none();
        }
        self.hydration_retry_attempt = 0;
        let chain_left_behind = crate::backend::chain_moved(
            self.chat_chain_id.to_owned(),
            self.network_chain_id.to_owned(),
        );
        self.channels = crate::backend::keep_channels(
            next.chat_loaded,
            chain_left_behind,
            next.channels.clone(),
            ::std::mem::take(&mut self.channels),
        );
        self.channel_reads = crate::backend::initial_channel_reads(
            self.channels.clone(),
            ::std::mem::take(&mut self.channel_reads),
        );
        self.chat_chain_id = crate::backend::keep_str(
            next.chat_loaded && (!(self.network_chain_id).is_empty()),
            &self.network_chain_id,
            &self.chat_chain_id,
        );
        self.history_view = self.history_view && (!next.chat_loaded);
        self.chat_land_seq = crate::backend::keep_i64(next.chat_loaded, 0, self.chat_land_seq);
        self.active_channel = crate::backend::keep_str(
            next.chat_loaded,
            &(next.active_channel),
            &self.active_channel,
        );
        self.active_dm_peer = crate::backend::keep_str(
            next.chat_loaded && (!self.loading),
            &(crate::backend::dm_peer_of_channel(
                self.active_dm_peer.to_owned(),
                self.dm_peers.clone(),
                self.active_channel.to_owned(),
            )),
            &self.active_dm_peer,
        );
        self.active_dm =
            crate::backend::dm_peer_named(self.dm_peers.clone(), self.active_dm_peer.to_owned());
        self.active_channel_name = crate::backend::keep_str(
            next.chat_loaded,
            &(next.active_channel_name),
            &self.active_channel_name,
        );
        self.active_channel_archived = crate::backend::keep_bool(
            next.chat_loaded,
            next.active_channel_archived,
            self.active_channel_archived,
        );
        self.active_channel_members_only = crate::backend::keep_bool(
            next.chat_loaded,
            next.active_channel_members_only,
            self.active_channel_members_only,
        );
        self.huddle_joined_at =
            crate::backend::keep_i64(self.huddle_joined, self.huddle_joined_at, self.huddle_now);
        let huddle = crate::backend::huddle_after_load(
            next.chat_loaded,
            self.huddle_joined,
            self.huddle_channel.to_owned(),
            self.huddle_channel_name.to_owned(),
            self.huddle_roster.clone(),
            self.active_channel.to_owned(),
            self.active_channel_name.to_owned(),
            next.huddle_roster.clone(),
        );
        self.huddle_joined = huddle.joined;
        self.huddle_roster = huddle.roster.clone();
        self.huddle_rows = crate::call::huddle_tile_rows(
            self.huddle_roster.clone(),
            self.call_peers.clone(),
            self.call_muted,
            self.call_speaking,
        );
        self.huddle_channel = huddle.channel.to_owned();
        self.huddle_channel_name = huddle.channel_name.to_owned();
        self.channel_members = crate::backend::keep_members(
            next.chat_loaded,
            next.channel_members.clone(),
            ::std::mem::take(&mut self.channel_members),
        );
        self.post_refusal = crate::backend::post_gate(
            self.active_channel_archived,
            self.active_channel_members_only,
            self.channel_members.clone(),
            self.settings_user_key.to_owned(),
        );
        let resync_tail_channel = crate::backend::keep_str(
            (!self.history_view) && (self.shell_tab == ShellTab::Chat),
            &self.active_channel,
            "" ,
        );
        self.unread_boundary = crate::backend::frozen_unread_boundary(
            self.channel_reads.clone(),
            self.channels.clone(),
            self.active_channel.to_owned(),
            self.active_channel.to_owned(),
            self.unread_boundary,
        );
        self.channel_reads = crate::backend::mark_channel_read(
            ::std::mem::take(&mut self.channel_reads),
            resync_tail_channel.to_owned(),
            crate::backend::channel_head_seq(self.channels.clone(), resync_tail_channel.to_owned()),
        );
        self.rooms = crate::backend::chat_sidebar_rooms(
            self.channels.clone(),
            self.dm_peers.clone(),
            self.channel_reads.clone(),
        );
        self.dm_rows = crate::backend::chat_sidebar_dms(
            self.channels.clone(),
            self.dm_peers.clone(),
            self.channel_reads.clone(),
        );
        self.mutation_phase =
            crate::backend::mutation_phase_after_recovery(self.mutation_phase);
        self.error = "".to_owned();
        crate::shell::close::<AppMessage>(crate::backend::window_target_unless(
            self.huddle_joined,
            self.huddle_win,
        ))
    }
    fn on_live_resync_failed(&mut self, cause: crate::backend::HydrationError) -> Task<AppMessage> {
        if cause.generation != self.hydration_generation {
            return Task::none();
        }
        self.status = "Sync delayed".to_owned();
        self.error = "Live sync interrupted. Retrying…".to_owned();
        self.hydration_retry_attempt += 1;
        let pending_task = Task::perform(
            crate::backend::live_resync_load(
                self.connected_rpc.to_owned(),
                self.active_channel.to_owned(),
                true,
                false,
                self.hydration_generation,
                self.hydration_retry_attempt,
            ),
            |result| match result {
                Ok(value) => AppMessage::LiveResynced(value),
                Err(error) => AppMessage::LiveResyncFailed(error),
            },
        );
        self.live_resync_generation = self.live_resync_generation.wrapping_add(1);
        let request_generation = self.live_resync_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .live_resync_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::LiveResyncReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_select_shell_tab(&mut self, next: ShellTab) -> Task<AppMessage> {
        let staying_on_settings =
            (self.shell_tab == ShellTab::Settings) && (next == ShellTab::Settings);
        let keeping_authentication =
            staying_on_settings && (!(self.account_ceremony_phase).is_empty());
        if keeping_authentication {
            return Task::none();
        }
        self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_qr_auth_task.take() {
            previous_handle.abort();
        }
        self.account_desktop_auth_generation = self.account_desktop_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_desktop_auth_task.take() {
            previous_handle.abort();
        }
        self.account_busy = self.account_busy && (self.account_ceremony_phase).is_empty();
        self.account_ceremony_phase = "".to_owned();
        self.account_ceremony_qr = "".to_owned();
        self.account_ceremony_detail = "".to_owned();
        self.account_ceremony_left = "".to_owned();
        self.shell_tab = next;
        let chat_tab_channel = crate::backend::keep_str(
            (self.shell_tab == ShellTab::Chat) && (!self.history_view),
            &self.active_channel,
            "" ,
        );
        let chat_tab_arrivals =
            crate::backend::channel_head_seq(self.channels.clone(), chat_tab_channel.to_owned())
                > crate::backend::channel_last_read(
                    self.channel_reads.clone(),
                    chat_tab_channel.to_owned(),
                );
        self.unread_boundary = crate::backend::keep_i64(
            chat_tab_arrivals,
            crate::backend::channel_last_read(
                self.channel_reads.clone(),
                chat_tab_channel.to_owned(),
            ),
            self.unread_boundary,
        );
        self.channel_reads = crate::backend::mark_channel_read(
            ::std::mem::take(&mut self.channel_reads),
            chat_tab_channel.to_owned(),
            crate::backend::channel_head_seq(self.channels.clone(), chat_tab_channel.to_owned()),
        );
        self.rooms = crate::backend::chat_sidebar_rooms(
            self.channels.clone(),
            self.dm_peers.clone(),
            self.channel_reads.clone(),
        );
        self.dm_rows = crate::backend::chat_sidebar_dms(
            self.channels.clone(),
            self.dm_peers.clone(),
            self.channel_reads.clone(),
        );
        self.error = "".to_owned();
        if !self.connected {
            return Task::none();
        }
        if (self.shell_tab == ShellTab::Chat) || (self.shell_tab == ShellTab::Pages) {
            return Task::none();
        }
        self.members_generation += 1;
        self.account_generation += 1;
        self.settings_generation = crate::backend::keep_i64(
            self.shell_tab == ShellTab::Settings,
            self.settings_generation + 1,
            self.settings_generation,
        );
        Task::batch([
            Task::done(crate::backend::load_request(
                crate::backend::tab_reads_plane(self.shell_tab, "members".to_owned()),
                self.connected_rpc.to_owned(),
                "".to_owned(),
                self.members_generation,
            ))
            .and_then(Task::done)
            .map(AppMessage::MembersLoadSelected),
            Task::done(crate::backend::load_request(
                self.shell_tab == ShellTab::Settings,
                self.connected_rpc.to_owned(),
                "".to_owned(),
                self.settings_generation,
            ))
            .and_then(Task::done)
            .map(AppMessage::SettingsLoadSelected),
            Task::done(crate::backend::load_request(
                crate::backend::tab_reads_plane(self.shell_tab, "account".to_owned()),
                self.connected_rpc.to_owned(),
                "".to_owned(),
                self.account_generation,
            ))
            .and_then(Task::done)
            .map(AppMessage::AccountLoadSelected),
        ])
    }
    fn on_members_load_selected(
        &mut self,
        request: crate::backend::LoadRequest,
    ) -> Task<AppMessage> {
        let obsolete_request =
            (request.rpc != self.connected_rpc) || (request.generation != self.members_generation);
        if obsolete_request {
            return Task::none();
        }
        let pending_task = Task::perform(
            crate::backend::load_members(request.rpc.to_owned(), request.generation),
            |result| match result {
                Ok(value) => AppMessage::MembersLoaded(value),
                Err(error) => AppMessage::MembersFailed(error),
            },
        );
        self.members_load_generation = self.members_load_generation.wrapping_add(1);
        let request_generation = self.members_load_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .members_load_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::MembersLoadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_settings_load_selected(
        &mut self,
        request: crate::backend::LoadRequest,
    ) -> Task<AppMessage> {
        let obsolete_request =
            (request.rpc != self.connected_rpc) || (request.generation != self.settings_generation);
        let unmounted = self.shell_tab != ShellTab::Settings;
        if obsolete_request || unmounted {
            return Task::none();
        }
        let pending_task = Task::perform(
            crate::backend::load_settings_facts(request.rpc.to_owned(), request.generation),
            |result| match result {
                Ok(value) => AppMessage::SettingsLoaded(value),
                Err(error) => AppMessage::SettingsFailed(error),
            },
        );
        self.settings_load_generation = self.settings_load_generation.wrapping_add(1);
        let request_generation = self.settings_load_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .settings_load_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::SettingsLoadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_account_load_selected(
        &mut self,
        request: crate::backend::LoadRequest,
    ) -> Task<AppMessage> {
        let obsolete_request =
            (request.rpc != self.connected_rpc) || (request.generation != self.account_generation);
        if obsolete_request {
            return Task::none();
        }
        let pending_task = Task::perform(
            crate::backend::load_account(request.rpc.to_owned(), request.generation),
            |result| match result {
                Ok(value) => AppMessage::AccountLoaded(value),
                Err(error) => AppMessage::AccountFailed(error),
            },
        );
        self.account_load_generation = self.account_load_generation.wrapping_add(1);
        let request_generation = self.account_load_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .account_load_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::AccountLoadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_dm_peers_load_selected(
        &mut self,
        request: crate::backend::LoadRequest,
    ) -> Task<AppMessage> {
        let obsolete_request =
            (request.rpc != self.connected_rpc) || (request.generation != self.dm_peers_generation);
        if obsolete_request {
            return Task::none();
        }
        let pending_task = Task::perform(
            crate::backend::load_dm_peers(request.rpc.to_owned(), request.generation),
            |result| match result {
                Ok(value) => AppMessage::DmPeersLoaded(value),
                Err(error) => AppMessage::DmPeersFailed(error),
            },
        );
        self.dm_peers_load_generation = self.dm_peers_load_generation.wrapping_add(1);
        let request_generation = self.dm_peers_load_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .dm_peers_load_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::DmPeersLoadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_names_moved_selected(
        &mut self,
        request: crate::backend::LoadRequest,
    ) -> Task<AppMessage> {
        let obsolete_request = (request.rpc != self.connected_rpc)
            || (request.generation != self.hydration_generation);
        if obsolete_request {
            return Task::none();
        }
        self.hydration_generation += 1;
        self.hydration_retry_attempt = 0;
        let pending_task = Task::perform(
            crate::backend::live_resync_load(
                self.connected_rpc.to_owned(),
                self.active_channel.to_owned(),
                true,
                false,
                self.hydration_generation,
                0,
            ),
            |result| match result {
                Ok(value) => AppMessage::LiveResynced(value),
                Err(error) => AppMessage::LiveResyncFailed(error),
            },
        );
        self.live_resync_generation = self.live_resync_generation.wrapping_add(1);
        let request_generation = self.live_resync_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .live_resync_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::LiveResyncReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_tick(&mut self) -> Task<AppMessage> {
        self.huddle_now += 1;
        Task::none()
    }
    fn on_wall_tick(&mut self) -> Task<AppMessage> {
        self.wall_now = crate::backend::current_wall_seconds();
        Task::none()
    }
    fn on_window_was_closed(&mut self, id: crate::shell::WindowKey) -> Task<AppMessage> {
        let closed_welcome =
            (self.onboarding_win == Some(id)) && (self.hub_step == HubStep::Account);
        let closed_account = self.console_win == Some(id);
        let retirement = crate::backend::ceremony_retirement(closed_welcome, closed_account);
        self.onboarding_win = crate::backend::without_window(self.onboarding_win, id);
        self.console_win = crate::backend::without_window(self.console_win, id);
        self.huddle_win = crate::backend::without_window(self.huddle_win, id);
        let leaving = crate::backend::last_window_closed_exits(
            self.console_win,
            self.onboarding_win,
        );
        match retirement {
            CeremonyRetirement::Welcome => {
                self.welcome_qr_auth_generation = self.welcome_qr_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.welcome_qr_auth_task.take() {
                    previous_handle.abort();
                }
                self.welcome_desktop_auth_generation =
                    self.welcome_desktop_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.welcome_desktop_auth_task.take() {
                    previous_handle.abort();
                }
                self.mutation_phase = MutationPhase::Idle;
                self.ceremony_phase = "".to_owned();
                self.ceremony_qr = "".to_owned();
                self.ceremony_detail = "".to_owned();
                self.ceremony_left = "".to_owned();
                if !leaving {
                    return Task::none();
                }
                crate::shell::quit::<AppMessage>()
            }
            CeremonyRetirement::Account => {
                self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_qr_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_desktop_auth_generation =
                    self.account_desktop_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_desktop_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_busy = self.account_busy && (self.account_ceremony_phase).is_empty();
                self.account_ceremony_phase = "".to_owned();
                self.account_ceremony_qr = "".to_owned();
                self.account_ceremony_detail = "".to_owned();
                self.account_ceremony_left = "".to_owned();
                if !leaving {
                    return Task::none();
                }
                crate::shell::quit::<AppMessage>()
            }
            CeremonyRetirement::Keep => {
                if !leaving {
                    return Task::none();
                }
                crate::shell::quit::<AppMessage>()
            }
        }
    }
    fn on_tray_open(&mut self) -> Task<AppMessage> {
        let window_tracked = self.console_win.is_some() || self.onboarding_win.is_some();
        let opening = crate::backend::tray_open_action(self.connected, window_tracked);
        match opening {
            TrayOpen::Launch => {
                let (_, pending_task) = crate::shell::open(crate::shell::WindowKind::Onboarding);
                pending_task.map(AppMessage::OnboardingOpened)
            }
            TrayOpen::Console => Task::done(AppMessage::NetworkEntered),
            TrayOpen::Raise => Task::batch([
                crate::shell::raise::<AppMessage>(crate::backend::window_target(
                    self.console_win,
                )),
                crate::shell::raise::<AppMessage>(crate::backend::window_target(
                    self.onboarding_win,
                )),
            ]),
        }
    }
    fn on_tray_quit(&mut self) -> Task<AppMessage> {
        self.welcome_qr_auth_generation = self.welcome_qr_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.welcome_qr_auth_task.take() {
            previous_handle.abort();
        }
        self.welcome_desktop_auth_generation = self.welcome_desktop_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.welcome_desktop_auth_task.take() {
            previous_handle.abort();
        }
        self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_qr_auth_task.take() {
            previous_handle.abort();
        }
        self.account_desktop_auth_generation = self.account_desktop_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_desktop_auth_task.take() {
            previous_handle.abort();
        }
        crate::shell::quit::<AppMessage>()
    }
    fn on_modifier_state_changed(&mut self, mods: gpui_kit::Modifiers) -> Task<AppMessage> {
        self.cmd_held = crate::backend::command_held(mods);
        self.shift_held = crate::backend::shift_held(mods);
        Task::none()
    }
    fn on_close_launch_window(&mut self) -> Task<AppMessage> {
        crate::shell::close::<AppMessage>(crate::backend::window_target(
            self.onboarding_win,
        ))
    }
    fn on_window_focused(&mut self, id: crate::shell::WindowKey) -> Task<AppMessage> {
        self.focused_win = Some(id);
        crate::backend::note_window_focus(true).discard()
    }
    fn on_window_unfocused(&mut self, id: crate::shell::WindowKey) -> Task<AppMessage> {
        self.focused_win = crate::backend::without_window(self.focused_win, id);
        crate::backend::note_window_focus(self.focused_win.is_some()).discard()
    }
    fn on_command_chord_pressed(&mut self, event: crate::shell::KeyPress) -> Task<AppMessage> {
        let chord = crate::backend::command_chord(event.key.clone(), event.modifiers);
        match chord {
            CommandChord::Quit => {
                self.welcome_qr_auth_generation = self.welcome_qr_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.welcome_qr_auth_task.take() {
                    previous_handle.abort();
                }
                self.welcome_desktop_auth_generation =
                    self.welcome_desktop_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.welcome_desktop_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_qr_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_desktop_auth_generation =
                    self.account_desktop_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_desktop_auth_task.take() {
                    previous_handle.abort();
                }
                crate::shell::quit::<AppMessage>()
            }
            CommandChord::CloseWindow => crate::shell::close::<AppMessage>(
                crate::backend::window_target(self.focused_win),
            ),
            CommandChord::Ignored => Task::none(),
        }
    }
    fn on_tray_open_bell(&mut self) -> Task<AppMessage> {
        if self.console_win.is_none() {
            return Task::none();
        }
        self.bell_open = true;
        crate::shell::raise::<AppMessage>(crate::backend::window_target(self.console_win))
    }
    fn on_tray_go_chat(&mut self) -> Task<AppMessage> {
        if self.console_win.is_none() {
            return Task::none();
        }
        Task::batch([
            crate::shell::raise::<AppMessage>(crate::backend::window_target(
                self.console_win,
            )),
            Task::done(AppMessage::SelectShellTab(ShellTab::Chat)),
        ])
    }
    fn on_tray_go_pages(&mut self) -> Task<AppMessage> {
        if self.console_win.is_none() {
            return Task::none();
        }
        Task::batch([
            crate::shell::raise::<AppMessage>(crate::backend::window_target(
                self.console_win,
            )),
            Task::done(AppMessage::SelectShellTab(ShellTab::Pages)),
        ])
    }
    fn on_tray_go_node(&mut self) -> Task<AppMessage> {
        if self.console_win.is_none() {
            return Task::none();
        }
        Task::batch([
            crate::shell::raise::<AppMessage>(crate::backend::window_target(
                self.console_win,
            )),
            Task::done(AppMessage::SelectShellTab(ShellTab::Node)),
        ])
    }
    fn on_tray_go_settings(&mut self) -> Task<AppMessage> {
        if self.console_win.is_none() {
            return Task::none();
        }
        Task::batch([
            crate::shell::raise::<AppMessage>(crate::backend::window_target(
                self.console_win,
            )),
            Task::done(AppMessage::SelectShellTab(ShellTab::Settings)),
        ])
    }
    fn on_tray_reconnect(&mut self) -> Task<AppMessage> {
        if self.console_win.is_none() {
            return Task::none();
        }
        Task::done(AppMessage::Reconnect)
    }
    fn on_tray_copy_node_key(&mut self) -> Task<AppMessage> {
        if self.console_win.is_none() || (self.node_key).is_empty() {
            return Task::none();
        }
        self.toast = "Copied node key".to_owned();
        self.toast_age = 0;
        crate::shell::clipboard::<AppMessage>(self.node_key.to_owned())
    }
    fn on_mutation_failed(&mut self, cause: crate::backend::AppError) -> Task<AppMessage> {
        self.chat_edit_seq = crate::backend::message_seq_after_failure(
            self.chat_edit_seq,
            self.mutation_phase,
            cause.committed,
        );
        self.chat_edit_rev = crate::backend::message_seq_after_failure(
            self.chat_edit_rev,
            self.mutation_phase,
            cause.committed,
        );
        self.mutation_phase = crate::backend::mutation_failure_phase(cause.committed);
        self.channel_draft = crate::backend::restore_draft(
            self.channel_draft.to_owned(),
            self.pending_channel.to_owned(),
            cause.committed,
        );
        self.pending_channel = "".to_owned();
        self.error = cause.message.to_owned();
        if !cause.committed {
            return Task::none();
        }
        self.hydration_generation += 1;
        self.hydration_retry_attempt = 0;
        let pending_task = Task::perform(
            crate::backend::live_resync_load(
                self.connected_rpc.to_owned(),
                self.active_channel.to_owned(),
                true,
                false,
                self.hydration_generation,
                0,
            ),
            |result| match result {
                Ok(value) => AppMessage::LiveResynced(value),
                Err(error) => AppMessage::LiveResyncFailed(error),
            },
        );
        self.live_resync_generation = self.live_resync_generation.wrapping_add(1);
        let request_generation = self.live_resync_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .live_resync_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::LiveResyncReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_dismiss_error(&mut self) -> Task<AppMessage> {
        self.error = "".to_owned();
        Task::none()
    }
    fn on_connect_failed(&mut self, cause: crate::backend::HydrationError) -> Task<AppMessage> {
        if cause.generation != self.connect_generation {
            return Task::none();
        }
        self.hydration_generation += 1;
        self.bell_marking = false;
        self.bell_error = "".to_owned();
        self.bell_head_generation = self.bell_head_generation.wrapping_add(1);
        if let Some(previous_handle) = self.bell_head_task.take() {
            previous_handle.abort();
        }
        self.bell_navigation_generation = self.bell_navigation_generation.wrapping_add(1);
        if let Some(previous_handle) = self.bell_navigation_task.take() {
            previous_handle.abort();
        }
        self.bell_load_generation = self.bell_load_generation.wrapping_add(1);
        if let Some(previous_handle) = self.bell_load_task.take() {
            previous_handle.abort();
        }
        self.bell_presentations_generation = self.bell_presentations_generation.wrapping_add(1);
        if let Some(previous_handle) = self.bell_presentations_task.take() {
            previous_handle.abort();
        }
        self.bell_items = Vec::new();
        self.bell_presentations = Vec::new();
        self.bell_unread = 0;
        self.bell_read_through = 0;
        self.bell_clear_through = 0;
        self.connect_generation += 1;
        self.hydration_retry_attempt += 1;
        self.loading = false;
        self.status = "Offline".to_owned();
        self.error = cause.message.to_owned();
        self.onboarding_error = crate::backend::keep_str(
            self.console_entry == ConsoleEntry::Entering,
            &(cause.message),
            &self.onboarding_error,
        );
        let pending_task = Task::perform(
            crate::backend::connect(
                self.connected_rpc.to_owned(),
                self.hydration_retry_attempt,
                self.connect_generation,
            ),
            |result| match result {
                Ok(value) => AppMessage::WorkspaceConnected(value),
                Err(error) => AppMessage::ConnectFailed(error),
            },
        );
        self.connection_generation = self.connection_generation.wrapping_add(1);
        let request_generation = self.connection_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self.connection_task.replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::ConnectionReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_forge_view_event(
        &mut self,
        event: crate::module_view::ModuleViewEvent,
    ) -> Task<AppMessage> {
        match crate::module_view::forge_intent(&(event)) {
            ForgeIntent::OpenLink => Task::done(AppMessage::OpenMessageLink(
                crate::module_view::event_text(&(event), "url" ),
            )),
            ForgeIntent::Attach => Task::done(AppMessage::AttachmentQueued(
                crate::module_view::event_text(&(event), "scope"),
                crate::module_view::event_text(&(event), "id"),
                crate::module_view::event_text(&(event), "path"),
            )),
            ForgeIntent::Composer => {
                let scope = crate::module_view::event_text(&(event), "scope" );
                let submitted_scope = scope.to_owned();
                // the files the submit sent, linked into the note's body
                let links = crate::composer_surface::take_attachments(
                    &crate::module_view::event_text(&(event), "id"),
                );
                Task::done(AppMessage::ForgeComposerEvent(
                    submitted_scope.clone(),
                    crate::backend::attachment_body(
                        crate::module_view::event_text(&(event), "body"),
                        &links,
                    ),
                ))
            }
            ForgeIntent::Copy => {
                self.toast = crate::module_view::event_text(&(event), "label" );
                self.toast_age = 0;
                crate::shell::clipboard::<AppMessage>(crate::module_view::event_text(
                    &(event),
                    "text" ,
                ))
            }
        }
    }
    fn on_forge_composer_event(&mut self, scope: String, body: String) -> Task<AppMessage> {
        let channel = crate::backend::scope_channel(&(scope), &self.connected_rpc);
        match crate::backend::submit_verdict(
            self.loading,
            self.connected,
            channel.to_owned(),
            self.forge_note_pending.to_owned(),
            !(channel).is_empty(),
            scope.to_owned(),
            scope.to_owned(),
        ) {
            SubmitVerdict::Refused => {
                crate::module_view::chat_composer_unsent(&(scope), &(body), false);
                Task::none()
            }
            SubmitVerdict::Admitted => {
                let op = crate::backend::fresh_operation_id("forge-note".to_owned());
                self.forge_note_pending = op.to_owned();
                let send_operation = op.to_owned();
                let receipt_scope = scope.to_owned();
                let receipt_operation = op.to_owned();
                Task::perform(
                    crate::backend::send_message(
                        self.connected_rpc.to_owned(),
                        self.password.to_owned(),
                        channel.to_owned(),
                        op.to_owned(),
                        (body).trim().to_owned(),
                    ),
                    move |result| match result {
                        Ok(value) => AppMessage::ForgeNoteSent(send_operation.clone(), value),
                        Err(error) => AppMessage::ForgeNoteFailed(
                            receipt_scope.clone(),
                            receipt_operation.clone(),
                            error,
                        ),
                    },
                )
            }
        }
    }
    fn on_forge_note_sent(
        &mut self,
        op: String,
        _next: crate::backend::SendReceipt,
    ) -> Task<AppMessage> {
        if op != self.forge_note_pending {
            return Task::none();
        }
        self.forge_note_pending = "".to_owned();
        self.error = "".to_owned();
        Task::none()
    }
    fn on_forge_note_failed(
        &mut self,
        scope: String,
        op: String,
        cause: crate::backend::OptimisticMutationError,
    ) -> Task<AppMessage> {
        crate::module_view::chat_composer_unsent(&(scope), &(cause.body), cause.committed);
        if op != self.forge_note_pending {
            return Task::none();
        }
        self.forge_note_pending = "".to_owned();
        self.error = cause.message.to_owned();
        Task::none()
    }
    fn on_files_view_event(
        &mut self,
        event: crate::module_view::ModuleViewEvent,
    ) -> Task<AppMessage> {
        self.fs_drop_dir = crate::backend::keep_str(
            event.kind == "at",
            &(crate::module_view::event_text(&(event), "path" )),
            &self.fs_drop_dir,
        );
        if event.kind != "open_link" {
            return Task::none();
        }
        Task::done(AppMessage::OpenMessageLink(crate::module_view::event_text(
            &(event),
            "url" ,
        )))
    }
    fn on_fs_file_dropped(&mut self, path: String) -> Task<AppMessage> {
        // a drop on the chat attaches to the room's composer
        if self.shell_tab == ShellTab::Chat {
            return self.on_chat_file_dropped(path);
        }
        if ((self.shell_tab != ShellTab::Files) || self.fs_dropping) || (!self.connected) {
            return Task::none();
        }
        self.error = crate::backend::files_write_gate(
            self.fs_drop_dir.to_owned(),
            self.settings_user_key.to_owned(),
        );
        if !(self.error).is_empty() {
            return Task::none();
        }
        self.fs_dropping = true;
        Task::perform(
            crate::backend::files_upload(
                self.connected_rpc.to_owned(),
                self.password.to_owned(),
                self.fs_drop_dir.to_owned(),
                path.to_owned(),
            ),
            |result| match result {
                Ok(value) => AppMessage::FsDropped(value),
                Err(error) => AppMessage::FsDropFailed(error),
            },
        )
    }
    /// A file dropped anywhere on the chat: into the standby zone of the
    /// open room's composer, and on its way into DuckFS.
    fn on_chat_file_dropped(&mut self, path: String) -> Task<AppMessage> {
        if !self.connected || self.active_channel.is_empty() {
            return Task::none();
        }
        let scope = crate::backend::composer_scope(&self.connected_rpc, &self.active_channel);
        match crate::composer_surface::attach(&scope, &[path]) {
            Ok(queued) => Task::batch(queued.into_iter().map(|attachment| {
                Task::done(AppMessage::AttachmentQueued(
                    scope.clone(),
                    attachment.id,
                    attachment.path,
                ))
            })),
            Err(reason) => {
                self.error = reason;
                Task::none()
            }
        }
    }
    /// An attached file starts its upload under its own id.
    fn on_attachment_queued(&mut self, scope: String, id: String, path: String) -> Task<AppMessage> {
        let answer_scope = scope.clone();
        let answer_id = id.clone();
        Task::perform(
            crate::backend::attach_file(
                self.connected_rpc.to_owned(),
                self.password.to_owned(),
                id,
                path,
            ),
            move |outcome| {
                AppMessage::AttachmentUploaded(answer_scope.clone(), answer_id.clone(), outcome)
            },
        )
    }
    /// Where the file landed, or why it did not, back to its chip.
    fn on_attachment_uploaded(
        &mut self,
        scope: String,
        id: String,
        outcome: Result<String, String>,
    ) -> Task<AppMessage> {
        crate::composer_surface::attached(&scope, &id, outcome);
        Task::none()
    }
    fn on_fs_dropped(&mut self, _result: bool) -> Task<AppMessage> {
        self.fs_dropping = false;
        Task::none()
    }
    fn on_fs_drop_failed(&mut self, cause: crate::backend::AppError) -> Task<AppMessage> {
        self.fs_dropping = false;
        self.error = cause.message.to_owned();
        Task::none()
    }
    fn on_account_loaded(&mut self, next: crate::backend::AccountData) -> Task<AppMessage> {
        if next.generation != self.account_generation {
            return Task::none();
        }
        self.account_exists = next.exists;
        self.bell_marking = self.bell_marking && (self.account_number == next.number);
        self.bell_items = crate::backend::bell_account_items(
            ::std::mem::take(&mut self.bell_items),
            &self.account_number,
            &(next.number),
        );
        self.bell_presentations = crate::backend::merge_bell_presentations(
            crate::backend::bell_visible_items(
                &self.bell_items,
                &(next.number),
                &self.settings_user_key,
            ),
            ::std::mem::take(&mut self.bell_presentations),
            Vec::new(),
        );
        self.bell_unread = crate::backend::bell_unread_count(
            &self.bell_items,
            &(next.number),
            &self.settings_user_key,
        );
        self.bell_error = "".to_owned();
        self.bell_read_through = crate::backend::keep_i64(
            self.account_number == next.number,
            self.bell_read_through,
            0,
        );
        self.bell_clear_through = crate::backend::keep_i64(
            self.account_number == next.number,
            self.bell_clear_through,
            0,
        );
        self.bell_presentations_generation = self.bell_presentations_generation.wrapping_add(1);
        if let Some(previous_handle) = self.bell_presentations_task.take() {
            previous_handle.abort();
        }
        self.bell_head_generation = self.bell_head_generation.wrapping_add(1);
        if let Some(previous_handle) = self.bell_head_task.take() {
            previous_handle.abort();
        }
        self.bell_navigation_generation = self.bell_navigation_generation.wrapping_add(1);
        if let Some(previous_handle) = self.bell_navigation_task.take() {
            previous_handle.abort();
        }
        self.bell_marking = false;
        self.account_number = next.number.to_owned();
        self.account_name = next.name.to_owned();
        self.account_bio = next.bio.to_owned();
        let pending_task = {
            let reply_generation = self.connect_generation;
            let reply_account = next.number.to_owned();
            Task::perform(
                crate::backend::load_bell(
                    self.connected_rpc.to_owned(),
                    self.account_number.to_owned(),
                ),
                move |result| match result {
                    Ok(value) => {
                        AppMessage::BellLoaded(reply_generation, reply_account.clone(), value)
                    }
                    Err(error) => {
                        AppMessage::BellFailed(reply_generation, reply_account.clone(), error)
                    }
                },
            )
        };
        self.bell_load_generation = self.bell_load_generation.wrapping_add(1);
        let request_generation = self.bell_load_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self.bell_load_task.replace(request_handle.abort_on_drop()) {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::BellLoadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_account_failed(&mut self, cause: crate::backend::HydrationError) -> Task<AppMessage> {
        if cause.generation != self.account_generation {
            return Task::none();
        }
        Task::none()
    }
    fn on_account_renamed(&mut self, _result: bool) -> Task<AppMessage> {
        self.account_busy = false;
        self.account_generation += 1;
        let pending_task = Task::perform(
            crate::backend::load_account(self.connected_rpc.to_owned(), self.account_generation),
            |result| match result {
                Ok(value) => AppMessage::AccountLoaded(value),
                Err(error) => AppMessage::AccountFailed(error),
            },
        );
        self.account_load_generation = self.account_load_generation.wrapping_add(1);
        let request_generation = self.account_load_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .account_load_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::AccountLoadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_account_rename_failed(&mut self, cause: crate::backend::AppError) -> Task<AppMessage> {
        self.account_busy = false;
        self.error = cause.message.to_owned();
        Task::none()
    }
    fn on_account_ticket_minted(&mut self, ticket: String) -> Task<AppMessage> {
        self.account_busy = false;
        self.account_ticket = ticket.to_owned();
        Task::none()
    }
    fn on_account_ceremony_stepped(
        &mut self,
        next: crate::backend::CeremonyStep,
    ) -> Task<AppMessage> {
        let phase = crate::backend::ceremony_phase(&(next));
        self.account_ceremony_phase = next.phase.to_owned();
        self.account_ceremony_qr = next.qr.to_owned();
        self.account_ceremony_detail = next.detail.to_owned();
        self.account_ceremony_left = next.left.to_owned();
        match phase {
            CeremonyPhase::Done => {
                self.account_ceremony_phase = "".to_owned();
                self.account_ceremony_qr = "".to_owned();
                self.account_busy = false;
                self.account_generation += 1;
                let pending_task = Task::perform(
                    crate::backend::load_account(
                        self.connected_rpc.to_owned(),
                        self.account_generation,
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::AccountLoaded(value),
                        Err(error) => AppMessage::AccountFailed(error),
                    },
                );
                self.account_load_generation = self.account_load_generation.wrapping_add(1);
                let request_generation = self.account_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .account_load_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::AccountLoadReply(request_generation, Box::new(reply_message))
                })
            }
            CeremonyPhase::Failed => {
                self.account_ceremony_phase = "".to_owned();
                self.account_ceremony_qr = "".to_owned();
                self.account_busy = false;
                self.error = next.detail.to_owned();
                Task::none()
            }
            CeremonyPhase::ShowQr => {
                self.error = "".to_owned();
                Task::none()
            }
            CeremonyPhase::Working => {
                self.error = "".to_owned();
                Task::none()
            }
        }
    }
    fn on_account_changed(&mut self, _result: bool) -> Task<AppMessage> {
        self.account_ceremony_phase = "".to_owned();
        self.account_ceremony_qr = "".to_owned();
        self.account_ceremony_detail = "".to_owned();
        self.account_ceremony_left = "".to_owned();
        self.account_busy = false;
        self.account_ticket = "".to_owned();
        self.account_generation += 1;
        let pending_task = Task::perform(
            crate::backend::load_account(self.connected_rpc.to_owned(), self.account_generation),
            |result| match result {
                Ok(value) => AppMessage::AccountLoaded(value),
                Err(error) => AppMessage::AccountFailed(error),
            },
        );
        self.account_load_generation = self.account_load_generation.wrapping_add(1);
        let request_generation = self.account_load_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .account_load_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::AccountLoadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_account_op_failed(&mut self, cause: crate::backend::AppError) -> Task<AppMessage> {
        self.account_ceremony_phase = "".to_owned();
        self.account_ceremony_qr = "".to_owned();
        self.account_ceremony_detail = "".to_owned();
        self.account_ceremony_left = "".to_owned();
        self.account_busy = false;
        self.error = cause.message.to_owned();
        Task::none()
    }
    fn on_open_run_panel(&mut self, dispatch_id: String) -> Task<AppMessage> {
        self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_qr_auth_task.take() {
            previous_handle.abort();
        }
        self.account_desktop_auth_generation = self.account_desktop_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_desktop_auth_task.take() {
            previous_handle.abort();
        }
        self.account_busy = self.account_busy && (self.account_ceremony_phase).is_empty();
        self.account_ceremony_phase = "".to_owned();
        self.account_ceremony_qr = "".to_owned();
        self.account_ceremony_detail = "".to_owned();
        self.account_ceremony_left = "".to_owned();
        self.shell_tab = ShellTab::Agents;
        self.agents_open_run = dispatch_id.to_owned();
        self.agents_opened += 1;
        Task::none()
    }
    fn on_governance_view_event(
        &mut self,
        event: crate::module_view::ModuleViewEvent,
    ) -> Task<AppMessage> {
        if event.kind != "badge" {
            return Task::none();
        }
        self.gov_open = crate::module_view::event_int(&(event), "count" );
        Task::none()
    }
    fn on_members_view_event(
        &mut self,
        event: crate::module_view::ModuleViewEvent,
    ) -> Task<AppMessage> {
        if (!self.connected) || (event.kind != "copy") {
            return Task::none();
        }
        self.toast = crate::module_view::event_text(&(event), "label" );
        self.toast_age = 0;
        crate::shell::clipboard::<AppMessage>(crate::module_view::event_text(&(event), "text" ))
    }
    fn on_members_loaded(&mut self, next: crate::backend::MembersData) -> Task<AppMessage> {
        if next.generation != self.members_generation {
            return Task::none();
        }
        self.members_answered = true;
        self.members_rows = next.members.clone();
        Task::none()
    }
    fn on_members_failed(&mut self, cause: crate::backend::HydrationError) -> Task<AppMessage> {
        if cause.generation != self.members_generation {
            return Task::none();
        }
        Task::none()
    }
    fn on_dm_peers_loaded(&mut self, next: crate::backend::DmPeersData) -> Task<AppMessage> {
        if next.generation != self.dm_peers_generation {
            return Task::none();
        }
        self.dm_peers = next.peers.clone();
        self.rooms = crate::backend::chat_sidebar_rooms(
            self.channels.clone(),
            self.dm_peers.clone(),
            self.channel_reads.clone(),
        );
        self.dm_rows = crate::backend::chat_sidebar_dms(
            self.channels.clone(),
            self.dm_peers.clone(),
            self.channel_reads.clone(),
        );
        self.active_dm =
            crate::backend::dm_peer_named(self.dm_peers.clone(), self.active_dm_peer.to_owned());
        Task::none()
    }
    fn on_dm_peers_failed(&mut self, cause: crate::backend::HydrationError) -> Task<AppMessage> {
        if cause.generation != self.dm_peers_generation {
            return Task::none();
        }
        Task::none()
    }
    fn on_agents_view_event(
        &mut self,
        event: crate::module_view::ModuleViewEvent,
    ) -> Task<AppMessage> {
        if !self.connected {
            return Task::none();
        }
        match crate::module_view::agents_intent(&(event)) {
            AgentsIntent::Badge => {
                self.agents_live = crate::module_view::event_int(&(event), "count" ) > 0;
                Task::none()
            }
            AgentsIntent::Register => Task::perform(
                crate::backend::register_agent(
                    self.connected_rpc.to_owned(),
                    self.password.to_owned(),
                    self.account_number.to_owned(),
                    event.detail.to_owned(),
                ),
                |result| match result {
                    Ok(value) => AppMessage::AgentStatusSet(value),
                    Err(error) => AppMessage::MutationFailed(error),
                },
            ),
            AgentsIntent::OpenRun => Task::done(AppMessage::OpenRunPanel(
                crate::module_view::event_text(&(event), "dispatch_id" ),
            )),
            AgentsIntent::OpenLink => Task::done(AppMessage::OpenMessageLink(
                crate::module_view::event_text(&(event), "url" ),
            )),
        }
    }
    fn on_agent_status_set(&mut self, _result: bool) -> Task<AppMessage> {
        self.error = "".to_owned();
        Task::none()
    }
    fn on_node_view_event(
        &mut self,
        event: crate::module_view::ModuleViewEvent,
    ) -> Task<AppMessage> {
        if event.kind != "copy" {
            return Task::none();
        }
        self.toast = crate::module_view::event_text(&(event), "label" );
        self.toast_age = 0;
        crate::shell::clipboard::<AppMessage>(crate::module_view::event_text(&(event), "text" ))
    }
    fn on_node_facts_loaded(&mut self, next: crate::backend::NodeFacts) -> Task<AppMessage> {
        self.node_key = next.public_key.to_owned();
        self.node_version = next.version.to_owned();
        self.node_root_hash = next.root_hash.to_owned();
        self.network_chain_id = next.chain_id.to_owned();
        self.network_name = crate::backend::network_label(
            &self.network_chain_id,
            &self.connected_rpc,
        );
        self.node_last_finalized = next.last_finalized_at;
        self.node_checkpoint = next.checkpoint_height;
        self.node_height = next.height;
        self.node_view_label = crate::backend::optional_number(next.view);
        self.node_quorum_label = crate::backend::optional_number(next.quorum);
        self.node_reachable_label =
            crate::backend::optional_number(next.reachable_validators);
        self.node_phase = next.phase.to_owned();
        self.node_phase_since = next.phase_since;
        self.node_sync_target = next.sync_target;
        self.node_sync_applied = next.sync_applied;
        self.node_sync_retries = next.sync_retries;
        self.node_sync_failures = next.sync_failures;
        self.node_sync_last_error = next.sync_last_error.to_owned();
        let launch_link = self.startup_duck_link.to_owned();
        self.startup_duck_link = "".to_owned();
        if (launch_link).is_empty() {
            return Task::none();
        }
        Task::done(AppMessage::OpenMessageLink(launch_link.to_owned()))
    }
    fn on_node_facts_failed(&mut self, _cause: crate::backend::AppError) -> Task<AppMessage> {
        Task::none()
    }
    fn on_node_status_pushed(&mut self, next: crate::backend::NodeFacts) -> Task<AppMessage> {
        self.node_key = next.public_key.to_owned();
        self.node_version = next.version.to_owned();
        self.node_root_hash = next.root_hash.to_owned();
        self.network_chain_id = next.chain_id.to_owned();
        self.network_name = crate::backend::network_label(
            &self.network_chain_id,
            &self.connected_rpc,
        );
        self.node_last_finalized = next.last_finalized_at;
        self.node_checkpoint = next.checkpoint_height;
        self.node_height = next.height;
        self.node_view_label = crate::backend::optional_number(next.view);
        self.node_quorum_label = crate::backend::optional_number(next.quorum);
        self.node_reachable_label =
            crate::backend::optional_number(next.reachable_validators);
        self.node_phase = next.phase.to_owned();
        self.node_phase_since = next.phase_since;
        self.node_sync_target = next.sync_target;
        self.node_sync_applied = next.sync_applied;
        self.node_sync_retries = next.sync_retries;
        self.node_sync_failures = next.sync_failures;
        self.node_sync_last_error = next.sync_last_error.to_owned();
        Task::none()
    }
    fn on_settings_loaded(&mut self, next: crate::backend::SettingsFacts) -> Task<AppMessage> {
        if next.generation != self.settings_generation {
            return Task::none();
        }
        self.node_data_dir = next.data_dir.to_owned();
        self.settings_key_path = next.key_path.to_owned();
        self.settings_key_state = next.key_state.to_owned();
        self.settings_user_key = next.user_key.to_owned();
        self.post_refusal = crate::backend::post_gate(
            self.active_channel_archived,
            self.active_channel_members_only,
            self.channel_members.clone(),
            self.settings_user_key.to_owned(),
        );
        Task::none()
    }
    fn on_settings_failed(&mut self, cause: crate::backend::HydrationError) -> Task<AppMessage> {
        if cause.generation != self.settings_generation {
            return Task::none();
        }
        Task::none()
    }
    fn on_settings_view_event(
        &mut self,
        event: crate::module_view::ModuleViewEvent,
    ) -> Task<AppMessage> {
        match crate::module_view::settings_intent(&(event)) {
            SettingsIntent::Tab => Task::done(AppMessage::SelectShellTab(
                crate::module_view::settings_event_tab(&(event)),
            )),
            SettingsIntent::Reconnect => Task::done(AppMessage::Reconnect),
            SettingsIntent::SwitchNetwork => Task::done(AppMessage::SwitchNetwork),
            SettingsIntent::Unlock => {
                if (self.mutation_phase != MutationPhase::Idle)
                    || (crate::module_view::event_text(&(event), "password" )).is_empty()
                {
                    return Task::none();
                }
                self.error = "".to_owned();
                self.password = crate::module_view::event_text(&(event), "password" );
                Task::perform(
                    crate::backend::unlock_user_key(
                        self.connected_rpc.to_owned(),
                        self.password.to_owned(),
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::SettingsUnlocked(value),
                        Err(error) => AppMessage::SettingsUnlockFailed(error),
                    },
                )
            }
            SettingsIntent::Lock => {
                self.password = "".to_owned();
                self.signer_key = "".to_owned();
                self.live_agents = Vec::new();
                (Task::perform(crate::backend::lock_signer(), |value| value))
                    .discard::<AppMessage>()
            }
            SettingsIntent::Rename => {
                if (((!self.connected) || (!self.account_exists)) || self.account_busy)
                    || (crate::module_view::event_text(&(event), "name" )).is_empty()
                {
                    return Task::none();
                }
                self.account_busy = true;
                self.error = "".to_owned();
                Task::perform(
                    crate::backend::set_account_name(
                        self.connected_rpc.to_owned(),
                        self.password.to_owned(),
                        crate::module_view::event_text(&(event), "name" ),
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::AccountRenamed(value),
                        Err(error) => AppMessage::AccountRenameFailed(error),
                    },
                )
            }
            SettingsIntent::Create => {
                if ((((!self.connected) || self.account_exists) || self.account_busy)
                    || (self.password).is_empty())
                    || (crate::module_view::event_text(&(event), "name" )).is_empty()
                {
                    return Task::none();
                }
                self.account_busy = true;
                self.error = "".to_owned();
                Task::perform(
                    crate::backend::create_account(
                        self.connected_rpc.to_owned(),
                        self.password.to_owned(),
                        crate::module_view::event_text(&(event), "name" ),
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::AccountChanged(value),
                        Err(error) => AppMessage::AccountOpFailed(error),
                    },
                )
            }
            SettingsIntent::KeyAdd => {
                if ((((!self.connected) || (!self.account_exists)) || self.account_busy)
                    || (self.password).is_empty())
                    || (crate::module_view::event_text(&(event), "pubkey" )).is_empty()
                {
                    return Task::none();
                }
                self.account_busy = true;
                self.error = "".to_owned();
                self.account_ticket = "".to_owned();
                Task::perform(
                    crate::backend::mint_key_ticket(
                        self.connected_rpc.to_owned(),
                        self.password.to_owned(),
                        self.network_chain_id.to_owned(),
                        crate::module_view::event_text(&(event), "pubkey" ),
                        crate::module_view::event_text(&(event), "label" ),
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::AccountTicketMinted(value),
                        Err(error) => AppMessage::AccountOpFailed(error),
                    },
                )
            }
            SettingsIntent::Join => {
                if (((!self.connected) || self.account_busy) || (self.password).is_empty())
                    || (crate::module_view::event_text(&(event), "ticket" )).is_empty()
                {
                    return Task::none();
                }
                self.account_busy = true;
                self.error = "".to_owned();
                Task::perform(
                    crate::backend::join_with_ticket(
                        self.connected_rpc.to_owned(),
                        self.password.to_owned(),
                        crate::module_view::event_text(&(event), "ticket" ),
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::AccountChanged(value),
                        Err(error) => AppMessage::AccountOpFailed(error),
                    },
                )
            }
            SettingsIntent::KeyRemove => {
                if (((!self.connected) || (!self.account_exists)) || self.account_busy)
                    || (self.password).is_empty()
                {
                    return Task::none();
                }
                self.account_busy = true;
                self.error = "".to_owned();
                Task::perform(
                    crate::backend::remove_account_key(
                        self.connected_rpc.to_owned(),
                        self.password.to_owned(),
                        crate::module_view::event_text(&(event), "pubkey" ),
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::AccountChanged(value),
                        Err(error) => AppMessage::AccountOpFailed(error),
                    },
                )
            }
            SettingsIntent::Passkey => {
                if (((!self.connected) || (!self.account_exists)) || self.account_busy)
                    || (self.password).is_empty()
                {
                    return Task::none();
                }
                self.account_busy = true;
                self.error = "".to_owned();
                self.account_ceremony_phase = "working".to_owned();
                self.account_ceremony_detail = "Preparing the passkey…".to_owned();
                let pending_task = Task::run(
                    crate::backend::add_passkey_by_qr(
                        self.connected_rpc.to_owned(),
                        self.password.to_owned(),
                        self.network_chain_id.to_owned(),
                        crate::module_view::event_text(&(event), "label" ),
                    ),
                    AppMessage::AccountCeremonyStepped,
                );
                self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
                let request_generation = self.account_qr_auth_generation;
                let pending_task = pending_task
                    .map(move |reply_message| {
                        AppMessage::AccountQrAuthReply(
                            request_generation,
                            Some(Box::new(reply_message)),
                        )
                    })
                    .chain(Task::done(AppMessage::AccountQrAuthReply(
                        request_generation,
                        None,
                    )));
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .account_qr_auth_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task
            }
            SettingsIntent::PasskeyDesktop => {
                if (((!self.connected) || (!self.account_exists)) || self.account_busy)
                    || (self.password).is_empty()
                {
                    return Task::none();
                }
                self.account_busy = true;
                self.error = "".to_owned();
                self.account_ceremony_phase = "working".to_owned();
                self.account_ceremony_detail = "Continue in the browser…".to_owned();
                let pending_task = Task::perform(
                    crate::backend::register_passkey(
                        self.connected_rpc.to_owned(),
                        self.password.to_owned(),
                        self.network_chain_id.to_owned(),
                        crate::module_view::event_text(&(event), "label" ),
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::AccountChanged(value),
                        Err(error) => AppMessage::AccountOpFailed(error),
                    },
                );
                self.account_desktop_auth_generation =
                    self.account_desktop_auth_generation.wrapping_add(1);
                let request_generation = self.account_desktop_auth_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .account_desktop_auth_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::AccountDesktopAuthReply(request_generation, Box::new(reply_message))
                })
            }
            SettingsIntent::CeremonyCancel => {
                self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_qr_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_desktop_auth_generation =
                    self.account_desktop_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_desktop_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_busy = false;
                self.account_ceremony_phase = "".to_owned();
                self.account_ceremony_qr = "".to_owned();
                self.account_ceremony_detail = "".to_owned();
                self.account_ceremony_left = "".to_owned();
                Task::none()
            }
            SettingsIntent::Wallet => {
                if (((!self.connected) || (!self.account_exists)) || self.account_busy)
                    || (self.password).is_empty()
                {
                    return Task::none();
                }
                self.account_busy = true;
                self.error = "".to_owned();
                self.account_ceremony_phase = "working".to_owned();
                self.account_ceremony_detail = "Continue in the browser…".to_owned();
                let pending_task = Task::perform(
                    crate::backend::link_wallet(
                        self.connected_rpc.to_owned(),
                        self.password.to_owned(),
                        self.network_chain_id.to_owned(),
                        crate::module_view::event_text(&(event), "label" ),
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::AccountChanged(value),
                        Err(error) => AppMessage::AccountOpFailed(error),
                    },
                );
                self.account_desktop_auth_generation =
                    self.account_desktop_auth_generation.wrapping_add(1);
                let request_generation = self.account_desktop_auth_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .account_desktop_auth_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::AccountDesktopAuthReply(request_generation, Box::new(reply_message))
                })
            }
            SettingsIntent::Login => {
                if (((!self.connected) || self.account_exists) || self.account_busy)
                    || (self.password).is_empty()
                {
                    return Task::none();
                }
                self.account_busy = true;
                self.error = "".to_owned();
                self.account_ceremony_phase = "working".to_owned();
                self.account_ceremony_detail = "Continue in the browser…".to_owned();
                let pending_task = Task::perform(
                    crate::backend::login_with_passkey(
                        self.connected_rpc.to_owned(),
                        self.password.to_owned(),
                        self.network_chain_id.to_owned(),
                        "".to_owned(),
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::AccountChanged(value),
                        Err(error) => AppMessage::AccountOpFailed(error),
                    },
                );
                self.account_desktop_auth_generation =
                    self.account_desktop_auth_generation.wrapping_add(1);
                let request_generation = self.account_desktop_auth_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .account_desktop_auth_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::AccountDesktopAuthReply(request_generation, Box::new(reply_message))
                })
            }
            SettingsIntent::Copy => {
                self.toast = crate::module_view::event_text(&(event), "label" );
                self.toast_age = 0;
                crate::shell::clipboard::<AppMessage>(crate::module_view::event_text(
                    &(event),
                    "text" ,
                ))
            }
            SettingsIntent::Light => Task::done(AppMessage::SetAppearanceLight),
            SettingsIntent::Dark => Task::done(AppMessage::SetAppearanceDark),
            SettingsIntent::Notifications => {
                self.desktop_notifications = crate::module_view::event_flag(&(event), "enabled" );
                let pending_task = Task::perform(
                    crate::backend::save_desktop_notifications(self.desktop_notifications),
                    AppMessage::DesktopNotificationsSaved,
                );
                self.notifications_save_generation =
                    self.notifications_save_generation.wrapping_add(1);
                let request_generation = self.notifications_save_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .notifications_save_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::NotificationsSaveReply(request_generation, Box::new(reply_message))
                })
            }
        }
    }
    fn on_settings_unlocked(&mut self, pubkey: String) -> Task<AppMessage> {
        self.error = "".to_owned();
        self.signer_key = pubkey.to_owned();
        self.live_agents = Vec::new();
        Task::none()
    }
    fn on_settings_unlock_failed(&mut self, cause: crate::backend::AppError) -> Task<AppMessage> {
        self.password = "".to_owned();
        self.error = cause.message.to_owned();
        Task::none()
    }
    fn on_copy_to_clipboard(&mut self, text: String, label: String) -> Task<AppMessage> {
        self.toast = label.to_owned();
        self.toast_age = 0;
        crate::shell::clipboard::<AppMessage>(text.to_owned())
    }
    fn on_dismiss_toast(&mut self) -> Task<AppMessage> {
        self.toast = "".to_owned();
        self.toast_age = 0;
        Task::none()
    }
    fn on_toast_tick(&mut self) -> Task<AppMessage> {
        self.toast_age += 1;
        if self.toast_age < 9 {
            return Task::none();
        }
        self.toast = "".to_owned();
        self.toast_age = 0;
        Task::none()
    }
    fn on_explorer_view_event(
        &mut self,
        event: crate::module_view::ModuleViewEvent,
    ) -> Task<AppMessage> {
        if event.kind != "copy" {
            return Task::none();
        }
        self.toast = crate::module_view::event_text(&(event), "label" );
        self.toast_age = 0;
        crate::shell::clipboard::<AppMessage>(crate::module_view::event_text(&(event), "text" ))
    }
    fn on_close_palette(&mut self) -> Task<AppMessage> {
        self.palette_search_generation = self.palette_search_generation.wrapping_add(1);
        if let Some(previous_handle) = self.palette_search_task.take() {
            previous_handle.abort();
        }
        self.palette_search_phase = SearchPhase::Idle;
        self.palette_open = false;
        Task::none()
    }
    fn on_toggle_bell(&mut self) -> Task<AppMessage> {
        self.bell_open = !self.bell_open;
        if !self.bell_open {
            return Task::none();
        }
        self.bell_error = "".to_owned();
        let pending_task = {
            let reply_generation = self.connect_generation;
            let reply_account = self.account_number.to_owned();
            Task::perform(
                crate::backend::load_bell(
                    self.connected_rpc.to_owned(),
                    self.account_number.to_owned(),
                ),
                move |result| match result {
                    Ok(value) => {
                        AppMessage::BellLoaded(reply_generation, reply_account.clone(), value)
                    }
                    Err(error) => {
                        AppMessage::BellFailed(reply_generation, reply_account.clone(), error)
                    }
                },
            )
        };
        self.bell_load_generation = self.bell_load_generation.wrapping_add(1);
        let request_generation = self.bell_load_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self.bell_load_task.replace(request_handle.abort_on_drop()) {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::BellLoadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_reload_bell(&mut self) -> Task<AppMessage> {
        self.bell_error = "".to_owned();
        let pending_task = {
            let reply_generation = self.connect_generation;
            let reply_account = self.account_number.to_owned();
            Task::perform(
                crate::backend::load_bell(
                    self.connected_rpc.to_owned(),
                    self.account_number.to_owned(),
                ),
                move |result| match result {
                    Ok(value) => {
                        AppMessage::BellLoaded(reply_generation, reply_account.clone(), value)
                    }
                    Err(error) => {
                        AppMessage::BellFailed(reply_generation, reply_account.clone(), error)
                    }
                },
            )
        };
        self.bell_load_generation = self.bell_load_generation.wrapping_add(1);
        let request_generation = self.bell_load_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self.bell_load_task.replace(request_handle.abort_on_drop()) {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::BellLoadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_close_bell(&mut self) -> Task<AppMessage> {
        self.bell_open = false;
        Task::none()
    }
    fn on_mark_bell_read_submit(&mut self) -> Task<AppMessage> {
        if (self.bell_unread <= 0) || self.bell_marking {
            return Task::none();
        }
        self.bell_marking = true;
        self.bell_error = "".to_owned();
        let pending_task = {
            let reply_generation = self.connect_generation;
            let reply_account = self.account_number.to_owned();
            Task::perform(
                crate::backend::mark_bell_read(
                    self.connected_rpc.to_owned(),
                    self.password.to_owned(),
                    self.account_number.to_owned(),
                    crate::backend::bell_head(self.bell_items.clone()),
                ),
                move |result| match result {
                    Ok(value) => {
                        AppMessage::BellMarked(reply_generation, reply_account.clone(), value)
                    }
                    Err(error) => {
                        AppMessage::BellMarkFailed(reply_generation, reply_account.clone(), error)
                    }
                },
            )
        };
        self.bell_head_generation = self.bell_head_generation.wrapping_add(1);
        let request_generation = self.bell_head_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self.bell_head_task.replace(request_handle.abort_on_drop()) {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::BellHeadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_bell_loaded(
        &mut self,
        generation: i64,
        account: String,
        next: crate::backend::BellData,
    ) -> Task<AppMessage> {
        if (generation != self.connect_generation) || (account != self.account_number) {
            return Task::none();
        }
        self.bell_error = "".to_owned();
        self.bell_items = crate::backend::merge_bell_loaded(
            ::std::mem::take(&mut self.bell_items),
            next.items.clone(),
            self.bell_read_through,
            self.bell_clear_through,
        );
        self.bell_presentations = crate::backend::merge_bell_presentations(
            crate::backend::bell_visible_items(
                &self.bell_items,
                &self.account_number,
                &self.settings_user_key,
            ),
            ::std::mem::take(&mut self.bell_presentations),
            next.presentations.clone(),
        );
        self.bell_unread = crate::backend::bell_unread_count(
            &self.bell_items,
            &self.account_number,
            &self.settings_user_key,
        );
        Task::none()
    }
    fn on_bell_context_loaded(
        &mut self,
        generation: i64,
        account: String,
        next: Vec<crate::backend::BellPresentation>,
    ) -> Task<AppMessage> {
        if (generation != self.connect_generation) || (account != self.account_number) {
            return Task::none();
        }
        self.bell_presentations = crate::backend::merge_bell_presentations(
            crate::backend::bell_visible_items(
                &self.bell_items,
                &self.account_number,
                &self.settings_user_key,
            ),
            ::std::mem::take(&mut self.bell_presentations),
            next.clone(),
        );
        self.bell_error = "".to_owned();
        Task::none()
    }
    fn on_bell_failed(
        &mut self,
        generation: i64,
        account: String,
        cause: crate::backend::AppError,
    ) -> Task<AppMessage> {
        if (generation != self.connect_generation) || (account != self.account_number) {
            return Task::none();
        }
        self.bell_error = cause.message.to_owned();
        Task::none()
    }
    fn on_bell_marked(
        &mut self,
        generation: i64,
        account: String,
        delta: crate::backend::BellDelta,
    ) -> Task<AppMessage> {
        if (generation != self.connect_generation) || (account != self.account_number) {
            return Task::none();
        }
        self.bell_marking = false;
        self.bell_read_through = crate::backend::keep_i64(
            delta.up_to_seq > self.bell_read_through,
            delta.up_to_seq,
            self.bell_read_through,
        );
        self.bell_items =
            crate::backend::apply_bell(::std::mem::take(&mut self.bell_items), delta.clone());
        self.bell_unread = crate::backend::bell_unread_count(
            &self.bell_items,
            &self.account_number,
            &self.settings_user_key,
        );
        Task::none()
    }
    fn on_bell_mark_failed(
        &mut self,
        generation: i64,
        account: String,
        cause: crate::backend::AppError,
    ) -> Task<AppMessage> {
        if (generation != self.connect_generation) || (account != self.account_number) {
            return Task::none();
        }
        self.bell_marking = false;
        self.bell_error = cause.message.to_owned();
        Task::none()
    }
    fn on_bell_open_item(
        &mut self,
        generation: i64,
        account: String,
        context: crate::backend::BellPresentation,
    ) -> Task<AppMessage> {
        if (generation != self.connect_generation) || (account != self.account_number) {
            return Task::none();
        }
        if (context.target == BellTarget::Unavailable) || (context.object).is_empty() {
            return Task::none();
        }
        self.bell_open = false;
        match context.target {
            BellTarget::Run => Task::done(AppMessage::OpenRunPanel(context.object.to_owned())),
            BellTarget::Page => {
                let pending_task = {
                    let target_anchor = context.anchor.to_owned();
                    Task::done(AppMessage::OpenPageSearchHit(
                        context.object.to_owned(),
                        target_anchor.clone(),
                    ))
                };
                self.bell_navigation_generation = self.bell_navigation_generation.wrapping_add(1);
                let request_generation = self.bell_navigation_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .bell_navigation_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::BellNavigationReply(request_generation, Box::new(reply_message))
                })
            }
            BellTarget::Message => Task::done(AppMessage::OpenMessageLink(
                crate::backend::bell_link(&(context), self.network_chain_id.to_owned()),
            )),
            BellTarget::Forge => Task::done(AppMessage::OpenMessageLink(
                crate::backend::bell_link(&(context), self.network_chain_id.to_owned()),
            )),
            BellTarget::Repo => Task::done(AppMessage::OpenMessageLink(crate::backend::bell_link(
                &(context),
                self.network_chain_id.to_owned(),
            ))),
            BellTarget::Unavailable => Task::none(),
        }
    }
    fn on_global_key_pressed(&mut self, event: crate::shell::KeyPress) -> Task<AppMessage> {
        let escape_key = crate::backend::escape_target(
            event.key.clone(),
            self.palette_open,
            self.bell_open,
            self.channel_create_open,
        );
        let palette_key = crate::backend::palette_key_action(
            event.key.clone(),
            event.modifiers,
            self.palette_open,
        );
        if (escape_key).is_empty() && (palette_key == "none") {
            return Task::none();
        }
        self.bell_open = self.bell_open && (escape_key != "bell");
        self.channel_create_open = self.channel_create_open && (escape_key != "channel_create");
        if palette_key == "none" {
            return Task::none();
        }
        if (palette_key == "open") && (!self.connected) {
            return Task::none();
        }
        self.palette_search_generation = self.palette_search_generation.wrapping_add(1);
        if let Some(previous_handle) = self.palette_search_task.take() {
            previous_handle.abort();
        }
        self.palette_open = palette_key == "open";
        self.palette_draft = "".to_owned();
        self.palette_chat_hits = Vec::new();
        self.palette_page_hits = Vec::new();
        self.palette_search_phase = SearchPhase::Idle;
        if !self.palette_open {
            return Task::none();
        }
        crate::shell::focus::<AppMessage>(
            "Ducktape/workspace-tabs/overlays/palette-input".to_owned(),
        )
    }
    fn on_palette_changed(&mut self, next: String) -> Task<AppMessage> {
        self.palette_search_generation = self.palette_search_generation.wrapping_add(1);
        if let Some(previous_handle) = self.palette_search_task.take() {
            previous_handle.abort();
        }
        self.palette_draft = next.to_owned();
        self.palette_search_phase = SearchPhase::Idle;
        self.palette_chat_hits = Vec::new();
        self.palette_page_hits = Vec::new();
        if ((self.palette_draft).trim().to_owned()).is_empty() {
            return Task::none();
        }
        self.palette_search_phase = SearchPhase::Searching;
        let pending_task = Task::perform(
            crate::backend::palette_search(
                self.connected_rpc.to_owned(),
                (self.palette_draft).trim().to_owned(),
            ),
            |result| match result {
                Ok(value) => AppMessage::PaletteResults(value),
                Err(error) => AppMessage::PaletteSearchFailed(error),
            },
        );
        self.palette_search_generation = self.palette_search_generation.wrapping_add(1);
        let request_generation = self.palette_search_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .palette_search_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::PaletteSearchReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_palette_results(&mut self, next: crate::backend::PaletteSearchData) -> Task<AppMessage> {
        self.palette_chat_hits = next.chat_hits.clone();
        self.palette_page_hits = next.page_hits.clone();
        self.palette_search_phase = SearchPhase::Done;
        Task::none()
    }
    fn on_palette_search_failed(&mut self, _cause: crate::backend::AppError) -> Task<AppMessage> {
        self.palette_search_phase = SearchPhase::Idle;
        self.palette_chat_hits = Vec::new();
        self.palette_page_hits = Vec::new();
        Task::none()
    }
    fn on_open_chat_search_hit(&mut self, channel_id: String, target_seq: i64) -> Task<AppMessage> {
        if self.mutation_phase != MutationPhase::Idle {
            return Task::none();
        }
        let next_channel = crate::backend::channel_switch_facts(
            self.channel_reads.clone(),
            self.channels.clone(),
            self.active_channel.to_owned(),
            channel_id.to_owned(),
            self.unread_boundary,
            self.active_channel_name.to_owned(),
        );
        self.unread_boundary = next_channel.unread_boundary;
        self.active_channel = channel_id.to_owned();
        self.chat_land_seq = target_seq;
        self.active_dm_peer = crate::backend::dm_peer_of_channel(
            self.active_dm_peer.to_owned(),
            self.dm_peers.clone(),
            self.active_channel.to_owned(),
        );
        self.active_dm =
            crate::backend::dm_peer_named(self.dm_peers.clone(), self.active_dm_peer.to_owned());
        self.active_channel_name = next_channel.name.to_owned();
        self.active_channel_archived = next_channel.archived;
        self.active_channel_members_only = next_channel.members_only;
        self.history_view = true;
        self.chat_at_tail = false;
        self.channel_members = Vec::new();
        let post_gate_known = !self.active_channel_members_only;
        self.post_refusal = crate::backend::keep_str(
            post_gate_known,
            &(crate::backend::post_gate(
                self.active_channel_archived,
                self.active_channel_members_only,
                self.channel_members.clone(),
                self.settings_user_key.to_owned(),
            )),
            "" ,
        );
        self.palette_open = false;
        self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_qr_auth_task.take() {
            previous_handle.abort();
        }
        self.account_desktop_auth_generation = self.account_desktop_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_desktop_auth_task.take() {
            previous_handle.abort();
        }
        self.account_busy = self.account_busy && (self.account_ceremony_phase).is_empty();
        self.account_ceremony_phase = "".to_owned();
        self.account_ceremony_qr = "".to_owned();
        self.account_ceremony_detail = "".to_owned();
        self.account_ceremony_left = "".to_owned();
        self.shell_tab = ShellTab::Chat;
        self.hydration_generation += 1;
        self.hydration_retry_attempt = 0;
        self.loading = true;
        self.error = "".to_owned();
        self.chat_generation += 1;
        let pending_task = Task::perform(
            crate::backend::load_channel_window(
                self.connected_rpc.to_owned(),
                self.active_channel.to_owned(),
                self.chat_generation,
            ),
            |result| match result {
                Ok(value) => AppMessage::ChatUpdated(value),
                Err(error) => AppMessage::ChatLoadFailed(error),
            },
        );
        self.channel_window_load_generation = self.channel_window_load_generation.wrapping_add(1);
        let request_generation = self.channel_window_load_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .channel_window_load_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::ChannelWindowLoadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_choose_channel(&mut self, id: String) -> Task<AppMessage> {
        if self.mutation_phase != MutationPhase::Idle {
            return Task::none();
        }
        self.active_dm_peer = "".to_owned();
        self.active_dm = crate::backend::no_dm_peer();
        self.history_view = false;
        self.chat_at_tail = true;
        self.chat_land_seq = 0;
        let next_channel = crate::backend::channel_switch_facts(
            self.channel_reads.clone(),
            self.channels.clone(),
            self.active_channel.to_owned(),
            id.to_owned(),
            self.unread_boundary,
            self.active_channel_name.to_owned(),
        );
        self.unread_boundary = next_channel.unread_boundary;
        self.active_channel = id.to_owned();
        self.active_channel_name = next_channel.name.to_owned();
        self.active_channel_archived = next_channel.archived;
        self.active_channel_members_only = next_channel.members_only;
        self.channel_members = Vec::new();
        let post_gate_known = !self.active_channel_members_only;
        self.post_refusal = crate::backend::keep_str(
            post_gate_known,
            &(crate::backend::post_gate(
                self.active_channel_archived,
                self.active_channel_members_only,
                self.channel_members.clone(),
                self.settings_user_key.to_owned(),
            )),
            "" ,
        );
        self.hydration_generation += 1;
        self.hydration_retry_attempt = 0;
        self.loading = true;
        self.error = "".to_owned();
        self.chat_generation += 1;
        let pending_task = Task::perform(
            crate::backend::load_channel_window(
                self.connected_rpc.to_owned(),
                self.active_channel.to_owned(),
                self.chat_generation,
            ),
            |result| match result {
                Ok(value) => AppMessage::ChatUpdated(value),
                Err(error) => AppMessage::ChatLoadFailed(error),
            },
        );
        self.channel_window_load_generation = self.channel_window_load_generation.wrapping_add(1);
        let request_generation = self.channel_window_load_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .channel_window_load_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::ChannelWindowLoadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_choose_dm(&mut self, peer_key: String) -> Task<AppMessage> {
        if (self.mutation_phase != MutationPhase::Idle) || (peer_key).is_empty() {
            return Task::none();
        }
        self.channel_window_load_generation = self.channel_window_load_generation.wrapping_add(1);
        if let Some(previous_handle) = self.channel_window_load_task.take() {
            previous_handle.abort();
        }
        self.active_dm_peer = peer_key.to_owned();
        self.active_dm =
            crate::backend::dm_peer_named(self.dm_peers.clone(), self.active_dm_peer.to_owned());
        let dm_room =
            crate::backend::dm_room_of_peer(self.dm_peers.clone(), self.active_dm_peer.to_owned());
        self.history_view = false;
        self.chat_at_tail = true;
        self.chat_land_seq = 0;
        let next_channel = crate::backend::channel_switch_facts(
            self.channel_reads.clone(),
            self.channels.clone(),
            self.active_channel.to_owned(),
            dm_room.to_owned(),
            self.unread_boundary,
            self.active_channel_name.to_owned(),
        );
        self.unread_boundary = next_channel.unread_boundary;
        self.active_channel = dm_room.to_owned();
        self.active_channel_name = next_channel.name.to_owned();
        self.active_channel_archived = next_channel.archived;
        self.active_channel_members_only = next_channel.members_only;
        self.channel_members = Vec::new();
        self.post_refusal = "".to_owned();
        self.hydration_generation += 1;
        self.hydration_retry_attempt = 0;
        self.loading = true;
        self.error = "".to_owned();
        self.chat_generation += 1;
        Task::perform(
            crate::backend::open_dm(
                self.connected_rpc.to_owned(),
                self.password.to_owned(),
                self.active_dm_peer.to_owned(),
                self.chat_generation,
            ),
            |result| match result {
                Ok(value) => AppMessage::ChatUpdated(value),
                Err(error) => AppMessage::ChatLoadFailed(error),
            },
        )
    }
    fn on_create_channel_submit(&mut self) -> Task<AppMessage> {
        if (self.loading || (self.mutation_phase != MutationPhase::Idle))
            || ((self.channel_draft).trim().to_owned()).is_empty()
        {
            return Task::none();
        }
        self.hydration_generation += 1;
        self.hydration_retry_attempt = 0;
        self.mutation_phase = MutationPhase::Channel;
        self.pending_channel = (self.channel_draft).trim().to_owned();
        self.channel_draft = "".to_owned();
        self.error = "".to_owned();
        self.chat_generation += 1;
        Task::perform(
            crate::backend::create_channel(
                self.connected_rpc.to_owned(),
                self.password.to_owned(),
                self.pending_channel.to_owned(),
                self.channel_create_members_only,
                self.chat_generation,
            ),
            |result| match result {
                Ok(value) => AppMessage::ChannelCreated(value),
                Err(error) => AppMessage::MutationFailed(error),
            },
        )
    }
    fn on_toggle_channel_create_members_only(&mut self) -> Task<AppMessage> {
        self.channel_create_members_only = !self.channel_create_members_only;
        Task::none()
    }
    fn on_toggle_channel_create(&mut self) -> Task<AppMessage> {
        self.channel_create_open = !self.channel_create_open;
        Task::none()
    }
    fn on_join_huddle_submit(&mut self) -> Task<AppMessage> {
        if ((self.loading || (self.mutation_phase != MutationPhase::Idle))
            || (self.active_channel).is_empty())
            || self.active_channel_archived
        {
            return Task::none();
        }
        self.hydration_generation += 1;
        self.hydration_retry_attempt = 0;
        self.mutation_phase = MutationPhase::Huddle;
        self.error = "".to_owned();
        Task::perform(
            crate::backend::join_huddle(
                self.connected_rpc.to_owned(),
                self.password.to_owned(),
                self.active_channel.to_owned(),
            ),
            |result| match result {
                Ok(value) => AppMessage::HuddleJoinedAck(value),
                Err(error) => AppMessage::MutationFailed(error),
            },
        )
    }
    fn on_huddle_joined_ack(&mut self, _result: bool) -> Task<AppMessage> {
        self.mutation_phase = MutationPhase::Idle;
        self.error = "".to_owned();
        self.huddle_joined = true;
        self.huddle_channel = self.active_channel.to_owned();
        self.huddle_channel_name = self.active_channel_name.to_owned();
        self.huddle_joined_at = self.huddle_now;
        self.chat_generation += 1;
        Task::batch([Task::done(AppMessage::ShowHuddle), {
            let pending_task = Task::perform(
                crate::backend::load_channel_window(
                    self.connected_rpc.to_owned(),
                    self.active_channel.to_owned(),
                    self.chat_generation,
                ),
                |result| match result {
                    Ok(value) => AppMessage::ChatUpdated(value),
                    Err(error) => AppMessage::ChatLoadFailed(error),
                },
            );
            self.channel_window_load_generation =
                self.channel_window_load_generation.wrapping_add(1);
            let request_generation = self.channel_window_load_generation;
            let (pending_task, request_handle) = pending_task.abortable();
            if let Some(previous_handle) = self
                .channel_window_load_task
                .replace(request_handle.abort_on_drop())
            {
                previous_handle.abort();
            }
            pending_task.map(move |reply_message| {
                AppMessage::ChannelWindowLoadReply(request_generation, Box::new(reply_message))
            })
        }])
    }
    fn on_chat_begin_edit(
        &mut self,
        scope: String,
        body: String,
        seq: i64,
        rev: i64,
    ) -> Task<AppMessage> {
        if (body).is_empty() || (seq <= 0) {
            return Task::none();
        }
        self.chat_edit_seq = seq;
        self.chat_edit_rev = rev;
        crate::module_view::chat_composer_seed(&(scope), &(body));
        Task::none()
    }
    fn on_composer_submitted(
        &mut self,
        kind: ComposerKind,
        pending_body: String,
        pending_id: String,
        scope: String,
    ) -> Task<AppMessage> {
        // the files the submit sent, already in DuckFS, and the body that links them
        let links = crate::composer_surface::take_attachments(&pending_id);
        let pending_body = crate::backend::attachment_body(pending_body, &links);
        match kind {
            ComposerKind::Message => {
                match crate::backend::submit_verdict(
                    self.loading,
                    self.connected,
                    self.active_channel.to_owned(),
                    self.post_refusal.to_owned(),
                    true,
                    scope.to_owned(),
                    crate::backend::composer_scope(&self.connected_rpc, &self.active_channel),
                ) {
                    SubmitVerdict::Refused => {
                        crate::module_view::chat_composer_unsent(&(scope), &(pending_body), false);
                        Task::none()
                    }
                    SubmitVerdict::Admitted => {
                        self.hydration_generation += 1;
                        self.hydration_retry_attempt = 0;
                        self.chat_pending_sends = crate::backend::send_pending(
                            ::std::mem::take(&mut self.chat_pending_sends),
                            pending_id.to_owned(),
                            pending_body.to_owned(),
                            0,
                        );
                        self.error = "".to_owned();
                        self.chat_at_tail = true;
                        self.history_view = false;
                        self.chat_sent_serial += 1;
                        Task::perform(
                            crate::backend::send_message(
                                self.connected_rpc.to_owned(),
                                self.password.to_owned(),
                                self.active_channel.to_owned(),
                                pending_id.to_owned(),
                                pending_body.to_owned(),
                            ),
                            |result| match result {
                                Ok(value) => AppMessage::MessageSent(value),
                                Err(error) => AppMessage::MessageSendFailed(error),
                            },
                        )
                    }
                }
            }
            ComposerKind::Reply => {
                let thread_seq = crate::backend::scope_thread_seq(&(scope));
                match crate::backend::submit_verdict(
                    false,
                    self.connected,
                    self.active_channel.to_owned(),
                    self.post_refusal.to_owned(),
                    thread_seq > 0,
                    scope.to_owned(),
                    crate::backend::thread_scope(
                        &self.connected_rpc,
                        &self.active_channel,
                        thread_seq,
                    ),
                ) {
                    SubmitVerdict::Refused => {
                        crate::module_view::chat_composer_unsent(&(scope), &(pending_body), false);
                        Task::none()
                    }
                    SubmitVerdict::Admitted => {
                        self.hydration_generation += 1;
                        self.hydration_retry_attempt = 0;
                        self.chat_pending_sends = crate::backend::send_pending(
                            ::std::mem::take(&mut self.chat_pending_sends),
                            pending_id.to_owned(),
                            pending_body.to_owned(),
                            thread_seq,
                        );
                        self.error = "".to_owned();
                        Task::perform(
                            crate::backend::send_reply(
                                self.connected_rpc.to_owned(),
                                self.password.to_owned(),
                                self.active_channel.to_owned(),
                                thread_seq,
                                pending_id.to_owned(),
                                pending_body.to_owned(),
                            ),
                            |result| match result {
                                Ok(value) => AppMessage::ThreadReplySent(value),
                                Err(error) => AppMessage::ThreadReplySendFailed(error),
                            },
                        )
                    }
                }
            }
            ComposerKind::Edit => {
                if scope
                    != crate::backend::edit_scope(
                        &self.connected_rpc,
                        &self.active_channel,
                        self.chat_edit_seq,
                    )
                {
                    return Task::none();
                }
                Task::done(AppMessage::EditMessageSubmit(
                    (pending_body).trim().to_owned(),
                ))
            }
            ComposerKind::ThreadEdit => {
                if scope
                    != crate::backend::edit_scope(
                        &self.connected_rpc,
                        &self.active_channel,
                        self.chat_edit_seq,
                    )
                {
                    return Task::none();
                }
                Task::done(AppMessage::EditMessageSubmit(
                    (pending_body).trim().to_owned(),
                ))
            }
        }
    }
    fn on_edit_message_submit(&mut self, text: String) -> Task<AppMessage> {
        if (((self.loading || (self.mutation_phase != MutationPhase::Idle))
            || (self.active_channel).is_empty())
            || (self.chat_edit_seq <= 0))
            || ((text).trim().to_owned()).is_empty()
        {
            return Task::none();
        }
        self.hydration_generation += 1;
        self.hydration_retry_attempt = 0;
        self.mutation_phase = MutationPhase::MessageEdit;
        self.error = "".to_owned();
        Task::perform(
            crate::backend::edit_message(
                self.connected_rpc.to_owned(),
                self.password.to_owned(),
                self.active_channel.to_owned(),
                self.chat_edit_seq,
                self.chat_edit_rev,
                (text).trim().to_owned(),
            ),
            |result| match result {
                Ok(value) => AppMessage::ChatAcked(value),
                Err(error) => AppMessage::MutationFailed(error),
            },
        )
    }
    fn on_message_sent(&mut self, next: crate::backend::SendReceipt) -> Task<AppMessage> {
        self.chat_pending_sends = crate::backend::send_settled(
            ::std::mem::take(&mut self.chat_pending_sends),
            &(next.operation_id),
        );
        if self.active_channel != next.channel_id {
            return Task::none();
        }
        self.error = "".to_owned();
        Task::none()
    }
    fn on_message_send_failed(
        &mut self,
        cause: crate::backend::OptimisticMutationError,
    ) -> Task<AppMessage> {
        self.error = cause.message.to_owned();
        self.chat_pending_sends = crate::backend::send_failed(
            ::std::mem::take(&mut self.chat_pending_sends),
            &(cause.operation_id),
            cause.committed,
        );
        crate::module_view::chat_composer_unsent(
            &(crate::backend::composer_scope(&self.connected_rpc, &(cause.scope_id))),
            &(cause.body),
            cause.committed,
        );
        if (self.active_channel != cause.scope_id) || (!cause.committed) {
            return Task::none();
        }
        self.hydration_generation += 1;
        self.hydration_retry_attempt = 0;
        let pending_task = Task::perform(
            crate::backend::live_resync_load(
                self.connected_rpc.to_owned(),
                self.active_channel.to_owned(),
                true,
                false,
                self.hydration_generation,
                0,
            ),
            |result| match result {
                Ok(value) => AppMessage::LiveResynced(value),
                Err(error) => AppMessage::LiveResyncFailed(error),
            },
        );
        self.live_resync_generation = self.live_resync_generation.wrapping_add(1);
        let request_generation = self.live_resync_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .live_resync_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::LiveResyncReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_thread_reply_send_failed(
        &mut self,
        cause: crate::backend::OptimisticMutationError,
    ) -> Task<AppMessage> {
        self.error = cause.message.to_owned();
        self.chat_pending_sends = crate::backend::send_failed(
            ::std::mem::take(&mut self.chat_pending_sends),
            &(cause.operation_id),
            cause.committed,
        );
        crate::module_view::chat_composer_unsent(
            &(crate::backend::thread_scope(
                &self.connected_rpc,
                &(cause.scope_id),
                cause.thread_seq,
            )),
            &(cause.body),
            cause.committed,
        );
        if (self.active_channel != cause.scope_id) || (!cause.committed) {
            return Task::none();
        }
        self.hydration_generation += 1;
        self.hydration_retry_attempt = 0;
        let pending_task = Task::perform(
            crate::backend::live_resync_load(
                self.connected_rpc.to_owned(),
                self.active_channel.to_owned(),
                true,
                false,
                self.hydration_generation,
                0,
            ),
            |result| match result {
                Ok(value) => AppMessage::LiveResynced(value),
                Err(error) => AppMessage::LiveResyncFailed(error),
            },
        );
        self.live_resync_generation = self.live_resync_generation.wrapping_add(1);
        let request_generation = self.live_resync_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .live_resync_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::LiveResyncReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_thread_reply_sent(&mut self, next: crate::backend::SendReceipt) -> Task<AppMessage> {
        self.chat_pending_sends = crate::backend::send_settled(
            ::std::mem::take(&mut self.chat_pending_sends),
            &(next.operation_id),
        );
        if self.active_channel != next.channel_id {
            return Task::none();
        }
        self.error = "".to_owned();
        Task::none()
    }
    fn on_chat_updated(&mut self, next: crate::backend::ChatData) -> Task<AppMessage> {
        if next.generation != self.chat_generation {
            return Task::none();
        }
        self.channels = crate::backend::upsert_channel_rows(
            ::std::mem::take(&mut self.channels),
            next.channels.clone(),
        );
        self.unread_boundary = crate::backend::frozen_unread_boundary(
            self.channel_reads.clone(),
            self.channels.clone(),
            self.active_channel.to_owned(),
            next.active_channel.to_owned(),
            self.unread_boundary,
        );
        self.channel_reads = crate::backend::mark_channel_read(
            ::std::mem::take(&mut self.channel_reads),
            next.active_channel.to_owned(),
            crate::backend::channel_head_seq(self.channels.clone(), next.active_channel.to_owned()),
        );
        self.rooms = crate::backend::chat_sidebar_rooms(
            self.channels.clone(),
            self.dm_peers.clone(),
            self.channel_reads.clone(),
        );
        self.dm_rows = crate::backend::chat_sidebar_dms(
            self.channels.clone(),
            self.dm_peers.clone(),
            self.channel_reads.clone(),
        );
        let landed_elsewhere = self.active_channel != next.active_channel;
        self.history_view = self.history_view && (!landed_elsewhere);
        self.chat_land_seq = crate::backend::keep_i64(landed_elsewhere, 0, self.chat_land_seq);
        self.active_channel = next.active_channel.to_owned();
        self.active_dm_peer = crate::backend::dm_peer_of_channel(
            self.active_dm_peer.to_owned(),
            self.dm_peers.clone(),
            self.active_channel.to_owned(),
        );
        self.active_dm =
            crate::backend::dm_peer_named(self.dm_peers.clone(), self.active_dm_peer.to_owned());
        self.active_channel_name = next.active_channel_name.to_owned();
        self.active_channel_archived = next.active_channel_archived;
        self.active_channel_members_only = next.active_channel_members_only;
        self.huddle_joined_at =
            crate::backend::keep_i64(self.huddle_joined, self.huddle_joined_at, self.huddle_now);
        let huddle = crate::backend::huddle_after_load(
            true,
            self.huddle_joined,
            self.huddle_channel.to_owned(),
            self.huddle_channel_name.to_owned(),
            self.huddle_roster.clone(),
            self.active_channel.to_owned(),
            self.active_channel_name.to_owned(),
            next.huddle_roster.clone(),
        );
        self.huddle_joined = huddle.joined;
        self.huddle_roster = huddle.roster.clone();
        self.huddle_rows = crate::call::huddle_tile_rows(
            self.huddle_roster.clone(),
            self.call_peers.clone(),
            self.call_muted,
            self.call_speaking,
        );
        self.huddle_channel = huddle.channel.to_owned();
        self.huddle_channel_name = huddle.channel_name.to_owned();
        self.channel_members = next.channel_members.clone();
        crate::module_view::chat_composer_roster(
            &(crate::backend::composer_scope(&self.connected_rpc, &self.active_channel)),
            &self.channel_members,
        );
        self.post_refusal = crate::backend::post_gate(
            self.active_channel_archived,
            self.active_channel_members_only,
            self.channel_members.clone(),
            self.settings_user_key.to_owned(),
        );
        self.loading = false;
        self.error = "".to_owned();
        crate::shell::close::<AppMessage>(crate::backend::window_target_unless(
            self.huddle_joined,
            self.huddle_win,
        ))
    }
    fn on_chat_load_failed(&mut self, cause: crate::backend::HydrationError) -> Task<AppMessage> {
        if cause.generation != self.chat_generation {
            return Task::none();
        }
        self.hydration_generation += 1;
        self.hydration_retry_attempt = 0;
        self.loading = false;
        self.error = cause.message.to_owned();
        Task::none()
    }
    fn on_channel_created(&mut self, next: crate::backend::ChatData) -> Task<AppMessage> {
        self.pending_channel = "".to_owned();
        self.channel_create_open = false;
        self.channel_create_members_only = false;
        self.mutation_phase = MutationPhase::Idle;
        if next.generation != self.chat_generation {
            return Task::none();
        }
        self.history_view = false;
        self.chat_at_tail = true;
        self.chat_land_seq = 0;
        self.channels = crate::backend::upsert_channel_rows(
            ::std::mem::take(&mut self.channels),
            next.channels.clone(),
        );
        self.unread_boundary = crate::backend::frozen_unread_boundary(
            self.channel_reads.clone(),
            self.channels.clone(),
            self.active_channel.to_owned(),
            next.active_channel.to_owned(),
            self.unread_boundary,
        );
        self.channel_reads = crate::backend::mark_channel_read(
            ::std::mem::take(&mut self.channel_reads),
            next.active_channel.to_owned(),
            crate::backend::channel_head_seq(self.channels.clone(), next.active_channel.to_owned()),
        );
        self.rooms = crate::backend::chat_sidebar_rooms(
            self.channels.clone(),
            self.dm_peers.clone(),
            self.channel_reads.clone(),
        );
        self.dm_rows = crate::backend::chat_sidebar_dms(
            self.channels.clone(),
            self.dm_peers.clone(),
            self.channel_reads.clone(),
        );
        self.active_channel = next.active_channel.to_owned();
        self.active_dm_peer = crate::backend::dm_peer_of_channel(
            self.active_dm_peer.to_owned(),
            self.dm_peers.clone(),
            self.active_channel.to_owned(),
        );
        self.active_dm =
            crate::backend::dm_peer_named(self.dm_peers.clone(), self.active_dm_peer.to_owned());
        self.active_channel_name = next.active_channel_name.to_owned();
        self.active_channel_archived = next.active_channel_archived;
        self.active_channel_members_only = next.active_channel_members_only;
        self.huddle_joined_at =
            crate::backend::keep_i64(self.huddle_joined, self.huddle_joined_at, self.huddle_now);
        let huddle = crate::backend::huddle_after_load(
            true,
            self.huddle_joined,
            self.huddle_channel.to_owned(),
            self.huddle_channel_name.to_owned(),
            self.huddle_roster.clone(),
            self.active_channel.to_owned(),
            self.active_channel_name.to_owned(),
            next.huddle_roster.clone(),
        );
        self.huddle_joined = huddle.joined;
        self.huddle_roster = huddle.roster.clone();
        self.huddle_rows = crate::call::huddle_tile_rows(
            self.huddle_roster.clone(),
            self.call_peers.clone(),
            self.call_muted,
            self.call_speaking,
        );
        self.huddle_channel = huddle.channel.to_owned();
        self.huddle_channel_name = huddle.channel_name.to_owned();
        self.channel_members = next.channel_members.clone();
        crate::module_view::chat_composer_roster(
            &(crate::backend::composer_scope(&self.connected_rpc, &self.active_channel)),
            &self.channel_members,
        );
        self.post_refusal = crate::backend::post_gate(
            self.active_channel_archived,
            self.active_channel_members_only,
            self.channel_members.clone(),
            self.settings_user_key.to_owned(),
        );
        self.error = "".to_owned();
        crate::shell::close::<AppMessage>(crate::backend::window_target_unless(
            self.huddle_joined,
            self.huddle_win,
        ))
    }
    fn on_live_agents_event(&mut self, next: crate::backend::LiveAgentNotice) -> Task<AppMessage> {
        if crate::backend::live_agents_stale(
            &(next),
            &self.connected_rpc,
            &self.network_chain_id,
            self.connect_generation,
            &self.signer_key,
        ) {
            return Task::none();
        }
        self.live_agents = next.rows.clone();
        Task::none()
    }
    fn on_live_cancel_acked(&mut self, _ok: bool) -> Task<AppMessage> {
        self.error = "".to_owned();
        Task::none()
    }
    fn on_chat_acked(&mut self, _result: bool) -> Task<AppMessage> {
        self.chat_edit_seq = 0;
        self.chat_edit_rev = 0;
        self.pending_channel = "".to_owned();
        self.channel_create_open = false;
        self.mutation_phase = MutationPhase::Idle;
        self.error = "".to_owned();
        Task::none()
    }
    fn on_copy_message_link(&mut self, link: String) -> Task<AppMessage> {
        if (link).is_empty() {
            return Task::none();
        }
        let confirmation = "Message link copied".to_owned();
        Task::done(AppMessage::CopyToClipboard(
            link.to_owned(),
            confirmation.clone(),
        ))
    }
    fn on_open_message_link(&mut self, url: String) -> Task<AppMessage> {
        if (url).is_empty() {
            return Task::none();
        }
        let link =
            crate::backend::resolve_duck_link(url.to_owned(), self.network_chain_id.to_owned());
        match link.kind {
            DuckKind::Unknown => {
                self.error = "this link names nothing the app can open".to_owned();
                Task::none()
            }
            DuckKind::ForeignNetwork => {
                self.error = crate::backend::foreign_network_error(
                    link.net.to_owned(),
                    self.network_chain_id.to_owned(),
                );
                Task::none()
            }
            DuckKind::Web => Task::perform(
                crate::backend::open_external_url(url.to_owned()),
                |result| result.err().map(AppMessage::ExternalUrlFailed),
            )
            .and_then(Task::done),
            DuckKind::Page => {
                let target_block = link.block.to_owned();
                Task::done(AppMessage::OpenPageSearchHit(
                    link.page.to_owned(),
                    target_block.clone(),
                ))
            }
            DuckKind::Run => Task::done(AppMessage::OpenRunPanel(link.dispatch.to_owned())),
            DuckKind::Files => {
                self.fs_route = link.path.to_owned();
                self.fs_route_serial += 1;
                self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_qr_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_desktop_auth_generation =
                    self.account_desktop_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_desktop_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_busy = self.account_busy && (self.account_ceremony_phase).is_empty();
                self.account_ceremony_phase = "".to_owned();
                self.account_ceremony_qr = "".to_owned();
                self.account_ceremony_detail = "".to_owned();
                self.account_ceremony_left = "".to_owned();
                self.shell_tab = ShellTab::Files;
                Task::none()
            }
            DuckKind::ForgeRepo => {
                self.forge_link = url.to_owned();
                self.forge_link_tick += 1;
                self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_qr_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_desktop_auth_generation =
                    self.account_desktop_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_desktop_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_busy = self.account_busy && (self.account_ceremony_phase).is_empty();
                self.account_ceremony_phase = "".to_owned();
                self.account_ceremony_qr = "".to_owned();
                self.account_ceremony_detail = "".to_owned();
                self.account_ceremony_left = "".to_owned();
                self.shell_tab = ShellTab::Forge;
                Task::none()
            }
            DuckKind::ForgeItem => {
                self.forge_link = url.to_owned();
                self.forge_link_tick += 1;
                self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_qr_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_desktop_auth_generation =
                    self.account_desktop_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_desktop_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_busy = self.account_busy && (self.account_ceremony_phase).is_empty();
                self.account_ceremony_phase = "".to_owned();
                self.account_ceremony_qr = "".to_owned();
                self.account_ceremony_detail = "".to_owned();
                self.account_ceremony_left = "".to_owned();
                self.shell_tab = ShellTab::Forge;
                Task::none()
            }
            DuckKind::ForgeBlob => {
                self.forge_link = url.to_owned();
                self.forge_link_tick += 1;
                self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_qr_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_desktop_auth_generation =
                    self.account_desktop_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_desktop_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_busy = self.account_busy && (self.account_ceremony_phase).is_empty();
                self.account_ceremony_phase = "".to_owned();
                self.account_ceremony_qr = "".to_owned();
                self.account_ceremony_detail = "".to_owned();
                self.account_ceremony_left = "".to_owned();
                self.shell_tab = ShellTab::Forge;
                Task::none()
            }
            DuckKind::Channel => {
                self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_qr_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_desktop_auth_generation =
                    self.account_desktop_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_desktop_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_busy = self.account_busy && (self.account_ceremony_phase).is_empty();
                self.account_ceremony_phase = "".to_owned();
                self.account_ceremony_qr = "".to_owned();
                self.account_ceremony_detail = "".to_owned();
                self.account_ceremony_left = "".to_owned();
                self.shell_tab = ShellTab::Chat;
                Task::done(AppMessage::ChooseChannel(link.channel.to_owned()))
            }
            DuckKind::ChannelMessage => {
                let message_sequence = link.seq;
                Task::done(AppMessage::OpenChatSearchHit(
                    link.channel.to_owned(),
                    message_sequence,
                ))
            }
            DuckKind::Account => {
                self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_qr_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_desktop_auth_generation =
                    self.account_desktop_auth_generation.wrapping_add(1);
                if let Some(previous_handle) = self.account_desktop_auth_task.take() {
                    previous_handle.abort();
                }
                self.account_busy = self.account_busy && (self.account_ceremony_phase).is_empty();
                self.account_ceremony_phase = "".to_owned();
                self.account_ceremony_qr = "".to_owned();
                self.account_ceremony_detail = "".to_owned();
                self.account_ceremony_left = "".to_owned();
                self.shell_tab = ShellTab::Chat;
                Task::done(AppMessage::ChooseDm(link.account.to_owned()))
            }
        }
    }
    fn on_chat_scrolled(
        &mut self,
        _absolute_x: f64,
        _absolute_y: f64,
        _relative_x: f64,
        relative_y: f64,
    ) -> Task<AppMessage> {
        self.chat_at_tail = crate::backend::near_scroll_tail(relative_y);
        self.history_view = (!self.chat_at_tail) || (self.chat_land_seq > 0);
        Task::none()
    }
    fn on_copy_chord_pressed(&mut self, event: crate::shell::KeyPress) -> Task<AppMessage> {
        if !crate::backend::is_copy_chord(event.key.clone(), event.modifiers) {
            return Task::none();
        }
        if self.shell_tab != ShellTab::Chat {
            return Task::none();
        }
        self.chat_copy_chord_serial += 1;
        Task::none()
    }
    fn on_chat_view_event(
        &mut self,
        event: crate::module_view::ModuleViewEvent,
    ) -> Task<AppMessage> {
        match crate::module_view::chat_intent(&(event)) {
            ChatIntent::OpenHit => {
                let target_seq = crate::module_view::event_int(&(event), "target_seq" );
                let target_sequence = target_seq;
                Task::done(AppMessage::OpenChatSearchHit(
                    crate::module_view::event_text(&(event), "channel" ),
                    target_sequence,
                ))
            }
            ChatIntent::ToggleCreate => Task::done(AppMessage::ToggleChannelCreate),
            ChatIntent::ChooseChannel => Task::done(AppMessage::ChooseChannel(
                crate::module_view::event_text(&(event), "id" ),
            )),
            ChatIntent::ChooseDm => Task::done(AppMessage::ChooseDm(
                crate::module_view::event_text(&(event), "key" ),
            )),
            ChatIntent::ShowHuddle => Task::done(AppMessage::ShowHuddle),
            ChatIntent::LeaveHuddle => Task::done(AppMessage::LeaveHuddleHere),
            ChatIntent::JoinHuddle => Task::done(AppMessage::JoinHuddleSubmit),
            ChatIntent::Scrolled => {
                let absolute_y = crate::module_view::event_num(&(event), "absolute_y" );
                let relative_x = crate::module_view::event_num(&(event), "relative_x" );
                let relative_y = crate::module_view::event_num(&(event), "relative_y" );
                let pointer_absolute_y = absolute_y;
                let pointer_relative_x = relative_x;
                let pointer_relative_y = relative_y;
                Task::done(AppMessage::ChatScrolled(
                    crate::module_view::event_num(&(event), "absolute_x" ),
                    pointer_absolute_y,
                    pointer_relative_x,
                    pointer_relative_y,
                ))
            }
            ChatIntent::OpenLink => Task::done(AppMessage::OpenMessageLink(
                crate::module_view::event_text(&(event), "url" ),
            )),
            ChatIntent::Copy => {
                let label = crate::module_view::event_text(&(event), "label" );
                let clipboard_label = label.to_owned();
                Task::done(AppMessage::CopyToClipboard(
                    crate::module_view::event_text(&(event), "text" ),
                    clipboard_label.clone(),
                ))
            }
            ChatIntent::CopyLink => Task::done(AppMessage::CopyMessageLink(
                crate::module_view::event_text(&(event), "link" ),
            )),
            ChatIntent::BeginEdit => {
                let body = crate::module_view::event_text(&(event), "body" );
                let seq = crate::module_view::event_int(&(event), "seq" );
                let rev = crate::module_view::event_int(&(event), "rev" );
                let edit_body = body.to_owned();
                let edit_sequence = seq;
                let edit_revision = rev;
                Task::done(AppMessage::ChatBeginEdit(
                    crate::module_view::event_text(&(event), "scope" ),
                    edit_body.clone(),
                    edit_sequence,
                    edit_revision,
                ))
            }
            ChatIntent::CancelRun => Task::perform(
                crate::backend::cancel_agent_run(
                    self.connected_rpc.to_owned(),
                    self.password.to_owned(),
                    crate::module_view::event_text(&(event), "run_id" ),
                ),
                |result| match result {
                    Ok(value) => AppMessage::LiveCancelAcked(value),
                    Err(error) => AppMessage::MutationFailed(error),
                },
            ),
            ChatIntent::OpenRun => Task::done(AppMessage::OpenRunPanel(
                crate::module_view::event_text(&(event), "dispatch_id" ),
            )),
            ChatIntent::Attach => Task::done(AppMessage::AttachmentQueued(
                crate::module_view::event_text(&(event), "scope"),
                crate::module_view::event_text(&(event), "id"),
                crate::module_view::event_text(&(event), "path"),
            )),
            ChatIntent::Composer => {
                let kind = crate::module_view::chat_event_kind(&(event));
                let id = crate::module_view::event_text(&(event), "id" );
                let scope = crate::module_view::event_text(&(event), "scope" );
                let reaction_kind = kind;
                let reaction_id = id.to_owned();
                let reaction_scope = scope.to_owned();
                Task::done(AppMessage::ComposerSubmitted(
                    reaction_kind,
                    crate::module_view::event_text(&(event), "body" ),
                    reaction_id.clone(),
                    reaction_scope.clone(),
                ))
            }
        }
    }
    fn on_pages_view_event(
        &mut self,
        event: crate::module_view::ModuleViewEvent,
    ) -> Task<AppMessage> {
        match crate::module_view::pages_intent(&(event)) {
            PagesIntent::OpenLink => Task::done(AppMessage::OpenMessageLink(
                crate::module_view::event_text(&(event), "link" ),
            )),
            PagesIntent::Copy => {
                self.toast = crate::module_view::event_text(&(event), "label" );
                self.toast_age = 0;
                crate::shell::clipboard::<AppMessage>(crate::module_view::event_text(
                    &(event),
                    "text" ,
                ))
            }
        }
    }
    fn on_open_page_search_hit(&mut self, page_id: String, _block_id: String) -> Task<AppMessage> {
        if (page_id).is_empty() {
            return Task::none();
        }
        self.palette_open = false;
        self.shell_tab = ShellTab::Pages;
        self.page_route = page_id.to_owned();
        self.page_route_serial += 1;
        Task::none()
    }
    fn on_external_url_failed(&mut self, cause: crate::backend::AppError) -> Task<AppMessage> {
        self.error = cause.message.to_owned();
        Task::none()
    }
    fn on_onboarding_opened(&mut self, id: crate::shell::WindowKey) -> Task<AppMessage> {
        self.onboarding_win = Some(id);
        Task::batch([
            {
                let pending_task = Task::perform(crate::backend::load_appearance(), |value| {
                    AppMessage::AppearanceLoaded(value)
                });
                self.appearance_load_generation = self.appearance_load_generation.wrapping_add(1);
                let request_generation = self.appearance_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .appearance_load_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::AppearanceLoadReply(request_generation, Box::new(reply_message))
                })
            },
            {
                let pending_task =
                    Task::perform(crate::backend::load_desktop_notifications(), |value| {
                        AppMessage::DesktopNotificationsLoaded(value)
                    });
                self.notifications_load_generation =
                    self.notifications_load_generation.wrapping_add(1);
                let request_generation = self.notifications_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .notifications_load_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::NotificationsLoadReply(request_generation, Box::new(reply_message))
                })
            },
            {
                let pending_task = Task::perform(crate::backend::hub_state(), |value| {
                    AppMessage::HubBooted(value)
                });
                self.hub_load_generation = self.hub_load_generation.wrapping_add(1);
                let request_generation = self.hub_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) =
                    self.hub_load_task.replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::HubLoadReply(request_generation, Box::new(reply_message))
                })
            },
        ])
    }
    fn on_hub_booted(&mut self, state: crate::backend::HubState) -> Task<AppMessage> {
        self.hub_networks = state.networks.clone();
        self.hub_selected = state.preselect.to_owned();
        self.hub_step = HubStep::Networks;
        self.onboarding_error = "".to_owned();
        let pending_task = Task::run(crate::backend::probe_known_networks(), |value| {
            AppMessage::NetworkProbed(value)
        });
        self.network_probe_generation = self.network_probe_generation.wrapping_add(1);
        let request_generation = self.network_probe_generation;
        let pending_task = pending_task
            .map(move |reply_message| {
                AppMessage::NetworkProbeReply(request_generation, Some(Box::new(reply_message)))
            })
            .chain(Task::done(AppMessage::NetworkProbeReply(
                request_generation,
                None,
            )));
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .network_probe_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task
    }
    fn on_hub_refreshed(&mut self, state: crate::backend::HubState) -> Task<AppMessage> {
        self.hub_networks = state.networks.clone();
        self.hub_selected = crate::backend::refreshed_hub_selection(
            state.networks.clone(),
            self.hub_selected.to_owned(),
            state.preselect.to_owned(),
        );
        let pending_task = Task::run(crate::backend::probe_known_networks(), |value| {
            AppMessage::NetworkProbed(value)
        });
        self.network_probe_generation = self.network_probe_generation.wrapping_add(1);
        let request_generation = self.network_probe_generation;
        let pending_task = pending_task
            .map(move |reply_message| {
                AppMessage::NetworkProbeReply(request_generation, Some(Box::new(reply_message)))
            })
            .chain(Task::done(AppMessage::NetworkProbeReply(
                request_generation,
                None,
            )));
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .network_probe_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task
    }
    fn on_network_probed(&mut self, probe: crate::backend::HubProbe) -> Task<AppMessage> {
        self.hub_networks = crate::backend::apply_network_probe(
            ::std::mem::take(&mut self.hub_networks),
            probe.clone(),
        );
        Task::none()
    }
    fn on_pick_wallet(&mut self, name: String) -> Task<AppMessage> {
        self.hub_wallet_selected = name.to_owned();
        Task::none()
    }
    fn on_unlock_submit(&mut self, pw: String) -> Task<AppMessage> {
        if ((self.mutation_phase != MutationPhase::Idle) || (pw).is_empty())
            || (self.hub_wallet_selected).is_empty()
        {
            return Task::none();
        }
        self.onboarding_error = "".to_owned();
        self.password = pw.to_owned();
        self.mutation_phase = MutationPhase::Onboarding;
        Task::perform(
            crate::backend::unlock_wallet(
                self.rpc.to_owned(),
                self.hub_wallet_selected.to_owned(),
                self.password.to_owned(),
            ),
            |result| match result {
                Ok(value) => AppMessage::KeyUnlocked(value),
                Err(error) => AppMessage::LoginFailed(error),
            },
        )
    }
    fn on_key_unlocked(&mut self, pubkey: String) -> Task<AppMessage> {
        self.onboarding_error = "".to_owned();
        self.signer_key = pubkey.to_owned();
        self.live_agents = Vec::new();
        Task::batch([
            {
                let pending_task = Task::perform(
                    crate::backend::load_account(self.rpc.to_owned(), self.account_generation),
                    |result| match result {
                        Ok(value) => AppMessage::AccountProbed(value),
                        Err(error) => AppMessage::AccountProbeFailed(error),
                    },
                );
                self.welcome_account_load_generation =
                    self.welcome_account_load_generation.wrapping_add(1);
                let request_generation = self.welcome_account_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .welcome_account_load_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::WelcomeAccountLoadReply(request_generation, Box::new(reply_message))
                })
            },
            {
                let pending_task =
                    Task::perform(crate::backend::chain_id_of(self.rpc.to_owned()), |result| {
                        match result {
                            Ok(value) => AppMessage::ChainNamed(value),
                            Err(error) => AppMessage::ChainProbeFailed(error),
                        }
                    });
                self.chain_identity_load_generation =
                    self.chain_identity_load_generation.wrapping_add(1);
                let request_generation = self.chain_identity_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .chain_identity_load_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::ChainIdentityLoadReply(request_generation, Box::new(reply_message))
                })
            },
        ])
    }
    fn on_login_skip(&mut self) -> Task<AppMessage> {
        if self.mutation_phase != MutationPhase::Idle {
            return Task::none();
        }
        self.password = "".to_owned();
        self.hub_wallet_selected = "".to_owned();
        self.onboarding_error = "".to_owned();
        Task::done(AppMessage::NetworkEntered)
    }
    fn on_password_submit(&mut self, pw: String) -> Task<AppMessage> {
        if (self.mutation_phase != MutationPhase::Idle) || (pw).is_empty() {
            return Task::none();
        }
        self.onboarding_error = "".to_owned();
        self.password = pw.to_owned();
        self.mutation_phase = MutationPhase::Onboarding;
        Task::perform(
            crate::backend::create_device_key(self.rpc.to_owned(), self.password.to_owned()),
            |result| match result {
                Ok(value) => AppMessage::DeviceKeyCreated(value),
                Err(error) => AppMessage::LoginFailed(error),
            },
        )
    }
    fn on_device_key_created(&mut self, _name: String) -> Task<AppMessage> {
        self.mutation_phase = MutationPhase::Idle;
        self.hub_step = HubStep::Phrase;
        Task::none()
    }
    fn on_phrase_written_down(&mut self) -> Task<AppMessage> {
        if self.mutation_phase != MutationPhase::Idle {
            return Task::none();
        }
        self.onboarding_error = "".to_owned();
        self.hub_step = HubStep::Confirm;
        Task::none()
    }
    fn on_show_phrase_again(&mut self) -> Task<AppMessage> {
        if self.mutation_phase != MutationPhase::Idle {
            return Task::none();
        }
        self.onboarding_error = "".to_owned();
        self.hub_step = HubStep::Phrase;
        Task::none()
    }
    fn on_confirm_phrase_submit(&mut self, answer: String) -> Task<AppMessage> {
        if (self.mutation_phase != MutationPhase::Idle) || ((answer).trim().to_owned()).is_empty() {
            return Task::none();
        }
        self.onboarding_error = "".to_owned();
        self.mutation_phase = MutationPhase::Onboarding;
        Task::perform(
            crate::backend::confirm_recovery_phrase(
                self.rpc.to_owned(),
                answer.to_owned(),
                self.password.to_owned(),
            ),
            |result| match result {
                Ok(value) => AppMessage::PhraseConfirmed(value),
                Err(error) => AppMessage::PhraseConfirmFailed(error),
            },
        )
    }
    fn on_phrase_confirmed(&mut self, pubkey: String) -> Task<AppMessage> {
        self.onboarding_error = "".to_owned();
        self.signer_key = pubkey.to_owned();
        self.live_agents = Vec::new();
        Task::batch([
            {
                let pending_task = Task::perform(
                    crate::backend::load_account(self.rpc.to_owned(), self.account_generation),
                    |result| match result {
                        Ok(value) => AppMessage::AccountProbed(value),
                        Err(error) => AppMessage::AccountProbeFailed(error),
                    },
                );
                self.welcome_account_load_generation =
                    self.welcome_account_load_generation.wrapping_add(1);
                let request_generation = self.welcome_account_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .welcome_account_load_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::WelcomeAccountLoadReply(request_generation, Box::new(reply_message))
                })
            },
            {
                let pending_task =
                    Task::perform(crate::backend::chain_id_of(self.rpc.to_owned()), |result| {
                        match result {
                            Ok(value) => AppMessage::ChainNamed(value),
                            Err(error) => AppMessage::ChainProbeFailed(error),
                        }
                    });
                self.chain_identity_load_generation =
                    self.chain_identity_load_generation.wrapping_add(1);
                let request_generation = self.chain_identity_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .chain_identity_load_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::ChainIdentityLoadReply(request_generation, Box::new(reply_message))
                })
            },
        ])
    }
    fn on_phrase_confirm_failed(&mut self, cause: crate::backend::AppError) -> Task<AppMessage> {
        self.mutation_phase = MutationPhase::Idle;
        self.onboarding_error = cause.message.to_owned();
        Task::none()
    }
    fn on_go_restore(&mut self) -> Task<AppMessage> {
        if self.mutation_phase != MutationPhase::Idle {
            return Task::none();
        }
        self.secrets.clear("restore_words");
        self.onboarding_error = "".to_owned();
        self.hub_step = HubStep::Restore;
        Task::none()
    }
    fn on_go_login(&mut self) -> Task<AppMessage> {
        if self.mutation_phase != MutationPhase::Idle {
            return Task::none();
        }
        self.secrets.clear("restore_words");
        self.onboarding_error = "".to_owned();
        self.hub_step = crate::backend::hub_entry_step(self.hub_wallets.clone());
        Task::none()
    }
    fn on_restore_submit(&mut self, name: String, pw: String) -> Task<AppMessage> {
        if (((self.mutation_phase != MutationPhase::Idle)
            || (self.secrets.text("restore_words")).is_empty())
            || (pw).is_empty())
            || (name).is_empty()
        {
            return Task::none();
        }
        self.onboarding_error = "".to_owned();
        self.password = pw.to_owned();
        self.hub_wallet_selected = name.to_owned();
        self.mutation_phase = MutationPhase::Onboarding;
        Task::perform(
            crate::backend::restore_user_key(
                self.rpc.to_owned(),
                name.to_owned(),
                self.secrets.read("restore_words"),
                self.password.to_owned(),
            ),
            |result| match result {
                Ok(value) => AppMessage::KeyRestored(value),
                Err(error) => AppMessage::LoginFailed(error),
            },
        )
    }
    fn on_key_restored(&mut self, pubkey: String) -> Task<AppMessage> {
        self.secrets.clear("restore_words");
        self.onboarding_error = "".to_owned();
        self.signer_key = pubkey.to_owned();
        self.live_agents = Vec::new();
        Task::batch([
            {
                let pending_task = Task::perform(
                    crate::backend::load_account(self.rpc.to_owned(), self.account_generation),
                    |result| match result {
                        Ok(value) => AppMessage::AccountProbed(value),
                        Err(error) => AppMessage::AccountProbeFailed(error),
                    },
                );
                self.welcome_account_load_generation =
                    self.welcome_account_load_generation.wrapping_add(1);
                let request_generation = self.welcome_account_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .welcome_account_load_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::WelcomeAccountLoadReply(request_generation, Box::new(reply_message))
                })
            },
            {
                let pending_task =
                    Task::perform(crate::backend::chain_id_of(self.rpc.to_owned()), |result| {
                        match result {
                            Ok(value) => AppMessage::ChainNamed(value),
                            Err(error) => AppMessage::ChainProbeFailed(error),
                        }
                    });
                self.chain_identity_load_generation =
                    self.chain_identity_load_generation.wrapping_add(1);
                let request_generation = self.chain_identity_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .chain_identity_load_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::ChainIdentityLoadReply(request_generation, Box::new(reply_message))
                })
            },
        ])
    }
    fn on_login_failed(&mut self, cause: crate::backend::AppError) -> Task<AppMessage> {
        self.mutation_phase = MutationPhase::Idle;
        self.password = "".to_owned();
        self.onboarding_error = cause.message.to_owned();
        Task::none()
    }
    fn on_pick_network(&mut self, id: String) -> Task<AppMessage> {
        self.hub_selected = id.to_owned();
        Task::none()
    }
    fn on_open_network_submit(&mut self) -> Task<AppMessage> {
        if (self.mutation_phase != MutationPhase::Idle)
            || (crate::backend::selected_network_endpoint(
                self.hub_networks.clone(),
                self.hub_selected.to_owned(),
            ))
            .is_empty()
        {
            return Task::none();
        }
        self.rpc = crate::backend::selected_network_endpoint(
            self.hub_networks.clone(),
            self.hub_selected.to_owned(),
        );
        self.network_name = crate::backend::selected_network_name(
            self.hub_networks.clone(),
            self.hub_selected.to_owned(),
        );
        self.onboarding_error = "".to_owned();
        self.password = "".to_owned();
        self.hub_wallet_selected = "".to_owned();
        self.mutation_phase = MutationPhase::Onboarding;
        let pending_task =
            Task::perform(crate::backend::load_wallets(self.rpc.to_owned()), |value| {
                AppMessage::WalletsLoaded(value)
            });
        self.wallets_load_generation = self.wallets_load_generation.wrapping_add(1);
        let request_generation = self.wallets_load_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .wallets_load_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::WalletsLoadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_connect_remote_submit(&mut self, endpoint: String) -> Task<AppMessage> {
        if (self.mutation_phase != MutationPhase::Idle) || ((endpoint).trim().to_owned()).is_empty()
        {
            return Task::none();
        }
        self.rpc = crate::backend::canonical_endpoint(endpoint.to_owned());
        self.network_name = crate::backend::network_label("", &self.rpc);
        self.onboarding_error = "".to_owned();
        self.password = "".to_owned();
        self.hub_wallet_selected = "".to_owned();
        self.mutation_phase = MutationPhase::Onboarding;
        let pending_task =
            Task::perform(crate::backend::load_wallets(self.rpc.to_owned()), |value| {
                AppMessage::WalletsLoaded(value)
            });
        self.wallets_load_generation = self.wallets_load_generation.wrapping_add(1);
        let request_generation = self.wallets_load_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .wallets_load_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::WalletsLoadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_wallets_loaded(&mut self, list: crate::backend::WalletList) -> Task<AppMessage> {
        let door = crate::backend::wallet_door(&(list));
        self.mutation_phase = MutationPhase::Idle;
        self.hub_wallets = list.wallets.clone();
        self.hub_wallet_selected = crate::backend::preselect_wallet(list.wallets.clone());
        self.onboarding_error = list.error.to_owned();
        match door {
            WalletDoor::Wallets => {
                self.hub_step = HubStep::Wallets;
                Task::none()
            }
            WalletDoor::Password => {
                self.hub_step = HubStep::Password;
                Task::none()
            }
            WalletDoor::Unreached => Task::none(),
        }
    }
    fn on_chain_named(&mut self, id: String) -> Task<AppMessage> {
        self.hub_chain_id = id.to_owned();
        Task::none()
    }
    fn on_chain_probe_failed(&mut self, _cause: crate::backend::AppError) -> Task<AppMessage> {
        self.hub_chain_id = "".to_owned();
        Task::none()
    }
    fn on_account_probed(&mut self, next: crate::backend::AccountData) -> Task<AppMessage> {
        self.mutation_phase = MutationPhase::Idle;
        let probe = crate::backend::account_probe(next.exists);
        match probe {
            AccountProbe::Found => Task::done(AppMessage::NetworkEntered),
            AccountProbe::Missing => {
                self.network_name = crate::backend::network_label(
                    &self.hub_chain_id,
                    &self.rpc,
                );
                self.ceremony_phase = "".to_owned();
                self.ceremony_qr = "".to_owned();
                self.ceremony_detail = "".to_owned();
                self.ceremony_left = "".to_owned();
                self.hub_step = HubStep::Account;
                Task::none()
            }
        }
    }
    fn on_account_probe_failed(
        &mut self,
        cause: crate::backend::HydrationError,
    ) -> Task<AppMessage> {
        self.mutation_phase = MutationPhase::Idle;
        self.onboarding_error = cause.message.to_owned();
        Task::none()
    }
    fn on_welcome_skip(&mut self) -> Task<AppMessage> {
        if self.mutation_phase != MutationPhase::Idle {
            return Task::none();
        }
        self.onboarding_error = "".to_owned();
        Task::done(AppMessage::NetworkEntered)
    }
    fn on_welcome_cancel(&mut self) -> Task<AppMessage> {
        self.welcome_qr_auth_generation = self.welcome_qr_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.welcome_qr_auth_task.take() {
            previous_handle.abort();
        }
        self.welcome_desktop_auth_generation = self.welcome_desktop_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.welcome_desktop_auth_task.take() {
            previous_handle.abort();
        }
        self.mutation_phase = MutationPhase::Idle;
        self.ceremony_phase = "".to_owned();
        self.ceremony_qr = "".to_owned();
        self.ceremony_detail = "".to_owned();
        self.ceremony_left = "".to_owned();
        Task::none()
    }
    fn on_welcome_create_submit(&mut self, name: String) -> Task<AppMessage> {
        if ((self.mutation_phase != MutationPhase::Idle) || (name).is_empty())
            || (self.hub_chain_id).is_empty()
        {
            return Task::none();
        }
        self.onboarding_error = "".to_owned();
        self.mutation_phase = MutationPhase::Onboarding;
        let pending_task = Task::run(
            crate::backend::create_account_by_qr(
                self.rpc.to_owned(),
                self.password.to_owned(),
                self.hub_chain_id.to_owned(),
                name.to_owned(),
            ),
            AppMessage::CeremonyStepped,
        );
        self.welcome_qr_auth_generation = self.welcome_qr_auth_generation.wrapping_add(1);
        let request_generation = self.welcome_qr_auth_generation;
        let pending_task = pending_task
            .map(move |reply_message| {
                AppMessage::WelcomeQrAuthReply(request_generation, Some(Box::new(reply_message)))
            })
            .chain(Task::done(AppMessage::WelcomeQrAuthReply(
                request_generation,
                None,
            )));
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .welcome_qr_auth_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task
    }
    fn on_welcome_login_submit(&mut self) -> Task<AppMessage> {
        if (self.mutation_phase != MutationPhase::Idle) || (self.hub_chain_id).is_empty() {
            return Task::none();
        }
        self.onboarding_error = "".to_owned();
        self.mutation_phase = MutationPhase::Onboarding;
        let pending_task = Task::run(
            crate::backend::login_by_qr(
                self.rpc.to_owned(),
                self.password.to_owned(),
                self.hub_chain_id.to_owned(),
            ),
            AppMessage::CeremonyStepped,
        );
        self.welcome_qr_auth_generation = self.welcome_qr_auth_generation.wrapping_add(1);
        let request_generation = self.welcome_qr_auth_generation;
        let pending_task = pending_task
            .map(move |reply_message| {
                AppMessage::WelcomeQrAuthReply(request_generation, Some(Box::new(reply_message)))
            })
            .chain(Task::done(AppMessage::WelcomeQrAuthReply(
                request_generation,
                None,
            )));
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .welcome_qr_auth_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task
    }
    fn on_welcome_desktop(&mut self) -> Task<AppMessage> {
        if self.ceremony_phase != "show_qr" {
            return Task::none();
        }
        self.welcome_qr_auth_generation = self.welcome_qr_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.welcome_qr_auth_task.take() {
            previous_handle.abort();
        }
        self.ceremony_phase = "working".to_owned();
        self.ceremony_qr = "".to_owned();
        self.ceremony_detail = "Continue in the browser…".to_owned();
        let door = crate::backend::welcome_door(&self.welcome_name_draft);
        match door {
            WelcomeDoor::Create => {
                let pending_task = Task::perform(
                    crate::backend::register_passkey(
                        self.rpc.to_owned(),
                        self.password.to_owned(),
                        self.hub_chain_id.to_owned(),
                        "".to_owned(),
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::WelcomeDesktopDone(value),
                        Err(error) => AppMessage::WelcomeFailed(error),
                    },
                );
                self.welcome_desktop_auth_generation =
                    self.welcome_desktop_auth_generation.wrapping_add(1);
                let request_generation = self.welcome_desktop_auth_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .welcome_desktop_auth_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::WelcomeDesktopAuthReply(request_generation, Box::new(reply_message))
                })
            }
            WelcomeDoor::Login => {
                let pending_task = Task::perform(
                    crate::backend::login_with_passkey(
                        self.rpc.to_owned(),
                        self.password.to_owned(),
                        self.hub_chain_id.to_owned(),
                        "".to_owned(),
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::WelcomeDesktopDone(value),
                        Err(error) => AppMessage::WelcomeFailed(error),
                    },
                );
                self.welcome_desktop_auth_generation =
                    self.welcome_desktop_auth_generation.wrapping_add(1);
                let request_generation = self.welcome_desktop_auth_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) = self
                    .welcome_desktop_auth_task
                    .replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::WelcomeDesktopAuthReply(request_generation, Box::new(reply_message))
                })
            }
        }
    }
    fn on_welcome_desktop_done(&mut self, _ok: bool) -> Task<AppMessage> {
        self.mutation_phase = MutationPhase::Idle;
        self.ceremony_phase = "".to_owned();
        self.ceremony_qr = "".to_owned();
        self.ceremony_detail = "".to_owned();
        self.ceremony_left = "".to_owned();
        Task::done(AppMessage::NetworkEntered)
    }
    fn on_ceremony_stepped(&mut self, next: crate::backend::CeremonyStep) -> Task<AppMessage> {
        let phase = crate::backend::ceremony_phase(&(next));
        self.ceremony_phase = next.phase.to_owned();
        self.ceremony_qr = next.qr.to_owned();
        self.ceremony_detail = next.detail.to_owned();
        self.ceremony_left = next.left.to_owned();
        match phase {
            CeremonyPhase::Done => {
                self.mutation_phase = MutationPhase::Idle;
                self.ceremony_phase = "".to_owned();
                self.ceremony_qr = "".to_owned();
                Task::done(AppMessage::NetworkEntered)
            }
            CeremonyPhase::Failed => {
                self.mutation_phase = MutationPhase::Idle;
                self.ceremony_phase = "".to_owned();
                self.ceremony_qr = "".to_owned();
                self.onboarding_error = next.detail.to_owned();
                Task::none()
            }
            CeremonyPhase::ShowQr => {
                self.onboarding_error = "".to_owned();
                Task::none()
            }
            CeremonyPhase::Working => {
                self.onboarding_error = "".to_owned();
                Task::none()
            }
        }
    }
    fn on_welcome_failed(&mut self, cause: crate::backend::AppError) -> Task<AppMessage> {
        self.mutation_phase = MutationPhase::Idle;
        self.ceremony_phase = "".to_owned();
        self.ceremony_qr = "".to_owned();
        self.ceremony_detail = "".to_owned();
        self.ceremony_left = "".to_owned();
        self.onboarding_error = cause.message.to_owned();
        Task::none()
    }
    fn on_network_entered(&mut self) -> Task<AppMessage> {
        self.welcome_qr_auth_generation = self.welcome_qr_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.welcome_qr_auth_task.take() {
            previous_handle.abort();
        }
        self.welcome_desktop_auth_generation = self.welcome_desktop_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.welcome_desktop_auth_task.take() {
            previous_handle.abort();
        }
        self.mutation_phase = MutationPhase::Idle;
        self.ceremony_phase = "".to_owned();
        self.ceremony_qr = "".to_owned();
        self.ceremony_detail = "".to_owned();
        self.ceremony_left = "".to_owned();
        self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_qr_auth_task.take() {
            previous_handle.abort();
        }
        self.account_desktop_auth_generation = self.account_desktop_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_desktop_auth_task.take() {
            previous_handle.abort();
        }
        self.account_busy = false;
        self.account_banner_dismissed = false;
        self.account_ceremony_phase = "".to_owned();
        self.account_ceremony_qr = "".to_owned();
        self.account_ceremony_detail = "".to_owned();
        self.account_ceremony_left = "".to_owned();
        self.palette_search_generation = self.palette_search_generation.wrapping_add(1);
        if let Some(previous_handle) = self.palette_search_task.take() {
            previous_handle.abort();
        }
        self.channel_window_load_generation = self.channel_window_load_generation.wrapping_add(1);
        if let Some(previous_handle) = self.channel_window_load_task.take() {
            previous_handle.abort();
        }
        self.live_resync_generation = self.live_resync_generation.wrapping_add(1);
        if let Some(previous_handle) = self.live_resync_task.take() {
            previous_handle.abort();
        }
        self.wall_now = crate::backend::current_wall_seconds();
        self.connected = false;
        self.loading = true;
        self.status = "Connecting…".to_owned();
        self.error = "".to_owned();
        self.onboarding_error = "".to_owned();
        self.connected_rpc = self.rpc.to_owned();
        self.network_chain_id = "".to_owned();
        self.network_name = crate::backend::network_label(
            &self.network_chain_id,
            &self.connected_rpc,
        );
        self.hydration_generation += 1;
        self.connect_generation += 1;
        self.hydration_retry_attempt = 0;
        self.mutation_phase = MutationPhase::Idle;
        self.channels = Vec::new();
        self.rooms = Vec::new();
        self.dm_rows = Vec::new();
        self.chat_at_tail = true;
        self.chat_land_seq = 0;
        self.chat_pending_sends = Vec::new();
        self.chat_edit_seq = 0;
        self.chat_edit_rev = 0;
        self.channel_reads = Vec::new();
        self.unread_boundary = 0;
        self.active_channel = "".to_owned();
        self.active_dm_peer = "".to_owned();
        self.active_dm = crate::backend::no_dm_peer();
        self.history_view = false;
        self.active_channel_name = "".to_owned();
        self.active_channel_archived = false;
        self.active_channel_members_only = false;
        self.channel_members = Vec::new();
        self.post_refusal = "".to_owned();
        self.channel_draft = "".to_owned();
        self.pending_channel = "".to_owned();
        self.page_route = "".to_owned();
        self.palette_draft = "".to_owned();
        self.palette_chat_hits = Vec::new();
        self.palette_page_hits = Vec::new();
        self.palette_search_phase = SearchPhase::Idle;
        self.forge_note_pending = "".to_owned();
        self.forge_link = "".to_owned();
        self.huddle_joined = false;
        self.huddle_channel = "".to_owned();
        self.huddle_channel_name = "".to_owned();
        self.huddle_joined_at = 0;
        self.huddle_roster = Vec::new();
        self.huddle_rows = Vec::new();
        self.call_status = "".to_owned();
        self.call_muted = false;
        self.call_deafened = false;
        self.call_speaking = false;
        self.call_camera = false;
        self.call_sharing = false;
        self.call_video_live = false;
        self.huddle_stage = "".to_owned();
        self.call_peers = Vec::new();
        if (self.connected_rpc).is_empty() {
            return Task::none();
        }
        self.console_entry = ConsoleEntry::Entering;
        Task::batch([
            (Task::perform(
                crate::backend::remember_network(self.connected_rpc.to_owned()),
                |value| value,
            ))
            .discard::<AppMessage>(),
            {
                let pending_task = Task::perform(
                    crate::backend::connect(
                        self.connected_rpc.to_owned(),
                        0,
                        self.connect_generation,
                    ),
                    |result| match result {
                        Ok(value) => AppMessage::WorkspaceConnected(value),
                        Err(error) => AppMessage::ConnectFailed(error),
                    },
                );
                self.connection_generation = self.connection_generation.wrapping_add(1);
                let request_generation = self.connection_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) =
                    self.connection_task.replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::ConnectionReply(request_generation, Box::new(reply_message))
                })
            },
        ])
    }
    fn on_console_opened(&mut self, id: crate::shell::WindowKey) -> Task<AppMessage> {
        self.console_win = Some(id);
        crate::shell::close::<AppMessage>(crate::backend::window_target(
            self.onboarding_win,
        ))
    }
    fn on_forget_network_submit(&mut self, id: String) -> Task<AppMessage> {
        if self.mutation_phase != MutationPhase::Idle {
            return Task::none();
        }
        Task::perform(crate::backend::forget_network(id.to_owned()), |value| {
            AppMessage::NetworkForgotten(value)
        })
    }
    fn on_network_forgotten(&mut self, _written: bool) -> Task<AppMessage> {
        let pending_task = Task::perform(crate::backend::hub_state(), |value| {
            AppMessage::HubRefreshed(value)
        });
        self.hub_load_generation = self.hub_load_generation.wrapping_add(1);
        let request_generation = self.hub_load_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self.hub_load_task.replace(request_handle.abort_on_drop()) {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::HubLoadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_go_join(&mut self) -> Task<AppMessage> {
        if self.mutation_phase != MutationPhase::Idle {
            return Task::none();
        }
        self.secrets.clear("join_invite");
        self.hub_step = HubStep::Join;
        self.onboarding_error = "".to_owned();
        Task::none()
    }
    fn on_go_networks(&mut self) -> Task<AppMessage> {
        let unrelated_mutation =
            (self.mutation_phase != MutationPhase::Idle) && (self.hub_step != HubStep::Account);
        if unrelated_mutation {
            return Task::none();
        }
        self.welcome_qr_auth_generation = self.welcome_qr_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.welcome_qr_auth_task.take() {
            previous_handle.abort();
        }
        self.welcome_desktop_auth_generation = self.welcome_desktop_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.welcome_desktop_auth_task.take() {
            previous_handle.abort();
        }
        self.wallets_load_generation = self.wallets_load_generation.wrapping_add(1);
        if let Some(previous_handle) = self.wallets_load_task.take() {
            previous_handle.abort();
        }
        self.mutation_phase = MutationPhase::Idle;
        self.ceremony_phase = "".to_owned();
        self.ceremony_qr = "".to_owned();
        self.ceremony_detail = "".to_owned();
        self.ceremony_left = "".to_owned();
        self.secrets.clear("restore_words");
        self.secrets.clear("join_invite");
        self.onboarding_error = "".to_owned();
        self.password = "".to_owned();
        self.hub_wallets = Vec::new();
        self.hub_wallet_selected = "".to_owned();
        self.hub_step = HubStep::Networks;
        let pending_task = Task::perform(crate::backend::hub_state(), |value| {
            AppMessage::HubRefreshed(value)
        });
        self.hub_load_generation = self.hub_load_generation.wrapping_add(1);
        let request_generation = self.hub_load_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self.hub_load_task.replace(request_handle.abort_on_drop()) {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::HubLoadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_join_network_submit(&mut self) -> Task<AppMessage> {
        if (self.mutation_phase != MutationPhase::Idle)
            || (self.secrets.text("join_invite")).is_empty()
        {
            return Task::none();
        }
        self.onboarding_error = "".to_owned();
        self.mutation_phase = MutationPhase::Onboarding;
        Task::perform(
            crate::backend::join_network(self.secrets.read("join_invite")),
            |result| match result {
                Ok(value) => AppMessage::WorkspaceMaterialized(value),
                Err(error) => AppMessage::OnboardingFailed(error),
            },
        )
    }
    fn on_workspace_materialized(
        &mut self,
        init: crate::backend::WorkspaceInit,
    ) -> Task<AppMessage> {
        self.secrets.clear("join_invite");
        self.mutation_phase = MutationPhase::Idle;
        self.onboarding_name = init.chain_id.to_owned();
        self.rpc = init.rpc.to_owned();
        self.invite_link = "".to_owned();
        self.provision_steps = Vec::new();
        self.provision_index = 0;
        self.onboarding_error = "".to_owned();
        self.hub_step = HubStep::Provisioning;
        let pending_task = Task::run(
            crate::backend::provision_progress(init.chain_id.to_owned(), init.rpc.to_owned()),
            AppMessage::ProvisionStepped,
        );
        self.provision_progress_generation = self.provision_progress_generation.wrapping_add(1);
        let request_generation = self.provision_progress_generation;
        let pending_task = pending_task
            .map(move |reply_message| {
                AppMessage::ProvisionProgressReply(
                    request_generation,
                    Some(Box::new(reply_message)),
                )
            })
            .chain(Task::done(AppMessage::ProvisionProgressReply(
                request_generation,
                None,
            )));
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .provision_progress_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task
    }
    fn on_provision_stepped(&mut self, step: crate::backend::ProvisionStep) -> Task<AppMessage> {
        let settled = step.settled;
        self.provision_index = step.index;
        self.provision_steps = vec![step.clone()];
        if (self.provision_index != 5) || (!settled) {
            return Task::none();
        }
        self.hub_step = HubStep::Live;
        Task::perform(
            crate::backend::mint_invite(self.onboarding_name.to_owned()),
            |result| match result {
                Ok(value) => AppMessage::OnboardingInviteMinted(value),
                Err(error) => AppMessage::OnboardingFailed(error),
            },
        )
    }
    fn on_onboarding_invite_minted(&mut self, blob: String) -> Task<AppMessage> {
        self.invite_link = blob.to_owned();
        self.onboarding_error = "".to_owned();
        Task::none()
    }
    fn on_copy_onboarding_invite(&mut self) -> Task<AppMessage> {
        if (self.invite_link).is_empty() {
            return Task::none();
        }
        self.toast = "Invite copied".to_owned();
        self.toast_age = 0;
        crate::shell::clipboard::<AppMessage>(self.invite_link.to_owned())
    }
    fn on_enter_console(&mut self) -> Task<AppMessage> {
        if self.mutation_phase != MutationPhase::Idle {
            return Task::none();
        }
        self.network_name =
            crate::backend::network_label(&self.onboarding_name, &self.rpc);
        self.onboarding_error = "".to_owned();
        self.password = "".to_owned();
        self.hub_wallet_selected = "".to_owned();
        self.mutation_phase = MutationPhase::Onboarding;
        let pending_task =
            Task::perform(crate::backend::load_wallets(self.rpc.to_owned()), |value| {
                AppMessage::WalletsLoaded(value)
            });
        self.wallets_load_generation = self.wallets_load_generation.wrapping_add(1);
        let request_generation = self.wallets_load_generation;
        let (pending_task, request_handle) = pending_task.abortable();
        if let Some(previous_handle) = self
            .wallets_load_task
            .replace(request_handle.abort_on_drop())
        {
            previous_handle.abort();
        }
        pending_task.map(move |reply_message| {
            AppMessage::WalletsLoadReply(request_generation, Box::new(reply_message))
        })
    }
    fn on_onboarding_failed(&mut self, cause: crate::backend::AppError) -> Task<AppMessage> {
        self.mutation_phase = MutationPhase::Idle;
        self.onboarding_error = cause.message.to_owned();
        Task::none()
    }
    fn on_switch_network(&mut self) -> Task<AppMessage> {
        if self.mutation_phase != MutationPhase::Idle {
            return Task::none();
        }
        self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_qr_auth_task.take() {
            previous_handle.abort();
        }
        self.account_desktop_auth_generation = self.account_desktop_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_desktop_auth_task.take() {
            previous_handle.abort();
        }
        self.account_busy = self.account_busy && (self.account_ceremony_phase).is_empty();
        self.account_ceremony_phase = "".to_owned();
        self.account_ceremony_qr = "".to_owned();
        self.account_ceremony_detail = "".to_owned();
        self.account_ceremony_left = "".to_owned();
        let (_, pending_task) = crate::shell::open(crate::shell::WindowKind::Onboarding);
        pending_task.map(AppMessage::OnboardingReopened)
    }
    fn on_onboarding_reopened(&mut self, id: crate::shell::WindowKey) -> Task<AppMessage> {
        self.onboarding_win = Some(id);
        self.hub_step = HubStep::Networks;
        self.password = "".to_owned();
        self.hub_wallets = Vec::new();
        self.hub_wallet_selected = "".to_owned();
        self.signer_key = "".to_owned();
        self.live_agents = Vec::new();
        Task::batch([
            crate::shell::close::<AppMessage>(crate::backend::window_target(
                self.console_win,
            )),
            crate::shell::close::<AppMessage>(crate::backend::window_target(
                self.huddle_win,
            )),
            (Task::perform(crate::backend::lock_signer(), |value| value)).discard::<AppMessage>(),
            {
                let pending_task = Task::perform(crate::backend::hub_state(), |value| {
                    AppMessage::HubRefreshed(value)
                });
                self.hub_load_generation = self.hub_load_generation.wrapping_add(1);
                let request_generation = self.hub_load_generation;
                let (pending_task, request_handle) = pending_task.abortable();
                if let Some(previous_handle) =
                    self.hub_load_task.replace(request_handle.abort_on_drop())
                {
                    previous_handle.abort();
                }
                pending_task.map(move |reply_message| {
                    AppMessage::HubLoadReply(request_generation, Box::new(reply_message))
                })
            },
        ])
    }
    fn on_dismiss_account_banner(&mut self) -> Task<AppMessage> {
        self.account_banner_dismissed = true;
        Task::none()
    }
    fn on_open_account_welcome(&mut self) -> Task<AppMessage> {
        if self.mutation_phase != MutationPhase::Idle {
            return Task::none();
        }
        self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_qr_auth_task.take() {
            previous_handle.abort();
        }
        self.account_desktop_auth_generation = self.account_desktop_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_desktop_auth_task.take() {
            previous_handle.abort();
        }
        self.account_busy = self.account_busy && (self.account_ceremony_phase).is_empty();
        self.account_ceremony_phase = "".to_owned();
        self.account_ceremony_qr = "".to_owned();
        self.account_ceremony_detail = "".to_owned();
        self.account_ceremony_left = "".to_owned();
        self.rpc = self.connected_rpc.to_owned();
        self.hub_chain_id = self.network_chain_id.to_owned();
        let (_, pending_task) = crate::shell::open(crate::shell::WindowKind::Onboarding);
        pending_task.map(AppMessage::WelcomeReopened)
    }
    fn on_welcome_reopened(&mut self, id: crate::shell::WindowKey) -> Task<AppMessage> {
        self.onboarding_win = Some(id);
        self.ceremony_phase = "".to_owned();
        self.ceremony_qr = "".to_owned();
        self.ceremony_detail = "".to_owned();
        self.onboarding_error = "".to_owned();
        self.hub_step = HubStep::Account;
        Task::batch([
            crate::shell::close::<AppMessage>(crate::backend::window_target(
                self.console_win,
            )),
            crate::shell::close::<AppMessage>(crate::backend::window_target(
                self.huddle_win,
            )),
        ])
    }
    fn on_call_event(&mut self, event: crate::call::CallEvent) -> Task<AppMessage> {
        self.call_status =
            crate::call::call_status_after(self.call_status.to_owned(), event.clone());
        self.call_muted =
            crate::backend::keep_bool(event.kind == "connecting", false, self.call_muted);
        self.call_deafened =
            crate::backend::keep_bool(event.kind == "connecting", false, self.call_deafened);
        self.call_camera =
            crate::backend::keep_bool(event.kind == "connecting", false, self.call_camera);
        self.call_sharing =
            crate::backend::keep_bool(event.kind == "connecting", false, self.call_sharing);
        self.call_speaking = crate::call::call_speaking_after(self.call_speaking, &event);
        self.call_peers =
            crate::call::apply_call_peer(::std::mem::take(&mut self.call_peers), event.clone());
        self.huddle_rows = crate::call::huddle_tile_rows(
            self.huddle_roster.clone(),
            self.call_peers.clone(),
            self.call_muted,
            self.call_speaking,
        );
        self.call_video_live = crate::call::call_video_live_after(
            self.call_peers.clone(),
            self.call_camera,
            self.call_sharing,
        );
        self.huddle_stage =
            crate::call::huddle_stage_peer(self.call_peers.clone(), self.call_sharing);
        Task::none()
    }
    fn on_toggle_call_mute(&mut self) -> Task<AppMessage> {
        if !self.huddle_joined {
            return Task::none();
        }
        self.call_muted = crate::call::call_set_muted(!self.call_muted);
        self.huddle_rows = crate::call::huddle_tile_rows(
            self.huddle_roster.clone(),
            self.call_peers.clone(),
            self.call_muted,
            self.call_speaking,
        );
        Task::none()
    }
    /// Deafen mutes with it, the way Discord ties the two; undeafen leaves
    /// the mic where it is, so a person who muted first stays muted.
    fn on_toggle_call_deafen(&mut self) -> Task<AppMessage> {
        if !self.huddle_joined {
            return Task::none();
        }
        self.call_deafened = crate::call::call_set_deafened(!self.call_deafened);
        if self.call_deafened {
            self.call_muted = crate::call::call_set_muted(true);
        }
        self.huddle_rows = crate::call::huddle_tile_rows(
            self.huddle_roster.clone(),
            self.call_peers.clone(),
            self.call_muted,
            self.call_speaking,
        );
        Task::none()
    }
    fn on_toggle_call_camera(&mut self) -> Task<AppMessage> {
        let source = crate::video::call_use_camera(!self.call_camera);
        self.call_camera = source.camera;
        self.call_sharing = source.sharing;
        self.call_video_live = crate::call::call_video_live_after(
            self.call_peers.clone(),
            self.call_camera,
            self.call_sharing,
        );
        self.huddle_stage =
            crate::call::huddle_stage_peer(self.call_peers.clone(), self.call_sharing);
        Task::none()
    }
    fn on_toggle_call_screen(&mut self) -> Task<AppMessage> {
        let source = crate::video::call_use_screen(!self.call_sharing);
        self.call_camera = source.camera;
        self.call_sharing = source.sharing;
        self.call_video_live = crate::call::call_video_live_after(
            self.call_peers.clone(),
            self.call_camera,
            self.call_sharing,
        );
        self.huddle_stage =
            crate::call::huddle_stage_peer(self.call_peers.clone(), self.call_sharing);
        Task::none()
    }
    fn on_show_huddle(&mut self) -> Task<AppMessage> {
        let summon = crate::backend::huddle_summon(self.huddle_win);
        match summon {
            WindowSummon::Open => {
                let (_, pending_task) = crate::shell::open(crate::shell::WindowKind::Huddle);
                pending_task.map(AppMessage::HuddleOpened)
            }
            WindowSummon::Raise => crate::shell::raise::<AppMessage>(
                crate::backend::window_target(self.huddle_win),
            ),
        }
    }
    fn on_huddle_opened(&mut self, id: crate::shell::WindowKey) -> Task<AppMessage> {
        self.huddle_win = Some(id);
        Task::none()
    }
    fn on_huddle_go_channel(&mut self) -> Task<AppMessage> {
        if (self.loading || (self.mutation_phase != MutationPhase::Idle))
            || (self.huddle_channel).is_empty()
        {
            return Task::none();
        }
        self.account_qr_auth_generation = self.account_qr_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_qr_auth_task.take() {
            previous_handle.abort();
        }
        self.account_desktop_auth_generation = self.account_desktop_auth_generation.wrapping_add(1);
        if let Some(previous_handle) = self.account_desktop_auth_task.take() {
            previous_handle.abort();
        }
        self.account_busy = self.account_busy && (self.account_ceremony_phase).is_empty();
        self.account_ceremony_phase = "".to_owned();
        self.account_ceremony_qr = "".to_owned();
        self.account_ceremony_detail = "".to_owned();
        self.account_ceremony_left = "".to_owned();
        self.shell_tab = ShellTab::Chat;
        Task::done(AppMessage::ChooseChannel(self.huddle_channel.to_owned()))
    }
    fn on_leave_huddle_here(&mut self) -> Task<AppMessage> {
        if ((self.loading || (self.mutation_phase != MutationPhase::Idle)) || (!self.huddle_joined))
            || (self.huddle_channel).is_empty()
        {
            return Task::none();
        }
        self.hydration_generation += 1;
        self.hydration_retry_attempt = 0;
        self.mutation_phase = MutationPhase::Huddle;
        self.call_status = "".to_owned();
        self.call_muted = false;
        self.call_deafened = false;
        self.call_speaking = false;
        self.call_camera = false;
        self.call_sharing = false;
        self.call_video_live = false;
        self.huddle_stage = "".to_owned();
        self.call_peers = Vec::new();
        self.huddle_rows = crate::call::huddle_tile_rows(
            self.huddle_roster.clone(),
            self.call_peers.clone(),
            self.call_muted,
            self.call_speaking,
        );
        self.error = "".to_owned();
        Task::batch([
            crate::shell::close::<AppMessage>(crate::backend::window_target(
                self.huddle_win,
            )),
            Task::perform(
                crate::backend::leave_huddle(
                    self.connected_rpc.to_owned(),
                    self.password.to_owned(),
                    self.huddle_channel.to_owned(),
                ),
                |result| match result {
                    Ok(value) => AppMessage::HuddleLeft(value),
                    Err(error) => AppMessage::MutationFailed(error),
                },
            ),
        ])
    }
    fn on_huddle_left(&mut self, _result: bool) -> Task<AppMessage> {
        self.huddle_joined = false;
        self.huddle_roster = Vec::new();
        self.huddle_rows = Vec::new();
        self.huddle_channel = "".to_owned();
        self.huddle_channel_name = "".to_owned();
        self.huddle_joined_at = 0;
        self.mutation_phase = MutationPhase::Idle;
        self.error = "".to_owned();
        Task::none()
    }
    fn on_secret_typed(&mut self, slot: String, text: String) -> Task<AppMessage> {
        if let Some(slot) = match slot.as_str() {
            "restore_words" => Some("restore_words"),
            "join_invite" => Some("join_invite"),
            _ => None,
        } {
            self.secrets.set(slot, text);
        }
        Task::none()
    }
    fn on_channel_draft_changed(&mut self, value: String) -> Task<AppMessage> {
        self.channel_draft = value;
        Task::none()
    }
}
