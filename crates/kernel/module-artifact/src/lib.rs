//! One deployable unit: a module (consensus code, optional index mapper,
//! optional view) or a view alone. The frame says which; the registry commits
//! the hash of the whole frame and activates it once.
use std::collections::BTreeMap;

use borsh::BorshSerialize;
use sha2::{Digest as _, Sha256};

/// The complete encoded artifact is bounded like the node's staging lane.
pub const MAX_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_VIEW_ASSETS: usize = 4096;
pub const MAX_ASSET_PATH_BYTES: usize = 1024;

/// The frame tag: what the artifact IS. The registry entry's `kind` names the
/// same thing; a `Module` reader handed a `View` frame refuses, it never
/// improvises an empty core.
const MODULE_TAG: u8 = 0;
const VIEW_TAG: u8 = 1;

/// One deployable unit. The tag is the first byte of the frame, and the hash
/// covers the whole frame, tag included.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
pub enum Artifact {
    /// Consensus code, its optional index mapper, and its optional view.
    Module(ModuleArtifact),
    /// A view alone: no component, no mapper, no consensus state.
    View(ViewArtifact),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
pub struct ModuleArtifact {
    pub component: Vec<u8>,
    pub index: Option<Vec<u8>>,
    /// `None` removes the view, including all its assets, at activation.
    pub view: Option<ViewArtifact>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize)]
pub struct ViewArtifact {
    pub component: Vec<u8>,
    /// Logical relative paths. No asset may also be another asset's directory.
    pub assets: BTreeMap<String, Vec<u8>>,
}

impl ModuleArtifact {
    pub fn component(component: Vec<u8>) -> Self {
        Self {
            component,
            index: None,
            view: None,
        }
    }
}

impl From<ModuleArtifact> for Artifact {
    fn from(module: ModuleArtifact) -> Self {
        Self::Module(module)
    }
}

impl From<ViewArtifact> for Artifact {
    fn from(view: ViewArtifact) -> Self {
        Self::View(view)
    }
}

impl Artifact {
    /// A module artifact of bare consensus code.
    pub fn module(component: Vec<u8>) -> Self {
        Self::Module(ModuleArtifact::component(component))
    }

    /// Serializes the owned value. Trust boundaries must validate with `decode`.
    pub fn encode(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("artifact serializes")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        Ok(match ArtifactRef::decode(bytes)? {
            ArtifactRef::Module(module) => Self::Module(ModuleArtifact {
                component: module.component.to_vec(),
                index: module.index.map(<[u8]>::to_vec),
                view: module.view.map(ViewArtifactRef::to_owned),
            }),
            ArtifactRef::View(view) => Self::View(view.to_owned()),
        })
    }

    pub fn hash(&self) -> [u8; 32] {
        Sha256::digest(self.encode()).into()
    }

    /// The view this artifact carries: a module's optional view, or the view
    /// itself. `None` only for a module without one.
    pub fn view(&self) -> Option<&ViewArtifact> {
        match self {
            Self::Module(module) => module.view.as_ref(),
            Self::View(view) => Some(view),
        }
    }
}

/// A validated view over the Borsh artifact frame, without copying payload bytes.
#[derive(Debug)]
pub enum ArtifactRef<'a> {
    Module(ModuleArtifactRef<'a>),
    View(ViewArtifactRef<'a>),
}

#[derive(Debug)]
pub struct ModuleArtifactRef<'a> {
    pub component: &'a [u8],
    pub index: Option<&'a [u8]>,
    pub view: Option<ViewArtifactRef<'a>>,
}

/// Only map nodes allocate; keys and payloads borrow the bounded artifact frame.
#[derive(Debug)]
pub struct ViewArtifactRef<'a> {
    pub component: &'a [u8],
    pub assets: BTreeMap<&'a str, &'a [u8]>,
}

impl ViewArtifactRef<'_> {
    pub fn to_owned(self) -> ViewArtifact {
        ViewArtifact {
            component: self.component.to_vec(),
            assets: self
                .assets
                .into_iter()
                .map(|(path, bytes)| (path.to_owned(), bytes.to_vec()))
                .collect(),
        }
    }
}

