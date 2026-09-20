//! the shared duckfs test harness: the deterministic `sdk::Ctx` (the shared
//! `sdk_testkit::TestCtx`), a module constructor over a tempdir, and hash
//! helpers. every later test file mounts this module and uses a subset, so
//! unused helpers are expected here.
#![allow(dead_code)]

use sdk::{Env, Origin};
use sha2::{Digest as _, Sha256};

pub use sdk_testkit::TestCtx;

/// a `files`-scoped [`TestCtx`] at block `height` (`consensus_time == height`)
/// with `origin`; the module id is fixed to "files", the harness's only tenant.
/// captured follow-up msgs are read back via [`TestCtx::msgs`].
pub fn test_ctx(origin: Origin, height: u64) -> TestCtx {
    TestCtx::with_env(Env {
        height,
        consensus_time: height,
        origin,
        me: "files".into(),
        cause: sdk::Cause::Direct,
    })
    .on_query("identity", |req| {
        let identity_module::IdentityQuery::OfKey { .. } =
            identity_module::decode_query(req).map_err(|e| sdk::Error::module("codec", e))?
        else {
            return Err(sdk::Error::QueryUnsupported);
        };
        Ok(identity_module::encode_reply(
            &identity_module::IdentityReply::Account(None),
        ))
    })
}

pub fn open_files(dir: &tempfile::TempDir) -> files::Files {
    files::Files::open("files", dir.path().to_path_buf()).expect("open")
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

pub fn to_hex(bytes: &[u8]) -> String {
    files::to_hex(bytes)
}

/// Re-openable in-memory sibling state for the files crash/restart tests. The
/// filesystem itself still reloads its refs and objects from its actual disk.
#[derive(Clone, Default)]
pub struct SharedStore(
    std::rc::Rc<std::cell::RefCell<std::collections::BTreeMap<Vec<u8>, Vec<u8>>>>,
);

#[async_trait::async_trait(?Send)]
impl sdk::MerkleStore for SharedStore {
    async fn get(&self, key: &[u8; sdk::ROOT_LEN]) -> Result<Option<Vec<u8>>, sdk::Error> {
        Ok(self.0.borrow().get(key.as_slice()).cloned())
    }
    async fn commit_batch(
        &mut self,
        writes: Vec<([u8; sdk::ROOT_LEN], Option<Vec<u8>>)>,
    ) -> Result<(), sdk::Error> {
        let mut map = self.0.borrow_mut();
        for (key, value) in writes {
            match value {
                Some(value) => {
                    map.insert(key.to_vec(), value);
                }
                None => {
                    map.remove(key.as_slice());
                }
            }
        }
        Ok(())
    }
    fn root(&self) -> sdk::StateRoot {
        sdk::StateRoot(sha256(&sdk::hash::encode_pairs(&self.0.borrow())))
    }
    async fn sync_target(&self) -> Result<sdk::ResolverSyncTarget, sdk::Error> {
        Err(sdk::Error::QueryUnsupported)
    }
    async fn serve_sync(&self, _: &[u8]) -> Result<Vec<u8>, sdk::Error> {
        Err(sdk::Error::QueryUnsupported)
    }
}

pub fn watch_msgs(ctx: &TestCtx) -> Vec<&sdk::Msg> {
    ctx.msgs()
        .iter()
        .filter(|msg| msg.target != "attribution")
        .collect()
}
