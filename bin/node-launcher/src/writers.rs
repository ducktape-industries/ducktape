//! The named writers and probes every effect goes through. Nothing here
//! decides: each function is told a path and does one thing to it.
//!
//! Symlink policy, as in the app's launcher: the launcher never follows a
//! symlink it did not write. `state.json` and every `releases/<sha>` component
//! are opened only after `symlink_metadata` says they are not links;
//! `current`/`previous` are the only links, by construction, and their targets
//! are checked against the exact `releases/<sha>` they must name.

use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use app_update::{Phase, Sha, state};
use sha2::{Digest, Sha256};

use crate::layout::{Layout, NODE_EXE};
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

/// One supervisor's exclusive hold on a workspace, kept for as long as it
/// supervises. Dropping it releases the lock, and so does the process dying
/// however it died — which is why this is `flock` and not a pid file a killed
/// launcher leaves behind for nobody to clear.
#[derive(Debug)]
pub struct Claim(fs::File);

impl Drop for Claim {
    fn drop(&mut self) {
        // SAFETY: our own descriptor, open until this struct is gone.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Claim the workspace for this `run` or `install`, or refuse because another
/// one holds it.
///
/// ONE WRITER PER WORKSPACE. `run` owns `state.json` and the install path, and
/// a second one decides from the same files with its own memory of what it has
/// already answered for: it re-stages a release the first rolled back from,
/// and a qualify it passes flips `current` out from under the first's live
/// node. Its own child cannot bind the node's listeners either, so besides
/// that it does nothing but restart a node that dies on every boot. `install`
/// rewrites both files whole, so under a live `run` it resets the phase that
/// `run` is in the middle of, and beside a second install the two race the
/// link. `service` mode claims nothing — several daemons share one workspace on
/// purpose, and none of them writes.
pub fn claim(path: &Path) -> Result<Claim, Refusal> {
    refuse_symlink(path)?;
    let parent = path.parent().ok_or_else(|| {
        Refusal::new("claim_failed", format!("{} has no parent", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|error| Refusal::io("claim_failed", parent, &error))?;
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|error| Refusal::io("claim_failed", path, &error))?;
    // SAFETY: `file` outlives the call; `flock` only takes a lock on its fd.
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    match taken {
        true => Ok(Claim(file)),
        false => Err(Refusal::new(
            "workspace_locked",
            format!(
                "{} is held by another ducktape-node-launcher — a workspace has one writer; \
                 stop the one running it first",
                path.display()
            ),
        )),
    }
}

/// What `state.json` holds; `None` when there is no file, an error for a link
/// or unparsable contents.
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
pub fn replace_symlink(link: &Path, target: &Path) -> Result<(), Refusal> {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).map_err(|error| Refusal::io("symlink_failed", parent, &error))?;
    }
    let tmp = tmp_name(link);
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)
        .and_then(|()| fs::rename(&tmp, link))
        .map_err(|error| {
            let _ = fs::remove_file(&tmp);
            Refusal::io("symlink_failed", link, &error)
        })
}

/// Where a link points, or `None` when there is no link (an absent path or a
/// non-link, which is a refusal for the install path).
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

/// Copy `from` into `to`, directories and regular files only — the founding
/// set an install carries into the release it seeds. Anything else in the
/// source (a link, a device) is skipped rather than followed: this launcher
/// never follows a symlink it did not write.
pub fn copy_tree(from: &Path, to: &Path) -> Result<(), Refusal> {
    fs::create_dir_all(to).map_err(|error| Refusal::io("copy_failed", to, &error))?;
    let entries = fs::read_dir(from).map_err(|error| Refusal::io("copy_failed", from, &error))?;
    for entry in entries {
        let entry = entry.map_err(|error| Refusal::io("copy_failed", from, &error))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|error| Refusal::io("copy_failed", &source, &error))?;
        if kind.is_dir() {
            copy_tree(&source, &target)?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        fs::copy(&source, &target).map_err(|error| Refusal::io("copy_failed", &target, &error))?;
    }
    Ok(())
}

/// A release directory holds one executable `ducktape`, and it is a real
/// directory this launcher extracted — never a link someone left there.
pub fn require_release(dir: &Path) -> Result<(), Refusal> {
    refuse_symlink(dir)?;
    let exe = dir.join(NODE_EXE);
    refuse_symlink(&exe)?;
    let meta =
        fs::metadata(&exe).map_err(|error| Refusal::io("release_incomplete", &exe, &error))?;
    let runnable = meta.is_file() && meta.permissions().mode() & 0o111 != 0;
    match runnable {
        true => Ok(()),
        false => Err(Refusal::new(
            "release_incomplete",
            format!("{} is not an executable file", exe.display()),
        )),
    }
}

/// Extract the archive into `release_dir` after re-checking its identity.
/// The directory is replaced whole: a directory at a sha this launcher has
/// not verified is a leftover, never a release.
pub fn stage(archive: &Path, sha: Sha, release_dir: &Path) -> Result<(), Refusal> {
    let landed = digest_file(archive)?;
    let matches = landed == sha;
    if !matches {
        return Err(Refusal::new(
            "sha256_mismatch",
            format!("the archive hashes to {landed}, not {sha}"),
        ));
    }
    clear_dir(release_dir)?;
    extract(archive, release_dir)?;
    require_release(release_dir)
}

fn clear_dir(dir: &Path) -> Result<(), Refusal> {
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return Ok(());
    };
    if meta.file_type().is_symlink() {
        return Err(Refusal::new(
            "release_dir_is_a_link",
            format!("{} is a symlink", dir.display()),
        ));
    }
    unseal(dir);
    fs::remove_dir_all(dir).map_err(|error| Refusal::io("release_dir_unremovable", dir, &error))
}

