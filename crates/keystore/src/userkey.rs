//! the encrypted `user.key` codec: argon2id-derived KEK + XChaCha20-Poly1305 at rest,
//! and the BIP39 mnemonic <-> 32-byte-seed encoding used to reveal/restore
//! the user's ed25519 identity. Mirrors `config::load_or_generate_identity`'s
//! file discipline (0600 and `create_new`) for the persisted file.
//!
//! ## the custody model
//! - **the mnemonic IS the identity.** [`mnemonic_of_seed`] /
//!   [`seed_of_mnemonic`] use `Mnemonic::from_entropy` / `to_entropy` only —
//!   NEVER `to_seed`'s BIP39 PBKDF2 stretch — so the 24 words are a
//!   checksum-carrying, identity-preserving encoding of the raw 32-byte
//!   ed25519 seed. Restore-by-mnemonic reproduces byte-identical keys.
//! - **the password is local-encryption-only.** It never enters key
//!   derivation for the identity itself; it only wraps the seed at rest
//!   (argon2id -> XChaCha20-Poly1305 KEK), so a forgotten password is
//!   recoverable via the mnemonic (restore, then set a new password).
//!
//! ## encrypted line format
//! `ducktape-user-key-v1:<base64(salt(16) || argon2-params(12) ||
//! nonce(24) || pubkey(32) || ciphertext(32+16))>`
//! - `argon2-params` = `m_kib, t, p` as little-endian `u32`s (default
//!   65536 KiB / 3 / 1), encoded explicitly so they can be raised later
//!   without breaking old files.
//! - `pubkey` rides in the clear — it's public data, and it lets `status`
//!   report identity while locked, without a password.
//! - the format prefix ([`USER_KEY_ENCRYPTED_PREFIX`]) is bound as the AEAD
//!   associated data, so ciphertext minted under one format tag can never be
//!   replayed as another.
//!
//! this module's CLI wiring (`user key init/restore/unlock/reveal/status`)
//! lives in `userkey_cli.rs`.

use std::path::Path;

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine as _;
use chacha20poly1305::aead::{Aead as _, Payload};
use chacha20poly1305::{KeyInit as _, XChaCha20Poly1305, XNonce};
use commonware_codec::DecodeExt as _;
use commonware_cryptography::{Signer as _, ed25519};
use rand::RngCore as _;
use zeroize::Zeroizing;

/// the encrypted line prefix — also the literal AEAD associated data bound to the
/// ciphertext (see module docs).
pub const USER_KEY_ENCRYPTED_PREFIX: &str = "ducktape-user-key-v1:";

/// the error `open_user_key` returns for any AEAD failure — a bad password
/// and a tampered/corrupted file are cryptographically indistinguishable, so
/// this message deliberately doesn't claim to know which.
const WRONG_PASSWORD_ERR: &str = "corrupt or wrong password";

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const PARAMS_LEN: usize = 12; // m_kib, t, p as LE u32
const SEED_LEN: usize = 32;
const PUBKEY_LEN: usize = 32;
const TAG_LEN: usize = 16;
const CIPHERTEXT_LEN: usize = SEED_LEN + TAG_LEN;
const BLOB_LEN: usize = SALT_LEN + PARAMS_LEN + NONCE_LEN + PUBKEY_LEN + CIPHERTEXT_LEN;

/// argon2id defaults for freshly-sealed files; encoded explicitly per-file
/// (not hardcoded at open time) so a future default bump doesn't strand
/// files sealed under the old ones.
const DEFAULT_M_KIB: u32 = 65536;
const DEFAULT_T_COST: u32 = 3;
const DEFAULT_P_COST: u32 = 1;

/// sanity bounds on the params a file may DECLARE. argon2 0.5's
/// `Params::new` only enforces minimums, so without these a
/// corrupted/tampered params field could request terabytes of memory or
/// billions of passes BEFORE the AEAD check — a DoS instead of the promised
/// clean error. wide enough for any plausible future default bump (up to
/// 1 GiB / 64 passes / 8 lanes), narrow enough that the worst accepted
/// derivation stays interactive-scale. checked at parse time (the file's
/// declared params) AND at seal time (so a future `DEFAULT_*` bump beyond
/// them fails loudly in tests instead of minting unopenable files).
const M_KIB_RANGE: std::ops::RangeInclusive<u32> = 8_192..=1_048_576;
const T_COST_RANGE: std::ops::RangeInclusive<u32> = 1..=64;
const P_COST_RANGE: std::ops::RangeInclusive<u32> = 1..=8;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// a parsed encrypted blob. `pubkey` rides in the clear (public data — lets
/// `status` report identity without a password); everything needed to
/// decrypt is held privately, exercised only through [`open_user_key`] (one
/// parse path, not two).
#[derive(Debug)]
pub struct EncryptedUserKey {
    pub pubkey: Vec<u8>,
    salt: [u8; SALT_LEN],
    m_kib: u32,
    t_cost: u32,
    p_cost: u32,
    nonce: [u8; NONCE_LEN],
    ciphertext: Vec<u8>,
}

