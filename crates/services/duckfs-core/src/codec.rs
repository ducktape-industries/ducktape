//! the shared canonical cursor codec — the one strict byte grammar every
//! duckfs frame speaks: little-endian integers, `u64 len ‖ utf-8 bytes`
//! strings, raw 32-byte ids, 0/1 booleans, and a whole-input `finish`.
//!
//! `objects.rs` (object bodies — id preimages) and `state.rs` (the refs
//! image — the root preimage and the snapshot-lane payload) both write with
//! these push helpers and read through [`Reader`], so the two frames cannot
//! drift apart — before this module each file carried a private copy of the
//! same helpers, which was exactly the drift risk. the byte layouts are
//! contracts (cursors cross the wire; bodies are hash preimages), so the
//! golden-byte tests in `tests/object_model.rs` / `tests/state_root.rs` pin
//! that this shared codec encodes identically to the former private copies.
//!
//! every read advances the cursor and bounds-checks against the input; a
//! field that runs past the end is a truncation, and callers end with
//! [`Reader::finish`] so unconsumed trailing bytes reject. errors carry the
//! caller-supplied frame label ("object body" / "refs image") so a reject
//! still names the frame it came from. pure core: no `std::fs`, no sdk.

/// append the canonical string shape: `u64 len (LE) ‖ utf-8 bytes`.
pub(crate) fn push_string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u64).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

/// append a little-endian u32 (the canonical count shape).
pub(crate) fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// a strict decode cursor over one frame. `what` labels errors with the frame
/// name the caller decodes ("object body", "refs image").
pub(crate) struct Reader<'a> {
    what: &'static str,
    bytes: &'a [u8],
    off: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(what: &'static str, bytes: &'a [u8]) -> Self {
        Self {
            what,
            bytes,
            off: 0,
        }
    }

    /// read `N` bytes, advancing the cursor; running past the end is a
    /// truncation.
    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let end = self
            .off
            .checked_add(N)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| format!("{} truncated", self.what))?;
        let mut buf = [0u8; N];
        buf.copy_from_slice(&self.bytes[self.off..end]);
        self.off = end;
        Ok(buf)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, String> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.array::<2>()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.array::<4>()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.array::<8>()?))
    }

    pub(crate) fn bytes32(&mut self) -> Result<[u8; 32], String> {
        self.array::<32>()
    }

    /// a single-byte boolean; only 0 and 1 are canonical, any other byte
    /// rejects.
    pub(crate) fn boolean(&mut self) -> Result<bool, String> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(format!("{} boolean byte is not 0/1", self.what)),
        }
    }

    /// a `u64` length prefix followed by exactly that many utf-8 bytes. the
    /// length is bounded by the remaining input before any allocation, so a
    /// bogus length truncates rather than over-allocating.
    pub(crate) fn bytes(&mut self) -> Result<Vec<u8>, String> {
        let len = self.u64()?;
        let len = usize::try_from(len).map_err(|_| format!("{} truncated", self.what))?;
        let end = self
            .off
            .checked_add(len)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| format!("{} truncated", self.what))?;
        let value = self.bytes[self.off..end].to_vec();
        self.off = end;
        Ok(value)
    }

    pub(crate) fn string(&mut self) -> Result<String, String> {
        String::from_utf8(self.bytes()?).map_err(|_| format!("{} string is not utf-8", self.what))
    }

    /// every byte must be accounted for — a decode that stops short of the
    /// end saw trailing bytes and is not canonical.
    pub(crate) fn finish(self) -> Result<(), String> {
        if self.off != self.bytes.len() {
            return Err(format!("{} has trailing bytes", self.what));
        }
        Ok(())
    }
}

// these run under `--no-default-features` too — the codec is pure core.
#[cfg(test)]
mod tests {
    use super::*;

    /// one buffer written with the push helpers reads back field-for-field —
    /// the writer and reader agree on every shape.
    #[test]
    fn writes_read_back() {
        let mut buf = Vec::new();
        push_u32(&mut buf, 7);
        push_string(&mut buf, "hé"); // multi-byte utf-8 counts bytes, not chars
        buf.push(1);
        buf.extend_from_slice(&[9u8; 32]);
        buf.extend_from_slice(&5u64.to_le_bytes());

        let mut r = Reader::new("object body", &buf);
        assert_eq!(r.u32().unwrap(), 7);
        assert_eq!(r.string().unwrap(), "hé");
        assert!(r.boolean().unwrap());
        assert_eq!(r.bytes32().unwrap(), [9u8; 32]);
        assert_eq!(r.u64().unwrap(), 5);
        r.finish().expect("fully consumed");
    }

    /// the strict-grammar rejects: truncation, non-0/1 booleans, non-utf-8
    /// strings, a string length past the end, and trailing bytes — each error
    /// labeled with the caller's frame name.
    #[test]
    fn strict_rejects_are_labeled() {
        // truncated fixed-width read.
        let mut r = Reader::new("refs image", &[0u8; 3]);
        let err = r.u32().unwrap_err();
        assert_eq!(err, "refs image truncated");

        // non-canonical boolean byte.
        let mut r = Reader::new("object body", &[2u8]);
        let err = r.boolean().unwrap_err();
        assert_eq!(err, "object body boolean byte is not 0/1");

        // string length running past the end truncates, never allocates.
        let mut lying = (u64::MAX).to_le_bytes().to_vec();
        lying.push(b'x');
        let mut r = Reader::new("refs image", &lying);
        assert_eq!(r.string().unwrap_err(), "refs image truncated");

        // non-utf-8 string bytes reject.
        let mut bad = 1u64.to_le_bytes().to_vec();
        bad.push(0xFF);
        let mut r = Reader::new("object body", &bad);
        assert_eq!(r.string().unwrap_err(), "object body string is not utf-8");

        // trailing bytes reject at finish.
        let mut r = Reader::new("refs image", &[0u8; 2]);
        r.u8().unwrap();
        assert_eq!(r.finish().unwrap_err(), "refs image has trailing bytes");
    }
}
