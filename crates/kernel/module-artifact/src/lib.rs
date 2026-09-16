//! One deployable unit: a module (consensus code, optional index mapper,
//! optional view) or a view alone. The frame says which; the registry commits
//! the hash of the whole frame and activates it once.
use std::collections::BTreeMap;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The complete encoded artifact is bounded like the node's staging lane.
pub const MAX_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_VIEW_ASSETS: usize = 4096;
pub const MAX_ASSET_PATH_BYTES: usize = 1024;
/// How many data-plane lanes one module may declare. A lane is a pair of
/// well-known overlay ports on every node of the network, and the registry
/// only has 99 ids to hand out in total — a module wanting more than a
/// handful is describing a protocol, not a deployment.
pub const MAX_ARTIFACT_LANES: usize = 8;
/// A lane name is a short token, not free text: it lands in the registry's
/// root-hashed lane table, which every node reads.
pub const MAX_LANE_NAME_BYTES: usize = 32;

/// What a module asks the network for: one data-plane lane, by the name its
/// host binds it under.
///
/// This lives HERE, in the frame, because the frame is what a module ships.
/// A deployment that declared its lanes anywhere else — a table in the node
/// binary, a field in a genesis file — would make the network's answer to
/// "which lanes does this module have" depend on something other than the
/// bytes its hash covers.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneDecl {
    /// The id this lane wants. It decides two overlay ports on every node, so
    /// the registry — not the frame — is what refuses a taken or reserved one.
    pub id: u8,
    /// What this lane IS to the module that declares it: `voice`, `video`,
    /// `telemetry`. The host binds by `(module_id, name)`, never by position
    /// — a binding that depended on declaration order would break silently
    /// the first time a module declared a third lane.
    pub name: String,
    /// `None` is datagram-only — sockets, no stream plane.
    pub stream: Option<LaneStream>,
}

/// The stream half of a lane: present only when the lane carries one.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneStream {
    pub pacing: LanePacing,
    /// Accepted-but-unclaimed inbound streams before the plane refuses.
    pub accept_backlog: u32,
}

/// Whether a lane's streams share this process's one link budget or run to a
/// ceiling of their own.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum LanePacing {
    /// Joins the process-wide bulk budget every other shared lane draws from.
    Shared,
    /// Names its own ceiling and burst, in bytes per second and bytes.
    Local {
        bulk_bytes_per_sec: u64,
        bulk_burst_bytes: u64,
    },
}

/// A lane name is 1..=32 bytes of `[a-z0-9_]`. Bounded and lowercase because
/// it is committed state every node reads, and a declaration must not be able
/// to grow the lane table with a long string.
pub fn lane_name_is_well_formed(name: &str) -> bool {
    let within_bound = !name.is_empty() && name.len() <= MAX_LANE_NAME_BYTES;
    within_bound
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

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
    /// The data-plane lanes this deployment asks for, ascending by id and
    /// unique by both id and name within the frame. Empty for the modules
    /// that want none, which is most of them.
    pub lanes: Vec<LaneDecl>,
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
            lanes: Vec::new(),
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
                lanes: module.lanes,
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
    /// Owned, unlike the payloads: a lane declaration is a handful of bytes
    /// and every reader wants it as values, so borrowing would buy nothing
    /// and cost every caller a lifetime.
    pub lanes: Vec<LaneDecl>,
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
    let lanes = take_lanes(bytes)?;
    Ok(ModuleArtifactRef {
        component,
        index,
        view,
        lanes,
    })
}

/// The declared lane list, validated as it is read.
///
/// Canonical like the asset map above it, and for the same reason: the frame
/// is hashed, so one declaration must have exactly one encoding. Ascending by
/// id with no repeats gives that, and rejecting a duplicate NAME here means
/// the registry never has to answer which of two same-named lanes a host
/// should bind. The count is bounded BEFORE any entry is read.
fn take_lanes(bytes: &mut &[u8]) -> Result<Vec<LaneDecl>, String> {
    let count = take_length(bytes)?;
    if count > MAX_ARTIFACT_LANES {
        return Err(format!(
            "module artifact declares more than {MAX_ARTIFACT_LANES} lanes"
        ));
    }
    let mut lanes: Vec<LaneDecl> = Vec::with_capacity(count);
    for _ in 0..count {
        let Some((&id, tail)) = bytes.split_first() else {
            return Err("module artifact has a truncated lane id".into());
        };
        *bytes = tail;
        let name = std::str::from_utf8(take_bytes(bytes)?)
            .map_err(|_| "module artifact lane name is not UTF-8")?
            .to_owned();
        let stream = match take_tag(bytes)? {
            0 => None,
            1 => Some(take_lane_stream(bytes)?),
            _ => return Err("module artifact has an invalid lane stream tag".into()),
        };
        lanes.push(LaneDecl { id, name, stream });
    }
    validate_lanes(&lanes)?;
    Ok(lanes)
}

