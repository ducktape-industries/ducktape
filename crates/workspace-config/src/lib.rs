//! A ducktape workspace ON DISK: the identity file, the shareable network
//! descriptor, this device's `node.toml` plumbing, the invite codec — and the
//! [`join`] ceremony that writes all of them from a pasted blob.
//!
//! It sat inside `bin/node` until the desktop app needed to join a network
//! without spawning `ducktape node join` and diffing the workspace registry to
//! find out which directory the child had created. Reading and writing a
//! workspace is what two programs do; RESOLVING one into a runnable daemon is
//! what the daemon does, and that half stayed behind in `bin/node/src/config`.
//!
//! ## the two shapes
//!
//! **network shape** (production): `node.toml` names a `key_file` (a persisted
//! ed25519 identity) and a `network` descriptor file. `network.toml` is THE
//! shareable genesis artifact — identical content on every member is what
//! makes genesis deterministic, exactly like a chain-id + genesis file:
//! `chain_id` doubles as the commonware namespace (it domain-separates the
//! mesh handshake, the simplex scheme, and the epoch genesis floor), and
//! `validators` seeds the valset module at genesis.
//!
//! **dev-seed shape** (tests/demos/the app's generated solo config): the flat
//! `id`/`peer_seeds`/`validator_seeds` form, identities derived with
//! `PrivateKey::from_seed`. kept verbatim so examples and demo scripts stay
//! runnable; `resolve` treats a config WITHOUT a `network` field as this shape.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use commonware_codec::{DecodeExt as _, Encode as _};
use commonware_cryptography::{Signer as _, ed25519};
use commonware_p2p::Ingress;
use commonware_utils::Hostname;
use serde::{Deserialize, Serialize};
use tracing::warn;

// The submodules are public as well as flat-re-exported: `config/resolve.rs`
// back in bin/node reaches several of them by path (`node_toml::RawNodeToml`),
// and it is the one caller that wants the raw, pre-resolution shapes.
pub mod genesis;
pub mod staged_key;
mod view_files;
pub use view_files::{ensure_view_ready, read_deployment_files};
pub mod identity;
pub mod invite;
pub mod join;
pub mod node_toml;

pub use genesis::*;
pub use identity::*;
pub use invite::*;
pub use join::*;
pub use node_toml::*;

/// the guest artifacts a workspace's runs boot: `<workspace>/guest`, holding
/// the `vmlinux` and `rootfs.ext4` that `ops/build-guest-rootfs.sh` writes
/// there (`OUT=<workspace>/guest`).
///
/// Per workspace, never per host: two networks on one machine boot two
/// guests, and rebuilding one cannot change what the other runs. The
/// `[sandbox]` table names no path for the same reason — a config copied
/// between workspaces would otherwise point at images that are not there.
pub fn guest_dir(workspace: &Path) -> PathBuf {
    workspace.join("guest")
}

/// the guest kernel every run of this workspace boots.
pub fn guest_kernel(workspace: &Path) -> PathBuf {
    guest_dir(workspace).join("vmlinux")
}

/// the guest root filesystem every run of this workspace attaches read-only.
pub fn guest_rootfs(workspace: &Path) -> PathBuf {
    guest_dir(workspace).join("rootfs.ext4")
}

/// where `ducktape agent install` puts the agent CLIs a workspace's guest
/// lends to a run: `<workspace>/executors`. The node derives the guest's
/// read-only image from this directory (`sandbox_host::executor_image`), so
/// what the directory holds is exactly what a run can exec.
pub fn executor_dir(workspace: &Path) -> PathBuf {
    workspace.join("executors")
}

/// the operator capability specs a workspace's compute daemon offers: every
/// `*.toml` under `<workspace>/capabilities`
/// (`docs/records/specs/capability-spec.md`). An absent directory is a node
/// with no operator specs, not an error.
pub fn capability_dir(workspace: &Path) -> PathBuf {
    workspace.join("capabilities")
}

/// where the FOUNDING SET is read from — the directory of
/// `<id>.component.wasm`, `<id>.index.wasm` and `netstack.component.wasm`
/// files `node init` composes a genesis from, and the daemons that run no
/// network (noded, simnode, the dev shape) compose directly from:
/// `$DUCKTAPE_MODULES_DIR`, else the set the build staged beside this
/// executable under `staged` ([`staged_modules_dir`]).
///
/// `staged` is the name the build that linked this binary staged its set
/// under — `noded::services::STAGED_SET`, baked in by the stager's own run.
/// It is an argument because this crate has no build script to bake it with
/// (see `staged_key`).
///
/// This is the only place bare wasm files are read. A network's own wasm
/// lives in its workspace genesis, never under the ducktape home: two
/// networks carry two sets, and no directory outside a workspace decides
/// what its network runs.
pub fn modules_dir(staged: &str) -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("DUCKTAPE_MODULES_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let exe = std::env::current_exe().map_err(|e| format!("current executable: {e}"))?;
    staged_modules_dir(&exe, staged).ok_or_else(|| no_founding_set(&exe))
}

/// What a binary with no set beside it says. It names the ARTIFACT that is
/// missing, because on a release-only host nothing else can put one there: a
/// node release archive carries `modules/` next to `ducktape`, and a binary
/// lifted out of the archive on its own is the one way to end up here.
fn no_founding_set(exe: &Path) -> String {
    format!(
        "no founding set beside {} — a node release archive carries the set its binary was \
         built with as `modules/` beside `ducktape`, so unpack the WHOLE archive (download it \
         again if only the binary was copied); `ops/release/archive.sh --kind node` packs one \
         and `ducktape-node-launcher` unpacks it into <workspace>/updates/releases/<sha>/. In a \
         checkout, `cargo build` stages this checkout's own set beside the binaries it links \
         (target/<profile>/modules%<checkout>).",
        exe.display(),
    )
}

/// The simulator's packaged preset also includes its small KV test module.
/// An explicit modules directory remains the caller's complete artifact set.
pub fn sim_modules_dir(staged: &str) -> Result<PathBuf, String> {
    let configured = std::env::var_os("DUCKTAPE_MODULES_DIR");
    if let Some(dir) = configured {
        return Ok(PathBuf::from(dir));
    }
    Ok(sim_twin(&modules_dir(staged)?))
}

/// the simulation set beside a resolved founding set: this checkout's keyed
/// pair (`modules-<checkout>` -> `sim-modules-<checkout>`), or the installed
/// pair (`modules` -> `sim-modules`).
fn sim_twin(modules: &Path) -> PathBuf {
    let resolved = modules.file_name().and_then(|name| name.to_str());
    let sim = match resolved.and_then(|name| name.strip_prefix("modules")) {
        Some(key) => format!("sim-modules{key}"),
        None => "sim-modules".to_owned(),
    };
    modules.with_file_name(sim)
}

/// the founding set the build staged beside `exe` under the name `staged`:
/// `<exe dir>/<staged>` (a `cargo build` binary in `target/<profile>/`), else
/// `<exe dir>/../<staged>` (a test executable cargo runs from
/// `target/<profile>/deps/`) — then the unkeyed `modules` in the same two
/// places, the INSTALLED layout `make install-node`, a release archive and the
/// pinned dognet binaries have, where nothing else writes. `None` when none of
/// the four exists.
///
/// Nothing here reads a file to learn the name: the profile directory is
/// written by every checkout building into the target, so a name read from it
/// at boot is the last builder's, not this binary's.
pub fn staged_modules_dir(exe: &Path, staged: &str) -> Option<PathBuf> {
    let exe_dir = exe.parent()?;
    let deps_parent = exe_dir.parent();
    let candidates = [
        Some(exe_dir.join(staged)),
        deps_parent.map(|dir| dir.join(staged)),
        Some(exe_dir.join("modules")),
        deps_parent.map(|dir| dir.join("modules")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|candidate| candidate.is_dir())
}

/// The build that staged `set`, as `crates/noded/build.rs` recorded it, or
/// `None` for a set with no record (an installed layout, or one staged before
/// this file existed).
///
/// A set is not a shared thing, and a profile directory is shared, so the
/// record is what lets a reader tell its own set from a stranger's.
pub fn staged_by(set: &Path) -> Option<String> {
    let recorded = std::fs::read_to_string(set.join(staged_key::STAGED_OWNER)).ok()?;
    Some(recorded.trim().to_owned())
}

/// default recovery checkpoint cadence: small enough that boot replay stays
/// cheap, large enough that snapshotting the in-memory cohort is amortized.
pub const DEFAULT_CHECKPOINT_BLOCKS: u64 = 32;

/// default idle block cadence, in milliseconds: one block a second. every
/// consensus timer is a fixed multiple of it (`consensus::Cadence`).
pub const DEFAULT_BLOCK_TIME_MS: u64 = 1_000;

// hex codecs for keys, roots, and pasted blobs — the shared home is
// duckfs-core (`to_hex`/`unhex`); these re-exports keep the long-standing
// `config::hex_bytes`/`config::unhex` call sites working unchanged.
pub use duckfs_core::{to_hex as hex_bytes, unhex};

// ============================================================================
// the network descriptor — network.toml, the shareable genesis artifact.
// ============================================================================

/// one genesis module: the consensus-visible id and the sha256 (hex) of the
/// component bytes every node seeds into the code registry at block zero.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModuleCode {
    /// the consensus-visible module id, the host registry key.
    pub id: String,
    /// sha256 of the complete deployment, 64 hex chars; IN the genesis fingerprint.
    pub code_hash: String,
}

/// A module id is one filesystem name and contains none of the genesis
/// fingerprint's delimiters. The same id addresses a module in consensus
/// state, a descriptor, and the workspace's artifact directory.
pub fn validate_module_id(id: &str) -> Result<(), String> {
    let is_empty = id.is_empty();
    let has_delimiter = id.contains('=') || id.contains('\n');
    let has_path_separator = id.contains('/') || id.contains('\\') || id.contains('\0');
    let is_relative_directory = matches!(id, "." | "..");
    let invalid = is_empty || has_delimiter || has_path_separator || is_relative_directory;
    if invalid {
        return Err(format!(
            "module id {id:?} is not a bare identifier (empty, a path, or contains '=' / newline)"
        ));
    }
    Ok(())
}

/// a chain id has the shape `mint_chain_id` produces: `<name>#<8 lowercase
/// hex>`. Checked on a chain id that reaches the program from outside it
/// (an invite's descriptor) — a shape `node init` never mints (no `#`, more
/// than one, or a non-hex/non-lowercase/wrong-length suffix) is refused
/// rather than trusted to sit next to a normally-minted id in the registry.
pub fn validate_chain_id_shape(id: &str) -> Result<(), String> {
    let malformed = || format!("chain id {id:?} is not <name>#<8 hex> — refusing to join it");
    let (name, salt) = id.split_once('#').ok_or_else(malformed)?;
    let is_hex_salt = salt.len() == 8 && salt.bytes().all(|b| b.is_ascii_hexdigit());
    let is_lowercase = salt.chars().all(|c| !c.is_ascii_uppercase());
    if name.is_empty() || !is_hex_salt || !is_lowercase {
        return Err(malformed());
    }
    Ok(())
}

/// the floor a beat must clear: the idle heartbeat (`pump_heartbeat` in
/// `bin/node/src/validator/run/drain.rs`) only ever fires from the drain
/// pump's own tick (`DRAIN_TICK` in `bin/node/src/constants.rs`), so a beat
/// under this many ms cannot land as the one idle block per interval that
/// `Cadence::idle_hold` (`crates/kernel/consensus/src/lib.rs`) requires — it
/// would land as a nullify + finalize instead. `bin/node/src/constants.rs`
/// carries a const assertion that `DRAIN_TICK` agrees with this value.
pub const MIN_BLOCK_TIME_MS: u64 = 100;

