//! Pointing the install path at a release, and finding out where it points.
//!
//! Linux: `current` and `previous` are symlinks into `releases/`; a flip is
//! a symlink at a temp name + rename(2), so the install path always resolves
//! to one whole release. A boot in `Swapping` reads the link.
//!
//! macOS: the installed `Ducktape.app` is swapped whole against the staged
//! one with `renamex_np(RENAME_SWAP)` (atomic, same volume), and the old
//! bundle — now sitting at the staged path — is moved to
//! `releases/<from>/Ducktape.app`, which `previous` then names. Because both
//! paths hold a bundle before AND after the swap, the flip first records the
//! incoming executable's digest in `<updates>/swap.json`; a boot in
//! `Swapping` hashes the installed executable against it to learn whether the
//! swap landed, then finishes the bookkeeping.

use std::path::Path;

use app_update::SwapState;

use crate::fs;
use crate::layout::bundle_bin_dir;
use crate::plan::{Flip, Link};
use crate::refusal::Refusal;

pub fn perform(flip: &Flip) -> Result<(), Refusal> {
    match flip {
        Flip::Symlinks {
            current,
            previous,
            to_dir,
            ..
        } => flip_symlinks(current, previous, to_dir),
        Flip::BundleSwap {
            to,
            installed,
            staged,
            park,
            previous,
            journal,
            ..
        } => macos::swap_bundles(&BundleSwap {
            to: *to,
            installed,
            staged,
            park,
            previous,
            journal,
        }),
    }
}

pub fn resolve(flip: &Flip) -> Result<SwapState, Refusal> {
    match flip {
        Flip::Symlinks {
            current, previous, ..
        } => resolve_symlinks(current, previous),
        Flip::BundleSwap {
            to,
            installed,
            staged,
            park,
            previous,
            journal,
            ..
        } => macos::resolve_swap(&BundleSwap {
            to: *to,
            installed,
            staged,
            park,
            previous,
            journal,
        }),
    }
}

fn flip_symlinks(current: &Link, previous: &Link, to_dir: &Path) -> Result<(), Refusal> {
    fs::require_release(to_dir, to_dir)?;
    fs::replace_symlink(previous)?;
    fs::replace_symlink(current)
}

fn resolve_symlinks(current: &Link, previous: &Link) -> Result<SwapState, Refusal> {
    let points_at = fs::read_link(&current.path)?;
    let landed = points_at.as_deref() == Some(current.target.as_path());
    let untouched = points_at.as_deref() == Some(previous.target.as_path());
    match (landed, untouched) {
        (true, _) => {
            fs::replace_symlink(previous)?;
            Ok(SwapState::Landed)
        }
        (false, true) => Ok(SwapState::Untouched),
        (false, false) => Err(Refusal::new(
            "install_path_unknown",
            format!(
                "{} points at {:?}, neither side of the swap",
                current.path.display(),
                points_at
            ),
        )),
    }
}

/// The paths of one macOS bundle swap.
// Read by the macOS swap; the Linux build carries the type so the journal
// codec below stays testable on every platform.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct BundleSwap<'a> {
    pub to: app_update::Sha,
    pub installed: &'a Path,
    pub staged: &'a Path,
    pub park: &'a Path,
    pub previous: &'a Link,
    pub journal: &'a Path,
}

/// What a bundle swap wrote before `RENAME_SWAP`: which release is coming
/// in, the digest of its executable, and where the outgoing bundle sits
/// right after the swap. Pure over its two files so a Linux test covers
/// the codec and the landed decision; only the macOS swap calls it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Journal {
    pub to: app_update::Sha,
    pub app_digest: app_update::Sha,
    pub swapped_with: std::path::PathBuf,
}

/// The installed bundle's executable digest against a journal: the swap
/// landed when they agree. Pure over the two values so a test can run it
/// on Linux; the digest itself comes from [`fs::digest_file`].
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn journal_says_landed(journal: &Journal, installed_digest: app_update::Sha) -> bool {
    journal.app_digest == installed_digest
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn read_journal(path: &Path) -> Result<Option<Journal>, Refusal> {
    fs::refuse_symlink(path)?;
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Refusal::io("journal_unreadable", path, &error)),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| Refusal::new("journal_invalid", format!("{}: {error}", path.display())))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn write_journal(path: &Path, journal: &Journal) -> Result<(), Refusal> {
    let text = serde_json::to_string_pretty(journal).expect("a Journal always serializes");
    fs::persist(path, &text)
}

