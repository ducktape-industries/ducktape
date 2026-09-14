//! the run workspace as a block device.
//!
//! Firecracker's device model is deliberately minimal — virtio-block, -net,
//! -vsock, -balloon, -rng and a serial console, nothing else. There is no
//! virtio-fs, so the workspace is handed over as a per-run ext4 image: built
//! from the workdir before boot, mounted by the guest init, and read back after
//! the guest reports its exit code.
//!
//! Both directions are ROOTLESS and mount-free. `mke2fs -d` populates an image
//! from a directory without ever mounting it, and `debugfs -R rdump` walks one
//! back out. That matters: a loop mount would need root, and a node that needs
//! root to move a workspace is a node that runs as root.
//!
//! The round trip is CPU-bound, not I/O-bound — measured at 186 MB/s and 8,860
//! files/s against an NVMe that sustains 2.9 GB/s, with `mke2fs` at 99% of one
//! core. Do not reach for faster storage to speed this up; the spec's *Build
//! caches* section records the measurement and the conclusion drawn from it.

use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// the floor: ext4 metadata plus a journal does not fit in a few hundred KiB,
/// and `mke2fs` silently drops the journal below ~16 MiB ("Filesystem too small
/// for a journal"). A journal-less workspace image is a torn tree after a hard
/// VM kill, so the floor is above that threshold rather than at it.
pub const MIN_WORKSPACE_BYTES: u64 = 32 * 1024 * 1024;

/// the ceiling, refused before a VM boots. A run whose workspace does not fit
/// cannot be salvaged by retrying, so it must fail at submit-adjacent time.
pub const MAX_WORKSPACE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Spare room over the input tree for filesystem metadata.
/// ext4's own metadata (inodes, bitmaps, the journal) does not fit
/// in the file bytes alone, and `mke2fs -d` fails with `No space left` if the
/// image is sized at exactly the payload.
const IMAGE_METADATA_MARGIN_PERCENT: u64 = 20;

/// Small images need a fixed allowance too: near 128 MiB the default ext4
/// journal takes 16 MiB, and inode tables take roughly another 1/16 of the
/// image. A 20% margin over a ~100 MiB payload cannot hold both. Reserve
/// 32 MiB until the proportional allowance is larger.
const MIN_IMAGE_METADATA_BYTES: u64 = 32 * 1024 * 1024;

/// A writable tree gets the existing byte limit as sparse capacity. Input size
/// does not predict output size: a small source checkout can produce a much
/// larger build. Only written blocks consume host disk; the guest filesystem
/// enforces the limit when a run writes.
pub fn sized_for(workdir: &Path) -> Result<u64, String> {
    let measured = tree_bytes(workdir)?;
    size_or_refuse("workspace", workdir, with_metadata(measured))?;
    Ok(MAX_WORKSPACE_BYTES)
}

fn with_metadata(measured: u64) -> u64 {
    let margin = (measured / 100 * IMAGE_METADATA_MARGIN_PERCENT).max(MIN_IMAGE_METADATA_BYTES);
    measured.saturating_add(margin).max(MIN_WORKSPACE_BYTES)
}

/// the image size for a READ-ONLY tree: measured plus a metadata margin, and
/// no headroom at all.
///
/// Tripling a read-only image was not merely wasteful — it decided runs. A
/// run's read-only inputs are its PATH commands, and on a machine where those
/// include a build directory they measure gigabytes; at ×3 the same tree
/// crossed the cap and the run was REFUSED for a size two thirds of which was
/// zeroes nothing could ever write to.
pub fn sized_for_read_only(dir: &Path) -> Result<u64, String> {
    let measured = tree_bytes(dir)?;
    size_or_refuse("read-only inputs", dir, with_metadata(measured))
}

