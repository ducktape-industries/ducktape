//! The contract stamp's guard: a node runs only on state applied under its own contract, and `roll`
//! carries a stopped workspace across a bump that changes how blocks decode, never by replaying one.
use std::cmp::Ordering;
use std::path::PathBuf;
use std::time::Duration;

use node::Node;

use crate::wire::NODE_CONTRACT;
use crate::workspace::{Descriptor, Stamp, Workspace};
use crate::{Client, Context, Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rolled {
    Already(Stamp),
    Stamped(Stamp),
    Synced { stamp: Stamp, aside: PathBuf },
}

pub(crate) fn admits(stamp: Stamp, tip: u64) -> Result<()> {
    if stamp.contract != NODE_CONTRACT {
        return Err(Error::Unrolled(format!(
            "this workspace's state was applied under contract {} and this binary speaks \
             {NODE_CONTRACT}: with the node stopped, run `ducktape roll <R>` at the announced \
             roll height R (its tip is {tip})",
            stamp.contract
        )));
    }
    let before_its_state = tip + 1 < stamp.from;
    if before_its_state {
        return Err(Error::Unrolled(format!(
            "this workspace's tip {tip} is below height {}, where its contract {} state starts, \
             so it holds state from before the roll: run `ducktape roll {}`",
            stamp.from,
            stamp.contract,
            stamp.from - 1
        )));
    }
    Ok(())
}

pub async fn roll<E: Context>(context: E, workspace: &Workspace, height: u64) -> Result<Rolled> {
    let stamp = workspace.stamp()?;
    let descriptor = workspace.descriptor()?;
    let tip = Node::open(context.child("node"), &descriptor.network, workspace.dir())
        .await?
        .tip()?
        .height;
    let current = stamp.contract == NODE_CONTRACT && tip + 1 >= stamp.from;
    if current {
        return Ok(Rolled::Already(stamp));
    }
    match tip.cmp(&height) {
        Ordering::Equal => {
            let stamp = Stamp {
                contract: NODE_CONTRACT,
                from: height + 1,
            };
            workspace.write_stamp(&stamp)?;
            Ok(Rolled::Stamped(stamp))
        }
        Ordering::Greater => Err(Error::Unrolled(format!(
            "this workspace's tip {tip} is past the roll height {height}: a binary before \
             contract {NODE_CONTRACT} applied blocks after the roll, so its state is not the \
             network's; move every file but identity.key and runtime/{network}-simplex-* aside, \
             then `ducktape join` again",
            network = descriptor.network
        ))),
        Ordering::Less => synced(context, workspace, &descriptor, height, tip).await,
    }
}

async fn synced<E: Context>(
    context: E,
    workspace: &Workspace,
    descriptor: &Descriptor,
    height: u64,
    tip: u64,
) -> Result<Rolled> {
    let Some(base) = workspace.source()? else {
        return Err(Error::Unrolled(format!(
            "this workspace's tip {tip} is behind the roll height {height} and it names no \
             source to sync from"
        )));
    };
    let source = Client::new(base.clone());
    let status = source.status().await?;
    let serves = status.contract >= NODE_CONTRACT && status.height >= height;
    if !serves {
        return Err(unserved(&base, status.contract, status.height, height));
    }
    let aside = workspace.set_aside(&format!("pre-roll-{tip}"))?;
    let joined = statesync::join(
        context.child("join"),
        &descriptor.network,
        workspace.dir(),
        descriptor.id(),
        Duration::from_millis(descriptor.block_time_ms),
        source,
    )
    .await?;
    let head = joined.node.tip()?.height;
    if head < height {
        return Err(unserved(&base, status.contract, head, height));
    }
    workspace.write_anchor(&joined.anchor)?;
    let stamp = Stamp {
        contract: NODE_CONTRACT,
        from: head + 1,
    };
    workspace.write_stamp(&stamp)?;
    Ok(Rolled::Synced { stamp, aside })
}

fn unserved(base: &str, contract: u32, head: u64, height: u64) -> Error {
    Error::Unrolled(format!(
        "the source {base} is on contract {contract} at head {head}: a node behind the roll \
         height {height} syncs only from a source on contract {NODE_CONTRACT} or later whose \
         head is at {height} or past"
    ))
}
