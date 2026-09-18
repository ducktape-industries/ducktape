//! `ducktape module …` — the operator's side of a live code swap.
//!
//! `update`/`register` drive the governance proposal that schedules the swap
//! FIRST, then stage the component at this node's owner-gated admin route
//! (which fans it out to every validator and returns their receipts). That
//! order is load-bearing: the code plane admits only a digest consensus names,
//! and for a brand-new artifact the open proposal is the only thing that names
//! it (#1861). A holdout receipt is a diagnostic — the swap's readiness quorum,
//! not the fan-out, is what holds activation. `status` reads the modules
//! registry. `pack` prepares the same artifact offline, including in a run's
//! guest. Nothing here runs inside the node.

use std::path::PathBuf;

use commonware_cryptography::Signer as _;

use crate::cli::{CeremonyOutcome, GovSigner, rpc_call, rpc_query};
use crate::cli_args::{Selector, StatusArgs};
use crate::config::{self, hex_bytes};

type CommandResult = Result<(), Box<dyn std::error::Error>>;

/// how long a stage waits on the node's answer. the node answers only once its
/// fan-out to every validator settles and sets no deadline of its own, so this
/// is the one bound on a wedged push; it has to cover a full-size component
/// reaching the slowest validator.
const STAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// the `module` family's verbs.
#[derive(Debug, clap::Subcommand)]
pub enum ModuleCmd {
    /// package a component and optional mapper offline; print the deployment SHA-256
    Pack(PackArgs),
    /// schedule a code swap for a registered module
    Update(StageArgs),
    /// register a new module id with its first code
    Register(StageArgs),
    /// the modules registry: active code and any pending swap per module
    Status(StatusArgs),
}

#[derive(Debug, clap::Args)]
pub struct PackArgs {
    #[arg(value_name = "COMPONENT.WASM")]
    pub component: PathBuf,
    /// Optional mapper included in the deployment; omission removes it on activation.
    #[arg(long, value_name = "INDEX.WASM")]
    pub index: Option<PathBuf>,
    /// View deployed with the module; omission removes its UI.
    #[arg(long, value_name = "VIEW.WASM")]
    pub view: Option<PathBuf>,
    /// View assets rooted here, using canonical relative paths.
    #[arg(long, value_name = "ASSETS", requires = "view")]
    pub assets: Option<PathBuf>,
    /// The module's data-plane lane declaration (JSON). A module that needs
    /// no lane passes none, which is most of them.
    #[arg(long, value_name = "LANES.JSON")]
    pub lanes: Option<PathBuf>,
    /// Write the canonical deployment artifact here.
    #[arg(long, value_name = "ARTIFACT")]
    pub out: PathBuf,
}

/// `<id> [<component.wasm>] [--index <index.wasm>] [--view <view.wasm>] [--after N]`
/// — shared by update and register. A component (with or without `--view`)
/// stages a `module` entry; `--view` with no component stages a `view` entry
/// — a UI with no consensus code, drawn by the desktop off the registry.
#[derive(Debug, clap::Args)]
pub struct StageArgs {
    /// the module id the code belongs to
    #[arg(value_name = "ID")]
    pub id: String,
    /// the component bytes to stage; omit it (with --view) for a view-only entry
    #[arg(value_name = "COMPONENT.WASM", required_unless_present = "view")]
    pub component: Option<PathBuf>,
    /// Optional mapper deployed and activated with this component; omission removes it.
    #[arg(long, value_name = "INDEX.WASM", requires = "component")]
    pub index: Option<PathBuf>,
    /// View deployed with the module (omission removes its UI), or, with no
    /// component, the whole view-only deployment.
    #[arg(long, value_name = "VIEW.WASM")]
    pub view: Option<PathBuf>,
    /// View assets rooted here, using canonical relative paths.
    #[arg(long, value_name = "ASSETS", requires = "view")]
    pub assets: Option<PathBuf>,
    /// The module's data-plane lane declaration (JSON): the lanes this
    /// deployment asks the network for. It rides the artifact frame, so the
    /// hash governance votes on covers the lanes as well as the code.
    #[arg(long, value_name = "LANES.JSON")]
    pub lanes: Option<PathBuf>,
    /// blocks after the proposal's EXECUTE height (not this node's height
    /// right now) at which the swap activates — the same value for every
    /// member co-signing the same proposal, whatever height each one is at
    /// when it runs this command.
    #[arg(long, default_value_t = 50)]
    pub after: u64,
    #[command(flatten)]
    pub selector: Selector,
}

/// dispatch one `module` verb.
pub fn run(cmd: ModuleCmd) -> CommandResult {
    match cmd {
        ModuleCmd::Pack(args) => cmd_pack(args),
        ModuleCmd::Update(args) => cmd_update(args),
        ModuleCmd::Register(args) => cmd_register(args),
        ModuleCmd::Status(args) => cmd_status(args),
    }
}

fn cmd_pack(args: PackArgs) -> CommandResult {
    let artifact = workspace_config::read_deployment_files(
        Some(&args.component),
        args.index.as_deref(),
        args.view.as_deref(),
        args.assets.as_deref(),
        args.lanes.as_deref(),
    )?;
    std::fs::write(&args.out, artifact.encode())?;
    println!("{}", hex_bytes(&artifact.hash()));
    Ok(())
}

/// which staging verb is running — the only difference between the two is
/// the governance action they propose.
#[derive(Clone, Copy, Debug)]
enum Verb {
    Update,
    Register,
}