/// A node archive carries regular files and directories, at relative paths
/// that stay inside the release directory. Anything else — an absolute path,
/// a `..`, a symlink, a hard link, a device — is refused by name rather than
/// unpacked and hoped about.
fn extract(archive: &Path, release_dir: &Path) -> Result<(), Refusal> {
    let file =
        fs::File::open(archive).map_err(|error| Refusal::io("archive_unreadable", archive, &error))?;
    let decoder = zstd::Decoder::new(file)
        .map_err(|error| Refusal::io("archive_unreadable", archive, &error))?;
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(false);
    tar.set_preserve_ownerships(false);
    fs::create_dir_all(release_dir)
        .map_err(|error| Refusal::io("extract_failed", release_dir, &error))?;
    let entries = tar
        .entries()
        .map_err(|error| Refusal::io("archive_unreadable", archive, &error))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|error| Refusal::io("archive_unreadable", archive, &error))?;
        let path = entry
            .path()
            .map_err(|error| Refusal::io("archive_unreadable", archive, &error))?
            .into_owned();
        let destination = destination(release_dir, &path)?;
        let kind = entry.header().entry_type();
        let admitted = kind.is_file() || kind.is_dir();
        if !admitted {
            return Err(Refusal::new(
                "archive_entry_refused",
                format!("{} is a {kind:?} entry", path.display()),
            ));
        }
        // A release carries the founding set in `modules/`, so entries are
        // nested. `unpack` writes a file but never makes its parent, and
        // whether a directory entry precedes its files is up to whichever tar
        // wrote the archive — so the directory is made here, from a path every
        // component of which `destination` has already checked is a plain name.
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| Refusal::io("extract_failed", parent, &error))?;
        }
        entry
            .unpack(&destination)
            .map_err(|error| Refusal::io("extract_failed", &destination, &error))?;
    }
    Ok(())
}

/// The absolute path an archive entry unpacks to, or a refusal: every
/// component must be a plain name.
fn destination(release_dir: &Path, entry: &Path) -> Result<PathBuf, Refusal> {
    let mut destination = release_dir.to_path_buf();
    for component in entry.components() {
        match component {
            Component::Normal(name) => destination.push(name),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                return Err(Refusal::new(
                    "archive_path_escapes",
                    format!("{} leaves the release directory", entry.display()),
                ));
            }
        }
    }
    Ok(destination)
}

/// `chmod -R a-w`: what was verified stays what is flipped.
pub fn seal(dir: &Path) {
    set_writable(dir, false);
}

pub fn unseal(dir: &Path) {
    set_writable(dir, true);
}

/// Best effort by design: a release that cannot be sealed is still a release,
/// and refusing the stage over a permission bit would strand the node on the
/// old binary for a cosmetic reason.
fn set_writable(path: &Path, writable: bool) {
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            set_writable(&entry.path(), writable);
        }
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    let mut mode = meta.permissions().mode();
    mode = match writable {
        true => mode | 0o200,
        false => mode & !0o222,
    };
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

/// Remove every release directory not in `keep`, and every stale `.partial`.
pub fn collect(layout: &Layout, keep: &[Sha]) {
    let kept: Vec<String> = keep.iter().map(Sha::to_string).collect();
    let Ok(entries) = fs::read_dir(layout.releases_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_kept_release = kept.contains(&name);
        let is_kept_partial = kept
            .iter()
            .any(|sha| name == format!("{sha}.partial"));
        if is_kept_release || is_kept_partial {
            continue;
        }
        let path = entry.path();
        unseal(&path);
        let removed = match path.is_dir() {
            true => fs::remove_dir_all(&path),
            false => fs::remove_file(&path),
        };
        if let Err(error) = removed {
            tracing::debug!(target: crate::TARGET, path = %path.display(), %error, "could not collect");
        }
    }
}

