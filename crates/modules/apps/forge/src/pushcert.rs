//! git's push certificate (`git push --signed`), as consensus reads it.
//!
//! git writes the certificate itself (send-pack.c `generate_push_cert`):
//!
//! ```text
//! certificate version 0.1
//! pusher <signing key ident> <timestamp> <tz>
//! pushee <url>
//! nonce <nonce the server advertised>
//! [push-option …]
//!
//! <old sha1> <new sha1> <refname>
//! …
//! ```
//!
//! and signs exactly that text with the pusher's SSH key (`ssh-keygen -Y sign
//! -n git`). The smart-HTTP bridge puts text + signature on the op as
//! [`PushCert`]; every validator then checks, in this order: the SSHSIG
//! verifies for the key it embeds; the certificate's update list IS the op's
//! (a cert cannot be borrowed to authorize different moves); the nonce is
//! EXACTLY what [`nonce`] makes of this chain and this repo. Freshness is not
//! a concern: a certificate names exact old→new moves, so a replay is a no-op
//! CAS.
//!
//! ## the chain half — #1761 / #1773
//!
//! the bridge ALSO checks the nonce, by exact string equality
//! (`git_http.rs`'s `parse_push_commands`) — but the bridge is not on the
//! trust path: `PushRefs` is an ordinary op, so any account with
//! `/v1/submit/frame` standing can carry a certificate lifted from a
//! DIFFERENT ducktape network straight past the bridge, and every validator
//! must re-verify the same check consensus can compute unaided. that needs
//! consensus to know ITS OWN chain id — the same genesis-config seam
//! `identity`/`gateway`/`runs` bind their chain id through
//! (`sdk::genesis_config`, `ducktape_module_sdk::genesis_chain_id`) — which now
//! reaches an `Odb`-backed module too (`noded::compose`'s `odb_genesis_config`,
//! #1773): forge's [`crate::guest`] shape declares the `chain_id` config key,
//! so every dispatch reads it back and [`signer`] checks the FULL nonce
//! against `nonce(chain_id, repo)` — no half-measure, no chain pinned off the
//! first certificate this forge instance happens to see.

use crate::PushCert;
use crate::oid::Oid;
use crate::tracker_iface::RefUpdate;

const VERSION_LINE: &str = "certificate version 0.1";
const HEADS: &str = "refs/heads/";

/// the parsed certificate: what the pusher committed to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Certificate {
    pub nonce: String,
    pub updates: Vec<RefUpdate>,
}

/// the nonce a node advertises for `repo`, and the one consensus requires a
/// certificate's nonce to equal exactly: `<sha256 of the chain id>/<repo>`.
///
/// THE CHAIN ID IS HASHED, NOT SPELLED, because git validates a nonce before
/// it will sign one and a chain id does not survive that check
/// (`send-pack.c`'s `reject_invalid_nonce`, measured against git 2.43):
///
/// * a byte outside `[A-Za-z0-9]` and `-._/<>=` is refused outright, and every
///   chain id carries a `#` (`<name>#<8 hex>`) — so `git push --signed` died
///   on every real network with "the receiving end asked to sign an invalid
///   nonce", while an e2e cluster named `ducktape-e2e-<pid>-<seq>` signed fine;
/// * a nonce of 256 bytes or more is refused, and a chain NAME has no length
///   bound to keep one under it;
/// * a space would not even be refused — it ends the capability value, so git
///   would sign a silently truncated nonce that consensus then rejects.
///
/// A sha256 answers all three at once: 64 bytes of `[0-9a-f]`, whatever the
/// chain id is. It binds exactly as tightly — a certificate still names one
/// chain and one repo — and both sides reach it through this one function, so
/// the bridge advertises what [`signer`] recomputes.
pub fn nonce(chain_id: &str, repo: &str) -> String {
    use sha2::Digest as _;
    let chain = sha2::Sha256::digest(chain_id.as_bytes());
    format!("{}/{repo}", crate::hex(&chain))
}

