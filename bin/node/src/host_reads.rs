use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519;

use host::Host;

use crate::module_contracts::{governance, valset};

/// read the valset module's current membership projection (committed state —
/// called between drains, outside any block).
///
/// an unreadable valset is NOT an observation of an empty one (#1820): a
/// transient query error and a genuinely empty committed set must stay
/// distinguishable to the cutover step, so this returns `Err` on either the
/// query or the decode failing rather than degrading both to `Vec::new()`.
pub(crate) async fn read_valset_members(host: &Host) -> Result<Vec<Vec<u8>>, String> {
    use crate::module_contracts::valset::{ValsetQuery, ValsetReply, decode_reply, encode_query};
    let reply = host
        .query("valset", &encode_query(&ValsetQuery::Validators))
        .await
        .map_err(|e| e.to_string())?;
    match decode_reply(&reply)? {
        ValsetReply::Validators(v) => Ok(v),
        other => Err(format!("unexpected valset reply variant: {other:?}")),
    }
}

/// read the valset module's current RESIDENT projection (committed state —
/// called between drains, outside any block; same read point as
/// [`read_valset_members`], so a boundary read sees one frozen state).
pub(crate) async fn read_valset_residents(host: &Host) -> Vec<Vec<u8>> {
    use crate::module_contracts::valset::{ValsetQuery, ValsetReply, decode_reply, encode_query};
    let Ok(reply) = host
        .query("valset", &encode_query(&ValsetQuery::Residents))
        .await
    else {
        return Vec::new();
    };
    match decode_reply(&reply) {
        Ok(ValsetReply::Residents(v)) => v,
        Ok(_) | Err(_) => Vec::new(),
    }
}

/// read the valset module's retained mesh-generation window (committed state —
/// called between drains, outside any block; same read point as
/// [`read_valset_members`]). unreadable degrades to EMPTY: callers treat an
/// empty window per their role (a validator fail-stops, a poller retries).
pub(crate) async fn read_valset_mesh_window(host: &Host) -> Vec<valset::GenerationSet> {
    use crate::module_contracts::valset::{ValsetQuery, ValsetReply, decode_reply, encode_query};
    let Ok(reply) = host
        .query("valset", &encode_query(&ValsetQuery::MeshWindow))
        .await
    else {
        return Vec::new();
    };
    match decode_reply(&reply) {
        Ok(ValsetReply::MeshWindow(window)) => window,
        Ok(_) | Err(_) => Vec::new(),
    }
}

/// the committed mesh window in its sync-wire shape: `(latest generation,
/// entries)` — the fill for [`statesync::TipCoords`] / `BoundaryCoords`.
/// statesync stays valset-agnostic, so the conversion lives here.
pub(crate) async fn read_sync_mesh_window(host: &Host) -> (u64, Vec<statesync::MeshWindowEntry>) {
    let window = read_valset_mesh_window(host).await;
    let generation = window.last().map(|s| s.generation).unwrap_or(0);
    let entries = window
        .into_iter()
        .map(|s| statesync::MeshWindowEntry {
            generation: s.generation,
            validators: s.validators,
            residents: s.residents,
        })
        .collect();
    (generation, entries)
}

/// read ONE committed invite redemption by token nonce — the exactly-once
/// set's point read (committed+staged projection, between drains). an
/// unreadable reply degrades to `None`: the gate then simply cannot pre-empt
/// a spent invite, and the in-consensus exactly-once check still holds.
pub(crate) async fn read_redemption_from_host(
    host: &Host,
    nonce: &[u8],
) -> Option<governance::RedemptionView> {
    use crate::module_contracts::governance::{GovQuery, GovReply, decode_reply, encode_query};
    let reply = host
        .query(
            "governance",
            &encode_query(&GovQuery::Redemption {
                nonce: nonce.to_vec(),
            }),
        )
        .await
        .ok()?;
    match decode_reply(&reply) {
        Ok(GovReply::Redemption(view)) => view,
        Ok(_) | Err(_) => None,
    }
}

