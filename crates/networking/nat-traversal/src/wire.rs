use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use arrayvec::ArrayVec;
use borsh::{BorshDeserialize, BorshSchema, BorshSerialize};

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize, BorshSchema,
)]
pub struct NodeKey(pub [u8; 32]);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    BindRequest {
        from: NodeKey,
    },
    BindResponse {
        reflexive: SocketAddr,
        /// A stateless return-routability proof for `reflexive`
        /// (`CookieKey::mint`): the caller echoes this back in `Register` to
        /// prove it actually received a coordinator reply at that source,
        /// rather than having a first-seen mapping planted by a spoofed
        /// source address.
        cookie: [u8; 32],
    },
    Register {
        key: NodeKey,
        /// Required only when the coordinator holds no LIVE mapping for
        /// `key` (first-seen or expired) — see
        /// `CookieKey::verify`. A same-source refresh of a live mapping
        /// ignores this field.
        cookie: [u8; 32],
    },
    /// Rebind re-advertisement: republish the sender's reflexive under a
    /// strictly-monotonic `nonce`. Unlike `Register` (an unconditional nonce-0
    /// boot baseline), the coordinator applies the `AdvertBook` staleness guard
    /// so a replayed/reordered equal-or-lower nonce cannot roll a fresh mapping
    /// back. The reflexive stored is still the coordinator-observed source, never
    /// this datagram's self-report — the nonce only orders adverts.
    Readvertise {
        key: NodeKey,
        nonce: u64,
        /// Required only when the coordinator holds no LIVE mapping for
        /// `key` (first-seen or expired) — same rule as `Register::cookie`,
        /// see `CookieKey::verify`. A same-source rebind of a live mapping
        /// ignores this field.
        cookie: [u8; 32],
    },
    Lookup {
        key: NodeKey,
    },
    LookupResponse {
        key: NodeKey,
        reflexive: Option<SocketAddr>,
    },
    PunchSync {
        peer: NodeKey,
        peer_reflexive: SocketAddr,
    },
    Punch {
        from: NodeKey,
    },
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("buffer too short")]
    Short,
    #[error("bad tag {0}")]
    BadTag(u8),
    #[error("bad address encoding")]
    BadAddr,
    #[error("trailing bytes after message")]
    Trailing,
    #[error("bad crypto encoding")]
    BadCrypto,
    #[error("auth envelope inner is not a request")]
    NotARequest,
}

const TAG_BIND_REQ: u8 = 1;
const TAG_BIND_RESP: u8 = 2;
const TAG_REGISTER: u8 = 3;
const TAG_LOOKUP: u8 = 4;
const TAG_LOOKUP_RESP: u8 = 5;
const TAG_PUNCH_SYNC: u8 = 6;
const TAG_PUNCH: u8 = 7;
const TAG_READVERTISE: u8 = 8;
const TAG_AUTH_REQUEST: u8 = 9;
// UDP tags live below 16. Tags >= 16 belong to the TCP relay frames
// (`relay.rs`) and are NEVER valid here: the PoP signing namespace is shared
// between UDP requests and the relay intro, so byte-level disjointness of the
// two encodings is load-bearing — a signature minted over one shape must not
// verify as the other.

// The tiny put/Reader encoding helpers are pub(crate) so the TCP relay frames
// (relay.rs) share ONE encoding idiom with the UDP messages instead of
// duplicating it.
pub(crate) fn put<const CAP: usize>(out: &mut ArrayVec<u8, CAP>, bytes: &[u8]) {
    out.try_extend_from_slice(bytes)
        .expect("wire buffer capacity covers every message");
}

pub(crate) fn put_key<const CAP: usize>(out: &mut ArrayVec<u8, CAP>, key: &NodeKey) {
    put(out, &key.0);
}

pub(crate) fn put_cookie<const CAP: usize>(out: &mut ArrayVec<u8, CAP>, cookie: &[u8; 32]) {
    put(out, cookie);
}

#[cfg(feature = "runtime")]
pub(crate) fn put_u16<const CAP: usize>(out: &mut ArrayVec<u8, CAP>, value: u16) {
    put(out, &value.to_be_bytes());
}