// ============================================================================
// crypto primitives
// ============================================================================

/// reject argon2 params outside [`M_KIB_RANGE`]/[`T_COST_RANGE`]/
/// [`P_COST_RANGE`] — see those constants for why this must happen before
/// any derivation is attempted.
fn check_params(m_kib: u32, t_cost: u32, p_cost: u32) -> Result<(), String> {
    if !M_KIB_RANGE.contains(&m_kib)
        || !T_COST_RANGE.contains(&t_cost)
        || !P_COST_RANGE.contains(&p_cost)
    {
        return Err(format!(
            "argon2 params out of range: m={m_kib} KiB, t={t_cost}, p={p_cost} \
             (accepted: m in {}..={}, t in {}..={}, p in {}..={})",
            M_KIB_RANGE.start(),
            M_KIB_RANGE.end(),
            T_COST_RANGE.start(),
            T_COST_RANGE.end(),
            P_COST_RANGE.start(),
            P_COST_RANGE.end(),
        ));
    }
    Ok(())
}

/// argon2id(password, salt) -> a 32-byte KEK, zeroized on drop.
fn derive_kek(
    password: &str,
    salt: &[u8],
    m_kib: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<Zeroizing<[u8; 32]>, String> {
    let params = Params::new(m_kib, t_cost, p_cost, Some(32))
        .map_err(|e| format!("invalid argon2 params: {e}"))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut kek = Zeroizing::new([0u8; 32]);
    argon2
        .hash_password_into(password.as_bytes(), salt, kek.as_mut_slice())
        .map_err(|e| format!("argon2 derivation failed: {e}"))?;
    Ok(kek)
}

/// parse an encrypted line into its fields, without touching the password/crypto.
fn parse_encrypted(line: &str) -> Result<EncryptedUserKey, String> {
    let body = line
        .strip_prefix(USER_KEY_ENCRYPTED_PREFIX)
        .ok_or_else(|| "not an encrypted user-key line".to_string())?;
    let blob = B64
        .decode(body.trim_end())
        .map_err(|e| format!("encrypted line is not valid base64: {e}"))?;
    if blob.len() != BLOB_LEN {
        return Err(format!(
            "encrypted blob has wrong length: {} (want {BLOB_LEN})",
            blob.len()
        ));
    }

    let mut off = 0;
    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&blob[off..off + SALT_LEN]);
    off += SALT_LEN;
    let m_kib = u32::from_le_bytes(blob[off..off + 4].try_into().expect("4 bytes"));
    off += 4;
    let t_cost = u32::from_le_bytes(blob[off..off + 4].try_into().expect("4 bytes"));
    off += 4;
    let p_cost = u32::from_le_bytes(blob[off..off + 4].try_into().expect("4 bytes"));
    off += 4;
    // bound the DECLARED params before anything downstream can act on them
    // (a parse failure, same class as a malformed length — never a
    // derivation attempt).
    check_params(m_kib, t_cost, p_cost)?;
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&blob[off..off + NONCE_LEN]);
    off += NONCE_LEN;
    let pubkey = blob[off..off + PUBKEY_LEN].to_vec();
    off += PUBKEY_LEN;
    let ciphertext = blob[off..off + CIPHERTEXT_LEN].to_vec();

    Ok(EncryptedUserKey {
        pubkey,
        salt,
        m_kib,
        t_cost,
        p_cost,
        nonce,
        ciphertext,
    })
}

/// decrypt a parsed encrypted blob's seed under `password`, binding `aad` (the
/// declared version prefix) as associated data. split out from
/// [`open_user_key`] so the AAD-binding property (a blob sealed under one
/// prefix must not open under another) is directly testable.
///
/// zeroization boundary: every buffer THIS module holds the seed in (the
/// AEAD's plaintext `Vec` and the returned array) is `Zeroizing` — scrubbed
/// on drop. once the caller feeds the bytes into
/// `ed25519::PrivateKey::decode`, the copy inside commonware's type is
/// theirs; its internal hygiene is out of our hands.
fn decrypt_seed(
    enc: &EncryptedUserKey,
    password: &str,
    aad: &[u8],
) -> Result<Zeroizing<[u8; 32]>, String> {
    let kek = derive_kek(password, &enc.salt, enc.m_kib, enc.t_cost, enc.p_cost)?;
    let cipher = XChaCha20Poly1305::new_from_slice(kek.as_slice())
        .map_err(|e| format!("bad KEK length: {e}"))?;
    let nonce = XNonce::from_slice(&enc.nonce);
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                nonce,
                Payload {
                    msg: &enc.ciphertext,
                    aad,
                },
            )
            .map_err(|_| WRONG_PASSWORD_ERR.to_string())?,
    );
    if plaintext.len() != SEED_LEN {
        return Err(WRONG_PASSWORD_ERR.to_string());
    }
    let mut seed = Zeroizing::new([0u8; SEED_LEN]);
    seed.copy_from_slice(&plaintext);
    Ok(seed)
}

