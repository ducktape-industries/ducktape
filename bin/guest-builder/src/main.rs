//! `guest-builder` — build one module's wasm guest out of the platform
//! repository at a revision.
//!
//! a module carries its whole guest surface itself: a `src/guest.rs` behind a
//! wasm-only `guest` feature (the dispatch shell + the component export) over
//! the module SDK (`crates/module-sdk`). packaging that as a cdylib is
//! identical across modules — a manifest, a one-line lib, a `[workspace]`
//! table, the wasm32 patch set — so none of it is checked in: this tool
//! synthesizes it into a scratch workspace, builds for
//! `wasm32-unknown-unknown`, componentizes, and writes the artifact:
//!
//! ```text
//! guest-builder <module-dir> [--index] [--rev <sha>]
//!               [--out <artifact.wasm>] [--scratch <dir>]
//! guest-builder componentize <core.wasm> --out <component.wasm>
//! ```
//!
//! the shell's ONE dependency is the module, reached out of the platform
//! repository as a git source ([`PLATFORM_GIT`]) at the revision the shell
//! lock pins — never out of the checkout in place. that is what makes a module
//! independently buildable and its bytes reproducible: the build inputs are
//! the module's revision, its lock, and the toolchain — the rust channel
//! `rust-toolchain.toml` pins and the componentizer this crate links
//! (`wit-component`, pinned in its manifest; see the crate root) — and
//! nothing else.
//!
//! * every platform crate the module reads (the SDK, a sibling's wire types,
//!   the wasm32 patch stubs) resolves inside that one git source at that one
//!   revision — a path dependency inside a git checkout IS the git source —
//!   so a module's platform is one revision by construction.
//! * a git source's location is no part of a symbol hash (a path dependency's
//!   absolute location is), so bytes do not depend on where a checkout lives.
//!   the directory cargo unpacks the revision into is remapped out of panic
//!   paths by [`remap_flags`].
//! * the shell names the module WITHOUT a `rev`: cargo hashes a git reference
//!   as written into `-C metadata`, so a revision in the manifest would change
//!   every symbol name — and every artifact's bytes — on every commit. the
//!   revision lives in the lock, which is not hashed, so a module rebuilt
//!   at a later revision that changed none of the sources it compiles yields
//!   byte-identical output.
//! * the shell lock is the module's `guest.lock`, committed beside its
//!   artifacts: the record of the revision and the registry versions an
//!   artifact came from, and the seed of the next build, so a crates.io
//!   publish between two rebuilds does not move the bytes. a canonical build
//!   writes artifact and lock together; `--out` writes the artifact alone.
//!
//! the revision defaults to the checkout's HEAD and must be reachable at
//! [`PLATFORM_GIT`]: push before building. uncommitted inputs anywhere in the
//! resolved platform graph (including the SDK and sibling packages) are
//! refused, since the build would silently compile HEAD instead.
//!
//! `--index` builds the module's INDEX guest instead: the fluentabi mapper
//! behind the crate's `index-guest` feature (a `src/index_guest.rs` — see the
//! index-guest crate). same shell, a second member, and the artifact stays
//! core wasm (`index.wasm`, no componentize step): the fluent31 engine
//! executes plain wasm32 modules, not components.
//!
//! a module authored outside this repository needs none of the shell: its
//! crate is the cdylib, it pins `ducktape-module-sdk` and the patch stubs by
//! git revision in its own manifest, and plain cargo + the `componentize`
//! verb (or `wasm-tools component new` at the same release) build it.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

/// the platform repository every shell reaches the module and the platform
/// crates through. a constant, not the checkout's remote: the URL is part of
/// every symbol hash, so two builders spelling it differently would produce
/// different bytes for the same revision.
const PLATFORM_GIT: &str = "https://github.com/orthory/ducktape";

const USAGE: &str = "usage: guest-builder <module-dir> [--index] [--rev <sha>] \
     [--out <artifact.wasm>] [--scratch <dir>]\n       \
     guest-builder componentize <core.wasm> --out <component.wasm>\n       \
     guest-builder vendor --out <dir> [--directory <path the config names>]";

/// which of a module's two guests to build. the consensus component and the
/// index mapper share the shell; everything guest-specific — contract
/// feature, shell member, artifact name, whether the cdylib is componentized —
/// hangs off this one discriminant.
#[derive(Clone, Copy, PartialEq, Debug)]
enum GuestKind {
    /// the `ducktape:module` consensus component (`guest` feature).
    Component,
    /// the fluentabi index mapper (`index-guest` feature), core wasm.
    Index,
}

impl GuestKind {
    const ALL: [GuestKind; 2] = [GuestKind::Component, GuestKind::Index];

    fn feature(self) -> &'static str {
        match self {
            GuestKind::Component => "guest",
            GuestKind::Index => "index-guest",
        }
    }

    fn artifact(self) -> &'static str {
        match self {
            GuestKind::Component => "component.wasm",
            GuestKind::Index => "index.wasm",
        }
    }

    /// the shell workspace member (and the cdylib's name suffix) for this guest.
    fn member(self) -> &'static str {
        match self {
            GuestKind::Component => "component",
            GuestKind::Index => "index",
        }
    }

    fn missing_feature_hint(self, name: &str) -> String {
        match self {
            GuestKind::Component => format!(
                "module `{name}` declares no `guest` feature — the port lives in the \
                 module crate (a `src/guest.rs` behind `guest = [\"dep:ducktape-module-sdk\"]`); \
                 see crates/modules/apps/tasks for the shape"
            ),
            GuestKind::Index => format!(
                "module `{name}` declares no `index-guest` feature — the index mapper \
                 lives in the module crate (a `src/index_guest.rs` behind \
                 `index-guest = [\"index_guest/guest\"]`); see crates/modules/apps/tasks \
                 for the shape"
            ),
        }
    }
}

fn main() {
    let Err(err) = run() else { return };
    eprintln!("guest-builder: {err}");
    process::exit(1);
}

fn run() -> Result<(), String> {
    match parse_args()? {
        Args::Build(args) => build_guest(args),
        Args::Componentize { core, out } => componentize(&core, &out),
        Args::Vendor { out, directory } => vendor(&out, directory.as_deref()),
    }
}