pub(crate) fn put_u64<const CAP: usize>(out: &mut ArrayVec<u8, CAP>, value: u64) {
    put(out, &value.to_be_bytes());
}

fn put_addr<const CAP: usize>(out: &mut ArrayVec<u8, CAP>, addr: &SocketAddr) {
    match addr.ip() {
        IpAddr::V4(ip) => {
            out.push(4);
            put(out, &ip.octets());
        }
        IpAddr::V6(ip) => {
            out.push(6);
            put(out, &ip.octets());
        }
    }
    put(out, &addr.port().to_be_bytes());
}

pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    /// Bytes not yet consumed — the whole-buffer trailing-garbage check.
    #[cfg(feature = "runtime")]
    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.pos.checked_add(n).ok_or(WireError::Short)?;
        if end > self.buf.len() {
            return Err(WireError::Short);
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    pub(crate) fn key(&mut self) -> Result<NodeKey, WireError> {
        let s = self.take(32)?;
        let mut k = [0u8; 32];
        k.copy_from_slice(s);
        Ok(NodeKey(k))
    }
    pub(crate) fn cookie(&mut self) -> Result<[u8; 32], WireError> {
        let s = self.take(32)?;
        let mut c = [0u8; 32];
        c.copy_from_slice(s);
        Ok(c)
    }
    #[cfg(feature = "runtime")]
    pub(crate) fn u16(&mut self) -> Result<u16, WireError> {
        let s = self.take(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }
    pub(crate) fn u64(&mut self) -> Result<u64, WireError> {
        let s = self.take(8)?;
        let mut b = [0u8; 8];
        b.copy_from_slice(s);
        Ok(u64::from_be_bytes(b))
    }
    fn addr(&mut self) -> Result<SocketAddr, WireError> {
        let fam = self.take(1)?[0];
        let ip = match fam {
            4 => {
                let o = self.take(4)?;
                IpAddr::V4(Ipv4Addr::new(o[0], o[1], o[2], o[3]))
            }
            6 => {
                let o = self.take(16)?;
                let mut b = [0u8; 16];
                b.copy_from_slice(o);
                IpAddr::V6(Ipv6Addr::from(b))
            }
            _ => return Err(WireError::BadAddr),
        };
        let p = self.take(2)?;
        let port = u16::from_be_bytes([p[0], p[1]]);
        Ok(SocketAddr::new(ip, port))
    }
    pub(crate) fn sig(&mut self) -> Result<commonware_cryptography::ed25519::Signature, WireError> {
        use commonware_codec::DecodeExt as _;
        let s = self.take(64)?;
        commonware_cryptography::ed25519::Signature::decode(s).map_err(|_| WireError::BadCrypto)
    }
    pub(crate) fn pubkey(
        &mut self,
    ) -> Result<commonware_cryptography::ed25519::PublicKey, WireError> {
        use commonware_codec::DecodeExt as _;
        let s = self.take(32)?;
        commonware_cryptography::ed25519::PublicKey::decode(s).map_err(|_| WireError::BadCrypto)
    }
}

impl Msg {
    /// Largest encoded bare message — a `Readvertise` (tag + key + 8-byte
    /// nonce + 32-byte cookie). This fixed upper bound lets the hot UDP loop
    /// encode replies on its stack instead of allocating per datagram.
    pub const MAX_ENCODED_LEN: usize = 1 + 32 + 8 + 32;

    /// Encode into a stack-backed, fixed-capacity vector.
    pub fn encode_inline(&self) -> ArrayVec<u8, { Self::MAX_ENCODED_LEN }> {
        let mut out = ArrayVec::new();
        self.write(&mut out);
        out
    }

    pub fn encode(&self) -> Vec<u8> {
        self.encode_inline().into_iter().collect()
    }

    fn write<const CAP: usize>(&self, out: &mut ArrayVec<u8, CAP>) {
        match self {
            Msg::BindRequest { from } => {
                out.push(TAG_BIND_REQ);
                put_key(out, from);
            }
            Msg::BindResponse { reflexive, cookie } => {
                out.push(TAG_BIND_RESP);
                put_addr(out, reflexive);
                put_cookie(out, cookie);
            }
            Msg::Register { key, cookie } => {
                out.push(TAG_REGISTER);
                put_key(out, key);
                put_cookie(out, cookie);
            }
            Msg::Readvertise { key, nonce, cookie } => {
                out.push(TAG_READVERTISE);
                put_key(out, key);
                put_u64(out, *nonce);
                put_cookie(out, cookie);
            }
            Msg::Lookup { key } => {
                out.push(TAG_LOOKUP);
                put_key(out, key);
            }
            Msg::LookupResponse { key, reflexive } => {
                out.push(TAG_LOOKUP_RESP);
                put_key(out, key);
                match reflexive {
                    Some(a) => {
                        out.push(1);
                        put_addr(out, a);
                    }
                    None => out.push(0),
                }
            }
            Msg::PunchSync {
                peer,
                peer_reflexive,
            } => {
                out.push(TAG_PUNCH_SYNC);
                put_key(out, peer);
                put_addr(out, peer_reflexive);
            }
            Msg::Punch { from } => {
                out.push(TAG_PUNCH);
                put_key(out, from);
            }
        }
    }

    /// The claimed identity of a client→coordinator *request*, if this is one.
    pub fn subject_key(&self) -> Option<NodeKey> {
        match self {
            Msg::BindRequest { from } => Some(*from),
            Msg::Register { key, .. } | Msg::Readvertise { key, .. } | Msg::Lookup { key } => {
                Some(*key)
            }
            _ => None,
        }
    }

    pub fn is_request(&self) -> bool {
        self.subject_key().is_some()
    }

    pub fn decode(buf: &[u8]) -> Result<Msg, WireError> {
        let mut r = Reader::new(buf);
        let msg = Msg::read(&mut r)?;
        // Reject oversized/malformed datagrams that decode a valid prefix but
        // carry trailing garbage: a well-formed message consumes the whole
        // buffer, nothing more.
        if r.pos != buf.len() {
            return Err(WireError::Trailing);
        }
        Ok(msg)
    }

    /// Read exactly one message (tag + body) from `r`, WITHOUT the
    /// whole-buffer check — used both by `decode` and the auth envelope.
    fn read(r: &mut Reader) -> Result<Msg, WireError> {
        let tag = r.take(1)?[0];
        let msg = match tag {
            TAG_BIND_REQ => Msg::BindRequest { from: r.key()? },
            TAG_BIND_RESP => Msg::BindResponse {
                reflexive: r.addr()?,
                cookie: r.cookie()?,
            },
            TAG_REGISTER => Msg::Register {
                key: r.key()?,
                cookie: r.cookie()?,
            },
            TAG_READVERTISE => Msg::Readvertise {
                key: r.key()?,
                nonce: r.u64()?,
                cookie: r.cookie()?,
            },
            TAG_LOOKUP => Msg::Lookup { key: r.key()? },
            TAG_LOOKUP_RESP => {
                let key = r.key()?;
                let present = r.take(1)?[0];
                let reflexive = match present {
                    0 => None,
                    1 => Some(r.addr()?),
                    _ => return Err(WireError::BadAddr),
                };
                Msg::LookupResponse { key, reflexive }
            }
            TAG_PUNCH_SYNC => Msg::PunchSync {
                peer: r.key()?,
                peer_reflexive: r.addr()?,
            },
            TAG_PUNCH => Msg::Punch { from: r.key()? },
            other => return Err(WireError::BadTag(other)),
        };
        Ok(msg)
    }
}

use crate::auth::{Authenticator, CoordCap, MAX_CAP_CHAIN};

/// One cap-chain link on the wire: issuer, not_after, issuer_sig.
const CAP_LINK_LEN: usize = 32 + 8 + 64;
/// An optional cap chain on the wire: a link count (0 = no cap), then each
/// link, the one naming the subject first and the root last.
pub(crate) const MAX_CAP_LEN: usize = 1 + MAX_CAP_CHAIN * CAP_LINK_LEN;

/// Write an optional cap chain. A chain deeper than [`MAX_CAP_CHAIN`] — one
/// neither `delegate_coord_cap` nor the decoder produces — is written cut to
/// its first links, a chain admission judges like any other.
pub(crate) fn put_cap<const CAP: usize>(out: &mut ArrayVec<u8, CAP>, cap: Option<&CoordCap>) {
    let links = || {
        cap.into_iter()
            .flat_map(CoordCap::links)
            .take(MAX_CAP_CHAIN)
    };
    out.push(links().count() as u8);
    for link in links() {
        put(out, link.issuer.as_ref());
        put_u64(out, link.not_after);
        put(out, link.issuer_sig.as_ref());
    }
}

impl Reader<'_> {
    /// Read an optional cap chain, refusing one deeper than
    /// [`MAX_CAP_CHAIN`] before reading any link.
    pub(crate) fn cap(&mut self) -> Result<Option<CoordCap>, WireError> {
        let count = usize::from(self.take(1)?[0]);
        if count > MAX_CAP_CHAIN {
            return Err(WireError::BadCrypto);
        }
        let mut links = ArrayVec::<_, MAX_CAP_CHAIN>::new();
        for _ in 0..count {
            links.push((self.pubkey()?, self.u64()?, self.sig()?));
        }
        // assembled from the root down, so no step recurses on what the peer sent.
        let cap = links
            .into_iter()
            .rev()
            .fold(None, |parent, (issuer, not_after, issuer_sig)| {
                Some(CoordCap {
                    issuer,
                    not_after,
                    issuer_sig,
                    parent: parent.map(Box::new),
                })
            });
        Ok(cap)
    }
}

