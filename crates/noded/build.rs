//! Stamp this build's identity into the binary, as a DIAGNOSTIC.
//!
//! `noded::services::build_identity` reads it back so `ducktape service status`
//! can print a daemon's build beside the node's own and make ordinary dev-loop
//! skew visible. Nothing is refused for it — see that function for why build
//! equality is not an admission rule.
//!
//! The package version cannot carry this: the repo pins version numbering at v1
//! permanently, so `CARGO_PKG_VERSION` is a constant and every pair of builds
//! would compare equal. The commit — plus a working-tree digest, since a dirty
//! build is not the commit it sits on — is the identity that actually moves.
//!
//! Git absent (a source tarball, a vendored build, Docker without `.git`) is
//! NOT an error and NOT a fallback to anything: the env var is simply left
//! unset, `build_identity()` is `None`, and the node reports its build as
//! `unknown` while serving every service plane normally.
//!
//! And stage the FOUNDING SET beside the binaries this library links into.
//!
//! A ducktape binary embeds no wasm (`AGENTS.md`, "No Embedded Wasm"): a
//! network's wasm is its genesis, and the one place bare wasm files are read
//! is the founding set `node init` composes a genesis from (and the daemons
//! that run no network compose directly from). `cargo build` is what puts
//! that set where a freshly built binary looks — beside the binary, under the
//! name THIS checkout owns (`target/<profile>/modules%<checkout path>`, see
//! `staged_key.rs`), a name this same run bakes into noded as
//! `DUCKTAPE_STAGED_SET` (`noded::services::STAGED_SET`, which
//! `workspace_config::modules_dir` resolves beside the executable) — so a
//! built node is complete without an install step, and a second checkout
//! sharing the target speaks only for its own set. The set is the checkout's committed
//! artifacts (`make wasm-modules`, `make modules-sync` and `make views-sync`
//! refresh them): one component per wasm module the topology names, plus one
//! index guest per module that declares one by carrying a committed
//! `index.wasm`, plus one view per founding id that declares one by carrying
//! a committed `crates/views/<id>/view.wasm`. A declared artifact the checkout
//! lacks fails the build here, naming the path, instead of `node init` later.

use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) mod view_staging;

// the name of the set THIS checkout stages, shared verbatim with the code that
// reads it back (`workspace_config::staged_modules_dir`): one file, included
// here and compiled into that library, because a build script and its reader
// disagreeing about the directory is the whole bug this keying fixes.
#[path = "../workspace-config/src/staged_key.rs"]
pub(crate) mod staged_key;

fn main() {
    // re-run when HEAD moves. `--git-path` resolves correctly inside a git
    // worktree, where `.git` is a file pointing elsewhere.
    for path in ["HEAD", "index"] {
        if let Some(resolved) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={resolved}");
        }
    }

    if let Some(build) = build_id() {
        println!("cargo:rustc-env=DUCKTAPE_BUILD={build}");
    }

    stage_founding_set();
}

/// copy every declared artifact into `<profile dir>/modules`, under the
/// founding-set names `workspace_config::genesis` reads
/// (`<id>.component.wasm`, `<id>.index.wasm`, `netstack.component.wasm`).
///
/// The profile dir is `OUT_DIR`'s third ancestor
/// (`target[/<triple>]/<profile>/build/<pkg>-<hash>/out`): cargo exposes no
/// variable for the directory a binary lands in, and this is the one fixed
/// relation between a build script's output and that directory.
fn stage_founding_set() {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let profile_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("OUT_DIR sits three levels under the profile dir");
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let checkout = staged_key::checkout_of_crate(&manifest);
    let modules = staged_dir(profile_dir, "modules", &checkout);
    stage_preset(&checkout, &modules, topology::PRODUCTION, topology::VIEWS);
    let simulation: Vec<&str> = topology::TOPOLOGY
        .modules
        .iter()
        .map(|module| module.id)
        .collect();
    stage_preset(
        &checkout,
        &staged_dir(profile_dir, "sim-modules", &checkout),
        &simulation,
        &[],
    );
    sweep_abandoned_sets(profile_dir);
    record_the_staging_build(&modules);
    println!("cargo:rustc-env={}", staged_set_env(&modules));
}

