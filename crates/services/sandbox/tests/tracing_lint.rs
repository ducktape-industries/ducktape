//! The sandbox lane's logging contract, guarded in source.
//!
//! This crate shipped with `tracing` in its `Cargo.toml` and **not one call** in
//! its source: a compute daemon that booted ten microVMs and executed ten
//! provider runs wrote a seven-line log, so a failed run's only evidence was
//! on-chain state and whatever `ps` caught before the VMM exited. The backend's
//! own plan had specified the handle and nothing held the code to it.
//!
//! Two rules, both from `CLAUDE.md`'s Logging section, both cheap to check and
//! impossible to keep by comment:
//!
//! 1. **Every event carries `target: "ducktape::sandbox"`.** The target is the
//!    filtering handle — `RUST_LOG=ducktape::sandbox=debug` has to light up this
//!    plane, which a crate-path target cannot express.
//! 2. **Every `warn!`/`error!` carries a `reason`.** A refusal that cannot be
//!    counted is prose; the `reason` token is what makes "this node refuses
//!    every run" distinguishable from "this one tree was too big".
//!
//! And the non-silence floor: the two files that own a run's lifecycle must
//! each say something, so the crate can never go quiet again by attrition.

use std::path::{Path, PathBuf};

/// the macros whose call sites this lint governs.
const EVENT_MACROS: &[&str] = &[
    "tracing::error!",
    "tracing::warn!",
    "tracing::info!",
    "tracing::debug!",
    "tracing::trace!",
];

/// the levels that must name a stable `reason` token.
const NEEDS_REASON: &[&str] = &["tracing::error!", "tracing::warn!"];

/// files that must not be silent: they own the run lifecycle.
const MUST_SPEAK: &[&str] = &["microvm.rs", "workspace_image.rs"];

/// files this crate SHARES VERBATIM with the guest's PID 1.
///
/// `bin/duck-guest-init/src/main.rs` `#[path]`-includes each of these, and that
/// binary depends on libc + serde only — deliberately, so PID 1 does not drag
/// tokio and tracing into a microVM. One `tracing::` call here breaks the guest
/// build, and `cargo clippy -p sandbox-host --tests --no-deps` — the gate this
/// crate is held to — cannot see it, because nothing in THIS crate's build
/// touches the guest binary.
const SHARED_WITH_GUEST_INIT: &[&str] = &["guest_manifest.rs", "guest_proto.rs", "guest_paths.rs"];

/// the whole crate's `info` budget: the daemon-boot probe and the executor
/// image (re)build, and nothing else. Both are per-{boot} facts — the image is
/// rebuilt only when the executors directory changed since the last build.
///
/// A run is per-{run}, not per-{boot}: promoting one of its lines to `info` for
/// visibility puts it in a 4096-line ring that a busy node evicts in minutes,
/// destroying the context around the failure it was added to explain. The run's
/// single `info` lives on the daemon side, where the capability tag and run key
/// exist to make it worth the slot.
const INFO_BUDGET: usize = 2;

struct Event {
    file: PathBuf,
    line: usize,
    macro_name: &'static str,
    body: String,
}

fn source_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read the sandbox src dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// the text of one macro call, from its `(` to the matching `)`.
///
/// String literals are skipped whole, so a parenthesis inside a message never
/// closes the call early — the reason this is a scanner and not a `find(')')`.
fn call_body(src: &str, open: usize) -> String {
    let bytes = src.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in bytes.iter().enumerate().skip(open) {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return src[open..=offset].to_string();
                }
            }
            _ => {}
        }
    }
    src[open..].to_string()
}

fn events() -> Vec<Event> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    for file in source_files(&root) {
        let src = std::fs::read_to_string(&file).expect("read source file");
        for macro_name in EVENT_MACROS {
            let mut from = 0usize;
            while let Some(hit) = src[from..].find(macro_name) {
                let at = from + hit;
                let open = at + macro_name.len() - 1;
                let body = call_body(&src, open);
                found.push(Event {
                    file: file.clone(),
                    line: src[..at].lines().count(),
                    macro_name,
                    body,
                });
                from = at + macro_name.len();
            }
        }
    }
    found
}

