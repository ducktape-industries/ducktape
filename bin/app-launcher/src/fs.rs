//! The few named writers and probes every executor path goes through.
//!
//! Symlink policy: the launcher never follows a symlink it did not write.
//! `state.json`, the swap journal, every `releases/<sha>` component and the
//! executables inside are opened only after `symlink_metadata` says they are
//! not links; `current`/`previous` are the only links, by construction, and
//! their targets are checked against the exact `releases/<sha>` they must
//! name.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use app_update::{Phase, Sha, state};
use sha2::{Digest, Sha256};

use crate::layout::{APP_EXE, LAUNCHER_EXE, VIEWS_DIR};
use crate::plan::Link;
use crate::refusal::Refusal;

/// Fail on a symlink at `path` itself; an absent path is fine.
pub fn refuse_symlink(path: &Path) -> Result<(), Refusal> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    match meta.file_type().is_symlink() {
        true => Err(Refusal::new(
            "symlink_refused",
            format!("{} is a symlink", path.display()),
        )),
        false => Ok(()),
    }
}

/// What `state.json` holds; `None` when there is no file (a dev copy of the
/// binaries with no install), an error for a link or unparsable contents.
pub fn read_state(path: &Path) -> Result<Option<Phase>, Refusal> {
    refuse_symlink(path)?;
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Refusal::io("state_unreadable", path, &error)),
    };
    state::decode(&text)
        .map(Some)
        .map_err(|error| Refusal::new("state_invalid", format!("{}: {error}", path.display())))
}

/// Write a file whole: tmp beside it, fsync, rename over.
pub fn persist(path: &Path, text: &str) -> Result<(), Refusal> {
    refuse_symlink(path)?;
    let parent = path.parent().ok_or_else(|| {
        Refusal::new(
            "persist_failed",
            format!("{} has no parent", path.display()),
        )
    })?;
    fs::create_dir_all(parent).map_err(|error| Refusal::io("persist_failed", parent, &error))?;
    let tmp = tmp_name(path);
    let write = || -> std::io::Result<()> {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    };
    write().map_err(|error| {
        let _ = fs::remove_file(&tmp);
        Refusal::io("persist_failed", path, &error)
    })
}

/// `link -> target`, atomically: a symlink at a temp name, renamed over.
pub fn replace_symlink(link: &Link) -> Result<(), Refusal> {
    let tmp = tmp_name(&link.path);
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&link.target, &tmp)
        .and_then(|()| fs::rename(&tmp, &link.path))
        .map_err(|error| {
            let _ = fs::remove_file(&tmp);
            Refusal::io("symlink_failed", &link.path, &error)
        })
}

/// Where a link points, or `None` when there is no link (an absent path or
/// a non-link, which is a refusal for the install path).
pub fn read_link(path: &Path) -> Result<Option<PathBuf>, Refusal> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Refusal::io("install_path_unreadable", path, &error)),
        Ok(meta) if meta.file_type().is_symlink() => fs::read_link(path)
            .map(Some)
            .map_err(|error| Refusal::io("install_path_unreadable", path, &error)),
        Ok(_) => Err(Refusal::new(
            "install_path_not_a_link",
            format!("{} exists and is not a symlink", path.display()),
        )),
    }
}

/// sha256 of a file's bytes, streamed.
pub fn digest_file(path: &Path) -> Result<Sha, Refusal> {
    refuse_symlink(path)?;
    let mut file =
        fs::File::open(path).map_err(|error| Refusal::io("digest_failed", path, &error))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| Refusal::io("digest_failed", path, &error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Sha::from_bytes(hasher.finalize().into()))
}

/// A regular, executable, non-link file.
pub fn require_executable(path: &Path) -> Result<(), Refusal> {
    refuse_symlink(path)?;
    let meta =
        fs::metadata(path).map_err(|error| Refusal::io("executable_missing", path, &error))?;
    let is_executable_file = meta.is_file() && meta.permissions().mode() & 0o111 != 0;
    match is_executable_file {
        true => Ok(()),
        false => Err(Refusal::new(
            "executable_missing",
            format!("{} is not an executable file", path.display()),
        )),
    }
}

/// A `views/` directory holding at least one `.wasm`.
pub fn require_views(bin_dir: &Path) -> Result<(), Refusal> {
    let views = bin_dir.join(VIEWS_DIR);
    let entries =
        fs::read_dir(&views).map_err(|error| Refusal::io("views_missing", &views, &error))?;
    let has_wasm = entries
        .flatten()
        .any(|entry| entry.path().extension().is_some_and(|ext| ext == "wasm"));
    match has_wasm {
        true => Ok(()),
        false => Err(Refusal::new(
            "views_missing",
            format!("{} holds no .wasm", views.display()),
        )),
    }
}

