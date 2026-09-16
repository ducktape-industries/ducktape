//! Tasting a proposed view: the taste set as the seats see it, and the
//! device's preference for it.
//!
//! The taste set (`view_source::taste_set`) is every `(module, hash)` an
//! open code proposal or a scheduled swap names. Walked here on connect and
//! on every block, each row is judged for whether this app can honestly
//! draw its view against the core that runs: a `Module` frame is tasteable
//! only when its consensus half is byte-identical to the active frame's
//! (`core_changes_too` otherwise), a `View` frame always is, and a frame
//! the node does not hold, does not verify, is not the entry's kind, ships
//! no view or speaks another wire protocol is refused by name. A refused
//! row is listed with its reason and never seated.
//!
//! What a member tastes is theirs alone: `prefs.json` keeps one hash per
//! chain per module, and the seat re-reads it at every connect. Nothing
//! here writes to the network.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

use crate::backend::view_source::{self, Entry, Kind, Stage, Tasteable};

/// Why a tasteable row cannot be seated on this app. Each is a stable
/// snake_case token in the `view_source` log line and the row's props.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The proposal admits a module the registry does not hold yet:
    /// there is no seat to taste it in.
    NotRegistered,
    /// The local node does not hold the bytes (yet).
    NotHeld,
    HashMismatch,
    InvalidArtifact,
    /// The frame's tag is not the registry entry's kind.
    KindMismatch,
    /// A module frame whose component or index differs from the active
    /// frame's: its view was built for a core that is not running.
    CoreChangesToo,
    WireProtocol,
    /// A module frame that removes the view.
    NoView,
}

impl Refusal {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::NotRegistered => "not_registered",
            Self::NotHeld => "not_held",
            Self::HashMismatch => "hash_mismatch",
            Self::InvalidArtifact => "invalid_artifact",
            Self::KindMismatch => "kind_mismatch",
            Self::CoreChangesToo => "core_changes_too",
            Self::WireProtocol => "wire_protocol",
            Self::NoView => "no_view",
        }
    }
}

/// One row of the taste set as the seats and the views see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) module: &'static str,
    pub(crate) hash: [u8; 32],
    pub(crate) proposal: Option<String>,
    pub(crate) stage: Stage,
    pub(crate) refusal: Option<Refusal>,
}

/// The taste set as last walked, and the chain it was walked on.
#[derive(Default)]
struct State {
    chain_id: String,
    rows: Vec<Row>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(Mutex::default)
}

/// Every row of the taste set as last walked.
pub(crate) fn rows() -> Vec<Row> {
    state().lock().expect("taste set").rows.clone()
}

/// The chain the taste set was last walked on: the key the preference is
/// kept under.
pub(crate) fn chain_id() -> String {
    state().lock().expect("taste set").chain_id.clone()
}

/// Whether `(module, hash)` is in the taste set at all — refused or not.
/// A seat tasting a hash that left the set returns to the active view.
pub(crate) fn listed(module: &str, hash: [u8; 32]) -> bool {
    state()
        .lock()
        .expect("taste set")
        .rows
        .iter()
        .any(|row| row.module == module && row.hash == hash)
}

/// Why `(module, hash)` cannot be seated: a refusal, or `not_listed` for a
/// pair the taste set does not name. `None` is a row a seat may take.
pub(crate) fn refused(module: &str, hash: [u8; 32]) -> Option<&'static str> {
    let rows = &state().lock().expect("taste set").rows;
    let row = rows
        .iter()
        .find(|row| row.module == module && row.hash == hash);
    match row {
        None => Some("not_listed"),
        Some(row) => row.refusal.map(Refusal::reason),
    }
}

/// Walks the taste set off `client` against `entries` (the registry as the
/// same tick read it): reads the proposals and the scheduled swaps, holds
/// every named frame and the active one beside it, judges each row, and
/// makes the result the taste set. The chain read with it is returned.
pub(crate) async fn refresh(
    client: &ducktape_rpc::Client,
    entries: &BTreeMap<String, Entry>,
) -> Result<view_source::Chain, view_source::Error> {
    let (chain, tasteable) = view_source::taste_set(client, entries).await?;
    let mut rows = Vec::with_capacity(tasteable.len());
    // the active frames stay held (those held at all — a walk fetches one
    // only beside a tasteable row): the way back from a taste is a swap
    // even when the taste was withdrawn on the same tick
    let mut kept: BTreeSet<[u8; 32]> = entries.values().filter_map(|entry| entry.hash).collect();
    for row in tasteable {
        let refusal = judge(client, entries, &row, &mut kept).await;
        rows.push(Row {
            module: super::intern(&row.module),
            hash: row.hash,
            proposal: row.proposal,
            stage: row.stage,
            refusal,
        });
    }
    view_source::retain_frames(&kept);
    let mut state = state().lock().expect("taste set");
    state.chain_id = chain.id.clone();
    state.rows = rows;
    Ok(chain)
}

