//! Fetch the view belonging to an already-selected registry deployment: a
//! module's embedded view, or a view-only artifact itself.
//!
//! The caller retains the expected hash and request generation, and rechecks
//! both before installing the result. This loader never selects a deployment
//! or substitutes a desktop resource after a failed fetch.

use std::fmt;

use ducktape_rpc::Client;
use module_artifact::{
    Artifact, ArtifactRef, MAX_ARTIFACT_BYTES, ModuleArtifact, ViewArtifact, ViewArtifactRef,
};
use sha2::{Digest as _, Sha256};

/// What an artifact frame is — a module (whose frame may embed a view) or
/// a view alone — as the frame's own tag says and the registry entry's
/// `kind` names, fixed at admission.
#[derive(serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Module,
    View,
}

#[derive(Debug)]
pub enum Error {
    /// The local node does not hold the blob: the bytes have not reached
    /// it (yet).
    NotHeld,
    Transport(ducktape_rpc::Error),
    HashMismatch,
    InvalidArtifact(String),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotHeld => formatter.write_str("view artifact not held by the node"),
            Self::Transport(error) => write!(formatter, "fetch view artifact: {error}"),
            Self::HashMismatch => formatter.write_str("view artifact deployment hash mismatch"),
            Self::InvalidArtifact(error) => write!(formatter, "invalid view artifact: {error}"),
        }
    }
}

impl std::error::Error for Error {}

/// One verified artifact frame, reduced to what a seat asks of it: what
/// it is, the identity of its consensus half, and its view. A module's
/// `core` is the hash of the same frame with its view removed — two
/// frames with equal cores run the same component and index byte for
/// byte, so a view read out of one is honest against the other's running
/// core. A view-only frame has no core.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub kind: Kind,
    pub core: Option<[u8; 32]>,
    pub view: Option<ViewArtifact>,
}

/// The frame under `expected_hash`, fetched off the node and verified: a
/// 404 is [`Error::NotHeld`], bytes that do not hash to it
/// [`Error::HashMismatch`].
pub async fn fetch(client: &Client, expected_hash: [u8; 32]) -> Result<Frame, Error> {
    let bytes = client
        .get_blob(&expected_hash, MAX_ARTIFACT_BYTES)
        .await
        .map_err(|error| {
            // the client's blob get carries the node's status line in its
            // one string: `RPC blob get returned 404 Not Found`
            let not_found = error.to_string().ends_with("returned 404 Not Found");
            match not_found {
                true => Error::NotHeld,
                false => Error::Transport(error),
            }
        })?;
    verified_frame(&bytes, expected_hash)
}

fn verified_frame(bytes: &[u8], expected_hash: [u8; 32]) -> Result<Frame, Error> {
    let actual_hash: [u8; 32] = Sha256::digest(bytes).into();
    if actual_hash != expected_hash {
        return Err(Error::HashMismatch);
    }
    // both arms carry a view the same way: a module's optional one, or the
    // view-only frame itself.
    let artifact = ArtifactRef::decode(bytes).map_err(Error::InvalidArtifact)?;
    Ok(match artifact {
        ArtifactRef::Module(module) => {
            let core = Artifact::Module(ModuleArtifact {
                component: module.component.to_vec(),
                index: module.index.map(<[u8]>::to_vec),
                view: None,
                // the lanes are the CORE's, not the view's: a view has no
                // sockets, and a deployment that asks for different lanes is
                // a different core even when its component bytes match.
                lanes: module.lanes,
            });
            Frame {
                kind: Kind::Module,
                core: Some(core.hash()),
                view: module.view.map(ViewArtifactRef::to_owned),
            }
        }
        ArtifactRef::View(view) => Frame {
            kind: Kind::View,
            core: None,
            view: Some(view.to_owned()),
        },
    })
}

/// Only a verified module artifact with no view is `Ok(None)`; a view-only
/// artifact always carries one.
#[cfg(test)]
fn verified_view(bytes: &[u8], expected_hash: [u8; 32]) -> Result<Option<ViewArtifact>, Error> {
    verified_frame(bytes, expected_hash).map(|frame| frame.view)
}

#[cfg(test)]
mod tests {
    use super::*;
    use module_artifact::{Artifact, ModuleArtifact};

    #[test]
    fn a_verified_view_preserves_assets_and_removal_is_explicit() {
        let view = ViewArtifact {
            component: vec![4, 5, 6],
            assets: [("icons/action.svg".to_owned(), b"<svg/>".to_vec())].into(),
        };
        let artifact = Artifact::Module(ModuleArtifact {
            component: vec![1, 2, 3],
            index: None,
            view: Some(view.clone()),
            lanes: Vec::new(),
        });
        assert_eq!(
            verified_view(&artifact.encode(), artifact.hash()).unwrap(),
            Some(view.clone())
        );
        let removed = Artifact::Module(ModuleArtifact {
            component: vec![1, 2, 3],
            index: None,
            view: None,
            lanes: Vec::new(),
        });
        assert_eq!(
            verified_view(&removed.encode(), removed.hash()).unwrap(),
            None
        );
        let view_only = Artifact::View(view.clone());
        assert_eq!(
            verified_view(&view_only.encode(), view_only.hash()).unwrap(),
            Some(view)
        );
    }

    #[test]
    fn a_frames_core_is_the_module_without_its_view() {
        let view = ViewArtifact {
            component: vec![4, 5, 6],
            assets: Default::default(),
        };
        let without = Artifact::Module(ModuleArtifact {
            component: vec![1, 2, 3],
            index: Some(vec![9]),
            view: None,
            lanes: Vec::new(),
        });
        let with = Artifact::Module(ModuleArtifact {
            component: vec![1, 2, 3],
            index: Some(vec![9]),
            view: Some(view.clone()),
            lanes: Vec::new(),
        });
        let other_core = Artifact::Module(ModuleArtifact {
            component: vec![1, 2, 3, 4],
            index: Some(vec![9]),
            view: Some(view.clone()),
            lanes: Vec::new(),
        });
        let frame =
            |artifact: &Artifact| verified_frame(&artifact.encode(), artifact.hash()).unwrap();
        assert_eq!(frame(&with).core, Some(without.hash()));
        assert_eq!(frame(&without).core, Some(without.hash()));
        assert_ne!(frame(&other_core).core, frame(&with).core);
        let view_only = Artifact::View(view.clone());
        assert_eq!(
            frame(&view_only),
            Frame {
                kind: Kind::View,
                core: None,
                view: Some(view),
            }
        );
    }

    #[test]
    fn tampering_and_malformed_artifacts_are_errors_not_removal() {
        let artifact = Artifact::module(vec![1, 2, 3]);
        let mut bytes = artifact.encode();
        bytes[5] ^= 1;
        assert!(matches!(
            verified_view(&bytes, artifact.hash()),
            Err(Error::HashMismatch)
        ));
        let malformed = b"not an artifact";
        assert!(matches!(
            verified_view(malformed, Sha256::digest(malformed).into()),
            Err(Error::InvalidArtifact(_))
        ));
    }
}