// ============================================================================
// public API
// ============================================================================

/// the deterministic sealing core: encrypt `seed` under `password` with an
/// EXPLICIT salt/nonce/params. [`seal_user_key`] is the production entry
/// (random salt/nonce, `DEFAULT_*` params); this core exists so tests can
/// pin the exact wire bytes (golden vector, field offsets) independently of
/// the encoder's RNG.
fn seal_with(
    seed: &[u8; 32],
    password: &str,
    salt: &[u8; SALT_LEN],
    nonce_bytes: &[u8; NONCE_LEN],
    m_kib: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<String, String> {
    // the same bounds parse_encrypted enforces — sealing outside them would mint a
    // file that can never be opened (a future DEFAULT_* bump beyond the
    // ranges fails loudly here, in tests, instead of in the field).
    check_params(m_kib, t_cost, p_cost)?;
    let signer = ed25519::PrivateKey::decode(seed.as_slice())
        .map_err(|e| format!("seed is not a valid ed25519 secret: {e}"))?;
    let pubkey = signer.public_key().as_ref().to_vec();

    let kek = derive_kek(password, salt, m_kib, t_cost, p_cost)?;
    let cipher = XChaCha20Poly1305::new_from_slice(kek.as_slice())
        .map_err(|e| format!("bad KEK length: {e}"))?;
    let nonce = XNonce::from_slice(nonce_bytes);
    let ciphertext = cipher
        .encrypt(
            nonce,
            Payload {
                msg: seed.as_slice(),
                aad: USER_KEY_ENCRYPTED_PREFIX.as_bytes(),
            },
        )
        .map_err(|e| format!("encryption failed: {e}"))?;

    let mut blob = Vec::with_capacity(BLOB_LEN);
    blob.extend_from_slice(salt);
    blob.extend_from_slice(&m_kib.to_le_bytes());
    blob.extend_from_slice(&t_cost.to_le_bytes());
    blob.extend_from_slice(&p_cost.to_le_bytes());
    blob.extend_from_slice(nonce_bytes);
    blob.extend_from_slice(&pubkey);
    blob.extend_from_slice(&ciphertext);
    debug_assert_eq!(blob.len(), BLOB_LEN);

    Ok(format!("{USER_KEY_ENCRYPTED_PREFIX}{}", B64.encode(blob)))
}

/// encrypt `seed` under `password` (argon2id defaults + fresh random
/// salt/nonce) and return the full encrypted line, ready to write to disk.
pub fn seal_user_key(seed: &[u8; 32], password: &str) -> Result<String, String> {
    let mut salt = [0u8; SALT_LEN];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    seal_with(
        seed,
        password,
        &salt,
        &nonce_bytes,
        DEFAULT_M_KIB,
        DEFAULT_T_COST,
        DEFAULT_P_COST,
    )
}

/// decrypt an encrypted line under `password`, returning the seed as a ready-to-use
/// signer. any AEAD failure (wrong password OR a tampered/corrupted file —
/// the two are indistinguishable) returns the exact string
/// `"corrupt or wrong password"`, as does a clear-pubkey field that doesn't
/// match the decrypted seed (the pubkey rides OUTSIDE the AEAD — only the
/// prefix is AAD — so a swapped pubkey wouldn't otherwise be caught, and a
/// caller trusting the parsed `EncryptedUserKey::pubkey` could be lied to).
pub fn open_user_key(line: &str, password: &str) -> Result<ed25519::PrivateKey, String> {
    let enc = parse_encrypted(line)?;
    let seed = decrypt_seed(&enc, password, USER_KEY_ENCRYPTED_PREFIX.as_bytes())?;
    let key = ed25519::PrivateKey::decode(seed.as_slice())
        .map_err(|e| format!("decrypted seed is not a valid ed25519 secret: {e}"))?;
    if key.public_key().as_ref() != enc.pubkey.as_slice() {
        return Err(WRONG_PASSWORD_ERR.to_string());
    }
    Ok(key)
}

/// `path`'s trimmed contents, refusing an empty file.
fn read_key_line(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {path:?}: {e}"))?;
    let line = text.trim();
    if line.is_empty() {
        return Err(format!("{path:?} is empty"));
    }
    Ok(line.to_string())
}

/// Read and validate an encrypted v1 key file.
pub fn read_user_key_file(path: &Path) -> Result<EncryptedUserKey, String> {
    let line = read_key_line(path)?;
    parse_encrypted(&line).map_err(|error| format!("{path:?}: {error}"))
}

/// What a key file is to a reader WITHOUT a password. Anything that is not a
/// parseable encrypted v1 line is [`Unreadable`](Self::Unreadable) — a
/// plaintext seed, a truncated write, and the right prefix over a corrupt
/// payload alike. Calling the last of those "encrypted" because its first
/// bytes match is a promise the unlock cannot keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyFileState {
    Absent,
    Encrypted,
    Unreadable,
}

