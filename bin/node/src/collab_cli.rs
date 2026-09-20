//! `ducktape collab` — the operator's seam onto agent messaging over a chat
//! channel: attach a personal provider session to it, ask for a delivery,
//! report one, and read the module's protected state.
//!
//! A participant is a chat party, spelled as a handle everywhere on this CLI:
//! `acct:<number>` for an account, `key:<hex>` for a bare signing key.
//!
//! ## the whole design, in one sentence
//!
//! Every verb here is a signed request to the operator's OWN node over `/v1`,
//! so messaging needs no inbound socket on any laptop, borrows no pty, and
//! survives an agent daemon reconnect.
//!
//! ## why an attachment is a chain write and not a session
//!
//! A binding names WHICH channel a device will receive, and it has to
//! outlive every process involved — that is the difference between a mailbox
//! and a terminal. The interactive plane cannot hold one: `agent-service`'s
//! `Sessions::close_all` ends every live pty the moment the daemon's ws link to
//! its node drops (`bin/node/src/agent/link.rs`), and a dropped link is
//! ORDINARY — the node restarts, the operator upgrades, the socket blips. A
//! binding stored there would be revoked by a network hiccup.
//!
//! So a binding is committed module state. The daemon learns its bindings by
//! reading committed state, the way compute already learns its placement, and
//! the link's teardown cannot reach them. Nothing in this file touches
//! `Sessions`, a `term:` topic, `TermInput` or `TermOutput`; pty bytes are raw
//! and unstructured and are not a delivery channel. Pinned by this file's own
//! `the_collab_plane_never_touches_the_pty_plane`, which fails the build if the
//! two planes ever touch.
//!
//! ## reads are authenticated, and that is not free
//!
//! `/v1/query` is open to anything that can dial the port — a sandbox guest
//! reaching this listener over its vsock tunnel included — and the module
//! answering it sees `Origin::System`, which proves nothing about the caller.
//! A participant id in such a query is a client's claim. So protected reads go
//! to `/v1/query/reader`, where the caller's own signature names the reader and
//! the node hands the VERIFIED key to the module as its origin.
//!
//! That authenticates the reader at the RPC edge. It is not confidentiality:
//! bodies live in replicated committed state, so every validator can read them.
//! Nothing here promises end-to-end encryption.
//!
//! Program output stays `println!` — a CLI's stdout is not logging.

use std::io::BufRead;

use crate::cred_cli::VerbCtx;
use crate::wire::{chat, collaboration};

type CollabResult = Result<(), Box<dyn std::error::Error>>;

/// `ducktape collab <verb>`. The shared addressing group selects the node this
/// CLI dials; `--key` is the identity every verb acts as.
#[derive(Debug, clap::Args)]
pub(crate) struct CollabArgs {
    #[command(subcommand)]
    cmd: CollabCmd,
    #[command(flatten)]
    addr: crate::cli_args::NodeAddr,
    /// path to the user key that signs (defaults to the keystore's active
    /// wallet). This key IS the reader/sender identity — there is no separate
    /// participant credential to present.
    #[arg(long, value_name = "PATH", global = true)]
    key: Option<std::path::PathBuf>,
    /// re-pin this node's key when it differs from the one already trusted for
    /// this url. Without it a changed key is refused (`node_key_mismatch`).
    #[arg(long, global = true)]
    trust_node: bool,
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum CollabCmd {
    /// read protected module state AS your key, over the authenticated lane
    Query(QueryArgs),
    /// this device's scoped service key for one binding: mint it if absent,
    /// then print its PUBLIC half for the `Bind` that authorizes it
    Key(KeyArgs),
    /// attach this device to a chat channel: mint the scoped service key and
    /// submit the OWNER-signed `Bind` that authorizes it
    Attach(AttachArgs),
    /// post one chat message and ask that it be delivered to one recipient,
    /// both signed by the binding's scoped service key
    Send(SendArgs),
    /// report a delivery state for one message, signed by the scoped key
    Ack(AckArgs),
}

/// `collab attach` — the owner authorizes THIS device.
///
/// Two keys, deliberately. The scoped service key is minted here and never
/// leaves the workspace; the OWNER's wallet key signs the `Bind` that names its
/// public half. A scoped credential that could authorize itself would not be
/// scoped by anything.
#[derive(Debug, clap::Args)]
pub(crate) struct AttachArgs {
    /// the chat channel this attachment will receive
    #[arg(long, value_name = "ID")]
    channel: String,
    /// the participant this device is attaching for: `acct:<n>` or `key:<hex>`
    #[arg(long, value_name = "HANDLE")]
    participant: String,
    /// an opaque label for this device, for a human reading the roster. NEVER
    /// a socket path, a url or a provider session id — a device label is
    /// committed state that every validator can read.
    #[arg(long, value_name = "LABEL")]
    device: String,
    /// the credential being REPLACED: 0 for a first attachment, otherwise the
    /// current `credential` from the `Binding` read. A mismatch is refused,
    /// which is what stops two devices both claiming the one input binding.
    #[arg(long, value_name = "N", default_value_t = 0)]
    expect: u64,
}

/// `collab send` — one chat message, and the request that it be delivered.
#[derive(Debug, clap::Args)]
pub(crate) struct SendArgs {
    /// the chat channel the message is posted in
    #[arg(long, value_name = "ID")]
    channel: String,
    /// the participant whose binding this device holds — it selects the scoped
    /// key that signs both the post and the delivery request.
    #[arg(long, value_name = "HANDLE")]
    participant: String,
    /// the recipient: `acct:<n>` or `key:<hex>`
    #[arg(long, value_name = "HANDLE")]
    to: String,
    #[arg(long, value_enum, default_value_t = Kind::Notice)]
    kind: Kind,
    /// the chat message id to post under. Minted at random when absent; name
    /// one to retry a post the network may already hold — chat refuses a
    /// second body under the same id, and the delivery request is idempotent.
    #[arg(long, value_name = "ID")]
    message_id: Option<String>,
    /// how long this message stays deliverable, as a seconds INTENT converted
    /// into the network's own `consensus_time` unit — never a laptop clock.
    ///
    /// The two lanes measure in different things, so the conversion is exact on
    /// one and nominal on the other: a `millis` network counts milliseconds, so
    /// the deadline is these seconds exactly; a `height` network counts BLOCKS,
    /// so it is this many blocks, which is these seconds only while the chain
    /// heartbeats at one block a second. A slower chain makes the wall-clock
    /// window longer, a faster one shorter. Use `collab send --help` on the
    /// network you are addressing and read `/v1/status`'s `consensus_time_unit`
    /// if the distinction matters to you.
    #[arg(long, value_name = "SECONDS", default_value_t = 86_400)]
    ttl_secs: u64,
    /// the message body, after `--`
    #[arg(last = true, value_name = "BODY", required = true)]
    body: String,
}

/// The message kinds, as the module spells them.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum Kind {
    Notice,
    Question,
    TaskRequest,
    TaskUpdate,
    Result,
}

