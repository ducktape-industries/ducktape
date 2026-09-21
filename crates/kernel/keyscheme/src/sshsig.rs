//! OpenSSH `SSHSIG` — what `ssh-keygen -Y sign` produces and `git push
//! --signed` (gpg.format=ssh) attaches to a push certificate.
//!
//! `PROTOCOL.sshsig`, ed25519 only:
//!
//! ```text
//! blob        = "SSHSIG" ‖ u32 1 ‖ string pubkey ‖ string namespace
//!               ‖ string reserved ‖ string hash_alg ‖ string signature
//! signed data = "SSHSIG" ‖ string namespace ‖ string reserved
//!               ‖ string hash_alg ‖ string H(message)
//! pubkey      = string "ssh-ed25519" ‖ string key(32)      (ssh wire)
//! signature   = string "ssh-ed25519" ‖ string sig(64)      (ssh wire)
//! ```
//!
//! The signature is RAW ed25519 over the signed data — not commonware's
//! namespaced envelope — so this module verifies with `ed25519-dalek`. The
//! namespace is the domain separator (`git` for push certificates, `ducktape`
//! for our own frames/consents signed by an SSH key); a signature minted under
//! one never verifies under another.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::Verifier as _;
use sha2::Digest as _;

/// the blob's leading magic — how the `Ed25519` scheme arm tells an SSHSIG
/// proof from a 64-byte commonware signature.
pub const SSHSIG_MAGIC: &[u8] = b"SSHSIG";
/// the namespace git signs push certificates under.
pub const GIT_SSH_NS: &str = "git";
/// the namespace an SSH key signs OUR bytes under (`ssh-keygen -Y sign -n
/// ducktape`): a frame preimage, an `AddKey` consent.
pub const DUCKTAPE_SSH_NS: &str = "ducktape";

const SSH_ED25519: &[u8] = b"ssh-ed25519";
const VERSION: u32 = 1;
const ARMOR_BEGIN: &str = "-----BEGIN SSH SIGNATURE-----";
const ARMOR_END: &str = "-----END SSH SIGNATURE-----";

/// the message hash an SSHSIG names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshHash {
    Sha256,
    Sha512,
}

impl SshHash {
    fn name(self) -> &'static [u8] {
        match self {
            SshHash::Sha256 => b"sha256",
            SshHash::Sha512 => b"sha512",
        }
    }

    fn digest(self, message: &[u8]) -> Vec<u8> {
        match self {
            SshHash::Sha256 => sha2::Sha256::digest(message).to_vec(),
            SshHash::Sha512 => sha2::Sha512::digest(message).to_vec(),
        }
    }
}

/// one parsed ed25519 SSHSIG.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshSig {
    /// the signer — SSHSIG carries its own public key.
    pub pubkey: [u8; 32],
    pub namespace: String,
    pub hash: SshHash,
    pub signature: [u8; 64],
}

/// parse a binary SSHSIG blob. Anything but version 1 / `ssh-ed25519` /
/// `sha256|sha512` is refused by name.
pub fn parse(blob: &[u8]) -> Result<SshSig, String> {
    let mut buf = blob
        .strip_prefix(SSHSIG_MAGIC)
        .ok_or("not an SSHSIG blob")?;
    let version = take_u32(&mut buf).ok_or("SSHSIG: truncated version")?;
    if version != VERSION {
        return Err(format!("SSHSIG: unsupported version {version}"));
    }
    let pubkey_wire = take_string(&mut buf).ok_or("SSHSIG: truncated public key")?;
    let namespace = take_string(&mut buf).ok_or("SSHSIG: truncated namespace")?;
    let _reserved = take_string(&mut buf).ok_or("SSHSIG: truncated reserved")?;
    let hash_name = take_string(&mut buf).ok_or("SSHSIG: truncated hash algorithm")?;
    let signature_wire = take_string(&mut buf).ok_or("SSHSIG: truncated signature")?;
    if !buf.is_empty() {
        return Err("SSHSIG: trailing bytes".into());
    }
    let hash = match hash_name {
        b"sha256" => SshHash::Sha256,
        b"sha512" => SshHash::Sha512,
        other => {
            return Err(format!(
                "SSHSIG: unsupported hash {:?}",
                String::from_utf8_lossy(other)
            ));
        }
    };
    let pubkey = ed25519_wire(pubkey_wire, 32).ok_or("SSHSIG: the key is not ssh-ed25519")?;
    let signature =
        ed25519_wire(signature_wire, 64).ok_or("SSHSIG: the signature is not ssh-ed25519")?;
    let namespace = String::from_utf8(namespace.to_vec())
        .map_err(|_| "SSHSIG: namespace is not utf-8".to_string())?;
    Ok(SshSig {
        pubkey: pubkey.try_into().expect("checked length"),
        namespace,
        hash,
        signature: signature.try_into().expect("checked length"),
    })
}