/// the size decision alone, split out so the refusal is unit-testable without
/// materialising gigabytes on disk.
///
/// `what` and `dir` are in the message because there are two images and one
/// used to say "workspace" for both — sending a reader to inspect a 4 KiB
/// workspace while the 3 GB asset tree that actually blew the cap went
/// unnamed.
pub fn size_or_refuse(what: &str, dir: &Path, size: u64) -> Result<u64, String> {
    if size > MAX_WORKSPACE_BYTES {
        // countable, because "this node refuses every run" and "this one tree
        // is too big" look identical from the outside until you can count them.
        tracing::warn!(
            target: "ducktape::sandbox",
            reason = "image_over_cap",
            what,
            size,
            cap = MAX_WORKSPACE_BYTES,
            "refusing to build an image over the cap"
        );
        return Err(format!(
            "the {what} at {} need {size} bytes of image, over the \
             {MAX_WORKSPACE_BYTES}-byte cap",
            dir.display()
        ));
    }
    Ok(size)
}

fn tree_bytes(dir: &Path) -> Result<u64, String> {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let entries = std::fs::read_dir(&next)
            .map_err(|e| format!("measure workspace {}: {e}", next.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("measure workspace: {e}"))?;
            let meta = entry
                .metadata()
                .map_err(|e| format!("measure {}: {e}", entry.path().display()))?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}

/// build an ext4 image of exactly `bytes` populated from `workdir`.
pub fn build(workdir: &Path, image: &Path, bytes: u64) -> Result<(), String> {
    let started = std::time::Instant::now();
    let tool = crate::host_tools::find_system_tool("mke2fs")
        .ok_or_else(|| "mke2fs is not on PATH; install e2fsprogs".to_string())?;
    let blocks = bytes.div_ceil(4096);
    let out = Command::new(&tool)
        .args(["-q", "-t", "ext4", "-b", "4096", "-d"])
        .arg(workdir)
        .arg(image)
        .arg(blocks.to_string())
        .output()
        .map_err(|e| format!("run mke2fs: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "mke2fs exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // the other half of a run's fixed cost, beside the copy back.
    tracing::debug!(
        target: "ducktape::sandbox",
        bytes,
        build_ms = started.elapsed().as_millis() as u64,
        "workspace image built"
    );
    Ok(())
}

/// walk `image` back out, REPLACING `dest` with the result.
///
/// `rdump` only ever adds: it copies the image's entries into its target and
/// never removes an entry the target has that the image lacks. Dumping
/// straight into `dest` would union the pre-run tree with the post-run one —
/// every file a run deleted or renamed away would silently come back. So the
/// dump lands in a fresh sibling directory first, and only once it succeeds
/// does `dest` get replaced by a rename swap: a failed `rdump` leaves the
/// pre-run tree at `dest` untouched instead of half-merged.
pub fn read_back(image: &Path, dest: &Path) -> Result<(), String> {
    let tool = crate::host_tools::find_system_tool("debugfs")
        .ok_or_else(|| "debugfs is not on PATH; install e2fsprogs".to_string())?;
    let fresh = sibling(dest, "readback");
    let _ = std::fs::remove_dir_all(&fresh);
    std::fs::create_dir_all(&fresh).map_err(|e| format!("create {}: {e}", fresh.display()))?;
    // `rdump / <fresh>` lands the image root's entries DIRECTLY in fresh — no
    // wrapper directory. Verified on a real tree: the round trip is
    // byte-identical, modes included, once `lost+found` is dropped.
    let out = Command::new(&tool)
        .arg("-R")
        .arg(format!("rdump / {}", fresh.display()))
        .arg(image)
        .output()
        .map_err(|e| format!("run debugfs: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&fresh);
        return Err(format!(
            "debugfs exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    drop_lost_found(&fresh)?;
    swap_in(dest, &fresh)
}

/// `<dest>-<tag>`, alongside `dest` rather than inside it so the swap below is
/// a same-filesystem rename, not a cross-device copy.
fn sibling(dest: &Path, tag: &str) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".{tag}"));
    dest.with_file_name(name)
}

/// replace `dest` with `fresh`: `dest` → `<dest>.pre`, `fresh` → `dest`,
/// then drop `.pre`. A failure past this point leaves `.pre` on disk rather
/// than losing the pre-run tree.
fn swap_in(dest: &Path, fresh: &Path) -> Result<(), String> {
    let pre = sibling(dest, "pre");
    let _ = std::fs::remove_dir_all(&pre);
    if dest.exists() {
        std::fs::rename(dest, &pre).map_err(|e| format!("stash {}: {e}", dest.display()))?;
    }
    std::fs::rename(fresh, dest).map_err(|e| format!("install {}: {e}", dest.display()))?;
    let _ = std::fs::remove_dir_all(&pre);
    Ok(())
}

/// build the per-run READ-ONLY asset image: the context doc, the skills tree,
/// and any host PATH directories the run declared.
///
/// Its layout mirrors the guest's: entry N of `assets` lands at `ro<N>/`, and a
/// FILE lands at the image root under its own name — one level above the
/// workspace, so a `workspace-parent` context doc still resolves as
/// `../<name>`. An empty `workspace/` directory is always created, because the
/// workspace image is mounted on top of it.
///
/// Never read back: nothing the run writes here survives, which is the point of
/// it being a separate device rather than part of the workspace. Handing the
/// skills tree back to the buyer as if the run had produced it would be wrong,
/// and on a second round trip it would nest.
pub fn build_assets(assets: &[GuestAsset], image: &Path, staging: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(staging);
    std::fs::create_dir_all(staging.join("workspace"))
        .map_err(|e| format!("stage {}: {e}", staging.display()))?;

    for (index, asset) in assets.iter().enumerate() {
        let target = staging.join(format!("ro{index}"));
        match asset {
            GuestAsset::Commands(source) => stage_commands(source, &target)?,
            GuestAsset::Whole(source) => stage_whole(source, staging, &target)?,
        }
    }

    // Removed on EVERY exit, not just the happy one. Staged inputs are a copy
    // of what already exists elsewhere, and the tree runs to gigabytes when a
    // PATH entry is a build directory — a `?` straight to the caller left one
    // behind per failed run, and they accumulate under the run root until
    // something notices the disk. Measured: 21 GB across one afternoon's runs.
    let built = sized_for_read_only(staging).and_then(|size| build(staging, image, size));
    let _ = std::fs::remove_dir_all(staging);
    built
}

/// one read-only input, and HOW MUCH of it crosses into the guest.
///
/// The distinction exists because a VM copies where a container bind-mounted.
/// Under a bind mount the size of a declared directory cost nothing, so nobody
/// had to think about it; a copy makes it the run's latency and the node's
/// disk, per run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestAsset {
    /// a PATH entry. Only the executables at its TOP LEVEL cross.
    ///
    /// That is not a heuristic, it is what a PATH entry means: resolution never
    /// recurses into a subdirectory and never resolves a non-executable, so
    /// nothing else in the tree is nameable from inside the guest. Measured on
    /// this repo's own node binary, whose PATH entry is `target/debug`: a 39 GB
    /// tree holding exactly ONE file — 0.95 GB — that a run could ever invoke.
    /// Copying the tree filled a 9.1 GB tmpfs and failed the run with
    /// `No space left on device` while copying an `.rlib`.
    Commands(PathBuf),
    /// a tree the run reads, or a single file: the skills root, the assembled
    /// context doc. Copied entire — every byte of these is readable input.
    Whole(PathBuf),
}

impl GuestAsset {
    pub fn path(&self) -> &Path {
        match self {
            Self::Commands(path) | Self::Whole(path) => path,
        }
    }
}

/// stage the executables a PATH entry actually offers, and nothing else.
fn stage_commands(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("create {}: {e}", to.display()))?;
    let entries = std::fs::read_dir(from).map_err(|e| format!("read {}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read {}: {e}", from.display()))?;
        let source = entry.path();
        // metadata(), not symlink_metadata(): a PATH directory of symlinks into
        // a versioned store is the normal shape, and the target is the command.
        let Ok(meta) = std::fs::metadata(&source) else {
            continue;
        };
        let is_command = meta.is_file() && meta.permissions().mode() & 0o111 != 0;
        if !is_command {
            continue;
        }
        let target = to.join(entry.file_name());
        std::fs::copy(&source, &target).map_err(|e| format!("copy {}: {e}", source.display()))?;
    }
    Ok(())
}

/// stage a whole input: a directory tree at `to`, or a file at the image root.
///
/// A file lands one level ABOVE the workspace under its own name, so a
/// `workspace-parent` context doc still resolves as `../<name>`.
fn stage_whole(source: &Path, staging: &Path, to: &Path) -> Result<(), String> {
    let meta =
        std::fs::metadata(source).map_err(|e| format!("stat asset {}: {e}", source.display()))?;
    if !meta.is_file() {
        return copy_tree(source, to);
    }
    let name = source
        .file_name()
        .ok_or_else(|| format!("asset {} has no file name", source.display()))?;
    std::fs::copy(source, staging.join(name))
        .map_err(|e| format!("stage asset {}: {e}", source.display()))?;
    Ok(())
}

/// how deep a staged tree may nest before the rest of it is dropped. A skills
/// tree is a handful of levels; the cap is what keeps a hostile one off the
/// daemon's stack.
const MAX_TREE_DEPTH: usize = 64;

/// does `target`, read off the symlink at `entry`, resolve back inside `root`?
///
/// The tree being staged is a consensus-published duckfs checkout, so its
/// symlink targets are written by the BUYER, not the operator. An absolute
/// target names a host path (the operator's keystore, their ssh keys) and a
/// `..` target walks out of the tree to the same effect; only a link that lands
/// back inside the staged root means anything in the guest anyway, since that
/// is the only tree the guest gets.
///
/// Purely lexical, and that is the point: it decides without touching the disk,
/// so a target that does not exist yet (or races) cannot talk it into a yes.
fn link_stays_inside(root: &Path, entry: &Path, target: &Path) -> bool {
    use std::path::Component;
    if target.is_absolute() {
        return false;
    }
    let Ok(relative) = entry.strip_prefix(root) else {
        return false;
    };
    // start where the link itself lives, then walk its target from there.
    let mut walked: Vec<Component<'_>> = match relative.parent() {
        Some(parent) => parent.components().collect(),
        None => Vec::new(),
    };
    for part in target.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if walked.pop().is_none() {
                    return false;
                }
            }
            Component::Normal(name) => walked.push(Component::Normal(name)),
            // a root or a windows prefix is an absolute target by another name.
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// what a walk of one staged tree refused, so the refusals are counted once
/// rather than warned per entry: a tree of a thousand escaping links would
/// otherwise evict the log ring that holds the run around it.
struct StagedTree {
    root: PathBuf,
    /// (device, inode) of every directory already descended into. A cycle
    /// cannot be built out of symlinks any more — none are followed — but a
    /// walk that terminates by construction is cheaper than one that argues.
    seen: std::collections::HashSet<(u64, u64)>,
    escaping_links: u64,
    too_deep: u64,
}