/// a beat under the drain-tick floor is not a cadence the idle heartbeat can
/// keep: every consensus timer is a multiple of it, so an unpaceable beat
/// either hot-spins (at zero) or breaks `idle_hold` (below the floor).
/// checked at every boundary a descriptor enters through, exactly like
/// [`validate_module_id`].
pub fn validate_block_time_ms(ms: u64) -> Result<(), String> {
    if ms < MIN_BLOCK_TIME_MS {
        return Err(format!(
            "block_time_ms must be at least {MIN_BLOCK_TIME_MS}ms (the node's idle drain tick)"
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NetworkDescriptor {
    /// human name + salt (e.g. "ducktape#a1b2c3d4"); doubles as the namespace.
    pub chain_id: String,
    /// genesis validator identities, hex ed25519 public keys. a SET — order
    /// never affects genesis (valset is order-independent) — kept sorted for
    /// stable file diffs.
    pub validators: Vec<String>,
    /// dial hints, "hexpubkey@host:port". advisory: joiners bootstrap off the
    /// first reachable entry that is not themselves.
    #[serde(default)]
    pub bootstrap: Vec<String>,
    /// typed reach hints, canonical strings like `direct:<hex>@host:port`.
    /// advisory and EXCLUDED from the genesis fingerprint, exactly like
    /// `bootstrap`. empty for bootstrap-only descriptors — then
    /// [`NetworkDescriptor::reach_hints`] synthesises all-`Direct` hints from
    /// `bootstrap`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reach: Vec<String>,
    /// Coordination privacy for the reachability plane. `None` => `Private`
    /// (the safer default). Operational policy, parsed like the reach hints —
    /// NOT part of `genesis_namespace` (validator identity only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordination: Option<String>,
    /// sha256 (hex) of the network's genesis file — the whole of it, every
    /// component and index guest. IN the genesis fingerprint: the network IS
    /// its genesis, so a joiner holding any other file cannot even handshake.
    /// `node init` writes the file beside this descriptor; a joiner installs
    /// it (`install_genesis`) only after the bytes hash to this.
    pub genesis: String,
    /// milliseconds between idle blocks — the network's BEAT, stated once by
    /// the founder (`node init --block-time-ms`) and inherited by every
    /// joiner. IN the genesis fingerprint because the invariant it carries is
    /// cross-node: every simplex timer is a fixed multiple of the local beat
    /// (`consensus::Cadence`), so a member running a faster one nullifies the
    /// very views a slower leader fills on its own heartbeat. One beat per
    /// network, or the members disagree about when a view is late.
    pub block_time_ms: u64,
    /// the genesis wasm set: `(id, sha256 hex)` per wasm tenant, sorted by id.
    /// IN the genesis fingerprint — a node built against different components
    /// is a different network, never a block-0 fork. the bytes are the
    /// genesis file's; this table is what the code registry seeds at block
    /// zero and what every component is checked against by name.
    ///
    /// KEEP THIS LAST: toml serializes an array-of-tables after every scalar,
    /// so a scalar declared below it would be written INSIDE the final
    /// `[[modules]]` table and read back as one of its keys.
    pub modules: Vec<ModuleCode>,
}

impl NetworkDescriptor {
    pub fn from_toml(text: &str) -> Result<Self, String> {
        let mut d: Self = toml::from_str(text).map_err(|e| format!("network descriptor: {e}"))?;
        // canonicalize at the boundary: hex is many-to-one under decode
        // (case, whitespace), so every descriptor INSIDE the program carries
        // trimmed, lowercase, sorted validator entries — string comparisons
        // (admit, membership hints) and the genesis fingerprint then agree
        // with what decode_key actually accepts.
        for v in &mut d.validators {
            *v = v.trim().to_ascii_lowercase();
        }
        d.validators.sort();
        for m in &mut d.modules {
            validate_module_id(&m.id)?;
            m.code_hash = m.code_hash.trim().to_ascii_lowercase();
        }
        d.modules.sort_by(|a, b| a.id.cmp(&b.id));
        d.genesis = d.genesis.trim().to_ascii_lowercase();
        validate_block_time_ms(d.block_time_ms)?;
        Ok(d)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("descriptor serializes")
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("read {path:?}: {e}"))?;
        Self::from_toml(&text)
    }

    /// write the descriptor via tmp-file + rename: this is the workspace's
    /// identity file, rewritten on every invite mint and admit — a crash
    /// mid-write must never leave a torn `network.toml` (see genesis.rs's
    /// own `write_atomic`, which this shares).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        genesis::write_atomic(path, self.to_toml().as_bytes())
    }

    pub fn validator_keys(&self) -> Result<Vec<ed25519::PublicKey>, String> {
        // dedup on the DECODED key: hex spelling is many-to-one (case), and a
        // duplicate that slipped through here would panic much later at
        // run_node's Set::try_from.
        let keys: Vec<ed25519::PublicKey> = self
            .validators
            .iter()
            .map(|h| decode_key(h))
            .collect::<Result<_, _>>()?;
        let mut seen = std::collections::BTreeSet::new();
        for k in &keys {
            if !seen.insert(k.as_ref().to_vec()) {
                return Err(format!(
                    "duplicate validator {} in network {}",
                    hex_bytes(k.as_ref()),
                    self.chain_id
                ));
            }
        }
        Ok(keys)
    }

    /// the namespace this network's nodes actually run under: the chain-id
    /// plus a GENESIS FINGERPRINT (sha256 over the sorted validator set +
    /// the sorted `id=code_hash` module lines + the genesis pin + the beat;
    /// bootstrap hints excluded — they are advisory and legitimately differ
    /// between members). because the
    /// namespace domain-separates the mesh handshake, the simplex scheme, and
    /// the epoch genesis floor, a member holding a STALE descriptor (e.g. it
    /// missed a pre-genesis `admit` and kept the old validator list, or was
    /// built against a different wasm set) cannot even connect — genesis
    /// divergence is a loud connectivity failure, never a silent state fork.
    pub fn genesis_namespace(&self) -> String {
        use commonware_cryptography::{Hasher as _, Sha256};
        // canonical form regardless of how the struct was built (from_toml
        // normalizes, but a hand-constructed descriptor must fingerprint
        // identically): trimmed, lowercased, sorted.
        let mut sorted: Vec<String> = self
            .validators
            .iter()
            .map(|v| v.trim().to_ascii_lowercase())
            .collect();
        sorted.sort();
        let mut hasher = Sha256::default();
        hasher.update(b"ducktape:genesis:v1:");
        for v in &sorted {
            hasher.update(b"\n");
            hasher.update(v.as_bytes());
        }
        let mut modules: Vec<&ModuleCode> = self.modules.iter().collect();
        modules.sort_by(|a, b| a.id.cmp(&b.id));
        for m in modules {
            hasher.update(b"\n");
            hasher.update(m.id.as_bytes());
            hasher.update(b"=");
            hasher.update(m.code_hash.trim().to_ascii_lowercase().as_bytes());
        }
        hasher.update(b"\ngenesis=");
        hasher.update(self.genesis.trim().to_ascii_lowercase().as_bytes());
        hasher.update(b"\nblock_time_ms=");
        hasher.update(self.block_time_ms.to_string().as_bytes());
        let (_, digest) = hasher.finalize();
        // 128 bits: a 32-bit suffix is grindable (~2^32 hashes finds an
        // admitted key that leaves the fingerprint unchanged, resurrecting
        // the silent stale-descriptor fork this exists to prevent).
        format!("{}@{}", self.chain_id, hex_bytes(&digest.as_ref()[..16]))
    }

    /// the genesis file's pin as raw bytes; a descriptor whose `genesis` is
    /// not 32 hex bytes names the network, because nothing can be founded or
    /// joined from it.
    pub fn genesis_hash(&self) -> Result<[u8; 32], String> {
        let bytes =
            unhex(&self.genesis).map_err(|e| format!("network {} genesis: {e}", self.chain_id))?;
        bytes
            .try_into()
            .map_err(|_| format!("network {} genesis is not a 32-byte sha256", self.chain_id))
    }

    /// the genesis code hashes, decoded: `id -> sha256`. a hash that is not 32
    /// bytes of hex names its module in the error.
    pub fn module_hashes(&self) -> Result<std::collections::BTreeMap<String, [u8; 32]>, String> {
        let mut out = std::collections::BTreeMap::new();
        for m in &self.modules {
            validate_module_id(&m.id)?;
            let bytes =
                unhex(&m.code_hash).map_err(|e| format!("module {} code_hash: {e}", m.id))?;
            let digest: [u8; 32] = bytes
                .try_into()
                .map_err(|_| format!("module {} code_hash is not 32 bytes", m.id))?;
            if out.insert(m.id.clone(), digest).is_some() {
                return Err(format!(
                    "duplicate module {} in network {}",
                    m.id, self.chain_id
                ));
            }
        }
        Ok(out)
    }

    /// bootstrap entries as dial INGRESSES. an IP literal becomes a socket
    /// ingress; a HOSTNAME stays a hostname (`Ingress::Dns`) and is re-resolved
    /// by the dialer at EVERY attempt — so `pubkey@node.example.com:443` keeps
    /// working when the tunnel behind the name moves, and an offline name never
    /// blocks startup. a MALFORMED entry (no `@`, bad key, not host:port) is a
    /// config error; an unspecified ip / port 0 is advisory and skipped. the
    /// live dial path is [`NetworkDescriptor::reach_entries`], which folds these
    /// Direct entries in alongside the typed `reach` hints.
    ///
    /// Public, though nothing on the boot path calls it: it is how a caller
    /// asks what the descriptor's raw hints PARSE to without going through
    /// resolution, which is exactly what `config::resolve`'s hostname tests
    /// need now that they live in a different crate from the descriptor.
    pub fn bootstrap_entries(&self) -> Result<Vec<(ed25519::PublicKey, Ingress)>, String> {
        let mut out = Vec::new();
        for entry in &self.bootstrap {
            let (key, host_port) = entry
                .split_once('@')
                .ok_or_else(|| format!("bootstrap entry {entry:?} is not pubkey@addr"))?;
            let key = decode_key(key)?;
            match ingress_of(host_port).map_err(|e| format!("bootstrap entry {entry:?}: {e}"))? {
                Some(ingress) => out.push((key, ingress)),
                None => continue, // unspecified ip / port 0 — advisory noise.
            }
        }
        Ok(out)
    }

    /// add a validator identity (pre-genesis membership — see `admit`).
    /// idempotent; keeps the list sorted.
    pub fn admit(&mut self, key: &ed25519::PublicKey) {
        let hex = hex_bytes(key.as_ref());
        if !self.validators.contains(&hex) {
            self.validators.push(hex);
            self.validators.sort();
        }
    }

    /// record a dial hint (`host:port`, an IP or a hostname) for `key`, replacing
    /// any previous hint for the same key (a member's advertised addr can move).
    ///
    /// Returns whether the entry set actually changed, so a caller that only
    /// rewrites the descriptor to record a bootstrap hint (every invite mint)
    /// can skip the write when this key already carries this address.
    pub fn add_bootstrap(&mut self, key: &ed25519::PublicKey, addr: &str) -> bool {
        let hex = hex_bytes(key.as_ref());
        let entry = format!("{hex}@{addr}");
        if self.bootstrap.contains(&entry) {
            return false;
        }
        self.bootstrap
            .retain(|e| !e.starts_with(&format!("{hex}@")));
        self.bootstrap.push(entry);
        self.bootstrap.sort();
        true
    }

    /// the reach hints, typed. if the descriptor carries explicit `reach`
    /// entries they parse to those; otherwise every `bootstrap` entry is a
    /// `Direct` hint (so a bootstrap-only descriptor yields all-`Direct`
    /// hints with no data duplicated and no double-dial).
    pub fn reach_hints(&self) -> Result<Vec<ReachHint>, String> {
        // the UNION of typed `reach` and untyped `bootstrap`: explicit typed
        // hints win over bootstrap-synthesised Direct hints for the same
        // member, but typed entries are a route set, not a per-key map. A
        // node may need both a rendezvous route and a tunnel-overlay route for
        // the same expected key.
        let mut bootstrap_by_key: std::collections::BTreeMap<Vec<u8>, ReachHint> =
            std::collections::BTreeMap::new();
        for entry in &self.bootstrap {
            let (k, addr) = entry
                .split_once('@')
                .ok_or_else(|| format!("bootstrap entry {entry:?} is not pubkey@addr"))?;
            let expected_key = decode_key(k)?;
            bootstrap_by_key.insert(
                expected_key.as_ref().to_vec(),
                ReachHint {
                    expected_key,
                    reach: Reach::Direct(addr.to_string()),
                },
            );
        }
        let mut typed = Vec::new();
        let mut typed_keys = std::collections::BTreeSet::new();
        for s in &self.reach {
            let hint = ReachHint::parse(s)?;
            // only a typed DIRECT route supersedes a bootstrap-
            // synthesised Direct for the same key (the member's dial address
            // moved/upgraded). a Coordinated route is an ADDITIONAL rendezvous
            // path, not a replacement — it must not erase a real direct dial
            // hint, or a founder that advertises a public address AND enables a
            // coordinator would ship coordinator-only reach and lose its direct
            // fallback (terminal once a punch fails, since there is no relay).
            // an overlay-ULA Direct (this chain's tunnel /48) is likewise an
            // ADDITIONAL route: it is only dialable once the reachability
            // plane's tunnels apply, so letting it evict the underlay hint
            // strands the member behind a plane that has not assembled yet
            // (first join, promotion reboot, same-host tests).
            if matches!(hint.reach, Reach::Direct(_)) && !self.overlay_route(&hint)? {
                typed_keys.insert(hint.expected_key.as_ref().to_vec());
            }
            typed.push(hint);
        }
        bootstrap_by_key.retain(|k, _| !typed_keys.contains(k));
        let mut out: Vec<_> = bootstrap_by_key.into_values().chain(typed).collect();
        out.sort_by_key(|h| h.to_canonical());
        Ok(out)
    }

    /// whether a hint's address lives inside this chain's overlay ULA /48 —
    /// the tunnel-plane addresses [`wireguard::ula_v6_member_addr`]
    /// derives. such a route needs applied tunnels to be dialable, so it is
    /// classified as an overlay route, never an underlay replacement.
    fn overlay_route(&self, hint: &ReachHint) -> Result<bool, String> {
        let addr = match &hint.reach {
            Reach::Direct(a) => a,
            Reach::Coordinated(_) => return Ok(false),
        };
        let Some(Ingress::Socket(sock)) = ingress_of(addr)? else {
            return Ok(false); // hostnames and advisory noise are underlay-class.
        };
        let std::net::IpAddr::V6(v6) = sock.ip() else {
            return Ok(false);
        };
        let prefix = wireguard::ula_v6_prefix(&self.genesis_namespace()).octets();
        Ok(v6.octets()[..6] == prefix[..6])
    }

    /// record a reach hint for a member, replacing any previous hint for the
    /// same expected key (a member's reach can move/upgrade). keeps the list
    /// sorted for stable file diffs — mirrors [`NetworkDescriptor::add_bootstrap`].
    pub fn add_reach(&mut self, hint: &ReachHint) {
        let ek = hex_bytes(hint.expected_key.as_ref());
        self.reach.retain(|s| {
            ReachHint::parse(s)
                .map(|h| hex_bytes(h.expected_key.as_ref()) != ek)
                .unwrap_or(true)
        });
        self.reach.push(hint.to_canonical());
        self.reach.sort();
    }

    /// record one explicit typed reach route without collapsing other typed
    /// routes for the same expected key. Use this when the descriptor needs a
    /// real route set, such as rendezvous plus a tunnel-overlay ingress.
    pub fn add_reach_route(&mut self, hint: &ReachHint) {
        let canonical = hint.to_canonical();
        if !self.reach.contains(&canonical) {
            self.reach.push(canonical);
            self.reach.sort();
        }
    }

    /// reach hints resolved to typed dial routes, hostname-native: `Direct`
    /// becomes an [`Ingress`] the mesh dials (a hostname stays a
    /// hostname, re-resolved per attempt); `Coordinated` becomes a route the
    /// nat client hole-punches through while still authenticating the
    /// target's own key end-to-end. advisory: an entry that cannot form a
    /// dialable ingress (unspecified ip / port 0 / malformed host) is skipped.
    pub fn reach_entries(&self) -> Result<Vec<(ed25519::PublicKey, ReachDial)>, String> {
        let mut out = Vec::new();
        // one DIRECT ingress per key, underlay preferred: the address book keeps one
        // dial address per peer, and an overlay ULA only answers once the
        // reachability plane's tunnels apply — so when a key carries both an
        // underlay route and its overlay route, the mesh dialer gets the
        // underlay and the plane owns the tunnel path. a key whose ONLY route
        // is the overlay (a fully-NATed member) still dials it, as before.
        let mut direct_at: std::collections::BTreeMap<Vec<u8>, (usize, bool)> =
            std::collections::BTreeMap::new();
        for hint in self.reach_hints()? {
            let dial = match &hint.reach {
                Reach::Direct(a) => match ingress_of(a)? {
                    Some(ingress) => ReachDial::Direct(ingress),
                    None => continue, // unspecified ip / port 0 — advisory noise.
                },
                Reach::Coordinated(c) => match ingress_of(&c.coord_addr)? {
                    Some(coord) => ReachDial::Coordinated {
                        coord,
                        coord_key: c.coord_key.clone(),
                    },
                    None => continue,
                },
            };
            if matches!(dial, ReachDial::Direct(_)) {
                let key = hint.expected_key.as_ref().to_vec();
                let overlay = self.overlay_route(&hint)?;
                match direct_at.get(&key) {
                    None => {
                        direct_at.insert(key, (out.len(), overlay));
                    }
                    Some(&(at, held_overlay)) => {
                        if held_overlay && !overlay {
                            // the held slot is the overlay route — replace it
                            // in place with the underlay one.
                            out[at] = (hint.expected_key.clone(), dial);
                            direct_at.insert(key, (at, false));
                        }
                        continue;
                    }
                }
            }
            out.push((hint.expected_key.clone(), dial));
        }
        Ok(out)
    }
}

/// coordination privacy for the reachability plane — per-network operational
/// policy (like `checkpoint_blocks`), NOT part of the genesis fingerprint.
/// `Public` = the coordinator admits any proof-of-possession request;
/// `Private` (the default) also requires a genesis-issued `CoordCap`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coordination {
    Public,
    Private,
}

/// the host of the shared public rendezvous coordinator (see
/// [`default_primary_coordinator`]).
const DEFAULT_COORDINATOR_HOST: &str = "relay.ducktape.industries";

/// Shared public rendezvous coordinator used when a network is created without
/// an explicit direct-only override: its host, on the port every coordinator
/// binds by default ([`nat_traversal::COORDINATOR_PORT`]).
pub fn default_primary_coordinator() -> String {
    format!(
        "{DEFAULT_COORDINATOR_HOST}:{}",
        nat_traversal::COORDINATOR_PORT
    )
}

/// The typed invite format still carries a coordinator key, but the deployed
/// coordinator is intentionally keyless. Keep one stable valid key in the
/// signed envelope until coordinator response signing exists.
pub fn keyless_coordinator_placeholder_key() -> ed25519::PublicKey {
    ed25519::PrivateKey::from_seed(0).public_key()
}

/// Resolve the primary coordinator option. `None` means "use the product
/// default"; `"none"`/`"off"` keeps the old direct-only posture.
pub fn primary_coordinator_or_default(raw: Option<&str>) -> Result<Option<String>, String> {
    let default = default_primary_coordinator();
    let coord = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&default);
    if matches!(coord, "none" | "off" | "direct") {
        return Ok(None);
    }
    match ingress_of(coord)? {
        Some(_) => Ok(Some(coord.to_string())),
        None => Err(format!("primary coordinator {coord:?} is not dialable")),
    }
}

