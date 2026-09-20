//! the CLIENT half of a mutating data-plane request's proof of possession:
//! the bytes an acting key signs and the three headers that carry the proof.
//!
//! it lives beside the op-frame codec because it is the same kind of thing —
//! a wire contract every signer in the tree (the CLI, the desktop app, the
//! daemon's own tests) must spell identically to what the daemon verifies.
//! the daemon's verify half reads these same definitions; a second spelling
//! of the message anywhere is a second place to get the binding wrong.

use commonware_cryptography::{Signer as _, ed25519};
use sha2::{Digest as _, Sha256};

/// PoP signing namespace for the mutating data plane.
pub const DATA_REQ_NS: &[u8] = b"ducktape-data-req-v1";

/// hex ed25519 public key (32 bytes) of the acting identity.
pub const KEY_HEADER: &str = "x-ducktape-key";
/// decimal unix seconds the request was signed at.
pub const TS_HEADER: &str = "x-ducktape-ts";
/// hex ed25519 signature (64 bytes) over the canonical request bytes.
pub const SIG_HEADER: &str = "x-ducktape-sig";

/// wall-clock seconds since the Unix epoch (saturating before 1970).
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// the canonical bytes a data-plane request's PoP signs / verifies: method,
/// path+query, the TARGET NODE's consensus key, the timestamp, and the sha256
/// of the BODY. the body digest is what the control plane's message omits, and
/// it is the difference that matters here: these routes carry the payload the
/// mutation applies, so a signature that did not cover it would authenticate a
/// caller while leaving an attacker free to swap what they wrote.
pub fn request_message(
    method: &str,
    path_and_query: &str,
    node_key: &[u8],
    ts: u64,
    body: &[u8],
) -> Vec<u8> {
    request_message_digest(
        method,
        path_and_query,
        node_key,
        ts,
        &Sha256::digest(body).into(),
    )
}

/// the same canonical bytes when the body's digest is already known and the
/// body itself is not — a blob upload STREAMS, so the only thing either end
/// ever holds of it is this hash. Identical bytes to [`request_message`] over
/// the same body: the signature does not know which way the digest arrived.
pub fn request_message_digest(
    method: &str,
    path_and_query: &str,
    node_key: &[u8],
    ts: u64,
    digest: &[u8; 32],
) -> Vec<u8> {
    let digest = digest.as_slice();
    let mut m = Vec::with_capacity(method.len() + path_and_query.len() + node_key.len() + 45);
    m.extend_from_slice(method.as_bytes());
    m.push(0x1f);
    m.extend_from_slice(path_and_query.as_bytes());
    m.push(0x1f);
    m.extend_from_slice(node_key);
    m.push(0x1f);
    m.extend_from_slice(&ts.to_be_bytes());
    m.push(0x1f);
    m.extend_from_slice(digest);
    m
}

/// sign one data-plane request with an acting key, bound to the target node.
pub fn sign_request(
    signer: &ed25519::PrivateKey,
    method: &str,
    path_and_query: &str,
    node_key: &[u8],
    ts: u64,
    body: &[u8],
) -> ed25519::Signature {
    sign_request_digest(
        signer,
        method,
        path_and_query,
        node_key,
        ts,
        &Sha256::digest(body).into(),
    )
}

/// sign one data-plane request whose body is known only by its digest.
pub fn sign_request_digest(
    signer: &ed25519::PrivateKey,
    method: &str,
    path_and_query: &str,
    node_key: &[u8],
    ts: u64,
    digest: &[u8; 32],
) -> ed25519::Signature {
    signer.sign(
        DATA_REQ_NS,
        &request_message_digest(method, path_and_query, node_key, ts, digest),
    )
}

/// the three headers a client attaches to a mutating request, ready to set
/// verbatim. every in-tree writer goes through THIS — a second place that
/// spells the trio is a second place to get the binding wrong.
pub fn request_headers(
    signer: &ed25519::PrivateKey,
    method: &str,
    path_and_query: &str,
    node_key: &[u8],
    body: &[u8],
) -> [(&'static str, String); 3] {
    request_headers_digest(
        signer,
        method,
        path_and_query,
        node_key,
        &Sha256::digest(body).into(),
    )
}

/// the same three headers for a body known only by its digest — what a
/// streamed upload signs, since it never holds the body to hash twice.
pub fn request_headers_digest(
    signer: &ed25519::PrivateKey,
    method: &str,
    path_and_query: &str,
    node_key: &[u8],
    digest: &[u8; 32],
) -> [(&'static str, String); 3] {
    let ts = now_secs();
    let sig = sign_request_digest(signer, method, path_and_query, node_key, ts, digest);
    [
        (KEY_HEADER, hex(signer.public_key().as_ref())),
        (TS_HEADER, ts.to_string()),
        (SIG_HEADER, hex(sig.as_ref())),
    ]
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_cryptography::Verifier as _;

    /// the trio verifies against the bytes the daemon rebuilds from the same
    /// method, path, node key, timestamp and body — the whole contract.
    #[test]
    fn the_headers_prove_possession_over_the_request() {
        let signer = ed25519::PrivateKey::from_seed(11);
        let node_key = [7u8; 32];
        let body = b"raw chunk bytes";
        let headers = request_headers(&signer, "POST", "/v1/files/blob", &node_key, body);
        let [(key_name, key_hex), (ts_name, ts), (sig_name, sig_hex)] = headers;
        assert_eq!(
            (key_name, ts_name, sig_name),
            (KEY_HEADER, TS_HEADER, SIG_HEADER)
        );
        assert_eq!(key_hex, hex(signer.public_key().as_ref()));

        let ts: u64 = ts.parse().expect("decimal seconds");
        let sig = sign_request(&signer, "POST", "/v1/files/blob", &node_key, ts, body);
        assert_eq!(
            sig_hex,
            hex(sig.as_ref()),
            "the header carries this signature"
        );
        let message = request_message("POST", "/v1/files/blob", &node_key, ts, body);
        assert!(signer.public_key().verify(DATA_REQ_NS, &message, &sig));

        // the body is inside the signed bytes: a swapped payload does not verify.
        let swapped = request_message("POST", "/v1/files/blob", &node_key, ts, b"other");
        assert!(!signer.public_key().verify(DATA_REQ_NS, &swapped, &sig));
        // and so is the node: the same request signed for another node fails.
        let elsewhere = request_message("POST", "/v1/files/blob", &[8u8; 32], ts, body);
        assert!(!signer.public_key().verify(DATA_REQ_NS, &elsewhere, &sig));
    }
}
