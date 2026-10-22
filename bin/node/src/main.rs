#[cfg(unix)]
mod bridge;
mod json;
mod machine;
mod mcp;

use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;

use abi::role::validators::{Grant, Invite, Motion, Op, QueryPolicy, Voted};
use abi::{BlobId, HashKind, Outcome, Scan, Scheme, role};
use clap::{Args, Parser, Subcommand};
use commonware_cryptography::{Signer as _, ed25519};
use commonware_runtime::Runner as _;
use commonware_runtime::tokio::{Config, Runner};
use futures::StreamExt as _;
use host::{Layer, RECENT};
use node::{Frame, Preconfirm};
use noded::wire::{Admin, EgressSet, Submitted};
use noded::{
    Client, ConsensusSettings, CpuList, Endpoint, HaloConfig, LoadConfig, LoadSettings, Logs,
    NodeConfig, Settings, Upload, Workspace,
};

const CLIENT: &str = concat!("ducktape-cli/", env!("CARGO_PKG_VERSION"));

#[derive(Parser)]
#[command(name = "ducktape", about = "a ducktape node and its client", version)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "the node's workspace [default: $DUCKTAPE_HOME]"
    )]
    workspace: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        help = "the node to talk to [default: the workspace's node]"
    )]
    node: Option<String>,
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand)]
enum Verb {
    #[command(about = "print the binary's version and node contract")]
    Version,
    #[command(about = "print this workspace's node key, minting one if absent")]
    Identity,
    #[command(about = "found a network in this workspace from a founding file")]
    Init {
        founding: PathBuf,
        #[arg(
            long,
            value_parser = registered_halo,
            help = "host:port where members reach this node over halo (QUIC on UDP); it seats \
                    this node's validator entry in the founding file, or must match it"
        )]
        halo: String,
    },
    #[command(about = "enroll this node in a network, then sync its state from a running node")]
    Join {
        source: String,
        #[arg(
            long,
            help = "where peers dial this node: tcp://host:port, tls://host:port, \
                    ws://host[:port] or wss://host[:port]; without it the node dials \
                    the others and is never dialed"
        )]
        address: Option<Endpoint>,
        #[arg(
            long,
            value_parser = registered_halo,
            help = "host:port where members reach this node over halo (QUIC on UDP)"
        )]
        halo: String,
        #[arg(
            long,
            help = "a validator's invite, in hex; none when the door is open"
        )]
        invite: Option<String>,
    },
    #[command(
        about = "enroll this workspace's key as a resident member that runs no node, such as a \
                 box serving egress over halo with `egress serve`; nothing is synced"
    )]
    Enroll {
        source: String,
        #[arg(
            long,
            value_parser = registered_halo,
            help = "host:port where members reach this key over halo (QUIC on UDP)"
        )]
        halo: String,
        #[arg(
            long,
            help = "a validator's invite, in hex; none when the door is open"
        )]
        invite: Option<String>,
    },
    #[command(about = "run the node; a flag overrides the workspace's node.toml")]
    Run(Box<RunArgs>),
    #[command(
        about = "write the block history this build keeps back into the archives an older \
                 build reads; run it with the node stopped, before an older build starts on \
                 this workspace"
    )]
    RestoreArchives,
    #[command(
        about = "carry this stopped workspace across a contract bump that changes how blocks \
                 decode: at the announced roll height it is stamped for this binary's contract; \
                 behind it, it state-syncs from its source first; past it, it is refused"
    )]
    Roll { height: u64 },
    #[command(
        subcommand,
        about = "a throwaway fork of a stopped network's state, run alone with no consensus: it takes any signer's ops with no proof, and makes blocks on its parent's block time or by hand"
    )]
    Fork(ForkVerb),
    #[command(about = "print the node's height, tip, root, epoch and identity")]
    Status,
    #[command(
        about = "submit a signed frame to the pool; the payload is a file or stdin. With \
                 --preconfirm the node also runs it now and prints its predicted receipt; with \
                 --land it waits for the block that carries the frame and prints that block's \
                 height and receipt, or `unlanded` when no block carried it before its anchor \
                 aged out"
    )]
    Submit {
        program: String,
        payload: Option<PathBuf>,
        #[arg(long, conflicts_with = "land")]
        preconfirm: bool,
        #[arg(long)]
        land: bool,
    },
    #[command(
        about = "send a program an op written as the JSON its schema reads (harness's network_schema \
                 shape), signed by the active wallet key. Prints JSON: the pooled frame; with \
                 --preconfirm the predicted outcome and what the op returned; with --land the block \
                 that carried it and what the op returned. A rejected or unlanded op fails"
    )]
    Op {
        program: String,
        op: String,
        #[arg(long, conflicts_with = "land")]
        preconfirm: bool,
        #[arg(long)]
        land: bool,
    },
    #[command(
        about = "ask a program a query written as the JSON its schema reads, and print its reply as \
                 JSON; with --raw the query is borsh bytes from a file or stdin and the answer is its \
                 raw bytes on stdout"
    )]
    Query {
        program: String,
        #[arg(required_unless_present = "raw", conflicts_with = "raw")]
        query: Option<String>,
        #[arg(long, value_name = "FILE", num_args = 0..=1)]
        raw: Option<Option<PathBuf>>,
        #[arg(long)]
        confirmed: bool,
    },
    #[command(
        about = "serve the Model Context Protocol on stdin and stdout, one JSON-RPC message a line, \
                 for any MCP client: network_schema, network_query and network_call over every \
                 program's live code, and files_read and files_write, each op and query signed by \
                 the active wallet key, whose password comes from DUCKTAPE_PASSWORD; it refuses a \
                 key that acts as no agent account"
    )]
    Mcp,
    #[cfg(unix)]
    #[command(
        subcommand,
        about = "answer as an agent account from an agent product: hear what attribution tells \
                 the account, run one product session per chat thread it answers in, and draft \
                 and post each answer, signed by the active wallet key"
    )]
    Bridge(BridgeVerb),
    #[command(about = "print a key's value; the key is hex, the value is raw")]
    Get {
        program: String,
        key: String,
        #[arg(long)]
        confirmed: bool,
    },
    #[command(about = "list a key range, one hex key and value per line")]
    Scan {
        program: String,
        #[arg(long, default_value = "")]
        lo: String,
        #[arg(long)]
        hi: Option<String>,
        #[arg(long)]
        reverse: bool,
        #[arg(long)]
        limit: Option<u64>,
        #[arg(long)]
        confirmed: bool,
    },
    #[command(subcommand, about = "the content-addressed blobs the node holds")]
    Blob(BlobVerb),
    #[command(about = "follow a program's confirmed writes, one line per key")]
    Changes { program: String },
    #[command(about = "list every program and its code blob")]
    Programs,
    #[command(about = "print the node's log ring")]
    Logs,
    #[command(about = "retune the running node's log filter")]
    LogFilter { directives: String },
    #[command(about = "stop the running node")]
    Shutdown,
    #[command(
        subcommand,
        about = "what the running node serves through the egress role"
    )]
    Egress(EgressVerb),
    #[command(about = "print the node's metrics")]
    Metrics,
    #[command(about = "print an invite this node signs: one stranger enrolls with it, once")]
    Invite {
        #[arg(long, default_value_t = 72)]
        hours: u64,
    },
    #[command(subcommand, about = "vote, as a validator, on a motion")]
    Vote(VoteVerb),
    #[command(about = "take this node out of the network's members")]
    Leave,
    #[command(about = "register the host:port where members reach this node over halo")]
    Halo {
        #[arg(value_parser = registered_halo)]
        halo: String,
    },
    #[command(subcommand, about = "the user keys that sign frames")]
    Wallet(WalletVerb),
    #[command(
        about = "pipe stdin and stdout to a service a member's node forwards over halo's \
                 ducktape/forward lane, through the node --node names, in a call signed by the \
                 active wallet key: what ssh's ProxyCommand runs"
    )]
    Forward {
        #[arg(help = "the member node's key, in hex")]
        member: String,
        #[arg(help = "the service its forward.toml names")]
        service: String,
    },
    #[command(
        subcommand,
        about = "run the work fleet assigns to the machines [machine] names, signed by machine.key"
    )]
    Machine(MachineVerb),
}

