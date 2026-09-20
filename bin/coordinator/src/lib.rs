//! Coordinator policy selection — the ONLY decision the untrusted coordinator
//! makes at boot: which [`nat_traversal::AuthPolicy`] to serve.
//!
//! Factored out of `main.rs` so it is unit-testable without spawning the
//! process. The coordinator stays keyless and dials nothing: `--genesis-set`
//! reads ONLY the PUBLIC validator pubkeys out of a `network.toml` (never a
//! secret, never written back), and every other input is a bare CLI flag.

use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519;
use serde::Deserialize;

/// This process's CPU time across every thread, user and system, in
/// nanoseconds — the kernel's own accounting, asked the POSIX way
/// (`getrusage`) so the reading is the same call on every host.
pub fn process_cpu_ns() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: `getrusage` writes one `rusage` into the pointer it is handed,
    // and `RUSAGE_SELF` is always a valid subject.
    let filled = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } == 0;
    if !filled {
        return None;
    }
    // SAFETY: the call returned 0, so the struct is filled.
    let usage = unsafe { usage.assume_init() };
    timeval_ns(usage.ru_utime)?.checked_add(timeval_ns(usage.ru_stime)?)
}

fn timeval_ns(time: libc::timeval) -> Option<u64> {
    let seconds = u64::try_from(time.tv_sec).ok()?;
    let microseconds = u64::try_from(time.tv_usec).ok()?;
    seconds
        .checked_mul(1_000_000_000)?
        .checked_add(microseconds.checked_mul(1_000)?)
}

/// This process's resident set, in bytes. The kernel keeps it where the host
/// keeps process facts: `/proc` on Linux, the task info call on macOS.
#[cfg(target_os = "linux")]
pub fn process_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    kib.checked_mul(1024)
}

#[cfg(target_os = "macos")]
pub fn process_rss_bytes() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::uninit();
    let size = i32::try_from(std::mem::size_of::<libc::proc_taskinfo>()).ok()?;
    let pid = i32::try_from(std::process::id()).ok()?;
    // SAFETY: `proc_pidinfo` writes at most `size` bytes into the buffer it
    // is handed, which is exactly one `proc_taskinfo`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: the call wrote the whole struct.
    let info = unsafe { info.assume_init() };
    Some(info.pti_resident_size)
}

/// The one field of `network.toml` the coordinator cares about: the genesis
/// validators, as hex ed25519 public keys. Every other key (chain_id,
/// bootstrap, reach, coordination, …) is ignored — serde drops unknown fields —
/// so a full descriptor parses here without dragging in `bin/node`.
#[derive(Debug, Deserialize)]
struct GenesisPin {
    #[serde(default)]
    validators: Vec<String>,
}

/// Select the authorization policy from CLI flags:
/// `--genesis-set <path>` => Private (the genesis set of that network.toml
///                           roots every cap chain it admits);
/// otherwise              => public with proof-of-possession.
pub fn select_policy(args: &[String]) -> std::io::Result<nat_traversal::AuthPolicy> {
    // `--genesis-set` presence is detected SEPARATELY from its value: a present
    // but value-less flag (bare `--genesis-set`, `--genesis-set` as the final
    // token, or immediately followed by another `--flag` — e.g. an unset shell
    // variable that collapses to nothing) is a HARD error, never a silent
    // fall-through to the weaker public policy. Downgrading a
    // Private (genesis/cap-gated) coordinator to public-PoP on a typo'd path
    // would admit any node with a valid proof-of-possession.
    if let Some(i) = args.iter().position(|a| a == "--genesis-set") {
        let path = args
            .get(i + 1)
            .filter(|v| !v.starts_with("--"))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "--genesis-set requires a <network.toml> path",
                )
            })?;
        let genesis_set = load_genesis_pubkeys(path)?;
        return Ok(nat_traversal::AuthPolicy::Private { genesis_set });
    }
    Ok(nat_traversal::AuthPolicy::Public)
}

/// Parse the PUBLIC genesis validator pubkeys out of a `network.toml`. This is
/// the ONLY new input the coordinator reads — public data, never a secret.
/// Mirrors `NetworkDescriptor::validator_keys` (bin/node/src/config.rs) without
/// depending on the node crate: decode each hex entry to an ed25519 pubkey and
/// reject a duplicate (a repeat would otherwise be a silently smaller valset).
fn load_genesis_pubkeys(path: &str) -> std::io::Result<Vec<ed25519::PublicKey>> {
    let text = std::fs::read_to_string(path)?;
    let pin: GenesisPin = toml::from_str(&text).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("network.toml: {e}"),
        )
    })?;
    let invalid = |msg: String| std::io::Error::new(std::io::ErrorKind::InvalidData, msg);

    let keys: Vec<ed25519::PublicKey> = pin
        .validators
        .iter()
        .map(|h| decode_key(h))
        .collect::<Result<_, _>>()
        .map_err(invalid)?;

    let mut seen = std::collections::BTreeSet::new();
    for k in &keys {
        if !seen.insert(k.as_ref().to_vec()) {
            return Err(invalid(format!(
                "duplicate validator {} in genesis set",
                hex_bytes(k.as_ref())
            )));
        }
    }
    Ok(keys)
}

/// Decode one hex-encoded ed25519 public key. Dependency-free hex (the
/// coordinator does not pull in bin/node's `unhex`); strict digits, even length.
fn decode_key(hex: &str) -> Result<ed25519::PublicKey, String> {
    let raw = unhex(hex.trim())?;
    ed25519::PublicKey::decode(raw.as_slice())
        .map_err(|e| format!("{hex:?} is not an ed25519 public key: {e}"))
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("hex string contains non-hex characters".into());
    }
    if !s.len().is_multiple_of(2) {
        return Err("hex string has odd length".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod readings {
    use super::*;

    /// Both readings answer on this host, and CPU time only grows: a process
    /// that just did work has spent more of it than before.
    #[test]
    fn the_process_readings_answer_and_cpu_time_only_grows() {
        let before = process_cpu_ns().expect("cpu time reads on this host");
        let mut spent = 0u64;
        for step in 0..2_000_000u64 {
            spent = spent.wrapping_mul(31).wrapping_add(step);
        }
        assert_ne!(spent, 1, "the loop ran");
        let after = process_cpu_ns().expect("cpu time reads on this host");
        assert!(
            after >= before,
            "cpu time went backwards: {before} → {after}"
        );
        let rss = process_rss_bytes().expect("resident size reads on this host");
        assert!(rss > 0, "a running process is resident");
    }
}
