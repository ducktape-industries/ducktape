//! Raw guest pictures outlive native renderer entities. A patched tree carries
//! only hashes; mounting that tree in a new window still needs the first bytes.

use std::collections::HashMap;
use view_wire as wire;

#[derive(Default)]
pub(super) struct Pictures {
    raster: HashMap<u64, wire::ImageData>,
    vector: HashMap<u64, Vec<u8>>,
}

impl Pictures {
    pub(super) fn adopt(&mut self, root: &mut wire::Node) {
        root.for_each_mut(&mut |node| match node {
            wire::Node::Image {
                hash,
                data: Some(data),
                ..
            }
            | wire::Node::ImageViewer {
                hash,
                data: Some(data),
                ..
            } => {
                self.raster.entry(*hash).or_insert_with(|| data.clone());
            }
            wire::Node::Svg {
                hash,
                bytes: Some(bytes),
                ..
            } => {
                self.vector.entry(*hash).or_insert_with(|| bytes.clone());
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
            fit: None, opacity: None, width: None, height: None,
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
    fn host_image_resource_survives_a_patch_and_remount() {
        let mut pictures = Pictures::default();
        let mut image = wire::Node::Image {
            key: "live-image".into(),
            hash: 11,
            data: Some(wire::ImageData::Resource("image:7".into())),
            label: None,
            fit: None,
            opacity: None,
            width: None,
            height: None,
        };
        pictures.adopt(&mut image);
        if let wire::Node::Image { data, .. } = &mut image {
            *data = None;
        }
        pictures.adopt(&mut image);
        pictures.hydrate(&mut image);
        assert!(matches!(image, wire::Node::Image {
            data: Some(wire::ImageData::Resource(ref key)), ..
        } if key == "image:7"));
    }
}