/// the widest nonce [`nonce`] can produce — 64 hex of chain digest, a `/`, and
/// a repo name at its own ceiling — against the limit git enforces. A nonce at
/// or past `GIT_NONCE_LEN_LIMIT` is refused unsigned, so this has to hold for
/// every repo name the module admits, not just the ones a test tries.
const GIT_NONCE_LEN_LIMIT: usize = 256;
const _: () = assert!(64 + 1 + crate::MAX_REPO_NAME_LEN < GIT_NONCE_LEN_LIMIT);

/// the certificate text git would write for `updates` under `nonce` — the
/// shape the bridge's and forge's tests sign; git's own carries pusher/pushee
/// lines this parser skips.
pub fn certificate(nonce: &str, updates: &[RefUpdate]) -> Vec<u8> {
    let mut text = format!("{VERSION_LINE}\nnonce {nonce}\n\n");
    for update in updates {
        let hex = |oid: &Option<Vec<u8>>| match oid {
            Some(bytes) => bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            None => "0".repeat(40),
        };
        text.push_str(&format!(
            "{} {} {HEADS}{}\n",
            hex(&update.prev_oid),
            hex(&update.new_oid),
            update.ref_name
        ));
    }
    text.into_bytes()
}

/// parse the signed text. Header lines other than `nonce` are skipped (git
/// adds pusher/pushee/push-option); every update line must name a branch.
pub fn parse(cert: &[u8]) -> Result<Certificate, String> {
    let text = std::str::from_utf8(cert).map_err(|_| "push certificate is not utf-8")?;
    let mut lines = text.lines();
    let versioned = lines.next() == Some(VERSION_LINE);
    if !versioned {
        return Err(format!(
            "push certificate does not start with {VERSION_LINE:?}"
        ));
    }
    let mut nonce = None;
    for line in lines.by_ref() {
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("nonce ") {
            nonce = Some(value.to_string());
        }
    }
    let Some(nonce) = nonce else {
        return Err("push certificate carries no nonce".into());
    };
    let mut updates = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        updates.push(update_line(line)?);
    }
    if updates.is_empty() {
        return Err("push certificate lists no ref updates".into());
    }
    Ok(Certificate { nonce, updates })
}

fn update_line(line: &str) -> Result<RefUpdate, String> {
    let mut parts = line.split(' ');
    let (Some(old), Some(new), Some(refname), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(format!(
            "push certificate line is not `<old> <new> <refname>`: {line:?}"
        ));
    };
    let Some(branch) = refname.strip_prefix(HEADS) else {
        return Err(format!(
            "push certificate moves a non-branch ref {refname:?}"
        ));
    };
    Ok(RefUpdate {
        ref_name: branch.to_string(),
        prev_oid: oid_field(old)?,
        new_oid: oid_field(new)?,
    })
}

/// a 40-hex sha1; the zero oid is "unborn"/"delete" (`None`).
fn oid_field(hex: &str) -> Result<Option<Vec<u8>>, String> {
    let oid =
        Oid::from_hex(hex).map_err(|_| format!("push certificate oid is not sha1 hex: {hex:?}"))?;
    Ok((!oid.is_zero()).then(|| oid.as_bytes().to_vec()))
}

/// the SSH key that signed `cert` for THIS push on THIS chain — the SSHSIG
/// verifies for the key it embeds, the certificate's updates equal `updates`
/// as a set, and its nonce is EXACTLY `nonce(chain_id, repo)`. The 32 raw
/// ed25519 key bytes are a member key's form. a certificate minted for a
/// different chain id, or a different repo, is refused here regardless of
/// whether this forge instance has ever accepted a certified push before.
pub fn signer(
    cert: &PushCert,
    chain_id: &str,
    repo: &str,
    updates: &[RefUpdate],
) -> Result<Vec<u8>, String> {
    let sig = keyscheme::sshsig::parse(&cert.sshsig)?;
    let verified = keyscheme::sshsig::verify_ed25519(
        &sig.pubkey,
        keyscheme::sshsig::GIT_SSH_NS,
        &cert.cert,
        &cert.sshsig,
    );
    if !verified {
        return Err("push certificate signature does not verify".into());
    }
    let certificate = parse(&cert.cert)?;
    let expected = nonce(chain_id, repo);
    if certificate.nonce != expected {
        return Err(format!(
            "push certificate nonce {:?} does not match this network's {expected:?}",
            certificate.nonce
        ));
    }
    let same_moves = sorted(&certificate.updates) == sorted(updates);
    if !same_moves {
        return Err("push certificate does not list this push's ref updates".into());
    }
    Ok(sig.pubkey.to_vec())
}