/// A release dir the launcher may flip to: not a link, holding both
/// executables and the views. Sealing it is the app's job; the launcher
/// only checks it is whole.
pub fn require_release(release_dir: &Path, bin_dir: &Path) -> Result<(), Refusal> {
    refuse_symlink(release_dir)?;
    let is_dir = fs::metadata(release_dir)
        .map(|meta| meta.is_dir())
        .unwrap_or(false);
    if !is_dir {
        return Err(Refusal::new(
            "release_missing",
            format!("{} is not a directory", release_dir.display()),
        ));
    }
    require_executable(&bin_dir.join(APP_EXE))?;
    require_executable(&bin_dir.join(LAUNCHER_EXE))?;
    require_views(bin_dir)
}

/// Whether this user may create and rename entries in `dir`.
pub fn dir_writable(dir: &Path) -> bool {
    let Ok(c_path) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: a NUL-terminated path and a flag constant; access(2) reads
    // nothing else.
    unsafe { libc::access(c_path.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

pub fn require_writable_install_dir(dir: &Path) -> Result<(), Refusal> {
    match dir_writable(dir) {
        true => Ok(()),
        false => Err(Refusal::new(
            "install_root_not_writable",
            format!(
                "{} is not writable by this user; the launcher installs and flips there without root — fix its permissions or set DUCKTAPE_INSTALL_DIR",
                dir.display()
            ),
        )),
    }
}

/// Copy a regular file with an explicit mode (`install -m`).
pub fn install_file(from: &Path, to: &Path, mode: u32) -> Result<(), Refusal> {
    refuse_symlink(from)?;
    fs::copy(from, to).map_err(|error| Refusal::io("copy_failed", to, &error))?;
    fs::set_permissions(to, fs::Permissions::from_mode(mode))
        .map_err(|error| Refusal::io("copy_failed", to, &error))
}

/// A whole bundle, copied with `/usr/bin/ditto`: the one copier that keeps
/// it whole (symlinks, modes, extended attributes, the code signature).
#[cfg(target_os = "macos")]
pub fn copy_bundle(from: &Path, to: &Path) -> Result<(), Refusal> {
    let status = std::process::Command::new("/usr/bin/ditto")
        .arg(from)
        .arg(to)
        .status()
        .map_err(|error| Refusal::io("copy_failed", to, &error))?;
    match status.success() {
        true => Ok(()),
        false => Err(Refusal::new(
            "copy_failed",
            format!("ditto {} {} exited {status}", from.display(), to.display()),
        )),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn copy_bundle(_from: &Path, _to: &Path) -> Result<(), Refusal> {
    Err(Refusal::new(
        "bundle_copy_needs_macos",
        "a whole-bundle copy is a macOS operation",
    ))
}

/// rename(2), or copy + remove across volumes.
pub fn move_tree(from: &Path, to: &Path) -> Result<(), Refusal> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            copy_bundle(from, to)?;
            fs::remove_dir_all(from).map_err(|error| Refusal::io("move_failed", from, &error))
        }
        Err(error) => Err(Refusal::io("move_failed", to, &error)),
    }
}

/// A per-process temp name beside `path`.
pub fn tmp_name(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".tmp-{}", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_writes_whole_and_read_state_refuses_a_link() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("updates").join("state.json");
        assert_eq!(read_state(&path).unwrap(), None);
        let phase = Phase::Idle(app_update::Idle {
            current: Sha::digest(b"a"),
            previous: None,
            pinned_sequence: 3,
        });
        persist(&path, &state::encode(&phase)).unwrap();
        assert_eq!(read_state(&path).unwrap(), Some(phase));
        assert!(fs::read_dir(path.parent().unwrap()).unwrap().count() == 1);
        let link = root.path().join("link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(read_state(&link).unwrap_err().reason, "symlink_refused");
        assert_eq!(persist(&link, "{}").unwrap_err().reason, "symlink_refused");
        fs::write(&path, "not json").unwrap();
        assert_eq!(read_state(&path).unwrap_err().reason, "state_invalid");
    }

    #[test]
    fn replace_symlink_swaps_atomically_and_read_link_names_the_target() {
        let root = tempfile::tempdir().unwrap();
        let link = Link {
            path: root.path().join("current"),
            target: "releases/a".into(),
        };
        assert_eq!(read_link(&link.path).unwrap(), None);
        replace_symlink(&link).unwrap();
        let again = Link {
            target: "releases/b".into(),
            ..link.clone()
        };
        replace_symlink(&again).unwrap();
        assert_eq!(
            read_link(&link.path).unwrap(),
            Some(PathBuf::from("releases/b"))
        );
        assert_eq!(
            fs::read_dir(root.path()).unwrap().count(),
            1,
            "no temp left behind"
        );
        fs::create_dir(root.path().join("dir")).unwrap();
        assert_eq!(
            read_link(&root.path().join("dir")).unwrap_err().reason,
            "install_path_not_a_link"
        );
    }

    #[test]
    fn digest_matches_sha_digest() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("bytes");
        fs::write(&path, b"hello").unwrap();
        assert_eq!(digest_file(&path).unwrap(), Sha::digest(b"hello"));
    }
}