fn build_guest(args: BuildArgs) -> Result<(), String> {
    let kind = args.kind;
    let platform_root = default_platform_root()?;
    let module_dir = canonical(&args.module_dir)?;
    let module = read_module(&platform_root, &module_dir)?;
    let declares_requested_guest = module.guests.contains(&kind);
    if !declares_requested_guest {
        return Err(kind.missing_feature_hint(&module.name));
    }

    let rev = match &args.rev {
        Some(rev) => rev.clone(),
        None => head(&platform_root)?,
    };

    let scratch = match args.scratch {
        Some(dir) => dir,
        None => platform_root
            .join("target/guest-builder")
            .join(&module.name),
    };
    eprintln!(
        "guest-builder: {} {} at {rev}",
        module.name,
        kind.artifact()
    );
    seed_lock(&scratch, &module_dir)?;
    let graph = pin(&scratch, &module, PLATFORM_GIT, &rev)?;
    let checkout = checkout_root(&graph, &module)?;
    let builds_head = args.rev.is_none();
    if builds_head {
        let inputs = platform_inputs(&graph, &checkout, PLATFORM_GIT)?;
        refuse_modified_sources(&platform_root, &inputs)?;
    }
    build(
        &scratch,
        &module.name,
        kind,
        &remap_flags(&scratch, &checkout),
    )?;

    let cdylib = cdylib_path(&scratch, &module.name, kind);
    // the canonical artifact and its lock are written together: the lock is
    // the record of THOSE bytes. a one-off `--out` build leaves the module
    // directory untouched, so a check that rebuilds every guest keeps the
    // tree clean.
    let out = match args.out {
        Some(path) => {
            write_artifact(kind, &cdylib, &path)?;
            path
        }
        None => {
            let canonical = module_dir.join(kind.artifact());
            write_artifact(kind, &cdylib, &canonical)?;
            record_lock(&scratch, &module_dir)?;
            canonical
        }
    };
    println!("{}", out.display());
    Ok(())
}

fn write_artifact(kind: GuestKind, cdylib: &Path, out: &Path) -> Result<(), String> {
    match kind {
        GuestKind::Component => componentize(cdylib, out),
        GuestKind::Index => copy_cdylib(cdylib, out),
    }
}

// ============================================================================
// argument parsing
// ============================================================================

/// the two things the tool does: build a module's guest out of the
/// repository, or wrap one already-built core module as a component.
enum Args {
    Build(BuildArgs),
    Componentize {
        core: PathBuf,
        out: PathBuf,
    },
    Vendor {
        out: PathBuf,
        /// what the emitted config names as the vendor directory, when that is
        /// not where this run writes it — a guest image builds the set on the
        /// host and mounts it somewhere else entirely.
        directory: Option<String>,
    },
}

struct BuildArgs {
    module_dir: PathBuf,
    kind: GuestKind,
    rev: Option<String>,
    out: Option<PathBuf>,
    scratch: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut argv = env::args().skip(1).peekable();
    // step: the verb ladder became a match when the third verb arrived. ONE
    // discriminant — the leading word — and the build verb is the unnamed one,
    // which is why it is the fallthrough rather than an arm.
    let verb = argv.peek().cloned().unwrap_or_default();
    match verb.as_str() {
        "componentize" => {
            argv.next();
            parse_componentize_args(argv)
        }
        "vendor" => {
            argv.next();
            parse_vendor_args(argv)
        }
        _ => parse_build_args(argv).map(Args::Build),
    }
}

fn parse_vendor_args(mut argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut out = None;
    let mut directory = None;
    while let Some(arg) = argv.next() {
        let value = argv
            .next()
            .ok_or_else(|| format!("{arg} needs a value\n{USAGE}"))?;
        match arg.as_str() {
            "--out" => out = Some(PathBuf::from(value)),
            "--directory" => directory = Some(value),
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    let Some(out) = out else {
        return Err(format!("vendor needs --out <dir>\n{USAGE}"));
    };
    Ok(Args::Vendor { out, directory })
}

fn parse_componentize_args(mut argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut core = None;
    let mut out = None;
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--out" => {
                out = Some(PathBuf::from(
                    argv.next()
                        .ok_or_else(|| format!("--out needs a value\n{USAGE}"))?,
                ));
            }
            flag if flag.starts_with("--") => {
                return Err(format!("unknown flag {flag}\n{USAGE}"));
            }
            positional => {
                let unclaimed = core.is_none();
                if !unclaimed {
                    return Err(format!("unexpected argument {positional}\n{USAGE}"));
                }
                core = Some(PathBuf::from(positional));
            }
        }
    }
    let (Some(core), Some(out)) = (core, out) else {
        return Err(USAGE.to_string());
    };
    Ok(Args::Componentize { core, out })
}

fn parse_build_args(mut argv: impl Iterator<Item = String>) -> Result<BuildArgs, String> {
    let mut module_dir = None;
    let mut kind = GuestKind::Component;
    let mut rev = None;
    let mut out = None;
    let mut scratch = None;

    while let Some(arg) = argv.next() {
        let flag_value = |argv: &mut dyn Iterator<Item = String>| {
            argv.next()
                .ok_or_else(|| format!("{arg} needs a value\n{USAGE}"))
        };
        match arg.as_str() {
            "--index" => kind = GuestKind::Index,
            "--rev" => rev = Some(flag_value(&mut argv)?),
            "--out" => out = Some(PathBuf::from(flag_value(&mut argv)?)),
            "--scratch" => scratch = Some(PathBuf::from(flag_value(&mut argv)?)),
            flag if flag.starts_with("--") => {
                return Err(format!("unknown flag {flag}\n{USAGE}"));
            }
            positional => {
                let unclaimed = module_dir.is_none();
                if !unclaimed {
                    return Err(format!("unexpected argument {positional}\n{USAGE}"));
                }
                module_dir = Some(PathBuf::from(positional));
            }
        }
    }

    let Some(module_dir) = module_dir else {
        return Err(USAGE.to_string());
    };
    Ok(BuildArgs {
        module_dir,
        kind,
        rev,
        out,
        scratch,
    })
}

// ============================================================================
// module introspection — name, place in the repository, declared guests
// ============================================================================

struct Module {
    name: String,
    /// the module directory relative to the platform root: its place in the
    /// repository, which is where the build reads it from.
    path: PathBuf,
    /// the guests the crate declares, by contract feature.
    guests: Vec<GuestKind>,
}

