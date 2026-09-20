//! Per-binding **scoped service keys**: the credential an attached personal
//! session signs its delivery receipts with.
//!
//! ## why not the owner's wallet key
//!
//! A receipt is emitted by a daemon reacting to a provider event — no human, no
//! terminal, no password. A wallet key costs an argon2id pass over 64 MiB and a
//! prompt, so it cannot be the signer here. Handing the daemon the owner's
//! account key would be worse still: it would let an attached session act as the
//! whole account, when what it needs is to advance one conversation's delivery
//! records.
//!
//! So a binding authorizes a key of its own. The OWNER signs the `Bind` that
//! names this key's public half; the scoped key never authorizes itself, and
//! replacing a binding retires the old key by generation.
//!
//! ## the trust model is the workspace, not the port
//!
//! The key is 32 bytes, hex, under the node's workspace — the same shape and
//! the same boundary as `service-link.token` and `admin.token`
//! (`noded::services`). "Can read the node's own workspace" is the whole claim.
//! A sandbox guest reaching the node's listener through its vsock tunnel holds
//! no workspace and so holds no key.
//!
//! On unix it is mode 0600 from creation. **On a non-unix host it is not
//! permission-restricted at all** — `OpenOptionsExt::mode` is a unix API with
//! no portable equivalent — so there the directory's ACLs are the only guard.
//! Stated rather than glossed: the two hosts do not offer the same protection,
//! and `publish_owner_only`'s non-unix arm says so at the code.
//!
//! Unlike those two, a service key **persists across boots**: its public half is
//! committed on-chain in the binding, so re-minting it at boot would silently
//! revoke every attachment on this host.
//!
//! ## a key is scoped to one NETWORK as well as one binding
//!
//! `chain_id` is part of the binding's identity here, not decoration. One
//! workspace can be pointed at a second network, and conversation and
//! participant ids are module-defined strings that carry no network in them —
//! so `("standup", "alice")` on network A and on network B are the same two
//! strings. Without the chain id they would name one key file.
//!
//! That matters because a submit frame is NOT chain-bound: `user_frame` signs
//! `signer ‖ seq ‖ target ‖ payload` and nothing else, so a frame minted for
//! one network verifies byte-identically on another. Sharing one key across
//! both would make a `Send` or `Acknowledge` signed for A replayable as the
//! same binding's op on B. Fencing the KEY is what stops it: B's binding
//! commits a different public key, so A's frame is signed by a key B's module
//! does not recognise, and B refuses it.
//!
//! The fence is the workspace's own `network.toml` (`chain_id`, which that
//! file's own doc calls the namespace) — immutable per network and readable
//! with no node running.
//!
//! ## the filename is a digest over a LENGTH-PREFIXED tuple
//!
//! A conversation id and a participant id are module-defined strings, and core
//! constrains them by length alone — `registry::check_id` accepts any bytes
//! that are non-empty and within `MAX_ID_BYTES`. So an id may contain ANY byte,
//! including whatever this file might pick as a separator.
//!
//! That rules out delimiter framing. A digest over `network ‖ 0x1f ‖
//! conversation ‖ 0x1f ‖ participant` collides on ids core accepts:
//! `("n", "a\x1fb", "c")` and `("n", "a", "b\x1fc")` are different bindings
//! that hash the same bytes, so two bindings would share one key — and a
//! session scoped to one conversation could sign the other's receipts.
//!
//! The digest is therefore over a length-prefixed tuple under a domain string:
//!
//! ```text
//! sha256( DOMAIN ‖ len_be(network) ‖ network
//!                ‖ len_be(conversation) ‖ conversation
//!                ‖ len_be(participant) ‖ participant )
//! ```
//!
//! A length prefix is unambiguous whatever the content, so no id can be spelled
//! to impersonate another tuple. The domain separates this digest from every
//! other sha256 in the tree. No id can walk out of the directory, and nothing
//! here has to be listed by name.

use commonware_codec::DecodeExt as _;
use commonware_cryptography::{Signer as _, ed25519};
use sha2::Digest as _;

#[cfg(test)]
use crate::wire::collaboration;

/// where a workspace keeps its scoped messaging keys. `collab-keys` rather than
/// `keys`, which is the WALLET keystore's directory: these are not wallets and
/// must never be offered as one.
const DIR: &str = "collab-keys";

