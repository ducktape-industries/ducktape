//! `ducktape fs` — the duckfs working-copy CLI.
//!
//! a clap-derive verb tree (the workspace adopted clap 4.6; the old "no clap"
//! prohibition is dead) over the `duckfs-client` engine and its `HttpNode`
//! transport. it ships the read verbs and the working-copy loop
//! (checkout/status/commit/pin). the FUSE `mount` verb was removed in the
//! 2026-07-13 storage-plane revision (see the duckfs spec's Mount surface
//! section; the implementation lives in git history at e7b4e1d1 if genuine
//! mount demand ever returns).
//!
//! exit codes are part of the contract: 0 success, 1 an operational failure (and
//! a dirty `status`), 2 a usage error (clap owns these, at the top-level parse)
//! and a commit conflict.

mod args;
mod read_cmds;
mod work_cmds;

use self::args::{NodeAddr, nfc_path};

/// the `ducktape fs` verb tree. usage errors (a missing positional, a bad
/// numeric flag, an unknown verb) are clap's job at the top-level parse — it
/// exits 2, matching the contract — so the handlers below carry `CliError` only
/// for operational failures (exit 1) and the commit conflict (exit 2).
#[derive(Debug, clap::Subcommand)]
pub(crate) enum FsCmd {
    /// list a directory
    Ls(LsArgs),
    /// stream a file to stdout
    Cat(CatArgs),
    /// print an entry's facts
    Stat(StatArgs),
    /// the commit window, newest-first
    History(HistoryArgs),
    /// changed leaves between two snapshots
    Diff(DiffArgs),
    /// materialize a subtree + .duckfs index
    Checkout(CheckoutArgs),
    /// show A/M/D (exit 1 when dirty)
    Status(StatusArgs),
    /// commit the working copy (exit 2 on conflict)
    Commit(CommitArgs),
    /// write one local file to a duckfs path in one commit
    Put(PutArgs),
    /// pin a snapshot so gc keeps it
    Pin(PinArgs),
    /// release a pin (the owner or `system` only)
    Unpin(UnpinArgs),
}

#[derive(Debug, clap::Args)]
pub(crate) struct LsArgs {
    /// the directory path to list
    #[arg(value_parser = nfc_path)]
    pub path: String,
    /// read at this snapshot instead of the head
    #[arg(long, value_name = "SNAPSHOT")]
    pub snapshot: Option<String>,
    /// per-request page size
    #[arg(long, value_name = "N")]
    pub limit: Option<u64>,
    /// emit one JSON array line instead of tab-separated rows
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub addr: NodeAddr,
}

#[derive(Debug, clap::Args)]
pub(crate) struct CatArgs {
    /// the file path to stream
    #[arg(value_parser = nfc_path)]
    pub path: String,
    /// read at this snapshot instead of the head
    #[arg(long, value_name = "SNAPSHOT")]
    pub snapshot: Option<String>,
    #[command(flatten)]
    pub addr: NodeAddr,
}

#[derive(Debug, clap::Args)]
pub(crate) struct StatArgs {
    /// the entry path to describe
    #[arg(value_parser = nfc_path)]
    pub path: String,
    /// read at this snapshot instead of the head
    #[arg(long, value_name = "SNAPSHOT")]
    pub snapshot: Option<String>,
    /// emit one JSON object line instead of key/value rows
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub addr: NodeAddr,
}

#[derive(Debug, clap::Args)]
pub(crate) struct HistoryArgs {
    /// how many commits to return
    #[arg(long, value_name = "N")]
    pub limit: Option<u64>,
    /// emit one JSON array line instead of tab-separated rows
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub addr: NodeAddr,
}

#[derive(Debug, clap::Args)]
pub(crate) struct DiffArgs {
    /// the base snapshot
    pub from: String,
    /// the target snapshot
    pub to: String,
    /// restrict the diff to leaves under this path prefix
    #[arg(long, value_name = "PREFIX", value_parser = nfc_path)]
    pub prefix: Option<String>,
    /// emit one JSON array line instead of tab-separated rows
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub addr: NodeAddr,
}

#[derive(Debug, clap::Args)]
pub(crate) struct CheckoutArgs {
    /// the subtree prefix to materialize
    #[arg(value_parser = nfc_path)]
    pub prefix: String,
    /// the directory to materialize into
    pub dir: String,
    /// check out this snapshot instead of the head
    #[arg(long, value_name = "SNAPSHOT")]
    pub snapshot: Option<String>,
    #[command(flatten)]
    pub addr: NodeAddr,
}

#[derive(Debug, clap::Args)]
pub(crate) struct StatusArgs {
    /// the checkout directory (default: the current directory)
    pub dir: Option<String>,
    /// report only changes at or under this path (repeatable). a path is
    /// relative to the checkout, or an absolute duckfs path
    #[arg(long = "path", value_name = "PATH", value_parser = nfc_path)]
    pub paths: Vec<String>,
}