/// The installed bundle's executable, for the journal comparison.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn installed_app_digest(installed: &Path) -> Result<app_update::Sha, Refusal> {
    fs::digest_file(&bundle_bin_dir(installed).join(crate::layout::APP_EXE))
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CString;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    use app_update::SwapState;

    use super::{BundleSwap, Journal, installed_app_digest, journal_says_landed};
    use crate::fs;
    use crate::layout::{APP_EXE, bundle_bin_dir};
    use crate::refusal::Refusal;

    /// Swap the installed bundle against the staged one and park the old
    /// bundle under `releases/<from>`.
    pub fn swap_bundles(swap: &BundleSwap<'_>) -> Result<(), Refusal> {
        fs::require_release(swap.staged, &bundle_bin_dir(swap.staged))?;
        let install_dir = parent_of(swap.installed)?;
        fs::require_writable_install_dir(install_dir)?;
        let park_is_free = std::fs::symlink_metadata(swap.park).is_err();
        if !park_is_free {
            return Err(Refusal::new(
                "park_slot_occupied",
                format!("{} already exists; remove it first", swap.park.display()),
            ));
        }
        // The app seals a staged release `a-w`; the swap renames entries
        // inside `releases/<to>/`, which needs write on that directory.
        unseal_dir(parent_of(swap.staged)?)?;
        let swap_source = same_volume_copy(swap.staged, install_dir, swap.to)?;
        let journal = Journal {
            to: swap.to,
            app_digest: fs::digest_file(&bundle_bin_dir(&swap_source).join(APP_EXE))?,
            swapped_with: swap_source.clone(),
        };
        super::write_journal(swap.journal, &journal)?;
        rename_swap(swap.installed, &swap_source)?;
        finish(swap, &journal)
    }

    /// After the swap landed: park the outgoing bundle, point `previous`
    /// at it, drop the journal. Idempotent, so a boot that finds a journal
    /// can call it again.
    fn finish(swap: &BundleSwap<'_>, journal: &Journal) -> Result<(), Refusal> {
        let old_still_at_swap_path = std::fs::symlink_metadata(&journal.swapped_with).is_ok();
        if old_still_at_swap_path {
            std::fs::create_dir_all(parent_of(swap.park)?)
                .map_err(|error| Refusal::io("park_failed", swap.park, &error))?;
            fs::move_tree(&journal.swapped_with, swap.park)?;
        }
        let staged_dir = parent_of(swap.staged)?;
        let _ = std::fs::remove_dir(staged_dir);
        fs::replace_symlink(swap.previous)?;
        std::fs::remove_file(swap.journal)
            .map_err(|error| Refusal::io("journal_unremovable", swap.journal, &error))
    }

    pub fn resolve_swap(swap: &BundleSwap<'_>) -> Result<SwapState, Refusal> {
        let Some(journal) = super::read_journal(swap.journal)? else {
            return Ok(SwapState::Untouched);
        };
        let is_this_swap = journal.to == swap.to;
        if !is_this_swap {
            return Err(Refusal::new(
                "journal_mismatch",
                format!("{} names another release", swap.journal.display()),
            ));
        }
        let installed = installed_app_digest(swap.installed)?;
        match journal_says_landed(&journal, installed) {
            true => {
                finish(swap, &journal)?;
                Ok(SwapState::Landed)
            }
            false => {
                std::fs::remove_file(swap.journal)
                    .map_err(|error| Refusal::io("journal_unremovable", swap.journal, &error))?;
                Ok(SwapState::Untouched)
            }
        }
    }

    /// `renamex_np(from, to, RENAME_SWAP)`: both paths exist before and
    /// after; their contents trade places atomically.
    fn rename_swap(installed: &Path, staged: &Path) -> Result<(), Refusal> {
        let from = c_path(installed)?;
        let to = c_path(staged)?;
        // SAFETY: two NUL-terminated paths and a flag constant.
        let rc = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_SWAP) };
        match rc {
            0 => Ok(()),
            _ => Err(Refusal::io(
                "swap_failed",
                installed,
                &std::io::Error::last_os_error(),
            )),
        }
    }

    /// The staged bundle, on the install dir's volume: itself when it
    /// already is, else a copy at `<install dir>/.Ducktape.app.<sha>`.
    fn same_volume_copy(
        staged: &Path,
        install_dir: &Path,
        sha: app_update::Sha,
    ) -> Result<std::path::PathBuf, Refusal> {
        let staged_dev = std::fs::metadata(staged)
            .map_err(|error| Refusal::io("release_missing", staged, &error))?
            .dev();
        let install_dev = std::fs::metadata(install_dir)
            .map_err(|error| Refusal::io("install_root_missing", install_dir, &error))?
            .dev();
        if staged_dev == install_dev {
            return Ok(staged.to_path_buf());
        }
        let copy = install_dir.join(format!(".Ducktape.app.{sha}"));
        let _ = std::fs::remove_dir_all(&copy);
        fs::copy_bundle(staged, &copy)?;
        Ok(copy)
    }

    fn unseal_dir(dir: &Path) -> Result<(), Refusal> {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir)
            .map_err(|error| Refusal::io("release_missing", dir, &error))?
            .permissions()
            .mode();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode | 0o200))
            .map_err(|error| Refusal::io("unseal_failed", dir, &error))
    }

    fn parent_of(path: &Path) -> Result<&Path, Refusal> {
        path.parent()
            .ok_or_else(|| Refusal::new("bad_path", format!("{} has no parent", path.display())))
    }

    fn c_path(path: &Path) -> Result<CString, Refusal> {
        CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| Refusal::new("bad_path", format!("{} holds a NUL", path.display())))
    }
}

