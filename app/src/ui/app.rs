use ducktape_view_guest::Task;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum LiveKind {
    Retry,
    Tip,
    Ready,
    Chat,
    Bell,
    Plane,
    Resync,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SearchPhase {
    Idle,
    Searching,
    Done,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Appearance {
    System,
    Light,
    Dark,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SubmitVerdict {
    Admitted,
    Refused,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ComposerKind {
    Message,
    Reply,
    Edit,
    ThreadEdit,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HubStep {
    Loading,
    Password,
    Phrase,
    Confirm,
    Wallets,
    Restore,
    Networks,
    Join,
    Provisioning,
    Live,
    Account,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum WalletDoor {
    Wallets,
    Password,
    Unreached,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum WindowSummon {
    Open,
    Raise,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TrayOpen {
    Launch,
    Console,
    Raise,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ConsoleEntry {
    Idle,
    Entering,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CommandChord {
    Ignored,
    Quit,
    CloseWindow,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AccountProbe {
    Found,
    Missing,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CeremonyPhase {
    Working,
    ShowQr,
    Done,
    Failed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum WelcomeDoor {
    Create,
    Login,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DuckKind {
    Unknown,
    Web,
    ForeignNetwork,
    Page,
    Files,
    ForgeRepo,
    ForgeItem,
    ForgeBlob,
    Channel,
    ChannelMessage,
    Run,
    Account,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ShellTab {
    Chat,
    Pages,
    Forge,
    Agents,
    Files,
    Explorer,
    Node,
    Members,
    Governance,
    Settings,
    /// A view the connected node's registry lists as a `Kind::View` entry,
    /// by its id: drawn after the built-in tabs, gone when the id leaves
    /// the registry.
    Registered(&'static str),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RegisteredIntent {
    OpenLink,
    Copy,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ForgeIntent {
    OpenLink,
    Copy,
    Composer,
    Attach,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AgentsIntent {
    Badge,
    Register,
    OpenRun,
    OpenLink,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SettingsIntent {
    Tab,
    Reconnect,
    SwitchNetwork,
    Unlock,
    Lock,
    Rename,
    Create,
    KeyAdd,
    Join,
    KeyRemove,
    Passkey,
    PasskeyDesktop,
    CeremonyCancel,
    Wallet,
    Login,
    Copy,
    Light,
    Dark,
    Notifications,
    Taste,
    Untaste,
    UpdateCheck,
    UpdateRestart,
    UpdateRollback,
}
/// The update controls the reader can press; each is one `app_update::Event`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum UpdateAction {
    CheckNow,
    RestartToUpdate,
    RollBack,
    DismissRollbackNotice,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PagesIntent {
    OpenLink,
    Copy,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ChatIntent {
    OpenHit,
    ToggleCreate,
    ChooseChannel,
    ChooseDm,
    ShowHuddle,
    LeaveHuddle,
    JoinHuddle,
    JoinVoice,
    Scrolled,
    OpenLink,
    Copy,
    CopyLink,
    BeginEdit,
    CancelRun,
    OpenRun,
    Composer,
    Attach,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MutationPhase {
    Idle,
    Recovering,
    Channel,
    Huddle,
    MessageEdit,
    Onboarding,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CeremonyRetirement {
    Keep,
    Welcome,
    Account,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum BellTarget {
    Unavailable,
    Message,
    Page,
    Forge,
    Repo,
    Run,
}
pub struct Ducktape {
    pub(crate) appearance_save_generation: u64,
    pub(crate) appearance_save_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) connection_generation: u64,
    pub(crate) connection_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) dm_peers_load_generation: u64,
    pub(crate) dm_peers_load_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) node_facts_load_generation: u64,
    pub(crate) node_facts_load_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) bell_load_generation: u64,
    pub(crate) bell_load_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) members_load_generation: u64,
    pub(crate) members_load_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) settings_load_generation: u64,
    pub(crate) settings_load_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) account_load_generation: u64,
    pub(crate) account_load_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) live_resync_generation: u64,
    pub(crate) live_resync_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) bell_presentations_generation: u64,
    pub(crate) bell_presentations_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) account_qr_auth_generation: u64,
    pub(crate) account_qr_auth_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) account_desktop_auth_generation: u64,
    pub(crate) account_desktop_auth_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) notifications_save_generation: u64,
    pub(crate) notifications_save_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) bell_head_generation: u64,
    pub(crate) bell_head_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) bell_navigation_generation: u64,
    pub(crate) bell_navigation_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) palette_search_generation: u64,
    pub(crate) palette_search_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) channel_window_load_generation: u64,
    pub(crate) channel_window_load_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) appearance_load_generation: u64,
    pub(crate) appearance_load_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) notifications_load_generation: u64,
    pub(crate) notifications_load_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) hub_load_generation: u64,
    pub(crate) hub_load_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) network_probe_generation: u64,
    pub(crate) network_probe_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) welcome_account_load_generation: u64,
    pub(crate) welcome_account_load_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) chain_identity_load_generation: u64,
    pub(crate) chain_identity_load_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) wallets_load_generation: u64,
    pub(crate) wallets_load_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) welcome_qr_auth_generation: u64,
    pub(crate) welcome_qr_auth_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) welcome_desktop_auth_generation: u64,
    pub(crate) welcome_desktop_auth_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) provision_progress_generation: u64,
    pub(crate) provision_progress_task: Option<::ducktape_view_guest::task::Handle>,
    pub(crate) appearance: Appearance,
    /// What `Appearance::System` resolved to at the last sync: the OS
    /// appearance the kit theme picked up. `is_dark` answers from it.
    pub(crate) system_dark: bool,
    pub(crate) desktop_notifications: bool,
    pub(crate) wall_now: i64,
    pub(crate) rpc: String,
    pub(crate) connected_rpc: String,
    pub(crate) password: String,
    pub(crate) status: String,
    pub(crate) connected: bool,
    /// The self-update machine's executor, present only when the launcher
    /// started this process (`DUCKTAPE_RELEASE` + `DUCKTAPE_UPDATE_STATE`).
    pub(crate) updater: Option<crate::backend::update::Updater>,
    pub(crate) loading: bool,
    pub(crate) views_live_serial: i64,
    pub(crate) cmd_held: bool,
    pub(crate) shift_held: bool,
    pub(crate) focused_win: Option<crate::shell::WindowKey>,
    pub(crate) block_height: i64,
    pub(crate) hydration_generation: i64,
    pub(crate) connect_generation: i64,
    pub(crate) signer_key: String,
    pub(crate) hydration_retry_attempt: i64,
    pub(crate) mutation_phase: MutationPhase,
    pub(crate) error: String,
    pub(crate) startup_duck_link: String,
    pub(crate) channels: Vec<crate::backend::ChatChannel>,
    pub(crate) rooms: Vec<crate::backend::ChatSidebarRow>,
    pub(crate) chat_generation: i64,
    pub(crate) channel_reads: Vec<crate::backend::ChannelRead>,
    pub(crate) unread_boundary: i64,
    pub(crate) active_channel: String,
    pub(crate) active_channel_name: String,
    pub(crate) active_channel_archived: bool,
    pub(crate) active_channel_members_only: bool,
    pub(crate) channel_members: Vec<crate::backend::ChatMember>,
    pub(crate) post_refusal: String,
    pub(crate) chat_land_seq: i64,
    pub(crate) chat_edit_seq: i64,
    pub(crate) chat_edit_rev: i64,
    pub(crate) live_agents: Vec<crate::backend::LiveAgentRow>,
    pub(crate) chat_sent_serial: i64,
    pub(crate) chat_pending_sends: Vec<crate::backend::PendingSend>,
    pub(crate) chat_copy_chord_serial: i64,
    pub(crate) channel_draft: String,
    pub(crate) channel_create_open: bool,
    pub(crate) channel_create_members_only: bool,
    pub(crate) channel_create_voice: bool,
    pub(crate) pending_channel: String,
    pub(crate) chat_at_tail: bool,
    pub(crate) history_view: bool,
    pub(crate) chat_chain_id: String,
    pub(crate) dm_peers: Vec<crate::backend::DmPeer>,
    pub(crate) dm_rows: Vec<crate::backend::DmSidebarRow>,
    pub(crate) dm_peers_generation: i64,
    pub(crate) active_dm_peer: String,
    pub(crate) active_dm: crate::backend::DmPeer,
    pub(crate) shell_tab: ShellTab,
    pub(crate) members_answered: bool,
    pub(crate) members_rows: Vec<crate::backend::MemberRow>,
    pub(crate) members_generation: i64,
    pub(crate) gov_open: i64,
    pub(crate) agents_open_run: String,
    pub(crate) agents_opened: i64,
    pub(crate) agents_live: bool,
    pub(crate) forge_note_pending: String,
    pub(crate) forge_link: String,
    pub(crate) forge_link_tick: i64,
    pub(crate) node_key: String,
    pub(crate) node_data_dir: String,
    pub(crate) settings_key_path: String,
    pub(crate) settings_key_state: String,
    pub(crate) settings_user_key: String,
    pub(crate) settings_generation: i64,
    pub(crate) account_exists: bool,
    pub(crate) account_number: String,
    pub(crate) account_name: String,
    pub(crate) account_bio: String,
    pub(crate) account_generation: i64,
    pub(crate) account_busy: bool,
    pub(crate) account_ticket: String,
    pub(crate) account_banner_dismissed: bool,
    pub(crate) account_ceremony_phase: String,
    pub(crate) account_ceremony_qr: String,
    pub(crate) account_ceremony_detail: String,
    pub(crate) account_ceremony_left: String,
    pub(crate) node_version: String,
    pub(crate) node_root_hash: String,
    pub(crate) network_chain_id: String,
    pub(crate) node_last_finalized: i64,
    pub(crate) node_checkpoint: i64,
    pub(crate) node_height: i64,
    pub(crate) node_phase: String,
    pub(crate) node_phase_since: i64,
    pub(crate) node_sync_target: i64,
    pub(crate) node_sync_applied: i64,
    pub(crate) node_sync_retries: i64,
    pub(crate) node_sync_failures: i64,
    pub(crate) node_sync_last_error: String,
    pub(crate) node_view_label: String,
    pub(crate) node_quorum_label: String,
    pub(crate) node_reachable_label: String,
    pub(crate) fs_drop_dir: String,
    pub(crate) fs_dropping: bool,
    pub(crate) fs_route: String,
    pub(crate) fs_route_serial: i64,
    pub(crate) palette_open: bool,
    pub(crate) bell_open: bool,
    pub(crate) bell_unread: i64,
    pub(crate) bell_items: Vec<crate::backend::BellItem>,
    pub(crate) bell_presentations: Vec<crate::backend::BellPresentation>,
    pub(crate) bell_read_through: i64,
    pub(crate) bell_clear_through: i64,
    pub(crate) bell_marking: bool,
    pub(crate) bell_error: String,
    pub(crate) palette_draft: String,
    pub(crate) palette_search_phase: SearchPhase,
    pub(crate) palette_chat_hits: Vec<crate::backend::ChatSearchHit>,
    pub(crate) palette_page_hits: Vec<crate::backend::PageSearchHit>,
    pub(crate) toast: String,
    pub(crate) toast_age: i64,
    pub(crate) page_route: String,
    pub(crate) page_route_serial: i64,
    pub(crate) onboarding_win: Option<crate::shell::WindowKey>,
    pub(crate) console_win: Option<crate::shell::WindowKey>,
    pub(crate) huddle_win: Option<crate::shell::WindowKey>,
    pub(crate) network_name: String,
    pub(crate) hub_step: HubStep,
    pub(crate) hub_networks: Vec<crate::backend::HubNetwork>,
    pub(crate) hub_selected: String,
    pub(crate) hub_wallets: Vec<crate::backend::WalletInfo>,
    pub(crate) hub_wallet_selected: String,
    pub(crate) onboarding_name: String,
    pub(crate) onboarding_error: String,
    pub(crate) wallet_opening_status: &'static str,
    pub(crate) console_entry: ConsoleEntry,
    pub(crate) connection_progress: String,
    pub(crate) invite_link: String,
    pub(crate) provision_steps: Vec<crate::backend::ProvisionStep>,
    pub(crate) provision_index: i64,
    pub(crate) hub_chain_id: String,
    pub(crate) welcome_name_draft: String,
    pub(crate) ceremony_phase: String,
    pub(crate) ceremony_qr: String,
    pub(crate) ceremony_detail: String,
    pub(crate) ceremony_left: String,
    pub(crate) huddle_joined: bool,
    pub(crate) huddle_channel: String,
    pub(crate) huddle_channel_name: String,
    pub(crate) huddle_joined_at: i64,
    pub(crate) huddle_now: i64,
    pub(crate) call_status: String,
    pub(crate) call_muted: bool,
    /// This side's mic voice gate, as the session last reported it.
    pub(crate) call_speaking: bool,
    pub(crate) call_peers: Vec<crate::call::CallEvent>,
    pub(crate) call_camera: bool,
    pub(crate) call_sharing: bool,
    pub(crate) call_video_live: bool,
    pub(crate) huddle_stage: String,
    pub(crate) huddle_roster: Vec<crate::backend::HuddleParticipant>,
    pub(crate) huddle_rows: Vec<crate::call::HuddleTileRow>,
    /// The huddle room's member roll, loaded on seating; the window offers
    /// the ones not seated as invite chips. A sent invite leaves the roll.
    pub(crate) huddle_invitees: Vec<crate::backend::ChatMember>,
    pub(crate) secrets: crate::secret::SecretStore,
}
impl ::std::fmt::Debug for Ducktape {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("Ducktape")
    }
}
#[derive(Clone)]
pub(crate) enum AppMessage {
    AppearanceSaveReply(u64, Box<AppMessage>),
    ConnectionReply(u64, Option<Box<AppMessage>>),
    ConnectionProgress(i64, &'static str),
    DmPeersLoadReply(u64, Box<AppMessage>),
    NodeFactsLoadReply(u64, Box<AppMessage>),
    BellLoadReply(u64, Box<AppMessage>),
    MembersLoadReply(u64, Box<AppMessage>),
    SettingsLoadReply(u64, Box<AppMessage>),
    AccountLoadReply(u64, Box<AppMessage>),
    LiveResyncReply(u64, Box<AppMessage>),
    BellPresentationsReply(u64, Box<AppMessage>),
    AccountQrAuthReply(u64, Option<Box<AppMessage>>),
    AccountDesktopAuthReply(u64, Box<AppMessage>),
    NotificationsSaveReply(u64, Box<AppMessage>),
    BellHeadReply(u64, Box<AppMessage>),
    BellNavigationReply(u64, Box<AppMessage>),
    PaletteSearchReply(u64, Box<AppMessage>),
    ChannelWindowLoadReply(u64, Box<AppMessage>),
    AppearanceLoadReply(u64, Box<AppMessage>),
    NotificationsLoadReply(u64, Box<AppMessage>),
    HubLoadReply(u64, Box<AppMessage>),
    NetworkProbeReply(u64, Option<Box<AppMessage>>),
    WelcomeAccountLoadReply(u64, Box<AppMessage>),
    ChainIdentityLoadReply(u64, Box<AppMessage>),
    WalletsLoadReply(u64, Box<AppMessage>),
    WelcomeQrAuthReply(u64, Option<Box<AppMessage>>),
    WelcomeDesktopAuthReply(u64, Box<AppMessage>),
    ProvisionProgressReply(u64, Option<Box<AppMessage>>),
    AppearanceLoaded(Appearance),
    SetAppearanceLight,
    SetAppearanceDark,
    AppearanceSaved(bool),
    DesktopNotificationsLoaded(bool),
    DesktopNotificationsSaved(bool),
    Reconnect,
    WorkspaceConnected(crate::backend::WorkspaceData),
    ConsoleEntryAnswered,
    LiveUpdated(crate::backend::LiveUpdate),
    LiveResynced(crate::backend::LiveRefresh),
    LiveResyncFailed(crate::backend::HydrationError),
    SelectShellTab(ShellTab),
    MembersLoadSelected(crate::backend::LoadRequest),
    SettingsLoadSelected(crate::backend::LoadRequest),
    AccountLoadSelected(crate::backend::LoadRequest),
    DmPeersLoadSelected(crate::backend::LoadRequest),
    NamesMovedSelected(crate::backend::LoadRequest),
    Tick,
    WallTick,
    WindowWasClosed(crate::shell::WindowKey),
    TrayOpen,
    TrayQuit,
    ModifierStateChanged(gpui_kit::Modifiers),
    CloseLaunchWindow,
    WindowFocused(crate::shell::WindowKey),
    WindowUnfocused(crate::shell::WindowKey),
    CommandChordPressed(crate::shell::KeyPress),
    TrayOpenBell,
    TrayGoChat,
    TrayGoPages,
    TrayGoNode,
    TrayGoSettings,
    TrayReconnect,
    TrayCopyNodeKey,
    MutationFailed(crate::backend::AppError),
    DismissError,
    ConnectFailed(crate::backend::HydrationError),
    ForgeViewEvent(crate::module_view::ModuleViewEvent),
    ForgeComposerEvent(String, String),
    ForgeNoteSent(String, crate::backend::SendReceipt),
    ForgeNoteFailed(String, String, crate::backend::OptimisticMutationError),
    FilesViewEvent(crate::module_view::ModuleViewEvent),
    FsFileDropped(String),
    /// A file in a composer's standby zone: its scope, upload id, local path.
    AttachmentQueued(String, String, String),
    /// Where an upload landed (a `duck://` address), or why it did not.
    AttachmentUploaded(String, String, Result<String, String>),
    FsDropped(bool),
    FsDropFailed(crate::backend::AppError),
    AccountLoaded(crate::backend::AccountData),
    AccountFailed(crate::backend::HydrationError),
    AccountRenamed(bool),
    AccountRenameFailed(crate::backend::AppError),
    AccountTicketMinted(String),
    AccountCeremonyStepped(crate::backend::CeremonyStep),
    AccountChanged(bool),
    AccountOpFailed(crate::backend::AppError),
    OpenRunPanel(String),
    GovernanceViewEvent(crate::module_view::ModuleViewEvent),
    MembersViewEvent(crate::module_view::ModuleViewEvent),
    MembersLoaded(crate::backend::MembersData),
    MembersFailed(crate::backend::HydrationError),
    DmPeersLoaded(crate::backend::DmPeersData),
    DmPeersFailed(crate::backend::HydrationError),
    AgentsViewEvent(crate::module_view::ModuleViewEvent),
    AgentStatusSet(bool),
    NodeViewEvent(crate::module_view::ModuleViewEvent),
    NodeFactsLoaded(crate::backend::NodeFacts),
    NodeFactsFailed(crate::backend::AppError),
    /// An update job (manifest fetch, archive download, verify) answered;
    /// `None` when the network served nothing.
    UpdateJobReplied(Option<app_update::Event>),
    /// The reader pressed one of the update controls (the console strip or
    /// the Settings "Updates" section).
    UpdateAction(UpdateAction),
    NodeStatusPushed(crate::backend::NodeFacts),
    SettingsLoaded(crate::backend::SettingsFacts),
    SettingsFailed(crate::backend::HydrationError),
    SettingsViewEvent(crate::module_view::ModuleViewEvent),
    SettingsUnlocked(String),
    SettingsUnlockFailed(crate::backend::AppError),
    CopyToClipboard(String, String),
    DismissToast,
    ToastTick,
    /// A sentence a taste left for the member: the proposed view they
    /// were trying was withdrawn, or became the current one.
    ViewNotice(String),
    ExplorerViewEvent(crate::module_view::ModuleViewEvent),
    ClosePalette,
    ToggleBell,
    ReloadBell,
    CloseBell,
    MarkBellReadSubmit,
    BellLoaded(i64, String, crate::backend::BellData),
    BellContextLoaded(i64, String, Vec<crate::backend::BellPresentation>),
    BellFailed(i64, String, crate::backend::AppError),
    BellMarked(i64, String, crate::backend::BellDelta),
    BellMarkFailed(i64, String, crate::backend::AppError),
    BellOpenItem(i64, String, crate::backend::BellPresentation),
    GlobalKeyPressed(crate::shell::KeyPress),
    PaletteChanged(String),
    PaletteResults(crate::backend::PaletteSearchData),
    PaletteSearchFailed(crate::backend::AppError),
    OpenChatSearchHit(String, i64),
    ChooseChannel(String),
    ChooseDm(String),
    CreateChannelSubmit,
    ToggleChannelCreateMembersOnly,
    ToggleChannelCreate,
    JoinHuddleSubmit,
    HuddleJoinedAck(bool),
    JoinVoice(String),
    VoiceJoined(String),
    ToggleChannelCreateVoice,
    ChatBeginEdit(String, String, i64, i64),
    ComposerSubmitted(ComposerKind, String, String, String),
    EditMessageSubmit(String),
    MessageSent(crate::backend::SendReceipt),
    MessageSendFailed(crate::backend::OptimisticMutationError),
    ThreadReplySendFailed(crate::backend::OptimisticMutationError),
    ThreadReplySent(crate::backend::SendReceipt),
    ChatUpdated(crate::backend::ChatData),
    ChatLoadFailed(crate::backend::HydrationError),
    ChannelCreated(crate::backend::ChatData),
    LiveAgentsEvent(crate::backend::LiveAgentNotice),
    LiveCancelAcked(bool),
    ChatAcked(bool),
    CopyMessageLink(String),
    OpenMessageLink(String),
    ChatScrolled(f64, f64, f64, f64),
    CopyChordPressed(crate::shell::KeyPress),
    ChatViewEvent(crate::module_view::ModuleViewEvent),
    PagesViewEvent(crate::module_view::ModuleViewEvent),
    RegisteredViewEvent(crate::module_view::ModuleViewEvent),
    OpenPageSearchHit(String, String),
    ExternalUrlFailed(crate::backend::AppError),
    OnboardingOpened(crate::shell::WindowKey),
    HubBooted(crate::backend::HubState),
    HubRefreshed(crate::backend::HubState),
    NetworkProbed(crate::backend::HubProbe),
    PickWallet(String),
    UnlockSubmit(String),
    KeyUnlocked(String),
    LoginSkip,
    PasswordSubmit(String),
    DeviceKeyCreated(String),
    PhraseWrittenDown,
    ShowPhraseAgain,
    ConfirmPhraseSubmit(String),
    PhraseConfirmed(String),
    PhraseConfirmFailed(crate::backend::AppError),
    GoRestore,
    GoLogin,
    RestoreSubmit(String, String),
    KeyRestored(String),
    LoginFailed(crate::backend::AppError),
    PickNetwork(String),
    OpenNetworkSubmit,
    ConnectRemoteSubmit(String),
    WalletsLoaded(crate::backend::WalletList),
    ChainNamed(String),
    ChainProbeFailed(crate::backend::AppError),
    AccountProbed(crate::backend::AccountData),
    AccountProbeFailed(crate::backend::HydrationError),
    WelcomeSkip,
    WelcomeCancel,
    WelcomeCreateSubmit(String),
    WelcomeLoginSubmit,
    WelcomeDesktop,
    WelcomeDesktopDone(bool),
    CeremonyStepped(crate::backend::CeremonyStep),
    WelcomeFailed(crate::backend::AppError),
    NetworkEntered,
    ConsoleOpened(crate::shell::WindowKey),
    ForgetNetworkSubmit(String),
    NetworkForgotten(bool),
    GoJoin,
    GoNetworks,
    JoinNetworkSubmit,
    WorkspaceMaterialized(crate::backend::WorkspaceInit),
    ProvisionStepped(crate::backend::ProvisionStep),
    OnboardingInviteMinted(String),
    CopyOnboardingInvite,
    EnterConsole,
    OnboardingFailed(crate::backend::AppError),
    SwitchNetwork,
    OnboardingReopened(crate::shell::WindowKey),
    DismissAccountBanner,
    OpenAccount,
    OpenAccountWelcome,
    WelcomeReopened(crate::shell::WindowKey),
    CallEvent(crate::call::CallEvent),
    ToggleCallMute,
    ToggleCallCamera,
    ToggleCallScreen,
    ShowHuddle,
    HuddleOpened(crate::shell::WindowKey),
    HuddleGoChannel,
    LeaveHuddleHere,
    HuddleLeft(bool),
    HuddleInviteesLoaded(Vec<crate::backend::ChatMember>),
    /// Invite a member (by their member-row key) into the huddle the reader
    /// is seated in.
    InviteToHuddle(String),
    HuddleInviteSent(String),
    HuddleInviteFailed(String, crate::backend::OptimisticMutationError),
    SecretTyped(String, String),
    ChannelDraftChanged(String),
}
impl ::std::fmt::Debug for AppMessage {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("AppMessage")
    }
}
impl Ducktape {
    pub(crate) fn is_dark(&self) -> bool {
        match self.appearance {
            Appearance::Dark => true,
            Appearance::Light => false,
            Appearance::System => self.system_dark,
        }
    }
    /// The updater's reading, when the app was started through the launcher.
    pub(crate) fn update_reading(&self) -> Option<crate::backend::update::UpdateReading> {
        self.updater.as_ref().map(|updater| updater.reading())
    }
    /// The console's update strip, if a phase draws one.
    pub(crate) fn update_strip(&self) -> Option<crate::backend::update::UpdateStrip> {
        crate::backend::update::strip_of(self.update_reading().as_ref())
    }
    /// The Settings "Updates" section's facts.
    pub(crate) fn update_facts(&self) -> crate::backend::update::UpdateFacts {
        crate::backend::update::facts_of(self.update_reading().as_ref(), self.wall_now)
    }
    pub(crate) fn initial_state() -> Self {
        Self {
            appearance_save_generation: 0,
            appearance_save_task: None,
            connection_generation: 0,
            connection_task: None,
            dm_peers_load_generation: 0,
            dm_peers_load_task: None,
            node_facts_load_generation: 0,
            node_facts_load_task: None,
            bell_load_generation: 0,
            bell_load_task: None,
            members_load_generation: 0,
            members_load_task: None,
            settings_load_generation: 0,
            settings_load_task: None,
            account_load_generation: 0,
            account_load_task: None,
            live_resync_generation: 0,
            live_resync_task: None,
            bell_presentations_generation: 0,
            bell_presentations_task: None,
            account_qr_auth_generation: 0,
            account_qr_auth_task: None,
            account_desktop_auth_generation: 0,
            account_desktop_auth_task: None,
            notifications_save_generation: 0,
            notifications_save_task: None,
            bell_head_generation: 0,
            bell_head_task: None,
            bell_navigation_generation: 0,
            bell_navigation_task: None,
            palette_search_generation: 0,
            palette_search_task: None,
            channel_window_load_generation: 0,
            channel_window_load_task: None,
            appearance_load_generation: 0,
            appearance_load_task: None,
            notifications_load_generation: 0,
            notifications_load_task: None,
            hub_load_generation: 0,
            hub_load_task: None,
            network_probe_generation: 0,
            network_probe_task: None,
            welcome_account_load_generation: 0,
            welcome_account_load_task: None,
            chain_identity_load_generation: 0,
            chain_identity_load_task: None,
            wallets_load_generation: 0,
            wallets_load_task: None,
            welcome_qr_auth_generation: 0,
            welcome_qr_auth_task: None,
            welcome_desktop_auth_generation: 0,
            welcome_desktop_auth_task: None,
            provision_progress_generation: 0,
            provision_progress_task: None,
            appearance: Appearance::System,
            system_dark: false,
            desktop_notifications: true,
            wall_now: (crate::backend::current_wall_seconds()),
            rpc: "".to_owned(),
            connected_rpc: "".to_owned(),
            password: "".to_owned(),
            status: "Connecting…".to_owned(),
            connected: false,
            updater: crate::backend::update::Updater::from_env(),
            loading: false,
            views_live_serial: 0,
            cmd_held: false,
            shift_held: false,
            focused_win: None,
            block_height: (-1),
            hydration_generation: 0,
            connect_generation: 0,
            signer_key: "".to_owned(),
            hydration_retry_attempt: 0,
            mutation_phase: MutationPhase::Idle,
            error: "".to_owned(),
            startup_duck_link: crate::backend::startup_duck_url(),
            channels: Vec::new(),
            rooms: Vec::new(),
            chat_generation: 0,
            channel_reads: Vec::new(),
            unread_boundary: 0,
            active_channel: "".to_owned(),
            active_channel_name: "".to_owned(),
            active_channel_archived: false,
            active_channel_members_only: false,
            channel_members: Vec::new(),
            post_refusal: "".to_owned(),
            chat_land_seq: 0,
            chat_edit_seq: 0,
            chat_edit_rev: 0,
            live_agents: Vec::new(),
            chat_sent_serial: 0,
            chat_pending_sends: Vec::new(),
            chat_copy_chord_serial: 0,
            channel_draft: "".to_owned(),
            channel_create_open: false,
            channel_create_members_only: false,
            channel_create_voice: false,
            pending_channel: "".to_owned(),
            chat_at_tail: true,
            history_view: false,
            chat_chain_id: "".to_owned(),
            dm_peers: Vec::new(),
            dm_rows: Vec::new(),
            dm_peers_generation: 0,
            active_dm_peer: "".to_owned(),
            active_dm: crate::backend::no_dm_peer(),
            shell_tab: ShellTab::Chat,
            members_answered: false,
            members_rows: Vec::new(),
            members_generation: 0,
            gov_open: 0,
            agents_open_run: "".to_owned(),
            agents_opened: 0,
            agents_live: false,
            forge_note_pending: "".to_owned(),
            forge_link: "".to_owned(),
            forge_link_tick: 0,
            node_key: "".to_owned(),
            node_data_dir: "".to_owned(),
            settings_key_path: "".to_owned(),
            settings_key_state: "".to_owned(),
            settings_user_key: "".to_owned(),
            settings_generation: 0,
            account_exists: false,
            account_number: "".to_owned(),
            account_name: "".to_owned(),
            account_bio: "".to_owned(),
            account_generation: 0,
            account_busy: false,
            account_ticket: "".to_owned(),
            account_banner_dismissed: false,
            account_ceremony_phase: "".to_owned(),
            account_ceremony_qr: "".to_owned(),
            account_ceremony_detail: "".to_owned(),
            account_ceremony_left: "".to_owned(),
            node_version: "".to_owned(),
            node_root_hash: "".to_owned(),
            network_chain_id: "".to_owned(),
            node_last_finalized: (-1),
            node_checkpoint: (-1),
            node_height: (-1),
            node_phase: "".to_owned(),
            node_phase_since: (-1),
            node_sync_target: (-1),
            node_sync_applied: (-1),
            node_sync_retries: 0,
            node_sync_failures: 0,
            node_sync_last_error: "".to_owned(),
            node_view_label: "—".to_owned(),
            node_quorum_label: "—".to_owned(),
            node_reachable_label: "—".to_owned(),
            fs_drop_dir: "/shared".to_owned(),
            fs_dropping: false,
            fs_route: "".to_owned(),
            fs_route_serial: 0,
            palette_open: false,
            bell_open: false,
            bell_unread: 0,
            bell_items: Vec::new(),
            bell_presentations: Vec::new(),
            bell_read_through: 0,
            bell_clear_through: 0,
            bell_marking: false,
            bell_error: "".to_owned(),
            palette_draft: "".to_owned(),
            palette_search_phase: SearchPhase::Idle,
            palette_chat_hits: Vec::new(),
            palette_page_hits: Vec::new(),
            toast: "".to_owned(),
            toast_age: 0,
            page_route: "".to_owned(),
            page_route_serial: 0,
            onboarding_win: None,
            console_win: None,
            huddle_win: None,
            network_name: "".to_owned(),
            hub_step: HubStep::Loading,
            hub_networks: Vec::new(),
            hub_selected: "".to_owned(),
            hub_wallets: Vec::new(),
            hub_wallet_selected: "".to_owned(),
            onboarding_name: "".to_owned(),
            onboarding_error: "".to_owned(),
            wallet_opening_status: "Unlocking wallet…",
            console_entry: ConsoleEntry::Idle,
            connection_progress: String::new(),
            invite_link: "".to_owned(),
            provision_steps: Vec::new(),
            provision_index: 0,
            hub_chain_id: "".to_owned(),
            welcome_name_draft: "".to_owned(),
            ceremony_phase: "".to_owned(),
            ceremony_qr: "".to_owned(),
            ceremony_detail: "".to_owned(),
            ceremony_left: "".to_owned(),
            huddle_joined: false,
            huddle_channel: "".to_owned(),
            huddle_channel_name: "".to_owned(),
            huddle_joined_at: 0,
            huddle_now: 0,
            call_status: "".to_owned(),
            call_muted: false,
            call_speaking: false,
            call_peers: Vec::new(),
            call_camera: false,
            call_sharing: false,
            call_video_live: false,
            huddle_stage: "".to_owned(),
            huddle_roster: Vec::new(),
            huddle_rows: Vec::new(),
            huddle_invitees: Vec::new(),
            secrets: Default::default(),
        }
    }
    fn open_onboarding(&mut self) -> Task<AppMessage> {
        let (_, task) = crate::shell::open(crate::shell::WindowKind::Onboarding);
        task.map(AppMessage::OnboardingOpened)
    }
    pub(crate) fn subscriptions(&self) -> ducktape_view_guest::Subscription<AppMessage> {
        use ducktape_view_guest::Subscription;
        let mut subscriptions = Vec::new();
        if self.connected {
            subscriptions.push(
                Subscription::run_with(self.connected_rpc.clone(), |rpc: &String| {
                    crate::backend::live_events(rpc.clone())
                })
                .map(AppMessage::LiveUpdated),
            );
            subscriptions.push(
                Subscription::run_with(self.connected_rpc.clone(), |rpc: &String| {
                    crate::backend::node_status_live(rpc.clone())
                })
                .map(AppMessage::NodeStatusPushed),
            );
            subscriptions.push(
                Subscription::run_with(
                    (
                        self.connected_rpc.clone(),
                        self.network_chain_id.clone(),
                        self.connect_generation,
                        self.signer_key.clone(),
                    ),
                    |data: &(String, String, i64, String)| {
                        crate::backend::chat_live_agents(
                            data.0.clone(),
                            data.1.clone(),
                            data.2,
                            data.3.clone(),
                        )
                    },
                )
                .map(AppMessage::LiveAgentsEvent),
            );
        }
        let active_call = self.connected && self.huddle_joined && !self.huddle_channel.is_empty();
        if active_call {
            subscriptions.push(
                Subscription::run_with(
                    (self.connected_rpc.clone(), self.huddle_channel.clone()),
                    |data: &(String, String)| {
                        crate::call::call_session(data.0.clone(), data.1.clone())
                    },
                )
                .map(AppMessage::CallEvent),
            );
        }
        if self.huddle_joined {
            subscriptions.push(Subscription::run(crate::shell::seconds).map(|()| AppMessage::Tick));
        }
        if self.console_win.is_some() {
            subscriptions
                .push(Subscription::run(crate::shell::seconds).map(|()| AppMessage::WallTick));
        }
        let toast_visible = !self.toast.is_empty();
        if toast_visible {
            subscriptions
                .push(Subscription::run(crate::shell::toast_ticks).map(|()| AppMessage::ToastTick));
        }
        subscriptions
            .push(Subscription::run(crate::module_view::notices).map(AppMessage::ViewNotice));
        Subscription::batch(subscriptions)
    }
    pub(crate) fn boot() -> (Self, Task<AppMessage>) {
        let mut state = Self::initial_state();
        let task = state.open_onboarding();
        (state, task)
    }