impl Verb {
    /// the two-token spelling the ceremony narrates (`ducktape module update …`).
    fn name(self) -> &'static str {
        match self {
            Verb::Update => "module update",
            Verb::Register => "module register",
        }
    }

    /// the governance action for this verb; `<id>@<12hex>` names the proposal
    /// (governance only requires the name be non-empty).
    fn action(
        self,
        module_id: &str,
        kind: modules::Kind,
        activation_lead: u64,
        code_hash: [u8; 32],
        lanes: Vec<modules::LaneDecl>,
    ) -> governance::GovAction {
        let name = format!("{module_id}@{}", short(&code_hash));
        let module_id = module_id.to_string();
        let code_hash = code_hash.to_vec();
        match self {
            // a swap keeps the entry's kind: the registry fixed it at admission.
            Verb::Update => governance::GovAction::UpdateModule {
                name,
                module_id,
                activation_lead,
                code_hash,
            },
            Verb::Register => governance::GovAction::RegisterModule {
                name,
                module_id,
                kind,
                activation_lead,
                code_hash,
                // read off the ARTIFACT FRAME, never typed at the command
                // line: the hash governance votes on covers the declaration,
                // so what a member approves and what the registry admits are
                // the same bytes.
                lanes,
            },
        }
    }
}

fn cmd_update(args: StageArgs) -> CommandResult {
    cmd_stage_and_schedule(args, Verb::Update)
}

fn cmd_register(args: StageArgs) -> CommandResult {
    cmd_stage_and_schedule(args, Verb::Register)
}

/// the ceremony's "same proposal" test for a module verb: the same variant,
/// module, code AND lead is the same proposal. `activation_lead` is relative
/// to the EXECUTE height, not whichever member's height happened to be
/// current when it proposed — so unlike the old absolute-height scheme, this
/// never goes stale while a ballot is outstanding; a member joins an open
/// proposal for as long as it stays Open, whatever height it runs at.
fn matches_module_action<'a>(
    verb: Verb,
    module_id: &'a str,
    code_hash: &'a [u8],
    activation_lead: u64,
) -> impl Fn(&governance::GovAction) -> bool + 'a {
    use governance::GovAction;
    move |action| match (verb, action) {
        (
            Verb::Update,
            GovAction::UpdateModule {
                module_id: m,
                code_hash: h,
                activation_lead: lead,
                ..
            },
        )
        | (
            Verb::Register,
            GovAction::RegisterModule {
                module_id: m,
                code_hash: h,
                activation_lead: lead,
                ..
            },
        ) => m == module_id && h.as_slice() == code_hash && *lead == activation_lead,
        (Verb::Update, _) | (Verb::Register, _) => false,
    }
}

/// `module update|register <id> <component.wasm> [--index <index.wasm>] [--after N]`: check the
/// registry's static rules, drive the governance proposal that schedules the
/// swap at its execute height + N, read the registry back for its verdict, and
/// stage the bytes at this node last (fan-out to every validator, holdouts
/// reported).
fn cmd_stage_and_schedule(args: StageArgs, verb: Verb) -> CommandResult {
    config::validate_module_id(&args.id)?;
    // the static half of the registry's lead rule, checked before anything is
    // staged or proposed: a lead at or under the floor can never schedule,
    // whatever height Execute lands at (governance validates this again at
    // Propose — this is just an earlier, friendlier refusal).
    let lead_too_short = args.after <= modules::MIN_SWAP_LEAD;
    if lead_too_short {
        return Err(format!(
            "--after {} cannot schedule anything: activation must exceed \
             execute-height+MIN_SWAP_LEAD ({}) (the default is 50)",
            args.after,
            modules::MIN_SWAP_LEAD
        )
        .into());
    }
    let artifact = workspace_config::read_deployment_files(
        args.component.as_deref(),
        args.index.as_deref(),
        args.view.as_deref(),
        args.assets.as_deref(),
        args.lanes.as_deref(),
    )?;
    // the frame says what the entry is: a component makes a module frame, a
    // view alone a view frame — and the registry entry is registered as that.
    let kind = noded::compose::artifact_kind(&artifact.encode())?;
    // and it says which lanes the deployment asks for, for the same reason.
    let declared_lanes = noded::compose::artifact_lanes(&artifact.encode())?;
    let bytes = artifact.encode();
    let cfg_path = args.selector.config_path()?;
    let resolved = config::resolve(&cfg_path)?;
    let node = crate::cli::DrivenNode::of(&resolved, verb.name())?;
    let rpc_addr = node.rpc();
    let http_base = node.http_base();
    let code_hash: [u8; 32] = {
        use sha2::Digest as _;
        sha2::Sha256::digest(&bytes).into()
    };

    // 1. the registry's static rules, before anything is staged or proposed:
    //    each would reject governance's execute in-kernel and leave a
    //    proposal open for its whole voting period.
    let live_modules = read_live_modules(rpc_addr)?;
    let precheck = registry_precheck(
        verb,
        &read_module_status(rpc_addr)?,
        &live_modules,
        &args.id,
        kind,
        &code_hash,
    )?;
    match precheck {
        Precheck::Proceed => {}
        // a member running the verb after the deciding ballot (or after the
        // activation): the registry already holds these bytes, so there is no
        // proposal to join or mint.
        Precheck::AlreadyHeld(held) => {
            eprintln!(
                "{} → {} is already {} — nothing to do; track with: ducktape module status",
                args.id,
                hex_bytes(&code_hash),
                held.word()
            );
            return Ok(());
        }
    }

    // 2. the ceremony, BEFORE the bytes move: join an open proposal for the
    //    same (verb, id, hash, lead) or propose; cast yes; execute when
    //    decidable. it runs first because an OPEN proposal is what makes the
    //    digest admissible on every peer (#1861) — the code plane refuses a
    //    push nothing in consensus names, so staging a brand-new artifact
    //    ahead of its proposal collected refusals, never receipts. `--after`
    //    is the activation_lead governance applies RELATIVE TO THE EXECUTE
    //    HEIGHT (#1775) — every member co-signing the same proposal passes the
    //    same number, so no per-member height computation is needed here.
    let signer = crate::cli::gov_signer(rpc_addr, &cfg_path, &resolved)?;
    let pubkey_hex = signer_pubkey_hex(&signer);
    let matches = matches_module_action(verb, &args.id, &code_hash, args.after);
    let ceremony = crate::cli::drive_proposal_ceremony(
        &node,
        &signer,
        &pubkey_hex,
        &pubkey_hex,
        verb.name(),
        "module:",
        verb.action(&args.id, kind, args.after, code_hash, declared_lanes),
        &matches,
    );
    let outcome = match ceremony {
        Ok(outcome) => outcome,
        Err(error) => return Err(ceremony_failed(rpc_addr, &args.id, &code_hash, error)),
    };

    // 3. the registry's verdict on the ceremony, read BEFORE the bytes move.
    match outcome {
        // this run's ballot did not decide: there is no schedule to read yet,
        // and it stages anyway so its peers hold the bytes when one does.
        CeremonyOutcome::AwaitingBallots => {}
        // the read has to happen here, not after the fan-out: a swap's whole
        // activation lead can elapse while a large artifact streams to every
        // member, and the read would then name an ACTIVE module rather than
        // the schedule this run just made. `?` because a passed proposal the
        // registry REFUSED leaves nothing to distribute either — no swap
        // names these bytes, so every peer would refuse the push.
        CeremonyOutcome::Passed => confirm_scheduled(rpc_addr, &args.id, &code_hash)?,
    }

    // 4. stage + fan-out, now that the digest is referenced. the token lives
    //    in the node's workspace — its `storage_dir` in the dev shape, which
    //    is NOT the config file's directory.
    let reply = stage_component(http_base, &resolved.service.workspace, &bytes)?;
    digest_matches(&reply, &bytes)?;
    eprintln!(
        "staged {} ({} bytes), {} peer receipt(s)",
        hex_bytes(&code_hash),
        reply.len,
        reply.receipts.len()
    );
    // the fan-out never pushes to the staging node itself, so the one valset
    // member allowed no receipt is THIS NODE's key — not the governance
    // signer's, which under share governance is an account key.
    let me_hex = hex_bytes(resolved.signer.public_key().as_ref());
    let members = crate::cli::read_members(rpc_addr)?;
    note_non_member_holdouts(&reply, &members);
    report_validator_holdouts(&reply, &members, &me_hex);
    Ok(())
}

