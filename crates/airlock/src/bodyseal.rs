//! Broker↔enclave body AEAD. Requests are one sealed blob; responses are a
//! sealed CHUNK STREAM: `[16B stream salt]` then repeated `[u32 BE len][ct]`,
//! nonce = `[4B zero ‖ u64 BE counter]` under a per-stream key — position is
//! authenticated, so reordering, replay, or splicing across streams fails to
//! open, and the mandatory `Final` marker makes truncation detectable. The
//! stream key ALSO binds the request blob's nonce, so an authentic response
//! captured for one request cannot be replayed as the answer to another. A
//! sealed session seals EVERY request, an empty body included —
//! `seal_request`/`open_request` AEAD-seal an empty plaintext into a
//! non-empty blob (the tag alone) just as they do a full one — so even a
//! bodyless GET carries a real nonce and stays bound; the enclave refuses any
//! sealed-session request that arrives without a sealed body (`server.rs`).
//! Path hosts (the publisher node outside the enclave, any relay) see
//! ciphertext, and a stolen bearer alone cannot produce a sealable body; the
//! enclave dedupes request nonces per sub.
//!
//! The request AEAD also binds the HTTP method and path+query as associated
//! data (`request_aad`) — a relay holding a live bearer plus a captured sealed
//! blob cannot redirect it to a different method or path; the tag fails to
//! open under mismatched AAD. Headers stay UNBOUND: `proxy_inner`'s denylist
//! (server.rs) is the only header rule, and forwarding the rest verbatim is
//! by design.

use anyhow::{Context, Result, bail};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};
use rand_core::{OsRng, RngCore};

use crate::aead;
use crate::handshake::SessionKeys;

/// Marks a sealed-body request; the enclave refuses plaintext on sealed
/// sessions and refuses this header on plaintext sessions.
pub const SEAL_HEADER: &str = "x-airlock-body-seal";
pub const SEAL_V1: &str = "v1";

const REQ_LABEL: &[u8] = b"airlock-body-req-v1";
const STREAM_LABEL: &[u8] = b"airlock-body-stream-v1";
const SALT_LEN: usize = 16;
/// Plaintext chunk framing: one marker byte, then payload. A `MARK_FINAL`
/// payload is empty on a completed stream and carries a refusal token when
/// the enclave gave up after the head was already committed (a signing
/// pipeline that refuses minutes in); the opener tells the two apart.
const MARK_HEAD: u8 = 0x02;
const MARK_DATA: u8 = 0x00;
const MARK_FINAL: u8 = 0x01;
/// Sanity ceiling on a single sealed chunk (plaintext side is bounded by the
/// gateway frame codec anyway).
const MAX_CHUNK_CT: usize = 2 * 1024 * 1024;

fn request_key(keys: &SessionKeys) -> [u8; 32] {
    aead::hkdf32(&keys.body, REQ_LABEL)
}

/// Canonical AAD binding a sealed request to the HTTP request line it is sent
/// as: `METHOD\n/path?query`. The caller and the gateway must derive this from
/// the same method and path+query or the AEAD tag refuses to open.
pub fn request_aad(method: &str, path_and_query: &str) -> Vec<u8> {
    format!("{method}\n{path_and_query}").into_bytes()
}

/// Seal a whole request body as one blob (`aead::seal` envelope), bound to
/// `aad` (see `request_aad`).
pub fn seal_request(keys: &SessionKeys, aad: &[u8], body: &[u8]) -> Vec<u8> {
    aead::seal(&request_key(keys), aad, body)
}

pub fn open_request(keys: &SessionKeys, aad: &[u8], blob: &[u8]) -> Result<Vec<u8>> {
    aead::open(&request_key(keys), aad, blob).context("sealed request body")
}

/// The request blob's unique nonce — the response-stream binding value (and
/// the enclave's per-sub replay-dedup key). Empty input (a bodyless request)
/// yields an empty binding.
pub fn request_binding(sealed_blob: &[u8]) -> Vec<u8> {
    sealed_blob
        .get(..12)
        .map(<[u8]>::to_vec)
        .unwrap_or_default()
}

fn stream_cipher(keys: &SessionKeys, salt: &[u8; SALT_LEN], binding: &[u8]) -> ChaCha20Poly1305 {
    let mut label = STREAM_LABEL.to_vec();
    label.extend_from_slice(binding);
    let key = aead::hkdf32_salted(&keys.body, salt, &label);
    ChaCha20Poly1305::new(Key::from_slice(&key))
}

fn counter_nonce(counter: u64) -> Nonce {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::from(nonce)
}

