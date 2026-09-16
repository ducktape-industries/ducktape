//! the desktop app's signed-commit lane, end to end minus the webview: the
//! EXACT payload files-client.ts builds (`JSON.stringify({ commit: body })`,
//! serde's externally-tagged `FilesMsg`), wrapped in the op frame
//! `user-sign-frame` prints, POSTed raw to `/v1/submit/frame` — and the files
//! module records the frame's verified signer as author. The node-authored
//! operator lane stays a distinct author; tampered frames are refused.

#[path = "fs_support/mod.rs"]
mod support;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use commonware_cryptography::{Signer as _, ed25519};
use duckfs_client::api::NodeApi as _;
use support::Harness;

/// minimal raw http/1.1 exchange with an explicit content-type — (status, body).
fn http(port: u16, method: &str, path: &str, content_type: &str, body: &[u8]) -> (u16, Vec<u8>) {
    nettest::http_bytes(port, method, path, content_type, body)
}

/// the same exchange carrying this node's operator credential — what a LOCAL
/// daemon presents, and the only way to reach the daemon-origin lane now that
/// an uncredentialed mutation is refused before the handler runs.
fn http_as_daemon(
    h: &Harness,
    port: u16,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
) -> (u16, Vec<u8>) {
    let (name, value) = h.write_header();
    nettest::try_http_bytes_with(port, method, path, content_type, &[(name, value)], body)
        .expect("app-surface request")
}

/// the commit payload exactly as the TS transport serializes it.
fn commit_payload(path: &str, message: &str, bytes: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "commit": {
            "base_snapshot": null,
            "message": message,
            "changes": [{
                "put": {
                    "path": path,
                    "exec": false,
                    "meta": {},
                    "content": { "inline": { "b64": STANDARD.encode(bytes) } },
                }
            }],
        }
    }))
    .expect("commit payload serializes")
}

