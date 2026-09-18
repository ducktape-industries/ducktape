//! the `fs` CLI error type, and the exit code it maps an unresolved node
//! address to. The addressing flags and the resolution ladder itself are
//! [`crate::cli_args::NodeAddr`] — ONE ladder for every family.

use duckfs_client::api::{ApiError, refusal_line};
use unicode_normalization::UnicodeNormalization as _;

pub use crate::cli_args::NodeAddr;

/// a clap `value_parser` for every path-shaped argument: NFC-normalize at the
/// one place an OS string becomes a CLI argument — the same crossing
/// `duckfs_client::scan::duckfs_join` owns for a scanned name.
/// `duckfs_core::paths::canonical` stays reject-only downstream (it only ever
/// rejects a byte sequence, never rewrites one) — a person-typed NFD path
/// (what macOS hands back for tab-completion, `find`, drag-and-drop on an
/// HFS+/legacy volume) is rewritten HERE, once, so every verb that takes a
/// path argument is covered instead of each one normalizing for itself. NFC is
/// idempotent, so an already-composed argument passes through unchanged.
pub fn nfc_path(raw: &str) -> Result<String, std::convert::Infallible> {
    Ok(raw.nfc().collect())
}

/// a CLI failure carrying the process exit code. code 2 is a usage error (an
/// unresolved node) and a commit conflict; code 1 is a general operational
/// failure (and a dirty `status`).
#[derive(Debug)]
pub struct CliError {
    pub code: u8,
    pub message: Message,
}

/// what a failed verb says on stderr.
#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    /// nothing: the verb already wrote its own output (a dirty `status`'s
    /// A/M/D lines, a commit conflict's report).
    Silent,
    /// this CLI's own sentence.
    Said(String),
    /// a refusal, its two halves kept apart until [`CliError::line`] renders
    /// them — nothing downstream ever re-parses one out of a joined string.
    Refused { reason: String, sentence: String },
}

impl CliError {
    /// a usage error (exit 2): an unresolved node address.
    pub fn usage(m: impl Into<String>) -> Self {
        CliError {
            code: 2,
            message: Message::Said(m.into()),
        }
    }

    /// a general operational failure (exit 1): an io error, an unreachable node.
    pub fn failed(m: impl Into<String>) -> Self {
        CliError {
            code: 1,
            message: Message::Said(m.into()),
        }
    }

    /// a refusal (exit 1): the class token and the words whoever refused
    /// wrote. every `ducktape fs` verb refuses through here, so `cat`, `ls` and
    /// `stat` cannot disagree about what a missing path looks like.
    pub fn refused(reason: impl Into<String>, sentence: impl Into<String>) -> Self {
        CliError {
            code: 1,
            message: Message::Refused {
                reason: reason.into(),
                sentence: sentence.into(),
            },
        }
    }

    /// exit with `code` and print NOTHING — the verb already wrote its output
    /// (a dirty `status` prints its A/M/D lines, then exits 1 silently; a commit
    /// conflict prints its report, then exits 2).
    pub fn silent(code: u8) -> Self {
        CliError {
            code,
            message: Message::Silent,
        }
    }

    /// the stderr line this failure prints after `ducktape fs: `, or `None`
    /// when the verb already wrote its own. a refusal renders through
    /// [`refusal_line`]: the sentence first, the class token last in brackets.
    pub fn line(&self) -> Option<String> {
        match &self.message {
            Message::Silent => None,
            Message::Said(m) => Some(m.clone()),
            Message::Refused { reason, sentence } => Some(refusal_line(reason, sentence)),
        }
    }
}

/// map a transport failure to a CLI failure — the ONE mapping every `ducktape
/// fs` verb uses. a refusal keeps both halves it arrived in; a connection
/// failure is THIS side's and says so, because the two are not the caller's to
/// fix in the same way.
pub fn api_err(e: ApiError) -> CliError {
    match e {
        ApiError::Rejected { reason, sentence } => CliError::refused(reason, sentence),
        ApiError::NotFound => CliError::refused("not_found", "the node has no such route"),
        ApiError::Transport(m) => CliError::failed(format!("cannot reach the node: {m}")),
    }
}