#[derive(Subcommand)]
enum ForkVerb {
    #[command(
        about = "make this workspace a fork of the stopped workspace --from: its state and blobs under a network id of its own; then `ducktape run` drives it"
    )]
    New {
        #[arg(long, help = "the stopped workspace the fork starts from")]
        from: PathBuf,
    },
    #[command(
        about = "on a fork, send a program an op as the signer key names, with no proof, and print what `op` prints; a validators vote is one of these per validator key"
    )]
    As {
        #[arg(help = "the signer's public key, in hex")]
        signer: String,
        program: String,
        op: String,
        #[arg(long, conflicts_with = "land")]
        preconfirm: bool,
        #[arg(long)]
        land: bool,
    },
    #[command(about = "on a fork, make blocks back to back until its tip reaches the height")]
    Advance { height: u64 },
}

#[derive(Args)]
struct RunArgs {
    #[arg(long, help = setting("the socket peers reach over tcp", "listen"))]
    listen: Option<SocketAddr>,
    #[arg(
        long,
        help = defaulted(
            "the socket the HTTP API and ws/wss peers reach",
            "http",
            noded::DEFAULT_HTTP
        )
    )]
    http: Option<SocketAddr>,
    #[arg(long, help = setting("the socket peers reach over tls", "tls"))]
    tls: Option<SocketAddr>,
    #[arg(
        long,
        help = setting(
            "the UDP socket desktop clients reach live tracks on over QUIC; without it they take \
             the websocket on the http port",
            "live"
        )
    )]
    live: Option<SocketAddr>,
    #[arg(
        long,
        help = setting(
            "the UDP socket halo binds; members reach it at the halo address this node registered",
            "halo"
        )
    )]
    halo: Option<SocketAddr>,
    #[arg(long, help = defaulted("the log filter", "log", noded::DEFAULT_LOG))]
    log: Option<String>,
    #[arg(
        long,
        value_parser = CpuList::parse,
        help = setting(
            "the CPUs, such as 0-5, that the threads ordering, applying and storing blocks run on; \
             the rpc runtime takes every other CPU. Unset, both run on every CPU",
            "block_cpus"
        )
    )]
    block_cpus: Option<CpuList>,
    #[arg(
        long,
        help = setting(
            "the bytes per second this node sends to other members over halo; unlimited when \
             absent",
            "load.halo_egress"
        )
    )]
    halo_egress: Option<u64>,
    #[arg(
        long,
        value_parser = permits,
        help = defaulted(
            "the streams members open to this node that wait for it to take them; past it the \
             node takes no more until one is taken",
            "load.halo_inbound_streams",
            HaloConfig::default().inbound_streams
        )
    )]
    halo_inbound_streams: Option<NonZeroUsize>,
    #[arg(
        long,
        value_parser = permits,
        help = defaulted(
            "the datagrams from members that wait for this node to read them; past it a datagram \
             is dropped",
            "load.halo_datagrams",
            HaloConfig::default().datagrams
        )
    )]
    halo_datagrams: Option<NonZeroUsize>,
    #[arg(
        long,
        value_parser = permits,
        help = defaulted(
            "the blobs this node sends to members at once",
            "load.blob_serves_in_flight",
            LoadConfig::default().blob_serves_in_flight
        )
    )]
    blob_serves_in_flight: Option<NonZeroUsize>,
    #[arg(
        long,
        value_parser = permits,
        help = defaulted(
            "the members' blob requests that wait for a send; past it a request is refused",
            "load.blob_backlog",
            LoadConfig::default().blob_backlog
        )
    )]
    blob_backlog: Option<NonZeroUsize>,
    #[arg(
        long,
        help = defaulted(
            "the blobs this node gathers from members at once",
            "load.blob_gatherers",
            LoadConfig::default().blob_gatherers
        )
    )]
    blob_gatherers: Option<NonZeroUsize>,
    #[arg(
        long,
        value_parser = permits,
        help = defaulted(
            "the live messages waiting for one session's loop; a full queue holds the publisher \
             back through the transport",
            "load.live_session_inbound",
            LoadConfig::default().live_session_inbound
        )
    )]
    live_session_inbound: Option<NonZeroUsize>,
    #[arg(
        long,
        value_parser = permits,
        help = defaulted(
            "the live messages and objects waiting for one session's connection; a session this \
             far behind is closed",
            "load.live_session_outbound",
            LoadConfig::default().live_session_outbound
        )
    )]
    live_session_outbound: Option<NonZeroUsize>,
    #[arg(
        long,
        value_parser = permits,
        help = defaulted(
            "the objects waiting for one live group stream's writer, to a client or a peer; past \
             it the group is dropped short",
            "load.live_group_queue",
            LoadConfig::default().live_group_queue
        )
    )]
    live_group_queue: Option<NonZeroUsize>,
    #[arg(
        long,
        value_parser = permits,
        help = defaulted(
            "the live lane items waiting for one peer's writer; past it the writer is dropped and \
             the peer resynced",
            "load.live_peer_queue",
            LoadConfig::default().live_peer_queue
        )
    )]
    live_peer_queue: Option<NonZeroUsize>,
    #[arg(
        long,
        value_parser = permits,
        help = defaulted(
            "the live lane streams peers opened that wait for this node to take them",
            "load.live_lane_backlog",
            LoadConfig::default().live_lane_backlog
        )
    )]
    live_lane_backlog: Option<NonZeroUsize>,
    #[arg(
        long,
        value_parser = permits,
        help = defaulted(
            "the live lane datagrams waiting per peer; past it the oldest is dropped",
            "load.live_lane_per_sender",
            LoadConfig::default().live_lane_per_sender
        )
    )]
    live_lane_per_sender: Option<NonZeroUsize>,
    #[arg(
        long,
        help = defaulted(
            "how long a closed live session's last message gets to reach the client, in \
             milliseconds",
            "load.live_linger_ms",
            LoadConfig::default().live_linger.as_millis()
        )
    )]
    live_linger_ms: Option<NonZeroU64>,
    #[arg(
        long,
        value_parser = permits,
        help = defaulted(
            "the egress lane streams boxes opened that wait for this node to take them",
            "load.egress_lane_backlog",
            LoadConfig::default().egress_lane_backlog
        )
    )]
    egress_lane_backlog: Option<NonZeroUsize>,
    #[arg(
        long,
        value_parser = permits,
        help = defaulted(
            "the forward lane streams members opened that wait for this node to take them",
            "load.forward_lane_backlog",
            LoadConfig::default().forward_lane_backlog
        )
    )]
    forward_lane_backlog: Option<NonZeroUsize>,
    #[arg(
        long,
        help = defaulted(
            "the fuel one query this node answers off the chain may burn, the queries it makes \
             included; past it the query is refused as out of fuel. Frames never take it",
            "load.query_fuel",
            "no cap: every query is metered and counted"
        )
    )]
    query_fuel: Option<NonZeroU64>,
    #[arg(
        long,
        help = defaulted(
            "how long a view waits for its leader's proposal",
            "consensus.leader_timeout_ms",
            "one block time"
        )
    )]
    leader_timeout_ms: Option<u64>,
    #[arg(
        long,
        help = defaulted(
            "how long a view waits to certify a proposal",
            "consensus.certification_timeout_ms",
            "30 block times"
        )
    )]
    certification_timeout_ms: Option<u64>,
    #[arg(
        long,
        help = defaulted(
            "how often a timed-out view resends its timeout vote",
            "consensus.timeout_retry_ms",
            "2 block times"
        )
    )]
    timeout_retry_ms: Option<u64>,
}