/// The rules a declared lane set obeys, wherever it comes from — decoded off
/// a frame, or read out of a module's own declaration before one is built.
///
/// ONE encoding per declaration is the point: the frame is hashed, so a set
/// that could be spelled two ways would be two deployments of one module.
/// Ascending by id with no repeats gives that. A duplicate NAME is refused
/// for a different reason — the host binds by `(module_id, name)`, so two
/// lanes of one name is a lookup with two answers.
pub fn validate_lanes(lanes: &[LaneDecl]) -> Result<(), String> {
    if lanes.len() > MAX_ARTIFACT_LANES {
        return Err(format!(
            "module artifact declares more than {MAX_ARTIFACT_LANES} lanes"
        ));
    }
    for (position, lane) in lanes.iter().enumerate() {
        if !lane_name_is_well_formed(&lane.name) {
            return Err(format!(
                "module artifact lane name {:?} is malformed: \
                 1..={MAX_LANE_NAME_BYTES} bytes of [a-z0-9_]",
                lane.name
            ));
        }
        let earlier = &lanes[..position];
        let out_of_order = earlier
            .last()
            .is_some_and(|previous| previous.id >= lane.id);
        if out_of_order {
            return Err("module artifact lanes are duplicated or out of order".into());
        }
        let name_taken = earlier.iter().any(|other| other.name == lane.name);
        if name_taken {
            return Err(format!(
                "module artifact declares two lanes named {:?}",
                lane.name
            ));
        }
    }
    Ok(())
}

fn take_lane_stream(bytes: &mut &[u8]) -> Result<LaneStream, String> {
    let pacing = match take_tag(bytes)? {
        0 => LanePacing::Shared,
        1 => LanePacing::Local {
            bulk_bytes_per_sec: take_u64(bytes)?,
            bulk_burst_bytes: take_u64(bytes)?,
        },
        _ => return Err("module artifact has an invalid lane pacing tag".into()),
    };
    let accept_backlog = take_u32(bytes)?;
    Ok(LaneStream {
        pacing,
        accept_backlog,
    })
}

fn take_u64(bytes: &mut &[u8]) -> Result<u64, String> {
    let Some((value, tail)) = bytes.split_at_checked(8) else {
        return Err("module artifact has a truncated lane budget".into());
    };
    *bytes = tail;
    Ok(u64::from_le_bytes(value.try_into().expect("eight bytes")))
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
    Ok(take_u32(bytes)? as usize)
}

