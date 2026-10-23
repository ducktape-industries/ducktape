// The node dials its own registered halo address as a member would, at start and whenever that
// address changes or is cleared, and reports whether it reached itself (`/v1/halo/self`). It only
// warns: a network that does not route a host back to its own public address fails the dial for it.

use std::sync::Arc;

use crate::wire::HaloSelf;
use crate::{Context, Daemon};

/// What the seating says of this node's own halo address.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Registered {
    Unseated,
    Unaddressed,
    At(String),
}

fn registered(halo: &halo::Halo) -> Registered {
    let members = halo.members();
    let seated = members.seated(halo.me());
    if !seated {
        return Registered::Unseated;
    }
    match members.halo(halo.me()) {
        Some(at) => Registered::At(at),
        None => Registered::Unaddressed,
    }
}

pub(crate) fn first(halo: &halo::Halo) -> HaloSelf {
    before_dialing(registered(halo))
}

fn before_dialing(registered: Registered) -> HaloSelf {
    match registered {
        Registered::Unseated => HaloSelf::Unregistered,
        Registered::Unaddressed => HaloSelf::Unaddressed,
        Registered::At(at) => HaloSelf::Dialing { at },
    }
}

pub(crate) async fn dialing<E: Context>(daemon: Arc<Daemon<E>>) {
    let halo = daemon.halo.clone();
    let mut changed = halo.membership_changed();
    let mut dialed = None;
    loop {
        let now = registered(&halo);
        let moved = dialed.as_ref() != Some(&now);
        if moved {
            dialed = Some(now.clone());
            daemon.halo_self.send_replace(before_dialing(now.clone()));
            daemon.halo_self.send_replace(reached(&halo, now).await);
        }
        if changed.changed().await.is_err() {
            return;
        }
    }
}

async fn reached(halo: &halo::Halo, registered: Registered) -> HaloSelf {
    let Registered::At(at) = registered else {
        return before_dialing(registered);
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