impl RunArgs {
    fn settings(self) -> Settings {
        Settings {
            listen: self.listen,
            http: self.http,
            tls: self.tls,
            halo: self.halo,
            live: self.live,
            log: self.log,
            block_cpus: self.block_cpus,
            load: LoadSettings {
                halo_egress: self.halo_egress,
                halo_inbound_streams: self.halo_inbound_streams,
                halo_datagrams: self.halo_datagrams,
                blob_serves_in_flight: self.blob_serves_in_flight,
                blob_backlog: self.blob_backlog,
                blob_gatherers: self.blob_gatherers,
                live_session_inbound: self.live_session_inbound,
                live_session_outbound: self.live_session_outbound,
                live_group_queue: self.live_group_queue,
                live_peer_queue: self.live_peer_queue,
                live_lane_backlog: self.live_lane_backlog,
                live_lane_per_sender: self.live_lane_per_sender,
                live_linger_ms: self.live_linger_ms,
                egress_lane_backlog: self.egress_lane_backlog,
                forward_lane_backlog: self.forward_lane_backlog,
                query_fuel: self.query_fuel,
            },
            consensus: ConsensusSettings {
                leader_timeout_ms: self.leader_timeout_ms,
                certification_timeout_ms: self.certification_timeout_ms,
                timeout_retry_ms: self.timeout_retry_ms,
            },
            machine: None,
        }
    }
}

fn setting(what: &str, key: &str) -> String {
    format!("{what} [node.toml: {key}]")
}

fn defaulted(what: &str, key: &str, default: impl std::fmt::Display) -> String {
    format!("{what} [node.toml: {key}] [default: {default}]")
}

#[derive(Subcommand)]
enum BlobVerb {
    #[command(about = "fetch a blob's framed bytes to stdout")]
    Get { id: String },
    #[command(
        about = "put files on the network as one bundle of blobs, signed by the active wallet \
                 key's account; prints each blob's id"
    )]
    Put {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        #[arg(long, help = "the kind of every blob in the bundle")]
        kind: String,
        #[arg(long, help = "wait until a quorum of validators hold the bundle")]
        wait: bool,
    },
    #[command(about = "list the blob ids the state names and the node lacks")]
    Missing,
    #[command(
        about = "with the node stopped, write every blob in this workspace's store back as one \
                 framed file per blob, the layout builds before segments read"
    )]
    Unpack,
}

#[derive(Subcommand)]
enum EgressVerb {
    #[command(
        about = "make the running node re-read egress.toml and serve what it now names; \
                 an invalid file leaves what it serves unchanged"
    )]
    Reload,
    #[command(
        about = "send the running node a whole egress.toml to serve, signed by the active wallet \
                 key or, with --node-key, the node's own; prints what it changes and whether each \
                 service's base answers from the node. With --dry nothing changes"
    )]
    Set {
        file: PathBuf,
        #[arg(long)]
        dry: bool,
        #[arg(long)]
        node_key: bool,
    },
    #[command(
        about = "serve what this workspace's egress.toml names with its member key and no node: \
                 hear assigned requests from the validators' nodes over the ducktape/egress halo \
                 lane and answer through them; --node names the node whose HTTP gives the \
                 members and the lane table"
    )]
    Serve {
        #[arg(
            long,
            help = "the local UDP address halo binds; enroll registers the host:port that reaches it"
        )]
        halo: std::net::SocketAddr,
    },
}

#[derive(Subcommand)]
enum VoteVerb {
    #[command(about = "seat a resident as a validator")]
    Promote { key: String },
    #[command(about = "unseat a validator; it stays a resident")]
    Demote { key: String },
    #[command(about = "take a member out of the network")]
    Remove { key: String },
    #[command(about = "let anyone enroll without an invite")]
    Open,
    #[command(about = "make enrolling take an invite")]
    Close,
    #[command(
        about = "send the program bound to a role an op, a file or stdin: `programs` schedules or cancels a program change, `halo` opens, sets or closes a lane"
    )]
    Role { role: String, op: Option<PathBuf> },
    #[command(about = "answer unsigned queries too (open), or signed ones only (signed)")]
    Queries { policy: Queries },
}

#[cfg(unix)]
#[derive(Subcommand)]
enum BridgeVerb {
    #[command(
        about = "run Claude Code (claude -p, stream-json) for each chat thread the account answers \
                 in, its ducktape tools this bridge's own MCP; arguments after -- go to every \
                 claude it starts"
    )]
    Claude {
        #[arg(
            long,
            help = "the bridge's state file: the last change it settled, each thread's session \
                    and open input"
        )]
        state: PathBuf,
        #[arg(long, default_value = "claude", help = "the claude program to run")]
        claude: PathBuf,
        #[arg(
            long,
            help = "read the wallet password as one line on stdin, never from DUCKTAPE_PASSWORD, \
                    whose value a child can read in /proc of its parent"
        )]
        password_stdin: bool,
        #[arg(last = true)]
        args: Vec<String>,
    },
    #[command(
        about = "carry one MCP session between stdin and stdout and a running bridge's socket, \
                 for the product sessions the bridge starts"
    )]
    Relay { socket: PathBuf, place: String },
}

#[derive(Subcommand)]
enum MachineVerb {
    #[command(
        about = "print the machine key's public half, minting machine.key if absent; it is never \
                 the node's identity key"
    )]
    Init,
    #[command(
        about = "say each machine is ready to fleet, run what fleet assigns it, beat while it holds \
                 a running run, and report how each run ended once its log and outputs are in files"
    )]
    Run {
        #[arg(
            long,
            help = "a file holding [machine] [default: the workspace's node.toml]"
        )]
        config: Option<PathBuf>,
        #[arg(
            long,
            help = "run as the node's child, whose CPU set, nice and memory cap the node set already"
        )]
        child: bool,
    },
    #[command(
        about = "sign with machine.key its consent to joining the machine's account, and print it as \
                 the JSON identity's add_key op takes as its consent"
    )]
    Consent {
        #[arg(long, help = "the machine's account, which machine.key joins")]
        account: u64,
        #[arg(
            long,
            help = "the network it is for; refused unless the node runs it [default: the node's]"
        )]
        network: Option<String>,
        #[arg(long, help = EXPIRES)]
        expires: Option<u64>,
    },
}

#[derive(Subcommand)]
enum WalletVerb {
    #[command(about = "mint a key; prints its public key and its mnemonic")]
    New { name: String },
    #[command(about = "restore a key from its mnemonic")]
    Import { name: String, mnemonic: String },
    #[command(about = "list the keys; the active one is starred")]
    List,
    #[command(about = "make a key the one that signs")]
    Use { name: String },
    #[command(
        about = "sign with the active key a consent to a key joining an account, and print it as the \
                 JSON identity's add_key op takes as its consent"
    )]
    Consent {
        #[arg(long, help = "the account the key joins")]
        account: u64,
        #[arg(long, help = "the joining key in hex [default: the active key]")]
        key: Option<String>,
        #[arg(long, value_enum, default_value_t = KeyScheme::Ed25519, help = "the joining key's scheme")]
        scheme: KeyScheme,
        #[arg(
            long,
            help = "the network it is for; refused unless the node runs it [default: the node's]"
        )]
        network: Option<String>,
        #[arg(long, help = EXPIRES)]
        expires: Option<u64>,
    },
}

