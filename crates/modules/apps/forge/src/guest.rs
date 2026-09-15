//! the wasm-guest twin of [`module`](crate::module): the pure
//! [`ForgeState`] core over the host state lane, so the accept/reject logic is
//! SINGLE-SOURCED with the native module — both call the SAME
//! [`ForgeState::apply`], arm-for-arm. the host owns everything that touches a
//! git object database (`root`/`snapshot`/`install`/materialization)
//! via the kernel `StateBacking::Odb` backing (`ForgeOdbBacking`, native-only);
//! the guest owns `execute` and product query policy over bounded Git reads.
//!
//! ## per-dispatch re-entry, and why it reproduces the native block
//!
//! the native module keeps ONE `ForgeState` alive across a whole block: every
//! op stages a per-branch fate (one fate per branch per block) and a tracker
//! mutation onto it, and `commit_block` publishes them. an adapter guest is
//! re-instantiated per DISPATCH, so it cannot hold a block-spanning core.
//! instead each dispatch re-enters the block from two host-lane values:
//!
//! 1. the chained STATE IMAGE under [`REFS_KEY`] — the committed image at the
//!    block's first dispatch, the previous dispatch's published image after
//!    (staged-over-committed, the read-your-writes seam);
//! 2. the BLOCK SCRATCH under [`BLOCK_SCRATCH_KEY`] — every fate staged so far
//!    this block, with the committed head each shadows. the kernel drops this
//!    key at the block boundary (it is never adopted into the backing), so a
//!    fresh block's first `state-get` returns `None`.
//!
//! [`ForgeState::from_lane`] rebuilds the exact native mid-block shape from
//! the two — committed refs with the staged fates on top — so the
//! one-fate-per-branch rule and every committed-only check decide identically.
//! after the op applies, the dispatch re-stages both values and hands each
//! packed head it staged to the object plane as a [`RefTarget`] record
//! (kind [`REF_TARGET_KIND`]): the kernel delivers those to the backing at
//! commit, which turns them back into the native per-branch publish.
//!
//! on a rejected op the `?` short-circuits BEFORE any state save or object
//! put, so the host aborts the block with nothing staged — the native
//! reject-then-`abort_block` sequence.
//!
//! ## the genesis-config chain id — #1773
//!
//! forge's per-network parameter is the CHAIN ID [`crate::pushcert::signer`]
//! checks a push certificate's nonce against. the odb
//! twin of identity's store-seeded `__config`: the shape above declares the
//! `chain_id` key, `noded::compose`'s `odb_genesis_config` resolves it against
//! the network's bindings and hands it to `WasmModule::over_odb`, which
//! serves it alongside [`REFS_KEY`] on the state lane — every dispatch reads
//! it back through [`ducktape_module_sdk::genesis_chain_id`].

use ducktape_module_sdk::error_to_wit as to_wit_error;
use ducktape_module_sdk::{Guest, WitCtx, block_on, host};

use crate::state::{
    BlockScratch, ForgeState, Image, REF_TARGET_KIND, decode_block_scratch, decode_image,
    encode_block_scratch, encode_ref_target,
};

/// the guest's state key — the chained state image, whose composed root the
/// host derives the module root from (`StateBacking::Odb`). MUST equal
/// `wasm_host::REFS_KEY` (`b"__state"`) — a mismatch silently forks the
/// network.
const REFS_KEY: &[u8] = b"__state";

/// the guest's EPHEMERAL sibling of [`REFS_KEY`]: the block scratch, staged
/// each dispatch and DROPPED at the block boundary by the kernel (never
/// adopted into the backing). it carries no consensus weight — the root is the
/// image's composition — it only lets a per-dispatch guest reproduce the
/// native block-spanning staging.
const BLOCK_SCRATCH_KEY: &[u8] = b"__block_refs";

/// where issue/PR discussion follow-ups go — the chat module every production
/// node registers beside forge (the native lanes pass the same id).
const CHAT_MODULE: &str = "chat";

/// re-enter the block: the chained image (missing = genesis, empty) and the
/// scratch so far (missing = the block's first dispatch). a malformed value
/// is host-store corruption surfaced as a deterministic reject, never a
/// silent re-genesis (which would wipe the module).
fn load() -> Result<(ForgeState, BlockScratch), host::Error> {
    let image = match host::state_get(REFS_KEY) {
        None => Image::default(),
        Some(bytes) => decode_image(&bytes).map_err(to_wit_error)?,
    };
    let scratch = match host::state_get(BLOCK_SCRATCH_KEY) {
        None => BlockScratch::default(),
        Some(bytes) => decode_block_scratch(&bytes).map_err(to_wit_error)?,
    };
    let state = ForgeState::from_lane(image, scratch.clone());
    Ok((state, scratch))
}

/// dispatch one op: re-enter the block, apply through the shared core, then
/// hand this dispatch's packed heads to the object plane and re-stage the
/// chained image + scratch as OUTER host writes. the host adopts the image at
/// the block boundary (dropping the scratch) or discards both on abort.
fn execute(payload: Vec<u8>) -> Result<(), host::Error> {
    let (mut state, before) = load()?;
    let chain_id = ducktape_module_sdk::genesis_chain_id("forge")?;
    let mut ctx = WitCtx::new();
    block_on(state.apply(
        &mut ctx,
        &payload,
        Some(CHAT_MODULE),
        Some("attribution"),
        &chain_id,
    ))
    .map_err(to_wit_error)?;
    for target in state.ref_targets_since(&before) {
        host::object_put(REF_TARGET_KIND, &encode_ref_target(&target));
    }
    host::state_set(REFS_KEY, &state.published_image());
    host::state_set(
        BLOCK_SCRATCH_KEY,
        &encode_block_scratch(&state.block_scratch()),
    );
    Ok(())
}

/// the `ducktape:module` component export. the packaging cdylib around this
/// crate is synthesized by `guest-builder` — this export is the whole of the
/// guest's entry wiring.
struct Component;

impl Guest for Component {
    fn initialize(_params: Vec<u8>) -> Result<(), host::Error> {
        Ok(())
    }

    fn finalize_block() -> Result<(), host::Error> {
        Ok(())
    }

    /// an odb port: the host wraps this component over the git substrate it
    /// provides for the module's id, seeding the `chain_id` genesis-config
    /// key [`pushcert::signer`](crate::pushcert::signer) checks a push
    /// certificate's nonce against.
    fn shape() -> host::ModuleShape {
        host::ModuleShape {
            config: vec![sdk::genesis_config::CHAIN_ID.into()],
            ..ducktape_module_sdk::git_shape()
        }
    }

    fn execute(payload: Vec<u8>) -> Result<(), host::Error> {
        execute(payload)
    }

    fn pending_items() -> Result<Vec<host::PendingItem>, host::Error> {
        Ok(Vec::new())
    }

    fn acknowledge(_ack: host::Ack) -> Result<(), host::Error> {
        Err(host::Error::Unsupported)
    }

    fn query(req: Vec<u8>) -> Result<Vec<u8>, host::Error> {
        let image = match host::state_get_committed(REFS_KEY) {
            Some(bytes) => decode_image(&bytes).map_err(to_wit_error)?,
            None => Image::default(),
        };
        crate::query::Reader {
            image: &image,
            git: crate::query::GuestGit,
        }
        .query(&req)
        .map_err(to_wit_error)
    }
}

ducktape_module_sdk::export_module!(Component);
