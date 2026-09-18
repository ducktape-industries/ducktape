//! Explicit resident installation. All durable state lives in Files, Agent,
//! Pages and Runs; merely starting a node or reading status provisions nothing.
mod package;
mod plan;

use package::package_files;

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use duckfs_client::{api::NodeApi, http::HttpNode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cred_cli::{VerbCtx, query_node};
use plan::Plan;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, clap::Args)]
pub(crate) struct ChiefArgs {
    #[command(subcommand)]
    command: ChiefCommand,
}
#[derive(Debug, clap::Subcommand)]
enum ChiefCommand {
    /// Explicitly provision a resident Chief; retry the same ID to reconcile.
    Add {
        #[arg(default_value = "chief")]
        chief_id: String,
        /// Ordinary Pi package directory, read at invocation time (never embedded).
        #[arg(long, value_name = "DIRECTORY")]
        package: PathBuf,
        /// Existing, distinct worker model using the built-in `pi` capability.
        /// Custom capability tags cannot be verified by this CLI.
        #[arg(long, value_name = "MODEL_ID")]
        worker: String,
        /// Explicitly bind an existing ordinary channel at its current cursor.
        #[arg(long, value_name = "CHANNEL_ID")]
        channel: Option<String>,
        /// New initialization identity, permitted only after a failed initializer.
        #[arg(long, value_name = "REQUEST_ID")]
        retry_initialization: Option<String>,
    },
    /// Read committed bindings, conversation state and initialization failures.
    Status {
        #[arg(default_value = "chief")]
        chief_id: String,
        /// Original installing account (address only; defaults to the current wallet).
        #[arg(long, value_name = "ACCOUNT")]
        installed_by: Option<u64>,
    },
    /// Stop admitting coordinating turns without replacing its history/bindings.
    Pause {
        #[arg(default_value = "chief")]
        chief_id: String,
        /// Original installing account; the current wallet must control the Chief.
        #[arg(long, value_name = "ACCOUNT")]
        installed_by: Option<u64>,
    },
    /// Resume the existing conversation; recover an interrupted native turn
    /// from its committed checkpoint, without reinstalling or resetting history.
    Resume {
        #[arg(default_value = "chief")]
        chief_id: String,
        /// Original installing account; the current wallet must control the Chief.
        #[arg(long, value_name = "ACCOUNT")]
        installed_by: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Manifest {
    plan: Plan,
    package_digest: String,
    package: run_envelope::ConversationPackage,
    program: agent::Program,
    initializer_steps: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Progress {
    account: u64,
    request_id: String,
}

pub(crate) fn run(args: ChiefArgs, ctx: &VerbCtx, stdin: &mut impl BufRead) -> Result<()> {
    let base = ctx.http_base()?;
    let user = ctx.signer(stdin)?;
    let controller = crate::account_cli::own_account(&base, user.public_key().as_ref())?.number;
    match args.command {
        ChiefCommand::Add {
            chief_id,
            package,
            worker,
            channel,
            retry_initialization,
        } => add(
            &base,
            &user,
            Plan::new(controller, &chief_id, channel.as_deref(), &worker)?,
            &package,
            retry_initialization.as_deref(),
        ),
        ChiefCommand::Status {
            chief_id,
            installed_by,
        } => {
            let manifest = require_manifest(&base, installed_by.unwrap_or(controller), &chief_id)?;
            print_status(&base, &manifest)
        }
        ChiefCommand::Pause {
            chief_id,
            installed_by,
        } => control(
            &base,
            &user,
            installed_by.unwrap_or(controller),
            &chief_id,
            plan::Control::Pause,
        ),
        ChiefCommand::Resume {
            chief_id,
            installed_by,
        } => control(
            &base,
            &user,
            installed_by.unwrap_or(controller),
            &chief_id,
            plan::Control::Resume,
        ),
    }
}

fn manifest_path(controller: u64, chief_id: &str) -> Result<String> {
    let plan = Plan::new(controller, chief_id, None, "validation-only")?;
    Ok(format!("{}/manifest.json", plan.root()))
}
fn read_manifest(
    node: &impl NodeApi,
    path: &str,
    snapshot: Option<&str>,
) -> Result<Option<Manifest>> {
    let Some(stat) = node.stat(path, snapshot)? else {
        return Ok(None);
    };
    if stat.size > 512 * 1024 {
        return Err("Chief manifest exceeds its size bound".into());
    }
    let (bytes, eof) = node.read(path, snapshot, 0, 512 * 1024)?;
    if !eof {
        return Err("truncated Chief manifest".into());
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}
fn require_manifest(base: &str, installed_by: u64, chief_id: &str) -> Result<Manifest> {
    let node = HttpNode::new(base);
    let head = node.refs()?.head;
    let manifest = read_manifest(
        &node,
        &manifest_path(installed_by, chief_id)?,
        head.as_deref(),
    )?
    .ok_or_else(|| format!("Chief {chief_id:?} is not installed by account {installed_by}"))?;
    validate_manifest(&manifest, installed_by, chief_id)?;
    // Files HEAD is only a locator. The immutable Agent receipt binds these
    // exact program bytes to the originally requested installation, not its
    // mutable manifest's claim about which account/request to query.
    provision_receipt(base, &manifest)?;
    Ok(manifest)
}

fn validate_manifest(manifest: &Manifest, installed_by: u64, chief_id: &str) -> Result<()> {
    let channel = manifest
        .plan
        .existing_channel
        .then_some(manifest.plan.channel_id.as_str());
    let expected = Plan::new(
        installed_by,
        chief_id,
        channel,
        &manifest.plan.worker_agent_id,
    )?;
    if manifest.plan != expected {
        return Err("Chief manifest does not match the requested installation".into());
    }
    let valid_digest = manifest.package_digest.len() == 64
        && manifest
            .package_digest
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'));
    let expected_prefix = format!("{}/packages/{}", expected.root(), manifest.package_digest);
    let package_matches = valid_digest
        && manifest.package.name == "chief"
        && manifest.package.source_prefix == expected_prefix;
    if !package_matches {
        return Err("Chief package does not match the requested installation".into());
    }
    let (program, steps) = plan::program(&expected, &manifest.package);
    let canonical_program = manifest.program == program && manifest.initializer_steps == steps;
    if !canonical_program {
        return Err("Chief manifest program differs from its installation bindings".into());
    }
    Ok(())
}
fn submit(base: &str, user: &PrivateKey, target: &str, bytes: Vec<u8>) -> Result<u64> {
    crate::node_http::submit_frame(base, &crate::userkey_cli::user_frame(user, target, bytes))
}
fn agent_query(base: &str, query: agent::AgentQuery) -> Result<agent::AgentReply> {
    Ok(serde_json::from_value(query_node(
        base,
        "agent",
        serde_json::to_value(query)?,
    )?)?)
}
fn provision_receipt(base: &str, manifest: &Manifest) -> Result<Option<agent::ProvisionReceipt>> {
    let plan = &manifest.plan;
    let reply = agent_query(
        base,
        agent::AgentQuery::Provision {
            controller: plan.controller,
            request_id: plan.namespace.clone(),
        },
    )?;
    let agent::AgentReply::Provision(receipt) = reply else {
        return Err("unexpected provision reply".into());
    };
    if let Some(receipt) = &receipt {
        let digest: [u8; 32] = Sha256::digest(borsh::to_vec(&("Chief", &manifest.program))?).into();
        if receipt.request_digest != digest {
            return Err("Chief manifest differs from its immutable provisioning receipt".into());
        }
    }
    Ok(receipt)
}
fn conversation(base: &str, id: &str) -> Result<Option<runs::ConversationView>> {
    let value = query_node(
        base,
        "runs",
        serde_json::json!({"conversation":{"conversation_id":id}}),
    )?;
    let reply: runs::RunsReply = serde_json::from_value(value)?;
    let runs::RunsReply::Conversation(value) = reply else {
        return Err("unexpected conversation reply".into());
    };
    Ok(value)
}
fn model(base: &str, id: &str) -> Result<Option<runs::ModelRecord>> {
    let value = query_node(
        base,
        "runs",
        serde_json::to_value(runs::RunsQuery::Model {
            query: runs::ModelQuery::Agent {
                agent_id: id.into(),
            },
        })?,
    )?;
    let reply: runs::RunsReply = serde_json::from_value(value)?;
    let runs::RunsReply::Model(runs::ModelReply::Agent(value)) = reply else {
        return Err("unexpected model reply".into());
    };
    Ok(value)
}

fn add(
    base: &str,
    user: &PrivateKey,
    plan: Plan,
    directory: &Path,
    retry: Option<&str>,
) -> Result<()> {
    let node = HttpNode::new(base);
    let head = node.refs()?.head;
    let path = manifest_path(plan.controller, &plan.chief_id)?;
    let source = package_files(directory, &plan)?;
    let digest = plan::hex(&Sha256::digest(sdk::wire::encode(&source)));
    let manifest = match read_manifest(&node, &path, head.as_deref())? {
        Some(manifest) => {
            let exact = manifest.plan == plan && manifest.package_digest == digest;
            if !exact {
                return Err("Chief ID is already bound to another package/configuration; retry its original inputs".into());
            }
            validate_manifest(&manifest, plan.controller, &plan.chief_id)?;
            provision_receipt(base, &manifest)?;
            manifest
        }
        None => {
            preflight(base, &plan)?;
            let package =
                stage_package(base, user, &node, &plan, &digest, head.as_deref(), &source)?;
            let (program, initializer_steps) = plan::program(&plan, &package);
            let manifest = Manifest {
                plan,
                package_digest: digest,
                package,
                program,
                initializer_steps,
            };
            // CAS against the head read BEFORE examining the manifest. A racing
            // installer cannot overwrite another install's committed binding.
            commit_files(
                base,
                user,
                head.as_deref(),
                "Chief installation manifest",
                BTreeMap::from([(path, serde_json::to_vec(&manifest)?)]),
            )?;
            manifest
        }
    };
    // A retry also verifies/restores the manifest's exact pin. Never provision
    // from an artifact whose retention failed or points at another snapshot.
    ensure_package_pin(base, user, &node, &manifest)?;
    submit(
        base,
        user,
        "agent",
        agent::encode_msg(&agent::AgentMsg::Provision {
            request_id: manifest.plan.namespace.clone(),
            name: "Chief".into(),
            program: manifest.program.clone(),
        }),
    )?;
    let receipt =
        provision_receipt(base, &manifest)?.ok_or("provision committed without its receipt")?;
    let agent::AgentReply::Binding(Some(binding)) = agent_query(
        base,
        agent::AgentQuery::Binding {
            account: receipt.account,
        },
    )?
    else {
        return Err("Chief account is no longer bound; refusing to reinstall it".into());
    };
    if binding.program != manifest.program {
        return Err(
            "Chief program was replaced; refusing to initialize a different binding".into(),
        );
    }
    let head = node.refs()?.head;
    let previous = read_progress(&node, &manifest.plan, head.as_deref())?;
    let mut progress = previous.clone().unwrap_or(Progress {
        account: receipt.account,
        request_id: "install".into(),
    });
    if progress.account != receipt.account {
        return Err("installation progress account conflicts with the provisioning receipt".into());
    }
    let new_retry = retry.filter(|id| *id != progress.request_id);
    if let Some(id) = new_retry {
        validate_retry(base, &progress, id)?;
        progress.request_id = id.into();
    }
    let needs_progress = previous
        .as_ref()
        .is_none_or(|previous| previous.request_id != progress.request_id);
    if needs_progress {
        commit_files(
            base,
            user,
            head.as_deref(),
            "Chief initialization binding",
            BTreeMap::from([(
                format!("{}/initialization.json", manifest.plan.root()),
                serde_json::to_vec(&progress)?,
            )]),
        )?;
    }
    // Agent deduplicates this identity. A successful submit admits initialization,
    // not all its queued resource calls; status below never calls that 'active'.
    submit(
        base,
        user,
        "agent",
        agent::encode_msg(&agent::AgentMsg::Initialize {
            account: receipt.account,
            request_id: progress.request_id,
        }),
    )?;
    print_status(base, &manifest)
}

/// The create-only stable pin is the first-wins package snapshot binding.
/// It precedes the manifest so a crash between those writes remains recoverable
/// after arbitrary unrelated Files commits. Racers can reuse its snapshot only
/// when their exact source subtree is present; the manifest CAS fixes which
/// package/configuration wins before any program account is provisioned.
#[allow(clippy::too_many_arguments)]
fn stage_package(
    base: &str,
    user: &PrivateKey,
    node: &impl NodeApi,
    plan: &Plan,
    digest: &str,
    head: Option<&str>,
    source: &BTreeMap<String, Vec<u8>>,
) -> Result<run_envelope::ConversationPackage> {
    let prefix = format!("{}/packages/{digest}", plan.root());
    let name = format!("{}-package", plan.namespace);
    let snapshot = match node.refs()?.pins.get(&name) {
        Some(snapshot) => snapshot.clone(),
        None => {
            let files = source
                .iter()
                .map(|(path, bytes)| (format!("{prefix}/{path}"), bytes.clone()))
                .collect();
            let candidate = commit_files(base, user, head, "Chief package", files)?;
            let output = submit_files_output(
                base,
                user,
                duckfs_core::FilesMsg::ProjectSnapshot {
                    snapshot: candidate,
                    path: prefix.clone(),
                },
            )?;
            let duckfs_core::WriteOutcome::ProjectSnapshot {
                snapshot: projected,
            } = output.outcome
            else {
                return Err("Files projection returned an unexpected receipt".into());
            };
            package::verify_snapshot(node, &prefix, &projected, source)?;
            let pinned = submit(
                base,
                user,
                "files",
                duckfs_core::encode_msg(&duckfs_core::FilesMsg::Pin {
                    snapshot: projected.clone(),
                    name: name.clone(),
                }),
            );
            match pinned {
                Ok(_) => projected,
                Err(error) => node.refs()?.pins.get(&name).cloned().ok_or(error)?,
            }
        }
    };
    package::verify_snapshot(node, &prefix, &snapshot, source)?;
    Ok(run_envelope::ConversationPackage {
        name: "chief".into(),
        source_prefix: prefix,
        source_snapshot: snapshot,
    })
}

fn ensure_package_pin(
    base: &str,
    user: &PrivateKey,
    node: &impl NodeApi,
    manifest: &Manifest,
) -> Result<()> {
    let name = format!("{}-package", manifest.plan.namespace);
    let refs = node.refs()?;
    if let Some(snapshot) = refs.pins.get(&name) {
        if snapshot != &manifest.package.source_snapshot {
            return Err("Chief package pin conflicts with its manifest".into());
        }
        return Ok(());
    }
    let result = submit(
        base,
        user,
        "files",
        duckfs_core::encode_msg(&duckfs_core::FilesMsg::Pin {
            snapshot: manifest.package.source_snapshot.clone(),
            name: name.clone(),
        }),
    );
    match result {
        Ok(_) => Ok(()),
        Err(error) => {
            let committed = node.refs()?.pins.get(&name) == Some(&manifest.package.source_snapshot);
            if committed {
                return Ok(());
            }
            Err(error)
        }
    }
}

fn preflight(base: &str, plan: &Plan) -> Result<()> {
    let Some(worker) = model(base, &plan.worker_agent_id)? else {
        return Err("--worker must name an existing model".into());
    };
    if worker.agent_id == plan.namespace {
        return Err("Chief cannot be its own worker".into());
    }
    if worker.capability != "pi" {
        return Err("--worker must use the native Pi conversation capability 'pi'".into());
    }
    if plan.existing_channel {
        let reply: chat::ChatReply = serde_json::from_value(query_node(
            base,
            "chat",
            serde_json::json!({"channel":{"channel_id":plan.channel_id}}),
        )?)?;
        let chat::ChatReply::Channel(Some(channel)) = reply else {
            return Err("--channel must name an existing channel".into());
        };
        let available = !channel.archived && channel.post_policy == chat::PostPolicy::Open;
        if !available {
            return Err("Chief requires an unarchived, open existing channel".into());
        }
    }
    Ok(())
}

fn read_progress(
    node: &impl NodeApi,
    plan: &Plan,
    snapshot: Option<&str>,
) -> Result<Option<Progress>> {
    let path = format!("{}/initialization.json", plan.root());
    let Some(stat) = node.stat(&path, snapshot)? else {
        return Ok(None);
    };
    if stat.size > 4096 {
        return Err("installation progress exceeds its bound".into());
    }
    let (bytes, eof) = node.read(&path, snapshot, 0, 4096)?;
    if !eof {
        return Err("truncated installation progress".into());
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}
fn initialization(base: &str, progress: &Progress) -> Result<Option<agent::InvocationView>> {
    // Initialization has one source object and one change. Point directly at
    // it rather than scanning the resident account's lifetime of model turns.
    let reply: attribution::AttributionReply = serde_json::from_value(query_node(
        base,
        "attribution",
        serde_json::json!({"changes_of":{
            "source":{"module":"agent","kind":agent::INITIALIZATION_KIND,"object":format!("{}/{}",progress.account,progress.request_id)},"after":0,"limit":2
        }}),
    )?)?;
    let attribution::AttributionReply::Changes(entries) = reply else {
        return Err("unexpected initialization attribution reply".into());
    };
    let Some(entry) = entries.first() else {
        return Ok(None);
    };
    let agent::AgentReply::Invocation(invocation) = agent_query(
        base,
        agent::AgentQuery::Invocation {
            account: progress.account,
            seq: entry.change.seq,
        },
    )?
    else {
        return Err("unexpected initialization invocation reply".into());
    };
    Ok(invocation)
}
fn validate_retry(base: &str, progress: &Progress, id: &str) -> Result<()> {
    let valid_id = id != "install"
        && !id.trim().is_empty()
        && id.len() <= agent::MAX_PROVISION_REQUEST_ID_BYTES
        && !id.chars().any(char::is_control);
    if !valid_id {
        return Err(
            "retry initialization needs a new bounded request identity, not 'install'".into(),
        );
    }
    let Some(last) = initialization(base, progress)? else {
        return Err("initialization has not failed; retry the original add instead".into());
    };
    let failed = matches!(last.status, agent::Status::Failed { .. });
    if !failed {
        return Err(
            "initialization is pending or finished; a new initialization is not permitted".into(),
        );
    }
    // The program performs stage reconciliation under the Chief account. Do not
    // substitute a controller-authored Page write or overwrite live resources.
    Ok(())
}

fn control(
    base: &str,
    user: &PrivateKey,
    installed_by: u64,
    chief_id: &str,
    action: plan::Control,
) -> Result<()> {
    let manifest = require_manifest(base, installed_by, chief_id)?;
    let receipt = provision_receipt(base, &manifest)?.ok_or("Chief is not yet provisioned")?;
    let state = conversation(base, &manifest.plan.namespace)?
        .ok_or("Chief conversation is still installing")?;
    if state.account != receipt.account {
        return Err("conversation account differs from the installation".into());
    }
    // New control decisions have distinct operation identities. Installer retries
    // use their separate fixed activation identity, never this control lane.
    let operation_id = format!("control-{}", plan::hex(&rand::random::<[u8; 16]>()));
    let messages = plan::control_messages(&state, action, &operation_id)?;
    for msg in messages {
        submit(base, user, "runs", runs::encode_msg(&msg))?;
    }
    print_status(base, &manifest)
}

fn print_status(base: &str, manifest: &Manifest) -> Result<()> {
    let receipt = provision_receipt(base, manifest)?;
    let Some(receipt) = receipt else {
        println!(
            "{}",
            serde_json::json!({"status":"staged","installation":manifest.plan,"package":manifest.package})
        );
        return Ok(());
    };
    let progress = read_progress(&HttpNode::new(base), &manifest.plan, None)?.unwrap_or(Progress {
        account: receipt.account,
        request_id: "install".into(),
    });
    if progress.account != receipt.account {
        return Err("installation progress account differs from provisioning receipt".into());
    }
    let invocation = initialization(base, &progress)?;
    let conversation = conversation(base, &manifest.plan.namespace)?;
    let installation_status = match invocation.as_ref().map(|entry| &entry.status) {
        Some(agent::Status::Failed { .. } | agent::Status::Aborted { .. }) => "failed",
        _ => "installing",
    };
    let inactive_status = match invocation.as_ref().map(|entry| &entry.status) {
        Some(agent::Status::Finished { .. }) => "paused",
        _ => installation_status,
    };
    let (status, problems) = match &conversation {
        Some(state) => {
            let problems = binding_problems(base, manifest, receipt.account, state)?;
            let status = match (problems.is_empty(), &state.status) {
                (false, _) => "degraded",
                (true, runs::ConversationStatus::Active) => "active",
                (true, runs::ConversationStatus::Inactive) => inactive_status,
                (true, runs::ConversationStatus::Paused { .. }) => "paused",
            };
            (status, problems)
        }
        None => (installation_status, Vec::new()),
    };
    println!(
        "{}",
        serde_json::json!({"status":status,"account":receipt.account,
        "installation":manifest.plan,"package":manifest.package,"conversation":conversation,"initialization":invocation.as_ref().map(|entry| serde_json::json!({"seq":entry.seq,"status":entry.status})),"initialization_request":progress.request_id,"problems":problems})
    );
    Ok(())
}

fn binding_problems(
    base: &str,
    manifest: &Manifest,
    account: u64,
    conversation: &runs::ConversationView,
) -> Result<Vec<&'static str>> {
    let mut problems = Vec::new();
    let source = runs::ConversationSource::Channel {
        channel_id: manifest.plan.channel_id.clone(),
    };
    let bound = conversation.account == account
        && conversation.agent_id == manifest.plan.namespace
        && conversation.source == source
        && conversation.packages == vec![manifest.package.clone()];
    if !bound {
        problems.push("conversation_binding_changed");
    }
    let binding = agent_query(base, agent::AgentQuery::Binding { account })?;
    let program_matches = matches!(binding, agent::AgentReply::Binding(Some(binding)) if binding.program == manifest.program);
    if !program_matches {
        problems.push("program_binding_changed");
    }
    let model = model(base, &manifest.plan.namespace)?;
    let model_ready = model.is_some_and(|model| {
        model.account == account
            && model.capability == "pi"
            && model.status == runs::ModelStatus::Active
    });
    if !model_ready {
        problems.push("model_unavailable");
    }
    let reply: chat::ChatReply = serde_json::from_value(query_node(
        base,
        "chat",
        serde_json::json!({"channel":{"channel_id":manifest.plan.channel_id}}),
    )?)?;
    let channel_ready = matches!(reply, chat::ChatReply::Channel(Some(channel)) if !channel.archived && channel.post_policy == chat::PostPolicy::Open);
    if !channel_ready {
        problems.push("channel_unavailable");
    }
    for (page_id, _) in manifest.plan.pages() {
        let reply: pages::PageReply = serde_json::from_value(query_node(
            base,
            "pages",
            serde_json::json!({"record_collection":{"page_id":page_id}}),
        )?)?;
        let collection_ready = matches!(reply, pages::PageReply::RecordCollection(Some(collection)) if collection.writer == pages::Party::Account(account));
        if !collection_ready {
            problems.push("page_collection_unavailable");
        }
    }
    let node = HttpNode::new(base);
    let pinned = node
        .refs()?
        .pins
        .get(&format!("{}-package", manifest.plan.namespace))
        == Some(&manifest.package.source_snapshot);
    if !pinned {
        problems.push("package_pin_missing");
    }
    let package = node.stat(
        &format!("{}/package.json", manifest.package.source_prefix),
        Some(&manifest.package.source_snapshot),
    )?;
    if package.is_none() {
        problems.push("pinned_package_unavailable");
    }
    Ok(problems)
}

fn submit_files_output(
    base: &str,
    user: &PrivateKey,
    msg: duckfs_core::FilesMsg,
) -> Result<duckfs_core::FilesWriteOutput> {
    let frame = crate::userkey_cli::user_frame(user, "files", duckfs_core::encode_msg(&msg));
    let assigned = crate::node_http::submit_frame_assigned(base, &frame)?;
    Ok(duckfs_core::decode_write_output(&assigned)?)
}

fn commit_files(
    base: &str,
    user: &PrivateKey,
    snapshot: Option<&str>,
    message: &str,
    files: BTreeMap<String, Vec<u8>>,
) -> Result<String> {
    let mut changes = Vec::new();
    for (path, bytes) in files {
        let mut chunks = Vec::new();
        for chunk in bytes.chunks(duckfs_core::CHUNK_SIZE as usize) {
            submit(base, user, "files", duckfs_core::encode_putblob(chunk))?;
            chunks.push(plan::hex(&duckfs_core::objects::object_id(
                duckfs_core::Kind::Chunk,
                chunk,
            )));
        }
        changes.push(duckfs_core::Change::Put {
            path,
            exec: false,
            meta: BTreeMap::new(),
            content: duckfs_core::Content::Chunks {
                size: bytes.len() as u64,
                chunks,
            },
        });
    }
    let output = submit_files_output(
        base,
        user,
        duckfs_core::FilesMsg::Commit {
            base_snapshot: snapshot.map(str::to_string),
            message: message.into(),
            changes,
        },
    )?;
    let duckfs_core::WriteOutcome::Commit { snapshot } = output.outcome else {
        return Err("Files commit returned an unexpected receipt".into());
    };
    Ok(snapshot)
}
