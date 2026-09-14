//! Staging a downloaded release on disk: the archive out of
//! `releases/<sha>.partial` into `releases/<sha>/` (the FINAL directory, never
//! a temp elsewhere — what is verified is what is flipped), the bundle check
//! that makes it trusted, the seal that keeps it that way, and the collector
//! that removes what no phase names any more.
//!
//! The archive holds the release directory's contents at its top level:
//! Linux `ducktape-launcher`, `ducktape-app`, `views/*.wasm`; macOS
//! `Ducktape.app/…`. Extraction accepts regular files, directories and
//! symlinks whose target stays under the release directory (the bundle's
//! `Contents/MacOS/views -> ../Resources/ice-bundle/views` is one); it refuses
//! absolute paths, `..`, a symlink that would leave the directory, and hard
//! links. Every refusal is a stable snake_case token: it is the banner's
//! "last refusal" and the log's `reason`.

use std::path::{Component, Path, PathBuf};

use app_update::Sha;
use tracing::{debug, info, warn};

pub(crate) const APP_EXE: &str = "ducktape-app";
pub(crate) const LAUNCHER_EXE: &str = "ducktape-launcher";
const VIEWS_DIR: &str = "views";
const BUNDLE: &str = "Ducktape.app";
const PARTIAL_SUFFIX: &str = ".partial";

/// `Verify`: hash the archive again, extract it into `release_dir`, check
/// the release is whole (and, on macOS, that its bundle signature holds).
/// `release_dir` is replaced whole if something is already there — a dir at
/// a sha the machine has not verified is a leftover, never a release.
pub(crate) fn stage(archive: &Path, sha: Sha, release_dir: &Path) -> Result<(), String> {
    let landed = digest_file(archive).map_err(|_| "io")?;
    let matches = landed == sha;
    if !matches {
        return Err("sha256_mismatch".into());
    }
    clear_release_dir(release_dir)?;
    extract(archive, release_dir)?;
    require_whole(release_dir)?;
    check_bundle(release_dir)?;
    Ok(())
}

fn digest_file(path: &Path) -> std::io::Result<Sha> {
    use sha2::Digest as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(Sha::from_bytes(hasher.finalize().into()))
}

/// A release dir is a real directory the app creates: a link at its path
/// is refused, a leftover directory is unsealed and removed.
fn clear_release_dir(release_dir: &Path) -> Result<(), String> {
    let Ok(meta) = std::fs::symlink_metadata(release_dir) else {
        return Ok(());
    };
    if meta.file_type().is_symlink() {
        return Err("release_dir_is_a_link".into());
    }
    unseal(release_dir);
    std::fs::remove_dir_all(release_dir).map_err(|_| "io".to_string())
}

fn extract(archive: &Path, release_dir: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|_| "io")?;
    let decoder = zstd::Decoder::new(file).map_err(|_| "archive_unreadable")?;
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(false);
    tar.set_preserve_ownerships(false);
    std::fs::create_dir_all(release_dir).map_err(|_| "io")?;
    let entries = tar.entries().map_err(|_| "archive_unreadable")?;
    for entry in entries {
        let mut entry = entry.map_err(|_| "archive_unreadable")?;
        let entry_path = entry.path().map_err(|_| "archive_unreadable")?.into_owned();
        let destination = entry_destination(release_dir, &entry_path)?;
        let kind = entry.header().entry_type();
        admit(kind, &destination, release_dir, &entry)?;
        entry.unpack(&destination).map_err(|_| "extract_failed")?;
    }
    Ok(())
}

/// Whether one entry, its path already inside the release dir, is a kind
/// a release is made of — and, for a symlink, one that stays inside.
fn admit<R: std::io::Read>(
    kind: tar::EntryType,
    destination: &Path,
    release_dir: &Path,
    entry: &tar::Entry<'_, R>,
) -> Result<(), String> {
    match kind {
        tar::EntryType::Regular | tar::EntryType::Directory => Ok(()),
        tar::EntryType::Symlink => {
            let target = entry
                .link_name()
                .map_err(|_| "archive_unreadable")?
                .ok_or("symlink_without_target")?;
            let stays_inside = symlink_stays_inside(release_dir, destination, &target);
            match stays_inside {
                true => Ok(()),
                false => Err("symlink_escapes".into()),
            }
        }
        tar::EntryType::Link => Err("hard_link".into()),
        tar::EntryType::Char
        | tar::EntryType::Block
        | tar::EntryType::Fifo
        | tar::EntryType::Continuous
        | tar::EntryType::GNULongName
        | tar::EntryType::GNULongLink
        | tar::EntryType::GNUSparse
        | tar::EntryType::XGlobalHeader
        | tar::EntryType::XHeader => Err("unsupported_entry".into()),
        // `EntryType` hides a `__Nonexhaustive(u8)` arm for the type bytes
        // the format does not name; none of them is a release file either.
        _ => Err("unsupported_entry".into()),
    }
}

