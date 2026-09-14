//! Raw guest pictures outlive native renderer entities. A patched tree carries
//! only hashes; mounting that tree in a new window still needs the first bytes.

use std::collections::HashMap;
use ui_lang_wire as wire;

const MAX_PICTURE_BYTES: usize = 8 * wire::MAX_PICTURE_BYTES_PER_FRAME;
const MAX_PICTURES: usize = 4096;

#[derive(Default)]
pub(super) struct Pictures {
    raster: HashMap<u64, wire::ImageData>,
    vector: HashMap<u64, Vec<u8>>,
}

impl Pictures {
    pub(super) fn adopt(&mut self, root: &mut wire::Node) {
        // Guests send each hash once. Eviction would lose an image the guest
        // still believes is present, so the lifetime budget refuses new hashes.
        let mut used = self.raster.values().map(wire::ImageData::byte_len).sum::<usize>()
            + self.vector.values().map(Vec::len).sum::<usize>();
        root.for_each_mut(&mut |node| match node {
            wire::Node::Image { hash, data: Some(data), .. }
            | wire::Node::ImageViewer { hash, data: Some(data), .. } => {
                let total = used + data.byte_len();
                let accepted = !self.raster.contains_key(hash) && total <= MAX_PICTURE_BYTES
                    && self.raster.len() + self.vector.len() < MAX_PICTURES;
                if accepted {
                    self.raster.insert(*hash, data.clone());
                    used = total;
                }
            }
            wire::Node::Svg { hash, bytes: Some(bytes), .. } => {
                let total = used + bytes.len();
                let accepted = !self.vector.contains_key(hash) && total <= MAX_PICTURE_BYTES
                    && self.raster.len() + self.vector.len() < MAX_PICTURES;
                if accepted {
                    self.vector.insert(*hash, bytes.clone());
                    used = total;
                }
            }
            _ => {}
        });
    }

    pub(super) fn hydrate(&self, root: &mut wire::Node) {
        root.for_each_mut(&mut |node| match node {
            wire::Node::Image { hash, data, .. } | wire::Node::ImageViewer { hash, data, .. } => {
                if data.is_none() { *data = self.raster.get(hash).cloned(); }
            }
            wire::Node::Svg { hash, bytes, .. }
                if bytes.is_none() => { *bytes = self.vector.get(hash).cloned(); }
            _ => {}
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector(hash: u64, bytes: Option<Vec<u8>>) -> wire::Node {
        wire::Node::Svg {
            key: format!("picture-{hash}"), hash, bytes,
            inherit_button_ink: false, label: None, color: None, hover: None,
            fit: None, rotation: None, opacity: None, width: None, height: None,
        }
    }

    #[test]
    fn hidden_pictures_survive_remount_and_the_first_hash_value_wins() {
        let mut pictures = Pictures::default();
        pictures.adopt(&mut vector(7, Some(b"first".to_vec())));
        pictures.adopt(&mut wire::Node::empty());
        pictures.adopt(&mut vector(7, Some(b"conflicting".to_vec())));
        let mut remounted = vector(7, None);
        pictures.hydrate(&mut remounted);
        assert!(matches!(remounted, wire::Node::Svg { bytes: Some(bytes), .. } if bytes == b"first"));
    }

    #[test]
    fn lifetime_budget_refuses_new_bytes_without_evicting_known_hashes() {
        let mut pictures = Pictures::default();
        pictures.vector.insert(1, vec![0; MAX_PICTURE_BYTES]);
        pictures.adopt(&mut vector(2, Some(vec![1])));
        assert!(pictures.vector.contains_key(&1));
        assert!(!pictures.vector.contains_key(&2));
        assert_eq!(pictures.vector.values().map(Vec::len).sum::<usize>(), MAX_PICTURE_BYTES);
    }

    #[test]
    fn entry_budget_also_bounds_empty_pictures() {
        let mut pictures = Pictures::default();
        for hash in 0..MAX_PICTURES as u64 {
            pictures.adopt(&mut vector(hash, Some(Vec::new())));
        }
        pictures.adopt(&mut vector(MAX_PICTURES as u64, Some(Vec::new())));
        assert_eq!(pictures.vector.len(), MAX_PICTURES);
    }
}