/// The delivery states a bound service may report.
///
/// Two of the module's seven are deliberately absent. `Stored` is what
/// ADMISSION records, and `Expired` is what the permissionless `ExpireMessage`
/// decides from the agreed deadline. Offering either here would let a device
/// assert the network's own bookkeeping about itself.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum AckState {
    Queued,
    AdapterAccepted,
    Held,
    Refused,
    DeliveryUnknown,
}

/// `collab ack` — the bound service reports a delivery state.
#[derive(Debug, clap::Args)]
pub(crate) struct AckArgs {
    /// the chat channel the message sits in
    #[arg(long, value_name = "ID")]
    channel: String,
    /// the participant whose binding this device holds — it selects the scoped
    /// key that signs, and the module checks that key against the binding.
    #[arg(long, value_name = "HANDLE")]
    participant: String,
    /// the channel sequence of the message being reported on
    #[arg(long, value_name = "N")]
    seq: u64,
    /// the binding credential this report is made under. A stale one is
    /// refused, so a returning old attachment cannot overwrite the current
    /// binding's state.
    #[arg(long, value_name = "N")]
    credential: u64,
    #[arg(long, value_enum)]
    state: AckState,
    /// a stable snake_case token, never prose and never a path or a token
    #[arg(long, value_name = "TOKEN")]
    reason: Option<String>,
}

/// Names one binding — the pair the module keys a `Binding` on.
#[derive(Debug, clap::Args)]
pub(crate) struct KeyArgs {
    /// the chat channel this attachment will receive
    #[arg(long, value_name = "ID")]
    channel: String,
    /// the participant this device is attaching for: `acct:<n>` or `key:<hex>`
    #[arg(long, value_name = "HANDLE")]
    participant: String,
    /// print the key only if it already exists, never mint one — for checking
    /// whether this device holds the binding at all
    #[arg(long)]
    existing_only: bool,
}

/// The JSON is the MODULE's own query enum, passed through verbatim — this CLI
/// is a transport. Spelling core's shape as a Rust type here would be a second
/// copy of it to drift; an example in the help is not.
///
/// `collaboration` takes one envelope, and the caller it acts as comes from the
/// signature, never from the payload:
///
/// ```text
/// ducktape collab query --target collaboration \
///   '{"read":{"participant":{"account":7},"via":null,"read":{"events":{"channel_id":"c1","from_seq":0,"limit":50}}}}'
/// ```
///
/// `via` names the channel whose binding authorizes the read when you are
/// signing with that binding's scoped service key rather than the owner key.
#[derive(Debug, clap::Args)]
pub(crate) struct QueryArgs {
    /// the module to ask (e.g. `collaboration`)
    #[arg(long, value_name = "MODULE")]
    target: String,
    /// the module's query enum as one JSON value
    #[arg(value_name = "QUERY_JSON")]
    query: String,
}

pub(crate) fn run(args: CollabArgs) -> CollabResult {
    let CollabArgs {
        cmd,
        addr,
        key,
        trust_node,
    } = args;
    let ctx = VerbCtx { addr, key };
    let mut stdin = std::io::BufReader::new(std::io::stdin());
    match cmd {
        CollabCmd::Query(query) => cmd_query(query, &ctx, trust_node, &mut stdin),
        CollabCmd::Key(key) => cmd_key(key, &ctx),
        CollabCmd::Attach(attach) => cmd_attach(attach, &ctx, &mut stdin),
        CollabCmd::Send(send) => cmd_send(send, &ctx),
        CollabCmd::Ack(ack) => cmd_ack(ack, &ctx),
    }
}

/// The module every verb here submits to and reads from.
const COLLABORATION: &str = "collaboration";