/// copy a directory tree, preserving mode bits.
///
/// Symlinks are NEVER followed: `symlink_metadata` decides every entry, and a
/// link is recreated as a link only when its target stays inside the tree.
/// Anything else — an absolute target, a `..` that escapes — is dropped, since
/// following it would copy a host file the publisher named into an image the
/// run reads.
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    let mut tree = StagedTree {
        root: from.to_path_buf(),
        seen: std::collections::HashSet::new(),
        escaping_links: 0,
        too_deep: 0,
    };
    copy_dir(&mut tree, from, to, 0)?;
    if tree.escaping_links > 0 {
        tracing::warn!(
            target: "ducktape::sandbox",
            reason = "asset_link_escapes_root",
            tree = %tree.root.display(),
            count = tree.escaping_links,
            "dropped symlinks whose target left the staged tree"
        );
    }
    if tree.too_deep > 0 {
        tracing::warn!(
            target: "ducktape::sandbox",
            reason = "asset_tree_too_deep",
            tree = %tree.root.display(),
            count = tree.too_deep,
            depth = MAX_TREE_DEPTH,
            "dropped subtrees below the depth cap"
        );
    }
    Ok(())
}

fn copy_dir(tree: &mut StagedTree, from: &Path, to: &Path, depth: usize) -> Result<(), String> {
    if depth > MAX_TREE_DEPTH {
        tree.too_deep += 1;
        return Ok(());
    }
    std::fs::create_dir_all(to).map_err(|e| format!("create {}: {e}", to.display()))?;
    let entries = std::fs::read_dir(from).map_err(|e| format!("read {}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read {}: {e}", from.display()))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let meta = match std::fs::symlink_metadata(&source) {
            Ok(meta) => meta,
            // an entry that vanished mid-walk is not worth failing a run
            Err(_) => continue,
        };
        if meta.is_symlink() {
            let Ok(link) = std::fs::read_link(&source) else {
                continue;
            };
            if !link_stays_inside(&tree.root, &source, &link) {
                tree.escaping_links += 1;
                continue;
            }
            std::os::unix::fs::symlink(&link, &target)
                .map_err(|e| format!("link {}: {e}", target.display()))?;
            continue;
        }
        if meta.is_dir() {
            let already_walked = !tree.seen.insert((meta.dev(), meta.ino()));
            if already_walked {
                continue;
            }
            copy_dir(tree, &source, &target, depth + 1)?;
            continue;
        }
        // a fifo, socket or device node is not an input: opening one blocks the
        // stage forever, and none of them mean anything to the guest.
        if !meta.is_file() {
            continue;
        }
        std::fs::copy(&source, &target).map_err(|e| format!("copy {}: {e}", source.display()))?;
    }
    Ok(())
}