/// One row judged: the frames it needs held (their hashes added to
/// `kept`), and why it cannot be seated, if it cannot.
async fn judge(
    client: &ducktape_rpc::Client,
    entries: &BTreeMap<String, Entry>,
    row: &Tasteable,
    kept: &mut BTreeSet<[u8; 32]>,
) -> Option<Refusal> {
    let Some(entry) = entries.get(&row.module) else {
        return Some(Refusal::NotRegistered);
    };
    let Some(active) = entry.hash else {
        return Some(Refusal::NotRegistered);
    };
    let proposed = match view_source::hold_frame(client, row.hash).await {
        Ok(frame) => frame,
        Err(error) => return Some(fetch_refusal(&error)),
    };
    kept.insert(row.hash);
    let kind_matches = proposed.kind == entry.kind;
    if !kind_matches {
        return Some(Refusal::KindMismatch);
    }
    let Some(view) = &proposed.view else {
        return Some(Refusal::NoView);
    };
    let honest_core = match proposed.kind {
        Kind::View => true,
        Kind::Module => {
            let current = match view_source::hold_frame(client, active).await {
                Ok(frame) => frame,
                Err(error) => return Some(fetch_refusal(&error)),
            };
            kept.insert(active);
            current.core == proposed.core
        }
    };
    if !honest_core {
        return Some(Refusal::CoreChangesToo);
    }
    let speaks_our_wire = view_wire::manifest::read_manifest(&view.component)
        .is_some_and(|manifest| manifest.check_wire_protocol().is_ok());
    if !speaks_our_wire {
        return Some(Refusal::WireProtocol);
    }
    None
}

/// A fetch that failed, as the row's refusal: a node that does not hold
/// the bytes and one that cannot answer for them hold nothing this app
/// can read.
fn fetch_refusal(error: &view_source::FetchError) -> Refusal {
    use view_source::FetchError;
    match error {
        FetchError::NotHeld | FetchError::Transport(_) => Refusal::NotHeld,
        FetchError::HashMismatch => Refusal::HashMismatch,
        FetchError::InvalidArtifact(_) => Refusal::InvalidArtifact,
    }
}

/// Nothing tasted, nothing held, nothing remembered, nothing noticed:
/// the previous test's word is not this one's.
#[cfg(test)]
pub(crate) fn clear() {
    let mut state = state().lock().expect("taste set");
    state.chain_id.clear();
    state.rows.clear();
    view_source::retain_frames(&BTreeSet::new());
    *test_prefs().lock().expect("test prefs") = serde_json::json!({});
    take_notices();
}

// ---------- the preference ----------

/// The prefs key: `"tasting": { "<chain_id>": { "<module_id>": "<hash hex>" } }`
/// — per device, per network, per module, one hash.
const TASTING_PREF: &str = "tasting";

/// The hashes this device tastes on `chain_id`, by module.
pub(crate) fn remembered(chain_id: &str) -> BTreeMap<String, [u8; 32]> {
    remembered_in(&read_prefs(), chain_id)
}

#[cfg(not(test))]
fn read_prefs() -> serde_json::Value {
    crate::backend::read_prefs()
}

#[cfg(not(test))]
fn write_prefs(prefs: &serde_json::Value) -> bool {
    crate::backend::write_prefs(prefs)
}

/// A test's prefs live in the process, never in the developer's own
/// `prefs.json`.
#[cfg(test)]
fn test_prefs() -> &'static Mutex<serde_json::Value> {
    static PREFS: OnceLock<Mutex<serde_json::Value>> = OnceLock::new();
    PREFS.get_or_init(|| Mutex::new(serde_json::json!({})))
}

#[cfg(test)]
fn read_prefs() -> serde_json::Value {
    test_prefs().lock().expect("test prefs").clone()
}

#[cfg(test)]
fn write_prefs(prefs: &serde_json::Value) -> bool {
    *test_prefs().lock().expect("test prefs") = prefs.clone();
    true
}