#[test]
fn signed_commit_lands_with_the_signer_as_author_and_home_authority() {
    let h = Harness::start();
    let port = h
        .node_url()
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .expect("harness port");

    let signer = ed25519::PrivateKey::from_seed(7);
    let actor = format!("ext:{}", noded::hex_bytes(signer.public_key().as_ref()));
    let home = format!("/home/{actor}/notes.txt");

    // the signer's own home subtree: authorized because the frame's verified
    // origin IS the owner. seq is a tie-breaker only — any u64.
    let payload = commit_payload(&home, "signed from the app", b"authored bytes");
    let frame = node::encode_frame(
        &signer,
        1,
        &sdk::Msg {
            target: "files".into(),
            payload,
        },
    );
    let (status, body) = http(
        port,
        "POST",
        "/v1/submit/frame",
        "application/octet-stream",
        &frame,
    );
    assert_eq!(
        status,
        200,
        "signed commit lands: {}",
        String::from_utf8_lossy(&body)
    );

    // the history records the SIGNER as the author.
    let (status, body) = http(
        port,
        "POST",
        "/v1/query",
        "application/json",
        br#"{"target":"files","query":{"history":{"limit":8}}}"#,
    );
    assert_eq!(status, 200);
    let history: serde_json::Value = serde_json::from_slice(&body).expect("history json");
    let author: duckfs_core::Actor =
        serde_json::from_value(history["history"][0]["author"].clone()).expect("typed author");
    assert_eq!(
        author,
        duckfs_core::Actor::Key(signer.public_key().as_ref().to_vec()),
        "the commit's author is the frame's verified signer"
    );

    // Home labels are shared namespace in the current module policy. A node
    // write may edit this path, but must never borrow the earlier user author.
    let commit_body = serde_json::json!({
        "base_snapshot": history["history"][0]["id"],
        "message": "forged from the daemon lane",
        "changes": [{
            "put": {
                "path": home,
                "exec": false,
                "meta": {},
                "content": { "inline": { "b64": STANDARD.encode(b"forged") } },
            }
        }],
    });
    let (status, body) = http_as_daemon(
        &h,
        port,
        "POST",
        "/v1/submit",
        "application/json",
        &serde_json::to_vec(
            &serde_json::json!({"target":"files","payload":{"commit":commit_body}}),
        )
        .unwrap(),
    );
    assert_eq!(status, 200, "node-authored write: {}", String::from_utf8_lossy(&body));
    let (_, body) = http(port, "POST", "/v1/query", "application/json", br#"{"target":"files","query":{"history":{"limit":8}}}"#);
    let history: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let author: duckfs_core::Actor = serde_json::from_value(history["history"][0]["author"].clone()).unwrap();
    assert_eq!(author, duckfs_core::Actor::Key(noded::DEFAULT_ORIGIN.as_bytes().to_vec()));

    // and a tampered frame (payload swapped after signing) never executes.
    let mut tampered = node::encode_frame(
        &signer,
        2,
        &sdk::Msg {
            target: "files".into(),
            payload: commit_payload(&home, "pre-tamper", b"a"),
        },
    );
    let n = tampered.len();
    tampered[n - 70] ^= 0x01; // flip a payload byte behind the signature
    let (status, _) = http(
        port,
        "POST",
        "/v1/submit/frame",
        "application/octet-stream",
        &tampered,
    );
    assert_eq!(status, 400, "a tampered frame is refused");
}

#[test]
fn admitted_signer_keeps_its_key_home_and_records_the_account_author() {
    let h = Harness::start();
    let port = h
        .node_url()
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .expect("harness port");
    let signer = ed25519::PrivateKey::from_seed(19);
    let key = signer.public_key().as_ref().to_vec();
    let home = format!("/home/ext:{}/note.txt", noded::hex_bytes(&key));
    let submit = |target: &str, payload: Vec<u8>, seq| {
        let frame = node::encode_frame(
            &signer,
            seq,
            &sdk::Msg {
                target: target.into(),
                payload,
            },
        );
        let (status, body) = http(
            port,
            "POST",
            "/v1/submit/frame",
            "application/octet-stream",
            &frame,
        );
        assert_eq!(
            status,
            200,
            "signed {target}: {}",
            String::from_utf8_lossy(&body)
        );
    };
    submit(
        "files",
        commit_payload(&home, "before admission", b"key"),
        1,
    );
    submit(
        "identity",
        identity::encode_msg(&identity::IdentityMsg::Create {
            name: "file-writer".into(),
            scheme: identity::KeyScheme::Ed25519,
        }),
        2,
    );
    let request = serde_json::to_vec(&serde_json::json!({
        "target": "identity",
        "query": identity::IdentityQuery::OfKey { key: key.clone() },
    }))
    .expect("account query");
    let (status, body) = http(port, "POST", "/v1/query", "application/json", &request);
    assert_eq!(status, 200);
    let identity::IdentityReply::Account(Some(account)) =
        identity::decode_reply(&body).expect("account reply")
    else {
        panic!("signer has an account");
    };
    // Admission changes the canonical author, while this exact signer keeps
    // authority over the key home it created before joining the account.
    let refs = h.files().refs().expect("committed files head");
    let mut admitted_commit: serde_json::Value =
        serde_json::from_slice(&commit_payload(&home, "after admission", b"account"))
            .expect("commit json");
    admitted_commit["commit"]["base_snapshot"] = serde_json::json!(refs.head);
    submit(
        "files",
        serde_json::to_vec(&admitted_commit).expect("admitted commit"),
        3,
    );
    let (status, body) = http(
        port,
        "POST",
        "/v1/query",
        "application/json",
        br#"{"target":"files","query":{"history":{"limit":8}}}"#,
    );
    assert_eq!(status, 200);
    let history: serde_json::Value = serde_json::from_slice(&body).expect("history json");
    let authors: Vec<duckfs_core::Actor> = history["history"]
        .as_array()
        .expect("snapshots")
        .iter()
        .map(|snapshot| serde_json::from_value(snapshot["author"].clone()).expect("typed author"))
        .collect();
    assert_eq!(
        authors,
        [
            duckfs_core::Actor::Account(account.number),
            duckfs_core::Actor::Key(key)
        ]
    );
}
