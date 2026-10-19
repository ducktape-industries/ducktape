use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;

use abi::role::validators::{Grant, Invite, Motion, Op, Voted};
use abi::{BlobId, HashKind, Outcome, Scan, role};
use clap::{Args, Parser, Subcommand};
use commonware_cryptography::{Signer as _, ed25519};
use commonware_runtime::Runner as _;
use commonware_runtime::tokio::{Config, Runner};
use futures::StreamExt as _;
use host::Layer;
use node::{Frame, Preconfirm};
use noded::wire::{Admin, Submitted};
use noded::{
    Client, ConsensusSettings, Endpoint, HaloConfig, LoadConfig, LoadSettings, Logs, NodeConfig,
    Settings, Upload, Workspace,
};

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
    #[command(about = "run the node; a flag overrides the workspace's node.toml")]
    Run(Box<RunArgs>),
    #[command(about = "print the node's height, tip, root, epoch and identity")]
    Status,
    #[command(
        about = "submit a signed frame to the pool; the payload is a file or stdin. With \
                 --preconfirm the node also runs it now and prints its predicted receipt"
    )]
    Submit {
        program: String,
        payload: Option<PathBuf>,
        #[arg(long)]
        preconfirm: bool,
    },
    #[command(about = "query a program; the request is a file or stdin, the answer is stdout")]
    Query {
        program: String,
        request: Option<PathBuf>,
        #[arg(long)]
        confirmed: bool,
    },
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
            "the HTTP reads this node serves at once; the rest wait their turn",
            "load.reads_in_flight",
            LoadConfig::default().reads_in_flight
        )
    )]
    reads_in_flight: Option<NonZeroUsize>,
    #[arg(
        long,
        value_parser = permits,
        help = defaulted(
            "the uploads (submit, blob put) this node takes at once; the rest wait their turn",
            "load.uploads_in_flight",
            LoadConfig::default().uploads_in_flight
        )
    )]
    uploads_in_flight: Option<NonZeroUsize>,
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
            load: LoadSettings {
                halo_egress: self.halo_egress,
                halo_inbound_streams: self.halo_inbound_streams,
                halo_datagrams: self.halo_datagrams,
                reads_in_flight: self.reads_in_flight,
                uploads_in_flight: self.uploads_in_flight,
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
            },
            consensus: ConsensusSettings {
                leader_timeout_ms: self.leader_timeout_ms,
                certification_timeout_ms: self.certification_timeout_ms,
                timeout_retry_ms: self.timeout_retry_ms,
            },
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
}

#[derive(Subcommand)]
enum EgressVerb {
    #[command(
        about = "make the running node re-read egress.toml and serve what it now names; \
                 an invalid file leaves what it serves unchanged"
    )]
    Reload,
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
            Runner::new(runtime_config(&workspace)).start(|context| async move {
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
        Verb::Run(args) => {
            let config = NodeConfig::load(&workspace, args.settings()).sentence()?;
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
                println!("http {}", running.http);
                running.stopped(signalled).await.sentence()
            })
        }
        Verb::Wallet(verb) => wallet(&workspace, verb),
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
        } => {
            let status = client.status().await.sentence()?;
            let key = signer(workspace)?;
            let frame = Frame::sign(
                &key,
                status.network.as_bytes(),
                status.anchor(),
                Some(salt()),
                &program,
                bytes(payload)?,
            );
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
        Verb::Query {
            program,
            request,
            confirmed,
        } => {
            let status = client.status().await.sentence()?;
            let key = signer(workspace)?;
            let frame = Frame::sign(
                &key,
                status.network.as_bytes(),
                status.anchor(),
                None,
                &program,
                bytes(request)?,
            );
            let answer = client
                .query(layer(confirmed), frame.encode())
                .await
                .sentence()?;
            emit(&answer)
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
        Verb::Version
        | Verb::Identity
        | Verb::Init { .. }
        | Verb::Join { .. }
        | Verb::Run(_)
        | Verb::Wallet(_) => unreachable!("handled before the client is built"),
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
    let frame = Frame::sign(
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
    let frame = Frame::sign(
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
    Ok(Frame::sign(
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

    use super::{Cli, ConsensusSettings, LoadSettings, Settings, Verb};

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
            "--reads-in-flight",
            "--uploads-in-flight",
            "--blob-serves-in-flight",
            "--blob-backlog",
            "--live-session-inbound",
            "--live-session-outbound",
            "--live-group-queue",
            "--live-peer-queue",
            "--live-lane-backlog",
            "--live-lane-per-sender",
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
            "--reads-in-flight",
            "16",
            "--uploads-in-flight",
            "2",
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
            "--leader-timeout-ms",
            "4000",
            "--certification-timeout-ms",
            "6000",
            "--timeout-retry-ms",
            "2000",
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
                load: LoadSettings {
                    halo_egress: Some(1_000_000),
                    halo_inbound_streams: Some(count(32)),
                    halo_datagrams: Some(count(128)),
                    reads_in_flight: Some(count(16)),
                    uploads_in_flight: Some(count(2)),
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
                },
                consensus: ConsensusSettings {
                    leader_timeout_ms: Some(4000),
                    certification_timeout_ms: Some(6000),
                    timeout_retry_ms: Some(2000),
                },
            }
        );
    }
}