impl CoordCap {
    /// This chain as bytes: the link count, then each link.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = ArrayVec::<u8, MAX_CAP_LEN>::new();
        put_cap(&mut out, Some(self));
        out.to_vec()
    }

    /// Whole-buffer inverse of [`CoordCap::encode`]: an empty chain, one
    /// deeper than [`MAX_CAP_CHAIN`], or trailing bytes is refused.
    pub fn decode(buf: &[u8]) -> Result<CoordCap, WireError> {
        let mut r = Reader::new(buf);
        let cap = r.cap()?.ok_or(WireError::BadCrypto)?;
        if r.pos != buf.len() {
            return Err(WireError::Trailing);
        }
        Ok(cap)
    }
}

/// An authenticated wrapper around one request `Msg`, carrying the per-request
/// authenticator. Wire tag `TAG_AUTH_REQUEST`. Only the four request shapes are wrappable.
///
/// `caller` is the authenticating identity — the key whose signer produced the
/// PoP. The coordinator authenticates THIS key, not the inner message's key:
/// for a `Lookup { key: peer }` the inner key is the peer being resolved, while
/// `caller` is the (different) node doing the resolving. Authenticating the
/// caller is what makes a cross-peer lookup possible; the inner key is only
/// cross-checked against `caller` for the self-ops (see the coordinator).
#[derive(Clone, Debug, PartialEq)]
pub struct AuthRequest {
    pub caller: NodeKey,
    pub inner: Msg,
    pub auth: Authenticator,
}