/// The `rustc-env` that names the set this run staged, baked into noded.
///
/// The name reaches a binary COMPILED IN, by the same build-script run that
/// staged the set, and never through a file in the profile directory: that
/// directory is shared by every checkout building into the target, so a file
/// there is rewritten by whichever of them builds next — and a binary reading
/// it at boot, a test suite already running included, follows the rewrite
/// onto another checkout's set.
///
/// One run writes the set's bytes, its owner record, this name and
/// `DUCKTAPE_BUILD`, so the four cannot disagree. A checkout whose build
/// reuses a noded unit another checkout's run produced links that checkout's
/// noded, name and stamp together: the set it resolves is the one its noded
/// was staged beside, which is cargo's shared-unit freshness to fix
/// (`noded::services::RESTAGE_COMMAND`), not a resolution a sibling can move
/// after the link.
pub(crate) fn staged_set_env(modules: &Path) -> String {
    let name = modules
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a staged set has a utf-8 directory name");
    format!("DUCKTAPE_STAGED_SET={name}")
}

/// Record in the set which build wrote it.
///
/// A checkout restages its OWN set without relinking every binary that reads
/// it (`cargo check -p noded` after a commit), so a binary linked earlier can
/// find newer bytes under its name; only a stamp inside the set can catch
/// that, and `noded::services::founding_set` refuses on it.
pub(crate) fn record_the_staging_build(modules: &Path) {
    let owner = modules.join(staged_key::STAGED_OWNER);
    let build = build_id().unwrap_or_else(|| staged_key::UNIDENTIFIED_BUILD.to_owned());
    write_without_truncating(&owner, build.as_bytes());
}

