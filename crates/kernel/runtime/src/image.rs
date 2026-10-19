//! A parked run's memory, kept as the pages that differ from its code's fresh memory, and the
//! memory a fresh instance becomes again once those pages are written back. Stored, the pages are
//! packed whole into one compressed value.
use std::io;

use borsh::{BorshDeserialize, BorshSerialize};

pub const PAGE_BYTES: usize = 4 << 10;
pub const PACK_LEVEL: i32 = 3;

#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Image {
    pub length: u64,
    pub pages: Vec<Page>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Page {
    pub index: u32,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Codec {
    Zstd,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Packed {
    pub length: u64,
    pub pages: Vec<u32>,
    pub codec: Codec,
    pub bytes: Vec<u8>,
}

impl Image {
    pub fn of(fresh: &[u8], memory: &[u8]) -> Image {
        let blank = [0u8; PAGE_BYTES];
        let pages = memory
            .chunks(PAGE_BYTES)
            .enumerate()
            .filter_map(|(index, now)| {
                let start = index * PAGE_BYTES;
                let before = fresh
                    .get(start..start + now.len())
                    .unwrap_or(&blank[..now.len()]);
                let changed = now != before;
                changed.then(|| Page {
                    index: index as u32,
                    bytes: now.to_vec(),
                })
            })
            .collect();
        Image {
            length: memory.len() as u64,
            pages,
        }
    }

    pub fn kept_bytes(&self) -> u64 {
        self.pages.iter().map(|page| page.bytes.len() as u64).sum()
    }

    pub fn onto(&self, fresh: &[u8]) -> Vec<u8> {
        let mut memory = fresh.to_vec();
        memory.resize(self.length as usize, 0);
        for page in &self.pages {
            let start = page.index as usize * PAGE_BYTES;
            memory[start..start + page.bytes.len()].copy_from_slice(&page.bytes);
        }
        memory
    }

    pub fn pack(&self) -> io::Result<Packed> {
        if let Some(page) = self
            .pages
            .iter()
            .find(|page| page.bytes.len() != PAGE_BYTES)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "page {} holds {} bytes, not a whole page",
                    page.index,
                    page.bytes.len()
                ),
            ));
        }
        let whole = self
            .pages
            .iter()
            .map(|page| page.bytes.as_slice())
            .collect::<Vec<_>>()
            .concat();
        Ok(Packed {
            length: self.length,
            pages: self.pages.iter().map(|page| page.index).collect(),
            codec: Codec::Zstd,
            bytes: zstd::bulk::compress(&whole, PACK_LEVEL)?,
        })
    }
}

impl Packed {
    pub fn unpack(self) -> io::Result<Image> {
        let size = self.pages.len() * PAGE_BYTES;
        let whole = match self.codec {
            Codec::Zstd => zstd::bulk::decompress(&self.bytes, size)?,
        };
        if whole.len() != size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} pages unpacked to {} bytes",
                    self.pages.len(),
                    whole.len()
                ),
            ));
        }
        let pages = self
            .pages
            .into_iter()
            .zip(whole.chunks(PAGE_BYTES))
            .map(|(index, bytes)| Page {
                index,
                bytes: bytes.to_vec(),
            })
            .collect();
        Ok(Image {
            length: self.length,
            pages,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: &str = "28b52ffd60001fb503006404000102030405060102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f41a820be6ee0d712f8fffff7af672449924c922489249224932449224912c92499249124495292a80304f6572ba5";

    fn two_pages() -> Image {
        Image {
            length: 8 * PAGE_BYTES as u64,
            pages: vec![
                Page {
                    index: 1,
                    bytes: (0..PAGE_BYTES).map(|i| (i % 7) as u8).collect(),
                },
                Page {
                    index: 4,
                    bytes: (0..PAGE_BYTES).map(|i| (i / 64) as u8).collect(),
                },
            ],
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn a_packed_image_unpacks_to_the_same_pages() {
        let image = two_pages();
        let packed = image.pack().unwrap();
        assert_eq!(packed.pages, [1, 4]);
        assert_eq!(packed.length, image.length);
        assert!(packed.bytes.len() < PAGE_BYTES);
        assert_eq!(packed.unpack().unwrap(), image);
    }

    #[test]
    fn packing_writes_the_bytes_every_node_writes() {
        assert_eq!(hex(&two_pages().pack().unwrap().bytes), GOLDEN);
    }

    #[test]
    fn a_part_page_is_not_packed_and_a_short_unpack_is_refused() {
        let mut image = two_pages();
        image.pages[1].bytes.pop();
        assert_eq!(
            image.pack().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let mut packed = two_pages().pack().unwrap();
        packed.pages.push(9);
        assert_eq!(
            packed.unpack().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn an_image_keeps_only_changed_pages_and_makes_the_memory_again() {
        let fresh: Vec<u8> = (0..3 * PAGE_BYTES).map(|i| (i % 7) as u8).collect();
        let mut memory = fresh.clone();
        memory[PAGE_BYTES + 5] = 99;
        memory.extend(vec![0u8; PAGE_BYTES]);
        memory.extend(vec![1u8; PAGE_BYTES]);
        let image = Image::of(&fresh, &memory);
        let kept: Vec<u32> = image.pages.iter().map(|page| page.index).collect();
        assert_eq!(kept, [1, 4]);
        assert_eq!(image.kept_bytes(), 2 * PAGE_BYTES as u64);
        assert_eq!(image.onto(&fresh), memory);
    }
}