/// One binding's identity: the network it lives on, and the two ids that name
/// it there on-chain.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BindingRef<'a> {
    /// the workspace's `network.toml` `chain_id`. Two networks never share a
    /// scoped key — see this module's header.
    pub(crate) network: &'a str,
    pub(crate) conversation: &'a str,
    pub(crate) participant: &'a str,
}

/// Domain separation for [`BindingRef::file_name`]: this digest names a scoped
/// service key and must never coincide with another sha256 over the same parts.
const KEY_FILE_DOMAIN: &str = "ducktape-collab-service-key-v1";

impl BindingRef<'_> {
    /// the digest that names this binding's key file.
    ///
    /// LENGTH-PREFIXED, not delimiter-separated: core accepts any bytes inside
    /// an id (`registry::check_id` checks length alone), so any separator this
    /// picked could appear in an id and let two different bindings hash
    /// identically. A length prefix cannot be spelled by content.
    fn file_name(&self) -> String {
        let mut hasher = sha2::Sha256::new();
        hasher.update(KEY_FILE_DOMAIN.as_bytes());
        for part in [self.network, self.conversation, self.participant] {
            // u64 big-endian: fixed width, so the prefix itself never has to be
            // parsed out of the stream to know where a part begins.
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part.as_bytes());
        }
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

fn key_path(workspace: &std::path::Path, binding: BindingRef<'_>) -> std::path::PathBuf {
    workspace.join(DIR).join(binding.file_name())
}

/// Load this binding's service key, or mint one if it has none.
///
/// Idempotent on purpose: `collab attach` is re-runnable, and a second attach
/// to the same binding must present the SAME public key or the on-chain
/// binding would stop matching the key that signs its receipts. Minting a fresh
/// key per call would make every re-attach silently break the previous one.
pub(crate) fn ensure(
    workspace: &std::path::Path,
    binding: BindingRef<'_>,
) -> Result<ed25519::PrivateKey, String> {
    if let Some(existing) = load(workspace, binding)? {
        return Ok(existing);
    }
    mint(workspace, binding)
}

/// This binding's service key, or `None` when it has never been attached here.
///
/// `None` is an ordinary state — a binding on another device — and never an
/// error, so a caller can tell "not attached here" from "attached and
/// unreadable" (a permissions or corruption problem, which IS an error).
pub(crate) fn load(
    workspace: &std::path::Path,
    binding: BindingRef<'_>,
) -> Result<Option<ed25519::PrivateKey>, String> {
    let path = key_path(workspace, binding);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("service key {}: {error}", path.display())),
    };
    // the PATH, never the contents: this error reaches a terminal and a log.
    let seed = crate::config::unhex(text.trim())
        .map_err(|_| format!("service key {} is not hex", path.display()))?;
    let key = ed25519::PrivateKey::decode(seed.as_slice())
        .map_err(|_| format!("service key {} is not an ed25519 key", path.display()))?;
    Ok(Some(key))
}

/// Mint a fresh service key for this binding.
///
/// Two runs racing on one binding cannot both mint: the loser's publish fails
/// with `AlreadyExists` and it reads the winner's key instead of overwriting it.
/// Overwriting would strand the on-chain binding on a public key nothing holds.
fn mint(
    workspace: &std::path::Path,
    binding: BindingRef<'_>,
) -> Result<ed25519::PrivateKey, String> {
    let dir = workspace.join(DIR);
    std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let path = key_path(workspace, binding);
    let seed = noded::services::new_secret();
    match publish_owner_only(&dir, &path, &seed) {
        Ok(()) => {}
        // lost the race: the winner's key is the binding's key.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return load(workspace, binding)?
                .ok_or_else(|| format!("service key {} vanished mid-mint", path.display()));
        }
        Err(error) => return Err(format!("service key {}: {error}", path.display())),
    }
    let bytes = crate::config::unhex(&seed).map_err(|error| error.to_string())?;
    ed25519::PrivateKey::decode(bytes.as_slice())
        .map_err(|_| "a fresh 32-byte secret is not a valid ed25519 key".to_string())
}