/// a passed proposal only ASKED the registry; the CLI's success line is the
/// registry's word, not governance's. read it back before saying "scheduled" —
/// the HEIGHT too: a run that joined an open proposal decided the FIRST
/// proposer's target, not the one it computed from its own height, so its own
/// number would name a block the swap never lands on.
fn confirm_scheduled(rpc_addr: &str, id: &str, code_hash: &[u8; 32]) -> CommandResult {
    let held = registry_holds(&read_module_status(rpc_addr)?, id, code_hash);
    match held {
        Some(Held::Pending { activation_height }) => {
            println!(
                "scheduled {id} → {} at height {activation_height}; track with: ducktape module status",
                hex_bytes(code_hash)
            );
            Ok(())
        }
        // the swap crossed its own activation between the execute and this
        // read: the registry holds the bytes as ACTIVE and there is no pending
        // height left to name.
        Some(Held::Active) => {
            println!(
                "{id} → {} is active; track with: ducktape module status",
                hex_bytes(code_hash)
            );
            Ok(())
        }
        None => Err(format!(
            "proposal passed but the modules registry holds no swap for {id} → {}. {}",
            hex_bytes(code_hash),
            registry_rules()
        )
        .into()),
    }
}

/// the ceremony failed. the one failure the registry causes: governance's
/// `Execute` emits the schedule to the modules registry in the SAME op, so a registry
/// refusal rejects the whole op — the proposal never settles and the ceremony
/// times out waiting for the tally. the proposal carries no reason, so on
/// exactly that failure, with the registry holding nothing for these bytes,
/// the rules are named here. every other ceremony error passes through.
fn ceremony_failed(
    rpc_addr: &str,
    id: &str,
    code_hash: &[u8; 32],
    error: Box<dyn std::error::Error>,
) -> Box<dyn std::error::Error> {
    let tally_never_settled = error.to_string() == crate::cli::TALLY_SETTLE_TIMEOUT;
    if !tally_never_settled {
        return error;
    }
    let Ok(modules) = read_module_status(rpc_addr) else {
        return error;
    };
    let scheduled = registry_holds(&modules, id, code_hash).is_some();
    if scheduled {
        return error;
    }
    format!(
        "{error}: governance's execute was refused by the modules registry, which holds no swap \
         for {id} → {}. {}",
        hex_bytes(code_hash),
        registry_rules()
    )
    .into()
}

/// what the committed registry lets this verb do with these bytes.
#[derive(Debug)]
enum Precheck {
    /// nothing in the registry stands in the way — stage and propose.
    Proceed,
    /// the registry already carries these bytes for the module: a member
    /// running after the deciding ballot, or after the activation. nothing to
    /// do — and the two are different facts, so the line names which.
    AlreadyHeld(Held),
}

/// how the registry carries a code hash for a module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    /// the module is RUNNING this code.
    Active,
    /// a scheduled swap will activate this code at its height.
    Pending { activation_height: u64 },
}

impl Held {
    /// the word the operator's "nothing to do" line uses.
    fn word(self) -> &'static str {
        match self {
            Held::Active => "active",
            Held::Pending { .. } => "scheduled",
        }
    }
}

/// the registry's STATIC rules (`modules` `handle_schedule_swap` /
/// `handle_schedule_register`), decided from the committed registry before
/// anything is staged or proposed. each refusal here would otherwise reject
/// governance's execute in-kernel and leave a proposal open for its whole
/// voting period; the wording mirrors the registry's own. pure.
fn registry_precheck(
    verb: Verb,
    modules: &[modules::ModuleCode],
    live_modules: &[String],
    id: &str,
    kind: modules::Kind,
    code_hash: &[u8; 32],
) -> Result<Precheck, String> {
    if let Some(held) = registry_holds(modules, id, code_hash) {
        return Ok(Precheck::AlreadyHeld(held));
    }
    // a swap can never change what an entry is: a module id takes module
    // frames, a view id view frames. every validator would refuse the
    // mismatched bytes at readiness and the swap would never latch.
    let entry = modules.iter().find(|m| m.module_id == id);
    if let Some(registered) = entry
        && registered.kind != kind
    {
        return Err(format!(
            "module {id} is registered as a {}, and a swap keeps its kind (a {} is a new id)",
            kind_word(registered.kind),
            kind_word(kind)
        ));
    }
    let already_live = live_modules.iter().any(|live| live == id);
    let registering_live_module = matches!(verb, Verb::Register) && already_live;
    if registering_live_module {
        return Err(format!(
            "module {id} is already registered (code changes go through `module update`)"
        ));
    }
    let other_swap_pending = entry.map(|m| m.pending.is_some());
    match (verb, other_swap_pending) {
        (Verb::Register, None) | (Verb::Update, Some(false)) => Ok(Precheck::Proceed),
        (Verb::Register, Some(true)) | (Verb::Register, Some(false)) => Err(format!(
            "module {id} is already registered (code changes go through `module update`)"
        )),
        (Verb::Update, None) => Err(format!(
            "cannot schedule a swap for unregistered module {id} (`module register` admits a new id)"
        )),
        (Verb::Update, Some(true)) => Err(format!(
            "module {id} already has a pending swap (cancel it first)"
        )),
    }
}