fn sorted(updates: &[RefUpdate]) -> Vec<&RefUpdate> {
    let mut sorted: Vec<&RefUpdate> = updates.iter().collect();
    sorted.sort_by(|a, b| a.ref_name.cmp(&b.ref_name));
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyscheme::sshsig::{GIT_SSH_NS, dearmor};
    use keyscheme::testkit::{ssh_key, ssh_pubkey, sshsig};

    /// a real `ssh-keygen -Y sign -n git` signature over [`CERT`] — git's own
    /// output, not ours, so the parser and the SSHSIG check are held to what
    /// git actually writes. Its nonce is the one [`nonce`] produces for chain
    /// `chain-a` and repo `lab`.
    const CERT: &str = "certificate version 0.1\npusher key::ssh-ed25519 AAAA 1756332000 +0000\npushee http://127.0.0.1:8844/forge/lab\nnonce 594586ec8545839343436a12f8c85fe8ca603c2a050cb9130f31c78cabcdecd9/lab\n\n0000000000000000000000000000000000000000 ab5b1f3d5b7e3e0e0d33e2c6d1f6c2a7d3a7f1e2 refs/heads/main\n";
    const ARMORED: &str = "-----BEGIN SSH SIGNATURE-----\n\
U1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAgVMCTLbeHvqm1iVUMxR1FbRxp6L\n\
/FUdZm0jg3wdq6tLMAAAADZ2l0AAAAAAAAAAZzaGE1MTIAAABTAAAAC3NzaC1lZDI1NTE5\n\
AAAAQLVICk0pyrHLcnEsEQ7c85Iz5LgrayYKAnmGYodzvOfoIE8zBAYc02eReGWJiWfDBK\n\
6Zl9YeToLeI+xpoeMG4Q4=\n\
-----END SSH SIGNATURE-----\n";

    fn main_birth() -> RefUpdate {
        RefUpdate {
            ref_name: "main".into(),
            prev_oid: None,
            new_oid: Some(
                (0..20)
                    .map(|i| {
                        u8::from_str_radix(
                            &"ab5b1f3d5b7e3e0e0d33e2c6d1f6c2a7d3a7f1e2"[2 * i..2 * i + 2],
                            16,
                        )
                        .unwrap()
                    })
                    .collect(),
            ),
        }
    }

    #[test]
    fn gits_own_certificate_parses_and_names_its_signer() {
        let parsed = parse(CERT.as_bytes()).unwrap();
        assert_eq!(parsed.nonce, nonce("chain-a", "lab"));
        assert_eq!(parsed.updates, vec![main_birth()]);
        let cert = PushCert {
            cert: CERT.as_bytes().to_vec(),
            sshsig: dearmor(ARMORED).unwrap(),
        };
        let key = signer(&cert, "chain-a", "lab", &[main_birth()]).unwrap();
        assert_eq!(
            key,
            keyscheme::sshsig::authorized_key(
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFTAky23h76ptYlVDMUdRW0caei/xVHWZtI4N8HaurSz"
            )
            .unwrap()
        );
        assert!(
            signer(&cert, "chain-a", "other", &[main_birth()])
                .unwrap_err()
                .contains("nonce")
        );
        assert!(
            signer(&cert, "chain-z", "lab", &[main_birth()])
                .unwrap_err()
                .contains("nonce"),
            "a certificate minted for a different chain is refused, not replayed onto this one"
        );
        let mut moved = main_birth();
        moved.ref_name = "dev".into();
        assert!(
            signer(&cert, "chain-a", "lab", &[moved])
                .unwrap_err()
                .contains("ref updates")
        );
        let mut extra = vec![main_birth(), main_birth()];
        extra[1].ref_name = "feature".into();
        assert!(signer(&cert, "chain-a", "lab", &extra).is_err());
        let mut forged = cert.clone();
        forged.cert.push(b'\n');
        forged.cert.extend_from_slice(b"0000000000000000000000000000000000000000 ab5b1f3d5b7e3e0e0d33e2c6d1f6c2a7d3a7f1e2 refs/heads/dev\n");
        assert!(
            signer(&forged, "chain-a", "lab", &[main_birth()])
                .unwrap_err()
                .contains("does not verify")
        );
    }

    #[test]
    fn the_builder_writes_what_git_would_and_the_parser_refuses_the_rest() {
        let sk = ssh_key(9);
        let updates = vec![
            RefUpdate {
                ref_name: "feature/x".into(),
                prev_oid: Some(vec![1; 20]),
                new_oid: None,
            },
            main_birth(),
        ];
        let text = certificate(&nonce("chain-b", "lab"), &updates);
        assert!(
            std::str::from_utf8(&text).unwrap().starts_with(&format!(
                "certificate version 0.1\nnonce {}\n\n",
                nonce("chain-b", "lab")
            ))
        );
        assert_eq!(parse(&text).unwrap().updates, updates);
        let cert = PushCert {
            sshsig: sshsig(&sk, GIT_SSH_NS, &text),
            cert: text.clone(),
        };
        let reordered: Vec<RefUpdate> = updates.iter().rev().cloned().collect();
        let key = signer(&cert, "chain-b", "lab", &reordered).unwrap();
        assert_eq!(key, ssh_pubkey(&sk), "order-free");
        let under_ducktape = PushCert {
            sshsig: sshsig(&sk, keyscheme::sshsig::DUCKTAPE_SSH_NS, &text),
            cert: text,
        };
        assert!(
            signer(&under_ducktape, "chain-b", "lab", &updates).is_err(),
            "namespace `git` only"
        );

        assert!(parse(b"nope").unwrap_err().contains("certificate version"));
        assert!(
            parse(b"certificate version 0.1\npusher x\n\n")
                .unwrap_err()
                .contains("nonce")
        );
        assert!(
            parse(b"certificate version 0.1\nnonce a/b\n\n")
                .unwrap_err()
                .contains("no ref updates")
        );
        assert!(parse(b"certificate version 0.1\nnonce a/b\n\n0000000000000000000000000000000000000000 ab5b1f3d5b7e3e0e0d33e2c6d1f6c2a7d3a7f1e2 refs/tags/v1\n").unwrap_err().contains("non-branch"));
        assert!(
            parse(b"certificate version 0.1\nnonce a/b\n\nzz ab refs/heads/main\n")
                .unwrap_err()
                .contains("sha1 hex")
        );
    }

    /// A nonce git will not sign is a repo nobody can push to, and the shape
    /// that broke was the ORDINARY one: every id `node init` mints is
    /// `<name>#<8 hex>`, while an e2e cluster is named `ducktape-e2e-<pid>-<n>`
    /// and carries no `#` — so this is checked against real-shaped ids, and
    /// against the two things git refuses (`send-pack.c`'s
    /// `reject_invalid_nonce`): a byte outside its alphabet, and 256 or more of
    /// them.
    #[test]
    fn a_minted_chain_ids_nonce_is_one_git_will_sign() {
        let git_accepts = |byte: u8| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'/' | b'<' | b'>' | b'=' | b'_')
        };
        let longest_repo = "r".repeat(crate::MAX_REPO_NAME_LEN);
        for (chain, repo) in [
            ("gw-a#238d3908", "ducktape"),
            ("dognet#25c07300", "default"),
            // a name is only "not empty, no `#`": whatever an operator typed
            // rides through the digest, including bytes that would otherwise
            // end the capability value or leave the alphabet.
            ("a network someone named#0b821df3", "lab"),
            ("실험망#deadbeef", longest_repo.as_str()),
        ] {
            let nonce = nonce(chain, repo);
            assert!(
                nonce.len() < GIT_NONCE_LEN_LIMIT,
                "{nonce:?} is {} bytes, at or past git's limit",
                nonce.len()
            );
            assert!(
                nonce.bytes().all(git_accepts),
                "{nonce:?} carries a byte git will not sign"
            );
        }
        assert_ne!(
            nonce("chain-a", "lab"),
            nonce("chain-b", "lab"),
            "two chains never share a nonce"
        );
        assert_ne!(
            nonce("chain-a", "lab"),
            nonce("chain-a", "other"),
            "two repos on one chain never share a nonce"
        );
    }
}