/// Remove keyed sets whose checkout is gone.
///
/// A worktree's life ends when its PR merges, and `ops/worktree-clean.sh`
/// removes the tree — but not the set it staged into a SHARED profile
/// directory. Those outlive it, and a reader that resolved the wrong name
/// still found a real, frozen founding set instead of nothing, which is what
/// made the whole failure quiet. A set nothing can own is swept here, by the
/// next build that passes through.
fn sweep_abandoned_sets(profile_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(profile_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let staged_set = name.starts_with("modules%") || name.starts_with("sim-modules%");
        if !staged_set {
            continue;
        }
        let abandoned = staged_key::checkout_of_set_name(name).is_some_and(|at| !at.is_dir());
        if abandoned {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// the directory THIS checkout stages `base` (`"modules"` / `"sim-modules"`)
/// into: the profile directory, plus the checkout's own name for that set.
///
/// EVERY staging destination comes from here. A bare `profile_dir.join(base)`
/// is the poisoning path this keying removed — one checkout writing a set
/// every other checkout reads — and
/// `a_staging_destination_is_always_keyed_to_the_checkout` (in
/// `tests/staged_destination.rs`) reads this file and fails if one comes back.
pub(crate) fn staged_dir(profile_dir: &Path, base: &str, checkout: &Path) -> PathBuf {
    profile_dir.join(staged_key::staged_set_name(base, checkout))
}

/// stage the module set `ids` and the view-only entries `views` into `dest`:
/// a module's component, mapper and (declared) view; a view-only entry's view
/// and assets alone, as `<id>.view.wasm` + `<id>.assets` with no component,
/// which `workspace_config::Genesis::compose` reads as a `Kind::View` entry.
pub(crate) fn stage_preset(checkout: &Path, dest: &Path, ids: &[&str], views: &[&str]) {
    std::fs::create_dir_all(dest).expect("create the staged module directory");
    for entry in std::fs::read_dir(dest).expect("read staged module directory") {
        let path = entry.expect("read staged artifact").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let artifact_id = name
            .strip_suffix(".component.wasm")
            .or_else(|| name.strip_suffix(".index.wasm"))
            .or_else(|| name.strip_suffix(".view.wasm"))
            .or_else(|| name.strip_suffix(".assets"))
            .or_else(|| name.strip_suffix(".lanes"));
        let obsolete = artifact_id
            .is_some_and(|id| id != "netstack" && !ids.contains(&id) && !views.contains(&id));
        if obsolete {
            if std::fs::symlink_metadata(&path)
                .expect("inspect obsolete staged artifact")
                .is_dir()
            {
                std::fs::remove_dir_all(&path).expect("remove obsolete staged assets");
            } else {
                std::fs::remove_file(&path).expect("remove obsolete staged artifact");
            }
        }
    }
    for id in ids {
        let spec = topology::TOPOLOGY
            .spec(id)
            .expect("build preset is in the catalog");
        let module_dir = module_dir(checkout, spec.id);
        stage(
            &module_dir.join("component.wasm"),
            &dest.join(format!("{}.component.wasm", spec.id)),
        );
        view_staging::stage_view(checkout, dest, id).expect("stage module view");
        stage_lane_declaration(&module_dir, dest, spec.id);
        let ships_guest = declares_index_guest(&module_dir);
        // The catalog and the committed artifacts must agree for build presets.
        assert_eq!(
            ships_guest, spec.has_index_guest,
            "module {}: crates/noded/build.rs sees a committed index.wasm = {ships_guest} but \
             topology::TOPOLOGY says has_index_guest = {} — update crates/topology/src/lib.rs \
             to match",
            spec.id, spec.has_index_guest
        );
        let index_path = dest.join(format!("{}.index.wasm", spec.id));
        if ships_guest {
            stage(&module_dir.join("index.wasm"), &index_path);
            continue;
        }
        match std::fs::remove_file(&index_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove obsolete mapper {}: {error}", index_path.display()),
        }
    }
    for id in views {
        assert!(
            topology::TOPOLOGY.spec(id).is_none(),
            "view {id} is also a module in the topology"
        );
        // a view-only entry has nothing but its view: topology::VIEWS declares
        // it, so a checkout lacking the artifact fails here like a missing
        // component, rather than founding a network without it.
        let view = view_staging::committed_view(checkout, id);
        assert!(
            view.is_file(),
            "view {id} is in topology::VIEWS but {} is not committed (run `make views-sync`)",
            view.display()
        );
        view_staging::stage_view(checkout, dest, id).expect("stage founding view");
    }
    stage(
        &checkout.join("crates/networking/netstack-machine/component.wasm"),
        &dest.join("netstack.component.wasm"),
    );
}

/// a module declares its data-plane lanes by carrying `lanes.json`: the FILE
/// IS THE DECLARATION, exactly like the index guest's shell below, so no
/// catalog anywhere lists which modules have lanes. Absent means none, and
/// removing the file removes the staged declaration too — the founding set
/// must never keep a lane the module stopped asking for. The path is a rerun
/// trigger, so editing the declaration re-stages it.
fn stage_lane_declaration(module_dir: &Path, dest: &Path, id: &str) {
    let source = module_dir.join("lanes.json");
    println!("cargo:rerun-if-changed={}", source.display());
    let staged = dest.join(format!("{id}.lanes"));
    if source.is_file() {
        stage(&source, &staged);
        return;
    }
    match std::fs::remove_file(&staged) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!(
            "remove obsolete lane declaration {}: {error}",
            staged.display()
        ),
    }
}

/// a module declares its index guest by carrying the committed artifact:
/// `index.wasm` beside its `component.wasm`. the artifact is the declaration
/// because this repo holds artifacts, not module source — an app module's
/// crate lives in ducktape-modules — so a module cannot ship a mapper the
/// founding set omits or be declared to ship one it lacks. the file is a rerun
/// trigger too: adding or removing the mapper re-stages.
fn declares_index_guest(module_dir: &Path) -> bool {
    let mapper = module_dir.join("index.wasm");
    println!("cargo:rerun-if-changed={}", mapper.display());
    mapper.is_file()
}

/// the checkout directory a module's committed artifacts live in: the
/// product modules under `crates/modules/apps`, the system ones under
/// `crates/modules/system`. Neither holding the module is a build error
/// naming both, since the topology declared a module the tree does not carry.
fn module_dir(checkout: &Path, id: &str) -> PathBuf {
    let candidates = [
        checkout.join("crates/modules/apps").join(id),
        checkout.join("crates/modules/system").join(id),
    ];
    candidates
        .iter()
        .find(|dir| dir.join("component.wasm").is_file())
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "module {id} is in the topology but neither {} nor {} holds a component.wasm \
                 (run `make wasm-modules`)",
                candidates[0].display(),
                candidates[1].display()
            )
        })
}