const EXPIRES: &str = "the unix time in milliseconds past which identity refuses the consent; \
                       without it the consent never expires, which gives a leaked one to nobody: \
                       the add_key that uses it still needs the signature of the account's manager \
                       (or of the joining key, on a user's account), and the consent is bound to \
                       the joining key's generation, the account and the network";

#[derive(Clone, Copy, clap::ValueEnum)]
enum KeyScheme {
    Ed25519,
    Secp256k1,
    Secp256r1,
}

impl From<KeyScheme> for Scheme {
    fn from(scheme: KeyScheme) -> Scheme {
        match scheme {
            KeyScheme::Ed25519 => Scheme::Ed25519,
            KeyScheme::Secp256k1 => Scheme::Secp256k1,
            KeyScheme::Secp256r1 => Scheme::Secp256r1,
        }
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Queries {
    Signed,
    Open,
}

impl From<Queries> for QueryPolicy {
    fn from(queries: Queries) -> QueryPolicy {
        match queries {
            Queries::Signed => QueryPolicy::Signed,
            Queries::Open => QueryPolicy::Open,
        }
    }
}

fn main() {
    let cli = Cli::parse();
    if let Err(error) = execute(cli) {
        eprintln!("ducktape: {error}");
        std::process::exit(1);
    }
}

fn execute(cli: Cli) -> Result<(), String> {
    if let Verb::Version = cli.verb {
        println!(
            "ducktape {} contract {}",
            env!("CARGO_PKG_VERSION"),
            noded::NODE_CONTRACT
        );
        return Ok(());
    }
    let workspace = match cli.workspace {
        Some(dir) => Workspace::at(dir),
        None => Workspace::at(ducktape_home::root()?),
    };
    match cli.verb {
        Verb::Identity => {
            let key = workspace
                .identity_or_create(rand_core::UnwrapErr(rand::rngs::SysRng))
                .sentence()?;
            println!("{}", hex::encode(key.public_key().as_ref()));
            Ok(())
        }
        Verb::Init { founding, halo } => {
            Runner::new(runtime_config(&workspace)).start(|context| async move {
                noded::init(context, &workspace, &founding, &halo)
                    .await
                    .sentence()
            })
        }
        Verb::Join {
            source,
            address,
            halo,
            invite,
        } => {
            let invite = invite.map(|hex| decode_invite(&hex)).transpose()?;
            let log = workspace.settings().sentence()?.log;
            let log = log.unwrap_or_else(|| noded::DEFAULT_LOG.to_string());
            Runner::new(runtime_config(&workspace)).start(|context| async move {
                Logs::install(&log)?;
                noded::join(
                    context,
                    &workspace,
                    Client::new(source),
                    address,
                    halo,
                    invite,
                )
                .await
                .sentence()
            })
        }
        Verb::Enroll {
            source,
            halo,
            invite,
        } => {
            let invite = invite.map(|hex| decode_invite(&hex)).transpose()?;
            Runner::new(runtime_config(&workspace)).start(|context| async move {
                noded::enroll(context, &workspace, Client::new(source), halo, invite)
                    .await
                    .sentence()
            })
        }
        Verb::Egress(EgressVerb::Serve { halo }) => {
            let client = client(&workspace, cli.node)?;
            let udp = std::net::UdpSocket::bind(halo).sentence()?;
            let log = workspace.settings().sentence()?.log;
            let log = log.unwrap_or_else(|| noded::DEFAULT_LOG.to_string());
            Runner::new(runtime_config(&workspace)).start(|context| async move {
                Logs::install(&log)?;
                noded::serve_box(&context, workspace, &client, udp)
                    .await
                    .sentence()
            })
        }
        Verb::Run(args) => {
            let mut config = NodeConfig::load(&workspace, args.settings()).sentence()?;
            if let Some(block) = &config.block_cpus {
                config.load.rpc_cpus = Some(noded::pin_block_threads(block).sentence()?);
            }
            let machine = config
                .machine
                .take()
                .filter(|machine| machine.on)
                .map(|machine| {
                    noded::machine::Limits::of(&machine, config.load.rpc_cpus.as_deref())
                });
            if let Some(limits) = &machine {
                noded::machine::supported(limits).sentence()?;
            }
            let runtime = runtime_config(&workspace).with_read_write_timeout(config.link_timeout());
            Runner::new(runtime).start(|context| async move {
                let logs = Logs::install(&config.log)?;
                let mut signalled = Box::pin(signalled().sentence()?);
                let starting = noded::run(
                    context,
                    &workspace,
                    logs,
                    config.listen,
                    config.halo,
                    config.load,
                    config.cadence,
                );
                let running = tokio::select! {
                    running = starting => running.sentence()?,
                    () = &mut signalled => {
                        stopped_starting();
                        return Ok(());
                    }
                };
                let keeping = match machine {
                    Some(limits) => Some(
                        noded::machine::keep(
                            &workspace,
                            running.http,
                            limits,
                            running.daemon.tips.subscribe(),
                            running.daemon.shutdown.subscribe(),
                        )
                        .sentence()?,
                    ),
                    None => None,
                };
                println!("http {}", running.http);
                let stopped = running.stopped(signalled).await;
                if let Some(keeping) = keeping {
                    let _ = keeping.await;
                }
                stopped.sentence()
            })
        }
        Verb::RestoreArchives => {
            Runner::new(runtime_config(&workspace)).start(|context| async move {
                let restored = noded::restore_archives(context, &workspace).await;
                let (blocks, summaries) = restored.sentence()?;
                println!("wrote back {blocks} blocks and {summaries} summaries");
                Ok(())
            })
        }
        Verb::Machine(MachineVerb::Init) => machine::init(&workspace),
        Verb::Machine(MachineVerb::Run { config, child }) => {
            let client = client(&workspace, cli.node)?;
            machine::run(&workspace, client, config, child)
        }
        Verb::Machine(MachineVerb::Consent {
            account,
            network,
            expires,
        }) => {
            let client = client(&workspace, cli.node)?;
            machine::consent(&workspace, client, account, network, expires)
        }
        #[cfg(unix)]
        Verb::Bridge(BridgeVerb::Relay { socket, place }) => {
            client_runtime()?.block_on(bridge::relay(socket, place))
        }
        Verb::Roll { height } => {
            let log = workspace.settings().sentence()?.log;
            let log = log.unwrap_or_else(|| noded::DEFAULT_LOG.to_string());
            Runner::new(runtime_config(&workspace)).start(|context| async move {
                Logs::install(&log)?;
                let rolled = noded::roll(context, &workspace, height).await.sentence()?;
                match rolled {
                    noded::Rolled::Already(stamp) => println!(
                        "already on contract {} from height {}",
                        stamp.contract, stamp.from
                    ),
                    noded::Rolled::Stamped(stamp) => println!(
                        "stamped contract {} from height {}",
                        stamp.contract, stamp.from
                    ),
                    noded::Rolled::Synced { stamp, aside } => println!(
                        "synced to height {} and stamped contract {} from height {}; the state \
                         from before the roll is in {}",
                        stamp.from - 1,
                        stamp.contract,
                        stamp.from,
                        aside.display()
                    ),
                }
                Ok(())
            })
        }
        Verb::Fork(ForkVerb::New { from }) => {
            let log = workspace.settings().sentence()?.log;
            let log = log.unwrap_or_else(|| noded::DEFAULT_LOG.to_string());
            Runner::new(runtime_config(&workspace)).start(|context| async move {
                Logs::install(&log)?;
                let source = Workspace::at(from);
                let forked = noded::fork(context, &source, &workspace).await.sentence()?;
                println!(
                    "forked {} at height {} as network {}; start it with `ducktape run`",
                    source.dir().display(),
                    forked.height,
                    forked.network
                );
                Ok(())
            })
        }
        Verb::Wallet(verb) if !matches!(verb, WalletVerb::Consent { .. }) => {
            wallet(&workspace, verb)
        }
        Verb::Blob(BlobVerb::Unpack) => {
            let store = blob::Blobs::open(&workspace.dir().join(host::BLOBS_DIR)).sentence()?;
            let unpacked = store.unpack().sentence()?;
            println!(
                "wrote back {} blobs; {} did not read and are left for the node to fetch again",
                unpacked.written, unpacked.unread
            );
            Ok(())
        }
        verb => {
            let client = client(&workspace, cli.node)?;
            client_runtime()?.block_on(talk(&workspace, &client, verb))
        }
    }
}

#[cfg(unix)]
fn signalled() -> std::io::Result<impl Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
    })
}