/// the registry's schedule rules, for a refusal it does not narrate itself.
fn registry_rules() -> String {
    format!(
        "its rules: --after (the activation lead) must exceed MIN_SWAP_LEAD ({}) — it is relative \
         to the EXECUTE height, so a long ballot never goes stale (the default is 50); one pending \
         swap per module (cancel it first); `register` needs an unregistered id and `update` a \
         registered one; the code must differ from the active code",
        modules::MIN_SWAP_LEAD
    )
}

/// how the registry carries `code_hash` for `id`, if it carries it at all.
/// pure: the one read behind both "nothing to do" and the post-`Passed`
/// confirmation.
fn registry_holds(modules: &[modules::ModuleCode], id: &str, code_hash: &[u8; 32]) -> Option<Held> {
    let entry = modules.iter().find(|m| m.module_id == id)?;
    let our_pending_swap = entry.pending.as_ref().filter(|p| p.code_hash == code_hash);
    if let Some(pending) = our_pending_swap {
        return Some(Held::Pending {
            activation_height: pending.activation_height,
        });
    }
    let already_active = entry.active_code_hash == code_hash;
    already_active.then_some(Held::Active)
}

/// The chain height, which is what tells a pending swap still in flight from
/// one whose activation passed without it.
fn read_height(rpc_addr: &str) -> Result<u64, String> {
    let reply = rpc_call(rpc_addr, &serde_json::json!({ "cmd": "status" }))?;
    if reply["ok"] != true {
        return Err(format!("status: {}", reply["error"]));
    }
    reply["status"]["height"]
        .as_u64()
        .ok_or_else(|| "node status carries no height".to_owned())
}

/// The running host determines which ids are occupied, including modules
/// with no code-registry record. A default build catalog says nothing about
/// the module set of this network.
fn read_live_modules(rpc_addr: &str) -> Result<Vec<String>, String> {
    let reply = rpc_call(rpc_addr, &serde_json::json!({ "cmd": "status" }))?;
    if reply["ok"] != true {
        return Err(format!("status: {}", reply["error"]));
    }
    let Some(modules) = reply["status"]["modules"].as_object() else {
        return Err("node status carries no module roster".into());
    };
    Ok(modules.keys().cloned().collect())
}

/// the signer's public key as hex: the proposal-id seed the ceremony mints
/// from (and nothing else — the receipt gate is keyed by the NODE's key).
pub(crate) fn signer_pubkey_hex(signer: &GovSigner) -> String {
    match signer {
        GovSigner::Node { key } => hex_bytes(key),
        GovSigner::User { key, .. } => hex_bytes(key.public_key().as_ref()),
    }
}

/// `module status [--config node.toml] [--json]`
fn cmd_status(args: StatusArgs) -> CommandResult {
    let cfg_path = args.selector.config_path()?;
    let resolved = config::resolve(&cfg_path)?;
    let rpc_addr = resolved
        .rpc_listen
        .clone()
        .ok_or("module status reads the node's local rpc — set `rpc_listen` in node.toml")?;
    let modules = read_module_status(&rpc_addr)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&modules)?);
        return Ok(());
    }
    let proposed = read_open_code_proposals(&rpc_addr)?;
    print!("{}", render_status(&modules, read_height(&rpc_addr)?));
    print!("{}", render_proposed(&proposed));
    Ok(())
}

/// one open code ballot: the proposal and the module and hash it would
/// install. every member may taste its view from the app while it is open,
/// so an operator sees "tasteable" here.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenCodeProposal {
    proposal_id: String,
    module_id: String,
    code_hash: Vec<u8>,
}

/// the open `UpdateModule` / `RegisterModule` proposals off the governance
/// register, in the register's order.
fn read_open_code_proposals(rpc_addr: &str) -> Result<Vec<OpenCodeProposal>, String> {
    use governance::{GovQuery, GovReply, decode_reply, encode_query};
    let raw = rpc_query(rpc_addr, "governance", &encode_query(&GovQuery::Proposals))?;
    match decode_reply(&raw)? {
        GovReply::Proposals(views) => Ok(open_code_proposals(&views)),
        other => Err(format!("expected Proposals, got {other:?}")),
    }
}

fn open_code_proposals(views: &[governance::ProposalView]) -> Vec<OpenCodeProposal> {
    use governance::{GovAction, ProposalStatus};
    views
        .iter()
        .filter(|view| view.status == ProposalStatus::Open)
        .filter_map(|view| {
            let (module_id, code_hash) = match &view.action {
                GovAction::UpdateModule {
                    module_id,
                    code_hash,
                    ..
                }
                | GovAction::RegisterModule {
                    module_id,
                    code_hash,
                    ..
                } => (module_id, code_hash),
                GovAction::AddValidator { .. }
                | GovAction::RemoveValidator { .. }
                | GovAction::Signal { .. }
                | GovAction::AddResident { .. }
                | GovAction::RemoveResident { .. }
                | GovAction::AdoptShares { .. }
                | GovAction::SetShares { .. }
                | GovAction::SetShareMode { .. }
                | GovAction::CancelModuleUpdate { .. }
                | GovAction::SetAclPolicy { .. } => return None,
            };
            Some(OpenCodeProposal {
                proposal_id: view.proposal_id.clone(),
                module_id: module_id.clone(),
                code_hash: code_hash.clone(),
            })
        })
        .collect()
}

