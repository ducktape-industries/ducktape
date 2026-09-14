//! the declared-shape pin over the committed component set: every
//! `tests/fixtures/<id>.component.wasm` answers the shape the host composes
//! it by (`WasmModule::declared_shape`), and this table IS today's set —
//! declared-equals-actual both ways, so a guest edit that moves a module's
//! backing, network config or query mode fails here, by name, before it
//! composes a different root or seats a shape the host cannot realize; and a
//! fixture added without a row (or a row without a fixture) fails the same
//! way.

use std::collections::BTreeMap;

use wasm_host::{Backing, Shape, WasmModule};

fn shape(backing: Backing, config: &[&str], committed_queries: bool) -> Shape {
    Shape {
        backing,
        config: config.iter().map(|k| (*k).to_string()).collect(),
        committed_queries,
    }
}

/// what each committed component declares.
fn expected() -> BTreeMap<String, Shape> {
    const CHAIN_ID: &[&str] = &[sdk::genesis_config::CHAIN_ID];
    const INVITE: &[&str] = &[sdk::genesis_config::INVITE];
    const NONE: &[&str] = &[];
    const CHAIN_ID_AND_TIME_UNIT: &[&str] =
        &[sdk::genesis_config::CHAIN_ID, sdk::genesis_config::TIME_UNIT];
    [
        ("acl", shape(Backing::Store, NONE, false)),
        ("agent", shape(Backing::Store, NONE, false)),
        ("automations", shape(Backing::Store, NONE, false)),
        ("capability", shape(Backing::Store, NONE, false)),
        ("chat", shape(Backing::Store, NONE, false)),
        // the network binding it refuses every op without, and the unit its
        // delivery deadlines are in.
        ("collaboration", shape(Backing::Store, CHAIN_ID_AND_TIME_UNIT, false)),
        ("directory", shape(Backing::Map, NONE, false)),
        // committed-only queries: the between-block delivery injection must
        // never observe a same-block staged write.
        ("dispatch", shape(Backing::Store, NONE, true)),
        ("files", shape(Backing::Odb, NONE, false)),
        ("forge", shape(Backing::Odb, CHAIN_ID, false)),
        ("gateway", shape(Backing::Store, CHAIN_ID, false)),
        ("governance", shape(Backing::Store, INVITE, false)),
        ("hello", shape(Backing::Map, NONE, false)),
        ("hello-replacement", shape(Backing::Map, NONE, false)),
        ("identity", shape(Backing::Store, CHAIN_ID, false)),
        ("inbox", shape(Backing::Store, NONE, false)),
        ("kv", shape(Backing::Store, NONE, false)),
        ("modules", shape(Backing::Store, NONE, false)),
        ("valset", shape(Backing::Store, NONE, false)),
        ("noop", shape(Backing::Map, NONE, false)),
        ("pages", shape(Backing::Store, NONE, false)),
        // the network binding, and the unit its conversation check-ins are in.
        ("runs", shape(Backing::Map, CHAIN_ID_AND_TIME_UNIT, false)),
        ("saga", shape(Backing::Store, NONE, false)),
        ("attribution", shape(Backing::Store, NONE, false)),
        ("tasks", shape(Backing::Store, NONE, false)),
    ]
    .into_iter()
    .map(|(id, shape)| (id.to_string(), shape))
    .collect()
}

/// the shape every `<id>.component.wasm` in the fixtures dir declares.
fn declared() -> BTreeMap<String, Shape> {
    const SUFFIX: &str = ".component.wasm";
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).expect("fixtures dir") {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let Some(id) = name.strip_suffix(SUFFIX) else {
            continue;
        };
        let bytes = std::fs::read(&path).expect("fixture bytes");
        let shape = WasmModule::declared_shape(&bytes)
            .unwrap_or_else(|e| panic!("{id} declares no readable shape: {e}"));
        out.insert(id.to_string(), shape);
    }
    out
}

#[test]
fn every_committed_component_declares_the_pinned_shape() {
    let declared = declared();
    let expected = expected();
    for (id, want) in &expected {
        match declared.get(id) {
            Some(got) => assert_eq!(got, want, "{id} declares a different shape than pinned"),
            None => panic!("{id} is pinned but has no fixture component"),
        }
    }
    let unpinned: Vec<&String> = declared.keys().filter(|id| !expected.contains_key(*id)).collect();
    assert!(unpinned.is_empty(), "fixture components with no pinned shape: {unpinned:?}");
}