impl<'a> ArtifactRef<'a> {
    pub fn decode(mut bytes: &'a [u8]) -> Result<Self, String> {
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err("module artifact exceeds the 16 MiB frame limit".into());
        }
        let artifact = match take_tag(&mut bytes)? {
            MODULE_TAG => Self::Module(take_module(&mut bytes)?),
            VIEW_TAG => Self::View(take_view(&mut bytes)?),
            _ => return Err("module artifact has an invalid kind tag".into()),
        };
        if !bytes.is_empty() {
            return Err("module artifact has trailing bytes".into());
        }
        Ok(artifact)
    }

    /// The view this frame carries: a module's optional view, or the view
    /// itself. `None` only for a module without one.
    pub fn view(self) -> Option<ViewArtifactRef<'a>> {
        match self {
            Self::Module(module) => module.view,
            Self::View(view) => Some(view),
        }
    }
}

fn take_module<'a>(bytes: &mut &'a [u8]) -> Result<ModuleArtifactRef<'a>, String> {
    let component = take_bytes(bytes)?;
    let index = match take_tag(bytes)? {
        0 => None,
        1 => Some(take_bytes(bytes)?),
        _ => return Err("module artifact has an invalid mapper tag".into()),
    };
    let view = match take_tag(bytes)? {
        0 => None,
        1 => Some(take_view(bytes)?),
        _ => return Err("module artifact has an invalid view tag".into()),
    };
    Ok(ModuleArtifactRef {
        component,
        index,
        view,
    })
}

fn take_view<'a>(bytes: &mut &'a [u8]) -> Result<ViewArtifactRef<'a>, String> {
    let component = take_bytes(bytes)?;
    let count = take_length(bytes)?;
    if count > MAX_VIEW_ASSETS {
        return Err("module artifact has more than 4096 view assets".into());
    }
    let mut assets = BTreeMap::new();
    let mut previous = None;
    for _ in 0..count {
        let path = std::str::from_utf8(take_bytes(bytes)?)
            .map_err(|_| "module artifact asset path is not UTF-8")?;
        validate_asset_path(path)?;
        let out_of_order = previous.is_some_and(|previous| previous >= path);
        if out_of_order {
            return Err("module artifact asset paths are duplicated or out of order".into());
        }
        let file_ancestor = path
            .match_indices('/')
            .any(|(end, _)| assets.contains_key(&path[..end]));
        if file_ancestor {
            return Err("module artifact asset path has a file ancestor".into());
        }
        let payload = take_bytes(bytes)?;
        assets.insert(path, payload);
        previous = Some(path);
    }
    Ok(ViewArtifactRef { component, assets })
}

/// Canonical logical paths use nonempty UTF-8 slash-separated segments.
/// Empty, dot and parent segments, backslashes, colons and controls are refused.
/// No filesystem normalization or extraction is performed.
pub fn validate_asset_path(path: &str) -> Result<(), String> {
    let bad_length = path.is_empty() || path.len() > MAX_ASSET_PATH_BYTES;
    if bad_length {
        return Err("module artifact asset path exceeds its length bounds".into());
    }
    let bad_character = path
        .chars()
        .any(|c| c.is_control() || matches!(c, '\\' | ':'));
    let bad_segment = path.split('/').any(|part| matches!(part, "" | "." | ".."));
    if bad_character || bad_segment {
        return Err("module artifact asset path is not a bounded canonical relative path".into());
    }
    Ok(())
}

fn take_tag(bytes: &mut &[u8]) -> Result<u8, String> {
    let Some((&tag, tail)) = bytes.split_first() else {
        return Err("module artifact has a missing option tag".into());
    };
    *bytes = tail;
    Ok(tag)
}

fn take_length(bytes: &mut &[u8]) -> Result<usize, String> {
    let Some((length, tail)) = bytes.split_at_checked(4) else {
        return Err("module artifact has a truncated length".into());
    };
    *bytes = tail;
    Ok(u32::from_le_bytes(length.try_into().expect("four-byte length")) as usize)
}