#[test]
fn every_sandbox_event_names_the_plane_target() {
    let offenders: Vec<String> = events()
        .iter()
        .filter(|event| !event.body.contains("target: \"ducktape::sandbox\""))
        .map(|event| {
            format!(
                "{}:{} {}",
                event.file.display(),
                event.line,
                event.macro_name
            )
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "every sandbox event needs `target: \"ducktape::sandbox\"` — it is the \
         filtering handle `RUST_LOG=ducktape::sandbox=debug` turns on:\n{}",
        offenders.join("\n"),
    );
}

#[test]
fn every_refusal_carries_a_reason_token() {
    let offenders: Vec<String> = events()
        .iter()
        .filter(|event| NEEDS_REASON.contains(&event.macro_name))
        .filter(|event| !event.body.contains("reason"))
        .map(|event| {
            format!(
                "{}:{} {}",
                event.file.display(),
                event.line,
                event.macro_name
            )
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "a warn/error must carry a stable snake_case `reason` token so it can be \
         counted, not just read:\n{}",
        offenders.join("\n"),
    );
}

#[test]
fn the_run_lifecycle_is_not_silent() {
    let found = events();
    for name in MUST_SPEAK {
        let spoke = found
            .iter()
            .any(|event| event.file.file_name().and_then(|f| f.to_str()) == Some(name));
        assert!(
            spoke,
            "{name} owns part of a run's lifecycle and emits no event; this crate \
             once had zero tracing calls in ten files and a run left no trace at all",
        );
    }
}

#[test]
fn the_files_the_guest_init_includes_stay_tracing_free() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    for name in SHARED_WITH_GUEST_INIT {
        let path = root.join(name);
        let src = std::fs::read_to_string(&path).expect("read a guest-shared source file");
        for (n, line) in src.lines().enumerate() {
            if line.contains("tracing::") || line.contains("use tracing") {
                offenders.push(format!("{}:{} {}", path.display(), n + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these files are #[path]-included by bin/duck-guest-init, which depends on \
         libc + serde only — a tracing call here breaks the guest's PID 1 build, and \
         this crate's own lint gate would never notice:\n{}",
        offenders.join("\n"),
    );
}

#[test]
fn the_crate_spends_exactly_its_info_budget() {
    let spent: Vec<String> = events()
        .iter()
        .filter(|event| event.macro_name == "tracing::info!")
        .map(|event| format!("{}:{}", event.file.display(), event.line))
        .collect();
    assert_eq!(
        spent.len(),
        INFO_BUDGET,
        "the sandbox crate's `info` budget is {INFO_BUDGET} (the daemon-boot probe and the executor image build). \
         Per-run and per-frame lines belong at debug/trace — an info that fires per run \
         evicts the ring it was added to explain:\n{}",
        spent.join("\n"),
    );
}

#[test]
fn nothing_in_the_sandbox_lane_prints() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    for file in source_files(&root) {
        let src = std::fs::read_to_string(&file).expect("read source file");
        for (n, line) in src.lines().enumerate() {
            if line.contains("println!(") || line.contains("eprintln!(") {
                offenders.push(format!("{}:{} {}", file.display(), n + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a print reaches neither the app's Logs tab nor `RUST_LOG`; use tracing:\n{}",
        offenders.join("\n"),
    );
}

#[test]
fn every_reason_is_a_snake_case_token() {
    let mut offenders = Vec::new();
    for event in events() {
        for token in reason_values(&event.body) {
            let snake = !token.is_empty()
                && token.starts_with(|c: char| c.is_ascii_lowercase())
                && token
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if !snake {
                offenders.push(format!(
                    "{}:{} reason = {token:?}",
                    event.file.display(),
                    event.line
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a `reason` is a stable snake_case token — greppable and countable, never prose:\n{}",
        offenders.join("\n"),
    );
}

/// every literal `reason = "..."` in one macro body.
fn reason_values(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(hit) = body[from..].find("reason = \"") {
        let start = from + hit + "reason = \"".len();
        let Some(end) = body[start..].find('"') else {
            return out;
        };
        out.push(body[start..start + end].to_string());
        from = start + end;
    }
    out
}

#[test]
fn no_run_event_logs_a_credential_or_a_uri_path() {
    // The guest's config home holds the per-run bearer, a vsock/uds path is
    // capability-bearing, and the manifest carries the run's argv and env.
    // None may reach the ring the app streams.
    const BANNED: &[&str] = &[
        "config_home",
        "credentials",
        "vsock_uds",
        "token",
        "manifest.env",
        "manifest.argv",
        "console_tail",
    ];
    let offenders: Vec<String> = events()
        .iter()
        .filter(|event| BANNED.iter().any(|banned| event.body.contains(banned)))
        .map(|event| {
            format!(
                "{}:{} {}",
                event.file.display(),
                event.line,
                event.macro_name
            )
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "a sandbox event names credential or capability-bearing state:\n{}",
        offenders.join("\n"),
    );
}