pub(crate) fn resume_member_keys(
    resumed: Option<&recovery::Recovered>,
    validators: &[ed25519::PublicKey],
) -> Result<Vec<ed25519::PublicKey>, String> {
    let raw: Vec<Vec<u8>> = match resumed {
        Some(rec) => rec.participants.clone(),
        None => validators.iter().map(|k| k.as_ref().to_vec()).collect(),
    };
    let mut keys = Vec::with_capacity(raw.len());
    for k in &raw {
        keys.push(
            ed25519::PublicKey::decode(k.as_slice())
                .map_err(|e| format!("recovered participant set holds a non-ed25519 key: {e}"))?,
        );
    }
    Ok(keys)
}

/// the recovered epoch's RESIDENT keys — empty on a fresh boot (genesis has
/// no residents) and on checkpoints written before the staged-admission tier.
pub(crate) fn resume_resident_keys(
    resumed: Option<&recovery::Recovered>,
) -> Result<Vec<ed25519::PublicKey>, String> {
    let raw: Vec<Vec<u8>> = match resumed {
        Some(rec) => rec.residents.clone(),
        None => Vec::new(),
    };
    let mut keys = Vec::with_capacity(raw.len());
    for k in &raw {
        keys.push(
            ed25519::PublicKey::decode(k.as_slice())
                .map_err(|e| format!("recovered resident set holds a non-ed25519 key: {e}"))?,
        );
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use sdk::{Ctx, Error, Module, ModuleId, Msg, StateRoot};

    use super::*;

    /// a valset stand-in that serves exactly the `Validators` reply this test
    /// stages — `Ok(keys)` for a real read, or `Err` to stand in for a query
    /// that failed (a qmdb read error, the module momentarily unresolvable).
    struct FakeValset(Result<Vec<Vec<u8>>, String>);

    #[async_trait::async_trait(?Send)]
    impl Module for FakeValset {
        fn id(&self) -> ModuleId {
            "valset".into()
        }

        fn root(&self) -> StateRoot {
            StateRoot::ZERO
        }

        async fn execute(&mut self, _ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
            Err(Error::QueryUnsupported)
        }

        async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
            use crate::module_contracts::valset::{
                ValsetQuery, ValsetReply, decode_query, encode_reply,
            };
            match decode_query(req) {
                Ok(ValsetQuery::Validators) => match &self.0 {
                    Ok(keys) => Ok(encode_reply(&ValsetReply::Validators(keys.clone()))),
                    Err(reason) => Err(Error::module("injected_fault", reason.clone())),
                },
                _ => Err(Error::QueryUnsupported),
            }
        }
    }

    fn host_serving(members: Result<Vec<Vec<u8>>, String>) -> Host {
        Host::genesis(vec![Box::new(FakeValset(members))]).expect("genesis")
    }

    /// #1820: a query error is NOT an observation of an empty set — the two
    /// must stay distinguishable so the drain can skip the cutover step
    /// instead of feeding it a bogus empty membership.
    #[test]
    fn a_failed_query_is_err_not_an_empty_set() {
        let host = host_serving(Err("qmdb read error".into()));
        let result = block_on(read_valset_members(&host));
        assert!(
            result.is_err(),
            "a query failure must surface as Err, never as Ok(vec![])"
        );
    }

    /// #1820: a genuinely empty committed set is a SUCCESSFUL read — the
    /// impossible state the drain fatals on, never conflated with a failed
    /// read that should just retry next block.
    #[test]
    fn a_successful_empty_read_is_ok_of_an_empty_vec() {
        let host = host_serving(Ok(Vec::new()));
        let result = block_on(read_valset_members(&host));
        assert_eq!(
            result,
            Ok(Vec::new()),
            "an empty committed set is a successful read, distinct from a failed one"
        );
    }

    /// the ordinary case still reads through.
    #[test]
    fn a_successful_nonempty_read_returns_the_members() {
        let host = host_serving(Ok(vec![vec![7u8; 32]]));
        let result = block_on(read_valset_members(&host));
        assert_eq!(result, Ok(vec![vec![7u8; 32]]));
    }

    /// an unregistered "valset" target — the same shape a query error takes
    /// on a live host — is also Err, not an empty set.
    #[test]
    fn an_unregistered_valset_module_is_err() {
        let host = Host::genesis(Vec::new()).expect("genesis");
        let result = block_on(read_valset_members(&host));
        assert!(result.is_err());
    }
}
