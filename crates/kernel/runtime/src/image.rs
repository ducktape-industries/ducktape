//! A parked run's memory, kept as the pages that differ from its code's fresh memory, and the
//! memory a fresh instance becomes again once those pages are written back.
use borsh::{BorshDeserialize, BorshSerialize};

pub const PAGE_BYTES: usize = 4 << 10;

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
