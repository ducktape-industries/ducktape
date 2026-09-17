//! the deterministic little-endian binary codec shared by forge's snapshot
//! container, the tracker's canonical bytes, and the on-disk tracker file.
//!
//! writes are plain appends onto a `Vec<u8>`; reads go through a bounds-checked
//! [`Reader`] so a forged length field from an UNTRUSTED byte source (a
//! byzantine snapshot, a tampered disk file) can never slice or allocate past
//! the buffer. every multi-byte integer is little-endian; strings are
//! `u32-LE(len) ++ utf8-bytes`.

use sdk::Error;

/// a bounds-checked cursor over untrusted bytes: every read verifies the
/// remaining length BEFORE slicing.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    pub fn done(&self) -> bool {
        self.pos == self.buf.len()
    }
    pub fn u8(&mut self) -> Result<u8, Error> {
        let b = self.take(1)?;
        Ok(b[0])
    }
    pub fn u32(&mut self) -> Result<u32, Error> {
        if self.remaining() < 4 {
            return Err(Error::module("codec", "forge codec: truncated u32 field"));
        }
        let v = u32::from_le_bytes(self.buf[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        Ok(v)
    }
    pub fn u64(&mut self) -> Result<u64, Error> {
        if self.remaining() < 8 {
            return Err(Error::module("codec", "forge codec: truncated u64 field"));
        }
        let v = u64::from_le_bytes(self.buf[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        Ok(v)
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.remaining() < n {
            return Err(Error::module(
                "codec",
                format!(
                    "forge codec: truncated field ({n} bytes needed, {} left)",
                    self.remaining()
                ),
            ));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    /// a `u32-LE(len) ++ utf8` string.
    pub fn str_(&mut self) -> Result<String, Error> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| Error::module("codec", "forge codec: string field not utf-8"))
    }
}

pub fn put_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}
pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
/// `u32-LE(len) ++ utf8-bytes`. every string forge encodes is cap-bounded well
/// below u32::MAX, so the cast never truncates.
pub fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}
pub fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_u32(out, b.len() as u32);
    out.extend_from_slice(b);
}