fn frame(ct: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + ct.len());
    out.extend_from_slice(&(ct.len() as u32).to_be_bytes());
    out.extend_from_slice(&ct);
    out
}

/// Enclave side: seals the response as it streams. Emit the salt prefix
/// first, then `seal_head`, any number of `seal_chunk`s, and ALWAYS
/// `seal_final` — the opener treats a missing final marker as truncation.
pub struct StreamSealer {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl StreamSealer {
    /// `binding` = `request_binding(sealed request blob)` — ties this response
    /// to the one request it answers (empty for a bodyless request).
    pub fn new(keys: &SessionKeys, binding: &[u8]) -> (Self, Vec<u8>) {
        let mut salt = [0u8; SALT_LEN];
        OsRng.fill_bytes(&mut salt);
        (
            Self {
                cipher: stream_cipher(keys, &salt, binding),
                counter: 0,
            },
            salt.to_vec(),
        )
    }

    fn seal_marked(&mut self, mark: u8, payload: &[u8]) -> Vec<u8> {
        let mut pt = Vec::with_capacity(1 + payload.len());
        pt.push(mark);
        pt.extend_from_slice(payload);
        let ct = self
            .cipher
            .encrypt(&counter_nonce(self.counter), pt.as_slice())
            .expect("ChaCha20-Poly1305 encryption does not fail on valid inputs");
        self.counter += 1;
        frame(ct)
    }

    /// The inner content-type, sealed as the first chunk.
    pub fn seal_head(&mut self, content_type: &str) -> Vec<u8> {
        self.seal_marked(MARK_HEAD, content_type.as_bytes())
    }

    pub fn seal_chunk(&mut self, data: &[u8]) -> Vec<u8> {
        self.seal_marked(MARK_DATA, data)
    }

    /// An empty, authenticated data chunk: liveness on a stream whose next
    /// bytes are minutes away (a notary wait), so every hop's idle deadline
    /// sees progress. Opens as an empty [`OpenedItem::Data`], which a reader
    /// appends as nothing.
    pub fn seal_keepalive(&mut self) -> Vec<u8> {
        self.seal_chunk(&[])
    }

    /// Authenticated end-of-stream. MUST be sent; its absence = truncation.
    pub fn seal_final(&mut self) -> Vec<u8> {
        self.seal_marked(MARK_FINAL, &[])
    }

    /// Authenticated end-of-stream that REFUSES: the head is already out, so
    /// the outcome rides in the Final marker as the refusal's token. Opens as
    /// [`OpenedItem::Refused`]; the stream is finished either way.
    pub fn seal_refused(&mut self, reason: &str) -> Vec<u8> {
        self.seal_marked(MARK_FINAL, reason.as_bytes())
    }
}

/// What the opener yields per sealed chunk.
#[derive(Debug, PartialEq, Eq)]
pub enum OpenedItem {
    /// The inner content-type (always the first item).
    Head(String),
    Data(Vec<u8>),
    /// Authenticated EOF. Anything after it is an error.
    Final,
    /// Authenticated EOF that withdraws the stream: the enclave refused after
    /// its head was committed, and this is the refusal's token. The data
    /// before it is not an answer.
    Refused(String),
}

/// Broker side: incremental parser over arbitrary byte splits.
pub struct StreamOpener {
    keys: SessionKeys,
    binding: Vec<u8>,
    cipher: Option<ChaCha20Poly1305>,
    buf: Vec<u8>,
    counter: u64,
    finished: bool,
}

impl StreamOpener {
    /// `binding` must equal the sealer's — the requester passes
    /// `request_binding(<the sealed blob it sent>)`.
    pub fn new(keys: &SessionKeys, binding: &[u8]) -> Self {
        Self {
            keys: keys.clone(),
            binding: binding.to_vec(),
            cipher: None,
            buf: Vec::new(),
            counter: 0,
            finished: false,
        }
    }

