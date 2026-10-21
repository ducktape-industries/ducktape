//! The bridge's local state file: the last attribution seq it settled, each place's product
//! session, whether it may still hold the place, its open inputs, and the wakes it waits on,
//! rewritten whole after every step.
use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Where {
    pub(crate) channel: String,
    pub(crate) thread: u64,
}

impl Where {
    pub(crate) fn key(&self) -> String {
        format!("chat/{}/{}", self.channel, self.thread)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Input {
    pub(crate) seq: u64,
    pub(crate) key: String,
    pub(crate) text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Wake {
    Clear(String),
    Model(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeptPlace {
    pub(crate) at: Where,
    pub(crate) session: Option<String>,
    pub(crate) held: bool,
    pub(crate) open: Vec<Input>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Kept {
    pub(crate) last: u64,
    pub(crate) after: Option<String>,
    pub(crate) places: BTreeMap<String, KeptPlace>,
    pub(crate) wakes: BTreeMap<u64, Wake>,
    pub(crate) unclaimed: BTreeMap<u64, Input>,
}

pub(crate) fn load(path: &Path) -> Result<Option<Kept>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|fault| format!("the state file {} does not read: {fault}", path.display())),
        Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(fault) => Err(format!(
            "the state file {} does not open: {fault}",
            path.display()
        )),
    }
}

pub(crate) fn save(path: &Path, kept: &Kept) -> Result<(), String> {
    let mut fresh = path.as_os_str().to_owned();
    fresh.push(".new");
    let bytes = serde_json::to_vec_pretty(kept).map_err(|fault| fault.to_string())?;
    std::fs::write(&fresh, bytes)
        .and_then(|()| std::fs::rename(&fresh, path))
        .map_err(|fault| format!("the state file {} is not written: {fault}", path.display()))
}