/// Resolve the ambient coordinator to a dial [`Ingress`] — the AMBIENT source
/// a joiner's NAT resolver binds (config/default), never one carried in an
/// invite. `None` when coordination is disabled (`"none"`/`"off"`/`"direct"`).
pub fn coordinator_ingress(raw: Option<&str>) -> Result<Option<Ingress>, String> {
    match primary_coordinator_or_default(raw)? {
        Some(addr) => ingress_of(&addr),
        None => Ok(None),
    }
}

impl NetworkDescriptor {
    pub fn coordination(&self) -> Coordination {
        match self.coordination.as_deref() {
            Some("public") => Coordination::Public,
            _ => Coordination::Private,
        }
    }

    /// Make `key` reachable through the configured public coordinator. This is
    /// advisory reachability state, not part of the genesis fingerprint.
    pub fn apply_primary_coordinator(
        &mut self,
        key: &ed25519::PublicKey,
        coord_addr: &str,
    ) -> Result<(), String> {
        let coord_addr = primary_coordinator_or_default(Some(coord_addr))?
            .ok_or("primary coordinator cannot be disabled here")?;
        self.coordination = Some("public".into());
        self.add_reach(&ReachHint {
            expected_key: key.clone(),
            reach: Reach::Coordinated(CoordRef {
                coord_addr,
                coord_key: keyless_coordinator_placeholder_key(),
            }),
        });
        Ok(())
    }

    pub fn has_coordinated_reach(&self) -> Result<bool, String> {
        Ok(self
            .reach_hints()?
            .iter()
            .any(|h| matches!(h.reach, Reach::Coordinated(_))))
    }
}

/// a reach hint resolved to how the mesh actually reaches a member. `Direct`
/// dials the ingress and authenticates `expected_key` end-to-end;
/// `Coordinated` carries the coordinator's own ingress + identity so the nat
/// client can rendezvous and hole-punch to the target.
#[derive(Clone, Debug)]
pub enum ReachDial {
    Direct(Ingress),
    Coordinated {
        coord: Ingress,
        coord_key: ed25519::PublicKey,
    },
}

pub fn decode_key(hex: &str) -> Result<ed25519::PublicKey, String> {
    let digits = hex.trim();
    let not_a_key = |why: String| format!("{hex:?} is not an ed25519 public key: {why}");
    // Every way this fails is said here, in this CLI's words: the codec's own
    // refusal of a short buffer ("Unexpected End-of-Buffer") is written for
    // its author, not for someone who typed 8 characters where 64 belong.
    let expected = 2 * <ed25519::PublicKey as commonware_codec::FixedSize>::SIZE;
    let typed = digits.chars().count();
    if typed != expected {
        return Err(not_a_key(format!(
            "expected {expected} hex characters, got {typed}"
        )));
    }
    let raw = unhex(digits).map_err(not_a_key)?;
    ed25519::PublicKey::decode(raw.as_slice())
        .map_err(|_| not_a_key("not a valid ed25519 point".into()))
}

// ============================================================================
// coordinator capability — the private-mode admission token a node presents on
// each rendezvous request. Minted by a genesis validator (`mint_coord_cap`),
// persisted 0600 beside the descriptor like `invite.token`. Genesis validators
// need none (the coordinator's pinned set covers them).
// ============================================================================

const COORD_CAP_FILE: &str = "coord.cap";
const COORD_CAP_LEN: usize = 32 + 8 + 64;

pub fn pack_coord_cap(cap: &nat_traversal::CoordCap) -> Vec<u8> {
    let mut out = Vec::with_capacity(COORD_CAP_LEN);
    out.extend_from_slice(cap.issuer.as_ref());
    out.extend_from_slice(&cap.not_after.to_be_bytes());
    out.extend_from_slice(cap.issuer_sig.encode().as_ref());
    out
}

pub fn unpack_coord_cap(bytes: &[u8]) -> Result<nat_traversal::CoordCap, String> {
    if bytes.len() != COORD_CAP_LEN {
        return Err(format!(
            "coord cap must be {COORD_CAP_LEN} bytes, got {}",
            bytes.len()
        ));
    }
    let issuer =
        ed25519::PublicKey::decode(&bytes[..32]).map_err(|e| format!("coord cap issuer: {e}"))?;
    let mut na = [0u8; 8];
    na.copy_from_slice(&bytes[32..40]);
    let not_after = u64::from_be_bytes(na);
    let issuer_sig =
        ed25519::Signature::decode(&bytes[40..]).map_err(|e| format!("coord cap sig: {e}"))?;
    Ok(nat_traversal::CoordCap {
        issuer,
        not_after,
        issuer_sig,
    })
}

pub fn save_coord_cap(dir: &Path, cap: &nat_traversal::CoordCap) -> Result<(), String> {
    use std::io::Write as _;
    let path = dir.join(COORD_CAP_FILE);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&path)
        .map_err(|e| format!("create {path:?}: {e}"))?;
    f.write_all(format!("{}\n", hex_bytes(&pack_coord_cap(cap))).as_bytes())
        .map_err(|e| format!("write {path:?}: {e}"))
}

pub fn load_coord_cap(dir: &Path) -> Option<nat_traversal::CoordCap> {
    let path = dir.join(COORD_CAP_FILE);
    let raw = std::fs::read_to_string(&path).ok()?;
    let bytes = unhex(raw.trim()).ok()?;
    unpack_coord_cap(&bytes).ok()
}

/// guard a join against clobbering a DIFFERENT network's descriptor: a
/// workspace dir only ever holds one network. a refreshed invite for the
/// SAME network (the documented re-join after a pre-genesis admit) may
/// replace it; anything else is almost certainly a paste into the wrong dir —
/// and for a founder, an unrecoverable one (the time-salted chain-id cannot
/// be re-minted).
///
/// the chain-id alone cannot decide that: it is free text an invite carries
/// verbatim, it appears in every invite the network ever hands out, and
/// `decode_invite_at` proves only that a blob is self-consistent — never that
/// its issuer belongs to the network it names. so an attacker who copies a
/// victim's chain-id onto a descriptor of their OWN validators mints a blob
/// that resolves to the victim's registry directory and overwrites a live
/// network's descriptor, stranding its identity and history.
///
/// what actually identifies a network is the genesis fingerprint
/// ([`NetworkDescriptor::genesis_namespace`]) — the mesh handshake, the
/// simplex scheme and the epoch floor are all domain-separated by it, so two
/// descriptors that differ in it can never exchange a frame. the ONE
/// legitimate way it changes under a workspace is the pre-genesis `admit`
/// refresh, which only ever GROWS the validator set over the same genesis pin,
/// module set and beat. so that is the exemption — and it takes BOTH halves.
///
/// `issuer` is the invite's verified issuer: `decode_invite_at` checks the
/// envelope signature against it before anything here runs, so it is the one
/// value in a pasted blob an attacker cannot choose freely. ANY join into a
/// live workspace — the same-fingerprint re-join as much as the admit refresh
/// — is admitted only when that key is already a validator in the descriptor
/// ON DISK, the set we trusted before this blob arrived. an attacker who
/// copies our genesis facts into their own descriptor satisfies the SHAPE of a
/// re-join (and can match the fingerprint outright, since the reach hints,
/// coordination mode, fronts and WireGuard bootstrap it rewrites are all
/// outside it), but signing as one of those keys needs a key they do not have.
pub fn guard_join_descriptor(
    dir: &Path,
    incoming: &NetworkDescriptor,
    issuer: &ed25519::PublicKey,
) -> Result<(), String> {
    let path = dir.join("network.toml");
    if !path.exists() {
        return Ok(());
    }
    let existing = NetworkDescriptor::load(&path)?;
    if existing.chain_id != incoming.chain_id {
        return Err(format!(
            "{} already belongs to network {} — refusing to replace its descriptor with an \
             invite for {}; join a different network with a fresh --dir",
            dir.display(),
            existing.chain_id,
            incoming.chain_id
        ));
    }
    let ours = validator_set(&existing);
    let same_genesis = existing.genesis_namespace() == incoming.genesis_namespace();
    let admit_shape = existing.genesis == incoming.genesis
        && existing.modules == incoming.modules
        && existing.block_time_ms == incoming.block_time_ms
        && ours.is_subset(&validator_set(incoming));
    if !same_genesis && !admit_shape {
        return Err(format!(
            "foreign_genesis: {} already belongs to genesis {} — refusing to replace its \
             descriptor with an invite that reuses the chain-id {} over a DIFFERENT genesis {} \
             (its validators, genesis pin, modules or beat are not yours); that invite was not \
             issued by your network",
            dir.display(),
            existing.genesis_namespace(),
            incoming.chain_id,
            incoming.genesis_namespace(),
        ));
    }
    // BOTH legitimate shapes land here, and BOTH need the signature. a
    // matching fingerprint is not proof of membership: `genesis_namespace`
    // covers the chain-id, validators, modules, genesis pin and beat and
    // NOTHING else — `reach`, `coordination`, the invite's fronts and its
    // WireGuard bootstrap all ride outside it, and `join_workspace` writes
    // every one of them wholesale. so a stranger who copies a leaked invite's
    // genesis facts verbatim and re-points the hints at hosts they control
    // mints a blob whose fingerprint MATCHES ours. the envelope signature is
    // the one field they cannot choose.
    let signed_by_a_resident_validator = ours.contains(&hex_bytes(issuer.as_ref()));
    if !signed_by_a_resident_validator {
        return Err(format!(
            "join_not_signed_by_a_member: {} already belongs to genesis {} — the invite carries \
             the shape of a re-join, but it was signed by {}, which is not a validator of the \
             descriptor on disk; anyone can copy your validator list into their own descriptor, \
             only one of those validators can sign as one",
            dir.display(),
            existing.genesis_namespace(),
            hex_bytes(issuer.as_ref()),
        ));
    }
    Ok(())
}

/// the validator entries as [`NetworkDescriptor::genesis_namespace`]
/// fingerprints them — trimmed and lowercased, order-independent.
fn validator_set(d: &NetworkDescriptor) -> std::collections::BTreeSet<String> {
    d.validators
        .iter()
        .map(|v| v.trim().to_ascii_lowercase())
        .collect()
}

/// the `host:port` peers should dial, if one is real: prefer `advertised`, else
/// the listen addr when it is concrete. the returned STRING is what lands in the
/// descriptor verbatim — a HOSTNAME stays a hostname (resolved at dial time), so
/// `node.example.com:443` is a valid advertised addr. an IP that is UNSPECIFIED
/// (0.0.0.0/[::]) or on port 0 is never dialable — writing one would hand every
/// joiner a hint that resolves to their own loopback. an explicitly-passed
/// advertised that is not dialable is an ERROR (the caller asked for it); a
/// non-dialable listen just means "no hint" (Ok(None)). the `"overlay"`
/// sentinel is also "no hint", not an error: the overlay ULA is dialable only
/// over an established tunnel, so it must never be minted into an invite a
/// fresh joiner would dial cold.
pub fn dialable(advertised: Option<&str>, listen: &str) -> Result<Option<String>, String> {
    if let Some(a) = advertised {
        let a = a.trim();
        if a == "overlay" {
            return Ok(None);
        }
        match a.parse::<SocketAddr>() {
            // an IP literal must be concrete.
            Ok(addr) if addr.ip().is_unspecified() || addr.port() == 0 => {
                return Err(format!(
                    "advertised addr {addr} is not dialable (unspecified ip or port 0)"
                ));
            }
            Ok(_) => {}
            // not an IP → a hostname; require a non-empty host and a real port.
            Err(_) if !is_host_port(a) => {
                return Err(format!("advertised addr {a:?} is not host:port"));
            }
            Err(_) => {}
        }
        return Ok(Some(a.to_string()));
    }
    let l: SocketAddr = listen.parse().map_err(|e| format!("listen: {e}"))?;
    Ok((!l.ip().is_unspecified() && l.port() != 0).then(|| l.to_string()))
}

/// a lightweight `host:port` shape check for a non-IP advertised hint: a
/// non-empty host and a numeric, non-zero port. resolution happens later, at
/// dial time (see [`NetworkDescriptor::bootstrap_entries`]).
fn is_host_port(s: &str) -> bool {
    match s.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p != 0),
        None => false,
    }
}

/// parse a `host:port` into a dial ingress WITHOUT resolving anything: an IP
/// literal becomes `Ingress::Socket`; a host:port-shaped name stays a hostname
/// (`Ingress::Dns`), re-resolved by dialing peers at each attempt. `Ok(None)`
/// = syntactically fine but never dialable (unspecified ip / port 0); a shape
/// that is neither an ip nor host:port is an error.
pub fn ingress_of(host_port: &str) -> Result<Option<Ingress>, String> {
    if let Ok(addr) = host_port.parse::<SocketAddr>() {
        if addr.ip().is_unspecified() || addr.port() == 0 {
            return Ok(None);
        }
        return Ok(Some(Ingress::Socket(addr)));
    }
    let Some((host, port)) = host_port.rsplit_once(':') else {
        return Err(format!("{host_port:?} is not host:port"));
    };
    let port: u16 = port
        .parse()
        .map_err(|_| format!("{host_port:?} has no valid port"))?;
    if port == 0 {
        return Ok(None);
    }
    let host = Hostname::new(host).map_err(|e| format!("{host_port:?}: bad hostname: {e:?}"))?;
    Ok(Some(Ingress::Dns { host, port }))
}