    /// Feed bytes as they arrive; returns every item completed by this feed.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<OpenedItem>> {
        if self.finished && !bytes.is_empty() {
            bail!("sealed stream: data after the final marker");
        }
        self.buf.extend_from_slice(bytes);
        let mut items = Vec::new();
        loop {
            if self.cipher.is_none() {
                if self.buf.len() < SALT_LEN {
                    break;
                }
                let salt: [u8; SALT_LEN] = self.buf[..SALT_LEN].try_into().unwrap();
                self.buf.drain(..SALT_LEN);
                self.cipher = Some(stream_cipher(&self.keys, &salt, &self.binding));
            }
            if self.buf.len() < 4 {
                break;
            }
            let len = u32::from_be_bytes(self.buf[..4].try_into().unwrap()) as usize;
            if len > MAX_CHUNK_CT {
                bail!("sealed stream: chunk of {len} bytes exceeds the ceiling");
            }
            if self.buf.len() < 4 + len {
                break;
            }
            let ct: Vec<u8> = self.buf[4..4 + len].to_vec();
            self.buf.drain(..4 + len);
            if self.finished {
                bail!("sealed stream: data after the final marker");
            }
            let cipher = self.cipher.as_ref().expect("cipher set above");
            let pt = cipher
                .decrypt(&counter_nonce(self.counter), ct.as_slice())
                .map_err(|_| anyhow::anyhow!(
                    "sealed stream: chunk {} failed to open (tampered, reordered, or wrong key)",
                    self.counter
                ))?;
            self.counter += 1;
            let (mark, payload) = pt.split_first().context("sealed stream: empty plaintext")?;
            let expected_head = self.counter == 1;
            match *mark {
                MARK_HEAD if expected_head => {
                    items.push(OpenedItem::Head(
                        String::from_utf8(payload.to_vec()).context("inner content-type utf8")?,
                    ));
                }
                MARK_HEAD => bail!("sealed stream: head chunk after the first position"),
                MARK_DATA if !expected_head => items.push(OpenedItem::Data(payload.to_vec())),
                MARK_DATA => bail!("sealed stream: data chunk before the head"),
                MARK_FINAL => {
                    self.finished = true;
                    let completed = payload.is_empty();
                    if completed {
                        items.push(OpenedItem::Final);
                    } else {
                        items.push(OpenedItem::Refused(
                            String::from_utf8(payload.to_vec()).context("refusal token utf8")?,
                        ));
                    }
                }
                other => bail!("sealed stream: unknown chunk marker {other}"),
            }
        }
        Ok(items)
    }