fn take_bytes<'a>(bytes: &mut &'a [u8]) -> Result<&'a [u8], String> {
    let length = take_length(bytes)?;
    let Some((value, tail)) = bytes.split_at_checked(length) else {
        return Err("module artifact has a truncated body".into());
    };
    *bytes = tail;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewed() -> ModuleArtifact {
        ModuleArtifact {
            component: vec![1],
            index: Some(vec![2]),
            view: Some(ViewArtifact {
                component: vec![3],
                assets: BTreeMap::from([("icons/mark.svg".into(), vec![4, 5])]),
            }),
        }
    }

    fn view_only() -> ViewArtifact {
        ViewArtifact {
            component: vec![3],
            assets: BTreeMap::from([("icons/tab.svg".into(), vec![4, 5])]),
        }
    }

    // A Vec of pairs has the map's Borsh framing but lets hostile tests supply
    // duplicate or out-of-order keys that BTreeMap itself cannot produce.
    fn raw_view(assets: Vec<(String, Vec<u8>)>) -> Vec<u8> {
        borsh::to_vec(&(
            MODULE_TAG,
            vec![1u8],
            None::<Vec<u8>>,
            Some((vec![2u8], assets)),
        ))
        .unwrap()
    }

    fn raw_view_only(assets: Vec<(String, Vec<u8>)>) -> Vec<u8> {
        borsh::to_vec(&(VIEW_TAG, vec![2u8], assets)).unwrap()
    }

    #[test]
    fn the_tag_is_the_first_byte_and_names_the_arm() {
        let module = Artifact::Module(viewed());
        let view = Artifact::View(view_only());
        assert_eq!(module.encode()[0], MODULE_TAG);
        assert_eq!(view.encode()[0], VIEW_TAG);
        assert!(matches!(
            ArtifactRef::decode(&module.encode()).unwrap(),
            ArtifactRef::Module(_)
        ));
        assert!(matches!(
            ArtifactRef::decode(&view.encode()).unwrap(),
            ArtifactRef::View(_)
        ));
        assert_eq!(Artifact::decode(&module.encode()).unwrap(), module);
        assert_eq!(Artifact::decode(&view.encode()).unwrap(), view);
        assert_eq!(module.view(), viewed().view.as_ref());
        assert_eq!(view.view(), Some(&view_only()));
        assert_eq!(Artifact::module(vec![1]).view(), None);
    }

    #[test]
    fn a_view_artifact_is_one_commitment_over_view_and_assets() {
        let original = Artifact::View(view_only());
        let encoded = original.encode();
        let ArtifactRef::View(borrowed) = ArtifactRef::decode(&encoded).unwrap() else {
            panic!("view frame decoded as a module");
        };
        assert_eq!(borrowed.component, [3]);
        assert_eq!(borrowed.assets["icons/tab.svg"], [4, 5]);
        let frame = encoded.as_ptr() as usize..encoded.as_ptr() as usize + encoded.len();
        assert!(frame.contains(&(borrowed.component.as_ptr() as usize)));
        let mut changed_component = view_only();
        changed_component.component.push(6);
        let mut changed_asset = view_only();
        changed_asset.assets.insert("icons/other.svg".into(), vec![7]);
        let mut no_assets = view_only();
        no_assets.assets.clear();
        for variant in [changed_component, changed_asset, no_assets] {
            let variant = Artifact::View(variant);
            assert_ne!(variant.hash(), original.hash(), "view change escaped commitment");
            assert_eq!(Artifact::decode(&variant.encode()).unwrap(), variant);
        }
        // The same view embedded in a module is a different commitment: the
        // tag is covered by the hash.
        let embedded = Artifact::Module(ModuleArtifact {
            component: Vec::new(),
            index: None,
            view: Some(view_only()),
        });
        assert_ne!(embedded.hash(), original.hash());
        assert!(
            ArtifactRef::decode(&raw_view_only(vec![("a".into(), vec![])])).is_ok(),
            "canonical view-only frame must decode"
        );
        assert!(
            ArtifactRef::decode(&raw_view_only(vec![("b".into(), vec![]), ("a".into(), vec![])]))
                .is_err(),
            "view-only frame skipped asset validation"
        );
        let mut truncated_count = raw_view_only(Vec::new());
        truncated_count.truncate(truncated_count.len() - 1);
        assert!(ArtifactRef::decode(&truncated_count).is_err());
    }

    #[test]
    fn view_and_assets_are_one_commitment_and_borrow_the_frame() {
        let original = Artifact::Module(viewed());
        let encoded = original.encode();
        assert_eq!(Artifact::decode(&encoded).unwrap(), original);
        let ArtifactRef::Module(borrowed) = ArtifactRef::decode(&encoded).unwrap() else {
            panic!("module frame decoded as a view");
        };
        assert_eq!(borrowed.component, [1]);
        assert_eq!(borrowed.index.unwrap(), [2]);
        let view = borrowed.view.unwrap();
        assert_eq!(view.component, [3]);
        assert_eq!(view.assets["icons/mark.svg"], [4, 5]);
        let frame = encoded.as_ptr() as usize..encoded.as_ptr() as usize + encoded.len();
        assert!(frame.contains(&(view.component.as_ptr() as usize)));
        let (key, value) = view.assets.first_key_value().unwrap();
        assert!(frame.contains(&(key.as_ptr() as usize)));
        assert!(frame.contains(&(value.as_ptr() as usize)));
        let mut variants = Vec::new();
        let mut changed = viewed();
        changed.view.as_mut().unwrap().component.push(6);
        variants.push(changed);
        let mut changed = viewed();
        changed
            .view
            .as_mut()
            .unwrap()
            .assets
            .get_mut("icons/mark.svg")
            .unwrap()
            .push(6);
        variants.push(changed);
        let mut changed = viewed();
        changed.view.as_mut().unwrap().assets =
            BTreeMap::from([("icons/other.svg".into(), vec![4, 5])]);
        variants.push(changed);
        let mut changed = viewed();
        changed.view.as_mut().unwrap().assets.clear();
        variants.push(changed);
        let mut removed = viewed();
        removed.view = None;
        variants.push(removed);
        for variant in variants {
            let variant = Artifact::Module(variant);
            assert_ne!(
                variant.hash(),
                original.hash(),
                "view change escaped commitment"
            );
            assert_eq!(Artifact::decode(&variant.encode()).unwrap(), variant);
        }
        let mut empty_view = viewed();
        empty_view.view = Some(ViewArtifact {
            component: Vec::new(),
            assets: BTreeMap::new(),
        });
        assert!(
            Artifact::decode(&Artifact::Module(empty_view).encode())
                .unwrap()
                .view()
                .is_some(),
            "only None is removal"
        );
    }

    #[test]
    fn only_canonical_bounded_asset_paths_are_admitted() {
        for path in [
            "", "/a", "a/", "a//b", ".", "..", "a/./b", "a/../b", "a\\b", "C:a", "a\0b", "a\nb",
        ] {
            assert!(
                ArtifactRef::decode(&raw_view(vec![(path.into(), vec![])])).is_err(),
                "accepted path {path:?}"
            );
        }
        let longest = "a".repeat(MAX_ASSET_PATH_BYTES);
        for path in ["icons/마크.svg", longest.as_str()] {
            assert!(ArtifactRef::decode(&raw_view(vec![(path.into(), vec![])])).is_ok());
        }
        assert!(
            ArtifactRef::decode(&raw_view(vec![(
                "a".repeat(MAX_ASSET_PATH_BYTES + 1),
                vec![]
            )]))
            .is_err()
        );
        for keys in [["a", "a"], ["b", "a"]] {
            let bytes = raw_view(keys.into_iter().map(|key| (key.into(), vec![])).collect());
            assert!(
                ArtifactRef::decode(&bytes).is_err(),
                "accepted noncanonical key order {keys:?}"
            );
        }
    }

    #[test]
    fn file_ancestor_collisions_are_refused() {
        // The intervening key makes a previous-key-only prefix check insufficient.
        let bytes = raw_view(
            ["a", "a-b", "a/b"]
                .into_iter()
                .map(|key| (key.into(), vec![1]))
                .collect(),
        );
        assert!(
            ArtifactRef::decode(&bytes).is_err(),
            "file/ancestor collision accepted"
        );
        let siblings = raw_view(
            ["a/b", "a/c", "ab"]
                .into_iter()
                .map(|key| (key.into(), vec![1]))
                .collect(),
        );
        assert!(
            ArtifactRef::decode(&siblings).is_ok(),
            "distinct sibling assets rejected"
        );
    }

    #[test]
    fn asset_count_is_bounded_before_reading_entries() {
        let bytes = borsh::to_vec(&(
            MODULE_TAG,
            vec![1u8],
            None::<Vec<u8>>,
            1u8,
            vec![2u8],
            u32::MAX,
        ))
        .unwrap();
        let error = ArtifactRef::decode(&bytes).unwrap_err();
        assert!(
            error.contains("4096 view assets"),
            "must reject count before entries: {error}"
        );
        let assets: Vec<_> = (0..MAX_VIEW_ASSETS)
            .map(|i| (format!("{i:04}"), vec![]))
            .collect();
        assert_eq!(
            ArtifactRef::decode(&raw_view(assets))
                .unwrap()
                .view()
                .unwrap()
                .assets
                .len(),
            MAX_VIEW_ASSETS
        );
    }

    #[test]
    fn total_frame_limit_includes_all_encoded_bytes() {
        // One tag byte, a four-byte component length, and the two absent-option tags.
        let exact = Artifact::module(vec![0; MAX_ARTIFACT_BYTES - 7]).encode();
        assert_eq!(exact.len(), MAX_ARTIFACT_BYTES);
        assert!(ArtifactRef::decode(&exact).is_ok());
        let oversized = Artifact::module(vec![0; MAX_ARTIFACT_BYTES - 6]).encode();
        assert!(
            ArtifactRef::decode(&oversized).is_err(),
            "oversized frame accepted"
        );
        let mut aggregate = viewed();
        aggregate
            .view
            .as_mut()
            .unwrap()
            .assets
            .insert("large".into(), vec![0; MAX_ARTIFACT_BYTES]);
        assert!(
            ArtifactRef::decode(&Artifact::Module(aggregate).encode()).is_err(),
            "aggregate view bytes escaped frame limit"
        );
    }

    #[test]
    fn hostile_view_lengths_tags_and_previous_format_are_rejected() {
        let bytes = Artifact::Module(viewed()).encode();
        for end in 0..bytes.len() {
            assert!(
                ArtifactRef::decode(&bytes[..end]).is_err(),
                "accepted truncated frame at {end}"
            );
        }
        let untagged = borsh::to_vec(&(vec![1u8], None::<Vec<u8>>, None::<Vec<u8>>)).unwrap();
        assert!(
            ArtifactRef::decode(&untagged).is_err(),
            "untagged artifact format accepted"
        );
        let mut invalid = raw_view(vec![("a".into(), vec![])]);
        invalid[7] = 2; // view option tag after the kind tag, component and mapper tag
        assert!(ArtifactRef::decode(&invalid).is_err());
        let mut invalid = raw_view(vec![("a".into(), vec![])]);
        invalid[21] = 255; // asset path's first byte
        assert!(
            ArtifactRef::decode(&invalid).is_err(),
            "invalid UTF-8 path accepted"
        );
        assert!(
            ArtifactRef::decode(&[255; 4]).is_err(),
            "hostile length accepted"
        );
        assert!(
            ArtifactRef::decode(&Artifact::Module(viewed()).encode()).is_ok(),
            "refusals poisoned next decode"
        );
    }

    #[test]
    fn a_canonical_view_frame_is_accepted() {
        let assets = std::collections::BTreeMap::from([("icons/mark.svg", vec![4u8, 5])]);
        let bytes = borsh::to_vec(&(
            MODULE_TAG,
            vec![1u8],
            None::<Vec<u8>>,
            Some((vec![2u8], assets)),
        ))
        .unwrap();
        assert!(
            ArtifactRef::decode(&bytes).is_ok(),
            "canonical view artifact must decode"
        );
    }

    #[test]
    fn one_commitment_covers_both_components_and_mapper_removal() {
        let bare = ModuleArtifact::component(vec![1, 2, 3]);
        let indexed = ModuleArtifact {
            index: Some(vec![4, 5]),
            ..bare.clone()
        };
        let changed_mapper = ModuleArtifact {
            index: Some(vec![6]),
            ..bare.clone()
        };
        let changed_code = ModuleArtifact {
            component: vec![7],
            ..indexed.clone()
        };
        let indexed = Artifact::Module(indexed);
        for other in [bare.clone(), changed_mapper.clone(), changed_code.clone()] {
            assert_ne!(indexed.hash(), Artifact::Module(other).hash());
        }
        for artifact in [Artifact::Module(bare), indexed, Artifact::Module(changed_mapper), Artifact::Module(changed_code)] {
            assert_eq!(Artifact::decode(&artifact.encode()).unwrap(), artifact);
        }
    }

    #[test]
    fn malformed_tags_and_trailing_frames_are_rejected() {
        let bytes = Artifact::module(vec![1, 2, 3]).encode();
        for end in 0..bytes.len() {
            assert!(Artifact::decode(&bytes[..end]).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(Artifact::decode(&trailing).is_err());
        let mut invalid_view_tag = bytes.clone();
        *invalid_view_tag.last_mut().unwrap() = 2;
        assert!(Artifact::decode(&invalid_view_tag).is_err());
        for tag in 2..=u8::MAX {
            let mut invalid_kind_tag = bytes.clone();
            invalid_kind_tag[0] = tag;
            let error = Artifact::decode(&invalid_kind_tag).unwrap_err();
            assert!(error.contains("invalid kind tag"), "tag {tag}: {error}");
        }
        // A module frame's body under the view tag has the wrong shape.
        let mut wrong_arm = Artifact::Module(viewed()).encode();
        wrong_arm[0] = VIEW_TAG;
        assert!(Artifact::decode(&wrong_arm).is_err());
        let mut trailing_view = Artifact::View(view_only()).encode();
        trailing_view.push(0);
        assert!(Artifact::decode(&trailing_view).is_err());
        assert!(Artifact::decode(b"\0asm\r\0\x01\0").is_err());
        assert!(Artifact::decode(&[]).is_err());
    }
}