impl AuthRequest {
    /// Largest accepted envelope, including a capability and the largest bare
    /// `Msg`. Valid request inners are smaller; the broader bound also covers
    /// malformed response inners so they can still be encoded for rejection
    /// tests without an allocation fallback.
    pub const MAX_ENCODED_LEN: usize = 1 + 32 + Msg::MAX_ENCODED_LEN + 8 + 64 + MAX_CAP_LEN;

    /// Encode into a stack-backed, fixed-capacity vector.
    pub fn encode_inline(&self) -> ArrayVec<u8, { Self::MAX_ENCODED_LEN }> {
        let mut out = ArrayVec::new();
        out.push(TAG_AUTH_REQUEST);
        put_key(&mut out, &self.caller);
        self.inner.write(&mut out);
        put_u64(&mut out, self.auth.timestamp);
        put(&mut out, self.auth.pop_sig.as_ref());
        put_cap(&mut out, self.auth.cap.as_ref());
        out
    }

    pub fn encode(&self) -> Vec<u8> {
        self.encode_inline().into_iter().collect()
    }

    pub fn decode(buf: &[u8]) -> Result<AuthRequest, WireError> {
        let mut r = Reader::new(buf);
        let tag = r.take(1)?[0];
        if tag != TAG_AUTH_REQUEST {
            return Err(WireError::BadTag(tag));
        }
        let caller = r.key()?;
        let inner = Msg::read(&mut r)?;
        if !inner.is_request() {
            return Err(WireError::NotARequest);
        }
        let timestamp = r.u64()?;
        let pop_sig = r.sig()?;
        let cap = r.cap()?;
        if r.pos != buf.len() {
            return Err(WireError::Trailing);
        }
        Ok(AuthRequest {
            caller,
            inner,
            auth: Authenticator {
                timestamp,
                pop_sig,
                cap,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    fn addr(o: u8, p: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, o)), p)
    }

    fn addr6(last: u16, port: u16) -> SocketAddr {
        SocketAddr::new(
            IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, last)),
            port,
        )
    }

    #[test]
    fn every_variant_roundtrips() {
        let cases = vec![
            Msg::BindRequest {
                from: NodeKey([1u8; 32]),
            },
            Msg::BindResponse {
                reflexive: addr(2, 51820),
                cookie: [0xcau8; 32],
            },
            Msg::Register {
                key: NodeKey([3u8; 32]),
                cookie: [0xcbu8; 32],
            },
            Msg::Readvertise {
                key: NodeKey([13u8; 32]),
                nonce: 0x0102_0304_dead_beef,
                cookie: [0xcdu8; 32],
            },
            Msg::Lookup {
                key: NodeKey([4u8; 32]),
            },
            Msg::LookupResponse {
                key: NodeKey([5u8; 32]),
                reflexive: Some(addr6(6, 443)),
            },
            Msg::LookupResponse {
                key: NodeKey([7u8; 32]),
                reflexive: None,
            },
            Msg::PunchSync {
                peer: NodeKey([8u8; 32]),
                peer_reflexive: addr6(9, 7000),
            },
            Msg::Punch {
                from: NodeKey([10u8; 32]),
            },
        ];
        for m in cases {
            let bytes = m.encode();
            let inline = m.encode_inline();
            assert_eq!(&inline[..], bytes);
            let back = Msg::decode(&bytes).expect("decode");
            assert_eq!(m, back);
        }
    }

    #[test]
    fn short_buffer_is_error() {
        assert_eq!(Msg::decode(&[]), Err(WireError::Short));
        assert_eq!(Msg::decode(&[0xff]), Err(WireError::BadTag(0xff)));
    }

    #[test]
    fn relay_range_tags_are_never_udp_messages() {
        // Tags >= 16 are the TCP relay framing's namespace; the shared PoP
        // signing namespace makes byte-level disjointness load-bearing, so
        // every relay-range tag must decode as a clean BadTag here.
        for tag in [16u8, 17, 32, 0xff] {
            let mut frame = vec![tag];
            frame.extend_from_slice(&[0x22; 32]);
            assert_eq!(Msg::decode(&frame), Err(WireError::BadTag(tag)));
        }
    }

    #[test]
    fn decode_rejects_trailing_garbage_bytes() {
        // A well-formed message with extra bytes appended (oversized /
        // malformed datagram) must be rejected outright, not silently
        // accepted by ignoring whatever the reader didn't consume.
        let mut bytes = Msg::BindRequest {
            from: NodeKey([9u8; 32]),
        }
        .encode();
        bytes.push(0xff);
        assert_eq!(Msg::decode(&bytes), Err(WireError::Trailing));

        let mut bytes = Msg::PunchSync {
            peer: NodeKey([1u8; 32]),
            peer_reflexive: addr(2, 51820),
        }
        .encode();
        bytes.extend_from_slice(&[0, 0, 0]);
        assert_eq!(Msg::decode(&bytes), Err(WireError::Trailing));
    }

    #[test]
    fn readvertise_carries_key_and_nonce() {
        let m = Msg::Readvertise {
            key: NodeKey([0xab; 32]),
            nonce: 0xffff_0000_ffff_0001,
            cookie: [0xceu8; 32],
        };
        let back = Msg::decode(&m.encode()).expect("decode");
        assert_eq!(m, back);
        // Trailing garbage after a Readvertise is rejected like any other message.
        let mut bytes = m.encode();
        bytes.push(0xff);
        assert_eq!(Msg::decode(&bytes), Err(WireError::Trailing));
    }

    #[test]
    fn auth_request_roundtrips_for_every_request_shape() {
        use crate::auth::{mint_coord_cap, sign_authenticator};
        use commonware_cryptography::{Signer as _, ed25519};

        let node = ed25519::PrivateKey::from_seed(1);
        let g = ed25519::PrivateKey::from_seed(2);
        let mut subject = [0u8; 32];
        subject.copy_from_slice(node.public_key().as_ref());
        let subject = NodeKey(subject);

        let inners = vec![
            Msg::BindRequest { from: subject },
            Msg::Register {
                key: subject,
                cookie: [0xccu8; 32],
            },
            Msg::Readvertise {
                key: subject,
                nonce: 42,
                cookie: [0xcfu8; 32],
            },
            Msg::Lookup {
                key: NodeKey([7u8; 32]),
            },
        ];
        for inner in inners {
            // Without a cap, with a root cap, and with the deepest chain.
            let root = mint_coord_cap(&g, subject, 9_999_999);
            let deepest = (1..MAX_CAP_CHAIN).fold(root.clone(), |parent, _| {
                crate::auth::delegate_coord_cap(&parent, &g, subject, 9_999_999).unwrap()
            });
            for cap in [None, Some(root), Some(deepest)] {
                let auth = sign_authenticator(&node, &inner.encode(), 1234, cap);
                // caller is the authenticating identity — for a cross-peer
                // Lookup it deliberately differs from the inner key.
                let req = AuthRequest {
                    caller: subject,
                    inner: inner.clone(),
                    auth,
                };
                let bytes = req.encode();
                let inline = req.encode_inline();
                assert_eq!(&inline[..], bytes);
                let back = AuthRequest::decode(&bytes).expect("decode");
                assert_eq!(req, back);
            }
        }
    }

    #[test]
    fn auth_request_rejects_response_inner() {
        use crate::auth::sign_authenticator;
        use commonware_cryptography::{Signer as _, ed25519};
        let node = ed25519::PrivateKey::from_seed(1);
        // Hand-encode an envelope whose inner is a RESPONSE (LookupResponse).
        let inner = Msg::LookupResponse {
            key: NodeKey([1u8; 32]),
            reflexive: None,
        };
        let auth = sign_authenticator(&node, &inner.encode(), 1, None);
        let bytes = AuthRequest {
            caller: NodeKey([9u8; 32]),
            inner,
            auth,
        }
        .encode();
        assert_eq!(AuthRequest::decode(&bytes), Err(WireError::NotARequest));
    }

    #[test]
    fn auth_request_rejects_trailing_and_bare_msg_decode_rejects_its_tag() {
        use crate::auth::sign_authenticator;
        use commonware_cryptography::{Signer as _, ed25519};
        let node = ed25519::PrivateKey::from_seed(1);
        let inner = Msg::Register {
            key: NodeKey([2u8; 32]),
            cookie: [0xcdu8; 32],
        };
        let auth = sign_authenticator(&node, &inner.encode(), 1, None);
        let mut bytes = AuthRequest {
            caller: NodeKey([2u8; 32]),
            inner,
            auth,
        }
        .encode();
        bytes.push(0xff);
        assert_eq!(AuthRequest::decode(&bytes), Err(WireError::Trailing));
        // An auth-request envelope must NOT decode as a bare Msg.
        let clean = AuthRequest {
            caller: NodeKey([2u8; 32]),
            inner: Msg::Register {
                key: NodeKey([2u8; 32]),
                cookie: [0xcdu8; 32],
            },
            auth: sign_authenticator(
                &node,
                &Msg::Register {
                    key: NodeKey([2u8; 32]),
                    cookie: [0xcdu8; 32],
                }
                .encode(),
                1,
                None,
            ),
        }
        .encode();
        assert_eq!(
            Msg::decode(&clean),
            Err(WireError::BadTag(TAG_AUTH_REQUEST))
        );
    }
}
