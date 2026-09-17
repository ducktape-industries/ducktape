//! The pure half of the executor: every `Command` the state machine hands a
//! boot becomes an `Op` naming the exact paths the writers touch. No I/O
//! here, so a test can assert what a boot WOULD do on either platform.
//!
//! A boot may be told to persist, resolve a swap, flip, qualify and exec.
//! Everything else (`Fetch`, `Download`, `Verify`, `SealImmutable`,
//! `PinSuccessor`, `Gc`) is the running app's, and a `Banner` has no one to
//! show it to — the app rebuilds it from `state.json`.

use std::path::PathBuf;

use app_update::{Command, Sha, state};

use crate::layout::{Layout, Platform};
use crate::refusal::Refusal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Write `state.json` whole: tmp + fsync + rename.
    Persist {
        path: PathBuf,
        text: String,
    },
    Flip(Flip),
    /// Inspect the install path and answer `SwapResolved`; finish the
    /// bookkeeping of a swap that landed.
    ResolveSwap(Flip),
    /// Run the staged release's own launcher with `--qualify`.
    Qualify {
        sha: Sha,
        /// Checked whole (and not a link) before its launcher is spawned.
        release_dir: PathBuf,
        launcher: PathBuf,
        state: PathBuf,
    },
    /// Replace this process with `exe`.
    Exec(Exec),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exec {
    pub sha: Sha,
    pub exe: PathBuf,
    pub state: PathBuf,
    /// Linux: the install symlink and what it must point at for `sha` to be
    /// what runs; refused otherwise. macOS: the installed bundle is the
    /// current release by construction.
    pub expect_link: Option<Link>,
}

/// A symlink and its (relative) target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub path: PathBuf,
    pub target: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flip {
    /// Linux: `previous -> releases/<from>`, then `current -> releases/<to>`,
    /// each a symlink to a temp name + rename(2).
    Symlinks {
        from: Sha,
        to: Sha,
        current: Link,
        previous: Link,
        /// The dir that must hold a complete release before the flip.
        to_dir: PathBuf,
    },
    /// macOS: `renamex_np(installed, staged, RENAME_SWAP)`; the old bundle
    /// lands at `staged` and is moved to `park`; `previous` then names it.
    BundleSwap {
        from: Sha,
        to: Sha,
        installed: PathBuf,
        staged: PathBuf,
        park: PathBuf,
        previous: Link,
        journal: PathBuf,
    },
}

impl Flip {
    pub fn sides(&self) -> (Sha, Sha) {
        match self {
            Flip::Symlinks { from, to, .. } | Flip::BundleSwap { from, to, .. } => (*from, *to),
        }
    }
}

pub fn plan(layout: &Layout, commands: Vec<Command>) -> Result<Vec<Op>, Refusal> {
    let mut ops = Vec::with_capacity(commands.len());
    for command in commands {
        match command {
            Command::Persist(phase) => ops.push(Op::Persist {
                path: layout.state_path(),
                text: state::encode(&phase),
            }),
            Command::ResolveSwap { from, to } => ops.push(Op::ResolveSwap(flip(layout, from, to))),
            Command::Flip { from, to } => ops.push(Op::Flip(flip(layout, from, to))),
            Command::Qualify(sha) => ops.push(Op::Qualify {
                sha,
                release_dir: layout.release_dir(sha),
                launcher: layout.launcher_of(sha),
                state: layout.state_path(),
            }),
            Command::Exec(sha) => ops.push(Op::Exec(exec(layout, sha))),
            Command::Banner(_) => {}
            Command::Fetch
            | Command::Download { .. }
            | Command::Verify(_)
            | Command::SealImmutable(_)
            | Command::PinSuccessor(_)
            | Command::Gc { .. } => {
                return Err(Refusal::new(
                    "app_only_command",
                    format!("a boot was told to {command:?}; that is the running app's job"),
                ));
            }
        }
    }
    Ok(ops)
}

pub fn exec(layout: &Layout, sha: Sha) -> Exec {
    let expect_link = match layout.platform {
        Platform::Linux => Some(Link {
            path: layout.current_link(),
            target: Layout::link_target(sha),
        }),
        Platform::MacOs => None,
    };
    Exec {
        sha,
        exe: layout.app_exe(),
        state: layout.state_path(),
        expect_link,
    }
}