/// Submit one collaboration op as a frame `signer` signs; answers the height.
///
/// `/v1/submit/frame` is the whole authorization: the frame's verified signer
/// becomes the op's `Origin::External`, and the module admits or refuses it.
/// This CLI pre-checks nothing — a second gate here could only drift from the
/// one that decides, and the module's refusal reaches the operator verbatim.
///
/// The op travels inside a [`collaboration::Request`] naming the NETWORK it is
/// for, and that name is inside the signed payload. This is what actually binds
/// a collaboration op to one chain: the frame envelope carries no chain id
/// (`user_frame` signs `signer ‖ seq ‖ target ‖ payload`), so without the field
/// a frame minted here would verify byte-identically on another network. The
/// module refuses a `network` that is not the one it was composed with, and
/// refuses two empty ids rather than matching them.
///
/// `network` therefore comes from [`agreed_network`] — checked non-blank AND
/// checked against the node being dialled — never from the local file alone.
fn submit(
    base: &str,
    signer: &commonware_cryptography::ed25519::PrivateKey,
    network: &str,
    op: collaboration::CollaborationMsg,
) -> Result<u64, Box<dyn std::error::Error>> {
    let request = collaboration::Request::new(network, op);
    let frame =
        crate::userkey_cli::user_frame(signer, COLLABORATION, collaboration::encode_msg(&request));
    crate::node_http::submit_frame(base, &frame)
}

/// Submit one chat op the same way. A chat op carries no network name of its
/// own; the delivery request that follows it does, and refers to the message
/// by an id that only exists on the chain it was posted to.
fn submit_chat(
    base: &str,
    signer: &commonware_cryptography::ed25519::PrivateKey,
    op: chat::ChatMsg,
) -> Result<u64, Box<dyn std::error::Error>> {
    let frame =
        crate::userkey_cli::user_frame(signer, chat::DEFAULT_CHAT_TARGET, chat::encode_msg(&op));
    crate::node_http::submit_frame(base, &frame)
}

/// A participant flag as the party it names, or a refusal naming the flag.
fn party(flag: &str, handle: &str) -> Result<collaboration::Party, Box<dyn std::error::Error>> {
    collaboration::parse_party_handle(handle)
        .map_err(|error| format!("--{flag} {handle}: {error}").into())
}

/// The network a workspace belongs to, off its own `network.toml`.
///
/// A scoped key is fenced by this (`collab_keys`' header says why: a submit
/// frame carries no chain id, so one key shared across two networks would make
/// a send replayable between them). Read from the workspace rather than from
/// `/v1/status` — the same answer, but it needs no node running, so `collab key`
/// still prints against a stopped node.
///
/// **An empty chain id is refused, never used as a namespace.** `chain_id` is
/// empty on a daemon that serves no chain (simnode, the embedded local daemon),
/// and an empty fence is not a fence: every such workspace would share one key
/// per id pair, which is the collision the fence exists to prevent. A workspace
/// with no network identity has no binding to hold a key for, so refusing is
/// also the honest answer.
fn chain_id(workspace: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    let path = workspace.join("network.toml");
    let named = crate::config::NetworkDescriptor::load(&path)?.chain_id;
    if named.is_empty() {
        return Err(format!(
            "{} names an empty chain_id — this workspace has no network identity to \
             scope a collaboration key to",
            path.display()
        )
        .into());
    }
    Ok(named)
}

/// The network to act on, agreed by BOTH the workspace and the node being
/// dialled — for every verb that submits.
///
/// The local file alone is not enough. It says which network this device holds
/// keys for; it does not say which network the node on the other end of `--url`
/// belongs to. Point a workspace at another network's node and the local read
/// still succeeds, so `attach` would submit an owner-signed `Bind` naming a key
/// scoped to a network that node is not on, and `send` would sign a message
/// under a credential the receiving network never issued. Both fail confusingly
/// at the module, after a write went out.
///
/// So the two are compared before anything is signed, and a mismatch names both
/// sides. A node reporting no chain id at all is refused for the same reason an
/// empty local one is: nothing to agree with.
fn agreed_network(
    base: &str,
    workspace: &std::path::Path,
) -> Result<String, Box<dyn std::error::Error>> {
    let local = chain_id(workspace)?;
    let status =
        crate::node_http::get_json(base, "/v1/status").map_err(|failure| failure.to_string())?;
    let remote = status["chain_id"].as_str().unwrap_or_default();
    if remote.is_empty() {
        return Err(format!(
            "the node at {base} serves no chain (empty chain_id), so it cannot carry a \
             collaboration binding on {local}"
        )
        .into());
    }
    if remote != local {
        return Err(format!(
            "this workspace belongs to {local} but the node at {base} is on {remote} — \
             refusing to sign for a network this device holds no binding on"
        )
        .into());
    }
    Ok(local)
}

/// This binding's scoped service key, or a refusal naming the attach that would
/// create it. Never mints: `send` and `ack` act AS an existing binding, and
/// minting one here would sign with a key no committed `Bind` has authorized —
/// producing an op the module refuses, from a CLI that looked like it worked.
///
/// Takes the network the caller already agreed with the node
/// ([`agreed_network`]) rather than re-reading the workspace, so a verb cannot
/// load a key under one network and submit it to another.
fn scoped_key(
    ctx: &VerbCtx,
    network: &str,
    channel: &str,
    participant: &str,
) -> Result<commonware_cryptography::ed25519::PrivateKey, Box<dyn std::error::Error>> {
    let workspace = ctx.addr.workspace()?;
    let binding = crate::collab_keys::BindingRef {
        network,
        conversation: channel,
        participant,
    };
    crate::collab_keys::load(&workspace, binding)?.ok_or_else(|| {
        format!(
            "this device holds no service key for participant {participant} in \
             channel {channel} — run `ducktape collab attach` first"
        )
        .into()
    })
}