/// the modules registry over the generic query lane — the same shape
/// `read_members` uses for governance.
pub(crate) fn read_module_status(rpc_addr: &str) -> Result<Vec<modules::ModuleCode>, String> {
    use modules::{ModulesQuery, ModulesReply, decode_reply, encode_query};
    let raw = rpc_query(
        rpc_addr,
        "modules",
        &encode_query(&ModulesQuery::ModuleStatus),
    )?;
    match decode_reply(&raw)? {
        ModulesReply::ModuleStatus { modules } => Ok(modules),
        other => Err(format!("expected ModuleStatus, got {other:?}")),
    }
}

/// what the node's stage route answers with: the digest it ingested, its
/// length, and one receipt per peer the code plane fanned the bytes out to.
#[derive(Debug, Clone, serde::Deserialize)]
struct StageReply {
    /// hex sha256 of the artifact as the node stored it.
    pub digest: String,
    /// the artifact's length in bytes, as the node counted it.
    pub len: u64,
    /// one row per member — empty when the node fanned nothing out.
    pub receipts: Vec<PeerReceipt>,
}

/// one peer's answer to the fan-out (`noded::module_code::CodePeerReceipt`).
#[derive(Debug, Clone, serde::Deserialize)]
struct PeerReceipt {
    /// hex of the peer's public key.
    pub peer: String,
    /// "stored" | "already-have" | the peer's refusal reason.
    pub status: String,
    /// whether that peer now holds the bytes.
    pub ok: bool,
}

/// Stage the bytes at this node's owner-gated admin route with fan-out on: the
/// node stores them, pushes them to every validator, and answers with the
/// digest and one receipt per peer. Loopback exposure wants the operator token
/// minted beside `node.toml` at boot.
fn stage_component(
    http_base: &str,
    workspace: &std::path::Path,
    bytes: &[u8],
) -> Result<StageReply, String> {
    const PATH: &str = "/v1/admin/module-code/stage";
    let token = noded::admin::read_operator_token(workspace)?;
    // the node answers only once the fan-out settles, and it awaits that with
    // no deadline of its own; a wedged push must surface here, not hang.
    let resp = reqwest::blocking::Client::builder()
        .timeout(STAGE_TIMEOUT)
        .build()
        .map_err(|e| format!("stage client: {e}"))?
        .post(format!("{http_base}{PATH}?fanout=true"))
        .header(noded::admin::ADMIN_TOKEN_HEADER, token)
        .header("content-type", "application/octet-stream")
        .body(bytes.to_vec())
        .send()
        .map_err(|error| crate::node_http::transport_failure(http_base, PATH, &error).to_string())?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    let refused = !status.is_success();
    if refused {
        return Err(format!("stage rejected ({status}): {text}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("stage reply: {e}: {text}"))
}

/// the sentence under the holdout table — one wording, whichever gate.
const HOLDOUTS_HEAL: &str = "the swap cannot ACTIVATE until every validator holds the bytes and signals ready; each \
     holdout fetches the committed artifact off a peer before the boundary. re-run to push \
     again once they are reachable (staging is idempotent)";

/// where one validator stands after the fan-out.
enum Standing<'a> {
    /// the staging node itself, or a peer whose receipt is ok.
    Holds,
    /// a peer the fan-out reached whose node refused or lost the transfer:
    /// the status token it reported.
    Refused(&'a str),
    /// a member the fan-out never reached — no row in the receipt table.
    Unreached,
}

/// `member_hex`'s standing: the staging node holds its own bytes (the fan-out
/// never pushes to itself), every other member is read off its receipt.
fn standing<'a>(reply: &'a StageReply, member_hex: &str, me_hex: &str) -> Standing<'a> {
    if member_hex == me_hex {
        return Standing::Holds;
    }
    let Some(receipt) = reply.receipts.iter().find(|r| r.peer == member_hex) else {
        return Standing::Unreached;
    };
    if receipt.ok {
        Standing::Holds
    } else {
        Standing::Refused(&receipt.status)
    }
}

/// A DIAGNOSTIC, not a gate (#1861). A swap only makes sense once every
/// VALIDATOR can run the code, and the readiness quorum that arms it is the
/// valset — but that gate lives in consensus (`pump_code_readiness` + the
/// swap's R = n readiness latch), not here: the proposal is already open by
/// the time the bytes move, and refusing to propose on a holdout would strand
/// the very record that makes the digest admissible. So the receipt table —
/// one row per overlay peer the fan-out reached, validators and residents
/// alike — is read through the member set and each holdout (a refused
/// transfer, or no receipt at all because the node never dialled it) is named
/// with the status its node reported.
fn holdout_rows<'a>(
    reply: &'a StageReply,
    members: &[Vec<u8>],
    me_hex: &str,
) -> Vec<(String, &'a str)> {
    members
        .iter()
        .map(|member| hex_bytes(member))
        .filter_map(|member| match standing(reply, &member, me_hex) {
            Standing::Holds => None,
            Standing::Refused(status) => Some((member, status)),
            Standing::Unreached => Some((member, "no receipt (unreachable)")),
        })
        .collect()
}

/// print [`holdout_rows`] for the operator; silent when every member holds it.
fn report_validator_holdouts(reply: &StageReply, members: &[Vec<u8>], me_hex: &str) {
    let holdouts = holdout_rows(reply, members, me_hex);
    let every_validator_holds_it = holdouts.is_empty();
    if every_validator_holds_it {
        return;
    }
    eprintln!("peer  status");
    for (peer, status) in holdouts {
        eprintln!("{peer}  {status}");
    }
    eprintln!("{HOLDOUTS_HEAL}");
}