#[cfg(not(unix))]
fn signalled() -> std::io::Result<impl Future<Output = ()>> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
    })
}

fn stopped_starting() {
    tracing::info!(
        target: "ducktape::node",
        event = "node_stopped",
        reason = "signal",
        "the node stopped before it was up"
    );
}

fn runtime_config(workspace: &Workspace) -> Config {
    Config::new().with_storage_directory(workspace.runtime_dir())
}

fn client_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .sentence()
}

fn client(workspace: &Workspace, node: Option<String>) -> Result<Client, String> {
    let base = match node {
        Some(url) => url,
        None => format!("http://{}", workspace.http().sentence()?),
    };
    Ok(Client::new(base))
}

async fn talk(workspace: &Workspace, client: &Client, verb: Verb) -> Result<(), String> {
    match verb {
        Verb::Status => {
            let status = client.status().await.sentence()?;
            println!("network   {}", status.network);
            println!("height    {}", status.height);
            println!("tip       {}", hex::encode(status.tip));
            println!("root      {}", hex::encode(status.root.0));
            println!(
                "epoch     {} (length {})",
                status.epoch, status.epoch_length
            );
            println!(
                "time      {} (block {} ms)",
                status.time, status.block_time_ms
            );
            println!("identity  {}", hex::encode(&status.identity));
            println!("contract  {}", status.contract);
            let roles: Vec<String> = status
                .roles
                .iter()
                .map(|(role, program)| format!("{role} {program}"))
                .collect();
            println!("roles     {}", roles.join(", "));
            Ok(())
        }
        Verb::Submit {
            program,
            payload,
            preconfirm,
            land,
        } => {
            let status = client.status().await.sentence()?;
            let key = signer(workspace)?;
            let frame = Frame::sign_as(
                CLIENT,
                &key,
                status.network.as_bytes(),
                status.anchor(),
                Some(salt()),
                &program,
                bytes(payload)?,
            );
            if land {
                return submit_landed(client, &frame).await;
            }
            let preconfirm = match preconfirm {
                true => Preconfirm::On,
                false => Preconfirm::Off,
            };
            let id = frame.body.submission().id();
            let submitted = client.submit(frame.encode(), preconfirm).await.sentence()?;
            let receipt = match submitted {
                Submitted::Pooled => {
                    println!("pooled {}", hex::encode(id));
                    return Ok(());
                }
                Submitted::Preconfirmed(receipt) => receipt,
            };
            match receipt.outcome {
                Outcome::Applied { output } => {
                    println!("preconfirmed applied {}", hex::encode(output))
                }
                Outcome::Rejected(refusal) => {
                    println!(
                        "preconfirmed rejected {}: {}",
                        refusal.reason, refusal.sentence
                    )
                }
            }
            for event in receipt.events {
                println!("event {}", hex::encode(event));
            }
            Ok(())
        }
        Verb::Op {
            program,
            op,
            preconfirm,
            land,
        } => {
            let interface = json::interface(workspace, client, &program).await?;
            let payload = interface.encode_op(&json::parse(&op, "op")?).sentence()?;
            let status = client.status().await.sentence()?;
            let key = signer(workspace)?;
            let frame = Frame::sign_as(
                CLIENT,
                &key,
                status.network.as_bytes(),
                status.anchor(),
                Some(salt()),
                &program,
                payload.clone(),
            );
            let sent = json::Sent {
                interface: &interface,
                program: &program,
                payload: &payload,
            };
            let told = match (land, preconfirm) {
                (true, _) => sent.land(client, &frame).await?,
                (false, true) => sent.submit(client, &frame, Preconfirm::On).await?,
                (false, false) => sent.submit(client, &frame, Preconfirm::Off).await?,
            };
            json::print(&told);
            Ok(())
        }
        Verb::Query {
            program,
            query,
            raw,
            confirmed,
        } => {
            let Some(query) = query else {
                let request = bytes(raw.flatten())?;
                let status = client.status().await.sentence()?;
                let key = signer(workspace)?;
                let answer =
                    json::ask(client, &key, &status, &program, request, layer(confirmed)).await?;
                return emit(&answer);
            };
            let interface = json::interface(workspace, client, &program).await?;
            let request = interface
                .encode_query(&json::parse(&query, "query")?)
                .sentence()?;
            let status = client.status().await.sentence()?;
            let key = signer(workspace)?;
            let answer =
                json::ask(client, &key, &status, &program, request, layer(confirmed)).await?;
            json::print(&json::reply(&interface, &program, &answer)?);
            Ok(())
        }
        Verb::Fork(ForkVerb::As {
            signer,
            program,
            op,
            preconfirm,
            land,
        }) => {
            let interface = json::interface(workspace, client, &program).await?;
            let payload = interface.encode_op(&json::parse(&op, "op")?).sentence()?;
            let status = client.status().await.sentence()?;
            let anchor = status.anchor();
            let body = node::Body {
                scheme: keyscheme::KeyScheme::Ed25519,
                signer: hex::decode(&signer).sentence()?,
                network: status.network.into_bytes(),
                anchor,
                salt: Some(salt()),
                target: program.clone(),
                payload: payload.clone(),
                client: Some(CLIENT.to_owned()),
            };
            let sent = json::Sent {
                interface: &interface,
                program: &program,
                payload: &payload,
            };
            let told = match (land, preconfirm) {
                (true, _) => sent.land_played(client, &body).await?,
                (false, true) => sent.play(client, &body, Preconfirm::On).await?,
                (false, false) => sent.play(client, &body, Preconfirm::Off).await?,
            };
            json::print(&told);
            Ok(())
        }
        Verb::Fork(ForkVerb::Advance { height }) => {
            let reached = client.advance(height).await.sentence()?;
            println!("{reached}");
            Ok(())
        }
        Verb::Mcp => {
            let live = mcp::Live::seat(workspace, client, signer(workspace)?).await?;
            let input = tokio::io::BufReader::new(tokio::io::stdin());
            mcp::serve(&live, input, tokio::io::stdout())
                .await
                .sentence()
        }
        #[cfg(unix)]
        Verb::Bridge(BridgeVerb::Claude {
            state,
            claude,
            password_stdin,
            args,
        }) => {
            let key = bridge::key(workspace, password_stdin)?;
            let asked = bridge::Asked {
                state,
                claude,
                args,
            };
            bridge::claude(workspace, client, key, asked).await
        }
        #[cfg(unix)]
        Verb::Bridge(BridgeVerb::Relay { .. }) => {
            unreachable!("handled before the client is built")
        }
        Verb::Get {
            program,
            key,
            confirmed,
        } => {
            let key = hex::decode(key).sentence()?;
            match client
                .get(layer(confirmed), &program, &key)
                .await
                .sentence()?
            {
                Some(value) => emit(&value),
                None => Err("absent".into()),
            }
        }
        Verb::Scan {
            program,
            lo,
            hi,
            reverse,
            limit,
            confirmed,
        } => {
            let scan = Scan {
                lo: hex::decode(lo).sentence()?,
                hi: hi.map(hex::decode).transpose().sentence()?,
                reverse,
                limit,
            };
            for entry in client
                .scan(layer(confirmed), &program, scan)
                .await
                .sentence()?
            {
                println!("{}\t{}", hex::encode(entry.key), hex::encode(entry.value));
            }
            Ok(())
        }
        Verb::Blob(BlobVerb::Get { id }) => match client.blob(blob_id(&id)?).await.sentence()? {
            Some(framed) => emit(&framed),
            None => Err("absent".into()),
        },
        Verb::Blob(BlobVerb::Put { files, kind, wait }) => {
            put_blobs(workspace, client, files, &kind, wait).await
        }
        Verb::Blob(BlobVerb::Missing) => {
            for id in client.missing_blobs().await.sentence()? {
                println!("{}", blob_text(&id));
            }
            Ok(())
        }
        Verb::Changes { program } => {
            let mut changes = client.changes(&program).await.sentence()?;
            while let Some(change) = changes.next().await {
                let change = change.sentence()?;
                for (key, value) in change.writes {
                    let value = value.map(hex::encode).unwrap_or_else(|| "-".into());
                    println!("{}\t{}\t{value}", change.height, hex::encode(key));
                }
            }
            Ok(())
        }
        Verb::Programs => {
            for (program, code) in client.programs().await.sentence()? {
                println!("{program}\t{}", blob_text(&code));
            }
            Ok(())
        }
        Verb::Logs => {
            for line in client.logs().await.sentence()? {
                println!("{line}");
            }
            Ok(())
        }
        Verb::LogFilter { directives } => {
            let frame = admin(workspace, client, Admin::LogFilter(directives)).await?;
            client.admin(frame).await.sentence()
        }
        Verb::Shutdown => {
            let frame = admin(workspace, client, Admin::Shutdown).await?;
            client.admin(frame).await.sentence()
        }
        Verb::Egress(EgressVerb::Reload) => {
            let frame = admin(workspace, client, Admin::EgressReload).await?;
            client.admin(frame).await.sentence()
        }
        Verb::Egress(EgressVerb::Set {
            file,
            dry,
            node_key,
        }) => {
            let text = std::fs::read_to_string(&file).sentence()?;
            let key = match node_key {
                true => workspace.identity().sentence()?,
                false => signer(workspace)?,
            };
            let status = client.status().await.sentence()?;
            let frame = Frame::sign_as(
                CLIENT,
                &key,
                status.network.as_bytes(),
                status.anchor(),
                None,
                noded::wire::EGRESS,
                abi::encode(&EgressSet { text, dry }),
            );
            let plan = client.egress_set(frame.encode()).await.sentence()?;
            json::print(&json::egress(&plan));
            Ok(())
        }
        Verb::Metrics => {
            print!("{}", client.metrics().await.sentence()?);
            Ok(())
        }
        Verb::Invite { hours } => {
            let status = client.status().await.sentence()?;
            let identity = workspace.identity().sentence()?;
            let grant = Grant {
                network: status.network.into_bytes(),
                nonce: nonce(),
                expires: now_ms()? + hours * 3_600_000,
            };
            println!(
                "{}",
                hex::encode(abi::encode(&noded::invite(&identity, grant)))
            );
            Ok(())
        }
        Verb::Vote(verb) => {
            let motion = motion(verb)?;
            let (height, output) = membership(workspace, client, Op::Vote(motion)).await?;
            match abi::decode(&output).map_err(|refusal| refusal.sentence)? {
                Voted::Counted { votes, needed } => {
                    println!("counted {votes} of {needed} at block {height}")
                }
                Voted::Enacted => println!("enacted at block {height}"),
            }
            Ok(())
        }
        Verb::Leave => {
            let (height, _) = membership(workspace, client, Op::Leave).await?;
            println!("left at block {height}");
            Ok(())
        }
        Verb::Halo { halo } => {
            let (height, _) = membership(workspace, client, Op::SetHalo { halo }).await?;
            println!("halo address registered at block {height}");
            Ok(())
        }
        Verb::Wallet(WalletVerb::Consent {
            account,
            key,
            scheme,
            network,
            expires,
        }) => {
            let key = key.map(hex::decode).transpose().sentence()?;
            let signer = signer(workspace)?;
            let asked = json::Consenting {
                account,
                key,
                scheme: scheme.into(),
                network,
                expires,
            };
            json::print(&json::consent(client, &signer, asked).await?);
            Ok(())
        }
        Verb::Forward { member, service } => {
            let status = client.status().await.sentence()?;
            let key = signer(workspace)?;
            let member = hex::decode(member).sentence()?;
            let frame = Frame::sign_forward(
                &key,
                status.network.as_bytes(),
                status.anchor(),
                &service,
                member,
            );
            client
                .forward(&frame, tokio::io::stdin(), tokio::io::stdout())
                .await
                .sentence()
        }
        Verb::Version
        | Verb::Identity
        | Verb::Init { .. }
        | Verb::Join { .. }
        | Verb::Enroll { .. }
        | Verb::Egress(EgressVerb::Serve { .. })
        | Verb::Run(_)
        | Verb::RestoreArchives
        | Verb::Blob(BlobVerb::Unpack)
        | Verb::Roll { .. }
        | Verb::Fork(ForkVerb::New { .. })
        | Verb::Wallet(_)
        | Verb::Machine(_) => unreachable!("handled before the client is built"),
    }
}

