//! The genesis file — `<workspace>/genesis` — and the founding set it is
//! composed from.
//!
//! Each entry is one deployment: a consensus component and its optional
//! index mapper, encoded by `module-artifact`. A network's descriptor pins the
//! whole genesis file and every deployment hash. The blob plane stores those
//! encoded deployments; materialization writes their bare Wasm files beneath
//! `<workspace>/modules` for inspection. Live updates arrive through the code
//! registry and can replace either part without rewriting genesis.
//!
//! The FOUNDING SET is the one place bare wasm files are read: a directory of
//! `<id>.component.wasm`, `<id>.index.wasm` and `netstack.component.wasm` the
//! build stages beside its binaries (`crates/noded/build.rs`), read by `node
//! init` to compose a genesis and by the daemons that run no network (noded,
//! simnode, the dev shape) to compose directly. The supplied files determine
//! the module set. An index guest must have a component with the same id;
//! module ids and index presence are never checked against a binary catalog.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use borsh::{BorshDeserialize, BorshSerialize};
use module_artifact::{ArtifactRef, ModuleArtifactRef};

use crate::hex_bytes;

/// the genesis file's name inside a workspace.
pub const GENESIS_FILE: &str = "genesis";

/// the netstack guest's file name inside a founding set: the reachability
/// machine as a `ducktape:netstack` component. Not a genesis artifact — a
/// joiner runs it to reach the mesh before it holds any genesis — so it is
/// read from the founding set at plane wiring, never from the genesis.
pub const NETSTACK_COMPONENT_FILE: &str = "netstack.component.wasm";

/// `<dir>/genesis`.
pub fn genesis_path(dir: &Path) -> PathBuf {
    dir.join(GENESIS_FILE)
}

/// `<dir>/modules` — where a workspace's genesis is unpacked into bare files
/// ([`Genesis::materialize`]), in founding-set layout: the network's wasm,
/// readable on disk beside `network.toml`.
pub fn modules_path(dir: &Path) -> PathBuf {
    dir.join("modules")
}

/// `<dir>/<id>.component.wasm` — a consensus component in a founding set.
pub fn component_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.component.wasm"))
}

/// `<dir>/<id>.index.wasm` — a module's index guest in a founding set.
pub fn index_guest_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.index.wasm"))
}

/// Package a module's component, optional mapper, view and assets from a directory.
pub fn read_module_artifact(dir: &Path, id: &str) -> Result<module_artifact::Artifact, String> {
    crate::ensure_view_ready(dir, id)?;
    let component = component_path(dir, id);
    let index = optional_path(index_guest_path(dir, id))?;
    let view = optional_path(dir.join(format!("{id}.view.wasm")))?;
    let assets = optional_path(dir.join(format!("{id}.assets")))?;
    crate::read_deployment_files(
        &component,
        index.as_deref(),
        view.as_deref(),
        assets.as_deref(),
    )
}

fn optional_path(path: PathBuf) -> Result<Option<PathBuf>, String> {
    match std::fs::symlink_metadata(&path) {
        Ok(_) => Ok(Some(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("inspect {}: {error}", path.display())),
    }
}

/// `<dir>/netstack.component.wasm` — the netstack guest in a founding set.
pub fn netstack_component_path(dir: &Path) -> PathBuf {
    dir.join(NETSTACK_COMPONENT_FILE)
}

/// one wasm artifact a genesis carries, by the module id it belongs to.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Artifact {
    pub id: String,
    pub bytes: Vec<u8>,
}

/// the genesis: every wasm a node on this network runs. Both lists are
/// id-sorted, so the same set always encodes to the same bytes and the whole
/// file has one hash.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Genesis {
    /// Each entry is the same encoded deployment accepted by the code plane.
    pub modules: Vec<Artifact>,
}