    /// True once the authenticated final marker arrived. A stream that ends
    /// without it was TRUNCATED.
    pub fn finished(&self) -> bool {
        self.finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handshake;
    use crate::seal::SealKeypair;

    fn keys() -> SessionKeys {
        let enclave = SealKeypair::generate();
        let (eph, client) = handshake::client_handshake(&enclave.public_bytes());
        let server = handshake::enclave_session_keys(&enclave, &eph);
        assert_eq!(client.body, server.body);
        client
    }

    fn sealed_stream(keys: &SessionKeys, chunks: &[&[u8]]) -> Vec<u8> {
        let (mut sealer, mut wire) = StreamSealer::new(keys, b"");
        wire.extend(sealer.seal_head("text/event-stream"));
        for chunk in chunks {
            wire.extend(sealer.seal_chunk(chunk));
        }
        wire.extend(sealer.seal_final());
        wire
    }

    #[test]
    fn round_trips_across_arbitrary_feed_splits() {
        let keys = keys();
        let wire = sealed_stream(&keys, &[b"data: a\n\n", b"data: b\n\n"]);
        // Feed one byte at a time — the parser must reassemble.
        let mut opener = StreamOpener::new(&keys, b"");
        let mut items = Vec::new();
        for byte in &wire {
            items.extend(opener.feed(std::slice::from_ref(byte)).unwrap());
        }
        assert_eq!(
            items,
            vec![
                OpenedItem::Head("text/event-stream".into()),
                OpenedItem::Data(b"data: a\n\n".to_vec()),
                OpenedItem::Data(b"data: b\n\n".to_vec()),
                OpenedItem::Final,
            ]
        );
        assert!(opener.finished());
    }

    #[test]
    fn a_keepalive_opens_as_empty_data_and_a_refused_final_carries_its_token() {
        let keys = keys();
        let (mut sealer, mut wire) = StreamSealer::new(&keys, b"");
        wire.extend(sealer.seal_head("application/zstd"));
        wire.extend(sealer.seal_keepalive());
        wire.extend(sealer.seal_refused("notary_rejected"));
        let mut opener = StreamOpener::new(&keys, b"");
        let items = opener.feed(&wire).unwrap();
        assert_eq!(
            items,
            vec![
                OpenedItem::Head("application/zstd".into()),
                OpenedItem::Data(Vec::new()),
                OpenedItem::Refused("notary_rejected".into()),
            ]
        );
        assert!(
            opener.finished(),
            "a refused stream is finished, not truncated"
        );
        assert!(opener.feed(b"x").is_err(), "nothing may follow a refusal");
    }

    #[test]
    fn request_blob_round_trips_and_rejects_tamper() {
        let keys = keys();
        let aad = request_aad("POST", "/v1/messages");
        let blob = seal_request(&keys, &aad, br#"{"model":"claude"}"#);
        assert_eq!(
            open_request(&keys, &aad, &blob).unwrap(),
            br#"{"model":"claude"}"#
        );
        let mut bad = blob.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(open_request(&keys, &aad, &bad).is_err());
    }

    #[test]
    fn request_blob_refuses_a_different_method_or_path() {
        let keys = keys();
        let aad = request_aad("POST", "/v1/messages");
        let blob = seal_request(&keys, &aad, br#"{"model":"claude"}"#);
        assert!(open_request(&keys, &request_aad("GET", "/v1/messages"), &blob).is_err());
        assert!(open_request(&keys, &request_aad("POST", "/v1/other"), &blob).is_err());
        assert!(open_request(&keys, &aad, &blob).is_ok());
    }

    #[test]
    fn reordered_chunks_fail_to_open() {
        let keys = keys();
        let (mut sealer, salt) = StreamSealer::new(&keys, b"");
        let head = sealer.seal_head("t");
        let c1 = sealer.seal_chunk(b"one");
        let c2 = sealer.seal_chunk(b"two");
        let fin = sealer.seal_final();
        // Swap c1 and c2: position is authenticated by the counter nonce.
        let mut wire = salt;
        wire.extend(head);
        wire.extend(c2);
        wire.extend(c1);
        wire.extend(fin);
        let mut opener = StreamOpener::new(&keys, b"");
        assert!(opener.feed(&wire).is_err());
    }

    #[test]
    fn truncation_is_visible_as_missing_final() {
        let keys = keys();
        let wire = sealed_stream(&keys, &[b"payload"]);
        // Drop the final sealed chunk (17 bytes ct + 4 len = last 21+ bytes;
        // compute exactly: final = 1 marker byte + 16 tag = 17 ct + 4 frame).
        let truncated = &wire[..wire.len() - (4 + 1 + 16)];
        let mut opener = StreamOpener::new(&keys, b"");
        let items = opener.feed(truncated).unwrap();
        assert!(items.iter().all(|item| !matches!(item, OpenedItem::Final)));
        assert!(
            !opener.finished(),
            "a stream without the final marker is TRUNCATED"
        );
    }

    #[test]
    fn wrong_key_and_cross_stream_splice_fail() {
        let keys_a = keys();
        let keys_b = keys();
        let wire = sealed_stream(&keys_a, &[b"secret"]);
        assert!(StreamOpener::new(&keys_b, b"").feed(&wire).is_err());

        // Splice: a chunk from stream A inserted into stream B fails (per-
        // stream salt -> different key), even at the same counter position.
        let (mut sealer_b, salt_b) = StreamSealer::new(&keys_a, b"");
        let head_b = sealer_b.seal_head("t");
        let (mut sealer_c, _salt_c) = StreamSealer::new(&keys_a, b"");
        let _head_c = sealer_c.seal_head("t");
        let foreign = sealer_c.seal_chunk(b"foreign");
        let mut wire = salt_b;
        wire.extend(head_b);
        wire.extend(foreign);
        let mut opener = StreamOpener::new(&keys_a, b"");
        assert!(opener.feed(&wire).is_err());
    }

    #[test]
    fn a_response_bound_to_one_request_cannot_answer_another() {
        // The replay the review flagged: an authentic sealed response for
        // request A, returned verbatim for request B, must fail to open.
        let keys = keys();
        let aad = request_aad("POST", "/v1/messages");
        let blob_a = seal_request(&keys, &aad, b"request A");
        let blob_b = seal_request(&keys, &aad, b"request B");
        let (mut sealer, salt) = StreamSealer::new(&keys, &request_binding(&blob_a));
        let mut wire = salt;
        wire.extend(sealer.seal_head("t"));
        wire.extend(sealer.seal_chunk(b"answer for A"));
        wire.extend(sealer.seal_final());

        let mut opener_b = StreamOpener::new(&keys, &request_binding(&blob_b));
        assert!(
            opener_b.feed(&wire).is_err(),
            "replayed response must not open for B"
        );
        let mut opener_a = StreamOpener::new(&keys, &request_binding(&blob_a));
        assert!(
            opener_a.feed(&wire).is_ok(),
            "the bound requester still opens it"
        );
    }

    #[test]
    fn data_after_final_is_refused() {
        let keys = keys();
        let (mut sealer, salt) = StreamSealer::new(&keys, b"");
        let head = sealer.seal_head("t");
        let fin = sealer.seal_final();
        let extra = sealer.seal_chunk(b"zombie");
        let mut wire = salt;
        wire.extend(head);
        wire.extend(fin);
        wire.extend(extra);
        let mut opener = StreamOpener::new(&keys, b"");
        assert!(opener.feed(&wire).is_err());
    }
}