async fn put_blobs(
    workspace: &Workspace,
    client: &Client,
    files: Vec<PathBuf>,
    kind: &str,
    wait: bool,
) -> Result<(), String> {
    let mut entries = Vec::with_capacity(files.len());
    for file in &files {
        entries.push(blob::file_entry(kind, file).map_err(|refusal| refusal.sentence)?);
    }
    let status = client.status().await.sentence()?;
    let role = status
        .roles
        .bound(role::BLOBS)
        .map_err(|refusal| refusal.sentence)?;
    let key = signer(workspace)?;
    let put = blob::contract::Op::Put {
        blobs: entries.clone(),
    };
    let frame = Frame::sign_as(
        CLIENT,
        &key,
        status.network.as_bytes(),
        status.anchor(),
        Some(salt()),
        role,
        abi::encode(&put),
    );
    let uploads = files
        .into_iter()
        .map(|path| Upload {
            kind: kind.to_owned(),
            path,
        })
        .collect();
    let ids = || {
        for entry in &entries {
            println!("{}", blob_text(&entry.id));
        }
    };
    if !wait {
        client.put_blobs(&frame, uploads).await.sentence()?;
        ids();
        return Ok(());
    }
    let (height, receipt) = client.land_blobs(&frame, uploads).await.sentence()?;
    ids();
    let output = match receipt.outcome {
        Outcome::Applied { output } => output,
        Outcome::Rejected(refusal) => {
            return Err(format!("{}: {}", refusal.reason, refusal.sentence));
        }
    };
    let bundle: u64 = abi::decode(&output).map_err(|refusal| refusal.sentence)?;
    println!("bundle {bundle} put at block {height}");
    let since = client.held(role, bundle).await.sentence()?;
    println!("bundle {bundle} held since block {since}");
    Ok(())
}