impl KeyFileState {
    /// The lowercase token a listing or a settings row shows. One vocabulary,
    /// so the wallet list and the app's key-state rows cannot drift apart.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Encrypted => "encrypted",
            Self::Unreadable => "unreadable",
        }
    }
}

/// Classify `path` without a password, through the same parse every unlock
/// runs. Only a MISSING file is [`Absent`](KeyFileState::Absent): one that
/// exists but will not read is `Unreadable`, never "no key here".
pub fn key_file_state(path: &Path) -> KeyFileState {
    let missing = matches!(
        std::fs::metadata(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    );
    if missing {
        return KeyFileState::Absent;
    }
    match read_user_key_file(path) {
        Ok(_) => KeyFileState::Encrypted,
        Err(_) => KeyFileState::Unreadable,
    }
}

/// the 24-word BIP39 mnemonic encoding `seed`'s raw bytes as entropy (with
/// its checksum) — NEVER the BIP39 `to_seed` PBKDF2 stretch, so this is a
/// lossless, identity-preserving encoding: [`seed_of_mnemonic`] recovers the
/// exact same 32 bytes.
pub fn mnemonic_of_seed(seed: &[u8; 32]) -> String {
    bip39::Mnemonic::from_entropy(seed.as_slice())
        .expect("32 bytes is always valid bip39 entropy (256 bits)")
        .to_string()
}

/// the inverse of [`mnemonic_of_seed`]: validates the BIP39 checksum and
/// recovers the original 32-byte seed. a mangled word count, an unknown
/// word, or a flipped word (failing the checksum) all error.
pub fn seed_of_mnemonic(words: &str) -> Result<[u8; 32], String> {
    let mnemonic =
        bip39::Mnemonic::parse(words.trim()).map_err(|e| format!("invalid mnemonic: {e}"))?;
    let entropy = mnemonic.to_entropy();
    let len = entropy.len();
    entropy
        .try_into()
        .map_err(|_| format!("mnemonic encodes {len} bytes of entropy, want 32"))
}

/// Write a fresh encrypted line to `path`. Born 0600 (no
/// write-then-chmod window), and `create_new` so a concurrent init can't
/// clobber an existing identity.
pub fn write_user_key_new(path: &Path, line: &str) -> Result<(), String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {dir:?}: {e}"))?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| format!("create {path:?}: {e}"))?;
    if let Err(e) = std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes()) {
        // a partial file would shadow every future read with a parse error;
        // remove it so the next attempt starts clean.
        let _ = std::fs::remove_file(path);
        return Err(format!("write {path:?}: {e}"));
    }
    Ok(())
}

// ============================================================================
// the ceremonies — mint, restore, open
// ============================================================================
//
// A ceremony is the whole ordered act, not a primitive: mint = generate +
// seal + write + PROVE IT REOPENS, restore = validate the checksum + re-seal.
// They live here rather than in either caller because both a CLI verb and a
// desktop app perform them, and a second implementation of "mint an identity"
// is the one duplication whose divergence is unrecoverable — the failure is a
// key whose 24 words do not open it, discovered weeks later.
//
// They take a `password: &str` rather than a stdin handle: reading a secret is
// the CALLER's business (a terminal line, a text field), and threading a
// `BufRead` through here is what kept the app on the outside of a pipe.

/// the password floor for newly encrypted keys, enforced before any file is
/// touched. counts scalar chars, not bytes, so a multi-byte-but-short
/// password isn't laundered past the floor.
pub const MIN_PASSWORD_LEN: usize = 8;

pub fn check_password_len(password: &str) -> Result<(), String> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        ));
    }
    Ok(())
}

/// Open the encrypted key AT `path` under `password` — the signing seat every
/// caller takes before it can sign anything.
pub fn open_user_key_at(path: &Path, password: &str) -> Result<ed25519::PrivateKey, String> {
    open_user_key(&read_key_line(path)?, password)
}

/// Generate a fresh seed, seal it under `password`, write it to `path`
/// (refusing to overwrite), and hand back the 24 words AND the signer.
///
/// The signer comes back so a caller that mints does not have to re-read the
/// file with a password it would have to ask for a SECOND time.
pub fn mint_user_key(path: &Path, password: &str) -> Result<(String, ed25519::PrivateKey), String> {
    check_password_len(password)?;

    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    let words = mnemonic_of_seed(&seed);
    let line = seal_user_key(&seed, password)?;
    write_user_key_new(path, &line)?;

    let key = ed25519::PrivateKey::decode(seed.as_slice()).expect("32 random bytes decode");
    verify_the_key_reopens(path, password, &key)?;
    Ok((words, key))
}