/// resolve the node http base for a verb with no ambient address of its own:
/// [`NodeAddr::resolve`], the ONE ladder every family shares. Worktree verbs
/// that DO have one (a checkout's `.duckfs` index) call
/// [`NodeAddr::resolve_with`] in `work_cmds` instead.
pub fn resolve_node(addr: &NodeAddr) -> Result<String, CliError> {
    addr.resolve().map_err(CliError::usage)
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use crate::fs_cli::FsCmd;

    /// a top-level `Parser` wrapper so the tests can drive clap over `FsCmd`
    /// (a `Subcommand` can't be parsed on its own).
    #[derive(clap::Parser)]
    struct TestCli {
        #[command(subcommand)]
        cmd: FsCmd,
    }

    fn parse(argv: &[&str]) -> Result<FsCmd, clap::Error> {
        // argv[0] is the binary name clap discards.
        TestCli::try_parse_from(std::iter::once("ducktape").chain(argv.iter().copied()))
            .map(|c| c.cmd)
    }

    /// `ls --json <path>` — the valueless `--json` must NOT swallow the path,
    /// which stays positional.
    #[test]
    fn ls_json_keeps_path_positional() {
        let FsCmd::Ls(a) = parse(&["ls", "--json", "/dir"]).unwrap() else {
            panic!("expected ls");
        };
        assert!(a.json);
        assert_eq!(a.path, "/dir");
    }

    /// `commit --no-rebase <dir>` — the valueless `--no-rebase` leaves `<dir>`
    /// positional; `--message` still binds its value.
    #[test]
    fn commit_no_rebase_keeps_dir_positional() {
        let FsCmd::Commit(a) =
            parse(&["commit", "--no-rebase", "wt/dir", "--message", "m"]).unwrap()
        else {
            panic!("expected commit");
        };
        assert!(a.no_rebase);
        assert_eq!(a.dir.as_deref(), Some("wt/dir"));
        assert_eq!(a.message, "m");
    }

    /// `--path` is repeatable and does not collide with the `[dir]` positional —
    /// the pathspec the `MAX_CHANGES_PER_COMMIT` refusal tells the user to run.
    #[test]
    fn commit_takes_a_repeatable_pathspec() {
        let FsCmd::Commit(a) = parse(&[
            "commit",
            "wt",
            "--message",
            "m",
            "--path",
            "src",
            "--path",
            "docs/x.md",
        ])
        .unwrap() else {
            panic!("expected commit");
        };
        assert_eq!(a.dir.as_deref(), Some("wt"));
        assert_eq!(a.paths, vec!["src".to_string(), "docs/x.md".to_string()]);
    }

    /// `status` takes the same pathspec, so a user can see what it selects
    /// before committing it.
    #[test]
    fn status_takes_the_same_pathspec() {
        let FsCmd::Status(a) = parse(&["status", "--path", "src"]).unwrap() else {
            panic!("expected status");
        };
        assert!(a.dir.is_none());
        assert_eq!(a.paths, vec!["src".to_string()]);
    }

    /// `-n` is the short alias for `--network` and does not eat the positional.
    #[test]
    fn dash_n_maps_to_network_and_leaves_path() {
        let FsCmd::Ls(a) = parse(&["ls", "-n", "ducktape", "some/path"]).unwrap() else {
            panic!("expected ls");
        };
        assert_eq!(a.addr.network.as_deref(), Some("ducktape"));
        assert_eq!(a.path, "some/path");
    }

    /// a value flag binds the following token as its value.
    #[test]
    fn snapshot_flag_binds_its_value() {
        let FsCmd::Ls(a) = parse(&["ls", "/p", "--snapshot", "s1"]).unwrap() else {
            panic!("expected ls");
        };
        assert_eq!(a.path, "/p");
        assert_eq!(a.snapshot.as_deref(), Some("s1"));
    }

    /// clap validates the numeric flag: a non-number is a parse error (exit 2).
    #[test]
    fn limit_rejects_a_non_number() {
        assert!(parse(&["ls", "/p", "--limit", "abc"]).is_err());
    }

    /// an NFD-typed path argument — what macOS hands back for tab-completion,
    /// `find`, or drag-and-drop off an HFS+/legacy volume — is NFC-normalized
    /// at parse time. one table over every verb that takes a path-shaped
    /// argument, so a verb whose field skips `nfc_path` fails this test
    /// instead of silently shipping the gap #1455 reported.
    #[test]
    fn path_arguments_are_nfc_normalized() {
        use unicode_normalization::UnicodeNormalization as _;

        let nfc: String = "/nfd-check/설계.md".nfc().collect();
        let nfd: String = "/nfd-check/설계.md".nfd().collect();
        assert_ne!(nfc, nfd, "fixture must actually decompose under NFD");

        // one row per verb: the argv to parse, and how to pull the
        // normalized path back out of the parsed `FsCmd`.
        type PathExtractor = Box<dyn Fn(FsCmd) -> String>;
        let rows: Vec<(Vec<String>, PathExtractor)> = vec![
            (
                vec!["ls".into(), nfd.clone()],
                Box::new(|c| match c {
                    FsCmd::Ls(a) => a.path,
                    _ => panic!("expected ls"),
                }),
            ),
            (
                vec!["cat".into(), nfd.clone()],
                Box::new(|c| match c {
                    FsCmd::Cat(a) => a.path,
                    _ => panic!("expected cat"),
                }),
            ),
            (
                vec!["stat".into(), nfd.clone()],
                Box::new(|c| match c {
                    FsCmd::Stat(a) => a.path,
                    _ => panic!("expected stat"),
                }),
            ),
            (
                vec![
                    "diff".into(),
                    "s1".into(),
                    "s2".into(),
                    "--prefix".into(),
                    nfd.clone(),
                ],
                Box::new(|c| match c {
                    FsCmd::Diff(a) => a.prefix.unwrap(),
                    _ => panic!("expected diff"),
                }),
            ),
            (
                vec!["checkout".into(), nfd.clone(), "wt/dir".into()],
                Box::new(|c| match c {
                    FsCmd::Checkout(a) => a.prefix,
                    _ => panic!("expected checkout"),
                }),
            ),
            (
                vec!["status".into(), "--path".into(), nfd.clone()],
                Box::new(|c| match c {
                    FsCmd::Status(a) => a.paths.into_iter().next().unwrap(),
                    _ => panic!("expected status"),
                }),
            ),
            (
                vec![
                    "commit".into(),
                    "wt".into(),
                    "--message".into(),
                    "m".into(),
                    "--path".into(),
                    nfd.clone(),
                ],
                Box::new(|c| match c {
                    FsCmd::Commit(a) => a.paths.into_iter().next().unwrap(),
                    _ => panic!("expected commit"),
                }),
            ),
        ];

        for (argv, extract) in rows {
            let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
            let cmd = parse(&argv_refs).unwrap_or_else(|e| panic!("argv={argv:?}: {e}"));
            assert_eq!(extract(cmd), nfc, "argv={argv:?}");
        }
    }

    /// `--node` binds through the shared addressing group. The PRECEDENCE it
    /// sits at the top of is pinned once, in
    /// `cli_args::tests::the_node_address_ladder_ranks_flag_network_env_context_registry`
    /// — not re-asserted per family, which is how four of them drifted apart.
    #[test]
    fn dash_dash_node_binds_its_url() {
        let FsCmd::Ls(a) = parse(&["ls", "--node", "http://explicit:8844", "/p"]).unwrap() else {
            panic!("expected ls");
        };
        assert_eq!(a.addr.node.as_deref(), Some("http://explicit:8844"));
    }
}