async fn submit_landed(client: &Client, frame: &Frame) -> Result<(), String> {
    let blocks = client.changes(RECENT).await.sentence()?;
    client
        .submit(frame.encode(), Preconfirm::Off)
        .await
        .sentence()?;
    println!("pooled {}", hex::encode(frame.body.submission().id()));
    let (height, receipt) = match client.landed(frame, blocks).await {
        Ok(landed) => landed,
        Err(noded::Error::Unlanded) => {
            println!("unlanded");
            return Ok(());
        }
        Err(error) => return Err(error.to_string()),
    };
    match receipt.outcome {
        Outcome::Applied { output } => println!("landed {height} applied {}", hex::encode(output)),
        Outcome::Rejected(refusal) => println!(
            "landed {height} rejected {}: {}",
            refusal.reason, refusal.sentence
        ),
    }
    for event in receipt.events {
        println!("event {}", hex::encode(event));
    }
    Ok(())
}

async fn membership(
    workspace: &Workspace,
    client: &Client,
    op: Op,
) -> Result<(u64, Vec<u8>), String> {
    let status = client.status().await.sentence()?;
    let identity = workspace.identity().sentence()?;
    let role = status
        .roles
        .bound(role::VALIDATORS)
        .map_err(|refusal| refusal.sentence)?;
    let frame = Frame::sign_as(
        CLIENT,
        &identity,
        status.network.as_bytes(),
        status.anchor(),
        Some(salt()),
        role,
        abi::encode(&op),
    );
    let (height, receipt) = client.land(&frame).await.sentence()?;
    match receipt.outcome {
        Outcome::Applied { output } => Ok((height, output)),
        Outcome::Rejected(refusal) => Err(format!(
            "refused at block {height}: {}: {}",
            refusal.reason, refusal.sentence
        )),
    }
}

fn motion(verb: VoteVerb) -> Result<Motion, String> {
    let key = |hex: String| hex::decode(hex).sentence();
    Ok(match verb {
        VoteVerb::Promote { key: member } => Motion::Promote { key: key(member)? },
        VoteVerb::Demote { key: member } => Motion::Demote { key: key(member)? },
        VoteVerb::Remove { key: member } => Motion::Remove { key: key(member)? },
        VoteVerb::Open => Motion::Door { open: true },
        VoteVerb::Close => Motion::Door { open: false },
        VoteVerb::Role { role, op } => Motion::Role {
            role,
            op: bytes(op)?,
        },
        VoteVerb::Queries { policy } => Motion::Queries {
            policy: policy.into(),
        },
    })
}

fn registered_halo(text: &str) -> Result<String, String> {
    abi::role::validators::registered_halo(text).map_err(|refusal| refusal.sentence)?;
    Ok(text.to_owned())
}

fn permits(text: &str) -> Result<NonZeroUsize, String> {
    let count: NonZeroUsize = text.parse().sentence()?;
    noded::permits(count).sentence()
}

fn decode_invite(text: &str) -> Result<Invite, String> {
    let bytes = hex::decode(text).sentence()?;
    abi::decode(&bytes).map_err(|refusal| refusal.sentence)
}

fn nonce() -> [u8; 16] {
    let mut nonce = [0; 16];
    rand_core::Rng::fill_bytes(&mut rand_core::UnwrapErr(rand::rngs::SysRng), &mut nonce);
    nonce
}

fn now_ms() -> Result<u64, String> {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .sentence()?;
    Ok(since.as_millis() as u64)
}

fn salt() -> u64 {
    rand_core::Rng::next_u64(&mut rand_core::UnwrapErr(rand::rngs::SysRng))
}

async fn admin(workspace: &Workspace, client: &Client, verb: Admin) -> Result<Vec<u8>, String> {
    let status = client.status().await.sentence()?;
    let identity = workspace.identity().sentence()?;
    Ok(Frame::sign_as(
        CLIENT,
        &identity,
        status.network.as_bytes(),
        status.anchor(),
        None,
        noded::wire::ADMIN,
        abi::encode(&verb),
    )
    .encode())
}

fn wallet(workspace: &Workspace, verb: WalletVerb) -> Result<(), String> {
    match verb {
        WalletVerb::New { name } => {
            let (words, pubkey, activated) =
                keystore::wallet::create(workspace.dir(), &name, &password()?)?;
            println!("{pubkey}");
            println!("{words}");
            if !activated {
                println!("not active: run `ducktape wallet use {name}`");
            }
            Ok(())
        }
        WalletVerb::Import { name, mnemonic } => {
            let pubkey = keystore::wallet::import(workspace.dir(), &name, &mnemonic, &password()?)?;
            println!("{pubkey}");
            Ok(())
        }
        WalletVerb::List => {
            for row in keystore::wallet::list(workspace.dir())? {
                let mark = if row.active { "*" } else { " " };
                println!("{mark} {}\t{}\t{}", row.name, row.pubkey, row.state);
            }
            Ok(())
        }
        WalletVerb::Use { name } => keystore::wallet::activate(workspace.dir(), &name),
        WalletVerb::Consent { .. } => unreachable!("a consent asks the node, so talk signs it"),
    }
}

fn signer(workspace: &Workspace) -> Result<ed25519::PrivateKey, String> {
    let path = keystore::wallet::active_user_key(workspace.dir())?;
    keystore::userkey::open_user_key_at(&path, &password()?)
}

fn password() -> Result<String, String> {
    if let Ok(password) = std::env::var("DUCKTAPE_PASSWORD") {
        return Ok(password);
    }
    dialoguer::Password::new()
        .with_prompt("wallet password")
        .interact()
        .map_err(|error| error.to_string())
}

fn layer(confirmed: bool) -> Layer {
    if confirmed {
        Layer::Confirmed
    } else {
        Layer::Preconfirmed
    }
}

fn bytes(path: Option<PathBuf>) -> Result<Vec<u8>, String> {
    match path {
        Some(path) => std::fs::read(path).sentence(),
        None => {
            let mut bytes = Vec::new();
            std::io::stdin().read_to_end(&mut bytes).sentence()?;
            Ok(bytes)
        }
    }
}

fn emit(bytes: &[u8]) -> Result<(), String> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes).sentence()?;
    stdout.flush().sentence()
}

fn blob_text(id: &BlobId) -> String {
    let kind = match id.kind() {
        HashKind::Sha256 => "sha256",
        HashKind::Sha1 => "sha1",
    };
    format!("{kind}:{}", hex::encode(id.digest()))
}

fn blob_id(text: &str) -> Result<BlobId, String> {
    let (kind, digest) = text
        .split_once(':')
        .ok_or("a blob id is sha256:<hex> or sha1:<hex>")?;
    let digest = hex::decode(digest).sentence()?;
    match kind {
        "sha256" => Ok(BlobId::Sha256(
            digest
                .try_into()
                .map_err(|_| "a sha256 digest is 32 bytes")?,
        )),
        "sha1" => Ok(BlobId::Sha1(
            digest.try_into().map_err(|_| "a sha1 digest is 20 bytes")?,
        )),
        other => Err(format!("unknown hash kind {other}")),
    }
}

trait Sentence<T> {
    fn sentence(self) -> Result<T, String>;
}