fn tmp_name(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_zst(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, bytes, mode) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(*mode);
            header.set_cksum();
            builder.append_data(&mut header, name, *bytes).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        zstd::encode_all(tar.as_slice(), 0).unwrap()
    }

    #[test]
    fn a_release_archive_extracts_and_is_sealed() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("release.tar.zst");
        let bytes = tar_zst(&[("ducktape", b"#!/bin/sh\nexit 0\n", 0o755)]);
        fs::write(&archive, &bytes).unwrap();
        let sha = Sha::digest(&bytes);
        let release = dir.path().join("releases").join(sha.to_string());
        stage(&archive, sha, &release).unwrap();
        assert!(release.join("ducktape").exists());
        seal(&release);
        let mode = fs::metadata(release.join("ducktape"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o222, 0, "a sealed release is unwritable");
        // and re-staging over the sealed directory replaces it whole.
        stage(&archive, sha, &release).unwrap();
    }

    /// A node release is THREE things, not one: the binary, this launcher, and
    /// the founding set the binary founds and joins from (no binary carries
    /// wasm). All of them land in the release directory, so the flipped
    /// release is complete on a host that has nothing else.
    #[test]
    fn a_release_carries_the_launcher_and_the_founding_set_beside_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("release.tar.zst");
        let bytes = tar_zst(&[
            ("ducktape", b"#!/bin/sh\nexit 0\n", 0o755),
            ("ducktape-node-launcher", b"#!/bin/sh\nexit 0\n", 0o755),
            ("modules/netstack.component.wasm", b"\0asm", 0o644),
            ("modules/.staged-by", b"abc1234", 0o644),
        ]);
        fs::write(&archive, &bytes).unwrap();
        let release = dir.path().join("r");
        stage(&archive, Sha::digest(&bytes), &release).unwrap();
        assert!(release.join("ducktape-node-launcher").exists());
        assert!(release.join("modules/netstack.component.wasm").exists());
        // the set's owner record rides along: the binary beside it refuses a
        // set another build staged, so a release without it refuses itself.
        assert!(release.join("modules/.staged-by").exists());
    }

    #[test]
    fn the_archive_identity_and_its_entries_are_both_checked() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("release.tar.zst");
        let bytes = tar_zst(&[("ducktape", b"x", 0o755)]);
        fs::write(&archive, &bytes).unwrap();
        let release = dir.path().join("r");

        let wrong = Sha::digest(b"not this archive");
        assert_eq!(
            stage(&archive, wrong, &release).unwrap_err().reason,
            "sha256_mismatch"
        );

        // an entry that would leave the release directory is refused by name.
        // (`tar::Builder` will not even write one, so the destination rule is
        // exercised where it is enforced.)
        assert_eq!(
            destination(&release, Path::new("../ducktape"))
                .unwrap_err()
                .reason,
            "archive_path_escapes"
        );
        assert_eq!(
            destination(&release, Path::new("/etc/ducktape"))
                .unwrap_err()
                .reason,
            "archive_path_escapes"
        );
        assert_eq!(
            destination(&release, Path::new("./ducktape")).unwrap(),
            release.join("ducktape")
        );

        // and one that carries no `ducktape` at all is not a node release.
        let empty = tar_zst(&[("readme", b"x", 0o644)]);
        fs::write(&archive, &empty).unwrap();
        assert_eq!(
            stage(&archive, Sha::digest(&empty), &release)
                .unwrap_err()
                .reason,
            "release_incomplete"
        );
    }

    /// ONE `run` per workspace. The second claim is refused by name and takes
    /// nothing with it; the workspace is claimable again the moment the first
    /// is released. (`flock` is per open file description, so two claims in
    /// one process contend exactly as two launchers do.)
    #[test]
    fn a_workspace_holds_one_supervisor_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::of(dir.path());
        let path = layout.lock_path();

        let held = claim(&path).expect("the first run claims the workspace");
        assert_eq!(
            claim(&path).unwrap_err().reason,
            "workspace_locked",
            "a second run on a claimed workspace is refused"
        );
        drop(held);
        claim(&path).expect("a released workspace is claimable again");
    }

    #[test]
    fn collect_keeps_exactly_what_it_is_told() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::of(dir.path());
        let keep = Sha::digest(b"keep");
        let drop = Sha::digest(b"drop");
        fs::create_dir_all(layout.release_dir(keep)).unwrap();
        fs::create_dir_all(layout.release_dir(drop)).unwrap();
        fs::write(layout.partial(drop), b"half a download").unwrap();
        collect(&layout, &[keep]);
        assert!(layout.release_dir(keep).exists());
        assert!(!layout.release_dir(drop).exists());
        assert!(!layout.partial(drop).exists());
    }
}