/// `lost+found` is an ext4 artifact `mke2fs` creates, not something the run
/// produced. Handing it back would add a directory to the buyer's workspace
/// that nobody put there — and on a second round trip it would persist and
/// multiply.
fn drop_lost_found(dest: &Path) -> Result<(), String> {
    let stray = dest.join("lost+found");
    match std::fs::remove_dir_all(&stray) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {}: {e}", stray.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ducktape-wsimg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn have_e2fsprogs() -> bool {
        crate::host_tools::find_system_tool("mke2fs").is_some()
            && crate::host_tools::find_system_tool("debugfs").is_some()
    }

    /// The round trip is the whole contract: what the guest writes has to come
    /// A PATH entry offers exactly the commands at its top level. Copying the
    /// tree instead is not merely wasteful, it is unbounded: the node's own
    /// binary sits in a build directory whose tree measured 39 GB against the
    /// 0.95 GB of it a run can name, and copying it filled a tmpfs and failed
    /// the run with `No space left on device`.
    #[test]
    fn a_path_entry_hands_over_its_commands_and_nothing_else() {
        let root = scratch("path-entry");
        let from = root.join("bin");
        std::fs::create_dir_all(from.join("deps")).expect("tree");
        std::fs::write(from.join("tool"), b"#!/bin/sh\n").expect("command");
        std::fs::set_permissions(from.join("tool"), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        std::fs::write(from.join("notes.txt"), b"not a command").expect("data file");
        std::fs::write(from.join("deps").join("huge.rlib"), b"not reachable").expect("nested");

        let to = root.join("staged");
        stage_commands(&from, &to).expect("stage");

        let mut staged: Vec<String> = std::fs::read_dir(&to)
            .expect("read staged")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        staged.sort();
        assert_eq!(
            staged,
            vec!["tool".to_string()],
            "PATH resolution never recurses and never resolves a non-executable"
        );
        let mode = std::fs::metadata(to.join("tool"))
            .expect("stat")
            .permissions()
            .mode();
        assert!(mode & 0o111 != 0, "it must still be executable: {mode:o}");
        std::fs::remove_dir_all(root).ok();
    }

    /// back byte-identical, with the mode bits intact — an agent run that
    /// produces an executable must not hand back a non-executable one.
    #[test]
    fn a_workspace_survives_the_round_trip_with_modes_and_nesting() {
        use std::os::unix::fs::PermissionsExt as _;
        if !have_e2fsprogs() {
            return;
        }
        let root = scratch("round-trip");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("nested/deeper")).expect("nested");
        std::fs::write(src.join("plain.txt"), b"hello workspace").expect("plain");
        std::fs::write(src.join("nested/deeper/leaf.bin"), [0u8, 1, 2, 255]).expect("leaf");
        let script = src.join("run.sh");
        std::fs::write(&script, b"#!/bin/sh\necho hi\n").expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let image = root.join("ws.img");
        let size = sized_for(&src).expect("size");
        build(&src, &image, size).expect("build");

        let out = root.join("out");
        read_back(&image, &out).expect("read back");

        assert_eq!(
            std::fs::read(out.join("plain.txt")).expect("plain back"),
            b"hello workspace"
        );
        assert_eq!(
            std::fs::read(out.join("nested/deeper/leaf.bin")).expect("leaf back"),
            [0u8, 1, 2, 255]
        );
        let mode = std::fs::metadata(out.join("run.sh"))
            .expect("script back")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "the executable bit must survive");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `lost+found` is the filesystem's, not the run's. It must never appear in
    /// a workspace handed back to a buyer.
    #[test]
    fn the_read_back_does_not_hand_back_lost_and_found() {
        if !have_e2fsprogs() {
            return;
        }
        let root = scratch("lost-found");
        let src = root.join("src");
        std::fs::create_dir_all(&src).expect("src");
        std::fs::write(src.join("only.txt"), b"x").expect("only");

        let image = root.join("ws.img");
        build(&src, &image, sized_for(&src).expect("size")).expect("build");
        let out = root.join("out");
        read_back(&image, &out).expect("read back");

        let mut names: Vec<String> = std::fs::read_dir(&out)
            .expect("list")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["only.txt".to_string()]);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The regression this fix closes: `read_back` must REPLACE `dest`, not
    /// merge into it. The image is built from a tree where the guest already
    /// deleted `b` (a `git rm`, a "remove dead code" run); `dest` is the
    /// ORIGINAL pre-run checkout, which still has `a` and `b` both. A naive
    /// `rdump` into `dest` only adds entries, so `b` would survive the round
    /// trip even though the guest removed it — read_back must make `dest`
    /// match the image exactly.
    #[test]
    fn read_back_replaces_the_destination_instead_of_merging() {
        if !have_e2fsprogs() {
            return;
        }
        let root = scratch("replace-not-merge");

        // The pre-run checkout: what `dest` looks like before the run.
        let dest = root.join("dest");
        std::fs::create_dir_all(&dest).expect("dest");
        std::fs::write(dest.join("a.txt"), b"keep").expect("a");
        std::fs::write(dest.join("b.txt"), b"delete me").expect("b");

        // The post-run guest tree: same as dest, minus `b` — the guest deleted
        // it during the run. This is what gets built into the image.
        let guest = root.join("guest");
        std::fs::create_dir_all(&guest).expect("guest");
        std::fs::write(guest.join("a.txt"), b"keep").expect("a");

        let image = root.join("ws.img");
        build(&guest, &image, sized_for(&guest).expect("size")).expect("build");

        read_back(&image, &dest).expect("read back");

        let mut names: Vec<String> = std::fs::read_dir(&dest)
            .expect("list")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["a.txt".to_string()],
            "b.txt must not come back: read_back replaces dest, it does not merge into it"
        );
        assert_eq!(std::fs::read(dest.join("a.txt")).expect("a back"), b"keep");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A tree over the cap cannot be salvaged by retrying, so it is refused
    /// before anything is materialised on disk — and the refusal NAMES which
    /// of the run's two images it is about. There are two, they blow the cap
    /// for entirely different reasons, and a message that said "workspace" for
    /// both sent a reader to inspect a 4 KiB directory while the 3 GB asset
    /// tree that actually refused went unnamed.
    #[test]
    fn an_oversized_tree_is_refused_before_any_image_exists_and_names_itself() {
        let refused = size_or_refuse(
            "read-only inputs",
            Path::new("/run/assets"),
            MAX_WORKSPACE_BYTES + 1,
        )
        .expect_err("must refuse");
        assert!(refused.contains("over the"), "{refused}");
        assert!(
            refused.contains("read-only inputs"),
            "names the tree: {refused}"
        );
        assert!(
            refused.contains("/run/assets"),
            "names the directory: {refused}"
        );

        size_or_refuse("workspace", Path::new("/run/ws"), MAX_WORKSPACE_BYTES)
            .expect("the cap itself is allowed");
    }

    /// A read-only image gets a metadata margin, without writable capacity.
    ///
    /// This decided real runs: a run's read-only inputs are its PATH commands,
    /// and where those include a build directory they measure gigabytes. At ×3
    /// the same tree crossed the cap and the run was refused over a size two
    /// thirds of which was zeroes nothing could ever write to.
    #[test]
    fn a_read_only_image_is_not_sized_for_writes_that_cannot_happen() {
        let root = scratch("ro-size");
        // sparse and well over MIN_WORKSPACE_BYTES, so the FLOOR is not what
        // either answer is — the measurement is. `tree_bytes` reads the logical
        // length, so this costs no disk.
        let payload: u64 = 512 * 1024 * 1024;
        std::fs::File::create(root.join("blob"))
            .expect("create")
            .set_len(payload)
            .expect("set_len");

        let writable = sized_for(&root).expect("writable size");
        let read_only = sized_for_read_only(&root).expect("read-only size");
        assert_eq!(
            writable, MAX_WORKSPACE_BYTES,
            "writable has room for outputs"
        );
        assert!(
            read_only < writable,
            "read-only {read_only} must be under writable {writable}"
        );
        assert!(
            read_only >= payload,
            "…but still hold the payload: {read_only} < {payload}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn metadata_allowance_has_a_floor_without_changing_large_image_sizing() {
        let mib = 1024 * 1024;
        assert_eq!(with_metadata(0), MIN_WORKSPACE_BYTES);
        assert_eq!(with_metadata(1), 1 + 32 * mib);
        assert_eq!(with_metadata(108 * mib), 140 * mib);
        let large_payload = 512 * mib;
        assert_eq!(
            with_metadata(large_payload),
            large_payload + large_payload / 100 * 20
        );
        assert_eq!(with_metadata(u64::MAX), u64::MAX);
    }

    /// A dense executable around 100 MiB needs more than 20% overhead: the
    /// journal alone takes 16 MiB, before the inode tables and block bitmaps.
    #[test]
    fn a_dense_read_only_payload_fits_with_filesystem_metadata() {
        use std::io::{Read as _, Write as _};
        if !have_e2fsprogs() {
            return;
        }
        let root = scratch("ro-dense");
        let src = root.join("src");
        std::fs::create_dir(&src).expect("src");
        let mut file = std::fs::File::create(src.join("pi")).expect("payload");
        // Nonzero bytes force mke2fs to allocate every payload block; a sparse
        // file would pass even when the image cannot hold a real executable.
        // 108 MiB puts the old 20%-margin image above the journal's size step.
        let block = [7u8; 64 * 1024];
        let block_count = 108 * 1024 * 1024 / block.len();
        for _ in 0..block_count {
            file.write_all(&block).expect("write payload");
        }
        drop(file);

        let image = root.join("assets.img");
        let size = sized_for_read_only(&src).expect("read-only size");
        build(&src, &image, size).expect("dense payload must fit");
        let out = root.join("out");
        read_back(&image, &out).expect("read back");
        let mut restored = std::fs::File::open(out.join("pi")).expect("restored payload");
        assert_eq!(restored.metadata().expect("stat").len(), 108 * 1024 * 1024);
        let mut buffer = [0u8; 64 * 1024];
        for _ in 0..block_count {
            restored.read_exact(&mut buffer).expect("read payload");
            assert_eq!(buffer, block, "payload bytes must survive");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A small input must have room for a build independent of its input size.
    #[test]
    fn an_empty_workspace_can_produce_a_large_output_without_preallocating_it() {
        use std::io::Write as _;
        if !have_e2fsprogs() {
            return;
        }
        let root = scratch("floor");
        let src = root.join("src");
        std::fs::create_dir(&src).unwrap();
        let image = root.join("workspace.img");
        assert_eq!(sized_for(&src).expect("size"), MAX_WORKSPACE_BYTES);
        build(&src, &image, sized_for(&src).unwrap()).unwrap();
        let meta = std::fs::metadata(&image).unwrap();
        assert!(
            meta.blocks() * 512 < meta.len() / 2,
            "capacity must be sparse"
        );
        let payload = root.join("output");
        let mut file = std::fs::File::create(&payload).unwrap();
        let block = [7u8; 64 * 1024];
        for _ in 0..1024 {
            file.write_all(&block).unwrap();
        }
        drop(file);
        let result = Command::new(crate::host_tools::find_system_tool("debugfs").unwrap())
            .current_dir(&root)
            .args(["-w", "-R", "write output /grown"])
            .arg(&image)
            .output()
            .unwrap();
        assert!(result.status.success());
        let out = root.join("readback");
        read_back(&image, &out).unwrap();
        assert_eq!(
            std::fs::read(out.join("grown")).unwrap(),
            std::fs::read(payload).unwrap()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The staged tree is a duckfs checkout, so its links are the publisher's.
    /// A link out of the tree must not become a COPY of what it named: that is
    /// how the operator's keystore ends up in a buyer's run.
    #[test]
    fn a_staged_link_out_of_the_tree_is_dropped_not_followed() {
        let root = scratch("escaping-links");
        let secret = root.join("secret.key");
        std::fs::write(&secret, b"operator key material").expect("secret");
        let from = root.join("skills");
        std::fs::create_dir_all(from.join("sub")).expect("tree");
        std::fs::write(from.join("real.md"), b"a skill").expect("file");
        std::os::unix::fs::symlink(&secret, from.join("absolute")).expect("absolute link");
        std::os::unix::fs::symlink("../../secret.key", from.join("sub/escape"))
            .expect("dotdot link");
        std::os::unix::fs::symlink("../real.md", from.join("sub/inside")).expect("in-tree link");

        let to = root.join("staged");
        copy_tree(&from, &to).expect("stage");

        assert!(
            to.join("absolute").symlink_metadata().is_err(),
            "an absolute target names a host path, never a staged one"
        );
        assert!(
            to.join("sub/escape").symlink_metadata().is_err(),
            "a `..` target leaves the tree just as an absolute one does"
        );
        let staged_link = to
            .join("sub/inside")
            .symlink_metadata()
            .expect("in-tree link stays");
        assert!(
            staged_link.is_symlink(),
            "an in-tree link is preserved AS a link, not copied"
        );
        assert_eq!(
            std::fs::read_link(to.join("sub/inside")).expect("target"),
            Path::new("../real.md")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `loop -> ..` used to recurse until the daemon's stack or its disk gave
    /// out, because the walk resolved it into a directory. Nothing is followed
    /// now: the link is staged AS a link and the walk ends.
    #[test]
    fn a_self_referential_link_terminates_the_walk() {
        let root = scratch("cycle");
        let from = root.join("skills");
        std::fs::create_dir_all(from.join("deep")).expect("tree");
        std::fs::write(from.join("deep/leaf.md"), b"leaf").expect("leaf");
        std::os::unix::fs::symlink("..", from.join("deep/loop")).expect("cycle link");

        let to = root.join("staged");
        copy_tree(&from, &to).expect("stage terminates");

        assert!(
            to.join("deep/leaf.md").is_file(),
            "the real tree still lands"
        );
        let staged = to
            .join("deep/loop")
            .symlink_metadata()
            .expect("the cycle is staged");
        assert!(
            staged.is_symlink(),
            "a link back to the tree root is a link, never a copy of the tree"
        );
        assert_eq!(
            std::fs::read_link(to.join("deep/loop")).expect("target"),
            Path::new(".."),
            "the target is recreated verbatim, not walked"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The predicate on its own: it is lexical, so it answers without the disk.
    #[test]
    fn link_stays_inside_admits_only_relative_in_tree_targets() {
        let root = Path::new("/stage/skills");
        let entry = root.join("sub/link");
        assert!(link_stays_inside(root, &entry, Path::new("sibling.md")));
        assert!(link_stays_inside(root, &entry, Path::new("../top.md")));
        assert!(link_stays_inside(
            root,
            &entry,
            Path::new("./deeper/../ok.md")
        ));
        assert!(!link_stays_inside(
            root,
            &entry,
            Path::new("../../outside.md")
        ));
        assert!(!link_stays_inside(
            root,
            &entry,
            Path::new("/home/op/.ducktape")
        ));
    }
}
