//! the shared snapshot-codec primitives — ONE length-prefixed LE toolkit for
//! every module's canonical binary encoding (the `root()` preimage and the
//! snapshot format), replacing the per-module hand-rolled copies that used to
//! live in valset/governance/modules/identity/capability/saga/dispatch/….
//!
//! this is deliberately dumb: writers append, the [`Cursor`] reads forward
//! over UNTRUSTED bytes with every accessor length-checked before it reads,
//! so a forged length or count can never over-read or drive an allocation
//! the bytes could not back. modules keep owning their layouts; only the
//! primitives are shared, so an encoding bug is fixed once, not N times.

use crate::{Error, refusal};

/// append a `u64`-length-prefixed byte slice.
pub fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// append a `u64`-length-prefixed utf-8 string.
pub fn push_str(out: &mut Vec<u8>, s: &str) {
    push_bytes(out, s.as_bytes());
}

/// append an optional byte slice as a `0` flag, or `1` + length-prefixed bytes.
pub fn push_opt_bytes(out: &mut Vec<u8>, bytes: Option<&[u8]>) {
    match bytes {
        Some(b) => {
            out.push(1u8);
            push_bytes(out, b);
        }
        None => out.push(0u8),
    }
}

/// append an optional string as a `0` flag, or `1` + length-prefixed bytes.
pub fn push_opt_str(out: &mut Vec<u8>, s: Option<&str>) {
    push_opt_bytes(out, s.map(str::as_bytes));
}

/// append an optional `u64` as a `0` flag, or `1` + 8 LE bytes.
pub fn push_opt_u64(out: &mut Vec<u8>, v: Option<u64>) {
    match v {
        Some(v) => {
            out.push(1u8);
            out.extend_from_slice(&v.to_le_bytes());
        }
        None => out.push(0u8),
    }
}

/// a forward-only reader over untrusted canonical-encoding bytes: every
/// accessor checks the remaining buffer before it reads. `what` in each
/// accessor names the field for the error message, so a decode failure says
/// WHICH field was truncated/forged, not just that one was.
pub struct Cursor<'a> {
    buf: &'a [u8],
    /// a per-field length cap enforced by [`Cursor::bytes`] BEFORE it checks
    /// the remaining buffer — the recovery-journal defense against a forged
    /// length that is within the buffer but absurd for the field. `usize::MAX`
    /// (the [`Cursor::new`] default) means "no extra cap", so a plain cursor
    /// behaves exactly as before.
    max_field_len: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            max_field_len: usize::MAX,
        }
    }

    /// a cursor that additionally rejects any length-prefixed field longer than
    /// `max_field_len` (see the field doc) — the home for recovery's
    /// `MAX_RECORD_FIELD_LEN` / `MAX_CHECKPOINT_FIELD_LEN` per-field caps.
    pub fn with_cap(buf: &'a [u8], max_field_len: usize) -> Self {
        Self { buf, max_field_len }
    }

    /// bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.buf.len()
    }

    pub fn u64(&mut self, what: &str) -> Result<u64, Error> {
        let Some((head, rest)) = self.buf.split_first_chunk::<8>() else {
            return Err(truncated(what));
        };
        self.buf = rest;
        Ok(u64::from_le_bytes(*head))
    }

    pub fn u32(&mut self, what: &str) -> Result<u32, Error> {
        let Some((head, rest)) = self.buf.split_first_chunk::<4>() else {
            return Err(truncated(what));
        };
        self.buf = rest;
        Ok(u32::from_le_bytes(*head))
    }

    pub fn byte(&mut self, what: &str) -> Result<u8, Error> {
        let Some((&b, rest)) = self.buf.split_first() else {
            return Err(truncated(what));
        };
        self.buf = rest;
        Ok(b)
    }

    pub fn bool(&mut self, what: &str) -> Result<bool, Error> {
        match self.byte(what)? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(Error::module(
                refusal::INVALID_INPUT,
                format!("{what} flag must be 0 or 1, got {other}"),
            )),
        }
    }

    pub fn array<const N: usize>(&mut self, what: &str) -> Result<[u8; N], Error> {
        let Some((head, rest)) = self.buf.split_first_chunk::<N>() else {
            return Err(truncated(what));
        };
        self.buf = rest;
        Ok(*head)
    }

    /// a `u64`-length-prefixed byte slice, length-checked before the split.
    pub fn bytes(&mut self, what: &str) -> Result<&'a [u8], Error> {
        let len = self.u64(what)?;
        if len > self.max_field_len as u64 {
            return Err(Error::module(
                refusal::INVALID_INPUT,
                format!(
                    "{what} length {len} exceeds the field cap {}",
                    self.max_field_len
                ),
            ));
        }
        if len > self.buf.len() as u64 {
            return Err(Error::module(
                refusal::INVALID_INPUT,
                format!(
                    "{what} length {len} exceeds the {} remaining bytes",
                    self.buf.len()
                ),
            ));
        }
        let (head, rest) = self.buf.split_at(len as usize);
        self.buf = rest;
        Ok(head)
    }

    /// a length-prefixed utf-8 string.
    pub fn string(&mut self, what: &str) -> Result<String, Error> {
        let raw = self.bytes(what)?;
        std::str::from_utf8(raw)
            .map(str::to_string)
            .map_err(|e| Error::module(refusal::INVALID_INPUT, format!("{what} is not utf-8: {e}")))
    }

    /// an optional utf-8 string: flag byte `0` (absent) or `1` + a non-empty,
    /// `max`-bounded, valid-utf8 length-prefixed slice.
    pub fn opt_str(&mut self, max: usize, what: &str) -> Result<Option<String>, Error> {
        match self.byte(what)? {
            0 => Ok(None),
            1 => {
                let raw = self.bytes(what)?;
                if raw.is_empty() {
                    return Err(Error::module(
                        refusal::INVALID_INPUT,
                        format!("{what} flag set but empty"),
                    ));
                }
                if raw.len() > max {
                    return Err(Error::module(
                        refusal::INVALID_INPUT,
                        format!("{what} exceeds the {max}-byte limit"),
                    ));
                }
                let s = std::str::from_utf8(raw).map_err(|e| {
                    Error::module(refusal::INVALID_INPUT, format!("{what} is not utf-8: {e}"))
                })?;
                Ok(Some(s.to_string()))
            }
            other => Err(Error::module(
                refusal::INVALID_INPUT,
                format!("{what} flag must be 0 or 1, got {other}"),
            )),
        }
    }

    /// an optional byte slice: flag byte `0` (absent) or `1` + length-prefixed
    /// bytes — the plain mirror of [`push_opt_bytes`], no extra shape rules.
    pub fn opt_bytes(&mut self, what: &str) -> Result<Option<&'a [u8]>, Error> {
        Ok(if self.bool(what)? {
            Some(self.bytes(what)?)
        } else {
            None
        })
    }

    /// an optional `u64`: flag byte `0` (absent) or `1` + 8 LE bytes.
    pub fn opt_u64(&mut self, what: &str) -> Result<Option<u64>, Error> {
        Ok(if self.bool(what)? {
            Some(self.u64(what)?)
        } else {
            None
        })
    }

    /// reject a forged count that the remaining bytes could not possibly hold,
    /// before looping — `count * min_each` must fit.
    pub fn bound(&self, count: u64, min_each: u64, what: &str) -> Result<(), Error> {
        if count > (self.buf.len() as u64) / min_each.max(1) {
            return Err(Error::module(
                refusal::INVALID_INPUT,
                format!(
                    "{what} count {count} exceeds the {} remaining bytes",
                    self.buf.len()
                ),
            ));
        }
        Ok(())
    }

    /// the decode consumed everything — trailing bytes are a forgery signal.
    pub fn finish(&self, what: &str) -> Result<(), Error> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(Error::module(
                refusal::INVALID_INPUT,
                format!("{what} carries {} trailing bytes", self.buf.len()),
            ))
        }
    }
}