/// A peer outside the valset (a resident, a sentry) that did not take the
/// bytes is noted, never a refusal: it is no part of the readiness quorum,
/// and a replica missing a committed artifact fetches it off a peer before
/// the boundary.
fn note_non_member_holdouts(reply: &StageReply, members: &[Vec<u8>]) {
    let member_hex: std::collections::BTreeSet<String> =
        members.iter().map(|member| hex_bytes(member)).collect();
    let non_member_holdouts = reply
        .receipts
        .iter()
        .filter(|r| !r.ok && !member_hex.contains(&r.peer));
    for receipt in non_member_holdouts {
        eprintln!(
            "note: peer {} is not a validator and did not take the bytes ({}); \
             it fetches them itself before the boundary",
            receipt.peer, receipt.status
        );
    }
}

/// The digest the proposal will carry is the sha256 of the bytes WE read; the
/// node's answer must agree or something between us rewrote the file.
fn digest_matches(reply: &StageReply, bytes: &[u8]) -> Result<[u8; 32], String> {
    use sha2::Digest as _;
    let ours: [u8; 32] = sha2::Sha256::digest(bytes).into();
    let theirs = config::unhex(&reply.digest).map_err(|e| format!("stage digest: {e}"))?;
    let agree = theirs[..] == ours[..];
    if !agree {
        return Err(format!(
            "stage digest {} is not the sha256 of the deployment we packaged ({})",
            reply.digest,
            hex_bytes(&ours)
        ));
    }
    Ok(ours)
}

/// how much of a code hash a row shows — enough to tell two builds apart at a
/// glance, short enough that the table stays one line per module.
const SHORT_HASH: usize = 12;

/// one row per module: `id  active  pending`. Either column is `—` when there
/// is nothing to show; pending is otherwise `<hash> ready <k|✓> activation <h>`,
/// or `<hash> DEAD activation <h>` for a swap whose height came and went with
/// readiness never latched — it can only be replaced or cancelled now.
fn render_status(modules: &[modules::ModuleCode], height: u64) -> String {
    let id_width = modules
        .iter()
        .map(|m| m.module_id.len())
        .max()
        .unwrap_or_default()
        .max(2);
    let mut out = format!(
        "{:<id_width$}  {:<KIND_WIDTH$}  {:<SHORT_HASH$}  pending\n",
        "id", "kind", "active"
    );
    for m in modules {
        let kind = kind_word(m.kind);
        // `module register` writes an EMPTY active hash and leaves it empty
        // until the swap activates — the first thing an operator looks at.
        let never_activated = m.active_code_hash.is_empty();
        let active = if never_activated {
            "—".to_string()
        } else {
            short(&m.active_code_hash)
        };
        let pending = match &m.pending {
            None => "—".to_string(),
            Some(swap) => format!(
                "{}  {}  activation {}",
                short(&swap.code_hash),
                readiness_word(swap, height),
                swap.activation_height
            ),
        };
        out.push_str(&format!(
            "{:<id_width$}  {kind:<KIND_WIDTH$}  {active:<SHORT_HASH$}  {pending}\n",
            m.module_id
        ));
    }
    out
}

/// the open code ballots under the table, one line each: `proposed
/// <module>  <hash>  proposal <id>  (tasteable)` — nothing when none is open.
fn render_proposed(proposed: &[OpenCodeProposal]) -> String {
    if proposed.is_empty() {
        return String::new();
    }
    let id_width = proposed
        .iter()
        .map(|p| p.module_id.len())
        .max()
        .unwrap_or_default();
    let mut out = String::from("\nopen code proposals (tasteable from the app while open):\n");
    for p in proposed {
        out.push_str(&format!(
            "{:<id_width$}  {:<SHORT_HASH$}  proposal {}\n",
            p.module_id,
            short(&p.code_hash),
            p.proposal_id
        ));
    }
    out
}

/// the `kind` column's width: the longer of its two words.
const KIND_WIDTH: usize = 6;

/// the registry kind as the status row prints it.
fn kind_word(kind: modules::Kind) -> &'static str {
    match kind {
        modules::Kind::Module => "module",
        modules::Kind::View => "view",
    }
}

/// how far a pending swap's readiness has come: the count of validators that
/// signalled, or `✓` once the latch covered the whole set — and `DEAD` once
/// its activation height has passed without that latch. A dead pending never
/// arms, so an operator reading `ready 1` forever has to be told the swap is
/// over and has to be re-proposed, not waited on.
fn readiness_word(swap: &modules::ScheduledSwap, height: u64) -> String {
    if swap.stale_at(height) {
        return "DEAD".into();
    }
    if swap.ready_at.is_some() {
        return "ready ✓".into();
    }
    format!("ready {}", swap.readiness.len())
}