/// does `blob` prove that `pubkey` signed `message` under `namespace`? The
/// blob's embedded key must BE `pubkey` (a valid signature by someone else
/// is a categorical no), the namespace must match, and the raw ed25519
/// signature must verify over the reconstructed signed data.
pub fn verify_ed25519(pubkey: &[u8], namespace: &str, message: &[u8], blob: &[u8]) -> bool {
    let Ok(sig) = parse(blob) else {
        return false;
    };
    let same_signer = sig.pubkey.as_slice() == pubkey;
    let same_namespace = sig.namespace == namespace;
    if !same_signer || !same_namespace {
        return false;
    }
    let Ok(key) = ed25519_dalek::VerifyingKey::from_bytes(&sig.pubkey) else {
        return false;
    };
    let signed = signed_data(namespace, sig.hash, message);
    key.verify(
        &signed,
        &ed25519_dalek::Signature::from_bytes(&sig.signature),
    )
    .is_ok()
}

/// the bytes an SSH key is asked to sign (`ssh-keygen -Y sign -n ducktape`)
/// for `(ns, preimage)`: commonware's namespaced preimage, so an SSH key's
/// proof and a device key's proof bind the same bytes.
pub fn ssh_message(ns: &[u8], preimage: &[u8]) -> Vec<u8> {
    commonware_utils::union_unique(ns, preimage)
}

/// the bytes an SSHSIG signature is over.
pub(crate) fn signed_data(namespace: &str, hash: SshHash, message: &[u8]) -> Vec<u8> {
    let mut out = SSHSIG_MAGIC.to_vec();
    put_string(&mut out, namespace.as_bytes());
    put_string(&mut out, b"");
    put_string(&mut out, hash.name());
    put_string(&mut out, &hash.digest(message));
    out
}

/// assemble a blob from its parts — the testkit's signer (production only
/// ever verifies what `ssh-keygen` wrote).
#[cfg(any(test, feature = "testkit"))]
pub(crate) fn encode(sig: &SshSig) -> Vec<u8> {
    let mut out = SSHSIG_MAGIC.to_vec();
    out.extend_from_slice(&VERSION.to_be_bytes());
    let mut pubkey_wire = Vec::new();
    put_string(&mut pubkey_wire, SSH_ED25519);
    put_string(&mut pubkey_wire, &sig.pubkey);
    put_string(&mut out, &pubkey_wire);
    put_string(&mut out, sig.namespace.as_bytes());
    put_string(&mut out, b"");
    put_string(&mut out, sig.hash.name());
    let mut signature_wire = Vec::new();
    put_string(&mut signature_wire, SSH_ED25519);
    put_string(&mut signature_wire, &sig.signature);
    put_string(&mut out, &signature_wire);
    out
}

/// the armored form `ssh-keygen` writes and git carries: BEGIN line, base64
/// in 70-column rows, END line.
pub fn armor(blob: &[u8]) -> String {
    let body = B64.encode(blob);
    let mut out = String::from(ARMOR_BEGIN);
    out.push('\n');
    for row in body.as_bytes().chunks(70) {
        out.push_str(std::str::from_utf8(row).expect("base64 is ascii"));
        out.push('\n');
    }
    out.push_str(ARMOR_END);
    out.push('\n');
    out
}

/// the blob out of its armor; whitespace between the lines is ignored, the
/// BEGIN/END lines are required.
pub fn dearmor(text: &str) -> Result<Vec<u8>, String> {
    let inner = text
        .trim()
        .strip_prefix(ARMOR_BEGIN)
        .and_then(|rest| rest.strip_suffix(ARMOR_END))
        .ok_or("not an armored SSH signature")?;
    let body: String = inner.split_whitespace().collect();
    B64.decode(body)
        .map_err(|e| format!("SSH signature armor is not base64: {e}"))
}

/// the 32 raw key bytes of an `authorized_keys`-style line
/// (`ssh-ed25519 <base64> [comment]`) — what `id_ed25519.pub` holds.
pub fn authorized_key(line: &str) -> Result<Vec<u8>, String> {
    let mut fields = line.split_whitespace();
    let (Some(kind), Some(body)) = (fields.next(), fields.next()) else {
        return Err("not an OpenSSH public key line (`ssh-ed25519 <base64> [comment]`)".into());
    };
    if kind.as_bytes() != SSH_ED25519 {
        return Err(format!("only ssh-ed25519 keys can be members, not {kind}"));
    }
    let wire = B64
        .decode(body)
        .map_err(|e| format!("the public key is not base64: {e}"))?;
    ed25519_wire(&wire, 32)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| "the public key body is not an ssh-ed25519 key".to_string())
}

/// `string "ssh-ed25519" ‖ string bytes` → the bytes, when they are `len` long.
fn ed25519_wire(mut wire: &[u8], len: usize) -> Option<&[u8]> {
    let kind = take_string(&mut wire)?;
    let bytes = take_string(&mut wire)?;
    let is_ed25519 = kind == SSH_ED25519 && bytes.len() == len && wire.is_empty();
    is_ed25519.then_some(bytes)
}