fn truncated(what: &str) -> Error {
    Error::module(refusal::INVALID_INPUT, format!("{what} truncated"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_fails_closed() {
        let mut out = Vec::new();
        out.extend_from_slice(&7u64.to_le_bytes());
        push_str(&mut out, "hello");
        push_opt_str(&mut out, None);
        push_opt_str(&mut out, Some("x"));
        push_opt_u64(&mut out, None);
        push_opt_u64(&mut out, Some(9));
        push_opt_bytes(&mut out, Some(b""));
        out.push(1u8);

        let mut c = Cursor::new(&out);
        assert_eq!(c.u64("n").unwrap(), 7);
        assert_eq!(c.string("s").unwrap(), "hello");
        assert_eq!(c.opt_str(64, "a").unwrap(), None);
        assert_eq!(c.opt_str(64, "b").unwrap().as_deref(), Some("x"));
        assert_eq!(c.opt_u64("c").unwrap(), None);
        assert_eq!(c.opt_u64("d").unwrap(), Some(9));
        // the plain option reader admits an empty present value (opt_str
        // deliberately does not).
        assert_eq!(c.opt_bytes("e").unwrap(), Some(&b""[..]));
        assert!(c.bool("f").unwrap());
        c.finish("blob").unwrap();

        // forged length over-read fails closed with the field name.
        let mut bad = Vec::new();
        bad.extend_from_slice(&100u64.to_le_bytes());
        bad.push(1);
        let mut c = Cursor::new(&bad);
        let err = c.bytes("payload").unwrap_err().to_string();
        assert!(err.contains("payload"), "{err}");

        // trailing bytes are rejected.
        let c = Cursor::new(&[0u8]);
        assert!(c.finish("blob").is_err());

        // forged count bound.
        let c = Cursor::new(&[0u8; 16]);
        assert!(c.bound(3, 8, "entries").is_err());
        assert!(c.bound(2, 8, "entries").is_ok());
    }

    #[test]
    fn with_cap_rejects_oversized_field() {
        // a 4-byte field that fits the buffer but exceeds a 3-byte cap.
        let mut out = Vec::new();
        push_bytes(&mut out, &[1, 2, 3, 4]);

        // no cap: accepted.
        assert_eq!(Cursor::new(&out).bytes("f").unwrap(), &[1, 2, 3, 4]);

        // capped below the field length: rejected, and the message names the cap
        // (the recovery `record_keeps_the_operation_field_cap` contract).
        let err = Cursor::with_cap(&out, 3)
            .bytes("f")
            .unwrap_err()
            .to_string();
        assert!(err.contains("field cap"), "{err}");

        // capped at exactly the field length: still accepted.
        assert_eq!(Cursor::with_cap(&out, 4).bytes("f").unwrap(), &[1, 2, 3, 4]);
    }
}