impl<T, E: std::fmt::Display> Sentence<T> for Result<T, E> {
    fn sentence(self) -> Result<T, String> {
        self.map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use std::num::NonZeroUsize;

    use std::num::NonZeroU64;

    use super::{
        Cli, ConsensusSettings, CpuList, LoadSettings, Scheme, Settings, Verb, WalletVerb,
    };

    #[test]
    fn a_registered_halo_address_refuses_a_bind_only_host_and_run_binds_one() {
        let verbs: [&[&str]; 3] = [
            &["init", "founding.toml", "--halo"],
            &["join", "https://a.example.org", "--halo"],
            &["halo"],
        ];
        let parsed = |verb: &[&str], halo: &str| {
            let args: Vec<&str> = ["ducktape"]
                .into_iter()
                .chain(verb.iter().copied())
                .chain([halo])
                .collect();
            Cli::try_parse_from(args)
        };
        for verb in verbs {
            for bind_only in [
                "0.0.0.0:9100",
                "[::]:9100",
                "127.0.0.1:9100",
                "localhost:9100",
            ] {
                let refused = parsed(verb, bind_only).err().unwrap().to_string();
                assert!(
                    refused.contains("halo: "),
                    "{verb:?} {bind_only}: {refused}"
                );
            }
            assert!(parsed(verb, "a.example.org:9100").is_ok(), "{verb:?}");
        }
        let run = Cli::try_parse_from([
            "ducktape",
            "run",
            "--listen",
            "127.0.0.1:9000",
            "--halo",
            "0.0.0.0:9100",
        ])
        .unwrap();
        let Verb::Run(args) = run.verb else {
            panic!("run parses");
        };
        assert_eq!(args.halo, Some("0.0.0.0:9100".parse().unwrap()));
    }

    #[test]
    fn a_query_is_json_unless_it_says_raw_and_then_its_bytes_come_from_a_file_or_stdin() {
        let query = |args: &[&str]| {
            let all = ["ducktape", "query", "files"].into_iter();
            let parsed = Cli::try_parse_from(all.chain(args.iter().copied()))?;
            let Verb::Query { query, raw, .. } = parsed.verb else {
                panic!("not a query");
            };
            Ok::<_, clap::Error>((query, raw))
        };
        assert_eq!(
            query(&["\"Head\""]).unwrap(),
            (Some("\"Head\"".into()), None)
        );
        assert_eq!(query(&["--raw"]).unwrap(), (None, Some(None)));
        assert_eq!(
            query(&["--raw", "q.bin"]).unwrap(),
            (None, Some(Some("q.bin".into())))
        );
        assert!(query(&[]).is_err());
        assert!(query(&["\"Head\"", "--raw"]).is_err());
    }

    #[test]
    fn a_consent_never_expires_and_is_for_the_active_key_unless_told() {
        let parsed = Cli::try_parse_from(["ducktape", "wallet", "consent", "--account", "18"]);
        let Verb::Wallet(WalletVerb::Consent {
            account,
            key,
            scheme,
            network,
            expires,
        }) = parsed.unwrap().verb
        else {
            panic!("not a consent");
        };
        assert_eq!((account, key, network, expires), (18, None, None, None));
        assert_eq!(Scheme::from(scheme), Scheme::Ed25519);
        let passkey = [
            "ducktape",
            "wallet",
            "consent",
            "--account",
            "18",
            "--scheme",
            "secp256r1",
        ];
        assert!(Cli::try_parse_from(passkey).is_ok());
    }

    fn run(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(["ducktape", "run"].into_iter().chain(args.iter().copied()))
    }

    fn run_settings(args: &[&str]) -> Settings {
        let Verb::Run(run) = run(args).unwrap().verb else {
            panic!("not a run");
        };
        run.settings()
    }

    fn count(count: usize) -> NonZeroUsize {
        NonZeroUsize::new(count).unwrap()
    }

    #[test]
    fn a_count_flag_refuses_zero_and_past_a_tokio_semaphore() {
        let past_tokio = (tokio::sync::Semaphore::MAX_PERMITS + 1).to_string();
        for flag in [
            "--halo-inbound-streams",
            "--halo-datagrams",
            "--blob-serves-in-flight",
            "--blob-backlog",
            "--live-session-inbound",
            "--live-session-outbound",
            "--live-group-queue",
            "--live-peer-queue",
            "--live-lane-backlog",
            "--live-lane-per-sender",
            "--egress-lane-backlog",
        ] {
            assert!(run(&[flag, "0"]).is_err(), "{flag}");
            assert!(run(&[flag, &past_tokio]).is_err(), "{flag}");
            assert!(run(&[flag, "1"]).is_ok(), "{flag}");
        }
        assert!(run(&["--blob-gatherers", "0"]).is_err());
        assert!(
            run(&["--live-linger-ms", "0"]).is_err(),
            "a live clock runs"
        );
    }

    #[test]
    fn a_flag_not_given_leaves_its_setting_to_the_file() {
        assert_eq!(run_settings(&[]), Settings::default());
    }

    #[test]
    fn every_run_flag_reaches_its_setting() {
        let settings = run_settings(&[
            "--listen",
            "127.0.0.1:9000",
            "--http",
            "127.0.0.1:8844",
            "--tls",
            "127.0.0.1:9443",
            "--halo",
            "0.0.0.0:9100",
            "--live",
            "0.0.0.0:9200",
            "--log",
            "debug",
            "--halo-egress",
            "1000000",
            "--halo-inbound-streams",
            "32",
            "--halo-datagrams",
            "128",
            "--blob-serves-in-flight",
            "3",
            "--blob-backlog",
            "40",
            "--blob-gatherers",
            "5",
            "--live-session-inbound",
            "64",
            "--live-session-outbound",
            "256",
            "--live-group-queue",
            "8",
            "--live-peer-queue",
            "32",
            "--live-lane-backlog",
            "4",
            "--live-lane-per-sender",
            "16",
            "--live-linger-ms",
            "700",
            "--egress-lane-backlog",
            "2",
            "--query-fuel",
            "5000000",
            "--leader-timeout-ms",
            "4000",
            "--certification-timeout-ms",
            "6000",
            "--timeout-retry-ms",
            "2000",
            "--block-cpus",
            "0-5",
        ]);
        assert_eq!(
            settings,
            Settings {
                listen: Some("127.0.0.1:9000".parse().unwrap()),
                http: Some("127.0.0.1:8844".parse().unwrap()),
                tls: Some("127.0.0.1:9443".parse().unwrap()),
                halo: Some("0.0.0.0:9100".parse().unwrap()),
                live: Some("0.0.0.0:9200".parse().unwrap()),
                log: Some("debug".into()),
                block_cpus: Some(CpuList::parse("0-5").unwrap()),
                load: LoadSettings {
                    halo_egress: Some(1_000_000),
                    halo_inbound_streams: Some(count(32)),
                    halo_datagrams: Some(count(128)),
                    blob_serves_in_flight: Some(count(3)),
                    blob_backlog: Some(count(40)),
                    blob_gatherers: Some(count(5)),
                    live_session_inbound: Some(count(64)),
                    live_session_outbound: Some(count(256)),
                    live_group_queue: Some(count(8)),
                    live_peer_queue: Some(count(32)),
                    live_lane_backlog: Some(count(4)),
                    live_lane_per_sender: Some(count(16)),
                    live_linger_ms: NonZeroU64::new(700),
                    egress_lane_backlog: Some(count(2)),
                    forward_lane_backlog: None,
                    query_fuel: NonZeroU64::new(5_000_000),
                },
                consensus: ConsensusSettings {
                    leader_timeout_ms: Some(4000),
                    certification_timeout_ms: Some(6000),
                    timeout_retry_ms: Some(2000),
                },
                machine: None,
            }
        );
    }
}
