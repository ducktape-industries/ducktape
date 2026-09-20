//! the client half of the WebAuthn relying-party page.
//!
//! Nothing here talks WebAuthn: the browser does (`ops/auth-page/index.html`
//! at [`AUTH_PAGE`], RP ID = its own host). A client
//!
//! 1. builds a [`Request`] and opens the browser to [`request_url`] — the
//!    request rides the URL FRAGMENT, so it never reaches a server;
//! 2. binds a one-shot [`Listener`] on loopback; the page's result arrives as
//!    a top-level form POST (`result=<JSON>`) — a navigation, which Chrome's
//!    local-network-access rules and CORS both leave alone, unlike a `fetch`;
//!    or, when the request is shown as a QR for a phone, mints a [`Relay`]
//!    slot on the auth host and polls it for the same result;
//! 3. turns the [`Outcome`] into signed bytes the node accepts: a frame whose
//!    origin is the passkey/wallet ([`passkey_frame`], [`wallet_frame`]) or a
//!    passkey's consent to admit this device ([`login_consent`]).
//!
//! The contract is `ops/auth-page/README.md`; the verifier it must satisfy is
//! `keyscheme` (`Secp256r1` = the assertion envelope, `Secp256k1` =
//! `personal_sign` over [`keyscheme::personal_message`]).
//!
//! Three ceremony facts every caller sequences around:
//! - a passkey REGISTRATION is two touches: `create` yields the public key,
//!   then `get` over the `AddKey` frame preimage proves possession (a
//!   `webauthn.create` attestation carries no signature we can verify);
//! - a wallet is two touches: it reveals no public key on its own, so touch 1
//!   signs [`reveal_message`] and the key is recovered from that signature,
//!   touch 2 signs the real preimage;
//! - a LOGIN is two touches for the same reason: an add-key consent names the
//!   account it admits into, and a passkey only says which account it belongs
//!   to by answering ([`account_request`] -> [`assertion_account`]). Touch 1
//!   asks; touch 2 ([`login_request`]) is the consent, bound to that answer.

use std::net::Ipv4Addr;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use keyscheme::KeyScheme;
use sha2::{Digest as _, Sha256};

mod identity_contract;
use identity_contract as identity;

/// the live page. Its host IS the RP ID every passkey is scoped to — changing
/// it invalidates every registered passkey (acceptable at zero live networks).
pub const AUTH_PAGE: &str = "https://auth.ducktape.industries/";

/// the domain tag of a wallet's key-reveal touch: the wallet signs this ‖ 16
/// random bytes, and the client recovers its public key from the signature.
/// Nothing on chain ever verifies a reveal signature, so it authorizes nothing.
pub const REVEAL_NS: &[u8] = b"ducktape:reveal-key:v1";

/// the largest form body the listener reads — an assertion is a few KiB.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// The discoverable passkey's chain and account hint, stored as `user.id`.
/// An assertion returns this unsigned; a matching signature still proves access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserHandle {
    chain_hash: [u8; 32],
    number: u64,
}

impl UserHandle {
    pub fn new(chain_id: &str, number: u64) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"ducktape:passkey-account:v1\0");
        hash.update(chain_id.as_bytes());
        Self {
            chain_hash: hash.finalize().into(),
            number,
        }
    }

    pub fn to_bytes(self) -> [u8; 40] {
        let mut bytes = [0; 40];
        bytes[..32].copy_from_slice(&self.chain_hash);
        bytes[32..].copy_from_slice(&self.number.to_le_bytes());
        bytes
    }
}

/// one ceremony, as the page's fragment names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// `navigator.credentials.create()` — a new passkey for account `user`.
    /// The challenge is pass-through (a create attestation proves nothing
    /// here), so any 32 bytes do — [`create_challenge`].
    Create {
        challenge: [u8; 32],
        user: u64,
        chain_id: String,
        name: String,
    },
    /// `navigator.credentials.get()` with `allowCredentials: []` — the
    /// discoverable passkey signs `challenge` (already `SHA-256(ns ‖ preimage)`).
    Get { challenge: [u8; 32] },
    /// `personal_sign(message)` — the EXACT bytes, un-hashed; the wallet
    /// applies the EIP-191 envelope itself.
    Eth { message: Vec<u8> },
}

/// the page's result, decoded. The `create` attestation object is not kept:
/// possession is proven by the `get` that follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Create {
        credential_id: Vec<u8>,
        /// the 33-byte compressed SEC1 point (the page lifts it out of SPKI).
        public_key: Vec<u8>,
    },
    Get {
        authenticator_data: Vec<u8>,
        client_data_json: Vec<u8>,
        /// raw `R‖S`, 64 bytes (the page normalizes DER away).
        signature: Vec<u8>,
        /// The unsigned chain and account hint a registration wrote as `user.id`;
        /// `None` for a credential registered without one.
        user_handle: Option<UserHandle>,
    },
    Eth {
        address: String,
        /// `r‖s‖v`, 65 bytes.
        signature: Vec<u8>,
        /// the bytes the wallet was handed (echoed, so a client can check
        /// the touch answered THIS request).
        message: Vec<u8>,
    },
}

// ============================================================================
// the request URL
// ============================================================================

/// the URL to open: `page#op=…&challenge=…[&user=…&name=…]&cb=<callback>`.
/// Binary fields base64url without padding, `name` and `cb` percent-encoded.
pub fn request_url(page: &str, request: &Request, callback: &str) -> String {
    let mut params: Vec<String> = Vec::new();
    match request {
        Request::Create {
            challenge,
            user,
            chain_id,
            name,
        } => {
            params.push("op=create".into());
            params.push(format!("challenge={}", B64.encode(challenge)));
            let handle = UserHandle::new(chain_id, *user);
            params.push(format!("user={}", B64.encode(handle.to_bytes())));
            let label = format!("{name} · {chain_id}");
            params.push(format!("name={}", url_encode(&label)));
        }
        Request::Get { challenge } => {
            params.push("op=get".into());
            params.push(format!("challenge={}", B64.encode(challenge)));
        }
        Request::Eth { message } => {
            params.push("op=eth".into());
            params.push(format!("challenge={}", B64.encode(message)));
        }
    }
    params.push(format!("cb={}", url_encode(callback)));
    format!("{page}#{}", params.join("&"))
}

