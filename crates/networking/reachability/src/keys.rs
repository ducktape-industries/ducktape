//! The node's WireGuard identity: an X25519 keypair persisted beside
//! `identity.key`. Deliberately SEPARATE from the ed25519 signer — WireGuard
//! speaks X25519, and the two rotate independently: a WireGuard re-key is a
//! re-advertisement (a new mesh version), never an identity change.

use std::path::Path;

use wireguard::X25519PublicKey;

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("wireguard key file {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("wireguard key file {path}: expected 64 hex chars")]
    Malformed { path: String },
}

/// The persistent X25519 keypair driving this node's WireGuard interface.
pub struct WireGuardKeypair {
    secret: x25519_dalek::StaticSecret,
}

impl WireGuardKeypair {
    /// Load the keypair at `path`, or generate one and persist it. Returns
    /// `(keypair, generated)`. Mirrors `identity.key` discipline exactly:
    /// 64 hex chars on one line, and the file is BORN 0600 via `create_new`
    /// (no write-then-chmod window a co-tenant could read the secret
    /// through). ONE key per path, however many generators race for it: the
    /// node's own plane and a CLI minting an invite against the same
    /// workspace both reach here at boot, and the loser must come away
    /// holding the key that won — never an error, never a second key.
    pub fn load_or_generate(path: &Path) -> Result<(Self, bool), KeyError> {
        if path.exists() {
            return Self::load(path).map(|k| (k, false));
        }
        let mut raw = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut raw);
        // every 32-byte string is a valid X25519 secret (the scheme clamps),
        // so construction cannot fail on fresh OS randomness.
        let secret = x25519_dalek::StaticSecret::from(raw);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| io_error(path, e))?;
        }
        Self::publish(secret, path)
    }

    /// Persist a freshly generated `secret` at `path`, or load the key another
    /// generator published first. The key is COMPLETE before it can be
    /// observed: written to a private scratch file, then hard-linked into
    /// place — an atomic, no-clobber publish — so a racing generator either
    /// links first or finds a whole file to load, never a half-written one.
    fn publish(secret: x25519_dalek::StaticSecret, path: &Path) -> Result<(Self, bool), KeyError> {
        let scratch = scratch_path(path);
        write_private(&scratch, hex_encode(&secret.to_bytes()).as_bytes())
            .map_err(|e| io_error(&scratch, e))?;
        let linked = std::fs::hard_link(&scratch, path);
        // the scratch name is ours alone; the published file keeps the inode.
        let _ = std::fs::remove_file(&scratch);
        match linked {
            Ok(()) => Ok((Self { secret }, true)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Self::load(path).map(|k| (k, false))
            }
            Err(e) => Err(io_error(path, e)),
        }
    }

    fn load(path: &Path) -> Result<Self, KeyError> {
        let text = std::fs::read_to_string(path).map_err(|e| KeyError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        let raw = hex_decode32(text.trim()).ok_or_else(|| KeyError::Malformed {
            path: path.display().to_string(),
        })?;
        Ok(Self {
            secret: x25519_dalek::StaticSecret::from(raw),
        })
    }

    /// The public key peers advertise for us and pin in tunnel handshakes.
    pub fn public_key(&self) -> X25519PublicKey {
        X25519PublicKey(x25519_dalek::PublicKey::from(&self.secret).to_bytes())
    }

    /// The raw X25519 private key the interface effect installs.
    pub fn private_key_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }

    /// Open a [`crate::seal`] envelope sealed to this node's WireGuard X25519
    /// key. The secret never leaves the keypair — a member opens a joiner's
    /// sealed first-contact intro through here.
    pub fn open_sealed(&self, sealed: &[u8]) -> Result<Vec<u8>, String> {
        crate::seal::open(&self.secret, sealed)
    }
}

fn io_error(path: &Path, source: std::io::Error) -> KeyError {
    KeyError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// a scratch name beside `path` that no other generator can be using: the
/// process id tells racing processes apart, the nonce tells racing threads
/// of one process apart. never derived from the secret.
fn scratch_path(path: &Path) -> std::path::PathBuf {
    let nonce = rand::RngCore::next_u64(&mut rand::rngs::OsRng);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{}.{nonce:016x}.tmp", std::process::id()))
}

/// create `path` fresh, born 0600, holding exactly `bytes`.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in text.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_persists_and_reloads_the_same_keypair() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wireguard.key");

        let (fresh, generated) = WireGuardKeypair::load_or_generate(&path).unwrap();
        assert!(generated);
        assert_ne!(fresh.public_key().0, [0u8; 32]);

        let (reloaded, generated_again) = WireGuardKeypair::load_or_generate(&path).unwrap();
        assert!(!generated_again);
        assert_eq!(reloaded.public_key(), fresh.public_key());
        assert_eq!(reloaded.private_key_bytes(), fresh.private_key_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_born_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wireguard.key");
        WireGuardKeypair::load_or_generate(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn rejects_a_corrupt_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wireguard.key");
        std::fs::write(&path, "not hex at all").unwrap();
        assert!(matches!(
            WireGuardKeypair::load_or_generate(&path),
            Err(KeyError::Malformed { .. })
        ));
    }

    /// the losing generator of a race comes away holding the key that won:
    /// a keypair published under `path` after this one decided to generate
    /// is the one it returns, `generated` is false, and its scratch file is
    /// gone. this is the node's plane and an invite mint racing for one
    /// workspace's key at boot.
    #[test]
    fn a_generator_that_loses_the_publish_race_loads_the_winner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wireguard.key");
        let (winner, _) = WireGuardKeypair::load_or_generate(&path).unwrap();

        let mut raw = [7u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut raw);
        let loser_secret = x25519_dalek::StaticSecret::from(raw);
        let (held, generated) = WireGuardKeypair::publish(loser_secret, &path).unwrap();

        assert!(!generated, "the loser did not publish");
        assert_eq!(
            held.public_key(),
            winner.public_key(),
            "it holds the winner's key"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(leftovers, ["wireguard.key"], "no scratch file survives");
    }

    /// every generator racing for one path agrees on ONE key, and exactly
    /// one of them generated it.
    #[test]
    fn concurrent_generators_agree_on_one_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wireguard.key");
        let outcomes: Vec<(X25519PublicKey, bool)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        let (key, generated) = WireGuardKeypair::load_or_generate(&path).unwrap();
                        (key.public_key(), generated)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let (published, _) = WireGuardKeypair::load_or_generate(&path).unwrap();
        for (key, _) in &outcomes {
            assert_eq!(
                *key,
                published.public_key(),
                "every generator holds the published key"
            );
        }
        let generated = outcomes.iter().filter(|(_, generated)| *generated).count();
        assert_eq!(generated, 1, "exactly one generator published");
    }
}
