//! the `ducktape-simnode` binary — a thin CLI over [`simnode::boot`]. it parses
//! the same flags as before, defaults storage to a fresh per-pid temp dir, then
//! runs the embeddable engine with the process-global log subscriber installed.
//! all node logic lives in the library; this file is arg parsing + program
//! output (the CLI's stdout lines) + turning a fatal into exit 1.

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;

use simnode::{Persona, SimOpts};
use workspace_config::DEFAULT_SIMNODE_LISTEN;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut listen: SocketAddr = DEFAULT_SIMNODE_LISTEN.parse()?;
    let mut storage: Option<PathBuf> = None;
    let mut auto = false;
    let mut persona = Persona::Local;
    let mut echo_oracle = false;
    // opt-in governance genesis: empty valset_keys => the default 16-module set.
    // both are meaningful only together; the binding defaults to "sim".
    let mut valset_keys: Vec<Vec<u8>> = Vec::new();
    let mut invite_binding: Vec<u8> = b"sim".to_vec();
    // the fabricated mesh identity `status().public_key` serves — None by
    // default (no mesh), a canonical 64-hex string under `--node-key`.
    let mut node_key: Option<String> = None;
    // where the `<id>.component.wasm` tenants come from — None composes from
    // the repo's kernel fixtures.
    let mut modules_dir: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next().ok_or("--listen needs an addr")?.parse()?,
            "--storage" => storage = args.next().map(PathBuf::from),
            "--auto" => auto = true,
            "--persona" => {
                persona = match args.next().as_deref() {
                    Some("local") => Persona::Local,
                    Some("networked") => Persona::Networked,
                    other => {
                        return Err(
                            format!("--persona wants local|networked, got {other:?}").into()
                        );
                    }
                }
            }
            "--echo-oracle" => echo_oracle = true,
            // comma-separated hex ed25519 pubkeys, and repeatable — each 32-byte
            // key genesis-seeds the validator set. a malformed or wrong-length
            // key fails loud here, never silently seeds junk.
            "--with-valset" => {
                let spec = args
                    .next()
                    .ok_or("--with-valset needs comma-separated hex ed25519 pubkeys")?;
                for hex in spec.split(',').filter(|s| !s.is_empty()) {
                    let key = duckfs_core::unhex(hex)
                        .map_err(|e| format!("--with-valset key {hex:?}: {e}"))?;
                    if key.len() != 32 {
                        return Err(format!(
                            "--with-valset key {hex:?} decodes to {} bytes, want a 32-byte ed25519 pubkey",
                            key.len()
                        )
                        .into());
                    }
                    valset_keys.push(key);
                }
            }
            "--invite-binding" => {
                invite_binding = args
                    .next()
                    .ok_or("--invite-binding needs a string")?
                    .into_bytes();
            }
            // fabricate a mesh identity for consensus-op scenarios (huddle
            // membership names a node key). validated to 32 bytes here and
            // stored canonical (lowercase hex); junk fails loud, never seeds a
            // malformed key clients would try to route to.
            "--node-key" => {
                let hex = args
                    .next()
                    .ok_or("--node-key needs a 64-hex ed25519 pubkey")?;
                let key =
                    duckfs_core::unhex(&hex).map_err(|e| format!("--node-key {hex:?}: {e}"))?;
                if key.len() != 32 {
                    return Err(format!(
                        "--node-key {hex:?} decodes to {} bytes, want a 32-byte ed25519 pubkey",
                        key.len()
                    )
                    .into());
                }
                node_key = Some(noded::hex_bytes(&key));
            }
            "--modules" => {
                modules_dir = Some(PathBuf::from(args.next().ok_or("--modules needs a dir")?));
            }
            other => {
                return Err(format!(
                    "unexpected arg {other:?} (want --listen/--storage/--auto/--persona/\
                     --echo-oracle/--with-valset/--invite-binding/--node-key/--modules)"
                )
                .into());
            }
        }
    }
    // a fresh dir per run is the determinism contract; a reused dir is a
    // supported restart (resume above the index watermark).
    let storage = storage.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("ducktape-simnode-{}", std::process::id()))
    });
    let persona_label = format!("{persona:?}");
    let hold = !auto;

    let opts = SimOpts {
        auto,
        echo_oracle,
        valset_keys,
        invite_binding,
        node_key,
        persona,
        modules_dir,
        // the binary installs noded's process-global tracing subscriber (an
        // embedder does not — see SimOpts::install_log).
        install_log: true,
    };

    let handle = simnode::boot(&storage, listen, opts).unwrap_or_else(|error| {
        eprintln!("simnode boot failed: {error}");
        std::process::exit(1);
    });
    // Machine-readable readiness for child-process drivers. `:0` is the only
    // collision-free reservation across the genesis-to-bind gap, so report the
    // actual port only after `boot` owns its listener.
    println!("DUCKTAPE_SIMNODE_LISTEN={}", handle.addr());
    std::io::stdout().flush()?;
    tracing::info!(
        target: "ducktape::node",
        listen = %handle.addr(),
        storage = %storage.display(),
        hold,
        persona = %persona_label,
        "simnode listening"
    );
    match handle.wait() {
        Ok(()) => {
            tracing::info!(target: "ducktape::node", "simnode shutdown requested; exiting");
            Ok(())
        }
        Err(reason) => {
            tracing::error!(
                target: "ducktape::node",
                error = %reason,
                "simnode fatal"
            );
            std::process::exit(1);
        }
    }
}