/// percent-encode everything but the unreserved set; the page decodes with
/// `URLSearchParams`, which understands both this and the raw form.
fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if unreserved {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// a fresh pass-through challenge for a `create` — not verified by anything,
/// distinct per ceremony so a replayed result is recognizable as stale.
pub fn create_challenge() -> [u8; 32] {
    rand::random()
}

/// the bytes a wallet signs to reveal its key: [`REVEAL_NS`] ‖ 16 random bytes.
pub fn reveal_message() -> Vec<u8> {
    let nonce: [u8; 16] = rand::random();
    let mut message = REVEAL_NS.to_vec();
    message.extend_from_slice(&nonce);
    message
}

// ============================================================================
// the result
// ============================================================================

/// decode one result JSON line into an [`Outcome`]; the page's
/// `{"op","error","message"}` failure shape is an `Err` naming both.
pub fn parse_result(json: &str) -> Result<Outcome, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("auth page result is not JSON: {e}"))?;
    let op = value["op"].as_str().unwrap_or_default().to_string();
    if let Some(error) = value.get("error") {
        return Err(format!(
            "the {op} ceremony failed: {}: {}",
            error.as_str().unwrap_or("error"),
            value["message"].as_str().unwrap_or_default()
        ));
    }
    match op.as_str() {
        "create" => {
            // the browser is a trust boundary: the page compresses the SPKI
            // point itself, and a key spelled any other way is one the chain
            // would refuse as an origin AFTER a consent and a second touch had
            // already been spent on it.
            let public_key = binary(&value, "publicKey")?;
            if !KeyScheme::Secp256r1.pubkey_wellformed(&public_key) {
                return Err(format!(
                    "the auth page returned {} bytes that are not a compressed SEC1 P-256 point",
                    public_key.len()
                ));
            }
            Ok(Outcome::Create {
                credential_id: binary(&value, "credentialId")?,
                public_key,
            })
        }
        "get" => Ok(Outcome::Get {
            authenticator_data: binary(&value, "authenticatorData")?,
            client_data_json: binary(&value, "clientDataJSON")?,
            signature: binary(&value, "signature")?,
            user_handle: user_handle(&value)?,
        }),
        "eth" => Ok(Outcome::Eth {
            address: value["address"].as_str().unwrap_or_default().to_string(),
            signature: hex_0x(value["signature"].as_str().unwrap_or_default())?,
            message: binary(&value, "message")?,
        }),
        other => Err(format!("auth page result names an unknown op {other:?}")),
    }
}

fn binary(value: &serde_json::Value, field: &str) -> Result<Vec<u8>, String> {
    let Some(text) = value[field].as_str() else {
        return Err(format!("auth page result is missing {field:?}"));
    };
    B64.decode(text)
        .map_err(|e| format!("auth page result field {field:?} is not base64url: {e}"))
}

fn user_handle(value: &serde_json::Value) -> Result<Option<UserHandle>, String> {
    let Some(text) = value["userHandle"].as_str() else {
        return Ok(None);
    };
    let bytes = B64
        .decode(text)
        .map_err(|e| format!("auth page userHandle is not base64url: {e}"))?;
    let Ok(bytes) = <[u8; 40]>::try_from(bytes.as_slice()) else {
        return Err(
            "the passkey has an unsupported userHandle; recreate it with \
            `ducktape account key add --passkey` from a member device"
                .into(),
        );
    };
    let mut chain_hash = [0; 32];
    chain_hash.copy_from_slice(&bytes[..32]);
    let mut number = [0; 8];
    number.copy_from_slice(&bytes[32..]);
    Ok(Some(UserHandle {
        chain_hash,
        number: u64::from_le_bytes(number),
    }))
}

fn hex_0x(text: &str) -> Result<Vec<u8>, String> {
    let hex = text.strip_prefix("0x").unwrap_or(text);
    let odd = !hex.len().is_multiple_of(2);
    if odd {
        return Err("auth page eth signature has odd hex length".into());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| format!("auth page eth signature is not hex: {e}"))
        })
        .collect()
}

// ============================================================================
// the loopback callback
// ============================================================================

/// a one-shot loopback HTTP listener the page delivers its result to. Bound
/// on an ephemeral 127.0.0.1 port; [`Listener::wait`] serves exactly one
/// result POST and returns.
///
/// The port alone is not a secret — any local process, or any web origin the
/// user has open, can connect to it. `state` (32 random bytes, carried as a
/// path segment the page echoes back verbatim, since it posts to `cb`
/// exactly as given) binds the answer to THIS ceremony: [`serve_one`] refuses
/// every request whose path does not match rather than trusting whoever
/// connects first.
pub struct Listener {
    listener: TcpListener,
    state: String,
}

impl Listener {
    pub async fn bind() -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let raw: [u8; 32] = rand::random();
        Ok(Self {
            listener,
            state: B64.encode(raw),
        })
    }

    /// the `cb` to put in the request URL — loopback, which is all the page
    /// will deliver to, with the ceremony's `state` in the path.
    pub fn callback_url(&self) -> String {
        let port = self
            .listener
            .local_addr()
            .map(|addr| addr.port())
            .unwrap_or_default();
        format!("http://127.0.0.1:{port}/cb/{}", self.state)
    }

    /// Wait until the page POSTs a result to `/cb/<state>` — anything else
    /// (a wrong path, a stray GET, a malformed body) is answered and ignored,
    /// never ends the wait — then answer it and return. Dropping the future
    /// closes the listener and any accepted connection.
    pub async fn wait(self) -> Result<Outcome, String> {
        let path = format!("/cb/{}", self.state);
        loop {
            let (stream, _) = self
                .listener
                .accept()
                .await
                .map_err(|e| format!("auth callback listener: {e}"))?;
            if let Some(outcome) = serve_one(stream, &path).await? {
                return Ok(outcome);
            }
        }
    }
}

/// one HTTP exchange: `Some(outcome)` for a result POST landing on
/// `expected_path`, `None` for anything else (answered and ignored — a wrong
/// path gets a 404, a non-POST or a malformed body gets a holding/error page,
/// but the wait keeps going in every case).
async fn serve_one(mut stream: TcpStream, expected_path: &str) -> Result<Option<Outcome>, String> {
    let (method, path, body) = read_request(&mut stream).await?;
    if path != expected_path {
        respond(&mut stream, 404, "Not found.").await;
        return Ok(None);
    }
    if method != "POST" {
        respond(&mut stream, 200, "Waiting for the ceremony to finish…").await;
        return Ok(None);
    }
    let Some(result) = form_field(&body, "result") else {
        respond(&mut stream, 400, "The callback carried no result.").await;
        return Ok(None);
    };
    match parse_result(&result) {
        Ok(outcome) => {
            respond(&mut stream, 200, "Done — you can return to ducktape.").await;
            Ok(Some(outcome))
        }
        Err(message) => {
            respond(
                &mut stream,
                200,
                "The ceremony did not complete; ducktape has the details.",
            )
            .await;
            Err(message)
        }
    }
}