/// copy `src` to `dest` when the bytes differ, atomically (tmp + rename), and
/// give `dest` the source's mtime so a staged file never reads as newer than
/// this run to cargo's rerun check. Both paths are rerun triggers: a changed
/// artifact re-stages, and so does a deleted staged copy.
pub(crate) fn stage(src: &Path, dest: &Path) {
    println!("cargo:rerun-if-changed={}", src.display());
    println!("cargo:rerun-if-changed={}", dest.display());
    let bytes = std::fs::read(src)
        .unwrap_or_else(|e| panic!("read {} (run `make wasm-modules`): {e}", src.display()));
    let already_staged = std::fs::read(dest).is_ok_and(|have| have == bytes);
    if already_staged {
        return;
    }
    write_without_truncating(dest, &bytes);
    let modified = std::fs::metadata(src).and_then(|m| m.modified());
    if let Ok(modified) = modified {
        let _ = std::fs::File::open(dest).and_then(|f| f.set_modified(modified));
    }
}

/// Put `bytes` at `path` by writing a temporary beside it and renaming over
/// the name — never by opening `path` itself.
///
/// EVERY write into a staged set goes through here, because a staged file is
/// not only this build's. The node e2e pin HARDLINKS the set it pins
/// (`bin/node/tests/common/mod.rs`, "the link is the point — it pins the
/// INODE"), so a plain `std::fs::write` reaches inside a running suite's
/// pinned copy: it truncates the shared inode, and a node booting in that
/// window reads a component of zero bytes and fails closed on it. A rename
/// swings the directory entry onto a NEW inode instead, so the pin keeps the
/// bytes it linked and a reader sees the old file or the new one, never a
/// half-written one.
///
/// Same reason inside one build: `stage` is called for forty artifacts while
/// another checkout may be reading the same directory.
fn write_without_truncating(path: &Path, bytes: &[u8]) {
    // `.staged-by` carries no extension, so this appends
    // rather than replaces — and the pid keeps two builds passing through one
    // profile directory off each other's temporaries.
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes).unwrap_or_else(|e| panic!("write {}: {e}", tmp.display()));
    std::fs::rename(&tmp, path)
        .unwrap_or_else(|e| panic!("rename {} -> {}: {e}", tmp.display(), path.display()));
}

/// `<short sha>`, or `<short sha>-<digest>` when the working tree differs from
/// it. A bare `-dirty` marker would be useless here: the whole point is that
/// two DIFFERENT uncommitted trees at one commit must not compare equal, which
/// is the ordinary dev-loop skew (edit, rebuild the node, leave yesterday's
/// daemon running). Digesting the diff makes them differ.
fn build_id() -> Option<String> {
    let commit = git(&["rev-parse", "--short", "HEAD"])?;
    // tracked changes only: untracked scratch files are not part of the build.
    let diff = git(&["diff", "HEAD"]).unwrap_or_default();
    if diff.is_empty() {
        return Some(commit);
    }
    // `DefaultHasher` is not stable across toolchains, and that is fine now
    // that this is only ever DISPLAYED: two builds of the same dirty tree under
    // different toolchains render as different stamps, which reads as skew and
    // costs nothing. stdlib, so no build-dependency for one hash.
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    diff.hash(&mut hasher);
    Some(format!("{commit}-{:x}", hasher.finish()))
}

/// Run one git command, returning its trimmed stdout. `None` for any failure —
/// git missing, not a repository, or a non-zero exit.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}