fn remembered_in(prefs: &serde_json::Value, chain_id: &str) -> BTreeMap<String, [u8; 32]> {
    let Some(by_module) = prefs[TASTING_PREF][chain_id].as_object() else {
        return BTreeMap::new();
    };
    by_module
        .iter()
        .filter_map(|(module, hash)| {
            let hex = hash.as_str()?;
            let bytes = crate::backend::hex_decode(hex).ok()?;
            let hash: [u8; 32] = bytes.try_into().ok()?;
            Some((module.clone(), hash))
        })
        .collect()
}

/// This device tastes `hash` for `module` on `chain_id` from now on.
pub(crate) fn remember(chain_id: &str, module: &str, hash: [u8; 32]) -> bool {
    let mut prefs = read_prefs();
    remember_in(&mut prefs, chain_id, module, hash);
    write_prefs(&prefs)
}

fn remember_in(prefs: &mut serde_json::Value, chain_id: &str, module: &str, hash: [u8; 32]) {
    prefs[TASTING_PREF][chain_id][module] = serde_json::json!(crate::backend::hex_encode(&hash));
}

/// This device tastes nothing for `module` on `chain_id` any more.
pub(crate) fn forget(chain_id: &str, module: &str) -> bool {
    let mut prefs = read_prefs();
    forget_in(&mut prefs, chain_id, module);
    write_prefs(&prefs)
}

fn forget_in(prefs: &mut serde_json::Value, chain_id: &str, module: &str) {
    let Some(by_module) = prefs[TASTING_PREF][chain_id].as_object_mut() else {
        return;
    };
    by_module.remove(module);
}

// ---------- the notices ----------

/// A sentence for the member about their taste — the proposed view was
/// withdrawn, or became the current one — on its way to the app's toast:
/// the sender every seat writes, and the receiver the one subscriber
/// takes.
struct Notices {
    sender: tokio::sync::mpsc::UnboundedSender<String>,
    receiver: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<String>>>,
}

fn notices() -> &'static Notices {
    static NOTICES: OnceLock<Notices> = OnceLock::new();
    NOTICES.get_or_init(|| {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        Notices {
            sender,
            receiver: Mutex::new(Some(receiver)),
        }
    })
}

pub(crate) fn notice(text: String) {
    #[cfg(test)]
    noticed().lock().expect("taste notices").push(text.clone());
    let _ = notices().sender.send(text);
}

/// Every notice so far, for a test to read: the stream is one
/// subscriber's, and the app is not here.
#[cfg(test)]
fn noticed() -> &'static Mutex<Vec<String>> {
    static NOTICED: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    NOTICED.get_or_init(Mutex::default)
}

/// The notices since the last take.
#[cfg(test)]
pub(crate) fn take_notices() -> Vec<String> {
    std::mem::take(&mut *noticed().lock().expect("taste notices"))
}

/// Every notice, as a stream the app subscribes to once: the receiver is
/// taken by the first subscriber, and a second gets nothing.
pub(crate) fn notice_stream() -> impl futures::Stream<Item = String> {
    let receiver = notices().receiver.lock().expect("taste notices").take();
    futures::stream::unfold(receiver, |receiver| async move {
        let mut receiver = receiver?;
        let text = receiver.recv().await?;
        Some((text, Some(receiver)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preference_round_trips_one_hash_per_chain_per_module() {
        let mut prefs = serde_json::json!({ "appearance": "dark" });
        remember_in(&mut prefs, "chain-a", "chat", [1; 32]);
        remember_in(&mut prefs, "chain-a", "forge", [2; 32]);
        remember_in(&mut prefs, "chain-b", "chat", [3; 32]);
        assert_eq!(
            remembered_in(&prefs, "chain-a"),
            [("chat".to_owned(), [1; 32]), ("forge".to_owned(), [2; 32])].into()
        );
        assert_eq!(
            remembered_in(&prefs, "chain-b"),
            [("chat".to_owned(), [3; 32])].into()
        );
        assert_eq!(prefs["appearance"], "dark", "the other preferences stay");
        // a second hash for the same module replaces the first
        remember_in(&mut prefs, "chain-a", "chat", [4; 32]);
        assert_eq!(remembered_in(&prefs, "chain-a")["chat"], [4; 32]);
        forget_in(&mut prefs, "chain-a", "chat");
        assert_eq!(
            remembered_in(&prefs, "chain-a"),
            [("forge".to_owned(), [2; 32])].into()
        );
        forget_in(&mut prefs, "chain-c", "chat");
        // a hash that is not 32 bytes is no preference
        prefs[TASTING_PREF]["chain-b"]["chat"] = serde_json::json!("abcd");
        assert!(remembered_in(&prefs, "chain-b").is_empty());
    }
}