// ============================================================================
// typed reachability — how to reach a member's REAL node.
// ============================================================================

/// how to reach a member's REAL node. advisory (never part of the genesis
/// fingerprint); the mesh still authenticates the peer by its ed25519 key
/// end-to-end regardless of which socket got dialed.
#[derive(Clone, Debug, PartialEq)]
pub enum Reach {
    /// dial this `host:port` directly (today's bootstrap behaviour).
    Direct(String),
    /// dial a coordinator (`coord_addr`) and ask it for a path to the target.
    Coordinated(CoordRef),
}

/// how to reach a coordinator, plus the key it authenticates its channel with.
#[derive(Clone, Debug, PartialEq)]
pub struct CoordRef {
    pub coord_addr: String,
    pub coord_key: ed25519::PublicKey,
}

/// a signed-invite reach hint: the REAL node identity a joiner must end up
/// authenticating, plus how to get a path to it.
#[derive(Clone, Debug, PartialEq)]
pub struct ReachHint {
    pub expected_key: ed25519::PublicKey,
    pub reach: Reach,
}

impl ReachHint {
    /// canonical single-line form stored in `network.toml`'s `reach` array and
    /// parsed by [`ReachHint::parse`]. `@` separates the expected key from the
    /// address, `#` separates a coordinator address from its key; neither char
    /// occurs in a host:port (IPv6 uses `[..]:port`), so the split is unambiguous.
    pub fn to_canonical(&self) -> String {
        let ek = hex_bytes(self.expected_key.as_ref());
        match &self.reach {
            Reach::Direct(a) => format!("direct:{ek}@{a}"),
            Reach::Coordinated(c) => {
                format!(
                    "coordinated:{ek}@{}#{}",
                    c.coord_addr,
                    hex_bytes(c.coord_key.as_ref())
                )
            }
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        let (tag, rest) = s
            .split_once(':')
            .ok_or_else(|| format!("reach hint {s:?} missing a tag"))?;
        let (ek_hex, addr_part) = rest
            .split_once('@')
            .ok_or_else(|| format!("reach hint {s:?} is not tag:key@addr"))?;
        let expected_key = decode_key(ek_hex)?;
        let reach = match tag {
            "direct" => Reach::Direct(addr_part.to_string()),
            "coordinated" => {
                let (coord_addr, ck_hex) = addr_part
                    .rsplit_once('#')
                    .ok_or_else(|| format!("coordinated hint {s:?} missing #coord_key"))?;
                Reach::Coordinated(CoordRef {
                    coord_addr: coord_addr.to_string(),
                    coord_key: decode_key(ck_hex)?,
                })
            }
            other => return Err(format!("unknown reach tag {other:?} in {s:?}")),
        };
        Ok(Self {
            expected_key,
            reach,
        })
    }
}

/// EVERY candidate statesync source, ordered: the invite's bootstrap hints
/// first (a dial path is already configured for them), then the descriptor's
/// validators. the rotating client fails over down this list — any peer that
/// serves the channel will do, because every payload verifies against
/// consensus-agreed roots.
///
/// a hint is NEVER filtered against the descriptor's validator list: that list
/// is the GENESIS one, frozen for the process's life, so filtering by it drops
/// exactly the peer a joiner needs once the founders have been rotated out.
/// the live set replaces this seed as soon as the joiner reads a tip.
pub fn sync_source_candidates<A>(
    bootstrappers: &[(ed25519::PublicKey, A)],
    validators: &[ed25519::PublicKey],
    me: &ed25519::PublicKey,
) -> Vec<ed25519::PublicKey> {
    let mut out: Vec<ed25519::PublicKey> = Vec::new();
    for (k, _) in bootstrappers {
        if k != me && !out.contains(k) {
            out.push(k.clone());
        }
    }
    for k in validators {
        if k != me && !out.contains(k) {
            out.push(k.clone());
        }
    }
    out
}

// ============================================================================
// the founding record — `founding.toml`, the binary identity a workspace was
// founded with. Node-local: never shared with a peer, never in the genesis
// fingerprint, read by nothing but this node's own boot.
// ============================================================================

/// the founding record's file name inside a workspace.
pub const FOUNDING_FILE: &str = "founding.toml";

/// what the binary that materialized this workspace (`node init`, `node join`)
/// wrote down about itself.
///
/// `module_world` is the whole comparison. A module component is compiled
/// against the `ducktape:module` WIT world, and a host binding a different one
/// cannot instantiate it — the failure lands deep in component instantiation
/// ("type-checking export func `shape`"), reading like a compose bug rather
/// than the binary skew it is. `build` is DISPLAY only: a different commit is
/// fine, a different module world is not.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FoundingBinary {
    /// the founding binary's build identity (`noded::services::build_identity`)
    /// or `unknown` — named in the refusal, never compared.
    pub build: String,
    /// sha256 (hex) of the `ducktape:module` WIT world that binary speaks
    /// (`wasm_host::module_world_digest`).
    pub module_world: String,
}

impl FoundingBinary {
    /// write the record into `dir`, atomically like every other workspace
    /// identity file.
    pub fn save(&self, dir: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).expect("founding record serializes");
        genesis::write_atomic(&dir.join(FOUNDING_FILE), text.as_bytes())
    }

    /// the record in `dir`, or `None` where there is none — the dev-seed
    /// harness shape, which no `init`/`join` ever wrote one for.
    pub fn load(dir: &Path) -> Result<Option<Self>, String> {
        let path = dir.join(FOUNDING_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("read {path:?}: {e}")),
        };
        toml::from_str(&text)
            .map(Some)
            .map_err(|e| format!("{path:?}: {e}"))
    }
}

/// refuse a boot whose module world is not the one `dir` was founded with.
///
/// Up front and by name, because the alternative is a node that comes up,
/// replays, and dies inside component instantiation naming a `shape` export.
/// There is no tolerance window and no migration: the operator rebuilds the
/// binary from the founding commit, or re-founds the network.
pub fn guard_founding_binary(
    dir: &Path,
    chain_id: &str,
    build: &str,
    module_world: &str,
) -> Result<(), String> {
    let Some(founding) = FoundingBinary::load(dir)? else {
        return Ok(());
    };
    let speaks_the_founding_world = founding.module_world == module_world;
    if speaks_the_founding_world {
        return Ok(());
    }
    Err(format!(
        "refusing to boot {chain_id}: founded by {}, running {build} (module world differs) — \
         rebuild this binary from the founding commit, or re-found the network",
        founding.build
    ))
}

// ============================================================================
// the ducktape home — one directory per network, `~/.ducktape/<id>/`, and
// nothing else. A workspace is a network's WHOLE node home: node.toml,
// network.toml, identity.key, keys/, guest/, executors/, capabilities/ and
// storage/ all live inside it, so two networks on one machine share no file.
// The home is the list: `--network <chain id>` resolves by scanning it, so
// the CLI can address a node by the name humans actually know.
// ============================================================================

/// the ducktape home — [`ducktape_home::root`], re-exported under the name
/// this crate's callers already use. The rule and its test live in that
/// zero-dependency leaf, because the desktop app resolves the same root and
/// must not disagree about it.
pub fn ducktape_home() -> Result<PathBuf, String> {
    ducktape_home::root()
}

/// the directory a chain-id's workspace materializes into by default:
/// `<ducktape_home>/<chain-id>`. path separators in the id (a `--name` is
/// arbitrary text) are made inert — workspaces resolve by descriptor
/// content, so the directory name is display-only.
pub fn default_workspace_dir(chain_id: &str) -> Result<PathBuf, String> {
    let name = chain_id.replace(std::path::MAIN_SEPARATOR, "-");
    if !is_workspace_dir_name(&name) {
        return Err(format!(
            "chain id {chain_id:?} is not a workspace directory name — the ducktape home holds \
             one directory per network, so an id that resolves to the home itself or its \
             parent would scatter identity.key and network.toml over it instead; pass --dir \
             to choose a destination"
        ));
    }
    Ok(ducktape_home()?.join(name))
}

/// does this name address a directory INSIDE the ducktape home, and only that
/// one? a chain-id reaches here straight out of an untrusted invite
/// (`unpack_invite` asks only for UTF-8 under 255 bytes), and `""`, `"."` and
/// `".."` all join to the home itself or its parent — a workspace written
/// there is invisible to `list_workspaces`/`find_workspace_config`, which scan
/// subdirectories, so `-n <chain-id>` could never address it again. one real
/// path component, and nothing else.
fn is_workspace_dir_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    let single = matches!(components.next(), Some(std::path::Component::Normal(_)));
    single && components.next().is_none()
}

