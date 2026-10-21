// The node dials its own registered halo address as a member would, at start and whenever that
// address changes, and reports whether it reached itself (`/v1/halo/self`). It only warns: a
// network that does not route a host back to its own public address fails the dial for it alone.

use std::sync::Arc;

use crate::wire::HaloSelf;
use crate::{Context, Daemon};

pub(crate) fn first(halo: &halo::Halo) -> HaloSelf {
    match halo.members().halo(halo.me()) {
        Some(at) => HaloSelf::Dialing { at },
        None => HaloSelf::Unregistered,
    }
}

pub(crate) async fn dialing<E: Context>(daemon: Arc<Daemon<E>>) {
    let halo = daemon.halo.clone();
    let mut changed = halo.membership_changed();
    let mut dialed = None;
    loop {
        let at = halo.members().halo(halo.me());
        if at != dialed {
            dialed.clone_from(&at);
            daemon.halo_self.send_replace(first(&halo));
            daemon.halo_self.send_replace(reached(&halo, at).await);
        }
        if changed.changed().await.is_err() {
            return;
        }
    }
}

async fn reached(halo: &halo::Halo, at: Option<String>) -> HaloSelf {
    let Some(at) = at else {
        return HaloSelf::Unregistered;
    };
    match halo.dial_self(&at).await {
        Ok(()) => {
            tracing::info!(
                target: "ducktape::halo",
                event = "halo_reached",
                %at,
                "the node reached itself at its registered halo address, as a member would"
            );
            HaloSelf::Reached { at }
        }
        Err(reason) => {
            tracing::warn!(
                target: "ducktape::halo",
                event = "halo_unreachable",
                %at,
                reason,
                "the node did not reach itself at its registered halo address; members reach it relayed or punched through instead of directly there"
            );
            HaloSelf::Unreachable {
                at,
                reason: reason.into(),
            }
        }
    }
}