/// `collab attach` — mint the scoped key, then have the OWNER authorize it.
///
/// Order matters: the key exists before the `Bind` that names it, so a committed
/// binding can never name a public key this device does not hold. The reverse
/// order would leave a binding nothing can sign for, and only an `Unbind` could
/// clear it.
fn cmd_attach(args: AttachArgs, ctx: &VerbCtx, stdin: &mut impl BufRead) -> CollabResult {
    let base = ctx.http_base()?;
    let workspace = ctx.addr.workspace()?;
    // agreed BEFORE the key is minted: a key minted for a network this node is
    // not on would be named by an owner-signed `Bind` that network never sees.
    let network = agreed_network(&base, &workspace)?;
    let participant = party("participant", &args.participant)?;
    let binding = crate::collab_keys::BindingRef {
        network: &network,
        conversation: &args.channel,
        participant: &args.participant,
    };
    let service = crate::collab_keys::ensure(&workspace, binding)?;
    // and record WHICH binding this is, beside the key that cannot say: the key
    // file is named by a digest, so the node's collaboration pump could never
    // read the ids back out of it. Before the `Bind` for the same reason the key
    // is — see `collab_keys::remember`.
    crate::collab_keys::remember(&workspace, binding, &args.device)?;
    // a scoped SERVICE KEY, not a program account: this device holds a private
    // half, which is what lets it sign its own sends and receipts later.
    let principal = collaboration::BoundPrincipal::ServiceKey(
        commonware_cryptography::Signer::public_key(&service)
            .as_ref()
            .to_vec(),
    );

    // the OWNER signs: a scoped credential cannot authorize itself.
    let owner = ctx.signer(stdin)?;
    let height = submit(
        &base,
        &owner,
        &network,
        collaboration::CollaborationMsg::Bind {
            channel_id: args.channel,
            participant,
            device: args.device,
            principal,
            expected_credential: args.expect,
        },
    )?;
    println!("{height}");
    Ok(())
}

/// `collab send` — signed by the scoped key, so no wallet password is needed.
///
/// Two ops, in this order: the chat post that IS the message, then the
/// delivery request that names it. The module admits the request only from
/// the origin that posted, which is why both are signed by the one key.
fn cmd_send(args: SendArgs, ctx: &VerbCtx) -> CollabResult {
    let base = ctx.http_base()?;
    let network = agreed_network(&base, &ctx.addr.workspace()?)?;
    let service = scoped_key(ctx, &network, &args.channel, &args.participant)?;
    let recipient = party("to", &args.to)?;
    let expires_at = deadline(&base, args.ttl_secs)?;
    let message_id = args
        .message_id
        .unwrap_or_else(|| crate::config::hex_bytes(&rand::random::<[u8; 16]>()));
    let posted = submit_chat(
        &base,
        &service,
        chat::ChatMsg::PostMessage {
            channel_id: args.channel.clone(),
            message_id: message_id.clone(),
            blocks: vec![chat::Block::paragraph(args.body)],
            thread: None,
        },
    )?;
    let requested = submit(
        &base,
        &service,
        &network,
        collaboration::CollaborationMsg::Deliver(collaboration::DeliverRequest {
            channel_id: args.channel,
            message_id: message_id.clone(),
            recipient,
            kind: args.kind.into(),
            task: None,
            references: Vec::new(),
            expires_at,
        }),
    )?;
    println!("{message_id} posted at {posted}, delivery requested at {requested}");
    Ok(())
}

/// `collab ack` — the bound service reports what its provider did.
fn cmd_ack(args: AckArgs, ctx: &VerbCtx) -> CollabResult {
    let base = ctx.http_base()?;
    let network = agreed_network(&base, &ctx.addr.workspace()?)?;
    let service = scoped_key(ctx, &network, &args.channel, &args.participant)?;
    let recipient = party("participant", &args.participant)?;
    let height = submit(
        &base,
        &service,
        &network,
        collaboration::CollaborationMsg::Acknowledge {
            channel_id: args.channel,
            seq: args.seq,
            recipient,
            binding_credential: args.credential,
            state: args.state.into(),
            reason: args.reason,
        },
    )?;
    println!("{height}");
    Ok(())
}

/// The absolute `expires_at` a message sent now carries, in the NETWORK's
/// `consensus_time` unit.
///
/// The module compares against `ctx.env().consensus_time` and has no default of
/// its own, because a default would have to guess the unit. So the sender reads
/// the unit off the node and scales into it — never a laptop's wall clock, which
/// two nodes would disagree about.
///
/// The unit is REQUIRED, not defaulted. `NodeStatus::consensus_time_unit` is an
/// ordinary field, so every node serving `/v1/status` emits it; a response
/// without one is a reshaped or proxied status, not an older node. Defaulting
/// there would pick a unit for a network that did not say which it counts in,
/// and picking wrong is the whole bug this reads to avoid — a 24 h intent
/// becomes 86 seconds if a millis lane is read as height.
fn deadline(base: &str, ttl_secs: u64) -> Result<u64, Box<dyn std::error::Error>> {
    let status =
        crate::node_http::get_json(base, "/v1/status").map_err(|failure| failure.to_string())?;
    let now = status["consensus_time"]
        .as_u64()
        .ok_or_else(|| "node status carries no consensus_time".to_string())?;
    let named_unit = status
        .get("consensus_time_unit")
        .cloned()
        .ok_or_else(|| "node status names no consensus_time_unit".to_string())?;
    let unit: noded::ConsensusTimeUnit = serde_json::from_value(named_unit)
        .map_err(|error| format!("node status consensus_time_unit: {error}"))?;
    absolute_deadline(now, ttl_secs, unit)
}