pub fn flip(layout: &Layout, from: Sha, to: Sha) -> Flip {
    let previous = Link {
        path: layout.previous_link(),
        target: Layout::link_target(from),
    };
    match layout.platform {
        Platform::Linux => Flip::Symlinks {
            from,
            to,
            current: Link {
                path: layout.current_link(),
                target: Layout::link_target(to),
            },
            previous,
            to_dir: layout.release_dir(to),
        },
        Platform::MacOs => Flip::BundleSwap {
            from,
            to,
            installed: layout.installed_bundle(),
            staged: layout.staged_bundle(to),
            park: layout.staged_bundle(from),
            previous,
            journal: layout.journal_path(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::EnvInputs;
    use app_update::{Event, Idle, PendingHealthy, Phase, Staged, Swapping, step};

    fn layout(platform: Platform) -> Layout {
        let env = EnvInputs {
            xdg_config_home: Some("/s/cfg".into()),
            xdg_data_home: Some("/s/data".into()),
            home: Some("/home/op".into()),
            install_dir: Some("/s/apps".into()),
        };
        Layout::resolve(platform, &env).unwrap()
    }

    fn sha(name: &str) -> Sha {
        Sha::digest(name.as_bytes())
    }

    #[test]
    fn a_staged_boot_qualifies_with_the_staged_launcher() {
        let staged = Phase::Staged(Staged {
            current: sha("a"),
            previous: None,
            pinned_sequence: 1,
            staged: sha("b"),
            sequence: 1,
            display: "x".into(),
            node_contract: 1,
        });
        let (_, commands) = step(staged, Event::Boot);
        let ops = plan(&layout(Platform::Linux), commands.clone()).unwrap();
        assert_eq!(
            ops,
            vec![Op::Qualify {
                sha: sha("b"),
                release_dir: PathBuf::from(format!("/s/data/ducktape/releases/{}", sha("b"))),
                launcher: PathBuf::from(format!(
                    "/s/data/ducktape/releases/{}/ducktape-launcher",
                    sha("b")
                )),
                state: PathBuf::from("/s/cfg/ducktape/updates/state.json"),
            }]
        );
        let ops = plan(&layout(Platform::MacOs), commands).unwrap();
        let Op::Qualify { launcher, .. } = &ops[0] else {
            panic!("{ops:?}");
        };
        assert_eq!(
            launcher,
            &PathBuf::from(format!(
                "/s/cfg/ducktape/updates/releases/{}/Ducktape.app/Contents/MacOS/ducktape-launcher",
                sha("b")
            ))
        );
    }

    #[test]
    fn the_crash_rollback_flips_symlinks_then_execs_through_current() {
        let pending = Phase::PendingHealthy(PendingHealthy {
            current: sha("b"),
            previous: sha("a"),
            boots: 1,
            pinned_sequence: 1,
        });
        let (_, commands) = step(pending, Event::Boot);
        let ops = plan(&layout(Platform::Linux), commands).unwrap();
        assert_eq!(ops.len(), 4);
        assert!(matches!(ops[0], Op::Persist { .. }));
        assert_eq!(
            ops[1],
            Op::Flip(Flip::Symlinks {
                from: sha("b"),
                to: sha("a"),
                current: Link {
                    path: "/s/data/ducktape/current".into(),
                    target: PathBuf::from("releases").join(sha("a").to_string()),
                },
                previous: Link {
                    path: "/s/data/ducktape/previous".into(),
                    target: PathBuf::from("releases").join(sha("b").to_string()),
                },
                to_dir: PathBuf::from("/s/data/ducktape/releases").join(sha("a").to_string()),
            })
        );
        assert!(matches!(ops[2], Op::Persist { .. }));
        assert_eq!(
            ops[3],
            Op::Exec(Exec {
                sha: sha("a"),
                exe: "/s/data/ducktape/current/ducktape-app".into(),
                state: "/s/cfg/ducktape/updates/state.json".into(),
                expect_link: Some(Link {
                    path: "/s/data/ducktape/current".into(),
                    target: PathBuf::from("releases").join(sha("a").to_string()),
                }),
            })
        );
    }

    #[test]
    fn a_macos_swap_names_installed_staged_park_and_journal() {
        let swapping = Phase::Swapping(Swapping {
            from: sha("a"),
            to: sha("b"),
            pinned_sequence: 1,
        });
        let (_, commands) = step(swapping, Event::Boot);
        let ops = plan(&layout(Platform::MacOs), commands).unwrap();
        let releases = PathBuf::from("/s/cfg/ducktape/updates/releases");
        assert_eq!(
            ops,
            vec![Op::ResolveSwap(Flip::BundleSwap {
                from: sha("a"),
                to: sha("b"),
                installed: "/s/apps/Ducktape.app".into(),
                staged: releases.join(sha("b").to_string()).join("Ducktape.app"),
                park: releases.join(sha("a").to_string()).join("Ducktape.app"),
                previous: Link {
                    path: "/s/cfg/ducktape/updates/previous".into(),
                    target: PathBuf::from("releases").join(sha("a").to_string()),
                },
                journal: "/s/cfg/ducktape/updates/swap.json".into(),
            })]
        );
        let exec = exec(&layout(Platform::MacOs), sha("b"));
        assert_eq!(
            exec.exe,
            PathBuf::from("/s/apps/Ducktape.app/Contents/MacOS/ducktape-app")
        );
        assert_eq!(exec.expect_link, None);
    }

    #[test]
    fn app_only_commands_are_refused_and_banners_dropped() {
        let idle = Phase::Idle(Idle {
            current: sha("a"),
            previous: None,
            pinned_sequence: 0,
        });
        let (_, commands) = step(idle, Event::Tick);
        let refused = plan(&layout(Platform::Linux), commands).unwrap_err();
        assert_eq!(refused.reason, "app_only_command");
        let ops = plan(
            &layout(Platform::Linux),
            vec![Command::Banner(app_update::UpdateBanner::UpToDate)],
        )
        .unwrap();
        assert!(ops.is_empty());
    }
}