impl Genesis {
    /// Compose the supplied modules, including ids the binary has never
    /// encountered. An index guest belongs to the component with the same
    /// id. The reachability component is a separate plane's artifact.
    pub fn compose(source: &Path) -> Result<Self, String> {
        let components = discover_artifact_ids(source, ".component.wasm")?;
        if components.is_empty() {
            return Err(format!(
                "modules directory {} holds no module components",
                source.display()
            ));
        }
        let mut index_guests = discover_artifact_ids(source, ".index.wasm")?;
        let modules = components
            .into_iter()
            .map(|id| {
                index_guests.remove(&id);
                let bytes = read_module_artifact(source, &id)?.encode();
                Ok(Artifact { id, bytes })
            })
            .collect::<Result<Vec<_>, String>>()?;
        for entry in std::fs::read_dir(source).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err("non-UTF-8 founding file name".into());
            };
            let id = [".view.wasm", ".assets", ".view.pending"]
                .iter()
                .find_map(|suffix| name.strip_suffix(suffix));
            if let Some(id) = id
                && !modules.iter().any(|module| module.id == id)
            {
                return Err(format!(
                    "view or assets {id} has no module component in the genesis"
                ));
            }
        }
        if let Some(id) = index_guests.first() {
            return Err(format!(
                "index guest {id} has no module component in the genesis"
            ));
        }
        let genesis = Self { modules };
        genesis.validate()?;
        Ok(genesis)
    }

    /// unpack every artifact into `dir` in founding-set layout
    /// (`<id>.component.wasm`, `<id>.index.wasm`, `<id>.view.wasm`,
    /// `<id>.assets/`): the genesis's readable
    /// twin on disk. Rewritten in full on every call (each file atomically),
    /// so the directory always says what the genesis says — a founding set
    /// composed from it yields this genesis again.
    pub fn materialize(&self, dir: &Path) -> Result<(), String> {
        self.validate()?;
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        for module in &self.modules {
            let view_path = dir.join(format!("{}.view.wasm", module.id));
            let asset_path = dir.join(format!("{}.assets", module.id));
            let pending = dir.join(format!("{}.view.pending", module.id));
            // a module writes its core (and mapper) first; a view-only entry
            // owns no core file — the obsolete sweep below removes a stale one.
            let view = match ArtifactRef::decode(&module.bytes)? {
                ArtifactRef::Module(artifact) => {
                    write_atomic(&component_path(dir, &module.id), artifact.component)?;
                    if let Some(index) = artifact.index {
                        write_atomic(&index_guest_path(dir, &module.id), index)?;
                    }
                    artifact.view
                }
                ArtifactRef::View(view) => Some(view),
            };
            match view {
                Some(view) => {
                    write_atomic(&pending, b"view materialization pending")?;
                    crate::view_files::materialize_assets(dir, &module.id, &view.assets)?;
                    write_atomic(&view_path, view.component)?;
                }
                None => {
                    crate::view_files::remove_owned(&view_path)?;
                    crate::view_files::remove_owned(&asset_path)?;
                }
            }
            crate::view_files::remove_owned(&pending)?;
        }
        let entries = std::fs::read_dir(dir)
            .map_err(|e| format!("read modules directory {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("read {}: {e}", dir.display()))?;
            let filename = entry.file_name();
            let Some(filename) = filename.to_str() else {
                continue;
            };
            if filename == NETSTACK_COMPONENT_FILE {
                continue;
            }
            let obsolete_component = filename
                .strip_suffix(".component.wasm")
                .is_some_and(|id| self.component(id).is_none());
            let obsolete_index = filename
                .strip_suffix(".index.wasm")
                .is_some_and(|id| self.index_guest(id).is_none());
            let obsolete_view = [".view.wasm", ".view.pending", ".assets"]
                .iter()
                .any(|suffix| {
                    filename
                        .strip_suffix(suffix)
                        .is_some_and(|id| self.artifact(id).is_none())
                });
            let obsolete_artifact = obsolete_component || obsolete_index || obsolete_view;
            if obsolete_artifact {
                let path = entry.path();
                crate::view_files::remove_owned(&path)?;
            }
        }
        Ok(())
    }

    /// the file's bytes.
    pub fn encode(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("a genesis serializes")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let genesis: Self =
            borsh::from_slice(bytes).map_err(|e| format!("genesis file does not decode: {e}"))?;
        genesis.validate()?;
        Ok(genesis)
    }

    fn validate(&self) -> Result<(), String> {
        let canonical = self.modules.windows(2).all(|w| w[0].id < w[1].id);
        if !canonical {
            return Err("genesis file is not canonical (ids unsorted or duplicated)".into());
        }
        for artifact in &self.modules {
            crate::validate_module_id(&artifact.id)?;
            ArtifactRef::decode(&artifact.bytes)?;
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        Self::decode(&bytes).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// id → sha256 of each deployment: the descriptor's `modules` table.
    pub fn module_hashes(&self) -> BTreeMap<String, [u8; 32]> {
        self.modules
            .iter()
            .map(|a| (a.id.clone(), sha256(&a.bytes)))
            .collect()
    }

    pub fn artifact(&self, id: &str) -> Option<&[u8]> {
        self.modules
            .iter()
            .find(|module| module.id == id)
            .map(|module| module.bytes.as_slice())
    }

    /// the module arm of `id`'s artifact; `None` for an absent id and for a
    /// view-only entry, which has no core.
    fn module(&self, id: &str) -> Option<ModuleArtifactRef<'_>> {
        match ArtifactRef::decode(self.artifact(id)?).ok()? {
            ArtifactRef::Module(module) => Some(module),
            ArtifactRef::View(_) => None,
        }
    }

    pub fn component(&self, id: &str) -> Option<&[u8]> {
        Some(self.module(id)?.component)
    }

    pub fn index_guest(&self, id: &str) -> Option<&[u8]> {
        self.module(id)?.index
    }

    /// the deployments are exactly the descriptor's module set (`want`: id →
    /// `code_hash`), byte for byte: the same ids, each hashing to its entry.
    /// A mismatch names the module, because the operator's next move is to
    /// look at it.
    pub fn verify_modules(&self, want: &BTreeMap<String, [u8; 32]>) -> Result<(), String> {
        let have = self.module_hashes();
        for (id, hash) in want {
            match have.get(id) {
                None => {
                    return Err(format!(
                        "module {id}: the genesis carries no component for it"
                    ));
                }
                Some(got) if got != hash => {
                    return Err(format!(
                        "module {id}: the genesis component hashes to {} but network.toml says {}",
                        hex_bytes(got),
                        hex_bytes(hash)
                    ));
                }
                Some(_) => {}
            }
        }
        if let Some(extra) = have.keys().find(|id| !want.contains_key(*id)) {
            return Err(format!(
                "module {extra}: the genesis carries a component network.toml does not name"
            ));
        }
        Ok(())
    }
}

/// sha256 of a genesis file's bytes — the descriptor's `genesis` pin.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes).into()
}