fn take_u32(bytes: &mut &[u8]) -> Result<u32, String> {
    let Some((value, tail)) = bytes.split_at_checked(4) else {
        return Err("module artifact has a truncated length".into());
    };
    *bytes = tail;
    Ok(u32::from_le_bytes(value.try_into().expect("four bytes")))
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
            lanes: Vec::new(),
        }
    }

    /// the two shapes a lane comes in: datagram-only, and a stream half that
    /// names its own budget.
    fn lanes() -> Vec<LaneDecl> {
        vec![
            LaneDecl {
                id: 2,
                name: "voice".into(),
                stream: None,
            },
            LaneDecl {
                id: 5,
                name: "telemetry".into(),
                stream: Some(LaneStream {
                    pacing: LanePacing::Local {
                        bulk_bytes_per_sec: 24 * 1024 * 1024,
                        bulk_burst_bytes: 512 * 1024,
                    },
                    accept_backlog: 64,
                }),
            },
        ]
    }

    /// one lane in the frame's own field order: id, name, and the optional
    /// stream's (pacing tag, backlog).
    type RawLane = (u8, String, Option<(u8, u32)>);

    /// a Vec of raw lane tuples has the frame's borsh framing but lets a
    /// hostile test supply an order or a name the owned type cannot produce.
    fn raw_lanes(lanes: Vec<RawLane>) -> Vec<u8> {
        borsh::to_vec(&(
            MODULE_TAG,
            vec![1u8],
            None::<Vec<u8>>,
            None::<Vec<u8>>,
            lanes,
        ))
        .unwrap()
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
            Vec::<RawLane>::new(),
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
        changed_asset
            .assets
            .insert("icons/other.svg".into(), vec![7]);
        let mut no_assets = view_only();
        no_assets.assets.clear();
        for variant in [changed_component, changed_asset, no_assets] {
            let variant = Artifact::View(variant);
            assert_ne!(
                variant.hash(),
                original.hash(),
                "view change escaped commitment"
            );
            assert_eq!(Artifact::decode(&variant.encode()).unwrap(), variant);
        }
        // The same view embedded in a module is a different commitment: the
        // tag is covered by the hash.
        let embedded = Artifact::Module(ModuleArtifact {
            component: Vec::new(),
            index: None,
            view: Some(view_only()),
            lanes: Vec::new(),
        });
        assert_ne!(embedded.hash(), original.hash());
        assert!(
            ArtifactRef::decode(&raw_view_only(vec![("a".into(), vec![])])).is_ok(),
            "canonical view-only frame must decode"
        );
        assert!(
            ArtifactRef::decode(&raw_view_only(vec![
                ("b".into(), vec![]),
                ("a".into(), vec![])
            ]))
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
    fn declared_lanes_are_one_commitment_with_the_code() {
        // the lanes a deployment asks for ride the SAME hash as its bytes, so
        // a network cannot be handed code that quietly wants a different set
        // of ports than the frame it approved.
        let declared = Artifact::Module(ModuleArtifact {
            lanes: lanes(),
            ..viewed()
        });
        assert_eq!(Artifact::decode(&declared.encode()).unwrap(), declared);
        assert_ne!(
            declared.hash(),
            Artifact::Module(viewed()).hash(),
            "a lane declaration escaped the commitment"
        );
        let mut renamed = lanes();
        renamed[0].name = "audio".into();
        let mut renumbered = lanes();
        renumbered[0].id = 3;
        let mut repaced = lanes();
        repaced[1].stream.as_mut().unwrap().pacing = LanePacing::Shared;
        let mut rebacklogged = lanes();
        rebacklogged[1].stream.as_mut().unwrap().accept_backlog = 65;
        let mut dropped = lanes();
        dropped.pop();
        for variant in [renamed, renumbered, repaced, rebacklogged, dropped] {
            let variant = Artifact::Module(ModuleArtifact {
                lanes: variant,
                ..viewed()
            });
            assert_ne!(
                variant.hash(),
                declared.hash(),
                "lane change escaped the hash"
            );
            assert_eq!(Artifact::decode(&variant.encode()).unwrap(), variant);
        }
    }

    #[test]
    fn a_lane_list_is_canonical_bounded_and_named() {
        let good = |id, name: &str| (id, name.to_string(), None);
        assert!(ArtifactRef::decode(&raw_lanes(vec![good(2, "voice"), good(3, "video")])).is_ok());
        // ONE encoding per declaration, exactly like the asset map: ascending
        // by id, no repeats. Otherwise two frames with the same meaning would
        // hash differently and be two deployments.
        for bad_order in [
            vec![good(3, "video"), good(2, "voice")],
            vec![good(2, "voice"), good(2, "video")],
        ] {
            assert!(
                ArtifactRef::decode(&raw_lanes(bad_order)).is_err(),
                "accepted a non-canonical lane order"
            );
        }
        // a duplicate NAME is refused here so the registry never has to answer
        // which of two same-named lanes a host should bind.
        assert!(ArtifactRef::decode(&raw_lanes(vec![good(2, "voice"), good(3, "voice")])).is_err());
        for bad_name in ["", "Voice", "voice-lane", "voice lane", "보이스"] {
            assert!(
                ArtifactRef::decode(&raw_lanes(vec![good(2, bad_name)])).is_err(),
                "accepted lane name {bad_name:?}"
            );
        }
        let longest = "a".repeat(MAX_LANE_NAME_BYTES);
        assert!(ArtifactRef::decode(&raw_lanes(vec![good(2, &longest)])).is_ok());
        assert!(
            ArtifactRef::decode(&raw_lanes(vec![good(
                2,
                &"a".repeat(MAX_LANE_NAME_BYTES + 1)
            )]))
            .is_err()
        );
        // the count is bounded BEFORE any entry is read — a hostile count
        // must not get to allocate against it.
        let hostile_count = borsh::to_vec(&(
            MODULE_TAG,
            vec![1u8],
            None::<Vec<u8>>,
            None::<Vec<u8>>,
            u32::MAX,
        ))
        .unwrap();
        let error = ArtifactRef::decode(&hostile_count).unwrap_err();
        assert!(error.contains("more than 8 lanes"), "{error}");
        let full: Vec<_> = (0..MAX_ARTIFACT_LANES)
            .map(|i| good(i as u8 + 1, &format!("lane{i}")))
            .collect();
        assert!(ArtifactRef::decode(&raw_lanes(full)).is_ok());
        let over: Vec<_> = (0..=MAX_ARTIFACT_LANES)
            .map(|i| good(i as u8 + 1, &format!("lane{i}")))
            .collect();
        assert!(ArtifactRef::decode(&raw_lanes(over)).is_err());
        // and a truncated declaration is refused at every cut.
        let bytes = Artifact::Module(ModuleArtifact {
            lanes: lanes(),
            ..viewed()
        })
        .encode();
        for end in 0..bytes.len() {
            assert!(
                ArtifactRef::decode(&bytes[..end]).is_err(),
                "accepted a frame truncated at {end}"
            );
        }
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
        // One tag byte, a four-byte component length, the two absent-option
        // tags, and the empty lane list's four-byte count.
        let exact = Artifact::module(vec![0; MAX_ARTIFACT_BYTES - 11]).encode();
        assert_eq!(exact.len(), MAX_ARTIFACT_BYTES);
        assert!(ArtifactRef::decode(&exact).is_ok());
        let oversized = Artifact::module(vec![0; MAX_ARTIFACT_BYTES - 10]).encode();
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
            Vec::<RawLane>::new(),
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
        for artifact in [
            Artifact::Module(bare),
            indexed,
            Artifact::Module(changed_mapper),
            Artifact::Module(changed_code),
        ] {
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