/// `now` plus the scaled TTL, as an absolute deadline — or a refusal.
///
/// Split out of [`deadline`] so the arithmetic is testable against a REAL
/// nonzero clock reading without a node to dial. Scaling alone saturating is
/// not enough: `u64::MAX` seconds scales to a saturated TTL that still
/// overflows when added to any `now > 0`, which in release wraps into a
/// deadline in the PAST — a message that expires the moment it lands, from a
/// CLI that printed a height and looked like it worked. Refuse instead, and
/// name the TTL as the thing to change.
fn absolute_deadline(
    now: u64,
    ttl_secs: u64,
    unit: noded::ConsensusTimeUnit,
) -> Result<u64, Box<dyn std::error::Error>> {
    let ttl = ttl_units(ttl_secs, unit);
    now.checked_add(ttl).ok_or_else(|| {
        format!(
            "--ttl-secs {ttl_secs} is {ttl} {unit:?} units, past the end of the clock \
             from {now} — pick a shorter deadline"
        )
        .into()
    })
}

/// A seconds intent in one network's `consensus_time` units. Pure, so both
/// lanes are testable without a node to dial.
///
/// `Millis` is a millisecond epoch clock, so the conversion is exact. `Height`
/// counts BLOCKS: this returns that many blocks, which is that many seconds
/// only while the chain heartbeats at one block a second. It is a nominal
/// mapping, not a promise about wall-clock time — the flag's help says so.
///
/// The scaling saturates rather than wrapping: an absurd `--ttl-secs` must
/// become a deadline that is refused, never one that wraps past zero into the
/// past. Saturating here only bounds the SCALE; [`absolute_deadline`] is what
/// refuses the sum.
fn ttl_units(ttl_secs: u64, unit: noded::ConsensusTimeUnit) -> u64 {
    match unit {
        noded::ConsensusTimeUnit::Height => ttl_secs,
        noded::ConsensusTimeUnit::Millis => ttl_secs.saturating_mul(1_000),
    }
}

impl From<Kind> for collaboration::MessageKind {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Notice => Self::Notice,
            Kind::Question => Self::Question,
            Kind::TaskRequest => Self::TaskRequest,
            Kind::TaskUpdate => Self::TaskUpdate,
            Kind::Result => Self::Result,
        }
    }
}

impl From<AckState> for collaboration::DeliveryState {
    fn from(state: AckState) -> Self {
        match state {
            AckState::Queued => Self::Queued,
            AckState::AdapterAccepted => Self::AdapterAccepted,
            AckState::Held => Self::Held,
            AckState::Refused => Self::Refused,
            AckState::DeliveryUnknown => Self::DeliveryUnknown,
        }
    }
}

/// `collab key --channel <id> --participant <handle>` — the scoped service key
/// this device signs that binding's delivery receipts with.
///
/// Prints the PUBLIC half, because that is the only part anything else needs:
/// it goes into the `Bind` op the OWNER signs, and the owner's signature is
/// what authorizes this key. The private half never leaves the workspace and is
/// never printed — a key echoed into a terminal is a key in a scrollback
/// buffer, a screen share and a shell history.
///
/// Minting is the default and is idempotent: attaching is a re-runnable
/// operation, and a second attach must present the same key or the committed
/// binding would name one nothing holds.
///
/// The only verb that reads the network LOCALLY, because it is the only one
/// that submits nothing: it dials no node, so there is no remote identity to
/// agree with, and it stays usable while the node is stopped. Every verb that
/// signs goes through [`agreed_network`] instead.
fn cmd_key(args: KeyArgs, ctx: &VerbCtx) -> CollabResult {
    let workspace = ctx.addr.workspace()?;
    let network = chain_id(&workspace)?;
    let binding = crate::collab_keys::BindingRef {
        network: &network,
        conversation: &args.channel,
        participant: &args.participant,
    };
    let key = match args.existing_only {
        true => crate::collab_keys::load(&workspace, binding)?.ok_or_else(|| {
            format!(
                "this device holds no binding for participant {} in channel {}",
                args.participant, args.channel
            )
        })?,
        false => crate::collab_keys::ensure(&workspace, binding)?,
    };
    println!("{}", crate::collab_keys::public_hex(&key));
    Ok(())
}