/// decode `bytes` as the genesis the descriptor pins: the whole file must
/// hash to `hash` (the descriptor's `genesis`) and its deployments must be
/// `modules` (the descriptor's table, id → `code_hash`). Every reader of a
/// genesis that did not compose it goes through here — a joiner installing
/// a handed-out or fetched file, a member reopening its own.
pub fn verify_genesis(
    bytes: &[u8],
    hash: &[u8; 32],
    modules: &BTreeMap<String, [u8; 32]>,
) -> Result<Genesis, String> {
    let got = sha256(bytes);
    let is_the_networks_genesis = got == *hash;
    if !is_the_networks_genesis {
        return Err(format!(
            "genesis hashes to {} but network.toml says {} — this is not the network's genesis",
            hex_bytes(&got),
            hex_bytes(hash)
        ));
    }
    let genesis = Genesis::decode(bytes)?;
    genesis.verify_modules(modules)?;
    Ok(genesis)
}

/// install a genesis into `dir` from `bytes` (a file the founder handed out,
/// or the blob a member served), verified by [`verify_genesis`] first.
/// Nothing is written until it holds; the write is atomic (tmp + rename), so
/// a workspace never holds a half-written genesis.
pub fn install_genesis(
    dir: &Path,
    hash: &[u8; 32],
    modules: &BTreeMap<String, [u8; 32]>,
    bytes: &[u8],
) -> Result<Genesis, String> {
    let genesis = verify_genesis(bytes, hash, modules)?;
    write_atomic(&genesis_path(dir), bytes)?;
    Ok(genesis)
}