fn short(hash: &[u8]) -> String {
    hex_bytes(hash).chars().take(SHORT_HASH).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use modules::{ModuleCode, ScheduledSwap};

    #[test]
    fn status_lists_the_open_code_proposals_as_tasteable() {
        use governance::{GovAction, ProposalStatus, ProposalView, VoterKind, VotingRule};
        let view = |id: &str, action: GovAction, status: ProposalStatus| ProposalView {
            proposal_id: id.into(),
            action,
            proposer: vec![1],
            created_at: 1,
            deadline: 9,
            status,
            votes: Vec::new(),
            voter_kind: VoterKind::ValidatorNode,
            electorate: Vec::new(),
            voting_rule: VotingRule::Threshold { required_yes: 1 },
        };
        let update = |module: &str| GovAction::UpdateModule {
            name: "n".into(),
            module_id: module.into(),
            activation_lead: 10,
            code_hash: vec![0xcd; 32],
        };
        let views = vec![
            view("chat-1", update("chat"), ProposalStatus::Open),
            view("chat-0", update("chat"), ProposalStatus::Passed),
            view(
                "signal",
                GovAction::Signal { text: "hi".into() },
                ProposalStatus::Open,
            ),
            view(
                "home-1",
                GovAction::RegisterModule {
                    name: "n".into(),
                    module_id: "home".into(),
                    kind: modules::Kind::View,
                    activation_lead: 10,
                    code_hash: vec![0xab; 32],
                    lanes: Vec::new(),
                },
                ProposalStatus::Open,
            ),
        ];
        let proposed = open_code_proposals(&views);
        assert_eq!(
            proposed
                .iter()
                .map(|p| p.proposal_id.as_str())
                .collect::<Vec<_>>(),
            ["chat-1", "home-1"]
        );
        let out = render_proposed(&proposed);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "");
        assert_eq!(
            lines[1],
            "open code proposals (tasteable from the app while open):"
        );
        assert_eq!(lines[2], "chat  cdcdcdcdcdcd  proposal chat-1");
        assert_eq!(lines[3], "home  abababababab  proposal home-1");
        assert_eq!(render_proposed(&[]), "");
    }

    fn receipt(peer: &[u8], status: &str, ok: bool) -> PeerReceipt {
        PeerReceipt {
            peer: hex_bytes(peer),
            status: status.into(),
            ok,
        }
    }

    fn reply(receipts: Vec<PeerReceipt>) -> StageReply {
        StageReply {
            digest: "00".repeat(32),
            len: 3,
            receipts,
        }
    }

    #[test]
    fn a_validator_whose_node_refused_the_bytes_is_named_with_its_status() {
        let me = vec![0x01u8; 32];
        let stored = vec![0x02u8; 32];
        let refused = vec![0x03u8; 32];
        let reply = reply(vec![
            receipt(&stored, "stored", true),
            receipt(&refused, "module_artifact_too_large", false),
        ]);
        let members = vec![me.clone(), stored.clone(), refused.clone()];
        let rows = holdout_rows(&reply, &members, &hex_bytes(&me));
        assert_eq!(
            rows,
            vec![(hex_bytes(&refused), "module_artifact_too_large")],
            "holders are not listed"
        );
    }

    #[test]
    fn a_validator_without_a_receipt_is_named_unreachable() {
        let me = vec![0x01u8; 32];
        let answered = vec![0x02u8; 32];
        let silent = vec![0x03u8; 32];
        let reply = reply(vec![receipt(&answered, "stored", true)]);
        let members = vec![me.clone(), answered.clone(), silent.clone()];
        let rows = holdout_rows(&reply, &members, &hex_bytes(&me));
        assert_eq!(
            rows,
            vec![(hex_bytes(&silent), "no receipt (unreachable)")],
            "answered peers are not listed"
        );
        // me + every other member answered: nothing missing
        let present = vec![me.clone(), answered];
        assert!(holdout_rows(&reply, &present, &hex_bytes(&me)).is_empty());
        // a staging node outside the valset has no receipt of its own either
        let rows = holdout_rows(&reply, &present, "ff");
        assert_eq!(rows, vec![(hex_bytes(&me), "no receipt (unreachable)")]);
    }

    /// the receipt table names every overlay peer the fan-out reached, so a
    /// resident that could not take the bytes shows up in it; it is no part
    /// of the readiness quorum and is not a validator holdout.
    #[test]
    fn a_non_validator_holdout_is_not_a_validator_holdout() {
        let me = vec![0x01u8; 32];
        let other_validator = vec![0x02u8; 32];
        let resident = vec![0x03u8; 32];
        let reply = reply(vec![
            receipt(&other_validator, "already-have", true),
            receipt(&resident, "open failed: io: tcp connect refused", false),
        ]);
        let members = vec![me.clone(), other_validator];
        assert!(holdout_rows(&reply, &members, &hex_bytes(&me)).is_empty());
    }

    #[test]
    fn the_registry_precheck_refuses_each_static_rule_before_staging() {
        let ours = [0xabu8; 32];
        let theirs = [0xcdu8; 32];
        let swap = |hash: [u8; 32]| ScheduledSwap {
            name: "x".into(),
            activation_height: 9,
            code_hash: hash.to_vec(),
            readiness: Vec::new(),
            ready_at: None,
        };
        let entry = |active: &[u8], pending: Option<ScheduledSwap>| ModuleCode {
            module_id: "hello".into(),
            kind: modules::Kind::Module,
            active_code_hash: active.to_vec(),
            pending,
            history: Vec::new(),
        };
        let precheck = |verb, modules: &[ModuleCode]| {
            registry_precheck(verb, modules, &[], "hello", modules::Kind::Module, &ours)
        };

        // register: a free id proceeds; any existing entry refuses
        assert!(matches!(
            precheck(Verb::Register, &[]),
            Ok(Precheck::Proceed)
        ));
        let err = precheck(Verb::Register, &[entry(&theirs, None)]).unwrap_err();
        assert!(err.contains("already registered"), "{err}");
        // update: a missing entry refuses; a registered, quiet one proceeds
        let err = precheck(Verb::Update, &[]).unwrap_err();
        assert!(err.contains("unregistered module hello"), "{err}");
        assert!(matches!(
            precheck(Verb::Update, &[entry(&theirs, None)]),
            Ok(Precheck::Proceed)
        ));
        // a pending swap for OTHER bytes refuses either verb
        let busy = [entry(&theirs, Some(swap([0xefu8; 32])))];
        let err = precheck(Verb::Update, &busy).unwrap_err();
        assert!(err.contains("already has a pending swap"), "{err}");
        assert!(precheck(Verb::Register, &busy).is_err());
        // our own bytes: nothing to do, whichever verb — but a swap that has
        // not activated is "scheduled" and code the module already RUNS is
        // "active", and the operator's line says which.
        let pending_ours = [entry(&[], Some(swap(ours)))];
        assert!(matches!(
            precheck(Verb::Register, &pending_ours),
            // the height comes off the REGISTRY's pending swap — the only
            // number the confirmation line may name.
            Ok(Precheck::AlreadyHeld(Held::Pending {
                activation_height: 9
            }))
        ));
        assert!(matches!(
            precheck(Verb::Update, &[entry(&ours, None)]),
            Ok(Precheck::AlreadyHeld(Held::Active))
        ));
        assert_eq!(
            Held::Pending {
                activation_height: 9
            }
            .word(),
            "scheduled"
        );
        assert_eq!(Held::Active.word(), "active");
        // A familiar name is available when this network does not run it.
        let module = modules::Kind::Module;
        assert!(matches!(
            registry_precheck(Verb::Register, &[], &[], "identity", module, &ours),
            Ok(Precheck::Proceed)
        ));
        let live = vec!["identity".to_string()];
        let err =
            registry_precheck(Verb::Register, &[], &live, "identity", module, &ours).unwrap_err();
        assert!(err.contains("already registered"), "{err}");
        let err =
            registry_precheck(Verb::Update, &[], &live, "identity", module, &ours).unwrap_err();
        assert!(err.contains("unregistered module identity"), "{err}");
        // a swap keeps the entry's kind: view bytes under a module id (and
        // the reverse) are refused before anything is proposed.
        let err = registry_precheck(
            Verb::Update,
            &[entry(&theirs, None)],
            &[],
            "hello",
            modules::Kind::View,
            &ours,
        )
        .unwrap_err();
        assert!(err.contains("registered as a module"), "{err}");
    }

    #[test]
    fn the_matcher_checks_verb_id_code_and_lead() {
        let hash = [0xabu8; 32];
        let module = modules::Kind::Module;
        let no_lanes = Vec::new();
        let update = Verb::Update.action("hello", module, 100, hash, no_lanes.clone());
        let register = Verb::Register.action("hello", module, 100, hash, no_lanes.clone());
        let same_update = matches_module_action(Verb::Update, "hello", &hash, 100);
        assert!(same_update(&update));
        assert!(!same_update(&register), "register is not update");
        assert!(!same_update(&Verb::Update.action(
            "other",
            module,
            100,
            hash,
            no_lanes.clone()
        )));
        assert!(!same_update(&Verb::Update.action(
            "hello",
            module,
            100,
            [0xcdu8; 32],
            no_lanes.clone()
        )));
        // activation_lead is now a fixed part of the action's identity (it is
        // relative to the EXECUTE height, so it never goes stale): a
        // different lead is a DIFFERENT proposal, not one to join.
        assert!(!same_update(
            &Verb::Update.action("hello", module, 999, hash, no_lanes)
        ));
        let same_register = matches_module_action(Verb::Register, "hello", &hash, 100);
        assert!(same_register(&register));
        assert!(!same_register(&update));
    }

    #[test]
    fn the_node_digest_must_be_the_bytes_we_read() {
        use sha2::Digest as _;
        let bytes = b"component";
        let want = sha2::Sha256::digest(bytes);
        let good = StageReply {
            digest: hex_bytes(&want),
            len: 9,
            receipts: vec![],
        };
        assert_eq!(digest_matches(&good, bytes).unwrap()[..], want[..]);
        let lying = StageReply {
            digest: "ff".repeat(32),
            len: 9,
            receipts: vec![],
        };
        let err = digest_matches(&lying, bytes).unwrap_err();
        assert!(err.contains("digest"), "{err}");
    }

    #[test]
    fn status_rows_show_active_pending_and_readiness() {
        let active = vec![0xabu8; 32];
        let next = vec![0xcdu8; 32];
        let modules = vec![
            ModuleCode {
                module_id: "acl".into(),
                kind: modules::Kind::Module,
                active_code_hash: active.clone(),
                pending: None,
                history: Vec::new(),
            },
            ModuleCode {
                module_id: "hello".into(),
                kind: modules::Kind::Module,
                active_code_hash: active.clone(),
                pending: Some(ScheduledSwap {
                    name: "hello-2".into(),
                    activation_height: 120,
                    code_hash: next.clone(),
                    readiness: vec![vec![1], vec![2]],
                    ready_at: None,
                }),
                history: Vec::new(),
            },
            // `module register`: no active code at all until the swap lands.
            ModuleCode {
                module_id: "runs".into(),
                kind: modules::Kind::Module,
                active_code_hash: Vec::new(),
                pending: Some(ScheduledSwap {
                    name: "runs-1".into(),
                    activation_height: 120,
                    code_hash: next.clone(),
                    readiness: Vec::new(),
                    ready_at: None,
                }),
                history: Vec::new(),
            },
            // a view-only entry: the kind column is how an operator tells it apart.
            ModuleCode {
                module_id: "home".into(),
                kind: modules::Kind::View,
                active_code_hash: Vec::new(),
                pending: Some(ScheduledSwap {
                    name: "runs-1".into(),
                    activation_height: 120,
                    code_hash: next.clone(),
                    readiness: Vec::new(),
                    ready_at: None,
                }),
                history: Vec::new(),
            },
        ];
        let out = render_status(&modules, 100);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "id     kind    active        pending");
        assert_eq!(lines[1], "acl    module  abababababab  —");
        assert_eq!(
            lines[2],
            "hello  module  abababababab  cdcdcdcdcdcd  ready 2  activation 120"
        );
        // the active column stays 12 wide even when it is a single dash, so
        // the pending hashes line up with the row above.
        assert_eq!(
            lines[3],
            "runs   module  —             cdcdcdcdcdcd  ready 0  activation 120"
        );
        assert_eq!(
            lines[4],
            "home   view    —             cdcdcdcdcdcd  ready 0  activation 120"
        );
    }

    #[test]
    fn a_pending_swap_past_its_height_reads_dead() {
        let modules = vec![ModuleCode {
            module_id: "pages".into(),
            kind: modules::Kind::Module,
            active_code_hash: vec![0xabu8; 32],
            pending: Some(ScheduledSwap {
                name: "pages-2".into(),
                activation_height: 120,
                code_hash: vec![0xcdu8; 32],
                readiness: vec![vec![1]],
                ready_at: None,
            }),
            history: Vec::new(),
        }];
        // at the height itself the readiness never latched, so the swap can
        // no longer arm — an operator waiting on `ready 1` waits forever.
        let out = render_status(&modules, 120);
        assert!(
            out.contains("cdcdcdcdcdcd  DEAD  activation 120"),
            "{out}"
        );
        assert!(
            render_status(&modules, 119).contains("ready 1"),
            "a swap still short of its height is in flight"
        );
    }
}