/// Publish a secret at `path`: COMPLETE, durable, atomic, and never clobbering.
///
/// All four properties are load-bearing, and `create_new` + `write_all` gave
/// only the last one:
///
/// - **Complete.** Creating the final path and then writing it leaves a window
///   where the file EXISTS and is empty or half-written. A concurrent `ensure`
///   reads it in that window and gets a truncated key — or, worse, `load`
///   errors on a "corrupt" key that is merely unfinished. The secret is written
///   to a temp name and only ever appears at `path` complete.
/// - **Durable.** `attach` submits an owner-signed `Bind` naming this key's
///   public half. If the machine loses power after that op commits but before
///   the bytes reach the platter, the network holds a binding whose key this
///   device no longer has — unrecoverable without a fresh `Bind`. So the data
///   is fsynced BEFORE it is published, and the directory entry is fsynced
///   after, because an unsynced directory can lose the link itself.
/// - **Atomic and non-clobbering together.** `rename` is atomic but replaces,
///   which would silently rotate a key an on-chain binding already names.
///   `hard_link` is atomic and fails with `AlreadyExists` instead — the same
///   refusal `create_new` gave, so the race path above is unchanged.
///
/// The temp file is created in the SAME directory so the link cannot cross a
/// filesystem, and 0600 from the start so there is no world-readable window.
#[cfg(unix)]
fn publish_owner_only(
    dir: &std::path::Path,
    path: &std::path::Path,
    secret: &str,
) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let tmp = dir.join(temp_name());
    // a leftover from a killed run must not wedge this mint forever. Safe to
    // remove because the name is THIS call's alone — see `temp_name`.
    let _ = std::fs::remove_file(&tmp);

    let published = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(secret.as_bytes())?;
        // the DATA is durable before any name points at it.
        file.sync_all()?;
        drop(file);
        std::fs::hard_link(&tmp, path)
    })();

    // the temp name is scratch either way: on success `path` is the same inode,
    // on failure it holds a secret nothing will ever publish.
    let _ = std::fs::remove_file(&tmp);
    published?;

    // fsync the DIRECTORY: the bytes are durable but the name that finds them
    // is not until its parent is synced.
    std::fs::File::open(dir)?.sync_all()
}

/// A temp name belonging to exactly ONE call of [`publish_owner_only`].
///
/// Per-CALL, not per-binding: two threads minting the same binding is the
/// ordinary race (`collab attach` twice, a daemon and an operator at once), and
/// a name keyed on pid and binding is identical for both. They would then share
/// one temp file, and the pre-remove above would delete the other's half-written
/// secret — whose `hard_link` then fails ENOENT, turning a race that should
/// converge into an error. The counter is what makes each attempt's scratch its
/// own; the pid keeps two processes apart.
fn temp_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ATTEMPT: AtomicU64 = AtomicU64::new(0);
    format!(
        ".mint.{}.{}",
        std::process::id(),
        ATTEMPT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Non-unix publish: complete, durable and non-clobbering, but **NOT
/// permission-restricted**.
///
/// `OpenOptionsExt::mode` is a unix API and there is no portable equivalent, so
/// this file is protected by the workspace directory's ACLs alone. That is a
/// weaker claim than the unix path's 0600 and this comment exists so nobody
/// reads the module header as promising otherwise on such a host.
#[cfg(not(unix))]
fn publish_owner_only(
    dir: &std::path::Path,
    path: &std::path::Path,
    secret: &str,
) -> std::io::Result<()> {
    use std::io::Write as _;

    let tmp = dir.join(temp_name());
    let _ = std::fs::remove_file(&tmp);

    let published = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(secret.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::hard_link(&tmp, path)
    })();

    let _ = std::fs::remove_file(&tmp);
    published
}

/// The public half — what a `Bind` op carries on-chain as the binding's
/// authorized signer.
pub(crate) fn public_hex(key: &ed25519::PrivateKey) -> String {
    crate::config::hex_bytes(key.public_key().as_ref())
}

// ---- what this device has attached ----------------------------------------