fn discover_artifact_ids(
    source: &Path,
    suffix: &str,
) -> Result<std::collections::BTreeSet<String>, String> {
    let entries = std::fs::read_dir(source)
        .map_err(|e| format!("read modules directory {}: {e}", source.display()))?;
    let mut ids = std::collections::BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("read {}: {e}", source.display()))?;
        let filename = entry.file_name();
        let Some(filename) = filename.to_str() else {
            return Err(format!("non-UTF-8 filename in {}", source.display()));
        };
        if filename == NETSTACK_COMPONENT_FILE {
            continue;
        }
        let Some(id) = filename.strip_suffix(suffix) else {
            continue;
        };
        crate::validate_module_id(id)?;
        ids.insert(id.to_string());
    }
    Ok(ids)
}

/// write `bytes` to `path` via tmp-file + rename: a reader never observes a
/// truncated or partial file, only the old contents or the new ones. Shared
/// with [`crate::NetworkDescriptor::save`] — the descriptor is as much a
/// workspace identity file as the genesis is.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| format!("create {}: {e}", tmp.display()))?;
    let result = file
        .write_all(bytes)
        .map_err(|e| format!("write {}: {e}", tmp.display()))
        .and_then(|()| {
            drop(file);
            std::fs::rename(&tmp, path)
                .map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), path.display()))
        });
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModuleCode, NetworkDescriptor};

    fn founding_set(dir: &Path, components: &[(&str, &[u8])], guests: &[(&str, &[u8])]) {
        for (id, bytes) in components {
            std::fs::write(component_path(dir, id), bytes).unwrap();
        }
        for (id, bytes) in guests {
            std::fs::write(index_guest_path(dir, id), bytes).unwrap();
        }
    }

    fn descriptor_for(genesis: &Genesis) -> NetworkDescriptor {
        let bytes = genesis.encode();
        NetworkDescriptor {
            chain_id: "net#a1b2c3d4".into(),
            validators: vec![],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: crate::DEFAULT_BLOCK_TIME_MS,
            genesis: hex_bytes(&sha256(&bytes)),
            modules: genesis
                .module_hashes()
                .iter()
                .map(|(id, h)| ModuleCode {
                    id: id.clone(),
                    code_hash: hex_bytes(h),
                })
                .collect(),
        }
    }

    #[test]
    fn compose_reads_the_supplied_set_sorted_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        founding_set(
            dir.path(),
            &[("pages", b"P"), ("chat", b"C")],
            &[("chat", b"c-map")],
        );
        let genesis = Genesis::compose(dir.path()).unwrap();
        let ids: Vec<&str> = genesis.modules.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            ["chat", "pages"],
            "id-sorted regardless of directory order"
        );
        assert_eq!(genesis.component("pages"), Some(&b"P"[..]));
        assert_eq!(genesis.index_guest("chat"), Some(&b"c-map"[..]));
        assert_eq!(genesis.index_guest("pages"), None);
        let bytes = genesis.encode();
        assert_eq!(Genesis::decode(&bytes).unwrap(), genesis);
        assert_eq!(genesis.encode(), bytes, "canonical: same set, same bytes");
    }

    #[test]
    fn materialize_writes_a_founding_set_that_composes_back() {
        let src = tempfile::tempdir().unwrap();
        founding_set(
            src.path(),
            &[("pages", b"P"), ("chat", b"C")],
            &[("chat", b"c-map")],
        );
        let genesis = Genesis::compose(src.path()).unwrap();

        let ws = tempfile::tempdir().unwrap();
        let modules = modules_path(ws.path());
        // a stale file from an older genesis is overwritten, never kept
        std::fs::create_dir_all(&modules).unwrap();
        std::fs::write(component_path(&modules, "pages"), b"old").unwrap();
        std::fs::write(component_path(&modules, "retired"), b"old").unwrap();
        std::fs::write(index_guest_path(&modules, "pages"), b"old-map").unwrap();
        std::fs::write(modules.join("notes.txt"), b"operator notes").unwrap();
        genesis.materialize(&modules).unwrap();
        assert!(!component_path(&modules, "retired").exists());
        assert!(!index_guest_path(&modules, "pages").exists());
        assert!(modules.join("notes.txt").exists());
        assert_eq!(
            std::fs::read(component_path(&modules, "pages")).unwrap(),
            b"P"
        );
        assert_eq!(
            std::fs::read(index_guest_path(&modules, "chat")).unwrap(),
            b"c-map"
        );
        let again = Genesis::compose(&modules).unwrap();
        assert_eq!(again, genesis, "the directory is the genesis, unpacked");
    }

    #[test]
    fn a_view_only_entry_materializes_its_view_files_and_no_core() {
        let genesis = Genesis {
            modules: vec![Artifact {
                id: "home".into(),
                bytes: module_artifact::Artifact::View(module_artifact::ViewArtifact {
                    component: b"H".to_vec(),
                    assets: [("icons/tab.svg".to_owned(), b"<svg/>".to_vec())].into(),
                })
                .encode(),
            }],
        };
        assert_eq!(genesis.component("home"), None, "a view has no core");
        assert_eq!(genesis.index_guest("home"), None);
        assert!(genesis.artifact("home").is_some());

        let ws = tempfile::tempdir().unwrap();
        let modules = modules_path(ws.path());
        // a stale core under the id belongs to no entry: removed, never kept.
        std::fs::create_dir_all(&modules).unwrap();
        std::fs::write(component_path(&modules, "home"), b"old").unwrap();
        std::fs::write(index_guest_path(&modules, "home"), b"old-map").unwrap();
        genesis.materialize(&modules).unwrap();
        assert!(!component_path(&modules, "home").exists());
        assert!(!index_guest_path(&modules, "home").exists());
        assert_eq!(
            std::fs::read(modules.join("home.view.wasm")).unwrap(),
            b"H"
        );
        assert_eq!(
            std::fs::read(modules.join("home.assets/icons/tab.svg")).unwrap(),
            b"<svg/>"
        );
        assert!(!modules.join("home.view.pending").exists());
    }

    #[test]
    fn compose_accepts_unfamiliar_modules_and_excludes_the_reachability_plane() {
        let dir = tempfile::tempdir().unwrap();
        founding_set(dir.path(), &[("weather", b"W")], &[("weather", b"mapper")]);
        std::fs::write(netstack_component_path(dir.path()), b"network").unwrap();
        let genesis = Genesis::compose(dir.path()).unwrap();
        assert_eq!(
            genesis.modules,
            vec![Artifact {
                id: "weather".into(),
                bytes: module_artifact::Artifact::Module(module_artifact::ModuleArtifact {
                    view: None,
                    component: b"W".to_vec(),
                    index: Some(b"mapper".to_vec())
                })
                .encode()
            }]
        );
        assert_eq!(genesis.index_guest("weather"), Some(&b"mapper"[..]));
    }

    #[test]
    fn compose_and_decode_refuse_an_index_guest_without_its_component() {
        let dir = tempfile::tempdir().unwrap();
        founding_set(dir.path(), &[("pages", b"P")], &[("weather", b"mapper")]);
        let err = Genesis::compose(dir.path()).unwrap_err();
        assert!(
            err.contains("weather") && err.contains("no module component"),
            "{err}"
        );
        let invalid = Genesis {
            modules: vec![Artifact {
                id: "weather".into(),
                bytes: b"mapper".to_vec(),
            }],
        };
        assert!(Genesis::decode(&invalid.encode()).is_err());
    }

    #[test]
    fn compose_refuses_a_zero_byte_artifact_at_discovery() {
        let dir = tempfile::tempdir().unwrap();
        founding_set(dir.path(), &[("pages", b""), ("chat", b"C")], &[]);
        let err = Genesis::compose(dir.path()).unwrap_err();
        assert!(
            err.contains("pages.component.wasm") && err.contains("empty"),
            "{err}"
        );

        // the same check for a zero-byte mapper.
        let dir = tempfile::tempdir().unwrap();
        founding_set(dir.path(), &[("pages", b"P")], &[("pages", b"")]);
        let err = Genesis::compose(dir.path()).unwrap_err();
        assert!(
            err.contains("pages.index.wasm") && err.contains("empty"),
            "{err}"
        );
    }

    #[test]
    fn decode_and_materialize_refuse_ids_that_escape_the_module_directory() {
        let dir = tempfile::tempdir().unwrap();
        for id in [
            "../outside",
            "/absolute",
            "nested/module",
            "nested\\module",
            "",
            ".",
            "..",
        ] {
            let invalid = Genesis {
                modules: vec![Artifact {
                    id: id.into(),
                    bytes: vec![],
                }],
            };
            assert!(Genesis::decode(&invalid.encode()).is_err(), "{id:?}");
            assert!(
                invalid.materialize(&dir.path().join("modules")).is_err(),
                "{id:?}"
            );
        }
        assert!(!dir.path().join("modules").exists());
    }

    #[test]
    fn decode_refuses_a_non_canonical_file() {
        let unsorted = Genesis {
            modules: vec![
                Artifact {
                    id: "pages".into(),
                    bytes: vec![1],
                },
                Artifact {
                    id: "chat".into(),
                    bytes: vec![2],
                },
            ],
        };
        let err = Genesis::decode(&unsorted.encode()).unwrap_err();
        assert!(err.contains("canonical"), "{err}");
    }

    #[test]
    fn install_verifies_the_whole_file_then_every_component() {
        let dir = tempfile::tempdir().unwrap();
        let genesis = Genesis {
            modules: vec![Artifact {
                id: "pages".into(),
                bytes: module_artifact::Artifact::Module(module_artifact::ModuleArtifact {
                    view: None,
                    component: vec![1, 2, 3],
                    index: Some(vec![9]),
                })
                .encode(),
            }],
        };
        let descriptor = descriptor_for(&genesis);
        let bytes = genesis.encode();
        let hash = descriptor.genesis_hash().unwrap();
        let modules = descriptor.module_hashes().unwrap();

        let installed = install_genesis(dir.path(), &hash, &modules, &bytes).unwrap();
        assert_eq!(installed, genesis);
        assert_eq!(std::fs::read(genesis_path(dir.path())).unwrap(), bytes);

        // a different file: refused by the whole-file hash, nothing written.
        let other = tempfile::tempdir().unwrap();
        let mut tampered = bytes.clone();
        tampered.push(0);
        let err = install_genesis(other.path(), &hash, &modules, &tampered).unwrap_err();
        assert!(err.contains("not the network's genesis"), "{err}");
        assert!(!genesis_path(other.path()).exists());

        // the right file against a descriptor whose module table disagrees:
        // refused by module name.
        let mut wrong_modules = modules.clone();
        wrong_modules.insert("pages".into(), [0u8; 32]);
        let err = genesis.verify_modules(&wrong_modules).unwrap_err();
        assert!(err.starts_with("module pages:"), "{err}");
        let mut extra = modules.clone();
        extra.insert("chat".into(), [0u8; 32]);
        let err = genesis.verify_modules(&extra).unwrap_err();
        assert!(err.contains("no component"), "{err}");
    }
}
