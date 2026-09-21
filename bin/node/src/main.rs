use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
use std::path::PathBuf;

use abi::{BlobId, HashKind, Outcome, Scan};
use clap::{Parser, Subcommand};
use commonware_cryptography::{Signer as _, ed25519};
use commonware_runtime::Runner as _;
use commonware_runtime::tokio::{Config, Runner};
use futures::StreamExt as _;
use host::{Layer, SIGNERS};
use node::Frame;
use noded::wire::Admin;
use noded::{Client, Listen, Logs, Reach, Workspace};

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
    Init { founding: PathBuf },
    #[command(about = "join a network: sync its state from a running node and enroll as a member")]
    Join {
        source: String,
        #[arg(long, help = "the address peers dial this node at")]
        address: SocketAddr,
    },
    #[command(about = "run the node")]
    Run {
        #[arg(long, help = "the address peers dial")]
        listen: SocketAddr,
        #[arg(long, default_value = "127.0.0.1:0")]
        http: SocketAddr,
        #[arg(long, help = "peers on private addresses are dialed")]
        private: bool,
        #[arg(long, default_value = "info")]
        log: String,
    },
    #[command(about = "print the node's height, tip, root, epoch and identity")]
    Status,
    #[command(about = "submit a signed frame; the payload is a file or stdin")]
    Submit {
        program: String,
        payload: Option<PathBuf>,
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
    #[command(about = "print the node's metrics")]
    Metrics,
    #[command(subcommand, about = "the user keys that sign frames")]
    Wallet(WalletVerb),
}

#[derive(Subcommand)]
enum BlobVerb {
    #[command(about = "fetch a blob's framed bytes to stdout")]
    Get { id: String },
    #[command(about = "frame a file and install it on the node")]
    Put {
        file: PathBuf,
        #[arg(long, default_value = "blob")]
        kind: String,
        #[arg(long)]
        sha1: bool,
    },
    #[command(about = "list the blob ids the state names and the node lacks")]
    Missing,
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
        Verb::Init { founding } => node_runtime(&workspace).start(|context| async move {
            noded::init(context, &workspace, &founding).await.sentence()
        }),
        Verb::Join { source, address } => node_runtime(&workspace).start(|context| async move {
            noded::join(context, &workspace, Client::new(source), address)
                .await
                .sentence()
        }),
        Verb::Run {
            listen,
            http,
            private,
            log,
        } => node_runtime(&workspace).start(|context| async move {
            let logs = Logs::install(&log)?;
            let reach = if private {
                Reach::Private
            } else {
                Reach::Public
            };
            let listen = Listen {
                p2p: listen,
                http,
                reach,
            };
            let running = noded::run(context, &workspace, logs, listen)
                .await
                .sentence()?;
            println!("http {}", running.http);
            running.stopped().await;
            Ok(())
        }),
        Verb::Wallet(verb) => wallet(&workspace, verb),
        verb => {
            let client = client(&workspace, cli.node)?;
            client_runtime()?.block_on(talk(&workspace, &client, verb))
        }
    }
}

fn node_runtime(workspace: &Workspace) -> Runner {
    Runner::new(Config::new().with_storage_directory(workspace.runtime_dir()))
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
            Ok(())
        }
        Verb::Submit { program, payload } => {
            let status = client.status().await.sentence()?;
            let key = signer(workspace)?;
            let signer = key.public_key().as_ref().to_vec();
            let seq = client
                .get(Layer::Preconfirmed, SIGNERS, &signer)
                .await
                .sentence()?
                .map(|bytes| abi::decode::<u64>(&bytes))
                .transpose()
                .map_err(|refusal| refusal.sentence)?
                .unwrap_or(0);
            let frame = Frame::sign(
                &key,
                status.network.as_bytes(),
                seq,
                &program,
                bytes(payload)?,
            );
            let receipt = client.submit(frame.encode()).await.sentence()?;
            match receipt.outcome {
                Outcome::Applied { output } => println!("applied {}", hex::encode(output)),
                Outcome::Rejected(refusal) => {
                    println!("rejected {}: {}", refusal.reason, refusal.sentence)
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
                0,
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
        Verb::Blob(BlobVerb::Put { file, kind, sha1 }) => {
            let body = std::fs::read(file).sentence()?;
            let framed = blobs::frame(&kind, &body).map_err(|refusal| refusal.sentence)?;
            let hash = if sha1 {
                HashKind::Sha1
            } else {
                HashKind::Sha256
            };
            let id = blobs::id_of(hash, &framed);
            client.put_blob(id, framed).await.sentence()?;
            println!("{}", blob_text(&id));
            Ok(())
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
        Verb::Metrics => {
            print!("{}", client.metrics().await.sentence()?);
            Ok(())
        }
        Verb::Version
        | Verb::Identity
        | Verb::Init { .. }
        | Verb::Join { .. }
        | Verb::Run { .. }
        | Verb::Wallet(_) => unreachable!("handled before the client is built"),
    }
}

async fn admin(workspace: &Workspace, client: &Client, verb: Admin) -> Result<Vec<u8>, String> {
    let status = client.status().await.sentence()?;
    let identity = workspace.identity().sentence()?;
    Ok(Frame::sign(
        &identity,
        status.network.as_bytes(),
        0,
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