/// The attachments this device has made, one JSON object per line.
///
/// Separate from the keys on purpose. [`BindingRef::file_name`] is a DIGEST, so
/// the key directory cannot be read back into the ids that produced it — which
/// is the right shape for a secret's name and the wrong one for the node's own
/// collaboration pump, which has to know which bindings to pump before it can
/// authenticate a single read.
///
/// It discloses nothing new: a reader of this file can read the keys beside it,
/// which is total control of every binding it lists. And the ids are on-chain
/// already — the pump would learn them from the module the moment it held a key.
const ATTACHED: &str = "attached.jsonl";

/// One binding this device holds a key for, and the local session label its
/// deliveries are aimed at.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct Attached {
    pub(crate) network: String,
    pub(crate) conversation: String,
    pub(crate) participant: String,
    /// the opaque label the daemon resolves to a local provider session. It is
    /// the operator's `collab attach --device`, and the network records the same
    /// string — it names no path, session id or token.
    pub(crate) device: String,
}

/// Record that this device attached `binding` to `device`.
///
/// Append-only, and called BEFORE the owner-signed `Bind` for the same reason
/// the key is minted before it: a crash between the two leaves a device that can
/// still find and re-drive its binding, where the reverse order leaves the pump
/// blind to a binding the network already holds.
///
/// A line for a binding already listed is still appended — [`attached`] keeps
/// the LAST, so a re-attach to a new device label supersedes the old one without
/// this having to rewrite a file the pump may be reading.
pub(crate) fn remember(
    workspace: &std::path::Path,
    binding: BindingRef<'_>,
    device: &str,
) -> Result<(), String> {
    use std::io::Write as _;
    let dir = workspace.join(DIR);
    std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let path = dir.join(ATTACHED);
    let line = serde_json::to_string(&Attached {
        network: binding.network.to_string(),
        conversation: binding.conversation.to_string(),
        participant: binding.participant.to_string(),
        device: device.to_string(),
    })
    .map_err(|error| format!("attachment record: {error}"))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    // one write of one short line: the append is atomic on every filesystem
    // this runs on, so a concurrent `attach` cannot interleave into it.
    file.write_all(format!("{line}\n").as_bytes())
        .map_err(|error| format!("{}: {error}", path.display()))?;
    file.sync_data()
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// The bindings this device has attached on `network`, latest label per binding.
///
/// A missing file is an empty list, not an error: a node that has never run
/// `collab attach` has nothing to pump, which is an ordinary state. An
/// unparseable LINE is skipped with a warn rather than failing the read — one
/// torn tail (a crash mid-append) must not blind the pump to every binding
/// above it.
pub(crate) fn attached(
    workspace: &std::path::Path,
    network: &str,
) -> Result<Vec<Attached>, String> {
    let path = workspace.join(DIR).join(ATTACHED);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let mut latest: std::collections::BTreeMap<(String, String), Attached> = Default::default();
    let mut unreadable = 0usize;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(record) = serde_json::from_str::<Attached>(line) else {
            unreadable += 1;
            continue;
        };
        if record.network != network {
            continue;
        }
        latest.insert(
            (record.conversation.clone(), record.participant.clone()),
            record,
        );
    }
    if unreadable > 0 {
        tracing::warn!(
            target: "ducktape::collab",
            reason = "attachment_record_unreadable",
            lines = unreadable,
            "skipped attachment records this build cannot read"
        );
    }
    Ok(latest.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("temp workspace")
    }

    const NET: &str = "ducktape#a1b2c3d4";

    fn binding<'a>(conversation: &'a str, participant: &'a str) -> BindingRef<'a> {
        on_network(NET, conversation, participant)
    }

    fn on_network<'a>(
        network: &'a str,
        conversation: &'a str,
        participant: &'a str,
    ) -> BindingRef<'a> {
        BindingRef {
            network,
            conversation,
            participant,
        }
    }

    /// Re-attaching must present the SAME public key. A fresh key per call
    /// would leave the committed binding naming a key nothing holds, and every
    /// receipt it signed afterwards would be refused.
    #[test]
    fn ensure_is_idempotent_for_one_binding() {
        let dir = workspace();
        let first = ensure(dir.path(), binding("c1", "p1")).expect("mints");
        let second = ensure(dir.path(), binding("c1", "p1")).expect("loads");
        assert_eq!(public_hex(&first), public_hex(&second));
    }

    /// Racing mints converge on ONE key, and every racer gets that key.
    ///
    /// This is the property the publish rewrite exists for. Exactly one thread
    /// wins the `hard_link`; every loser must see `AlreadyExists` and read the
    /// WINNER's key rather than overwrite it — an overwrite would strand the
    /// on-chain binding on a public key nothing holds.
    ///
    /// It also covers the partial-read window that `create_new` + `write_all`
    /// had: under that shape a loser could observe the final path existing but
    /// empty and fail to decode it. Here the name only ever appears complete,
    /// so a loser either does not see it or sees all 32 bytes.
    ///
    /// Threads synchronize on a barrier, not a sleep — they are released
    /// together and the assertions are over the values they return.
    #[test]
    fn racing_mints_converge_on_one_key_and_never_clobber() {
        use std::sync::{Arc, Barrier};

        let dir = workspace();
        let racers = 8;
        let start = Arc::new(Barrier::new(racers));
        let path = Arc::new(dir.path().to_path_buf());

        let minted: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..racers)
                .map(|_| {
                    let start = Arc::clone(&start);
                    let path = Arc::clone(&path);
                    scope.spawn(move || {
                        start.wait();
                        let key = ensure(&path, binding("c1", "p1")).expect("mints or loads");
                        public_hex(&key)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("no racer panicked"))
                .collect()
        });

        let winner = &minted[0];
        assert!(
            minted.iter().all(|key| key == winner),
            "every racer must end up with the one published key: {minted:?}"
        );

        // and the file on disk is that key, not a ninth one.
        let stored = load(dir.path(), binding("c1", "p1"))
            .expect("readable")
            .expect("published");
        assert_eq!(&public_hex(&stored), winner);

        // exactly one key file, and no temp left behind.
        let entries: Vec<String> = std::fs::read_dir(dir.path().join(DIR))
            .expect("key dir")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
            .collect();
        assert_eq!(entries.len(), 1, "one binding, one file: {entries:?}");
        assert!(
            !entries[0].starts_with(".mint."),
            "a temp file was published or left behind: {entries:?}"
        );
    }

    /// A key is scoped to ONE binding: that is what "scoped" means, and it is
    /// why a compromised session cannot acknowledge another conversation's
    /// deliveries.
    #[test]
    fn each_binding_gets_its_own_key() {
        let dir = workspace();
        let a = ensure(dir.path(), binding("c1", "p1")).expect("mints");
        let other_conversation = ensure(dir.path(), binding("c2", "p1")).expect("mints");
        let other_participant = ensure(dir.path(), binding("c1", "p2")).expect("mints");
        assert_ne!(public_hex(&a), public_hex(&other_conversation));
        assert_ne!(public_hex(&a), public_hex(&other_participant));
    }

    /// The parts are separated before hashing, so a shift of any boundary
    /// between them cannot name the same file.
    #[test]
    fn the_id_parts_cannot_collide_by_shifting_a_boundary() {
        assert_ne!(
            binding("ab", "c").file_name(),
            binding("a", "bc").file_name()
        );
        assert_ne!(
            on_network("net", "a", "p").file_name(),
            on_network("ne", "ta", "p").file_name()
        );
    }

    /// A separator INSIDE an id must not let two bindings name one key file.
    ///
    /// This is why the digest is length-prefixed rather than delimiter-framed.
    /// Core constrains an id by length alone (`registry::check_id`: non-empty,
    /// within `MAX_ID_BYTES`), so every byte is legal inside one — including
    /// whatever this module might pick as a separator. Under the old
    /// `‖ 0x1f ‖` framing these two hash identical bytes, and the two bindings
    /// share a key: a session scoped to one conversation could then sign the
    /// other's receipts.
    ///
    /// Both ids here are ones core ACCEPTS, which is what makes it a real
    /// collision rather than a theoretical one.
    #[test]
    fn a_separator_inside_an_id_cannot_forge_another_binding() {
        for separator in ['\u{1f}', '\0', '/', ':'] {
            let left = format!("a{separator}b");
            let right = format!("b{separator}c");
            assert_ne!(
                on_network("n", &left, "c").file_name(),
                on_network("n", "a", &right).file_name(),
                "a {separator:?} inside an id collided two bindings"
            );
        }

        // and across the network boundary, the same way.
        assert_ne!(
            on_network("n\u{1f}a", "b", "c").file_name(),
            on_network("n", "a\u{1f}b", "c").file_name()
        );
    }

    /// The ids core actually accepts are arbitrary bytes within a length cap,
    /// so the digest must be total over them — no panic, no escaping rule, and
    /// a distinct name for every distinct tuple.
    #[test]
    fn every_id_core_accepts_gets_its_own_name() {
        let longest = "z".repeat(collaboration::MAX_ID_BYTES);
        let cases = [
            on_network("n", "a", "b"),
            on_network("n", "a", "b "),
            on_network("n", " a", "b"),
            on_network("n", "a\u{1f}", "b"),
            on_network("n", "a", "\u{1f}b"),
            on_network("n", "🦆", "b"),
            on_network("n", &longest, "b"),
            on_network("n", "a", &longest),
        ];
        let mut names: Vec<String> = cases.iter().map(BindingRef::file_name).collect();
        let total = names.len();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            total,
            "two accepted id tuples share a key file"
        );
        assert!(
            names
                .iter()
                .all(|name| name.len() == 64 && name.chars().all(|c| c.is_ascii_hexdigit())),
            "every name is a hex digest"
        );
    }

    /// One workspace pointed at a second network must not reuse the first
    /// network's scoped key for the same two ids.
    ///
    /// A submit frame is not chain-bound (`user_frame` signs signer, seq,
    /// target and payload — no chain id), so one shared key would make a `Send`
    /// or `Acknowledge` minted for one network replayable as the same binding's
    /// op on the other. Distinct keys are what refuse it: the second network's
    /// binding commits a different public key.
    #[test]
    fn the_same_binding_on_another_network_gets_another_key() {
        let dir = workspace();
        let here = ensure(
            dir.path(),
            on_network("ducktape#aaaa1111", "standup", "alice"),
        )
        .expect("mints");
        let elsewhere = ensure(
            dir.path(),
            on_network("ducktape#bbbb2222", "standup", "alice"),
        )
        .expect("mints");
        assert_ne!(
            public_hex(&here),
            public_hex(&elsewhere),
            "one workspace, two networks, two keys"
        );
    }

    /// A module-defined id is an arbitrary string. It must not be able to name
    /// a file outside the key directory.
    #[test]
    fn a_hostile_id_cannot_escape_the_key_directory() {
        let dir = workspace();
        let hostile = binding("../../../etc/passwd", "../../root/.ssh/id_ed25519");
        let path = key_path(dir.path(), hostile);
        let key_dir = dir.path().join(DIR);
        assert_eq!(
            path.parent(),
            Some(key_dir.as_path()),
            "every key lands in the key directory: {}",
            path.display()
        );
        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.len() == 64
                    && name.chars().all(|c| c.is_ascii_hexdigit())),
            "the file name is a digest, not an id"
        );
        ensure(dir.path(), hostile).expect("a hostile id is ordinary input here");
    }

    /// "Not attached on this device" is a state, not a failure — a binding can
    /// live on another laptop.
    #[test]
    fn an_unattached_binding_loads_as_none() {
        let dir = workspace();
        assert!(
            load(dir.path(), binding("never", "attached"))
                .expect("absence is not an error")
                .is_none()
        );
    }

    /// A key that IS there and cannot be read is a real problem and must not
    /// read as "not attached here" — that would silently mint a second key and
    /// strand the committed binding.
    #[test]
    fn a_corrupt_key_is_an_error_not_an_absence() {
        let dir = workspace();
        let target = binding("c1", "p1");
        ensure(dir.path(), target).expect("mints");
        std::fs::write(key_path(dir.path(), target), "not hex at all").expect("corrupt it");
        let failure = load(dir.path(), target).expect_err("a corrupt key is an error");
        assert!(failure.contains("not hex"), "{failure}");
    }

    /// 0600 is the whole trust model: the key is admissible because only this
    /// workspace's owner can read it.
    #[cfg(unix)]
    #[test]
    fn a_minted_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = workspace();
        let target = binding("c1", "p1");
        ensure(dir.path(), target).expect("mints");
        let mode = std::fs::metadata(key_path(dir.path(), target))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }
}