    #[cfg(test)]
    pub(crate) fn fixture_live_agent_session() -> (Self, Task<AppMessage>) {
        let mut state = Self::initial_state();
        state.connected = true;
        state.connected_rpc = "http://127.0.0.1:8844".to_owned();
        state.network_chain_id = "testnet#abcd".to_owned();
        state.connect_generation = 7;
        state.signer_key = "aa11".to_owned();
        state.shell_tab = ShellTab::Chat;
        state.live_agents = vec![crate::backend::LiveAgentRow {
            channel_id: "channel-a".to_owned(),
            anchor_seq: 2,
            run_id: "chat:2:agent-1".to_owned(),
            agent: "Chief Duck".to_owned(),
            status: "Reading the repo".to_owned(),
            ..Default::default()
        }];
        (state, Task::none())
    }
    #[cfg(test)]
    pub(crate) fn fixture_account_ceremony() -> (Self, Task<AppMessage>) {
        let mut state = Self::initial_state();
        state.connected = true;
        state.connected_rpc = "http://127.0.0.1:8844".to_owned();
        state.network_chain_id = "testnet#abcd".to_owned();
        state.connect_generation = 7;
        state.signer_key = "aa11".to_owned();
        state.shell_tab = ShellTab::Settings;
        state.account_ceremony_phase = "qr".to_owned();
        state.account_ceremony_qr = "otpauth://totp/demo".to_owned();
        (state, Task::none())
    }
}
#[path = "native_view.rs"]
mod native_view;
#[path = "app_update.rs"]
mod update;
#[cfg(test)]
mod state_tests {
    use super::*;
    use futures::{FutureExt, StreamExt};
    fn dispatch(state: &mut Ducktape, message: AppMessage) {
        let mut tasks = vec![state.update(message).into_stream()];
        while let Some(mut stream) = tasks.pop() {
            while let Some(Some(message)) = stream.next().now_or_never() {
                tasks.push(state.update(message).into_stream());
            }
        }
    }
    #[test]
    fn only_the_current_appearance_load_can_change_the_theme() {
        let mut state = Ducktape::initial_state();
        state.appearance = Appearance::System;
        state.appearance_load_generation = 2;
        dispatch(
            &mut state,
            AppMessage::AppearanceLoadReply(
                1,
                Box::new(AppMessage::AppearanceLoaded(Appearance::Dark)),
            ),
        );
        assert_eq!(state.appearance, Appearance::System);
        dispatch(
            &mut state,
            AppMessage::AppearanceLoadReply(
                2,
                Box::new(AppMessage::AppearanceLoaded(Appearance::Dark)),
            ),
        );
        assert_eq!(state.appearance, Appearance::Dark);
    }
    #[test]
    fn an_unlock_in_place_drops_the_previous_keys_live_rows() {
        let (mut state, _) = Ducktape::fixture_live_agent_session();
        let actual = !(state.live_agents).is_empty();
        assert!(actual);
        let reply_message = AppMessage::SettingsUnlocked("bb22".to_owned());
        dispatch(&mut state, reply_message);
        let actual = state.signer_key.to_owned();
        let expected = "bb22".to_owned();
        assert_eq!(actual, expected);
        let actual = (state.live_agents).is_empty();
        assert!(actual);
    }
    #[test]
    fn locking_the_seat_takes_the_private_output_with_it() {
        let (mut state, _) = Ducktape::fixture_live_agent_session();
        let actual = !(state.live_agents).is_empty();
        assert!(actual);
        let reply_message = AppMessage::SettingsViewEvent(crate::module_view::view_event(
            "lock".to_owned(),
            "".to_owned(),
        ));
        dispatch(&mut state, reply_message);
        let actual = (state.signer_key).is_empty();
        assert!(actual);
        let actual = (state.password).is_empty();
        assert!(actual);
        let actual = (state.live_agents).is_empty();
        assert!(actual);
    }
    #[tokio::test]
    async fn a_chat_address_opened_from_another_tab_lands_on_the_chat_tab() {
        let (mut state, _) = Ducktape::fixture_live_agent_session();
        let reply_message = AppMessage::SelectShellTab(ShellTab::Agents);
        dispatch(&mut state, reply_message);
        let actual = state.shell_tab.clone();
        let expected = ShellTab::Agents;
        assert_eq!(actual, expected);
        let reply_message = AppMessage::OpenMessageLink("duck://channel/general".to_owned());
        dispatch(&mut state, reply_message);
        let actual = state.shell_tab.clone();
        let expected = ShellTab::Chat;
        assert_eq!(actual, expected);
    }
    #[test]
    fn a_run_opened_mid_ceremony_retires_the_ceremony_like_any_tab_move() {
        let (mut state, _) = Ducktape::fixture_account_ceremony();
        let actual = !(state.account_ceremony_phase).is_empty();
        assert!(actual);
        let reply_message = AppMessage::OpenRunPanel("dispatch-1".to_owned());
        dispatch(&mut state, reply_message);
        let actual = state.shell_tab.clone();
        let expected = ShellTab::Agents;
        assert_eq!(actual, expected);
        let actual = state.agents_open_run.to_owned();
        let expected = "dispatch-1".to_owned();
        assert_eq!(actual, expected);
        let actual = (state.account_ceremony_phase).is_empty();
        assert!(actual);
        let actual = (state.account_ceremony_qr).is_empty();
        assert!(actual);
    }
}