/// Re-seal an identity from its 24 words at `path`, returning the signer.
pub fn restore_user_key_at(
    path: &Path,
    mnemonic: &str,
    password: &str,
) -> Result<ed25519::PrivateKey, String> {
    check_password_len(password)?;

    let seed = seed_of_mnemonic(mnemonic)?;
    let line = seal_user_key(&seed, password)?;
    write_user_key_new(path, &line)?;

    let key = ed25519::PrivateKey::decode(seed.as_slice())
        .map_err(|e| format!("restored seed is not a valid ed25519 secret: {e}"))?;
    verify_the_key_reopens(path, password, &key)?;
    Ok(key)
}

/// Read the key back and open it, before telling anyone it exists.
///
/// The failure this prevents is SILENT and TOTAL: the operator is shown 24
/// words, writes them down, and the file those words are supposed to unlock
/// cannot be opened by anything, ever. Nothing later in the flow would notice —
/// the mnemonic is correct, the file is present and well-formed, and the first
/// symptom is a wrong-password error at some unrelated moment weeks later.
///
/// It is not hypothetical. Sealing runs a 64 MiB argon2id buffer through the
/// host's memory, and on the machine this was written on a byte-identical
/// `seal_with` call produced a DIFFERENT ciphertext roughly 1 run in 240 under
/// parallel load — a silent bit flip on non-ECC memory. A short disk write
/// would land the same way. One extra argon2 pass, once, at the only moment a
/// key is irreplaceable, is the cheapest insurance in this file.
///
/// The unusable file is REMOVED on failure, because leaving it behind is worse
/// than not writing it: `write_user_key_new` refuses to overwrite, so a
/// corrupt key would block the retry that would have fixed it.
fn verify_the_key_reopens(
    path: &Path,
    password: &str,
    expected: &ed25519::PrivateKey,
) -> Result<(), String> {
    let reopened = open_user_key_at(path, password);
    let intact = reopened
        .as_ref()
        .is_ok_and(|key| key.public_key() == expected.public_key());
    if intact {
        return Ok(());
    }
    let _ = std::fs::remove_file(path);
    Err(format!(
        "the key just written to {} does not open with the password that sealed it, so it \
         has been removed rather than left unusable — this is a corrupted write (failing \
         memory or a full/faulty disk), not a wrong password. Run the command again; if it \
         happens twice, the host is at fault.",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_cryptography::ed25519;
    use std::io::Read as _;

    fn read_file(path: &std::path::Path) -> String {
        let mut s = String::new();
        std::fs::File::open(path)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        s
    }

    /// re-splice the argon2 params field of a sealed encrypted line (offsets per
    /// the wire spec: three LE u32s right after the 16-byte salt).
    fn respliced_params(line: &str, m_kib: u32, t_cost: u32, p_cost: u32) -> String {
        let body = line.strip_prefix(USER_KEY_ENCRYPTED_PREFIX).unwrap();
        let mut blob = B64.decode(body).unwrap();
        blob[SALT_LEN..SALT_LEN + 4].copy_from_slice(&m_kib.to_le_bytes());
        blob[SALT_LEN + 4..SALT_LEN + 8].copy_from_slice(&t_cost.to_le_bytes());
        blob[SALT_LEN + 8..SALT_LEN + 12].copy_from_slice(&p_cost.to_le_bytes());
        format!("{USER_KEY_ENCRYPTED_PREFIX}{}", B64.encode(blob))
    }

    /// A key that cannot be reopened is a key the operator has LOST, and they
    /// would be holding 24 correct words while believing otherwise. The check
    /// runs at mint time and refuses rather than reporting success — and it
    /// takes the unusable file with it, because `write_user_key_new` will not
    /// overwrite and a corpse there would block the retry that fixes this.
    #[test]
    fn a_key_that_does_not_reopen_is_refused_and_not_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user.key");

        // a real minted key verifies — the check is not vacuous.
        let (_, key) = mint_user_key(&path, "correct horse battery").expect("a good seal verifies");
        assert!(verify_the_key_reopens(&path, "correct horse battery", &key).is_ok());

        // now corrupt the sealed line the way a bit flip would, and the same
        // check must refuse it and remove it.
        let flipped = {
            let line = std::fs::read_to_string(&path).unwrap();
            let mut bytes = line.trim().as_bytes().to_vec();
            let last = bytes.len() - 1;
            bytes[last] = if bytes[last] == b'A' { b'B' } else { b'A' };
            String::from_utf8(bytes).unwrap()
        };
        std::fs::write(&path, &flipped).unwrap();
        let why = verify_the_key_reopens(&path, "correct horse battery", &key)
            .expect_err("a corrupted key must not pass");
        assert!(
            why.contains("not a wrong password"),
            "it must not send the reader hunting for a typo: {why}"
        );
        assert!(
            !path.exists(),
            "an unusable key file must not be left to block the retry"
        );
    }

    /// The ceremonies round-trip WITHOUT a pipe: mint hands back the words and
    /// a live signer, restore rebuilds the identical identity from those words,
    /// and both refuse a password under the floor before touching a file.
    #[test]
    fn mint_and_restore_round_trip_through_the_library() {
        let dir = tempfile::tempdir().unwrap();
        let minted = dir.path().join("minted.key");
        let (words, key) = mint_user_key(&minted, "password-123").unwrap();
        assert_eq!(words.split_whitespace().count(), 24);

        let restored_path = dir.path().join("restored.key");
        let restored = restore_user_key_at(&restored_path, &words, "another-password").unwrap();
        assert_eq!(restored.public_key(), key.public_key());

        // and it opens under the password it was re-sealed with, not the first.
        let opened = open_user_key_at(&restored_path, "another-password").unwrap();
        assert_eq!(opened.public_key(), key.public_key());
        assert!(open_user_key_at(&restored_path, "password-123").is_err());

        // the floor is enforced before anything is written.
        let short = dir.path().join("short.key");
        assert!(mint_user_key(&short, "pw").is_err());
        assert!(!short.exists());
        assert!(restore_user_key_at(&short, &words, "pw").is_err());
        assert!(!short.exists());
    }

    #[test]
    fn seal_open_round_trip() {
        let seed = [1u8; 32];
        let line = seal_user_key(&seed, "correct horse battery staple").unwrap();
        assert!(line.starts_with(USER_KEY_ENCRYPTED_PREFIX));
        let key = open_user_key(&line, "correct horse battery staple").unwrap();
        let expected = ed25519::PrivateKey::decode(seed.as_slice()).unwrap();
        assert_eq!(key.public_key(), expected.public_key());
    }

    #[test]
    fn open_wrong_password_fails_with_exact_message() {
        let seed = [2u8; 32];
        let line = seal_user_key(&seed, "right-password").unwrap();
        let err = open_user_key(&line, "wrong-password").unwrap_err();
        assert_eq!(err, "corrupt or wrong password");
    }

    #[test]
    fn open_tampered_ciphertext_fails_with_exact_message() {
        let seed = [3u8; 32];
        let line = seal_user_key(&seed, "pw").unwrap();
        let body = line.strip_prefix(USER_KEY_ENCRYPTED_PREFIX).unwrap();
        let mut blob = B64.decode(body).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xff; // flip a byte inside the AEAD tag/ciphertext
        let tampered = format!("{USER_KEY_ENCRYPTED_PREFIX}{}", B64.encode(blob));
        let err = open_user_key(&tampered, "pw").unwrap_err();
        assert_eq!(err, "corrupt or wrong password");
    }

    #[test]
    fn decrypt_seed_with_wrong_aad_fails_with_exact_message() {
        // white-box: the version prefix is bound as AEAD associated data,
        // so a blob sealed under one declared prefix must fail to open
        // under a different one, even with the right password.
        let seed = [4u8; 32];
        let line = seal_user_key(&seed, "pw").unwrap();
        let enc = parse_encrypted(&line).unwrap();
        let err = decrypt_seed(&enc, "pw", b"different-user-key-format:").unwrap_err();
        assert_eq!(err, "corrupt or wrong password");
    }

    #[test]
    fn encrypted_line_parses_with_clear_pubkey() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user.key");
        let seed = [5u8; 32];
        let line = seal_user_key(&seed, "pw").unwrap();
        write_user_key_new(&path, &line).unwrap();

        let expected = ed25519::PrivateKey::decode(seed.as_slice())
            .unwrap()
            .public_key();
        let enc = read_user_key_file(&path).unwrap();
        assert_eq!(enc.pubkey, expected.as_ref().to_vec());
    }

    #[test]
    fn bare_hex_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user.key");
        let seed = [6u8; 32];
        let hex_line = seed.iter().map(|b| format!("{b:02x}")).collect::<String>();
        write_user_key_new(&path, &hex_line).unwrap();
        assert!(read_user_key_file(&path).is_err());
    }

    #[test]
    fn absent_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.key");
        assert!(read_user_key_file(&path).is_err());
    }

    #[test]
    fn junk_content_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user.key");
        write_user_key_new(&path, "not a hex seed and not an encrypted line").unwrap();
        assert!(read_user_key_file(&path).is_err());
    }

    /// The classification every UI shows, in one place: the file that parses
    /// is the ONLY "encrypted" one. A file carrying the right prefix over a
    /// corrupt payload used to read "encrypted" in the app and "unreadable" in
    /// the wallet listing one screen earlier — same file, two answers.
    #[test]
    fn key_file_state_calls_only_a_parseable_file_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user.key");
        assert_eq!(key_file_state(&path), KeyFileState::Absent);

        let line = seal_user_key(&[8u8; 32], "pw").unwrap();
        write_user_key_new(&path, &line).unwrap();
        assert_eq!(key_file_state(&path), KeyFileState::Encrypted);

        std::fs::write(&path, format!("{USER_KEY_ENCRYPTED_PREFIX}not-base64!!")).unwrap();
        assert_eq!(key_file_state(&path), KeyFileState::Unreadable);

        std::fs::write(&path, "plaintext-key").unwrap();
        assert_eq!(key_file_state(&path), KeyFileState::Unreadable);
    }

    #[test]
    fn mnemonic_seed_round_trip_fixed_vector() {
        let seed = [7u8; 32];
        let words = mnemonic_of_seed(&seed);
        assert_eq!(words.split_whitespace().count(), 24);

        let restored = seed_of_mnemonic(&words).unwrap();
        assert_eq!(restored, seed);

        // identity-preserving: the pubkey from the restored bytes, decoded
        // the same way `config::load_or_generate_identity` decodes a raw
        // seed file, matches the original.
        let original_key = ed25519::PrivateKey::decode(seed.as_slice()).unwrap();
        let restored_key = ed25519::PrivateKey::decode(restored.as_slice()).unwrap();
        assert_eq!(original_key.public_key(), restored_key.public_key());
    }

    #[test]
    fn one_word_flipped_mnemonic_fails_checksum() {
        let seed = [8u8; 32];
        let words = mnemonic_of_seed(&seed);
        let mut parts: Vec<&str> = words.split_whitespace().collect();
        let wordlist = bip39::Language::English.word_list();
        let idx0 = bip39::Language::English.find_word(parts[0]).unwrap() as usize;
        let replacement = wordlist[(idx0 + 1) % wordlist.len()];
        parts[0] = replacement;
        let flipped = parts.join(" ");
        assert!(seed_of_mnemonic(&flipped).is_err());
    }

    #[test]
    fn argon2_params_round_trip_through_encoded_line() {
        let seed = [9u8; 32];
        let line = seal_user_key(&seed, "pw").unwrap();
        let enc = parse_encrypted(&line).unwrap();
        assert_eq!(enc.m_kib, 65536);
        assert_eq!(enc.t_cost, 3);
        assert_eq!(enc.p_cost, 1);
    }

    #[test]
    fn write_user_key_new_refuses_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user.key");
        write_user_key_new(&path, "first").unwrap();
        let err = write_user_key_new(&path, "second").unwrap_err();
        assert!(!err.is_empty());
        assert_eq!(read_file(&path).trim(), "first");
    }

    #[test]
    fn parse_rejects_out_of_range_argon2_params() {
        let line = seal_user_key(&[12u8; 32], "pw").unwrap();
        // oversized memory must be rejected at PARSE time, before any argon2
        // attempt — u32::MAX KiB would be a ~4 TiB allocation (instant DoS,
        // or an abort on infallible-alloc failure), not a clean error.
        assert!(parse_encrypted(&respliced_params(&line, u32::MAX, 3, 1)).is_err());
        // below the floor (8 MiB)
        assert!(parse_encrypted(&respliced_params(&line, 8_191, 3, 1)).is_err());
        // zero / oversized passes
        assert!(parse_encrypted(&respliced_params(&line, 65_536, 0, 1)).is_err());
        assert!(parse_encrypted(&respliced_params(&line, 65_536, 65, 1)).is_err());
        // zero / oversized parallelism
        assert!(parse_encrypted(&respliced_params(&line, 65_536, 3, 0)).is_err());
        assert!(parse_encrypted(&respliced_params(&line, 65_536, 3, 9)).is_err());
        // open_user_key surfaces the same rejection (it parses first, so it
        // never reaches derivation). NOTE: keep this AFTER the parse asserts
        // above — if the bounds regress, they fail the test before this line
        // can attempt the 4 TiB derivation.
        assert!(open_user_key(&respliced_params(&line, u32::MAX, 3, 1), "pw").is_err());
    }

    #[test]
    fn parse_accepts_boundary_argon2_params() {
        let line = seal_user_key(&[13u8; 32], "pw").unwrap();
        // both ends of every accepted range parse cleanly (decryption would
        // still fail — splicing params changes the KEK — but that's the
        // AEAD's job, not the parser's).
        assert!(parse_encrypted(&respliced_params(&line, 8_192, 1, 1)).is_ok());
        assert!(parse_encrypted(&respliced_params(&line, 1_048_576, 64, 8)).is_ok());
    }

    #[test]
    fn open_with_swapped_pubkey_fails_with_exact_message() {
        // the clear pubkey field is NOT covered by the AEAD (only the prefix
        // is AAD), so a swapped pubkey must be caught by the post-decrypt
        // cross-check against the decrypted seed's derived pubkey.
        let seed = [10u8; 32];
        let line = seal_user_key(&seed, "pw").unwrap();
        let other = ed25519::PrivateKey::decode([11u8; 32].as_slice())
            .unwrap()
            .public_key();
        let body = line.strip_prefix(USER_KEY_ENCRYPTED_PREFIX).unwrap();
        let mut blob = B64.decode(body).unwrap();
        let start = SALT_LEN + PARAMS_LEN + NONCE_LEN;
        blob[start..start + PUBKEY_LEN].copy_from_slice(other.as_ref());
        let swapped = format!("{USER_KEY_ENCRYPTED_PREFIX}{}", B64.encode(blob));
        let err = open_user_key(&swapped, "pw").unwrap_err();
        assert_eq!(err, "corrupt or wrong password");
    }

    #[test]
    fn golden_vector_pins_wire_format() {
        // the full encrypted line for a fixed seed/password/salt/nonce/params. this
        // pins EVERYTHING at once — argon2id KDF output, XChaCha ciphertext,
        // field order, LE param encoding, base64 alphabet (url-safe, no
        // pad), and the prefix — independently of the encoder. if this test
        // changes to these bytes alter the current encrypted format.
        let seed = [7u8; 32];
        let salt = [0xA1u8; SALT_LEN];
        let nonce = [0xB2u8; NONCE_LEN];
        let line = seal_with(&seed, "correct horse", &salt, &nonce, 65_536, 3, 1).unwrap();
        assert_eq!(
            line,
            "ducktape-user-key-v1:oaGhoaGhoaGhoaGhoaGhoQAAAQADAAAAAQAAALKysrKysrKysrKysrKysrKy\
             srKysrKysupKbGPinFIKvvVQexMuxfmVR3auvr57kkIe6mkURtIs-WYt2Z5Ws4TMsmxhtzpnlBqRi2VR\
             UEmVhQ-Q-_HPf4sbBBeGxf-D0HioE8ivI0Fe"
        );
        // and the pinned line opens with the right password.
        let key = open_user_key(&line, "correct horse").unwrap();
        let expected = ed25519::PrivateKey::decode(seed.as_slice()).unwrap();
        assert_eq!(key.public_key(), expected.public_key());
    }

    #[test]
    fn wire_offsets_match_spec() {
        // slice the decoded payload at the SPEC's byte offsets (literals,
        // not the module's consts) and assert each field lands where
        // parse_encrypted reads it: salt 0..16, params 16..28 (three LE u32),
        // nonce 28..52, pubkey 52..84, ciphertext 84..132.
        let seed = [14u8; 32];
        let salt = [0xC3u8; SALT_LEN];
        let nonce = [0xD4u8; NONCE_LEN];
        let line = seal_with(&seed, "pw", &salt, &nonce, 65_536, 3, 1).unwrap();
        let blob = B64
            .decode(line.strip_prefix(USER_KEY_ENCRYPTED_PREFIX).unwrap())
            .unwrap();
        assert_eq!(blob.len(), 132);
        assert_eq!(&blob[0..16], &salt);
        assert_eq!(u32::from_le_bytes(blob[16..20].try_into().unwrap()), 65_536);
        assert_eq!(u32::from_le_bytes(blob[20..24].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(blob[24..28].try_into().unwrap()), 1);
        assert_eq!(&blob[28..52], &nonce);
        let expected_pub = ed25519::PrivateKey::decode(seed.as_slice())
            .unwrap()
            .public_key();
        assert_eq!(&blob[52..84], expected_pub.as_ref());

        // parse_encrypted reads each field from exactly these offsets.
        let enc = parse_encrypted(&line).unwrap();
        assert_eq!(enc.salt, salt);
        assert_eq!(enc.m_kib, 65_536);
        assert_eq!(enc.t_cost, 3);
        assert_eq!(enc.p_cost, 1);
        assert_eq!(enc.nonce, nonce);
        assert_eq!(enc.pubkey, expected_pub.as_ref().to_vec());
        assert_eq!(enc.ciphertext, blob[84..132].to_vec());
    }

    #[test]
    fn open_honors_params_encoded_in_file() {
        // a file sealed with NON-default params must open: proves the
        // decrypt path derives with the file's encoded params, not
        // DEFAULT_* (a misread would fail decryption here).
        let seed = [15u8; 32];
        let salt = [0xE5u8; SALT_LEN];
        let nonce = [0xF6u8; NONCE_LEN];
        let line = seal_with(&seed, "pw", &salt, &nonce, 8_192, 1, 1).unwrap();
        let key = open_user_key(&line, "pw").unwrap();
        let expected = ed25519::PrivateKey::decode(seed.as_slice()).unwrap();
        assert_eq!(key.public_key(), expected.public_key());
        // and the exact-error contract holds on this path too.
        assert_eq!(
            open_user_key(&line, "wrong").unwrap_err(),
            "corrupt or wrong password"
        );
    }

    #[cfg(unix)]
    #[test]
    fn files_are_written_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user.key");
        write_user_key_new(&path, "content").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