fn take_u32(buf: &mut &[u8]) -> Option<u32> {
    let (head, rest) = buf.split_first_chunk::<4>()?;
    *buf = rest;
    Some(u32::from_be_bytes(*head))
}

fn take_string<'a>(buf: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = take_u32(buf)? as usize;
    let (head, rest) = buf.split_at_checked(len)?;
    *buf = rest;
    Some(head)
}

fn put_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{ssh_key, ssh_pubkey, sshsig};

    /// a REAL `ssh-keygen -Y sign -n git` over a push-certificate body, by a
    /// throwaway ed25519 key (`ssh-keygen -Y check-novalidate` says "Good").
    const PUB_LINE: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICY4ULdNq97xvPqcWgXa8ip3sUQnFC0KjK63Gnc7f9oh pusher@test";
    const CERT: &str = "certificate version 0.1\npusher key::ssh-ed25519 AAAA 1756332000 +0000\npushee http://127.0.0.1:8844/forge/lab\nnonce chain-a/lab\n\n0000000000000000000000000000000000000000 ab5b1f3d5b7e3e0e0d33e2c6d1f6c2a7d3a7f1e2 refs/heads/main\n";
    const ARMORED: &str = "-----BEGIN SSH SIGNATURE-----\n\
U1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAgJjhQt02r3vG8+pxaBdryKnexRC\n\
cULQqMrrcadzt/2iEAAAADZ2l0AAAAAAAAAAZzaGE1MTIAAABTAAAAC3NzaC1lZDI1NTE5\n\
AAAAQAkqyuC4rshUkBgUVsgAqGxBltLKRLcwdq5LAQn+2lCUmiUJWTsYTykmuaNO+cntB2\n\
ZYBzkWoVNWmNV5YTCuZwE=\n\
-----END SSH SIGNATURE-----\n";

    #[test]
    fn a_real_ssh_keygen_signature_verifies_under_its_namespace_only() {
        let pubkey = authorized_key(PUB_LINE).unwrap();
        assert_eq!(pubkey.len(), 32);
        let blob = dearmor(ARMORED).unwrap();
        let parsed = parse(&blob).unwrap();
        assert_eq!(
            parsed.pubkey.as_slice(),
            pubkey.as_slice(),
            "the blob names its signer"
        );
        assert_eq!(parsed.namespace, GIT_SSH_NS);
        assert_eq!(parsed.hash, SshHash::Sha512);
        assert!(verify_ed25519(&pubkey, GIT_SSH_NS, CERT.as_bytes(), &blob));
        assert!(
            !verify_ed25519(&pubkey, DUCKTAPE_SSH_NS, CERT.as_bytes(), &blob),
            "namespace"
        );
        assert!(
            !verify_ed25519(&pubkey, GIT_SSH_NS, CERT.trim_end().as_bytes(), &blob),
            "message"
        );
        let other = ssh_pubkey(&ssh_key(2));
        assert!(
            !verify_ed25519(&other, GIT_SSH_NS, CERT.as_bytes(), &blob),
            "signer"
        );
        let mut tampered = blob.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(!verify_ed25519(
            &pubkey,
            GIT_SSH_NS,
            CERT.as_bytes(),
            &tampered
        ));
        // re-encoding round-trips byte for byte, and armor is ssh-keygen's.
        assert_eq!(encode(&parsed), blob);
        assert_eq!(dearmor(&armor(&blob)).unwrap(), blob);
    }

    #[test]
    fn the_testkit_signer_matches_ssh_keygen() {
        let sk = ssh_key(7);
        let pubkey = ssh_pubkey(&sk);
        let blob = sshsig(&sk, DUCKTAPE_SSH_NS, b"a frame preimage");
        assert!(verify_ed25519(
            &pubkey,
            DUCKTAPE_SSH_NS,
            b"a frame preimage",
            &blob
        ));
        assert!(!verify_ed25519(
            &pubkey,
            GIT_SSH_NS,
            b"a frame preimage",
            &blob
        ));
        assert!(!verify_ed25519(&pubkey, DUCKTAPE_SSH_NS, b"another", &blob));
        assert!(parse(&blob).unwrap().hash == SshHash::Sha512);
    }

    #[test]
    fn malformed_blobs_and_key_lines_are_refused_by_name() {
        assert!(parse(b"SSHSIG").unwrap_err().contains("version"));
        assert!(parse(b"nope").unwrap_err().contains("not an SSHSIG"));
        let mut v2 = dearmor(ARMORED).unwrap();
        v2[9] = 2;
        assert!(parse(&v2).unwrap_err().contains("version 2"));
        assert!(dearmor("garbage").is_err());
        assert!(
            authorized_key("ssh-rsa AAAAB3 x")
                .unwrap_err()
                .contains("ssh-rsa")
        );
        assert!(authorized_key("ssh-ed25519 !!! x").is_err());
        assert!(authorized_key("").is_err());
    }
}