/// the request line's method and path, and the body (`content-length`
/// bounded).
async fn read_request(stream: &mut TcpStream) -> Result<(String, String, Vec<u8>), String> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("auth callback: {e}"))?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut content_length = 0usize;
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("auth callback: {e}"))?;
        let end_of_headers = read == 0 || line == "\r\n" || line == "\n";
        if end_of_headers {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    if content_length > MAX_BODY_BYTES {
        return Err(format!("auth callback body exceeds {MAX_BODY_BYTES} bytes"));
    }
    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| format!("auth callback body: {e}"))?;
    Ok((method, path, body))
}

async fn respond(stream: &mut TcpStream, status: u16, text: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    // the same card the page and the relay's "Done" wear (ops/auth-page).
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>ducktape</title>\
         <style>:root{{color-scheme:light dark;--fg:#1b1b1f;--bg:#f3f3f6;--card:#fff;--muted:#6b6b76;--line:#e2e2e8}}\
         @media (prefers-color-scheme:dark){{:root{{--fg:#ececf1;--bg:#111114;--card:#1b1b20;--muted:#9a9aa6;--line:#2a2a33}}}}\
         body{{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);\
         font:16px/1.5 system-ui,-apple-system,\"Segoe UI\",sans-serif}}\
         main{{width:min(26rem,calc(100vw - 2rem));background:var(--card);border:1px solid var(--line);border-radius:16px;padding:2rem}}\
         .brand{{font-size:.8rem;font-weight:600;letter-spacing:.06em;text-transform:uppercase;color:var(--muted)}}\
         p{{margin:1rem 0 0}}</style></head>\
         <body><main><div class=\"brand\">🦆 ducktape</div><p>{text}</p></main></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    // the page has already delivered its result; a peer that hung up before
    // reading the acknowledgement lost nothing.
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// one `application/x-www-form-urlencoded` field, decoded.
fn form_field(body: &[u8], name: &str) -> Option<String> {
    body.split(|b| *b == b'&').find_map(|pair| {
        let (key, value) = split_once(pair, b'=')?;
        let matches = form_decode(key) == name.as_bytes();
        if !matches {
            return None;
        }
        String::from_utf8(form_decode(value)).ok()
    })
}

fn split_once(bytes: &[u8], sep: u8) -> Option<(&[u8], &[u8])> {
    let at = bytes.iter().position(|b| *b == sep)?;
    Some((&bytes[..at], &bytes[at + 1..]))
}

/// `+` → space, `%XX` → byte; a malformed escape is kept verbatim.
fn form_decode(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len());
    let mut i = 0;
    while i < value.len() {
        let byte = value[i];
        let escape = byte == b'%' && i + 2 < value.len();
        if byte == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        if escape {
            let hex = std::str::from_utf8(&value[i + 1..i + 3]).unwrap_or_default();
            if let Ok(decoded) = u8::from_str_radix(hex, 16) {
                out.push(decoded);
                i += 3;
                continue;
            }
        }
        out.push(byte);
        i += 1;
    }
    out
}

/// open `url` in the system browser; `false` when no opener is available (a
/// headless box), in which case the caller prints the URL for a human.
pub fn open_browser(url: &str) -> bool {
    let attempts: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("open", &[])]
    } else if cfg!(target_os = "windows") {
        &[("cmd", &["/C", "start", ""])]
    } else {
        &[("xdg-open", &[])]
    };
    attempts.iter().any(|(program, args)| {
        std::process::Command::new(program)
            .args(*args)
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
    })
}

// ============================================================================
// the relay callback — a ceremony that ran on a phone
// ============================================================================

/// how often [`Relay::wait`] asks the auth host whether the phone answered.
pub const RELAY_POLL: Duration = Duration::from_millis(1500);

/// the auth host's `/r/<id>` slot the page POSTs to when the ceremony ran on
/// a phone that cannot reach this machine (the app showed the request as a
/// QR); [`Relay::wait`] polls it. Contract: `ops/auth-page/README.md` §Relay.
pub struct Relay {
    base: String,
    /// 32 random bytes, base64url — unguessable, so nobody else can poll it.
    pub id: String,
}

impl Default for Relay {
    fn default() -> Self {
        Self::new()
    }
}

impl Relay {
    /// at the live page.
    pub fn new() -> Self {
        Self::at(AUTH_PAGE)
    }

    /// at another deployment (`--auth-page`, tests). `base` ends with `/`.
    pub fn at(base: &str) -> Self {
        let raw: [u8; 32] = rand::random();
        Self {
            base: base.to_string(),
            id: B64.encode(raw),
        }
    }

    /// the `cb` to put in the request URL — the page accepts its own origin's
    /// `/r/<id>`.
    pub fn callback_url(&self) -> String {
        format!("{}r/{}", self.base, self.id)
    }

    /// Wait until the phone answers (200) or `deadline` passes; a 204 is
    /// "not yet". Dropping the future cancels requests and poll delays.
    pub async fn wait(self, deadline: Duration) -> Result<Outcome, String> {
        self.wait_reporting(deadline, |_| {}).await
    }

    /// [`Relay::wait`] that tells `progress` how long the code has left
    /// before every poll — a terminal's countdown line.
    pub async fn wait_reporting(
        self,
        deadline: Duration,
        mut progress: impl FnMut(Duration),
    ) -> Result<Outcome, String> {
        let url = self.callback_url();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("relay client: {e}"))?;
        let started = tokio::time::Instant::now();
        let mut polls: u32 = 0;
        let polling = async {
            loop {
                polls += 1;
                progress(deadline.saturating_sub(started.elapsed()));
                let response = client.get(&url).send().await.map_err(|error| {
                    let error = error.without_url();
                    tracing::warn!(
                        target: "ducktape::auth",
                        event = "relay_unreachable",
                        relay = %self.id,
                        polls,
                        error = %error,
                    );
                    format!("relay: {error}")
                })?;
                match response.status().as_u16() {
                    200 => {
                        let body = response
                            .text()
                            .await
                            .map_err(|error| format!("relay body: {}", error.without_url()))?;
                        let outcome = parse_result(&body);
                        tracing::info!(
                            target: "ducktape::auth",
                            event = "relay_answered",
                            relay = %self.id,
                            polls,
                            waited_ms = started.elapsed().as_millis() as u64,
                            ok = outcome.is_ok(),
                        );
                        return outcome;
                    }
                    204 => {
                        tracing::trace!(target: "ducktape::auth", event = "relay_poll", relay = %self.id, polls)
                    }
                    other => {
                        tracing::warn!(target: "ducktape::auth", event = "relay_refused", relay = %self.id, polls, status = other);
                        return Err(format!("relay answered {other}"));
                    }
                }
                tokio::time::sleep(RELAY_POLL).await;
            }
        };
        match tokio::time::timeout_at(started + deadline, polling).await {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(target: "ducktape::auth", event = "relay_timeout", relay = %self.id, polls);
                Err("the phone did not answer in time".into())
            }
        }
    }
}

/// `m:ss` of a duration, whole seconds — the countdown beside a QR.
pub fn countdown(left: Duration) -> String {
    let secs = left.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// the ceremony URL as a QR a phone can scan OFF A TERMINAL: half-block
/// cells (two module rows per text line), a quiet zone, dark modules drawn in
/// the terminal's foreground. A ~280-byte URL comes out around 37 lines by
/// 77 columns — inside an 80-column terminal.
pub fn terminal_qr(text: &str) -> Result<String, String> {
    let code = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::L)
        .map_err(|e| format!("qr: {e}"))?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build())
}

// ============================================================================
// ceremony builders — pure
// ============================================================================

/// the `get` a passkey answers to sign an op frame AS ITS ORIGIN, and the
/// preimage the answer completes ([`passkey_frame`]).
pub fn passkey_frame_request(pubkey: &[u8], seq: u64, msg: &sdk::Msg) -> (Request, Vec<u8>) {
    let preimage = node::frame_preimage(KeyScheme::Secp256r1, pubkey, seq, msg);
    let challenge = keyscheme::webauthn_challenge(node::FRAME_NS, &preimage);
    (Request::Get { challenge }, preimage)
}

/// the frame: preimage ‖ the assertion envelope — what `/v1/submit/frame`
/// verifies under `Secp256r1`.
pub fn passkey_frame(mut preimage: Vec<u8>, outcome: &Outcome) -> Result<Vec<u8>, String> {
    let Outcome::Get {
        authenticator_data,
        client_data_json,
        signature,
        ..
    } = outcome
    else {
        return Err("expected a passkey assertion (op=get)".into());
    };
    preimage.extend_from_slice(&keyscheme::webauthn_proof(
        authenticator_data,
        client_data_json,
        signature,
    ));
    Ok(preimage)
}

/// the `personal_sign` a wallet answers to sign an op frame AS ITS ORIGIN,
/// and the preimage the answer completes ([`wallet_frame`]).
pub fn wallet_frame_request(pubkey: &[u8], seq: u64, msg: &sdk::Msg) -> (Request, Vec<u8>) {
    let preimage = node::frame_preimage(KeyScheme::Secp256k1, pubkey, seq, msg);
    let message = keyscheme::personal_message(node::FRAME_NS, &preimage);
    (Request::Eth { message }, preimage)
}

/// the frame: preimage ‖ `r‖s‖v` — what `/v1/submit/frame` verifies under
/// `Secp256k1` (recovery against the origin).
pub fn wallet_frame(mut preimage: Vec<u8>, outcome: &Outcome) -> Result<Vec<u8>, String> {
    let Outcome::Eth { signature, .. } = outcome else {
        return Err("expected a wallet signature (op=eth)".into());
    };
    if signature.len() != 65 {
        return Err(format!(
            "a wallet signature is 65 bytes (r‖s‖v), got {}",
            signature.len()
        ));
    }
    preimage.extend_from_slice(signature);
    Ok(preimage)
}

/// the wallet's public key (33-byte compressed SEC1) recovered from its
/// key-reveal touch over `reveal` ([`reveal_message`]); the outcome must echo
/// exactly that message.
pub fn wallet_pubkey(reveal: &[u8], outcome: &Outcome) -> Result<Vec<u8>, String> {
    let Outcome::Eth {
        signature, message, ..
    } = outcome
    else {
        return Err("expected a wallet signature (op=eth)".into());
    };
    if message != reveal {
        return Err("the wallet signed a different message than the key reveal".into());
    }
    keyscheme::recover_personal_sign(message, signature)
        .ok_or_else(|| "the wallet signature does not recover to a key".to_string())
}

/// login, touch 1: a `get` that asks the passkey nothing but WHICH account it
/// belongs to. Its challenge is random and authorizes nothing — no consent is
/// minted from this answer; the account it reveals is what touch 2's consent
/// is bound to.
pub fn account_request() -> Request {
    Request::Get {
        challenge: create_challenge(),
    }
}

/// the account a passkey assertion names in its `userHandle`. Unsigned, so it
/// is a HINT: it picks which account to ask about, and [`login_add_key`] then
/// accepts only a consent a key OF that account actually signed.
pub fn assertion_account(chain_id: &str, outcome: &Outcome) -> Result<u64, String> {
    let Outcome::Get { user_handle, .. } = outcome else {
        return Err("expected a passkey assertion (op=get)".into());
    };
    let Some(handle) = user_handle else {
        return Err(
            "the passkey names no account (no userHandle) — register it with \
         `ducktape account key add --passkey` from a member device"
                .to_string(),
        );
    };
    let expected = UserHandle::new(chain_id, handle.number);
    let different_chain = handle.chain_hash != expected.chain_hash;
    if different_chain {
        return Err(format!(
            "this passkey belongs to a different chain; select a passkey for {chain_id}"
        ));
    }
    Ok(handle.number)
}

/// login, touch 2: the `get` a passkey answers to CONSENT to admitting
/// `device_key` (ed25519) into `account` at its `generation` on `chain_id`,
/// until `expires_at` — the identity module's own `AddKey` preimage, hashed
/// into the challenge.
pub fn login_request(
    chain_id: &str,
    device_key: &[u8],
    generation: u64,
    account: u64,
    expires_at: u64,
) -> Request {
    let preimage = identity::add_key_preimage(
        chain_id,
        KeyScheme::Ed25519,
        device_key,
        generation,
        account,
        expires_at,
    );
    let challenge = keyscheme::webauthn_challenge(identity::IDENTITY_ADD_KEY_NS, &preimage);
    Request::Get { challenge }
}

/// the login answer: the account the passkey names (its `userHandle`) and the
/// envelope proof an `AddKey { authorizer: { key: <that passkey>, proof } }`
/// carries. Which of the account's `Secp256r1` keys signed is the caller's to
/// find — by verifying the proof against each.
pub fn login_consent(chain_id: &str, outcome: &Outcome) -> Result<(u64, Vec<u8>), String> {
    let number = assertion_account(chain_id, outcome)?;
    let Outcome::Get {
        authenticator_data,
        client_data_json,
        signature,
        ..
    } = outcome
    else {
        return Err("expected a passkey assertion (op=get)".into());
    };
    Ok((
        number,
        keyscheme::webauthn_proof(authenticator_data, client_data_json, signature),
    ))
}

/// the `AddKey` a login submits: the authorizer is whichever of the account's
/// passkeys verifies the assertion (the page does not say which one signed),
/// carrying the assertion envelope as its proof. `Err` when none does — a
/// consent at another generation, or a passkey off this account.
pub fn login_add_key(
    chain_id: &str,
    device_key: &[u8],
    generation: u64,
    account: &identity::AccountView,
    label: Option<String>,
    proof: Vec<u8>,
    expires_at: u64,
) -> Result<identity::IdentityMsg, String> {
    let preimage = identity::add_key_preimage(
        chain_id,
        KeyScheme::Ed25519,
        device_key,
        generation,
        account.number,
        expires_at,
    );
    let signer = account
        .keys
        .iter()
        .filter(|key| key.scheme == KeyScheme::Secp256r1)
        .find(|key| {
            KeyScheme::Secp256r1.verify(
                &key.pubkey,
                identity::IDENTITY_ADD_KEY_NS,
                &preimage,
                &proof,
            )
        });
    let Some(signer) = signer else {
        return Err(format!(
            "no passkey on account {} signed this consent",
            account.number
        ));
    };
    Ok(identity::IdentityMsg::AddKey {
        scheme: KeyScheme::Ed25519,
        label,
        authorizer: identity::Authorizer {
            key: signer.pubkey.clone(),
            account: account.number,
            expires_at,
            proof,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyscheme::testkit::{
        eth_key, eth_proof, eth_pubkey, eth_sign_message, passkey, passkey_assertion_parts,
        passkey_pubkey,
    };

    const RP: &str = "auth.ducktape.industries";
    const CHAIN: &str = "demo#a1b2c3d4";
    // Independently computed with Python hashlib and Node node:crypto.
    const HANDLE_42: &str = "6zD6Woip0W_PPk0EWZGNZdwjPHgvY2dqMFHQVJ7xyIwqAAAAAAAAAA";

    #[test]
    fn a_chain_bound_handle_decodes_from_the_page() {
        let json = serde_json::json!({
            "op": "get",
            "authenticatorData": "AQ",
            "clientDataJSON": "Ag",
            "signature": "Aw",
            "userHandle": HANDLE_42,
        });
        let outcome = parse_result(&json.to_string()).unwrap();
        assert_eq!(assertion_account(CHAIN, &outcome).unwrap(), 42);
        assert!(
            assertion_account("demo#other", &outcome)
                .unwrap_err()
                .contains("different chain")
        );
        assert!(
            login_consent("demo#other", &outcome)
                .unwrap_err()
                .contains("different chain")
        );
    }

    #[test]
    fn registration_handles_are_stable_and_chain_scoped() {
        let handle = UserHandle::new(CHAIN, 42);
        assert_eq!(B64.encode(handle.to_bytes()), HANDLE_42);
        assert_eq!(handle, UserHandle::new(CHAIN, 42));
        assert_ne!(handle, UserHandle::new("demo#different", 42));
        assert_ne!(handle, UserHandle::new(CHAIN, 43));
        // Account numbers retain all 64 bits in little-endian order.
        assert_eq!(
            &UserHandle::new(CHAIN, u64::MAX).to_bytes()[32..],
            &[255; 8]
        );
    }

    #[test]
    fn registration_labels_preserve_and_escape_the_entire_chain_id() {
        let url = request_url(
            "https://p/",
            &Request::Create {
                challenge: [0; 32],
                user: 42,
                chain_id: "demo#0123456789abcdef&x=+끝".into(),
                name: "a&b #+".into(),
            },
            "cb",
        );
        assert!(url.contains(
            "&name=a%26b%20%23%2B%20%C2%B7%20demo%230123456789abcdef%26x%3D%2B%EB%81%9D&cb="
        ));
    }

    #[test]
    fn old_account_only_handles_require_recreating_the_passkey() {
        let result = parse_result(
            r#"{"op":"get","authenticatorData":"AQ","clientDataJSON":"Ag","signature":"Aw","userHandle":"KgAAAAAAAAA"}"#,
        );
        assert!(result.is_err(), "old account-only handles must be refused");
        assert!(result.unwrap_err().contains("recreate"));
    }

    #[test]
    fn every_other_handle_length_is_refused() {
        for length in 0..=65 {
            if length == 40 {
                continue;
            }
            let json = serde_json::json!({
                "op": "get",
                "authenticatorData": "AQ",
                "clientDataJSON": "Ag",
                "signature": "Aw",
                "userHandle": B64.encode(vec![0; length]),
            });
            let error = parse_result(&json.to_string()).unwrap_err();
            assert!(error.contains("recreate"), "{length} bytes: {error}");
        }
    }

    fn msg() -> sdk::Msg {
        sdk::Msg {
            target: "identity".into(),
            payload: b"{\"set_name\":{\"name\":\"x\"}}".to_vec(),
        }
    }

    /// the fragment is the README's, field for field: b64url no padding,
    /// `user` = 32-byte chain hash followed by 8-byte LE account, `name`/`cb` percent-encoded.
    #[test]
    fn the_request_url_is_the_pages_contract() {
        let mut challenge = [0u8; 32];
        challenge[..3].copy_from_slice(&[1, 2, 3]);
        let url = request_url(
            "https://p/",
            &Request::Create {
                challenge,
                user: 42,
                chain_id: CHAIN.into(),
                name: "de mo".into(),
            },
            "http://127.0.0.1:9/",
        );
        let (page, fragment) = url.split_once('#').unwrap();
        assert_eq!(page, "https://p/");
        let params: Vec<&str> = fragment.split('&').collect();
        assert_eq!(params[0], "op=create");
        assert!(params[1].starts_with("challenge=AQID"), "{}", params[1]);
        assert_eq!(params[2], format!("user={HANDLE_42}"));
        assert_eq!(params[3], "name=de%20mo%20%C2%B7%20demo%23a1b2c3d4");
        assert_eq!(params[4], "cb=http%3A%2F%2F127.0.0.1%3A9%2F");

        let eth = request_url(
            "https://p/",
            &Request::Eth {
                message: vec![1, 2, 3],
            },
            "cb",
        );
        assert!(eth.ends_with("#op=eth&challenge=AQID&cb=cb"), "{eth}");
        let get = request_url("https://p/", &Request::Get { challenge }, "cb");
        assert!(get.contains("#op=get&challenge=AQID"), "{get}");
    }

    #[test]
    fn results_decode_and_a_failure_names_itself() {
        let registered = passkey_pubkey(&passkey(0x51));
        let create = parse_result(&format!(
            r#"{{"op":"create","credentialId":"AQID","publicKey":"{}","alg":-7,"attestationObject":"","clientDataJSON":""}}"#,
            B64.encode(&registered)
        ))
        .unwrap();
        assert_eq!(
            create,
            Outcome::Create {
                credential_id: vec![1, 2, 3],
                public_key: registered
            }
        );
        // a key the page did not compress is refused right here, before a
        // consent or a second touch is spent on it.
        assert!(
            parse_result(r#"{"op":"create","credentialId":"AQID","publicKey":"AgME","alg":-7}"#)
                .is_err()
        );
        let get = parse_result(
            r#"{"op":"get","credentialId":"AQID","authenticatorData":"AQ","clientDataJSON":"Ag","signature":"Aw","userHandle":"6zD6Woip0W_PPk0EWZGNZdwjPHgvY2dqMFHQVJ7xyIwqAAAAAAAAAA"}"#,
        )
        .unwrap();
        assert_eq!(
            get,
            Outcome::Get {
                authenticator_data: vec![1],
                client_data_json: vec![2],
                signature: vec![3],
                user_handle: Some(UserHandle::new(CHAIN, 42))
            }
        );
        let anonymous = parse_result(
            r#"{"op":"get","credentialId":"AQID","authenticatorData":"AQ","clientDataJSON":"Ag","signature":"Aw","userHandle":null}"#,
        )
        .unwrap();
        assert!(matches!(
            anonymous,
            Outcome::Get {
                user_handle: None,
                ..
            }
        ));
        let eth =
            parse_result(r#"{"op":"eth","address":"0xab","signature":"0x0102","message":"AQID"}"#)
                .unwrap();
        assert_eq!(
            eth,
            Outcome::Eth {
                address: "0xab".into(),
                signature: vec![1, 2],
                message: vec![1, 2, 3]
            }
        );
        let err = parse_result(r#"{"op":"get","error":"NotAllowedError","message":"cancelled"}"#)
            .unwrap_err();
        assert_eq!(err, "the get ceremony failed: NotAllowedError: cancelled");
        assert!(parse_result("nope").is_err());
        assert!(parse_result(r#"{"op":"get","authenticatorData":"AQ"}"#).is_err());
    }

    /// split a `callback_url` into the loopback port and the path the page
    /// (or a test peer) must hit.
    fn port_and_path(cb: &str) -> (u16, String) {
        let rest = cb.trim_start_matches("http://127.0.0.1:");
        let (port, path) = rest.split_once('/').unwrap();
        (port.parse().unwrap(), format!("/{path}"))
    }

    async fn send(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await.unwrap();
        answer
    }

    const REAL_BODY: &str = "result=%7B%22op%22%3A%22eth%22%2C%22address%22%3A%220xab%22%2C%22signature%22%3A%220x01%22%2C%22message%22%3A%22AQID%22%7D&x=y+z";

    fn post(path: &str, body: &str) -> String {
        format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// the page's delivery, as bytes on the wire: only a POST to the exact
    /// `/cb/<state>` path lands the result — a wrong path (someone else's
    /// process, a stray probe) is 404'd and the wait keeps going, and a
    /// right-path POST missing its `result` field is 400'd rather than
    /// aborting the ceremony.
    #[tokio::test]
    async fn the_listener_ignores_the_wrong_path_and_serves_the_real_post() {
        let listener = Listener::bind().await.unwrap();
        let (port, path) = port_and_path(&listener.callback_url());
        let served = tokio::spawn(listener.wait());

        // wrong path entirely (an unrelated local peer guessing at paths).
        let answer = send(port, "GET /favicon.ico HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");

        // a POST with the real body but the WRONG path — same as an attacker
        // who doesn't know this ceremony's state — must not land the result.
        let answer = send(port, &post("/cb/wrong-state", REAL_BODY)).await;
        assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");

        // right path, but a stray GET (a tab reload) — held, not landed.
        let answer = send(port, &format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n")).await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(answer.contains("Waiting"), "{answer}");

        // right path, malformed body — refused, but the wait keeps going.
        let answer = send(port, &post(&path, "not a form body")).await;
        assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");

        // right path, real body — this is the one that lands.
        let answer = send(port, &post(&path, REAL_BODY)).await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(answer.contains("return to ducktape"), "{answer}");

        let outcome = served.await.unwrap().unwrap();
        assert_eq!(
            outcome,
            Outcome::Eth {
                address: "0xab".into(),
                signature: vec![1],
                message: vec![1, 2, 3]
            }
        );
    }

    /// a passkey-origin frame decodes at the node with the passkey as its
    /// verified origin — the assertion faked exactly as an authenticator
    /// produces it, over the challenge the request named.
    #[test]
    fn a_passkey_signs_a_frame_as_its_origin() {
        let sk = passkey(3);
        let pubkey = passkey_pubkey(&sk);
        let (request, preimage) = passkey_frame_request(&pubkey, 7, &msg());
        let Request::Get { challenge } = &request else {
            panic!("a get")
        };
        assert_eq!(
            *challenge,
            keyscheme::webauthn_challenge(node::FRAME_NS, &preimage)
        );
        let (authenticator_data, client_data_json, signature) =
            passkey_assertion_parts(&sk, RP, node::FRAME_NS, &preimage);
        let outcome = Outcome::Get {
            authenticator_data,
            client_data_json,
            signature,
            user_handle: None,
        };
        let frame = passkey_frame(preimage, &outcome).unwrap();
        let (origin, decoded) = node::decode_frame(&frame).expect("the node verifies it");
        assert_eq!(origin, sdk::Origin::External(pubkey));
        assert_eq!(decoded.target, "identity");
        assert!(
            passkey_frame(
                Vec::new(),
                &Outcome::Create {
                    credential_id: vec![],
                    public_key: vec![]
                }
            )
            .is_err()
        );
    }

    /// a wallet-origin frame: touch 1 reveals the key, touch 2 signs the
    /// frame; the node recovers the same key from the frame's proof.
    #[test]
    fn a_wallet_reveals_its_key_then_signs_a_frame_as_its_origin() {
        let sk = eth_key(3);
        let reveal = reveal_message();
        assert!(reveal.starts_with(REVEAL_NS));
        let touch1 = Outcome::Eth {
            address: "0x".into(),
            signature: eth_sign_message(&sk, &reveal),
            message: reveal.clone(),
        };
        let pubkey = wallet_pubkey(&reveal, &touch1).unwrap();
        assert_eq!(pubkey, eth_pubkey(&sk));
        let other = Outcome::Eth {
            address: "0x".into(),
            signature: eth_sign_message(&sk, b"something else"),
            message: b"something else".to_vec(),
        };
        assert!(
            wallet_pubkey(&reveal, &other).is_err(),
            "a stale touch is refused"
        );

        let (request, preimage) = wallet_frame_request(&pubkey, 9, &msg());
        let Request::Eth { message } = &request else {
            panic!("an eth touch")
        };
        assert_eq!(
            *message,
            keyscheme::personal_message(node::FRAME_NS, &preimage)
        );
        let touch2 = Outcome::Eth {
            address: "0x".into(),
            signature: eth_proof(&sk, node::FRAME_NS, &preimage),
            message: message.clone(),
        };
        let frame = wallet_frame(preimage, &touch2).unwrap();
        let (origin, _) = node::decode_frame(&frame).expect("the node verifies it");
        assert_eq!(origin, sdk::Origin::External(pubkey));
    }

    /// QR login: the passkey's assertion over the device key's AddKey
    /// preimage IS the consent the identity module verifies, and the
    /// userHandle names the account to admit it into.
    #[test]
    fn a_login_assertion_is_the_add_key_consent() {
        let sk = passkey(2);
        let device_key = [7u8; 32];
        let request = login_request("chain-a", &device_key, 0, 11, 900);
        let preimage =
            identity::add_key_preimage("chain-a", KeyScheme::Ed25519, &device_key, 0, 11, 900);
        assert_eq!(
            request,
            Request::Get {
                challenge: keyscheme::webauthn_challenge(identity::IDENTITY_ADD_KEY_NS, &preimage)
            }
        );
        let (authenticator_data, client_data_json, signature) =
            passkey_assertion_parts(&sk, RP, identity::IDENTITY_ADD_KEY_NS, &preimage);
        let outcome = Outcome::Get {
            authenticator_data: authenticator_data.clone(),
            client_data_json: client_data_json.clone(),
            signature: signature.clone(),
            user_handle: Some(UserHandle::new("chain-a", 11)),
        };
        let (number, proof) = login_consent("chain-a", &outcome).unwrap();
        assert_eq!(number, 11);
        assert!(KeyScheme::Secp256r1.verify(
            &passkey_pubkey(&sk),
            identity::IDENTITY_ADD_KEY_NS,
            &preimage,
            &proof
        ));
        let anonymous = Outcome::Get {
            authenticator_data,
            client_data_json,
            signature,
            user_handle: None,
        };
        assert!(
            login_consent("chain-a", &anonymous)
                .unwrap_err()
                .contains("no userHandle")
        );
    }

    /// a login: the assertion is the consent on THIS device's `AddKey`, and the
    /// builder finds the passkey that signed it among the account's keys.
    #[test]
    fn login_builds_the_add_key_the_passkey_consented_to() {
        let device_key = [7u8; 32];
        let (other, mine) = (passkey(2), passkey(3));
        let key = |scheme, pubkey: Vec<u8>| identity::KeyView {
            scheme,
            pubkey,
            label: None,
            added_at: 0,
        };
        let account = identity::AccountView {
            control: identity::Control::Keys,
            number: 11,
            name: "alice".into(),
            keys: vec![
                key(KeyScheme::Ed25519, vec![1; 32]),
                key(KeyScheme::Secp256r1, passkey_pubkey(&other)),
                key(KeyScheme::Secp256r1, passkey_pubkey(&mine)),
            ],
            avatar: None,
            bio: None,
            updated_at: 0,
        };
        let preimage =
            identity::add_key_preimage("chain-a", KeyScheme::Ed25519, &device_key, 4, 11, 900);
        let (a, c, s) =
            passkey_assertion_parts(&mine, RP, identity::IDENTITY_ADD_KEY_NS, &preimage);
        let proof = keyscheme::webauthn_proof(&a, &c, &s);
        let msg = login_add_key(
            "chain-a",
            &device_key,
            4,
            &account,
            Some("laptop".into()),
            proof.clone(),
            900,
        )
        .unwrap();
        assert_eq!(
            msg,
            identity::IdentityMsg::AddKey {
                scheme: KeyScheme::Ed25519,
                label: Some("laptop".into()),
                authorizer: identity::Authorizer {
                    key: passkey_pubkey(&mine),
                    account: 11,
                    expires_at: 900,
                    proof: proof.clone(),
                },
            },
            "the passkey that signed is the authorizer"
        );
        // another generation, or a passkey off the account: no signer.
        assert!(login_add_key("chain-a", &device_key, 5, &account, None, proof, 900).is_err());
        let (a, c, s) =
            passkey_assertion_parts(&passkey(4), RP, identity::IDENTITY_ADD_KEY_NS, &preimage);
        let foreign = keyscheme::webauthn_proof(&a, &c, &s);
        assert!(login_add_key("chain-a", &device_key, 4, &account, None, foreign, 900).is_err());
    }

    #[tokio::test]
    async fn dropping_a_pending_accept_closes_the_listener() {
        let listener = Listener::bind().await.unwrap();
        let address = listener.listener.local_addr().unwrap();
        let mut wait = Box::pin(listener.wait());
        std::future::poll_fn(|cx| {
            assert!(wait.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(wait);
        assert!(TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn dropping_a_partial_request_closes_the_connection() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        peer.write_all(b"POST /cb/state HTTP/1.1\r\nContent-Length: 20\r\n\r\nx")
            .await
            .unwrap();
        // Readiness is the event proving the partial request reached this socket.
        stream.readable().await.unwrap();
        let mut wait = Box::pin(serve_one(stream, "/cb/state"));
        std::future::poll_fn(|cx| {
            assert!(wait.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(wait);
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_callback_ceremony_error_returns_the_reason() {
        let listener = Listener::bind().await.unwrap();
        let (port, path) = port_and_path(&listener.callback_url());
        let served = tokio::spawn(listener.wait());
        let body = format!(
            "result={}",
            url_encode(r#"{"op":"get","error":"cancelled","message":"browser closed"}"#)
        );
        let answer = send(port, &post(&path, &body)).await;
        assert!(answer.starts_with("HTTP/1.1 200"));
        let error = served.await.unwrap().unwrap_err();
        assert!(error.contains("browser closed"), "{error}");
    }

    #[test]
    fn form_decoding_handles_plus_and_escapes() {
        assert_eq!(form_decode(b"a+b%20c%zz"), b"a b c%zz");
        assert_eq!(
            form_field(b"x=1&result=%7B%7D", "result").as_deref(),
            Some("{}")
        );
        assert_eq!(form_field(b"x=1", "result"), None);
    }

    /// A finite relay server. Every accepted request is fully consumed.
    async fn fake_relay(
        absent: usize,
        json: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for served in 0..=absent {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (method, path, _) = read_request(&mut stream).await.unwrap();
                assert_eq!(method, "GET");
                assert!(path.starts_with("/r/"));
                let is_the_answer = served == absent;
                let response = match is_the_answer {
                    true => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
                        json.len()
                    ),
                    false => "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_string(),
                };
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (base, server)
    }

    #[test]
    fn a_relay_id_is_43_url_safe_chars_and_names_the_callback() {
        let relay = Relay::at("https://auth.example/");
        assert_eq!(relay.id.len(), 43);
        assert!(
            relay
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(
            relay.callback_url(),
            format!("https://auth.example/r/{}", relay.id)
        );
        assert_ne!(Relay::at("x").id, Relay::at("x").id);
    }

    #[tokio::test]
    async fn a_relay_waits_through_204s_and_takes_the_first_200() {
        let (base, server) = fake_relay(
            2,
            r#"{"op":"get","credentialId":"AQ","authenticatorData":"AQ","clientDataJSON":"AQ","signature":"AQ","userHandle":"6zD6Woip0W_PPk0EWZGNZdwjPHgvY2dqMFHQVJ7xyIwqAAAAAAAAAA"}"#,
        ).await;
        let outcome = Relay::at(&base)
            .wait(Duration::from_secs(20))
            .await
            .unwrap();
        server.await.unwrap();
        assert!(matches!(
            outcome,
            Outcome::Get {
                user_handle: Some(handle),
                ..
            } if handle == UserHandle::new(CHAIN, 42)
        ));
    }

    #[tokio::test]
    async fn dropping_a_relay_request_closes_the_pending_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay = Relay::at(&format!("http://{}/", listener.local_addr().unwrap()));
        let wait = tokio::spawn(relay.wait(Duration::from_secs(60)));
        let (mut peer, _) = listener.accept().await.unwrap();
        read_request(&mut peer).await.unwrap();
        wait.abort();
        assert!(wait.await.unwrap_err().is_cancelled());
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_relay_deadline_covers_pending_headers_and_body() {
        for response in [
            "",
            "HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{",
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let relay = Relay::at(&format!("http://{}/", listener.local_addr().unwrap()));
            let wait = tokio::spawn(relay.wait(Duration::from_secs(5)));
            let (mut peer, _) = listener.accept().await.unwrap();
            read_request(&mut peer).await.unwrap();
            peer.write_all(response.as_bytes()).await.unwrap();
            // Pause only after the real socket exchange, then expire the entire ceremony.
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(5)).await;
            let error = wait.await.unwrap().unwrap_err();
            assert!(error.contains("did not answer"), "{error}");
            let mut byte = [0];
            let closed = match peer.read(&mut byte).await {
                Ok(read) => read == 0,
                Err(error) => error.kind() == std::io::ErrorKind::ConnectionReset,
            };
            assert!(closed, "deadline must close the pending connection");
            tokio::time::resume();
        }
    }

    #[tokio::test]
    async fn a_relay_deadline_stops_the_delay_before_another_poll() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay = Relay::at(&format!("http://{}/", listener.local_addr().unwrap()));
        let (progress, mut polls) = tokio::sync::mpsc::unbounded_channel();
        let wait = tokio::spawn(async move {
            relay
                .wait_reporting(Duration::from_secs(1), move |left| {
                    progress.send(left).unwrap();
                })
                .await
        });
        let (mut peer, _) = listener.accept().await.unwrap();
        read_request(&mut peer).await.unwrap();
        peer.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        drop(peer);
        assert!(polls.recv().await.is_some());
        tokio::time::pause();
        let error = wait.await.unwrap().unwrap_err();
        assert!(error.contains("did not answer"), "{error}");
        assert_eq!(polls.recv().await, None, "no poll after the deadline");
    }

    #[test]
    fn a_countdown_is_minutes_and_two_digit_seconds() {
        assert_eq!(countdown(Duration::from_secs(300)), "5:00");
        assert_eq!(countdown(Duration::from_millis(61_900)), "1:01");
        assert_eq!(countdown(Duration::ZERO), "0:00");
    }

    /// the poller hears how long is left before each poll, shrinking.
    #[tokio::test]
    async fn a_relay_reports_the_time_left_before_each_poll() {
        let (base, server) = fake_relay(
            2,
            r#"{"op":"get","credentialId":"AQ","authenticatorData":"AQ","clientDataJSON":"AQ","signature":"AQ","userHandle":"6zD6Woip0W_PPk0EWZGNZdwjPHgvY2dqMFHQVJ7xyIwqAAAAAAAAAA"}"#,
        ).await;
        let mut seen = Vec::new();
        Relay::at(&base)
            .wait_reporting(Duration::from_secs(60), |left| seen.push(left))
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert!(seen[0] > seen[1] && seen[1] > seen[2], "{seen:?}");
    }

    /// a real ceremony URL renders inside an 80-column terminal: half-block
    /// rows, every line the same width, a quiet zone around the code.
    #[test]
    fn a_terminal_qr_of_a_ceremony_url_fits_eighty_columns() {
        let relay = Relay::new();
        let url = request_url(
            AUTH_PAGE,
            &Request::Create {
                challenge: [9u8; 32],
                user: 1234,
                chain_id: CHAIN.into(),
                name: "byeongsu".into(),
            },
            &relay.callback_url(),
        );
        let qr = terminal_qr(&url).unwrap();
        let lines: Vec<&str> = qr.lines().collect();
        let width = lines[0].chars().count();
        assert!(lines.iter().all(|l| l.chars().count() == width));
        assert!((30..=48).contains(&lines.len()), "{} lines", lines.len());
        assert!(width <= 80, "{width} columns");
        assert!(
            qr.chars()
                .all(|c| matches!(c, '█' | '▀' | '▄' | ' ' | '\n'))
        );
    }
}