#[derive(Debug, clap::Args)]
pub(crate) struct CommitArgs {
    /// the checkout directory (default: the current directory)
    pub dir: Option<String>,
    /// the commit message
    #[arg(long, value_name = "MESSAGE")]
    pub message: String,
    /// commit only the changes at or under this path (repeatable) — the way a
    /// tree past MAX_CHANGES_PER_COMMIT is committed, a subtree at a time. a
    /// path is relative to the checkout, or an absolute duckfs path.
    ///
    /// a FLAG, not a positional: `commit` already takes the checkout dir
    /// positionally, and `ducktape fs commit src/` reading as "the checkout is
    /// src/" is the kind of ambiguity that eats a commit
    #[arg(long = "path", value_name = "PATH", value_parser = nfc_path)]
    pub paths: Vec<String>,
    /// fail on a conflict instead of auto-rebasing onto the current head
    #[arg(long)]
    pub no_rebase: bool,
    #[command(flatten)]
    pub addr: NodeAddr,
    /// the user key that signs the write (default: the active wallet)
    #[arg(long, value_name = "PATH")]
    pub key: Option<std::path::PathBuf>,
    /// re-pin this node's identity to whatever it answers with now — the
    /// only way an already-trusted key changes (see `known_nodes`)
    #[arg(long)]
    pub trust_node: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct PutArgs {
    /// the local file to write
    pub local: std::path::PathBuf,
    /// the absolute duckfs path it lands at (parents are created)
    #[arg(value_parser = nfc_path)]
    pub path: String,
    /// the commit message (default: `put <path>`)
    #[arg(long, value_name = "MESSAGE")]
    pub message: Option<String>,
    #[command(flatten)]
    pub addr: NodeAddr,
    /// the user key that signs the write (default: the active wallet)
    #[arg(long, value_name = "PATH")]
    pub key: Option<std::path::PathBuf>,
    /// re-pin this node's identity to whatever it answers with now — the
    /// only way an already-trusted key changes (see `known_nodes`)
    #[arg(long)]
    pub trust_node: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct PinArgs {
    /// the snapshot to pin
    pub snapshot: String,
    /// the pin name gc keeps reachable
    pub name: String,
    #[command(flatten)]
    pub addr: NodeAddr,
    /// the user key that signs the write (default: the active wallet)
    #[arg(long, value_name = "PATH")]
    pub key: Option<std::path::PathBuf>,
    /// re-pin this node's identity to whatever it answers with now — the
    /// only way an already-trusted key changes (see `known_nodes`)
    #[arg(long)]
    pub trust_node: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct UnpinArgs {
    /// the pin name to release
    pub name: String,
    #[command(flatten)]
    pub addr: NodeAddr,
    /// the user key that signs the write (default: the active wallet)
    #[arg(long, value_name = "PATH")]
    pub key: Option<std::path::PathBuf>,
    /// re-pin this node's identity to whatever it answers with now — the
    /// only way an already-trusted key changes (see `known_nodes`)
    #[arg(long)]
    pub trust_node: bool,
}

/// `main` installs a subscriber only for `node run`, so a one-shot verb would
/// drop the engine's events on the floor — and the one that matters most (the
/// walk skipping a fifo it must never open) would be invisible exactly where a
/// user is watching. one stderr sink at `warn`, `RUST_LOG` overrides it, and
/// stdout stays the program output `cat`/`ls` write.
fn install_log_sink() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .try_init();
}

/// dispatch a parsed verb and map its `CliError` to the process exit code. an
/// EMPTY error message prints nothing — a dirty `status` and a commit conflict
/// each wrote their own output and only carry the exit code here.
pub(crate) fn run(cmd: FsCmd) -> u8 {
    install_log_sink();
    let outcome = match cmd {
        FsCmd::Ls(a) => read_cmds::ls(a),
        FsCmd::Cat(a) => read_cmds::cat(a),
        FsCmd::Stat(a) => read_cmds::stat(a),
        FsCmd::History(a) => read_cmds::history(a),
        FsCmd::Diff(a) => read_cmds::diff(a),
        FsCmd::Checkout(a) => work_cmds::checkout(a),
        FsCmd::Status(a) => work_cmds::status(a),
        FsCmd::Commit(a) => work_cmds::commit(a),
        FsCmd::Put(a) => work_cmds::put(a),
        FsCmd::Pin(a) => work_cmds::pin(a),
        FsCmd::Unpin(a) => work_cmds::unpin(a),
    };
    match outcome {
        Ok(()) => 0,
        Err(e) => {
            if !e.message.is_empty() {
                eprintln!("ducktape fs: {}", e.message);
            }
            e.code
        }
    }
}