#[cfg(not(target_os = "macos"))]
mod macos {
    use app_update::SwapState;

    use super::BundleSwap;
    use crate::refusal::Refusal;

    pub fn swap_bundles(_: &BundleSwap<'_>) -> Result<(), Refusal> {
        Err(Refusal::new(
            "bundle_swap_needs_macos",
            "a whole-bundle swap is a macOS operation",
        ))
    }

    pub fn resolve_swap(_: &BundleSwap<'_>) -> Result<SwapState, Refusal> {
        Err(Refusal::new(
            "bundle_swap_needs_macos",
            "a whole-bundle swap is a macOS operation",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use app_update::Sha;

    #[test]
    fn the_journal_round_trips_and_decides_landed_by_digest() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("swap.json");
        assert_eq!(read_journal(&path).unwrap(), None);
        let journal = Journal {
            to: Sha::digest(b"b"),
            app_digest: Sha::digest(b"app-b"),
            swapped_with: root.path().join("Ducktape.app"),
        };
        write_journal(&path, &journal).unwrap();
        assert_eq!(read_journal(&path).unwrap(), Some(journal));
        let journal = read_journal(&path).unwrap().unwrap();
        assert!(journal_says_landed(&journal, Sha::digest(b"app-b")));
        assert!(!journal_says_landed(&journal, Sha::digest(b"app-a")));
    }

    #[test]
    fn a_link_that_names_neither_side_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let current = Link {
            path: root.path().join("current"),
            target: "releases/b".into(),
        };
        let previous = Link {
            path: root.path().join("previous"),
            target: "releases/a".into(),
        };
        fs::replace_symlink(&Link {
            path: current.path.clone(),
            target: "releases/zzz".into(),
        })
        .unwrap();
        let refused = resolve_symlinks(&current, &previous).unwrap_err();
        assert_eq!(refused.reason, "install_path_unknown");
        fs::replace_symlink(&Link {
            path: current.path.clone(),
            target: "releases/a".into(),
        })
        .unwrap();
        assert_eq!(
            resolve_symlinks(&current, &previous).unwrap(),
            SwapState::Untouched
        );
        fs::replace_symlink(&current).unwrap();
        assert_eq!(
            resolve_symlinks(&current, &previous).unwrap(),
            SwapState::Landed
        );
        assert_eq!(
            fs::read_link(&previous.path).unwrap(),
            Some(std::path::PathBuf::from("releases/a"))
        );
    }
}