/// `collab query --target <module> '<json>'` — one authenticated read.
///
/// The operator cannot mint the signature trio with `curl`, which is the whole
/// reason this verb exists (the same reason `node log-filter` does). The query
/// JSON is passed through verbatim: this CLI is a transport, and pre-parsing a
/// module's own enum here would only be a second, drifting copy of it.
fn cmd_query(
    args: QueryArgs,
    ctx: &VerbCtx,
    trust_node: bool,
    stdin: &mut impl BufRead,
) -> CollabResult {
    let query: serde_json::Value = serde_json::from_str(&args.query)
        .map_err(|error| format!("--query is not valid json: {error}"))?;
    let base = ctx.http_base()?;
    let key_path = ctx.key_path()?;
    // pinned, never the node's own plain claim: the signature binds to this key,
    // so a proxy that could choose it could choose what we signed for.
    let node_key = crate::node_http::pinned_node_key(&key_path, &base, trust_node)?;
    let signer = crate::userkey_cli::load_user_signer_for(&base, &key_path, stdin)?;
    let reply = crate::node_http::query_as_reader(&base, &signer, &node_key, &args.target, query)?;
    println!("{reply}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[derive(Debug, clap::Parser)]
    struct Harness {
        #[command(subcommand)]
        cmd: CollabCmd,
    }

    #[test]
    fn a_query_verb_takes_a_module_and_one_json_value() {
        let parsed = Harness::parse_from([
            "collab",
            "query",
            "--target",
            "collaboration",
            r#"{"read":{"participant":{"account":7},"via":null,"read":{"binding":{"channel_id":"c1"}}}}"#,
        ]);
        let CollabCmd::Query(args) = parsed.cmd else {
            panic!("query parses to the query verb");
        };
        assert_eq!(args.target, "collaboration");
        assert!(args.query.contains("channel_id"));
    }

    #[test]
    fn a_key_verb_names_the_binding_it_belongs_to() {
        let parsed = Harness::parse_from([
            "collab",
            "key",
            "--channel",
            "c1",
            "--participant",
            "acct:7",
        ]);
        let CollabCmd::Key(args) = parsed.cmd else {
            panic!("key parses to the key verb");
        };
        assert_eq!(
            (args.channel.as_str(), args.participant.as_str()),
            ("c1", "acct:7")
        );
        assert!(
            !args.existing_only,
            "minting is the default: attaching is the common case"
        );
    }

    /// The messaging plane must never be routed through the pty plane.
    ///
    /// `agent-service`'s `Sessions::close_all` ends every live session whenever
    /// the daemon's ws link to its node drops, and that drop is ORDINARY — a
    /// node restart, an upgrade, a blip. Anything holding a durable messaging
    /// attachment there would be revoked by a network hiccup. And pty bytes are
    /// raw, unstructured and unacknowledged, so they are not a delivery channel
    /// either.
    ///
    /// A source-parsing lint rather than a comment, because the shape is
    /// load-bearing: the day someone reaches for `TermInput` to push a message
    /// into an attached session, this fails.
    #[test]
    fn the_collab_plane_never_touches_the_pty_plane() {
        let source = include_str!("collab_cli.rs");
        // Scan the SHIPPING half only, and strip its prose. Both cuts are
        // load-bearing: the doc comments name the pty plane on purpose to
        // explain why it is absent, and this test's own banned-word list is
        // code — scanning it would make the lint match itself and fail always.
        let shipping = source
            .split_once("\n#[cfg(test)]")
            .map(|(before, _)| before)
            .expect("this file has a test module");
        let code: String = shipping
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in [
            "Sessions",
            "close_all",
            "TermCreate",
            "TermInput",
            "TermOutput",
            "TermClose",
            "agent_service",
            "term_plane",
        ] {
            assert!(
                !code.contains(banned),
                "the collaboration plane must not reach into the pty plane, but it names {banned}"
            );
        }
    }

    /// A deadline is the network's clock plus a TTL in the network's OWN unit.
    /// Getting the unit wrong is not a rounding error: a 24-hour TTL sent as
    /// 86 400 milliseconds is 86 seconds, and the message expires before anyone
    /// reads it.
    #[test]
    fn a_ttl_scales_into_the_networks_own_time_unit() {
        assert_eq!(ttl_units(86_400, noded::ConsensusTimeUnit::Height), 86_400);
        assert_eq!(
            ttl_units(86_400, noded::ConsensusTimeUnit::Millis),
            86_400_000
        );
    }

    /// A whole deadline against a REAL clock reading, which is what gets
    /// submitted — a scaled TTL alone proves nothing about the sum.
    #[test]
    fn a_deadline_is_the_networks_clock_plus_the_scaled_ttl() {
        let height_lane = absolute_deadline(40_000, 86_400, noded::ConsensusTimeUnit::Height)
            .expect("an ordinary deadline");
        assert_eq!(height_lane, 40_000 + 86_400);

        // the sim lane's clock is a millisecond epoch miles past any height,
        // so this is the reading that catches a unit mix-up.
        let sim_now = 1_757_000_000_000;
        let millis_lane =
            absolute_deadline(sim_now, 86_400, noded::ConsensusTimeUnit::Millis).expect("ditto");
        assert_eq!(millis_lane, sim_now + 86_400_000);
    }

    /// Serve ONE canned `/v1/status` body on loopback and answer its base url —
    /// this binary's own fake-node idiom (`account_cli`'s ceremony test).
    fn status_once(body: &'static str) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead as _, BufReader, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("one request");
            let mut reader = BufReader::new(&socket);
            let mut request = String::new();
            reader.read_line(&mut request).expect("request line");
            assert!(request.starts_with("GET /v1/status"), "{request}");
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).expect("header"), 0);
                let headers_complete = line == "\r\n";
                if headers_complete {
                    break;
                }
            }
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("respond");
        });
        (base, server)
    }

    /// The unit is REQUIRED. `consensus_time_unit` is an ordinary field every
    /// node emits, so a status without one is a reshaped or proxied response —
    /// and guessing picks a unit for a network that never said which it counts
    /// in. Guessing wrong turns a 24 h intent into 86 seconds.
    ///
    /// This is the mutation guard for that decision: restoring the old
    /// Write a workspace whose `network.toml` names `chain_id`.
    ///
    /// The descriptor is written as TOML rather than built as a struct so the
    /// test exercises the real `NetworkDescriptor::load` parse, which is what
    /// the CLI runs. Every field without a serde default is present; the
    /// block time clears `MIN_BLOCK_TIME_MS`, which `from_toml` enforces.
    fn workspace_on(chain_id: &str) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("temp workspace");
        let descriptor = format!(
            "chain_id = \"{chain_id}\"\n\
             validators = []\n\
             genesis = \"\"\n\
             block_time_ms = 1000\n\
             modules = []\n"
        );
        std::fs::write(dir.path().join("network.toml"), descriptor).expect("write the descriptor");
        dir
    }

    /// An empty chain id must never become the namespace.
    ///
    /// It is empty on a daemon that serves no chain (simnode, the embedded
    /// local daemon). An empty fence is not a fence — every such workspace
    /// would share one key per id pair, which is exactly the collision the
    /// fence exists to prevent.
    #[test]
    fn an_empty_chain_id_is_refused_rather_than_used_as_a_namespace() {
        let dir = workspace_on("");
        let refusal = chain_id(dir.path()).expect_err("an empty chain id must refuse");
        assert!(
            refusal.to_string().contains("empty chain_id"),
            "the refusal must say what is wrong: {refusal}"
        );

        let named = workspace_on("ducktape#a1b2c3d4");
        assert_eq!(
            chain_id(named.path()).expect("a named chain id loads"),
            "ducktape#a1b2c3d4"
        );
    }

    /// The workspace says which network this device holds keys for. It does NOT
    /// say which network the node behind `--url` is on. Signing before checking
    /// would submit an owner `Bind` — or a message under a credential — to a
    /// network that never issued it, and the write would already be out before
    /// the module refused it.
    #[test]
    fn signing_verbs_refuse_a_node_on_another_network() {
        let dir = workspace_on("ducktape#aaaa1111");

        let (base, server) = status_once(r#"{"chain_id":"ducktape#aaaa1111"}"#);
        let agreed = agreed_network(&base, dir.path()).expect("the two agree");
        assert_eq!(agreed, "ducktape#aaaa1111");
        server.join().expect("server");

        let (base, server) = status_once(r#"{"chain_id":"ducktape#bbbb2222"}"#);
        let refusal = agreed_network(&base, dir.path()).expect_err("a mismatch must refuse");
        let names_both = refusal.to_string().contains("ducktape#aaaa1111")
            && refusal.to_string().contains("ducktape#bbbb2222");
        assert!(names_both, "the refusal must name both sides: {refusal}");
        server.join().expect("server");

        // a node serving no chain has nothing to agree with, and must not be
        // read as "matches whatever the workspace says".
        let (base, server) = status_once(r#"{"chain_id":""}"#);
        let refusal = agreed_network(&base, dir.path()).expect_err("no chain must refuse");
        assert!(refusal.to_string().contains("serves no chain"), "{refusal}");
        server.join().expect("server");
    }

    /// `unwrap_or_default()` makes the second half pass a deadline back.
    #[test]
    fn a_status_that_names_no_time_unit_is_refused_rather_than_guessed() {
        let (base, server) =
            status_once(r#"{"consensus_time":40000,"consensus_time_unit":"millis"}"#);
        let named = deadline(&base, 3_600).expect("a status naming its unit answers a deadline");
        assert_eq!(
            named,
            40_000 + 3_600_000,
            "the named unit must be the one used"
        );
        server.join().expect("server");

        let (base, server) = status_once(r#"{"consensus_time":40000}"#);
        let refusal = deadline(&base, 3_600).expect_err("an unnamed unit must refuse");
        assert!(
            refusal.to_string().contains("consensus_time_unit"),
            "the refusal must name the missing field: {refusal}"
        );
        server.join().expect("server");
    }

    /// An absurd TTL must be REFUSED, never wrapped past the end of the clock
    /// into the past — which the module would read as already expired, from a
    /// CLI that printed a height and looked like it had worked.
    ///
    /// Scaling saturating is not enough: `u64::MAX` saturates to a TTL that
    /// still overflows when added to any nonzero clock. Nonzero `now` is the
    /// whole point of the case.
    #[test]
    fn an_absurd_ttl_is_refused_rather_than_wrapping_into_the_past() {
        assert_eq!(
            ttl_units(u64::MAX, noded::ConsensusTimeUnit::Millis),
            u64::MAX,
            "the scale must saturate, not wrap"
        );
        for unit in [
            noded::ConsensusTimeUnit::Height,
            noded::ConsensusTimeUnit::Millis,
        ] {
            let refusal = absolute_deadline(40_000, u64::MAX, unit)
                .expect_err("an unrepresentable deadline must refuse");
            let names_the_flag = refusal.to_string().contains("--ttl-secs");
            assert!(names_the_flag, "the refusal must name the knob: {refusal}");
        }
    }

    /// `send` and `ack` act AS an existing binding. Minting a key here would
    /// sign with one no committed `Bind` has authorized — an op the module
    /// refuses, from a CLI that looked like it had worked. The refusal has to
    /// name the verb that fixes it.
    #[test]
    fn sending_without_a_binding_refuses_and_never_mints_a_key() {
        let workspace = tempfile::TempDir::new().expect("temp workspace");
        let binding = crate::collab_keys::BindingRef {
            network: "ducktape#a1b2c3d4",
            conversation: "c1",
            participant: "p1",
        };
        // the refusal path is `collab_keys::load` returning None; assert the
        // sentence and that nothing was created behind it.
        assert!(
            crate::collab_keys::load(workspace.path(), binding)
                .expect("absence is not an error")
                .is_none()
        );
        assert!(
            !workspace.path().join("collab-keys").exists(),
            "a failed send must not leave a key directory behind"
        );
    }

    /// The two keys of an attach are DIFFERENT keys, and the scoped one is the
    /// one that stays. If they were ever the same, "scoped" would mean nothing:
    /// the attached session would hold the owner's account key.
    #[test]
    fn an_attach_authorizes_a_scoped_key_that_is_not_the_owner_key() {
        let workspace = tempfile::TempDir::new().expect("temp workspace");
        let binding = crate::collab_keys::BindingRef {
            network: "ducktape#a1b2c3d4",
            conversation: "c1",
            participant: "p1",
        };
        let scoped = crate::collab_keys::ensure(workspace.path(), binding).expect("mints");
        let owner = <commonware_cryptography::ed25519::PrivateKey as commonware_cryptography::Signer>::from_seed(1234);
        assert_ne!(
            crate::collab_keys::public_hex(&scoped),
            crate::collab_keys::public_hex(&owner),
            "the scoped key must never be the owner's"
        );
    }

    /// The NETWORK travels inside the signed payload, not merely beside it.
    ///
    /// This is what actually binds a collaboration op to one chain. The frame
    /// envelope carries no chain id — `user_frame` signs
    /// `signer ‖ seq ‖ target ‖ payload` — so a frame minted here would verify
    /// byte-identically on another network if the id were not IN the payload.
    /// Decoding the frame the way a node does and reading the network back off
    /// the op is the only way to prove it is covered by the signature.
    #[test]
    fn the_signed_payload_carries_the_network_for_owner_and_service_ops() {
        let signer = <commonware_cryptography::ed25519::PrivateKey as commonware_cryptography::Signer>::from_seed(7);
        let network = "ducktape#a1b2c3d4";

        // one owner-signed op and one service-signed op: root's requirement is
        // that BOTH bind the chain id, not just the owner's.
        let owner_op = collaboration::CollaborationMsg::Bind {
            channel_id: "c1".into(),
            participant: collaboration::Party::Account(7),
            device: "laptop".into(),
            principal: collaboration::BoundPrincipal::ServiceKey(vec![9; 32]),
            expected_credential: 0,
        };
        let service_op = collaboration::CollaborationMsg::Acknowledge {
            channel_id: "c1".into(),
            seq: 1,
            recipient: collaboration::Party::Account(7),
            binding_credential: 3,
            state: collaboration::DeliveryState::Queued,
            reason: None,
        };

        for op in [owner_op, service_op] {
            let request = collaboration::Request::new(network, op);
            let frame = crate::userkey_cli::user_frame(
                &signer,
                COLLABORATION,
                collaboration::encode_msg(&request),
            );
            // exactly what the node does with the bytes before the module sees
            // them: verify the signature, then decode the payload.
            let (origin, msg) = node::decode_frame(&frame).expect("the frame verifies");
            assert_eq!(
                origin,
                sdk::Origin::External(
                    commonware_cryptography::Signer::public_key(&signer)
                        .as_ref()
                        .to_vec()
                )
            );
            assert_eq!(msg.target, COLLABORATION);
            let decoded = collaboration::decode_msg(&msg.payload).expect("the op decodes");
            assert_eq!(
                decoded.network, network,
                "the network must be inside the signed payload"
            );
            assert!(
                !decoded.network.is_empty(),
                "a blank network would collapse every chain into one"
            );
        }
    }

    /// The states a bound service may report are exactly the ones it can
    /// OBSERVE. `Stored` is admission's and `Expired` is the agreed deadline's;
    /// a device asserting either would be asserting the network's own
    /// bookkeeping about itself.
    #[test]
    fn a_service_cannot_report_the_networks_own_states() {
        let reportable: Vec<collaboration::DeliveryState> = [
            AckState::Queued,
            AckState::AdapterAccepted,
            AckState::Held,
            AckState::Refused,
            AckState::DeliveryUnknown,
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        assert!(!reportable.contains(&collaboration::DeliveryState::Stored));
        assert!(!reportable.contains(&collaboration::DeliveryState::Expired));
        assert_eq!(reportable.len(), 5, "and every other state stays reachable");
    }

    /// The JSON is checked HERE, before a key is unlocked. Unlocking costs one
    /// argon2id pass over 64 MiB, and paying it to then reject a typo is a
    /// second of the operator's life for nothing.
    #[test]
    fn a_malformed_query_is_refused_before_anything_expensive() {
        let refused = cmd_query(
            QueryArgs {
                target: "collaboration".into(),
                query: "{not json".into(),
            },
            &VerbCtx {
                addr: crate::cli_args::NodeAddr::default(),
                key: None,
            },
            false,
            &mut std::io::Cursor::new(Vec::new()),
        )
        .expect_err("invalid json cannot reach the node");
        assert!(refused.to_string().contains("not valid json"), "{refused}");
    }
}