/// THE "pick one" list. Every refusal that offers a choice between registered
/// workspaces renders it through here, so no two of them can disagree about
/// order or content.
///
/// Each line carries the CONFIG PATH beside the chain id, because the chain id
/// alone does not name a row: a founder and the member that joined it share
/// one, and a list that printed the id twice offered the operator two
/// identical strings and no way to act on either. The path is what tells them
/// apart and what `--config` takes. Same `<chain-id>\t<path>` shape as
/// `ducktape node list`, indented as a choice.
pub fn workspace_choices(rows: &[(String, PathBuf)]) -> String {
    rows.iter()
        .map(|(chain_id, node_toml)| format!("  {chain_id}\t{}", node_toml.display()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// resolve `--network <chain id>` to a workspace's node.toml: scan the
/// ducktape home for descriptors whose chain-id matches `needle` — exact
/// first, else a prefix (so `ducktape` finds `ducktape#a1b2c3d4`). the match
/// must be UNIQUE either way; ambiguity and absence are loud errors that name
/// what WAS found.
pub fn find_workspace_config(needle: &str) -> Result<PathBuf, String> {
    find_workspace_config_in(&ducktape_home()?, needle)
}

fn find_workspace_config_in(root: &Path, needle: &str) -> Result<PathBuf, String> {
    // an empty needle is not a prefix that matches everything, it is a missing
    // value: `-n ''` used to "match" every workspace on the box and then refuse
    // as ambiguous, naming the whole home for a flag the operator left blank.
    if needle.is_empty() {
        return Err(
            "-n/--network was given an empty value — pass a chain id or a unique prefix of one \
             (`ducktape node list`)"
                .into(),
        );
    }
    // the same scan `node list` prints, so the resolver and every "pick one"
    // list agree on which workspaces exist, in one order — sorted, not read_dir's.
    let registered = list_workspaces_in(root)?;
    let exact: Vec<(String, PathBuf)> = registered
        .iter()
        .filter(|(chain_id, _)| chain_id == needle)
        .cloned()
        .collect();
    // an exact hit still outranks a prefix hit, but it no longer ENDS the scan:
    // two workspaces of one network carry one chain id, and returning on the
    // first made `-n <exact id>` a read_dir coin flip between them.
    let matches = match exact.is_empty() {
        false => exact,
        true => registered
            .into_iter()
            .filter(|(chain_id, _)| chain_id.starts_with(needle))
            .collect(),
    };
    match matches.as_slice() {
        [] => Err(format!(
            "no workspace under {root:?} matches network {needle:?} — pass --config <node.toml>"
        )),
        [(_, node_toml)] => Ok(node_toml.clone()),
        several => Err(format!(
            "network {needle:?} is ambiguous — it matches {} registered workspaces, \
             select one with --config <path>:\n{}",
            several.len(),
            workspace_choices(several)
        )),
    }
}

/// resolve a `--network <chain id>` needle through the ducktape home to the
/// workspace directory (the node.toml's parent — what the `gateway` family
/// addresses) and, when the workspace exposes one, the node's HTTP base URL
/// (what the `fs`/`user` families dial). the base rewrites a wildcard bind to
/// loopback so a local CLI actually reaches it; it is `None` when the node.toml
/// carries no `http_listen` (no node-API surface to dial) — the callers that
/// need a URL turn that into a loud error, the ones that need only the
/// directory ignore it. ONE shared resolver so no family re-walks the home.
pub fn resolve_network(needle: &str) -> Result<(PathBuf, Option<String>), String> {
    resolve_network_in(&ducktape_home()?, needle)
}

fn resolve_network_in(root: &Path, needle: &str) -> Result<(PathBuf, Option<String>), String> {
    let node_toml = find_workspace_config_in(root, needle)?;
    let dir = node_toml
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let (raw, _) = node_toml::load_node_toml(&node_toml)?;
    Ok((dir, Some(http_base_of(&raw.http_listen))))
}

/// the node's HTTP base for a workspace DIRECTORY — the `--workspace <dir>`
/// twin of [`resolve_network`], which addresses the same node by chain id. One
/// resolver behind both so a family that accepts either selector dials the
/// same URL.
pub fn http_base_in(dir: &Path) -> Result<String, String> {
    let (raw, _) = node_toml::load_node_toml(&dir.join("node.toml"))?;
    Ok(http_base_of(&raw.http_listen))
}

/// the node's OPERATOR RPC address for a workspace DIRECTORY, verbatim — the
/// twin of [`http_base_in`] for the other local lane. Not normalized to
/// loopback the way an http base is: `rpc_listen` is dialed as written, so the
/// string a caller compares against is the string the config holds.
pub fn rpc_listen_in(dir: &Path) -> Result<String, String> {
    let (raw, _) = node_toml::load_node_toml(&dir.join("node.toml"))?;
    Ok(raw.rpc_listen)
}

/// `http://<loopback>:<port>` for a node.toml `http_listen`. ALWAYS loopback,
/// whatever the operator bound: every caller of this is a same-box process
/// dialing its OWN node, and the node's write gate admits a credentialed
/// caller only from a loopback peer (`noded::signed_req::operator_credential_matches`
/// ends in `on_box && token_matches`). Dialing a concrete bind address
/// (`http_listen = "10.0.0.5:8844"`) therefore arrives non-loopback and the
/// node refuses its own daemons.
///
/// The FAMILY is kept: a bracketed IPv6 literal maps to `[::1]`, not
/// `127.0.0.1` — a bindv6only listener refuses v4 loopback dials.
pub fn http_base_of(http_listen: &str) -> String {
    let Some((host, port)) = http_listen.rsplit_once(':') else {
        return format!("http://{http_listen}");
    };
    let is_ipv6_literal = host.starts_with('[');
    let loopback = if is_ipv6_literal {
        "[::1]"
    } else {
        "127.0.0.1"
    };
    format!("http://{loopback}:{port}")
}

/// enumerate the workspaces under the ducktape home: `(chain_id, node.toml
/// path)` per network, sorted by chain_id. an ABSENT home is an empty list,
/// not an error — no network has been founded or joined yet. one unreadable
/// descriptor is skipped, never fatal (same tolerance as `find_workspace_config`).
pub fn list_workspaces() -> Result<Vec<(String, PathBuf)>, String> {
    list_workspaces_in(&ducktape_home()?)
}

pub fn list_workspaces_in(root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read {root:?}: {e}")),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        let descriptor_path = dir.join("network.toml");
        if !descriptor_path.is_file() {
            continue;
        }
        let d = match NetworkDescriptor::load(&descriptor_path) {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    target: "ducktape::node",
                    reason = "descriptor_unreadable",
                    dir = %dir.display(),
                    error = %e,
                    "skipping workspace with an unreadable network.toml"
                );
                continue;
            }
        };
        out.push((d.chain_id, dir.join("node.toml")));
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE BUG (#2512): `node admit deadbeef` answered with the codec's
    /// "Unexpected End-of-Buffer: Not enough bytes remaining to read data".
    /// A person who typed the wrong thing is told what the right thing is, and
    /// a key that IS right still decodes (the must-pass case).
    #[test]
    fn a_bad_pubkey_is_refused_in_the_clis_own_words() {
        for (typed, why) in [
            ("deadbeef", "expected 64 hex characters, got 8"),
            (&"ab".repeat(33), "expected 64 hex characters, got 66"),
            (&"zz".repeat(32), ""),
        ] {
            let said = decode_key(typed).expect_err("not a key");
            assert!(
                said.starts_with(&format!("{typed:?} is not an ed25519 public key: ")),
                "{said}"
            );
            assert!(said.ends_with(why), "{said}");
            assert!(!said.contains("Buffer"), "no codec text: {said}");
        }
        let key = ed25519::PrivateKey::from_seed(7).public_key();
        assert_eq!(decode_key(&hex_bytes(key.as_ref())), Ok(key));
    }

    /// A binary resolves the set its build staged, by the name that build
    /// baked in, before the unkeyed one, from the profile directory and from
    /// `deps/` where cargo runs tests — and an installed layout (a plain
    /// `modules` beside the binary, which is what `make install-node` and the
    /// pinned dognet binaries have) resolves exactly as it did before any
    /// keying.
    #[test]
    fn a_binary_resolves_the_set_its_build_named_before_the_installed_one() {
        let scratch = tempfile::tempdir().unwrap();
        let profile = scratch.path().join("debug");
        let exe = profile.join("ducktape");
        std::fs::create_dir_all(profile.join("deps")).unwrap();
        let ours = "modules%home%someone%checkout";

        std::fs::create_dir(profile.join("modules")).unwrap();
        assert_eq!(
            staged_modules_dir(&exe, ours).unwrap(),
            profile.join("modules")
        );

        std::fs::create_dir(profile.join(ours)).unwrap();
        assert_eq!(
            staged_modules_dir(&exe, ours).unwrap(),
            profile.join(ours),
            "another checkout's `modules` must not outrank the one we were built with"
        );
        let test_exe = profile.join("deps/noded-1234");
        assert_eq!(
            staged_modules_dir(&test_exe, ours).unwrap(),
            profile.join(ours),
            "a test binary runs from deps/"
        );

        // the installed layout: one unkeyed set beside the binary, nothing else
        let installed = scratch.path().join("bin");
        std::fs::create_dir_all(installed.join("modules")).unwrap();
        assert_eq!(
            staged_modules_dir(&installed.join("ducktape"), ours).unwrap(),
            installed.join("modules")
        );
        assert_eq!(
            sim_twin(&installed.join("modules")),
            installed.join("sim-modules")
        );
        assert_eq!(
            sim_twin(&profile.join(ours)),
            profile.join("sim-modules%home%someone%checkout")
        );
    }

    /// A NODE RELEASE resolves its set exactly like an installed one, because
    /// there is one layout: the archive unpacks `ducktape` and `modules/` side
    /// by side into `<workspace>/updates/releases/<sha>/`, and `/proc/self/exe`
    /// resolves through `current` to that directory.
    #[test]
    fn a_release_directory_resolves_the_set_its_archive_unpacked_beside_the_binary() {
        let scratch = tempfile::tempdir().unwrap();
        let release = scratch.path().join("updates/releases/deadbeef");
        std::fs::create_dir_all(release.join("modules")).unwrap();
        let built_as = "modules%home%release%checkout";
        assert_eq!(
            staged_modules_dir(&release.join("ducktape"), built_as).unwrap(),
            release.join("modules")
        );

        // and a binary lifted out of the archive on its own — the one way to
        // reach a release host with no set — is told which ARTIFACT is
        // missing, never an environment variable it has no way to fill.
        let alone = scratch.path().join("bin/ducktape");
        assert!(staged_modules_dir(&alone, built_as).is_none());
        let refusal = no_founding_set(&alone);
        assert!(refusal.contains("release archive"), "{refusal}");
        assert!(refusal.contains("modules/"), "{refusal}");
        assert!(
            !refusal.contains("DUCKTAPE_MODULES_DIR"),
            "the fatal names the artifact to download, not an env var: {refusal}"
        );
    }

    /// What a binary resolves depends on nothing a sibling checkout writes.
    ///
    /// Every checkout building into a shared target writes into one profile
    /// directory: its own keyed set, and — at an older revision — a pointer
    /// file naming it. A binary that read the name from there at boot followed
    /// the LAST build, so a suite already running composed another revision's
    /// wasm the moment a sibling built. Here a sibling has staged its set and
    /// named it in every file it could, and the binary still resolves its own.
    #[test]
    fn a_siblings_build_cannot_redirect_the_set_a_binary_resolves() {
        let scratch = tempfile::tempdir().unwrap();
        let profile = scratch.path().join("debug");
        std::fs::create_dir_all(profile.join("deps")).unwrap();
        let ours = "modules%home%someone%ours";
        let theirs = "modules%home%someone%theirs";
        for set in [ours, theirs] {
            std::fs::create_dir(profile.join(set)).unwrap();
        }
        let exe = profile.join("ducktape");
        let test_exe = profile.join("deps/simnode-1234");
        let before = [&exe, &test_exe].map(|exe| staged_modules_dir(exe, ours));

        for dir in [&profile, &profile.join("deps")] {
            std::fs::write(dir.join(".staged-modules"), theirs).unwrap();
        }
        std::fs::write(
            profile.join(theirs).join(staged_key::STAGED_OWNER),
            "def5678",
        )
        .unwrap();

        let after = [&exe, &test_exe].map(|exe| staged_modules_dir(exe, ours));
        assert_eq!(
            after, before,
            "a sibling's build moved what this binary resolves"
        );
        assert_eq!(after, [Some(profile.join(ours)), Some(profile.join(ours))]);
        assert_eq!(
            sim_twin(&profile.join(ours)),
            profile.join("sim-modules%home%someone%ours"),
            "and the simulator's twin is ours too"
        );
    }

    /// The record inside a set says which build wrote it. Reading it back is
    /// what lets `noded::services::founding_set` refuse a stranger's.
    #[test]
    fn a_set_reports_the_build_that_staged_it() {
        let scratch = tempfile::tempdir().unwrap();
        let set = scratch.path().join("modules%somewhere");
        std::fs::create_dir_all(&set).unwrap();
        assert_eq!(staged_by(&set), None, "a set with no record claims nothing");
        std::fs::write(set.join(staged_key::STAGED_OWNER), "abc1234\n").unwrap();
        assert_eq!(staged_by(&set).as_deref(), Some("abc1234"));
    }

    /// A set name encodes its checkout, and the encoding round-trips — which
    /// is how the stager tells a set whose worktree is gone from a live one.
    #[test]
    fn a_set_name_names_the_checkout_it_belongs_to() {
        let checkout = Path::new("/home/eddy/dev/ducktape/ducktape");
        let name = staged_key::staged_set_name("modules", checkout);
        assert_eq!(name, "modules%home%eddy%dev%ducktape%ducktape");
        assert_eq!(
            staged_key::checkout_of_set_name(&name).as_deref(),
            Some(checkout)
        );
        assert_eq!(staged_key::checkout_of_set_name("modules"), None);
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ducktape-config-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    #[test]
    fn coord_cap_roundtrips_through_pack_and_files() {
        use commonware_cryptography::{Signer as _, ed25519};
        use nat_traversal::{NodeKey, mint_coord_cap};
        let g = ed25519::PrivateKey::from_seed(7);
        let subject = NodeKey([0x11; 32]);
        let cap = mint_coord_cap(&g, subject, 4_000_000);
        let bytes = pack_coord_cap(&cap);
        assert_eq!(bytes.len(), 32 + 8 + 64);
        assert_eq!(unpack_coord_cap(&bytes).unwrap(), cap);

        let dir = tempfile::tempdir().unwrap();
        assert!(load_coord_cap(dir.path()).is_none());
        save_coord_cap(dir.path(), &cap).unwrap();
        assert_eq!(load_coord_cap(dir.path()).unwrap(), cap);
    }

    #[test]
    fn list_workspaces_enumerates_sorted_and_tolerates_junk() {
        let root = tempfile::tempdir().unwrap();
        // absent subdir contents => empty, and an absent root is empty too.
        assert!(
            list_workspaces_in(&root.path().join("nope"))
                .unwrap()
                .is_empty()
        );
        for id in ["zebra#00000002", "alpha#00000001"] {
            let dir = root.path().join(id);
            std::fs::create_dir_all(&dir).unwrap();
            NetworkDescriptor {
                chain_id: id.into(),
                validators: vec![],
                bootstrap: vec![],
                reach: vec![],
                coordination: None,
                block_time_ms: DEFAULT_BLOCK_TIME_MS,
                genesis: String::new(),
                modules: Vec::new(),
            }
            .save(&dir.join("network.toml"))
            .unwrap();
        }
        // a bare dir with no network.toml is skipped, not counted.
        std::fs::create_dir_all(root.path().join("not-a-workspace")).unwrap();
        let got = list_workspaces_in(root.path()).unwrap();
        let ids: Vec<&str> = got.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(ids, ["alpha#00000001", "zebra#00000002"]);
        assert!(got[0].1.ends_with("alpha#00000001/node.toml"));
    }

    #[test]
    fn list_and_find_skip_a_torn_descriptor_instead_of_erroring() {
        let root = tempfile::tempdir().unwrap();
        let good = root.path().join("good#00000001");
        std::fs::create_dir_all(&good).unwrap();
        NetworkDescriptor {
            chain_id: "good#00000001".into(),
            validators: vec![],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        }
        .save(&good.join("network.toml"))
        .unwrap();
        // a torn (truncated mid-write) network.toml: present, but not
        // parseable toml.
        let torn = root.path().join("torn#00000002");
        std::fs::create_dir_all(&torn).unwrap();
        std::fs::write(torn.join("network.toml"), b"chain_id = \"torn#0000").unwrap();

        let listed = list_workspaces_in(root.path()).unwrap();
        let ids: Vec<&str> = listed.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(ids, ["good#00000001"]);

        // `find` still resolves the healthy workspace and does not surface
        // the damaged one as a match or a hard error.
        let found = find_workspace_config_in(root.path(), "good#00000001").unwrap();
        assert!(found.ends_with("good#00000001/node.toml"));
        assert!(find_workspace_config_in(root.path(), "torn").is_err());
    }

    #[test]
    fn save_is_atomic_and_leaves_no_partial_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("network.toml");
        let original = NetworkDescriptor {
            chain_id: "atomic#00000001".into(),
            validators: vec![],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        original.save(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        // save goes through a tmp file that is renamed into place: after a
        // successful save, no `.tmp.<pid>` sibling is left behind, and the
        // reader never sees anything but a complete descriptor.
        let leftover_tmp = std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.path().extension().is_some_and(|ext| ext != "toml"));
        assert!(!leftover_tmp, "save left a tmp file behind");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        // reloading yields the exact descriptor that was saved.
        assert_eq!(
            NetworkDescriptor::load(&path).unwrap().chain_id,
            original.chain_id
        );
    }

    #[test]
    fn chain_id_shape_matches_what_mint_chain_id_produces() {
        let minted = identity::mint_chain_id(
            "dognet",
            &commonware_cryptography::ed25519::PrivateKey::from_seed(1).public_key(),
        );
        assert!(validate_chain_id_shape(&minted).is_ok(), "{minted}");

        for bad in [
            "dognet",           // no salt at all
            "dognet#",          // empty salt
            "dognet#abcd123",   // 7 hex chars, not 8
            "dognet#abcd12345", // 9 hex chars
            "dognet#ABCD1234",  // uppercase
            "dognet#zzzzzzzz",  // not hex
            "#abcd1234",        // empty name
            "dog#net#abcd1234", // more than one '#' (splits on the first)
        ] {
            assert!(
                validate_chain_id_shape(bad).is_err(),
                "{bad:?} should not pass as a minted chain id"
            );
        }
    }

    #[test]
    fn coordination_defaults_to_private_and_parses_public() {
        let mut d = NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        // default (field unset) -> Private
        assert_eq!(d.coordination(), Coordination::Private);
        d.coordination = Some("public".to_string());
        assert_eq!(d.coordination(), Coordination::Public);
        d.coordination = Some("private".to_string());
        assert_eq!(d.coordination(), Coordination::Private);
    }

    #[test]
    fn primary_coordinator_defaults_to_deployed_public_rendezvous() {
        let coord = primary_coordinator_or_default(None).expect("default coordinator");
        assert_eq!(coord.as_deref(), Some("relay.ducktape.industries:3478"));

        let disabled = primary_coordinator_or_default(Some("none")).expect("disabled");
        assert_eq!(disabled, None);
    }

    /// `coordinator_ingress` — the AMBIENT source change (1) threads into
    /// both runtime call sites (main.rs:954 joiner, main.rs:3201 member) —
    /// resolves an explicit override, honors the disable sentinel, and
    /// falls back to the compiled default exactly like `coordinator_ingress
    /// (None)` did before change (1) existed (bit-identical absent case).
    #[test]
    fn coordinator_ingress_resolves_an_explicit_override() {
        match coordinator_ingress(Some("203.0.113.9:3478")).expect("dialable override") {
            Some(Ingress::Socket(addr)) => assert_eq!(addr, "203.0.113.9:3478".parse().unwrap()),
            other => panic!("expected a concrete socket ingress, got {other:?}"),
        }
        assert_eq!(
            coordinator_ingress(Some("none")).expect("disabled"),
            None,
            "the sentinel disables coordination outright — no ingress to bind"
        );
        match coordinator_ingress(None).expect("compiled default") {
            Some(Ingress::Dns { port, .. }) => assert_eq!(port, 3478),
            other => panic!("expected the compiled default's hostname ingress, got {other:?}"),
        }
    }

    #[test]
    fn apply_primary_coordinator_records_public_coordinated_self_hint() {
        let me = ed25519::PrivateKey::from_seed(7).public_key();
        let mut d = NetworkDescriptor {
            chain_id: "ducktape#a1b2c3d4".into(),
            validators: vec![hex_bytes(me.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };

        d.apply_primary_coordinator(&me, "relay.ducktape.industries:3478")
            .expect("coordinator hint");

        assert_eq!(d.coordination(), Coordination::Public);
        let hints = d.reach_hints().expect("hints");
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].expected_key, me);
        match &hints[0].reach {
            Reach::Coordinated(coord) => {
                assert_eq!(coord.coord_addr, "relay.ducktape.industries:3478");
                assert_eq!(coord.coord_key, keyless_coordinator_placeholder_key());
            }
            other => panic!("expected coordinated reach hint, got {other:?}"),
        }
    }

    /// the FULL delivery chain at the crypto level: a genesis validator mints a
    /// cap for a joiner, it is packed for the wire (`pack_coord_cap`), the
    /// joiner unpacks it (`unpack_coord_cap`) and presents it on an
    /// authenticated request — and the coordinator's private `verify_request`
    /// admits the joiner off that delivered cap. Proves the cap the member
    /// hands over its sealed `IntroReply::Admitted` ack actually authorizes the
    /// holder.
    #[test]
    fn delivered_cap_admits_the_joiner_under_private_policy() {
        use commonware_cryptography::Signer as _;
        use nat_traversal::{
            AuthPolicy, COORD_CAP_TTL_SECS, DEFAULT_FRESHNESS_WINDOW_SECS, NodeKey, mint_coord_cap,
            now_secs, sign_authenticator, verify_request,
        };

        let genesis = ed25519::PrivateKey::from_seed(1);
        let joiner = ed25519::PrivateKey::from_seed(2);
        let mut subj = [0u8; 32];
        subj.copy_from_slice(joiner.public_key().as_ref());
        let subject = NodeKey(subj);

        let now = now_secs();
        // MINT (member side) -> PACK (wire) -> UNPACK (joiner side).
        let minted = mint_coord_cap(&genesis, subject, now + COORD_CAP_TTL_SECS);
        let wire = pack_coord_cap(&minted);
        let delivered = unpack_coord_cap(&wire).expect("joiner unpacks the delivered cap");
        assert_eq!(delivered, minted, "the cap survives the wire byte-for-byte");

        // the joiner builds an authenticated request carrying the delivered cap.
        let inner = b"\x03register-request-bytes";
        let auth = sign_authenticator(&joiner, inner, now, Some(delivered));

        // the coordinator, pinned to this genesis key, admits the joiner.
        let policy = AuthPolicy::Private {
            genesis_set: vec![genesis.public_key()],
        };
        assert_eq!(
            verify_request(
                &policy,
                now,
                DEFAULT_FRESHNESS_WINDOW_SECS,
                subject,
                inner,
                &auth
            ),
            Ok(()),
            "the delivered cap admits the joiner"
        );

        // control: WITHOUT the cap the same private policy rejects the joiner.
        let bare = sign_authenticator(&joiner, inner, now, None);
        assert!(
            verify_request(
                &policy,
                now,
                DEFAULT_FRESHNESS_WINDOW_SECS,
                subject,
                inner,
                &bare
            )
            .is_err(),
            "a joiner with no cap is not admitted to a private network"
        );
    }

    #[test]
    fn network_flag_resolves_workspaces_by_chain_id_prefix() {
        let root = tmp("registry");
        for (ws, chain) in [("a", "ducktape#a1b2c3d4"), ("b", "kitchen#99887766")] {
            let dir = root.join(ws);
            std::fs::create_dir_all(&dir).expect("mk workspace");
            let d = NetworkDescriptor {
                chain_id: chain.into(),
                validators: vec![hex_bytes(
                    ed25519::PrivateKey::from_seed(40).public_key().as_ref(),
                )],
                bootstrap: vec![],
                reach: vec![],
                coordination: None,
                block_time_ms: DEFAULT_BLOCK_TIME_MS,
                genesis: String::new(),
                modules: Vec::new(),
            };
            d.save(&dir.join("network.toml")).expect("save");
        }
        // stray non-workspace entries are skipped, not errors.
        std::fs::create_dir_all(root.join("not-a-workspace")).expect("mk stray");

        // exact and unique-prefix both land on the workspace's node.toml.
        assert_eq!(
            find_workspace_config_in(&root, "ducktape#a1b2c3d4").expect("exact"),
            root.join("a").join("node.toml")
        );
        assert_eq!(
            find_workspace_config_in(&root, "kitchen").expect("prefix"),
            root.join("b").join("node.toml")
        );
        // absence is loud, and an empty value is refused as the missing value
        // it is rather than treated as a prefix that matches the whole box.
        assert!(find_workspace_config_in(&root, "nope").is_err());
        let ambiguous = find_workspace_config_in(&root, "").expect_err("empty");
        assert!(
            ambiguous.contains("empty value"),
            "an empty -n is a missing value, not a prefix that matches the box: {ambiguous}"
        );
    }

    /// Two workspaces of ONE network — a founder and the member that joined
    /// it — is the ordinary shape (`ops/refound-net.sh` builds exactly that),
    /// and the chain id they share cannot name either one. Returning the
    /// first `read_dir` reached made `-n <exact chain id>` a coin flip that
    /// read like an answer, for `node status`, `node peers`, `module update`
    /// and every other verb carrying the selector.
    #[test]
    fn a_chain_id_two_workspaces_share_is_refused_with_both_config_paths() {
        let root = tmp("shared-chain");
        for ws in ["founder", "joiner"] {
            let dir = root.join(ws);
            std::fs::create_dir_all(&dir).expect("mk workspace");
            NetworkDescriptor {
                chain_id: "twinned#a1b2c3d4".into(),
                validators: vec![],
                bootstrap: vec![],
                reach: vec![],
                coordination: None,
                block_time_ms: DEFAULT_BLOCK_TIME_MS,
                genesis: String::new(),
                modules: Vec::new(),
            }
            .save(&dir.join("network.toml"))
            .expect("save");
        }
        // the EXACT id is the sharp case: it used to return on the first hit.
        for needle in ["twinned#a1b2c3d4", "twinned"] {
            let err = find_workspace_config_in(&root, needle)
                .expect_err("a shared chain id names no single workspace");
            for ws in ["founder", "joiner"] {
                let path = root.join(ws).join("node.toml");
                assert!(
                    err.contains(&path.display().to_string()),
                    "{needle:?} must name {}: {err}",
                    path.display()
                );
            }
            assert!(err.contains("--config"), "{needle:?}: {err}");
        }
    }

    #[test]
    fn a_traversal_chain_id_never_becomes_a_workspace_directory() {
        // straight out of an untrusted invite: each of these joins to the
        // ducktape home itself or its parent instead of a per-network directory.
        for hostile in ["", ".", ".."] {
            let err = default_workspace_dir(hostile)
                .expect_err("a traversal chain id has no default workspace");
            assert!(
                err.contains("not a workspace directory name"),
                "{hostile:?}: {err}"
            );
        }
        // separators stay display-only: they collapse into one component
        // rather than descending, and a minted chain-id is untouched.
        assert!(is_workspace_dir_name(
            &"a/b".replace(std::path::MAIN_SEPARATOR, "-")
        ));
        assert!(is_workspace_dir_name(
            &"../..".replace(std::path::MAIN_SEPARATOR, "-")
        ));
        assert!(is_workspace_dir_name("ducktape#a1b2c3d4"));
    }

    fn write_workspace(root: &Path, ws: &str, chain: &str, listen: &str, http: &str) -> PathBuf {
        let dir = root.join(ws);
        std::fs::create_dir_all(&dir).expect("mk workspace");
        NetworkDescriptor {
            chain_id: chain.into(),
            validators: vec![],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        }
        .save(&dir.join("network.toml"))
        .expect("save descriptor");
        // the network shape is COMPLETE by construction — write every key.
        let node_toml = format!(
            "network = \"network.toml\"\nkey_file = \"identity.key\"\n\
             listen = \"{listen}\"\nadvertised = \"127.0.0.1:9000\"\n\
             storage_dir = 'storage'\nhttp_listen = \"{http}\"\n\
             gateway_listen = \"127.0.0.1:0\"\nrpc_listen = \"127.0.0.1:0\"\n\
             wireguard_listen = \"0.0.0.0:51820\"\ninvite_listen = \"0.0.0.0:51821\"\n\
             wireguard_advertised = \"auto\"\nprimary_coordinator = \"none\"\n\
             coordinator_relay = \"none\"\ncheckpoint_blocks = 32\n"
        );
        std::fs::write(dir.join("node.toml"), node_toml).expect("write node.toml");
        dir
    }

    #[test]
    fn resolve_network_reads_http_base_and_reports_a_missing_listen() {
        let root = tmp("resolve-net");
        // a workspace with a wildcard http listen resolves to a loopback base.
        let a = write_workspace(
            &root,
            "a",
            "ducktape#a1b2c3d4",
            "0.0.0.0:9000",
            "0.0.0.0:8844",
        );
        let (dir, http) = resolve_network_in(&root, "ducktape").expect("prefix resolves");
        assert_eq!(dir, a);
        assert_eq!(http.as_deref(), Some("http://127.0.0.1:8844"));

        // every network-shape workspace carries an http listen (the key is
        // required), so the base always resolves.
        let b = write_workspace(
            &root,
            "b",
            "kitchen#99887766",
            "127.0.0.1:9001",
            "127.0.0.1:9002",
        );
        let (bdir, bhttp) = resolve_network_in(&root, "kitchen").expect("prefix resolves");
        assert_eq!(bdir, b);
        assert_eq!(bhttp.as_deref(), Some("http://127.0.0.1:9002"));
    }

    #[test]
    fn http_base_is_always_loopback_in_the_bound_family() {
        assert_eq!(http_base_of("0.0.0.0:8844"), "http://127.0.0.1:8844");
        // a bindv6only `[::]` refuses v4 loopback dials → v6 loopback, not 127.0.0.1
        assert_eq!(http_base_of("[::]:8844"), "http://[::1]:8844");
        assert_eq!(http_base_of("127.0.0.1:8844"), "http://127.0.0.1:8844");
        assert_eq!(http_base_of("[::1]:8844"), "http://[::1]:8844");
        // a CONCRETE bind is still dialed over loopback: the node's write gate
        // admits only loopback peers, so 10.0.0.5 would make it refuse its own
        // daemons.
        assert_eq!(http_base_of("10.0.0.5:8844"), "http://127.0.0.1:8844");
        assert_eq!(http_base_of("192.168.1.5:80"), "http://127.0.0.1:80");
        assert_eq!(http_base_of("[2001:db8::5]:8844"), "http://[::1]:8844");
    }

    #[test]
    fn admit_is_idempotent_and_sorted() {
        let a = ed25519::PrivateKey::from_seed(1).public_key();
        let b = ed25519::PrivateKey::from_seed(2).public_key();
        let mut d = NetworkDescriptor {
            chain_id: "t#00000000".into(),
            validators: vec![],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        d.admit(&a);
        d.admit(&b);
        d.admit(&a);
        assert_eq!(d.validators.len(), 2);
        let mut sorted = d.validators.clone();
        sorted.sort();
        assert_eq!(d.validators, sorted);
    }

    #[test]
    fn duplicate_validators_are_a_config_error() {
        let a = ed25519::PrivateKey::from_seed(4).public_key();
        let d = NetworkDescriptor {
            chain_id: "dup#00000000".into(),
            validators: vec![hex_bytes(a.as_ref()), hex_bytes(a.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        assert!(
            d.validator_keys().is_err(),
            "dups must not reach Set::try_from"
        );
    }

    #[test]
    fn genesis_namespace_fingerprints_the_validator_set_not_the_hints() {
        let a = ed25519::PrivateKey::from_seed(5).public_key();
        let b = ed25519::PrivateKey::from_seed(6).public_key();
        let mut d = NetworkDescriptor {
            chain_id: "net#00000000".into(),
            validators: vec![hex_bytes(a.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        let founder_only = d.genesis_namespace();
        assert!(founder_only.starts_with("net#00000000@"));

        // bootstrap hints are advisory and legitimately differ per member —
        // they must NOT move the namespace.
        d.add_bootstrap(&a, "127.0.0.1:52200");
        assert_eq!(d.genesis_namespace(), founder_only);

        // admitting a member DOES move it: a stale descriptor can no longer
        // handshake, so genesis divergence is loud, not a silent fork.
        d.admit(&b);
        assert_ne!(d.genesis_namespace(), founder_only);

        // and it is order-independent (canonical over the sorted set).
        let mut reversed = d.clone();
        reversed.validators.reverse();
        assert_eq!(reversed.genesis_namespace(), d.genesis_namespace());
    }

    fn modules_fixture() -> Vec<ModuleCode> {
        vec![
            ModuleCode {
                id: "pages".into(),
                code_hash: "11".repeat(32),
            },
            ModuleCode {
                id: "chat".into(),
                code_hash: "22".repeat(32),
            },
        ]
    }

    #[test]
    fn genesis_namespace_fingerprints_the_module_hashes() {
        let a = ed25519::PrivateKey::from_seed(5).public_key();
        let mut d = NetworkDescriptor {
            chain_id: "net#00000000".into(),
            validators: vec![hex_bytes(a.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: modules_fixture(),
        };
        let base = d.genesis_namespace();
        // a different component for one module is a different network.
        d.modules[0].code_hash = "33".repeat(32);
        assert_ne!(d.genesis_namespace(), base);
        // order-independent: canonical over the id-sorted list.
        let mut reversed = d.clone();
        reversed.modules.reverse();
        assert_eq!(reversed.genesis_namespace(), d.genesis_namespace());
    }

    /// The beat is a genesis fact, so it is IN the fingerprint: two networks
    /// alike in every other respect but the cadence cannot handshake. That is
    /// what makes a mismatched member a loud connectivity failure instead of a
    /// validator quietly nullifying the views its slower leader was filling.
    #[test]
    fn genesis_namespace_fingerprints_the_beat() {
        let a = ed25519::PrivateKey::from_seed(5).public_key();
        let d = NetworkDescriptor {
            chain_id: "net#00000000".into(),
            validators: vec![hex_bytes(a.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: modules_fixture(),
        };
        let faster = NetworkDescriptor {
            block_time_ms: 100,
            ..d.clone()
        };
        assert_ne!(faster.genesis_namespace(), d.genesis_namespace());
    }

    /// a zero beat collapses every consensus timer to a hot spin, so it is
    /// refused at both boundaries a descriptor enters through — the file and
    /// (in the invite suite) the blob.
    #[test]
    fn a_zero_beat_is_refused_at_the_file_boundary() {
        let text = format!(
            "chain_id = \"net#00000000\"\nvalidators = []\ngenesis = \"{}\"\n\
             block_time_ms = 0\nmodules = []\n",
            "ab".repeat(32)
        );
        let err = NetworkDescriptor::from_toml(&text).unwrap_err();
        assert!(err.contains("block_time_ms"), "{err}");
    }

    /// the floor is the drain tick, not zero: a beat the idle heartbeat can't
    /// keep is refused just as loudly as a zero beat, and it names the floor.
    #[test]
    fn a_beat_under_the_drain_tick_is_refused() {
        let text = format!(
            "chain_id = \"net#00000000\"\nvalidators = []\ngenesis = \"{}\"\n\
             block_time_ms = 99\nmodules = []\n",
            "ab".repeat(32)
        );
        let err = NetworkDescriptor::from_toml(&text).unwrap_err();
        assert!(err.contains("block_time_ms"), "{err}");
        assert!(err.contains("100"), "{err}");

        let text = format!(
            "chain_id = \"net#00000000\"\nvalidators = []\ngenesis = \"{}\"\n\
             block_time_ms = 100\nmodules = []\n",
            "ab".repeat(32)
        );
        NetworkDescriptor::from_toml(&text).expect("the floor itself is accepted");
    }

    #[test]
    fn descriptor_without_modules_does_not_parse() {
        let text = format!(
            "chain_id = \"net#00000000\"\nvalidators = []\ngenesis = \"{}\"\n\
             block_time_ms = 1000\n",
            "ab".repeat(32)
        );
        let err = NetworkDescriptor::from_toml(&text).unwrap_err();
        assert!(err.contains("modules"), "{err}");
    }

    #[test]
    fn module_hashes_decode_and_refuse_bad_lengths() {
        let mut d = NetworkDescriptor {
            chain_id: "n".into(),
            validators: vec![],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: modules_fixture(),
        };
        let map = d.module_hashes().unwrap();
        assert_eq!(map["pages"], [0x11u8; 32]);
        d.modules.push(ModuleCode {
            id: "x".into(),
            code_hash: "ab".into(),
        });
        assert!(d.module_hashes().unwrap_err().contains("x"));
    }

    #[test]
    fn dialable_rejects_unspecified_ips_and_port_zero() {
        // listen fallback: concrete -> hint, unspecified/port-0 -> no hint.
        assert!(dialable(None, "127.0.0.1:52200").unwrap().is_some());
        assert!(dialable(None, "0.0.0.0:52200").unwrap().is_none());
        assert!(dialable(None, "[::]:52200").unwrap().is_none());
        assert!(dialable(None, "127.0.0.1:0").unwrap().is_none());
        // an EXPLICIT advertised addr that is not dialable is an error.
        assert!(dialable(Some("0.0.0.0:52200"), "127.0.0.1:52200").is_err());
        assert!(dialable(Some("1.2.3.4:0"), "127.0.0.1:52200").is_err());
        assert_eq!(
            dialable(Some("1.2.3.4:5"), "127.0.0.1:0").unwrap(),
            Some("1.2.3.4:5".to_string())
        );
        // a HOSTNAME advertised is kept verbatim (resolved at dial time), not
        // rejected — invites can carry a domain like node.example.com:443.
        assert_eq!(
            dialable(Some("node.example.com:443"), "127.0.0.1:0").unwrap(),
            Some("node.example.com:443".to_string())
        );
        assert!(dialable(Some("node.example.com:0"), "127.0.0.1:52200").is_err());
        assert!(dialable(Some("not-an-addr"), "127.0.0.1:52200").is_err());
        // the overlay sentinel is "no underlay hint", never an error — an
        // invite minted from an overlay-advertised member carries only the
        // descriptor's existing reach hints.
        assert!(dialable(Some("overlay"), "[::]:52200").unwrap().is_none());
    }

    #[test]
    fn join_guard_refuses_a_foreign_descriptor_but_allows_the_refresh() {
        let a = ed25519::PrivateKey::from_seed(11).public_key();
        let dir = tmp("joinguard");
        let ours = NetworkDescriptor {
            chain_id: "home#11111111".into(),
            validators: vec![hex_bytes(a.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        // empty dir: anything goes.
        assert!(guard_join_descriptor(&dir, &ours, &a).is_ok());
        ours.save(&dir.join("network.toml")).expect("save");

        // the refreshed invite (same chain-id, more members), issued by the
        // validator already on disk — the documented re-join.
        let mut refreshed = ours.clone();
        refreshed.admit(&ed25519::PrivateKey::from_seed(12).public_key());
        assert!(guard_join_descriptor(&dir, &refreshed, &a).is_ok());

        // a different network's invite must never clobber this workspace.
        let foreign = NetworkDescriptor {
            chain_id: "other#22222222".into(),
            ..ours.clone()
        };
        let err = guard_join_descriptor(&dir, &foreign, &a).expect_err("foreign refused");
        assert!(
            err.contains("home#11111111"),
            "error names the resident network: {err}"
        );
    }

    #[test]
    fn join_guard_refuses_an_invite_that_reuses_the_chain_id_over_a_foreign_genesis() {
        let ours_key = ed25519::PrivateKey::from_seed(31).public_key();
        let theirs = ed25519::PrivateKey::from_seed(32).public_key();
        let dir = tmp("joinguard-genesis");
        let ours = NetworkDescriptor {
            chain_id: "home#33333333".into(),
            validators: vec![hex_bytes(ours_key.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: "11".repeat(32),
            modules: Vec::new(),
        };
        ours.save(&dir.join("network.toml")).expect("save");

        // the whole attack: the chain-id is public, so it is copied verbatim;
        // everything that decides which network this is belongs to the attacker.
        let hostile = NetworkDescriptor {
            validators: vec![hex_bytes(theirs.as_ref())],
            genesis: "22".repeat(32),
            ..ours.clone()
        };
        let err =
            guard_join_descriptor(&dir, &hostile, &theirs).expect_err("foreign genesis refused");
        assert!(err.starts_with("foreign_genesis:"), "{err}");
        assert!(
            err.contains(&ours.genesis_namespace()) && err.contains(&hostile.genesis_namespace()),
            "error names both genesis fingerprints: {err}"
        );

        // and the beat alone is enough to make it a different network.
        let faster = NetworkDescriptor {
            block_time_ms: ours.block_time_ms + 1,
            ..ours.clone()
        };
        assert!(
            guard_join_descriptor(&dir, &faster, &ours_key).is_err(),
            "beat differs"
        );

        // the pre-genesis admit refresh stays allowed: same genesis pin,
        // modules and beat, and our validator is still in the set.
        let mut admitted = ours.clone();
        admitted.admit(&theirs);
        assert_ne!(admitted.genesis_namespace(), ours.genesis_namespace());
        guard_join_descriptor(&dir, &admitted, &ours_key)
            .expect("the admit refresh is the re-join");

        // dropping a validator we already trust is NOT a refresh.
        let evicted = NetworkDescriptor {
            validators: vec![hex_bytes(theirs.as_ref())],
            ..ours.clone()
        };
        assert!(
            guard_join_descriptor(&dir, &evicted, &ours_key).is_err(),
            "a set that drops our own validator is not an admit refresh"
        );
    }

    #[test]
    fn join_guard_refuses_a_refresh_signed_by_someone_who_is_not_our_validator() {
        let ours_key = ed25519::PrivateKey::from_seed(41).public_key();
        let stranger = ed25519::PrivateKey::from_seed(42).public_key();
        let dir = tmp("joinguard-issuer");
        let ours = NetworkDescriptor {
            chain_id: "home#44444444".into(),
            validators: vec![hex_bytes(ours_key.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: "11".repeat(32),
            modules: Vec::new(),
        };
        ours.save(&dir.join("network.toml")).expect("save");

        // the descriptor a patient attacker builds: it KEEPS our validators
        // (so the admit-refresh shape holds) and adds its own, over our
        // genesis pin, modules and beat. Only the signature gives it away.
        let mut refresh = ours.clone();
        refresh.admit(&stranger);
        assert_ne!(refresh.genesis_namespace(), ours.genesis_namespace());

        let err = guard_join_descriptor(&dir, &refresh, &stranger)
            .expect_err("a refresh signed by a non-member is refused");
        assert!(
            err.starts_with("join_not_signed_by_a_member:"),
            "its own reason token: {err}"
        );
        assert!(
            err.contains(&hex_bytes(stranger.as_ref())),
            "error names the issuer that signed it: {err}"
        );

        // the SAME descriptor, signed by the validator already on disk, is the
        // documented re-join.
        guard_join_descriptor(&dir, &refresh, &ours_key)
            .expect("a refresh signed by a resident validator is the re-join");
    }

    #[test]
    fn join_guard_refuses_a_same_genesis_blob_signed_by_a_stranger() {
        let ours_key = ed25519::PrivateKey::from_seed(51).public_key();
        let stranger = ed25519::PrivateKey::from_seed(52).public_key();
        let dir = tmp("joinguard-same-genesis");
        let ours = NetworkDescriptor {
            chain_id: "home#55555555".into(),
            validators: vec![hex_bytes(ours_key.as_ref())],
            bootstrap: vec![],
            reach: vec!["10.0.0.1:52200".into()],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: "11".repeat(32),
            modules: Vec::new(),
        };
        ours.save(&dir.join("network.toml")).expect("save");

        // every genesis fact copied verbatim from a leaked invite, so the
        // fingerprint MATCHES; only what rides outside it — the reach hints,
        // the coordination mode, and (in the invite) the fronts and WireGuard
        // bootstrap `join_workspace` writes wholesale — points at the
        // attacker.
        let hijack = NetworkDescriptor {
            reach: vec!["attacker.example.com:52200".into()],
            coordination: Some("public".into()),
            ..ours.clone()
        };
        assert_eq!(
            hijack.genesis_namespace(),
            ours.genesis_namespace(),
            "the hints this blob rewrites are outside the fingerprint"
        );

        let err = guard_join_descriptor(&dir, &hijack, &stranger)
            .expect_err("a same-genesis blob signed by a non-member is refused");
        assert!(
            err.starts_with("join_not_signed_by_a_member:"),
            "its own reason token: {err}"
        );
        assert!(
            err.contains(&hex_bytes(stranger.as_ref())),
            "error names the issuer that signed it: {err}"
        );

        // the same blob from a validator on disk is the documented re-join.
        guard_join_descriptor(&dir, &hijack, &ours_key)
            .expect("a same-genesis re-join signed by a resident validator is admitted");
    }

    #[test]
    fn mixed_case_duplicate_validators_are_caught_at_the_decoded_key() {
        let a = ed25519::PrivateKey::from_seed(21).public_key();
        let lower = hex_bytes(a.as_ref());
        let upper = lower.to_ascii_uppercase();
        // constructed directly (bypassing from_toml's normalization) — the
        // dedup must hold on the DECODED key, not the spelling.
        let d = NetworkDescriptor {
            chain_id: "case#00000000".into(),
            validators: vec![lower, upper],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        assert!(
            d.validator_keys().is_err(),
            "case variants decode to one key"
        );
    }

    #[test]
    fn descriptors_canonicalize_on_load_and_fingerprint_canonically() {
        let a = ed25519::PrivateKey::from_seed(22).public_key();
        let b = ed25519::PrivateKey::from_seed(23).public_key();
        let canonical = NetworkDescriptor {
            chain_id: "canon#00000000".into(),
            validators: vec![hex_bytes(a.as_ref()), hex_bytes(b.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: modules_fixture_sorted(),
        };
        // a hand-edited twin: uppercase, whitespace, different order — in the
        // module list as much as in the validator list.
        let messy = NetworkDescriptor {
            chain_id: "canon#00000000".into(),
            validators: vec![
                format!("  {}  ", hex_bytes(b.as_ref()).to_ascii_uppercase()),
                hex_bytes(a.as_ref()),
            ],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: format!("  {}  ", "ab".repeat(32).to_ascii_uppercase()),
            modules: vec![
                ModuleCode {
                    id: "pages".into(),
                    code_hash: format!("  {}  ", "11".repeat(32).to_ascii_uppercase()),
                },
                ModuleCode {
                    id: "chat".into(),
                    code_hash: "22".repeat(32),
                },
            ],
        };
        // identical decoded sets MUST run under the identical namespace.
        assert_eq!(messy.genesis_namespace(), canonical.genesis_namespace());
        // the fingerprint is 128-bit (32 hex chars) — wide enough that
        // grinding an admit that keeps it unchanged is infeasible.
        let ns = canonical.genesis_namespace();
        assert_eq!(ns.split('@').nth(1).unwrap().len(), 32);
        // and loading the messy spelling from toml normalizes it away.
        let reloaded = NetworkDescriptor::from_toml(&messy.to_toml()).unwrap();
        assert_eq!(reloaded.validators, {
            let mut v = vec![hex_bytes(a.as_ref()), hex_bytes(b.as_ref())];
            v.sort();
            v
        });
        // the module list is canonicalized the same way: hashes trimmed and
        // lowercased, entries id-sorted.
        assert_eq!(reloaded.modules, modules_fixture_sorted());
        assert_eq!(reloaded.genesis_namespace(), canonical.genesis_namespace());
    }

    /// the `modules_fixture()` pair in canonical form: id-sorted, lowercase.
    fn modules_fixture_sorted() -> Vec<ModuleCode> {
        vec![
            ModuleCode {
                id: "chat".into(),
                code_hash: "22".repeat(32),
            },
            ModuleCode {
                id: "pages".into(),
                code_hash: "11".repeat(32),
            },
        ]
    }

    #[test]
    fn a_module_id_carrying_a_fingerprint_delimiter_is_refused() {
        // `\n{id}={hash}` is the fingerprint's framing, so an id holding `=`
        // or a newline could make two distinct module sets fold to one
        // namespace. every entry point refuses it, naming the id.
        let toml = format!(
            "chain_id = \"canon#00000000\"\nvalidators = []\ngenesis = \"{}\"\n\
             block_time_ms = 1000\n\n\
             [[modules]]\nid = \"a=b\"\ncode_hash = \"{}\"\n",
            "ab".repeat(32),
            "11".repeat(32)
        );
        let err = NetworkDescriptor::from_toml(&toml).unwrap_err();
        assert!(err.contains("a=b"), "{err}");

        let mut d = NetworkDescriptor {
            chain_id: "canon#00000000".into(),
            validators: vec![],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: "ab".repeat(32),
            modules: vec![ModuleCode {
                id: "x\ny".into(),
                code_hash: "11".repeat(32),
            }],
        };
        let err = d.module_hashes().unwrap_err();
        assert!(err.contains("x\\ny"), "{err}");

        // and an empty id is no identifier at all.
        d.modules[0].id = String::new();
        assert!(d.module_hashes().is_err());
    }

    #[test]
    fn undialable_bootstrap_hints_are_skipped_not_dialed() {
        let a = ed25519::PrivateKey::from_seed(24).public_key();
        let b = ed25519::PrivateKey::from_seed(25).public_key();
        let d = NetworkDescriptor {
            chain_id: "hints#00000000".into(),
            validators: vec![hex_bytes(a.as_ref())],
            bootstrap: vec![
                format!("{}@0.0.0.0:52200", hex_bytes(a.as_ref())),
                format!("{}@127.0.0.1:0", hex_bytes(a.as_ref())),
                format!("{}@127.0.0.1:52200", hex_bytes(b.as_ref())),
            ],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        let entries = d.bootstrap_entries().expect("well-formed hints parse");
        assert_eq!(
            entries.len(),
            1,
            "0.0.0.0 and port-0 hints are advisory noise"
        );
        assert_eq!(entries[0].0, b);
        // malformed is still an error, never a skip.
        let bad = NetworkDescriptor {
            bootstrap: vec!["nope".into()],
            reach: vec![],
            coordination: None,
            ..d
        };
        assert!(bad.bootstrap_entries().is_err());
    }

    #[test]
    fn reach_hint_canonical_roundtrips_every_kind() {
        let ek = ed25519::PrivateKey::from_seed(11).public_key();
        let ck = ed25519::PrivateKey::from_seed(12).public_key();
        let cases = [
            ReachHint {
                expected_key: ek.clone(),
                reach: Reach::Direct("127.0.0.1:9000".into()),
            },
            ReachHint {
                expected_key: ek.clone(),
                reach: Reach::Coordinated(CoordRef {
                    coord_addr: "p2p.ducktape.industries:7777".into(),
                    coord_key: ck.clone(),
                }),
            },
        ];
        for h in cases {
            let s = h.to_canonical();
            assert_eq!(ReachHint::parse(&s).expect("parse"), h, "roundtrip {s}");
        }
    }

    #[test]
    fn reach_hint_parse_rejects_malformed() {
        assert!(ReachHint::parse("nope").is_err(), "no tag");
        assert!(ReachHint::parse("direct:deadbeef").is_err(), "no @addr");
        assert!(ReachHint::parse("bogus:00@host:1").is_err(), "unknown tag");
        assert!(ReachHint::parse("direct:zz@host:1").is_err(), "bad hex key");
        // coordinated without the #coord_key delimiter:
        assert!(
            ReachHint::parse("coordinated:00@host:1").is_err(),
            "missing #coord_key"
        );
    }

    #[test]
    fn reach_field_defaults_empty_and_toml_roundtrips() {
        let a = ed25519::PrivateKey::from_seed(21).public_key();
        let mut d = NetworkDescriptor {
            chain_id: "r#00000000".into(),
            validators: vec![hex_bytes(a.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        // an existing network.toml without a [reach] array still parses (serde default),
        // and an empty reach is not serialised (skip_serializing_if).
        assert!(!d.to_toml().contains("reach"));
        d.add_reach(&ReachHint {
            expected_key: a.clone(),
            reach: Reach::Direct("10.0.0.1:9000".into()),
        });
        let back = NetworkDescriptor::from_toml(&d.to_toml()).expect("roundtrip");
        assert_eq!(back.reach, d.reach);
    }

    #[test]
    fn reach_hints_synthesizes_direct_from_bootstrap_when_reach_empty() {
        let a = ed25519::PrivateKey::from_seed(22).public_key();
        let mut d = NetworkDescriptor {
            chain_id: "r#00000000".into(),
            validators: vec![hex_bytes(a.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        d.add_bootstrap(&a, "127.0.0.1:52200");
        let hints = d.reach_hints().expect("hints");
        assert_eq!(hints.len(), 1);
        assert_eq!(
            hints[0],
            ReachHint {
                expected_key: a,
                reach: Reach::Direct("127.0.0.1:52200".into())
            }
        );
    }

    #[test]
    fn add_reach_dedups_by_expected_key_and_sorts() {
        let a = ed25519::PrivateKey::from_seed(23).public_key();
        let coord = ed25519::PrivateKey::from_seed(24).public_key();
        let mut d = NetworkDescriptor {
            chain_id: "r#00000000".into(),
            validators: vec![hex_bytes(a.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        d.add_reach(&ReachHint {
            expected_key: a.clone(),
            reach: Reach::Direct("1.1.1.1:1".into()),
        });
        // same expected_key, different reach — replaces, never duplicates.
        d.add_reach(&ReachHint {
            expected_key: a.clone(),
            reach: Reach::Coordinated(CoordRef {
                coord_addr: "c:2".into(),
                coord_key: coord,
            }),
        });
        assert_eq!(d.reach.len(), 1);
        assert!(matches!(
            d.reach_hints().unwrap()[0].reach,
            Reach::Coordinated(_)
        ));
        let mut sorted = d.reach.clone();
        sorted.sort();
        assert_eq!(d.reach, sorted);
    }

    #[test]
    fn add_reach_route_keeps_coordinated_and_overlay_routes_for_same_key() {
        let a = ed25519::PrivateKey::from_seed(25).public_key();
        let coord = ed25519::PrivateKey::from_seed(26).public_key();
        let mut d = NetworkDescriptor {
            chain_id: "r#00000000".into(),
            validators: vec![hex_bytes(a.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        d.add_reach(&ReachHint {
            expected_key: a.clone(),
            reach: Reach::Coordinated(CoordRef {
                coord_addr: "127.0.0.1:3478".into(),
                coord_key: coord,
            }),
        });
        d.add_reach_route(&ReachHint {
            expected_key: a.clone(),
            reach: Reach::Direct("[fd87::1]:52200".into()),
        });

        let hints = d.reach_hints().expect("hints");
        assert_eq!(hints.len(), 2);
        assert!(
            hints
                .iter()
                .any(|h| matches!(h.reach, Reach::Coordinated(_)))
        );
        assert!(hints.iter().any(|h| matches!(h.reach, Reach::Direct(_))));

        let entries = d.reach_entries().expect("entries");
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .any(|(_, r)| matches!(r, ReachDial::Coordinated { .. }))
        );
        assert!(
            entries
                .iter()
                .any(|(_, r)| matches!(r, ReachDial::Direct(_)))
        );
    }

    #[test]
    fn a_coordinated_hint_does_not_suppress_a_founders_direct_bootstrap_route() {
        // a founder that advertises a real dial address AND enables a
        // coordinator must keep BOTH: the direct bootstrap route (punch-free
        // first choice) and the coordinated rendezvous route. a Coordinated
        // typed hint must not erase the bootstrap-synthesised Direct for the
        // same key — otherwise every invite ships coordinator-only reach and a
        // failed punch is terminal (no relay fallback).
        let me = ed25519::PrivateKey::from_seed(41).public_key();
        let coord = ed25519::PrivateKey::from_seed(42).public_key();
        let mut d = NetworkDescriptor {
            chain_id: "fp#00000000".into(),
            validators: vec![hex_bytes(me.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        d.add_bootstrap(&me, "203.0.113.7:52200");
        d.add_reach(&ReachHint {
            expected_key: me.clone(),
            reach: Reach::Coordinated(CoordRef {
                coord_addr: "127.0.0.1:3478".into(),
                coord_key: coord,
            }),
        });

        let hints = d.reach_hints().expect("hints");
        assert_eq!(
            hints.len(),
            2,
            "the direct bootstrap route must survive alongside the coordinated hint"
        );
        assert!(
            hints
                .iter()
                .any(|h| matches!(&h.reach, Reach::Direct(a) if a == "203.0.113.7:52200")),
            "the founder's advertised direct route was dropped"
        );
        assert!(
            hints
                .iter()
                .any(|h| matches!(h.reach, Reach::Coordinated(_)))
        );

        // a typed DIRECT hint for the same key still supersedes the bootstrap
        // Direct (the member's dial address moved) — no stale duplicate.
        d.add_reach_route(&ReachHint {
            expected_key: me.clone(),
            reach: Reach::Direct("[fd87::2]:52200".into()),
        });
        let hints = d.reach_hints().expect("hints");
        let directs: Vec<_> = hints
            .iter()
            .filter(|h| matches!(h.reach, Reach::Direct(_)))
            .collect();
        assert_eq!(
            directs.len(),
            1,
            "a typed Direct supersedes the bootstrap Direct for the same key"
        );
        assert!(matches!(&directs[0].reach, Reach::Direct(a) if a == "[fd87::2]:52200"));
    }

    #[test]
    fn an_overlay_ula_route_keeps_the_underlay_bootstrap_dial() {
        // the join path records the inviter's tunnel address (the chain's
        // overlay ULA) as a typed Direct route. that route is only dialable
        // once the reachability plane's tunnels apply, so it must ride
        // ALONGSIDE the underlay bootstrap hint — not evict it — and the mesh
        // dialer must keep dialing the underlay while the plane assembles.
        let me = ed25519::PrivateKey::from_seed(51).public_key();
        let mut d = NetworkDescriptor {
            chain_id: "fp#00000000".into(),
            validators: vec![hex_bytes(me.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        d.add_bootstrap(&me, "203.0.113.7:52200");
        // the EXACT derivation cmd_join uses for the inviter's tunnel route.
        let identity = wireguard::ValidatorIdentity::try_from(me.as_ref())
            .expect("test key is a valid identity");
        let ula = wireguard::ula_v6_member_addr(&d.genesis_namespace(), identity);
        d.add_reach_route(&ReachHint {
            expected_key: me.clone(),
            reach: Reach::Direct(format!("[{ula}]:52200")),
        });

        // the union keeps BOTH routes…
        let hints = d.reach_hints().expect("hints");
        let directs: Vec<_> = hints
            .iter()
            .filter(|h| matches!(h.reach, Reach::Direct(_)))
            .collect();
        assert_eq!(
            directs.len(),
            2,
            "the overlay ULA route must not evict the underlay bootstrap hint"
        );

        // …and the dialer gets ONE Direct ingress for the key: the underlay.
        let entries = d.reach_entries().expect("entries");
        let dials: Vec<_> = entries
            .iter()
            .filter(|(k, r)| *k == me && matches!(r, ReachDial::Direct(_)))
            .collect();
        assert_eq!(dials.len(), 1, "one Direct ingress per key");
        assert!(
            matches!(
                &dials[0].1,
                ReachDial::Direct(Ingress::Socket(s)) if s.to_string() == "203.0.113.7:52200"
            ),
            "the mesh dialer must prefer the underlay route, got {:?}",
            dials[0].1
        );
    }

    #[test]
    fn reach_hints_are_excluded_from_the_genesis_fingerprint() {
        let v = ed25519::PrivateKey::from_seed(31).public_key();
        let coord = ed25519::PrivateKey::from_seed(32).public_key();
        let base = NetworkDescriptor {
            chain_id: "fp#00000000".into(),
            validators: vec![hex_bytes(v.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        let ns0 = base.genesis_namespace();

        let mut with_reach = base.clone();
        with_reach.add_bootstrap(&v, "127.0.0.1:52200");
        with_reach.add_reach(&ReachHint {
            expected_key: v.clone(),
            reach: Reach::Coordinated(CoordRef {
                coord_addr: "p2p:7777".into(),
                coord_key: coord,
            }),
        });

        // advisory reach + bootstrap NEVER move the consensus identity.
        assert_eq!(with_reach.genesis_namespace(), ns0);
        // two descriptors differing ONLY in reach fingerprint identically.
        let mut other_reach = base.clone();
        other_reach.add_reach(&ReachHint {
            expected_key: v,
            reach: Reach::Direct("9.9.9.9:9".into()),
        });
        assert_eq!(other_reach.genesis_namespace(), ns0);
    }

    // ---- Task 7: reach resolution to dial targets ----

    #[test]
    fn coordinated_hint_resolves_dial_target_to_coord_addr_with_expected_key() {
        let target = ed25519::PrivateKey::from_seed(71).public_key();
        let coord = ed25519::PrivateKey::from_seed(72).public_key();
        let mut d = NetworkDescriptor {
            chain_id: "co#00000000".into(),
            validators: vec![hex_bytes(target.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        // a coordinated hint routes through the coordinator, but the identity
        // we expect end-to-end is the TARGET; the coordinator's own ingress and
        // key ride along for the nat client to rendezvous through.
        d.add_reach(&ReachHint {
            expected_key: target.clone(),
            reach: Reach::Coordinated(CoordRef {
                coord_addr: "127.0.0.1:59999".into(),
                coord_key: coord.clone(),
            }),
        });
        let entries = d.reach_entries().expect("resolve");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, target); // expect target key
        match &entries[0].1 {
            ReachDial::Coordinated {
                coord: c,
                coord_key,
            } => {
                assert_eq!(*c, Ingress::Socket("127.0.0.1:59999".parse().unwrap()));
                assert_eq!(*coord_key, coord);
            }
            other => panic!("expected a coordinated dial, got {other:?}"),
        }
    }

    #[test]
    fn reach_entries_folds_bootstrap_and_reach_into_direct_ingresses() {
        let a = ed25519::PrivateKey::from_seed(73).public_key();
        let mut d = NetworkDescriptor {
            chain_id: "co#00000000".into(),
            validators: vec![hex_bytes(a.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            genesis: String::new(),
            modules: Vec::new(),
        };
        // a bootstrap hint alone synthesises a Direct reach ingress...
        d.add_bootstrap(&a, "127.0.0.1:52200");
        let entries = d.reach_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, a);
        assert!(
            matches!(&entries[0].1, ReachDial::Direct(i) if *i == Ingress::Socket("127.0.0.1:52200".parse().unwrap()))
        );
        // ...and an explicit reach hint for the same key wins over it (union,
        // reach-preferred), still one Direct entry.
        d.add_reach(&ReachHint {
            expected_key: a.clone(),
            reach: Reach::Direct("127.0.0.1:52201".into()),
        });
        let entries = d.reach_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(
            matches!(&entries[0].1, ReachDial::Direct(i) if *i == Ingress::Socket("127.0.0.1:52201".parse().unwrap()))
        );
    }

    #[test]
    fn sync_source_candidates_prefer_hints_and_never_self() {
        let me = ed25519::PrivateKey::from_seed(31).public_key();
        let hinted = ed25519::PrivateKey::from_seed(32).public_key();
        let validator = ed25519::PrivateKey::from_seed(33).public_key();
        let addr: SocketAddr = "127.0.0.1:52200".parse().unwrap();
        let validators = vec![me.clone(), validator.clone()];

        // a hint outside the GENESIS validator list SURVIVES — it is the
        // promoted member a joiner reaches once the founders are rotated out.
        let hints = vec![
            (hinted.clone(), addr),
            (validator.clone(), addr),
            (me.clone(), addr),
        ];
        assert_eq!(
            sync_source_candidates(&hints, &validators, &me),
            vec![hinted.clone(), validator.clone()]
        );

        // no usable hint: any validator that is not us.
        let no_hints: &[(ed25519::PublicKey, SocketAddr)] = &[];
        assert_eq!(
            sync_source_candidates(no_hints, &validators, &me),
            vec![validator.clone()]
        );

        // solo network: nobody can serve.
        assert!(sync_source_candidates(no_hints, std::slice::from_ref(&me), &me).is_empty());
    }
}