/// read the module's package name and contract features via `cargo metadata`
/// on the working-tree manifest. the build compiles the repository, so the
/// module must be in it; a crate outside the platform checkout is an
/// out-of-tree module, which is its own cdylib and needs no shell.
fn read_module(platform_root: &Path, module_dir: &Path) -> Result<Module, String> {
    let Ok(path) = module_dir.strip_prefix(platform_root) else {
        return Err(format!(
            "{} is outside the platform checkout {} — guest-builder builds the modules of \
             this repository; a module authored elsewhere is its own cdylib crate pinning \
             ducktape-module-sdk by git revision, built with cargo and `guest-builder \
             componentize` directly",
            module_dir.display(),
            platform_root.display()
        ));
    };
    let manifest = module_dir.join("Cargo.toml");
    let output = Command::new(cargo())
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(&manifest)
        .output()
        .map_err(|e| format!("running cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata on {}: {}",
            manifest.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let meta: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("parsing cargo metadata output: {e}"))?;

    // metadata on a workspace member lists every member; the module is the
    // package whose manifest is the one we asked about.
    let is_this_module =
        |pkg: &&serde_json::Value| pkg["manifest_path"].as_str() == manifest.to_str();
    let Some(pkg) = meta["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(is_this_module)
    else {
        return Err(format!("{} is not a cargo package", module_dir.display()));
    };
    let Some(name) = pkg["name"].as_str() else {
        return Err(format!(
            "{}: package name missing from metadata",
            manifest.display()
        ));
    };
    let guests = GuestKind::ALL
        .into_iter()
        .filter(|kind| pkg["features"].get(kind.feature()).is_some())
        .collect();
    Ok(Module {
        name: name.to_string(),
        path: path.to_path_buf(),
        guests,
    })
}

/// the checkout's HEAD: the revision a build without `--rev` compiles.
fn head(platform_root: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(platform_root)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| format!("running git rev-parse: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse HEAD in {}: {}",
            platform_root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Every local source used by the platform graph must agree with HEAD. The
/// generated artifacts and lock are outputs, so a rebuild may rewrite them.
fn refuse_modified_sources(platform_root: &Path, inputs: &BTreeSet<PathBuf>) -> Result<(), String> {
    let mut changed = BTreeSet::new();
    for args in [
        vec!["diff", "--name-only", "-z", "HEAD", "--"],
        vec!["ls-files", "--others", "--exclude-standard", "-z", "--"],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(platform_root)
            .args(args)
            .args(inputs)
            .args([
                ":(exclude)**/component.wasm",
                ":(exclude)**/index.wasm",
                ":(exclude)**/guest.lock",
            ])
            .output()
            .map_err(|e| format!("checking platform sources: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "checking platform sources: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        changed.extend(
            output
                .stdout
                .split(|byte| *byte == 0)
                .filter(|path| !path.is_empty())
                .map(|path| String::from_utf8_lossy(path).into_owned()),
        );
    }
    if changed.is_empty() {
        return Ok(());
    }
    Err(format!(
        "uncommitted platform build inputs: {} — the guest compiles HEAD; commit and push these sources first (or pass --rev to build a specific revision)",
        changed.into_iter().collect::<Vec<_>>().join(", ")
    ))
}

/// Cargo's resolved platform packages include the SDK, sibling wire types,
/// and active patch crates. Workspace manifests and build configuration are
/// inputs even though Cargo does not report them as packages.
fn platform_inputs(
    graph: &serde_json::Value,
    checkout: &Path,
    git: &str,
) -> Result<BTreeSet<PathBuf>, String> {
    let mut inputs = BTreeSet::from([
        PathBuf::from("Cargo.toml"),
        PathBuf::from("rust-toolchain.toml"),
        PathBuf::from(".cargo"),
    ]);
    let source_prefix = format!("git+{git}?");
    let Some(packages) = graph["packages"].as_array() else {
        return Err("cargo metadata has no packages".to_string());
    };
    for package in packages {
        let from_platform = package["source"]
            .as_str()
            .is_some_and(|source| source.starts_with(&source_prefix));
        if !from_platform {
            continue;
        }
        let Some(manifest) = package["manifest_path"].as_str() else {
            return Err("platform package has no manifest path".to_string());
        };
        let path = Path::new(manifest)
            .strip_prefix(checkout)
            .map_err(|e| format!("platform package outside its checkout: {manifest}: {e}"))?;
        let Some(directory) = path.parent() else {
            return Err(format!("platform manifest has no directory: {manifest}"));
        };
        inputs.insert(directory.to_path_buf());
    }
    Ok(inputs)
}

// ============================================================================
// synthesis — the shell workspace every module shares
// ============================================================================

/// write the shell workspace: one cdylib member per guest the module
/// declares, each depending on the module alone (its contract feature on,
/// defaults off) out of the platform git source, plus the uniform wasm32 patch
/// set from the same source. regenerated on every run — nothing here is
/// hand-maintained state, except the lock, which is seeded from the module's
/// committed `guest.lock` when there is one.
fn synthesize(scratch: &Path, module: &Module, source: &str) -> Result<(), String> {
    for kind in &module.guests {
        let member = scratch.join(kind.member());
        let src = member.join("src");
        fs::create_dir_all(&src).map_err(|e| format!("creating {}: {e}", src.display()))?;
        write(
            &member.join("Cargo.toml"),
            &member_manifest(&module.name, *kind, source),
        )?;
        write(&src.join("lib.rs"), &member_lib(&module.name))?;
    }
    write(
        &scratch.join("Cargo.toml"),
        &workspace_manifest(&module.guests, source),
    )
}

fn seed_lock(scratch: &Path, module_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(scratch).map_err(|e| format!("creating {}: {e}", scratch.display()))?;
    let lock = scratch.join("Cargo.lock");
    let committed = module_dir.join("guest.lock");
    let Err(error) = fs::copy(&committed, &lock) else {
        return Ok(());
    };
    let seed_is_absent = error.kind() == std::io::ErrorKind::NotFound;
    if !seed_is_absent {
        return Err(format!(
            "seeding lock from {}: {error}",
            committed.display()
        ));
    }
    // A first build has no seed. A previous scratch lock is never an input.
    let Err(error) = fs::remove_file(&lock) else {
        return Ok(());
    };
    let scratch_lock_is_absent = error.kind() == std::io::ErrorKind::NotFound;
    if scratch_lock_is_absent {
        return Ok(());
    }
    Err(format!("removing scratch lock: {error}"))
}

fn workspace_manifest(guests: &[GuestKind], source: &str) -> String {
    let members: Vec<String> = guests
        .iter()
        .map(|kind| format!("\"{}\"", kind.member()))
        .collect();
    format!(
        r#"# synthesized by guest-builder — do not edit; regenerated on every build.
# the packaging shell only: the module logic and its guest ports live in the
# module crate, read out of the platform repository at the revision the lock
# pins. one member per guest the module declares.
[workspace]
members = [{members}]
resolver = "2"

# the uniform wasm32 patch set (crates/module-sdk/stubs in the platform
# repository, at the module's own revision): applied to every guest; cargo's
# "unused patch" warning on a module whose graph never pulls one of these
# crates is expected and harmless.
[patch.crates-io]
getrandom-02 = {{ package = "getrandom", version = "0.2", {source} }}
getrandom-03 = {{ package = "getrandom", version = "0.3", {source} }}
getrandom-04 = {{ package = "getrandom", version = "0.4", {source} }}
blst = {{ {source} }}
"#,
        members = members.join(", ")
    )
}

/// The member manifest uses an explicit revision during resolution, then
/// source alone during compilation: rustc must never hash a written selector.
fn member_manifest(name: &str, kind: GuestKind, source: &str) -> String {
    let feature = kind.feature();
    let member = kind.member();
    format!(
        r#"# synthesized by guest-builder — do not edit; regenerated on every build.
[package]
name = "{name}-{member}"
version = "0.0.0"
edition = "2021"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
{name} = {{ {source}, default-features = false, features = ["{feature}"] }}
"#
    )
}

fn member_lib(name: &str) -> String {
    format!(
        "// synthesized by guest-builder — link the module crate for its guest export.\n\
         extern crate {} as _;\n",
        snake(name)
    )
}

/// Resolve against the requested revision from the first lookup, including
/// a module that does not exist on the repository's default branch. The
/// explicit selector is removed from both manifests and lock before rustc
/// runs: only the lock's precise commit may vary between identical builds.
fn pin(scratch: &Path, module: &Module, git: &str, rev: &str) -> Result<serde_json::Value, String> {
    let locked_source = format!("git = {git:?}");
    let revision_source = format!("{locked_source}, rev = {rev:?}");
    synthesize(scratch, module, &revision_source)?;
    let output = Command::new(cargo())
        .args([
            "metadata",
            "--format-version",
            "1",
            "--filter-platform",
            "wasm32-unknown-unknown",
        ])
        .current_dir(scratch)
        .output()
        .map_err(|e| format!("resolving guest dependencies: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "resolving {} at {rev} from {git} failed (push the revision first): {}",
            module.name,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let graph: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("parsing cargo metadata output: {e}"))?;
    let lock = scratch.join("Cargo.lock");
    let content = fs::read_to_string(&lock).map_err(|e| format!("reading resolved lock: {e}"))?;
    let selected_source = format!("git+{git}?rev={rev}");
    let precise_source = format!("git+{git}");
    // Cargo uses this source ID in package entries and disambiguated dependency
    // strings. Normalize every occurrence so they continue to name one source.
    write(&lock, &content.replace(&selected_source, &precise_source))?;
    synthesize(scratch, module, &locked_source)?;
    Ok(graph)
}

/// Locate the platform checkout from the module's resolved manifest.
fn checkout_root(meta: &serde_json::Value, module: &Module) -> Result<PathBuf, String> {
    let is_the_module_from_git = |pkg: &&serde_json::Value| {
        let named = pkg["name"].as_str() == Some(module.name.as_str());
        let from_git = pkg["source"]
            .as_str()
            .is_some_and(|source| source.starts_with("git+"));
        named && from_git
    };
    let Some(pkg) = meta["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(is_the_module_from_git)
    else {
        return Err(format!(
            "{} is not in the shell's resolved graph as a git package",
            module.name
        ));
    };
    let Some(manifest_path) = pkg["manifest_path"].as_str() else {
        return Err(format!(
            "{}: manifest path missing from metadata",
            module.name
        ));
    };
    checkout_root_of(Path::new(manifest_path), &module.path)
}

/// the checkout a git package's manifest sits in: its manifest path minus the
/// module's place in the repository.
fn checkout_root_of(manifest_path: &Path, module_path: &Path) -> Result<PathBuf, String> {
    let module_manifest = module_path.join("Cargo.toml");
    let depth = module_manifest.components().count();
    let Some(root) = manifest_path.ancestors().nth(depth) else {
        return Err(format!(
            "{} is shallower than {}",
            manifest_path.display(),
            module_manifest.display()
        ));
    };
    let sits_at_its_repository_place = root.join(&module_manifest) == manifest_path;
    if !sits_at_its_repository_place {
        return Err(format!(
            "{} does not end in {}",
            manifest_path.display(),
            module_manifest.display()
        ));
    }
    Ok(root.to_path_buf())
}

// ============================================================================
// build + componentize
// ============================================================================

/// `--remap-path-prefix` mappings that keep every builder-local absolute path
/// out of the artifact's CONTENT — panic locations name their source file, so
/// without these the bytes carry the builder's `/home/<user>/...` around
/// forever. `ops/wasm-repro-check.sh` and the host-path scan in
/// `make wasm-modules-check` are the gates.
///
/// the checkout mapping is last on purpose: the checkout sits under
/// CARGO_HOME and rustc takes the LAST matching mapping, so the
/// revision-specific `git/checkouts/<repo>-<hash>/<rev>` directory becomes the
/// stable `/ducktape` — the same token at every revision — instead of a path
/// that would move the bytes on every commit.
fn remap_flags(scratch: &Path, checkout: &Path) -> String {
    let home = env::var("HOME").unwrap_or_default();
    let tool_home =
        |key: &str, dir: &str| env::var(key).unwrap_or_else(|_| format!("{home}/{dir}"));
    let mappings = [
        (tool_home("CARGO_HOME", ".cargo"), "/cargo"),
        (tool_home("RUSTUP_HOME", ".rustup"), "/rustup"),
        (scratch.display().to_string(), "/guest-builder"),
        (checkout.display().to_string(), "/ducktape"),
    ];
    let flags: Vec<String> = mappings
        .iter()
        .map(|(from, to)| format!("--remap-path-prefix={from}={to}"))
        .collect();
    // the ENCODED form's separator: plain `RUSTFLAGS` splits on whitespace, so
    // a path containing a space would tear one flag into two.
    flags.join("\x1f")
}

fn build(scratch: &Path, name: &str, kind: GuestKind, rustflags: &str) -> Result<(), String> {
    let status = build_command(scratch, name, kind, rustflags)
        .status()
        .map_err(|e| format!("running cargo build: {e}"))?;
    if !status.success() {
        return Err(format!("wasm32 build failed in {}", scratch.display()));
    }
    Ok(())
}

fn build_command(scratch: &Path, name: &str, kind: GuestKind, rustflags: &str) -> Command {
    let member = format!("{name}-{}", kind.member());
    let mut command = Command::new(cargo());
    command
        // `--locked`: the lock is complete after `pin`, and the bytes are
        // only reproducible if this build changes nothing in it.
        .args([
            "build",
            "--locked",
            "--target",
            "wasm32-unknown-unknown",
            "--release",
            "-p",
            &member,
        ])
        // Explicit CLI selection wins over inherited CARGO_TARGET_DIR and
        // Cargo configuration, matching the artifact lookup below.
        .arg("--target-dir")
        .arg("target")
        .env("CARGO_ENCODED_RUSTFLAGS", rustflags)
        // the encoded form wins over the plain one, but an inherited
        // `RUSTFLAGS` would be a confusing dead passenger.
        .env_remove("RUSTFLAGS")
        .current_dir(scratch);
    command
}

/// the one place every gated artifact is componentized: the linked
/// componentizer (the crate root), never a binary found on a PATH.
fn componentize(cdylib: &Path, out: &Path) -> Result<(), String> {
    let core = fs::read(cdylib).map_err(|e| format!("reading {}: {e}", cdylib.display()))?;
    let component = guest_builder::componentize(&core)
        .map_err(|e| format!("componentizing {}: {e}", cdylib.display()))?;
    fs::write(out, component).map_err(|e| format!("writing {}: {e}", out.display()))
}

/// an index guest ships as the built cdylib itself — fluentabi is core wasm,
/// so there is nothing to componentize.
fn copy_cdylib(cdylib: &Path, out: &Path) -> Result<(), String> {
    fs::copy(cdylib, out)
        .map(|_| ())
        .map_err(|e| format!("copying {} to {}: {e}", cdylib.display(), out.display()))
}

/// the shell lock, written back beside the module as its `guest.lock`: the
/// record of what the artifact was built from, and the seed of the next build.
fn record_lock(scratch: &Path, module_dir: &Path) -> Result<(), String> {
    let lock = module_dir.join("guest.lock");
    fs::copy(scratch.join("Cargo.lock"), &lock)
        .map(|_| ())
        .map_err(|e| format!("recording the shell lock as {}: {e}", lock.display()))
}

fn cdylib_path(scratch: &Path, name: &str, kind: GuestKind) -> PathBuf {
    scratch
        .join("target/wasm32-unknown-unknown/release")
        .join(format!("{}_{}.wasm", snake(name), kind.member()))
}

// ============================================================================
// small helpers
// ============================================================================

/// the cargo that invoked us (`cargo run` sets `CARGO`), else PATH's.
fn cargo() -> String {
    env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

/// the ducktape checkout this binary was built from: the source of the
/// default revision, and the tree a module directory must sit in.
///
/// Baked in at COMPILE time, which matters whenever worktrees share one cargo
/// target directory: the binary sitting in it belongs to whichever worktree
/// built it last, so `cargo run -p guest-builder` from yours can hand you a
/// sibling's — one that reads a sibling's modules, vendors a sibling's
/// dependency set, and refuses your own module with "is outside the platform
/// checkout". Build it into a private `CARGO_TARGET_DIR` when the answer has
/// to come from THIS tree.
fn default_platform_root() -> Result<PathBuf, String> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let Some(root) = manifest_dir.parent().and_then(Path::parent) else {
        return Err("cannot derive the platform root from the build location".to_string());
    };
    canonical(root)
}

fn canonical(path: &Path) -> Result<PathBuf, String> {
    path.canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn snake(name: &str) -> String {
    name.replace('-', "_")
}

fn write(path: &Path, content: &str) -> Result<(), String> {
    fs::write(path, content).map_err(|e| format!("writing {}: {e}", path.display()))
}

// ============================================================================
// tests
// ============================================================================

// ---- vendor: the registry a run builds a module against, with no network ----

/// `guest-builder vendor --out <dir> [--directory <path>]`: every crates.io
/// package a module guest build resolves, as a directory plus the cargo config
/// that points at it.
///
/// A run inside a microVM reaches the network through vsock tunnels and
/// nothing else, so a module build there resolves against vendored sources or
/// it does not resolve at all. The set is DERIVED, never listed: one
/// synthesized workspace depending on every crate that declares a `guest`
/// feature, seeded with the checkout's own lockfile, has a lockfile that IS
/// the union of what the modules need — and a crate nobody depends on is
/// simply never in it.
///
/// **The set is the LOCKFILE graph, not the compile graph, and that is not an
/// oversight.** Cargo's resolution is target-independent even though
/// compilation is not: a replaced source must hold every package in the
/// lockfile so the resolver can read its manifest, including ones no wasm32
/// build ever compiles. Filtering by `cargo tree --target
/// wasm32-unknown-unknown` removes `sha2`'s `cpufeatures` — declared under
/// `[target.'cfg(any(target_arch = "x86_64", …))'.dependencies]` — and every
/// build then fails with "no matching package named `cpufeatures` found". It
/// is the same reason `cargo vendor` has no `--target`: it could not honour
/// one. So `aws-lc-sys` rides along at 69 MB, unreachable and required.
///
/// The tree it reads is [`default_platform_root`], baked in at compile time —
/// so on a shared cargo target directory this can vendor a SIBLING worktree's
/// dependency set without saying so. Read that function's note first.
fn vendor(out: &Path, directory: Option<&str>) -> Result<(), String> {
    let root = default_platform_root()?;
    let scratch = root.join("target/guest-builder/vendor-shell");
    let _ = fs::remove_dir_all(&scratch);
    synthesize_vendor_shell(&scratch, &root)?;
    // the checkout's OWN pins: a run clones this tree, so the versions it asks
    // for are the versions that have to be on disk. Without the seed cargo
    // resolves to latest-compatible and vendors crates the clone never wants.
    fs::copy(root.join("Cargo.lock"), scratch.join("Cargo.lock"))
        .map_err(|e| format!("seeding the vendor lock: {e}"))?;

    let vendor_dir = out.join("vendor");
    fs::create_dir_all(out).map_err(|e| format!("creating {}: {e}", out.display()))?;
    let output = Command::new(cargo())
        .arg("vendor")
        .arg("--versioned-dirs")
        .arg(&vendor_dir)
        .current_dir(&scratch)
        .output()
        .map_err(|e| format!("running cargo vendor: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo vendor: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // cargo writes the config with the directory it just filled; an image build
    // fills it on the host and mounts it elsewhere, so `--directory` renames it
    // without touching the git-source stanzas, which carry exact revisions and
    // are nobody's to hand-write.
    let config = String::from_utf8_lossy(&output.stdout).into_owned();
    let named = match directory {
        None => config,
        Some(path) => config.replace(
            &format!("directory = \"{}\"", vendor_dir.display()),
            &format!("directory = \"{path}\""),
        ),
    };
    write(&out.join("config.toml"), &named)?;
    eprintln!(
        "guest-builder: vendored {} crates for {} guests",
        fs::read_dir(&vendor_dir).map(Iterator::count).unwrap_or(0),
        guest_packages(&root)?.len()
    );
    Ok(())
}

/// one workspace whose sole member depends on EVERY guest crate in the
/// checkout, carrying the same wasm32 patch set a build shell gets.
fn synthesize_vendor_shell(scratch: &Path, root: &Path) -> Result<(), String> {
    let shell = scratch.join("shell");
    let src = shell.join("src");
    fs::create_dir_all(&src).map_err(|e| format!("creating {}: {e}", src.display()))?;
    let deps: Vec<String> = guest_packages(root)?
        .into_iter()
        .map(|(name, dir)| {
            format!(
                "{name} = {{ path = {:?}, features = [\"guest\"], default-features = false }}",
                dir.display().to_string()
            )
        })
        .collect();
    write(
        &shell.join("Cargo.toml"),
        &format!(
            "# synthesized by `guest-builder vendor` — do not edit.\n\
             [package]\nname = \"guest-vendor-shell\"\nversion = \"0.0.0\"\n\
             edition = \"2021\"\npublish = false\n\n[lib]\npath = \"src/lib.rs\"\n\n\
             [dependencies]\n{}\n",
            deps.join("\n")
        ),
    )?;
    write(&src.join("lib.rs"), "")?;
    write(
        &scratch.join("Cargo.toml"),
        &format!(
            "# synthesized by `guest-builder vendor` — do not edit.\n\
             [workspace]\nmembers = [\"shell\"]\nresolver = \"2\"\n{}",
            wasm32_patch_section(root)?
        ),
    )
}

/// every package in the checkout that declares a `guest` feature — the same
/// question [`read_module`] asks of one directory, asked of the whole tree, so
/// a module added tomorrow is vendored for without anyone editing a list.
fn guest_packages(root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let output = Command::new(cargo())
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("running cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let meta: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("parsing cargo metadata output: {e}"))?;
    let mut found: Vec<(String, PathBuf)> = meta["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|pkg| {
            pkg["features"]
                .get(GuestKind::Component.feature())
                .is_some()
        })
        .filter_map(|pkg| {
            let name = pkg["name"].as_str()?.to_string();
            let dir = Path::new(pkg["manifest_path"].as_str()?)
                .parent()?
                .to_path_buf();
            Some((name, dir))
        })
        .collect();
    found.sort();
    if found.is_empty() {
        return Err(format!("no guest crates under {}", root.display()));
    }
    Ok(found)
}

/// the uniform wasm32 patch set, read out of `crates/module-sdk/stubs` rather
/// than written down again: a build shell spells these as one git source and
/// finds them by package name, which a path source cannot do, so the two
/// renderings differ — but the SET does not, because both come from that
/// directory. A stub added there is patched here without an edit.
fn wasm32_patch_section(root: &Path) -> Result<String, String> {
    let stubs = root.join("crates/module-sdk/stubs");
    let mut entries: Vec<PathBuf> = fs::read_dir(&stubs)
        .map_err(|e| format!("reading {}: {e}", stubs.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_dir())
        .collect();
    entries.sort();
    let mut lines = String::from(
        "\n# the uniform wasm32 patch set (crates/module-sdk/stubs): the crates a\n\
         # guest substitutes because they cannot compile to wasm32. Read from that\n\
         # directory, so this set cannot drift from the one a build shell applies.\n\
         [patch.crates-io]\n",
    );
    for dir in entries {
        let alias = dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("unreadable stub name in {}", stubs.display()))?;
        let (package, version) = stub_identity(&dir)?;
        lines.push_str(&format!(
            "{alias} = {{ package = \"{package}\", version = \"{version}\", path = {:?} }}\n",
            dir.display().to_string()
        ));
    }
    Ok(lines)
}

/// a stub's real package name and the `major.minor` requirement it stands in
/// for, from its own manifest — the first `name` and `version` under
/// `[package]`, which cargo's normalized manifests put first.
fn stub_identity(dir: &Path) -> Result<(String, String), String> {
    let manifest = dir.join("Cargo.toml");
    let text = fs::read_to_string(&manifest)
        .map_err(|e| format!("reading {}: {e}", manifest.display()))?;
    let field = |key: &str| {
        text.lines().find_map(|line| {
            line.trim()
                .strip_prefix(&format!("{key} = "))?
                .strip_prefix('"')?
                .strip_suffix('"')
                .map(str::to_string)
        })
    };
    let (Some(name), Some(version)) = (field("name"), field("version")) else {
        return Err(format!(
            "{}: no package name and version",
            manifest.display()
        ));
    };
    let mut parts = version.split('.');
    let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
        return Err(format!(
            "{}: version {version} is not major.minor",
            manifest.display()
        ));
    };
    Ok((name, format!("{major}.{minor}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a written git reference is hashed into every symbol name, so the shell
    /// must name the module by source alone and leave the revision to the lock.
    #[test]
    fn the_shell_names_the_module_by_source_alone() {
        let manifest = member_manifest(
            "chat",
            GuestKind::Component,
            &format!("git = {PLATFORM_GIT:?}"),
        );
        assert!(manifest.contains(
            "chat = { git = \"https://github.com/orthory/ducktape\", default-features = false, features = [\"guest\"] }"
        ));
        assert!(!manifest.contains("rev ="));
        assert!(!manifest.contains("branch ="));
        assert!(manifest.contains("name = \"chat-component\""));
    }

    #[test]
    fn the_workspace_has_one_member_per_declared_guest() {
        let both = workspace_manifest(
            &[GuestKind::Component, GuestKind::Index],
            &format!("git = {PLATFORM_GIT:?}"),
        );
        assert!(both.contains("members = [\"component\", \"index\"]"));
        let component_only =
            workspace_manifest(&[GuestKind::Component], &format!("git = {PLATFORM_GIT:?}"));
        assert!(component_only.contains("members = [\"component\"]"));
        // the patch stubs ride the same source, with no reference either
        assert!(both.contains("getrandom-02 = { package = \"getrandom\", version = \"0.2\", git = \"https://github.com/orthory/ducktape\" }"));
        assert!(!both.contains("rev ="));
    }

    /// rustc takes the last matching mapping, and the checkout lives under
    /// CARGO_HOME: the checkout's stable token must come after CARGO_HOME's.
    #[test]
    fn the_checkout_mapping_comes_after_cargo_home() {
        let flags = remap_flags(
            Path::new("/scratch"),
            Path::new("/home/u/.cargo/git/checkouts/ducktape-1234/abcdef0"),
        );
        let flags: Vec<&str> = flags.split('\x1f').collect();
        let cargo_home = flags
            .iter()
            .position(|f| f.ends_with("=/cargo"))
            .expect("cargo home mapping");
        let checkout = flags
            .iter()
            .position(|f| f.ends_with("=/ducktape"))
            .expect("checkout mapping");
        assert!(checkout > cargo_home, "{flags:?}");
        assert_eq!(
            flags[checkout],
            "--remap-path-prefix=/home/u/.cargo/git/checkouts/ducktape-1234/abcdef0=/ducktape"
        );
    }

    #[test]
    fn the_checkout_root_is_the_manifest_minus_the_repository_place() {
        let root = checkout_root_of(
            Path::new("/home/u/.cargo/git/checkouts/ducktape-1234/abcdef0/crates/modules/apps/chat/Cargo.toml"),
            Path::new("crates/modules/apps/chat"),
        )
        .expect("root");
        assert_eq!(
            root,
            Path::new("/home/u/.cargo/git/checkouts/ducktape-1234/abcdef0")
        );

        let wrong_place = checkout_root_of(
            Path::new("/somewhere/else/tasks/Cargo.toml"),
            Path::new("crates/modules/apps/chat"),
        );
        assert!(wrong_place.is_err());
    }

    #[test]
    fn build_and_componentization_use_the_same_explicit_target_directory() {
        for root in [
            Path::new("/checkout/target/guest-builder/collaboration"),
            Path::new("relative/scratch"),
        ] {
            let command = build_command(root, "collaboration", GuestKind::Component, "");
            let args: Vec<_> = command.get_args().collect();
            let index = args
                .iter()
                .position(|arg| *arg == "--target-dir")
                .expect("an inherited CARGO_TARGET_DIR must not redirect compilation");
            let target = command.get_current_dir().unwrap().join(args[index + 1]);
            assert_eq!(
                cdylib_path(root, "collaboration", GuestKind::Component),
                target.join("wasm32-unknown-unknown/release/collaboration_component.wasm"),
                "componentization must read the artifact just compiled"
            );
        }
    }

    /// where a round of the redirection test sends cargo's output when the
    /// explicit selection is dropped.
    #[derive(Clone, Copy)]
    enum Redirect {
        Environment,
        Configuration,
    }

    /// An inherited `CARGO_TARGET_DIR` or a `build.target-dir` in
    /// configuration must not move the compiler's output away from the path
    /// the artifact lookup reads: a redirected build either cannot be found at
    /// all, or leaves an EARLIER build's bytes there to be packaged under this
    /// build's lock. Each round compiles a real cdylib through the production
    /// seam and reads back what would be packaged.
    #[test]
    fn a_redirected_cargo_output_cannot_package_an_earlier_build() {
        let work = scratch();
        let shell = work.path().join("shell");
        fixture_file(
            &shell,
            "Cargo.toml",
            "[workspace]\nmembers = [\"component\", \"index\"]\nresolver = \"2\"\n",
        );
        for kind in GuestKind::ALL {
            fixture_file(
                &shell,
                &format!("{}/Cargo.toml", kind.member()),
                &format!(
                    "[package]\nname = \"probe-{}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[lib]\ncrate-type = [\"cdylib\"]\n",
                    kind.member()
                ),
            );
            fixture_file(&shell, &format!("{}/src/lib.rs", kind.member()), "");
        }
        run_command(
            Command::new(cargo())
                .current_dir(&shell)
                .arg("generate-lockfile"),
        );

        let decoy = |redirect: Redirect| match redirect {
            Redirect::Environment => work.path().join("decoy-environment"),
            Redirect::Configuration => work.path().join("decoy-configuration"),
        };
        let redirect_output = |redirect: Redirect, command: &mut Command| match redirect {
            Redirect::Environment => {
                command.env("CARGO_TARGET_DIR", decoy(redirect));
            }
            Redirect::Configuration => {
                fixture_file(
                    &shell,
                    ".cargo/config.toml",
                    &format!(
                        "[build]\ntarget-dir = {:?}\n",
                        decoy(redirect).display().to_string()
                    ),
                );
            }
        };
        // the same build minus the explicit selection: what the redirection
        // does when nothing overrides it, so a round that redirects nothing
        // cannot pass for one that does.
        let unselected = |kind: GuestKind| {
            let reference = build_command(&shell, "probe", kind, "");
            let mut args: Vec<std::ffi::OsString> =
                reference.get_args().map(ToOwned::to_owned).collect();
            let selection = args
                .iter()
                .position(|arg| arg == "--target-dir")
                .expect("the build must select its target directory explicitly");
            args.drain(selection..selection + 2);
            let mut command = Command::new(cargo());
            command
                .args(args)
                .env("CARGO_ENCODED_RUSTFLAGS", "")
                .env_remove("RUSTFLAGS")
                .current_dir(&shell);
            command
        };

        let mut packaged: std::collections::HashMap<&str, Vec<u8>> = Default::default();
        // round 1 builds a clean scratch; round 2 finds round 1's bytes
        // sitting at the lookup path.
        for (value, redirect) in [(1u32, Redirect::Environment), (2, Redirect::Configuration)] {
            for kind in GuestKind::ALL {
                fixture_file(
                    &shell,
                    &format!("{}/src/lib.rs", kind.member()),
                    &format!("#[no_mangle]\npub extern \"C\" fn value() -> u32 {{ {value} }}\n"),
                );
            }
            let mut without_selection = unselected(GuestKind::Component);
            redirect_output(redirect, &mut without_selection);
            run_command(&mut without_selection);
            assert!(
                decoy(redirect)
                    .join("wasm32-unknown-unknown/release/probe_component.wasm")
                    .is_file(),
                "the redirection under test moved nothing"
            );

            for kind in GuestKind::ALL {
                let mut command = build_command(&shell, "probe", kind, "");
                redirect_output(redirect, &mut command);
                run_command(&mut command);
                let artifact = cdylib_path(&shell, "probe", kind);
                let bytes =
                    fs::read(&artifact).unwrap_or_else(|e| panic!("{}: {e}", artifact.display()));
                assert_ne!(
                    packaged.get(kind.member()),
                    Some(&bytes),
                    "{}: the lookup still holds the earlier build's bytes",
                    kind.member()
                );
                packaged.insert(kind.member(), bytes);
            }
        }
    }

    fn scratch() -> tempfile::TempDir {
        let root = default_platform_root()
            .unwrap()
            .join("target/guest-builder-tests");
        fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }

    fn run_command(command: &mut Command) -> std::process::Output {
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = run_command(Command::new("git").arg("-C").arg(repo).args(args));
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn fixture_file(root: &Path, path: &str, content: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn platform_fixture(root: &Path) -> (Module, String, String) {
        fs::create_dir_all(root).unwrap();
        git(root, &["init", "--initial-branch=base"]);
        git(root, &["config", "user.name", "Guest builder test"]);
        git(
            root,
            &["config", "user.email", "guest-builder@example.invalid"],
        );
        fixture_file(root, ".gitignore", "target/\n");
        fixture_file(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"shared\"]\nresolver = \"2\"\n",
        );
        fixture_file(
            root,
            "shared/Cargo.toml",
            "[package]\nname = \"shared\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        fixture_file(root, "shared/src/lib.rs", "pub fn value() -> u32 { 1 }\n");
        for (directory, name, version) in [
            ("random02", "getrandom", "0.2.17"),
            ("random03", "getrandom", "0.3.4"),
            ("random04", "getrandom", "0.4.3"),
            ("blst", "blst", "0.3.16"),
        ] {
            fixture_file(
                root,
                &format!("stubs/{directory}/Cargo.toml"),
                &format!(
                    "[package]\nname = {name:?}\nversion = {version:?}\nedition = \"2021\"\n[workspace]\n"
                ),
            );
            fixture_file(root, &format!("stubs/{directory}/src/lib.rs"), "");
        }
        git(root, &["add", "."]);
        git(
            root,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "Base without the new module",
            ],
        );
        // A second source with the same package name forces Cargo to spell
        // source IDs in dependency entries as well as package records.
        let other = root.parent().unwrap().join("other-shared");
        fs::create_dir_all(&other).unwrap();
        git(&other, &["init", "--initial-branch=base"]);
        git(&other, &["config", "user.name", "Guest builder test"]);
        git(
            &other,
            &["config", "user.email", "guest-builder@example.invalid"],
        );
        fixture_file(
            &other,
            "Cargo.toml",
            "[package]\nname = \"shared\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n",
        );
        fixture_file(&other, "src/lib.rs", "pub fn value() -> u32 { 2 }\n");
        git(&other, &["add", "."]);
        git(
            &other,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "Independent shared package",
            ],
        );
        git(root, &["switch", "-c", "module"]);
        fixture_file(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"shared\", \"module\"]\nresolver = \"2\"\n",
        );
        fixture_file(
            root,
            "module/Cargo.toml",
            "[package]\nname = \"new-module\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\nguest = []\n[dependencies]\nshared = { path = \"../shared\" }\n",
        );
        let manifest = root.join("module/Cargo.toml");
        let mut content = fs::read_to_string(&manifest).unwrap();
        content.push_str(&format!(
            "other-shared = {{ package = \"shared\", git = \"file://{}\" }}\n",
            other.display()
        ));
        fs::write(manifest, content).unwrap();
        fixture_file(
            root,
            "module/src/lib.rs",
            "pub fn value() -> u32 { shared::value() }\n",
        );
        git(root, &["add", "."]);
        git(
            root,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "Introduce a guest on its branch",
            ],
        );
        let rev = git(root, &["rev-parse", "HEAD"]);
        git(root, &["switch", "base"]);
        let module = Module {
            name: "new-module".into(),
            path: "module".into(),
            guests: vec![GuestKind::Component],
        };
        (module, format!("file://{}", root.display()), rev)
    }

    #[test]
    fn first_build_resolves_a_package_absent_from_the_default_branch() {
        let work = scratch();
        let repo = work.path().join("platform");
        let (module, url, rev) = platform_fixture(&repo);
        let shell = work.path().join("shell");
        fixture_file(
            &shell,
            "Cargo.lock",
            "stale scratch state must not seed a first build",
        );
        seed_lock(&shell, &repo.join("module")).unwrap();
        assert!(!shell.join("Cargo.lock").exists());
        let graph = pin(&shell, &module, &url, &rev).unwrap();
        let checkout = checkout_root(&graph, &module).unwrap();
        assert!(checkout.join("module/src/lib.rs").is_file());
        let lock = fs::read_to_string(shell.join("Cargo.lock")).unwrap();
        assert!(lock.contains(&format!("git+{url}#{rev}")));
        assert!(!lock.contains("?rev="));
        assert!(!fs::read_to_string(shell.join("component/Cargo.toml"))
            .unwrap()
            .contains("rev ="));
        run_command(Command::new(cargo()).current_dir(&shell).args([
            "check",
            "--locked",
            "--target",
            "wasm32-unknown-unknown",
        ]));
        assert_eq!(lock, fs::read_to_string(shell.join("Cargo.lock")).unwrap());
    }

    #[test]
    fn resolved_inputs_refuse_shared_staged_and_untracked_sources() {
        let work = scratch();
        let repo = work.path().join("platform");
        let (module, url, rev) = platform_fixture(&repo);
        let shell = work.path().join("shell");
        let graph = pin(&shell, &module, &url, &rev).unwrap();
        let checkout = checkout_root(&graph, &module).unwrap();
        let inputs = platform_inputs(&graph, &checkout, &url).unwrap();
        assert!(inputs.contains(Path::new("shared")));
        git(&repo, &["switch", "module"]);
        refuse_modified_sources(&repo, &inputs).unwrap();
        for path in [
            "module/component.wasm",
            "module/index.wasm",
            "module/guest.lock",
            "unrelated/src/lib.rs",
        ] {
            fixture_file(&repo, path, "not a source in this guest's graph");
        }
        refuse_modified_sources(&repo, &inputs).unwrap();
        fixture_file(&repo, "shared/src/lib.rs", "pub fn value() -> u32 { 2 }\n");
        assert!(refuse_modified_sources(&repo, &inputs)
            .unwrap_err()
            .contains("shared/src/lib.rs"));
        git(&repo, &["add", "shared/src/lib.rs"]);
        assert!(refuse_modified_sources(&repo, &inputs)
            .unwrap_err()
            .contains("shared/src/lib.rs"));
        git(
            &repo,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "Change shared source",
            ],
        );
        fixture_file(&repo, "shared/src/new.rs", "pub const VALUE: u32 = 3;\n");
        assert!(refuse_modified_sources(&repo, &inputs)
            .unwrap_err()
            .contains("shared/src/new.rs"));
        fs::remove_file(repo.join("shared/src/new.rs")).unwrap();
        fixture_file(
            &repo,
            ".cargo/config.toml",
            "[build]\nincremental = false\n",
        );
        assert!(refuse_modified_sources(&repo, &inputs)
            .unwrap_err()
            .contains(".cargo/config.toml"));
    }

    #[test]
    fn both_macros_compile_with_only_the_module_sdk_platform_dependency() {
        let work = scratch();
        let sdk = default_platform_root().unwrap().join("crates/module-sdk");
        fixture_file(
            work.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"store\", \"snapshot\"]\nresolver = \"2\"\n",
        );
        let implementation = r#"
use ducktape_module_sdk::sdk;
struct Example;
#[async_trait::async_trait(?Send)]
impl sdk::Module for Example {
    fn id(&self) -> sdk::ModuleId { "example".into() }
    fn root(&self) -> sdk::StateRoot { sdk::StateRoot([0; 32]) }
    async fn execute(&mut self, _ctx: &mut dyn sdk::Ctx, _msg: &sdk::Msg) -> Result<(), sdk::Error> { Ok(()) }
}
"#;
        for kind in ["store", "snapshot"] {
            fixture_file(
                work.path(),
                &format!("{kind}/Cargo.toml"),
                &format!(
                    "[package]\nname = \"{kind}-macro-check\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[lib]\ncrate-type = [\"cdylib\"]\n[dependencies]\nducktape-module-sdk = {{ path = {sdk:?} }}\nasync-trait = \"0.1\"\n"
                ),
            );
            let snapshot = match kind {
                "snapshot" => {
                    r#"
impl Example {
    fn snapshot(&self) -> Vec<u8> { Vec::new() }
    fn install(&mut self, _bytes: &[u8], _root: sdk::StateRoot) -> Result<(), sdk::Error> { Ok(()) }
}
"#
                }
                _ => "",
            };
            fixture_file(
                work.path(),
                &format!("{kind}/src/lib.rs"),
                &format!(
                    "{implementation}\n{snapshot}\nducktape_module_sdk::{kind}_guest! {{ id: \"example\", module: Example, shape: ducktape_module_sdk::map_shape(), new: Example }}\n"
                ),
            );
        }
        run_command(Command::new(cargo()).current_dir(work.path()).args([
            "check",
            "--workspace",
            "--target",
            "wasm32-unknown-unknown",
        ]));
    }
}