/// Where an entry lands: under `root`, through normal components only.
fn entry_destination(root: &Path, entry_path: &Path) -> Result<PathBuf, String> {
    let mut destination = root.to_path_buf();
    for component in entry_path.components() {
        match component {
            Component::Normal(part) => destination.push(part),
            Component::CurDir => {}
            Component::ParentDir => return Err("path_escapes".into()),
            Component::RootDir | Component::Prefix(_) => return Err("path_absolute".into()),
        }
    }
    Ok(destination)
}

/// A symlink at `link` (under `root`) pointing at `target` stays inside
/// when the target is relative and, resolved lexically from the link's
/// directory, never climbs above `root`.
fn symlink_stays_inside(root: &Path, link: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let Ok(link_relative) = link.strip_prefix(root) else {
        return false;
    };
    let mut depth: Vec<&std::ffi::OsStr> = link_relative
        .parent()
        .map(|parent| {
            parent
                .components()
                .filter_map(|component| match component {
                    Component::Normal(part) => Some(part),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    for component in target.components() {
        match component {
            Component::Normal(part) => depth.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                let climbed_out = depth.pop().is_none();
                if climbed_out {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// The executables and the views the launcher's `--qualify` will look for,
/// named now so a truncated archive is a `release_incomplete`, not a later
/// `qualify` mystery.
fn require_whole(release_dir: &Path) -> Result<(), String> {
    let bin_dir = release_bin_dir(release_dir);
    let whole = [
        bin_dir.join(APP_EXE).is_file(),
        bin_dir.join(LAUNCHER_EXE).is_file(),
        bin_dir.join(VIEWS_DIR).is_dir(),
    ]
    .into_iter()
    .all(|present| present);
    match whole {
        true => Ok(()),
        false => Err("release_incomplete".into()),
    }
}

/// Where a release's executables sit: the dir itself on Linux,
/// `Ducktape.app/Contents/MacOS` on macOS.
pub(crate) fn release_bin_dir(release_dir: &Path) -> PathBuf {
    match cfg!(target_os = "macos") {
        true => release_dir.join(BUNDLE).join("Contents").join("MacOS"),
        false => release_dir.to_path_buf(),
    }
}

/// macOS: the extracted bundle's signature, deep and strict, and the
/// Gatekeeper assessment a double-click would face. What runs is what was
/// checked: the same directory is flipped into place.
#[cfg(target_os = "macos")]
fn check_bundle(release_dir: &Path) -> Result<(), String> {
    let bundle = release_dir.join(BUNDLE);
    run_check(
        "codesign",
        &["--verify", "--deep", "--strict"],
        &bundle,
        "codesign_rejected",
    )?;
    run_check("spctl", &["-a", "-t", "exec"], &bundle, "spctl_rejected")
}

#[cfg(target_os = "macos")]
fn run_check(tool: &str, args: &[&str], bundle: &Path, refusal: &str) -> Result<(), String> {
    let status = std::process::Command::new(tool)
        .args(args)
        .arg(bundle)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|_| format!("{tool}_spawn_failed"))?;
    match status.success() {
        true => Ok(()),
        false => Err(refusal.into()),
    }
}

/// Not macOS: there is no bundle signature to check; the archive's sha256
/// under the signed manifest is the whole trust.
#[cfg(not(target_os = "macos"))]
fn check_bundle(release_dir: &Path) -> Result<(), String> {
    debug!(target: "ducktape::update", event = "app_update_bundle_check_skipped", reason = "not_macos", dir = %release_dir.display());
    Ok(())
}

// ---- seal / unseal ---------------------------------------------------------

/// `chmod -R a-w`: nothing in a staged release changes after it verified.
/// Symlinks are left alone (their mode is meaningless; their target is
/// inside and gets its own visit).
pub(crate) fn seal(dir: &Path) -> std::io::Result<()> {
    walk(dir, &|path, mode| {
        let sealed = mode & !0o222;
        set_mode(path, sealed)
    })
}

/// `chmod -R u+w`, so a sealed release can be removed. Errors are ignored:
/// the remove that follows reports its own.
pub(crate) fn unseal(dir: &Path) {
    let _ = walk(dir, &|path, mode| set_mode(path, mode | 0o200));
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Every non-link entry under `dir`, children before their directory. A
/// sealed directory still reads (`r-x`) and chmod needs ownership, not a
/// writable parent, so one order serves both the seal and the unseal.
fn walk(dir: &Path, apply: &dyn Fn(&Path, u32) -> std::io::Result<()>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let meta = std::fs::symlink_metadata(dir)?;
    let is_link = meta.file_type().is_symlink();
    if is_link {
        return Ok(());
    }
    if meta.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            walk(&entry?.path(), apply)?;
        }
    }
    let mode = meta.permissions().mode() & 0o7777;
    apply(dir, mode)
}

// ---- gc --------------------------------------------------------------------

/// `Gc`: remove every `releases/<sha>` whose sha is not in `keep`, and every
/// `<sha>.partial` — after the healthy signal nothing is downloading, so a
/// partial is a leftover whatever its name. Only entries named by a sha are
/// touched; anything else in those directories is not this module's.
pub(crate) fn collect(releases_dir: &Path, partial_dir: &Path, keep: &[Sha]) {
    for (sha, path) in release_dirs(releases_dir) {
        let kept = keep.contains(&sha);
        if kept {
            continue;
        }
        unseal(&path);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                info!(target: "ducktape::update", event = "app_update_gc", sha = %sha, kind = "release")
            }
            Err(error) => {
                warn!(target: "ducktape::update", event = "app_update_gc_failed", sha = %sha, kind = "release", error = %error)
            }
        }
    }
    for (sha, path) in partials(partial_dir) {
        match std::fs::remove_file(&path) {
            Ok(()) => {
                info!(target: "ducktape::update", event = "app_update_gc", sha = %sha, kind = "partial")
            }
            Err(error) => {
                warn!(target: "ducktape::update", event = "app_update_gc_failed", sha = %sha, kind = "partial", error = %error)
            }
        }
    }
}

/// The real directories under `releases_dir` named by a sha.
fn release_dirs(releases_dir: &Path) -> Vec<(Sha, PathBuf)> {
    entries(releases_dir)
        .into_iter()
        .filter(|(_, path)| std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()))
        .filter_map(|(name, path)| name.parse::<Sha>().ok().map(|sha| (sha, path)))
        .collect()
}

/// The `<sha>.partial` files under `partial_dir`.
fn partials(partial_dir: &Path) -> Vec<(Sha, PathBuf)> {
    entries(partial_dir)
        .into_iter()
        .filter(|(_, path)| std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file()))
        .filter_map(|(name, path)| {
            let stem = name.strip_suffix(PARTIAL_SUFFIX)?;
            stem.parse::<Sha>().ok().map(|sha| (sha, path))
        })
        .collect()
}

fn entries(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            )
        })
        .collect()
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    /// A `.tar.zst` from a list of entries.
    pub(in crate::backend::update) enum Item {
        File(&'static str, Vec<u8>, u32),
        Dir(&'static str),
        Symlink(&'static str, &'static str),
        HardLink(&'static str, &'static str),
    }

    pub(in crate::backend::update) fn archive_of(items: &[Item]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for item in items {
            let mut header = tar::Header::new_gnu();
            match item {
                Item::File(path, bytes, mode) => {
                    header.set_size(bytes.len() as u64);
                    header.set_mode(*mode);
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, path, bytes.as_slice())
                        .unwrap();
                }
                Item::Dir(path) => {
                    header.set_size(0);
                    header.set_mode(0o755);
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, path, std::io::empty())
                        .unwrap();
                }
                Item::Symlink(path, target) => {
                    header.set_size(0);
                    header.set_mode(0o777);
                    header.set_entry_type(tar::EntryType::Symlink);
                    builder.append_link(&mut header, path, target).unwrap();
                }
                Item::HardLink(path, target) => {
                    header.set_size(0);
                    header.set_mode(0o644);
                    header.set_entry_type(tar::EntryType::Link);
                    builder.append_link(&mut header, path, target).unwrap();
                }
            }
        }
        let tar_bytes = builder.into_inner().unwrap();
        zstd::encode_all(tar_bytes.as_slice(), 3).unwrap()
    }

    pub(in crate::backend::update) fn whole_release() -> Vec<Item> {
        vec![
            Item::File(APP_EXE, b"app".to_vec(), 0o755),
            Item::File(LAUNCHER_EXE, b"launcher".to_vec(), 0o755),
            Item::Dir("views"),
            Item::File("views/chat.wasm", b"wasm".to_vec(), 0o644),
            Item::Symlink("views-alias", "views"),
            Item::Symlink("views/back", "../views/chat.wasm"),
        ]
    }

    /// A raw entry the `tar` builder itself refuses to write (`..`, a
    /// leading `/`): the header's name bytes set directly.
    fn raw_entry(name: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
        header.set_cksum();
        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&header, &b"x"[..]).unwrap();
        zstd::encode_all(builder.into_inner().unwrap().as_slice(), 3).unwrap()
    }

    fn staged_raw(name: &[u8]) -> Result<(), String> {
        let bytes = raw_entry(name);
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("raw.partial");
        std::fs::write(&archive, &bytes).unwrap();
        stage(&archive, Sha::digest(&bytes), &root.path().join("release"))
    }

    fn staged(items: &[Item]) -> (tempfile::TempDir, Result<(), String>) {
        let root = tempfile::tempdir().unwrap();
        let bytes = archive_of(items);
        let sha = Sha::digest(&bytes);
        let archive = root.path().join(format!("{sha}.partial"));
        std::fs::write(&archive, &bytes).unwrap();
        let outcome = stage(&archive, sha, &root.path().join("release"));
        (root, outcome)
    }

    #[test]
    fn a_whole_archive_extracts_with_its_modes_and_inside_links() {
        let (root, outcome) = staged(&whole_release());
        assert_eq!(outcome, Ok(()));
        let release = root.path().join("release");
        let app_mode = std::fs::metadata(release.join(APP_EXE))
            .unwrap()
            .permissions()
            .mode()
            & 0o111;
        assert_eq!(app_mode, 0o111, "the executable bit survives");
        assert_eq!(
            std::fs::read(release.join("views/chat.wasm")).unwrap(),
            b"wasm"
        );
        assert_eq!(
            std::fs::read_link(release.join("views-alias")).unwrap(),
            PathBuf::from("views")
        );
        assert_eq!(std::fs::read(release.join("views/back")).unwrap(), b"wasm");
    }

    #[test]
    fn the_archive_hash_is_checked_again_before_anything_lands() {
        let root = tempfile::tempdir().unwrap();
        let bytes = archive_of(&whole_release());
        let archive = root.path().join("x.partial");
        std::fs::write(&archive, &bytes).unwrap();
        let outcome = stage(
            &archive,
            Sha::digest(b"other"),
            &root.path().join("release"),
        );
        assert_eq!(outcome, Err("sha256_mismatch".into()));
        assert!(!root.path().join("release").exists());
    }

    #[test]
    fn a_parent_component_is_refused() {
        assert_eq!(staged_raw(b"../escape"), Err("path_escapes".into()));
        assert_eq!(
            staged_raw(b"views/../../escape"),
            Err("path_escapes".into())
        );
    }

    #[test]
    fn an_absolute_path_is_refused() {
        assert_eq!(staged_raw(b"/etc/passwd"), Err("path_absolute".into()));
    }

    #[test]
    fn a_symlink_leaving_the_release_dir_is_refused() {
        let mut items = whole_release();
        items.push(Item::Symlink("views/out", "../../secret"));
        let (_root, outcome) = staged(&items);
        assert_eq!(outcome, Err("symlink_escapes".into()));

        let mut items = whole_release();
        items.push(Item::Symlink("abs", "/etc"));
        let (_root, outcome) = staged(&items);
        assert_eq!(outcome, Err("symlink_escapes".into()));
    }

    #[test]
    fn a_hard_link_is_refused() {
        let mut items = whole_release();
        items.push(Item::HardLink("twin", APP_EXE));
        let (_root, outcome) = staged(&items);
        assert_eq!(outcome, Err("hard_link".into()));
    }

    #[test]
    fn a_truncated_release_is_incomplete() {
        let (_root, outcome) = staged(&[Item::File(APP_EXE, b"app".to_vec(), 0o755)]);
        assert_eq!(outcome, Err("release_incomplete".into()));
    }

    #[test]
    fn a_leftover_dir_is_replaced_and_a_link_at_its_path_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let bytes = archive_of(&whole_release());
        let sha = Sha::digest(&bytes);
        let archive = root.path().join(format!("{sha}.partial"));
        std::fs::write(&archive, &bytes).unwrap();
        let release = root.path().join("release");
        std::fs::create_dir_all(release.join("stale")).unwrap();
        std::fs::write(release.join("stale/old"), b"old").unwrap();
        seal(&release).unwrap();
        assert_eq!(stage(&archive, sha, &release), Ok(()));
        assert!(!release.join("stale").exists(), "the leftover went");

        let linked = root.path().join("linked");
        std::os::unix::fs::symlink(&release, &linked).unwrap();
        assert_eq!(
            stage(&archive, sha, &linked),
            Err("release_dir_is_a_link".into())
        );
    }

    #[test]
    fn seal_takes_every_write_bit_and_unseal_gives_the_owner_one_back() {
        let (root, outcome) = staged(&whole_release());
        assert_eq!(outcome, Ok(()));
        let release = root.path().join("release");
        seal(&release).unwrap();
        for path in [
            release.clone(),
            release.join(APP_EXE),
            release.join("views"),
            release.join("views/chat.wasm"),
        ] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o222, 0, "{} is writable", path.display());
        }
        assert!(
            std::fs::write(release.join("views/new"), b"x").is_err(),
            "a sealed dir takes no new entry"
        );
        unseal(&release);
        for path in [release.join(APP_EXE), release.join("views")] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o200, 0o200, "{} not reopened", path.display());
        }
    }

    #[test]
    fn collect_removes_only_unkept_shas_and_every_partial() {
        let root = tempfile::tempdir().unwrap();
        let releases = root.path().join("releases");
        let keep = Sha::digest(b"keep");
        let drop = Sha::digest(b"drop");
        for sha in [keep, drop] {
            let dir = releases.join(sha.to_string());
            std::fs::create_dir_all(dir.join("views")).unwrap();
            std::fs::write(dir.join(APP_EXE), b"app").unwrap();
            seal(&dir).unwrap();
        }
        std::fs::create_dir_all(releases.join("not-a-sha")).unwrap();
        std::fs::write(releases.join(format!("{keep}.partial")), b"p").unwrap();
        std::fs::write(releases.join(format!("{drop}.partial")), b"p").unwrap();
        std::fs::write(releases.join("state.json"), b"{}").unwrap();

        collect(&releases, &releases, &[keep]);

        assert!(releases.join(keep.to_string()).join(APP_EXE).is_file());
        assert!(
            !releases.join(drop.to_string()).exists(),
            "the unkept release went"
        );
        assert!(
            releases.join("not-a-sha").is_dir(),
            "a stranger dir is untouched"
        );
        assert!(!releases.join(format!("{keep}.partial")).exists());
        assert!(!releases.join(format!("{drop}.partial")).exists());
        assert!(
            releases.join("state.json").is_file(),
            "a stranger file is untouched"
        );
    }

    #[test]
    fn symlink_containment_is_lexical_from_the_links_directory() {
        let root = Path::new("/r");
        let inside = |link: &str, target: &str| {
            symlink_stays_inside(root, &root.join(link), Path::new(target))
        };
        assert!(inside("a/b", "../c"));
        assert!(inside("a/b", "./c/../d"));
        assert!(inside(
            "Contents/MacOS/views",
            "../Resources/ice-bundle/views"
        ));
        assert!(!inside("a/b", "../../c"));
        assert!(!inside("top", "../x"));
        assert!(!inside("a", "/etc"));
    }
}
