//! Who may publish and subscribe in a room: the room's program wrote the answer as state under
//! the `live` table, and the node reads it where it reads any state. The row for the account
//! decides; failing that the `*` row; no row means no.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use abi::live::{self, Grant};
use abi::{Origin, role::identity};
use host::Layer;
use live_wire::{Account, pattern_covers};

use crate::Shared;

pub type Answered<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the relay reads from the chain: the account a key acts as, and a grant row.
pub trait Chain: Send + Sync + 'static {
    /// The account the identity role answers for `key`; `None` when it holds none or does
    /// not act.
    fn account(&self, key: &[u8]) -> Answered<'_, Option<Account>>;
    /// The grant row `live/<room>/<who>` of `module`, decoded.
    fn grant(&self, module: &str, room: &str, who: Option<Account>) -> Answered<'_, Option<Grant>>;
}

impl<E: crate::Context> Chain for Shared<E> {
    fn account(&self, key: &[u8]) -> Answered<'_, Option<Account>> {
        let key = key.to_vec();
        Box::pin(async move {
            let node = self.lock().await;
            let identity = node.host().roles().identity.clone();
            let asked = abi::encode(&identity::Query::Account(key));
            let answered = node
                .query(Layer::Preconfirmed, Origin::System, &identity, asked)
                .await;
            let Ok(Ok(bytes)) = answered else {
                return None;
            };
            match abi::decode(&bytes) {
                Ok(identity::Reply::Account(account)) => account,
                Ok(_) | Err(_) => None,
            }
        })
    }

    fn grant(&self, module: &str, room: &str, who: Option<Account>) -> Answered<'_, Option<Grant>> {
        let key = live::key(room, who);
        let module = module.to_owned();
        Box::pin(async move {
            let node = self.lock().await;
            let row = node.view(Layer::Preconfirmed).get(&module, &key).ok()??;
            abi::decode(&row).ok()
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Publish,
    Subscribe,
}

/// Where a track lives, as its namespace spells it: `<chain>/<module>/<room>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place<'a> {
    pub chain: &'a str,
    pub module: &'a str,
    pub room: &'a str,
}

pub fn place(namespace: &str) -> Option<Place<'_>> {
    let (chain, rest) = namespace.split_once('/')?;
    let (module, room) = rest.split_once('/')?;
    let whole = !chain.is_empty() && !module.is_empty() && !room.is_empty();
    whole.then_some(Place {
        chain,
        module,
        room,
    })
}

fn covered(grant: &Grant, act: Act, name: &str) -> bool {
    let patterns = match act {
        Act::Publish => &grant.publish,
        Act::Subscribe => &grant.subscribe,
    };
    patterns.iter().any(|pattern| pattern_covers(pattern, name))
}

type Room = (String, String, Account);

/// The grant decision per (module, room, account), read once from the chain and kept until a
/// block writes under the module's `live/` table.
pub struct Grants {
    chain: Arc<dyn Chain>,
    decided: Mutex<HashMap<Room, Option<Grant>>>,
}

impl Grants {
    pub fn new(chain: Arc<dyn Chain>) -> Grants {
        Grants {
            chain,
            decided: Mutex::default(),
        }
    }

    pub fn chain(&self) -> &dyn Chain {
        self.chain.as_ref()
    }

    async fn decided(&self, module: &str, room: &str, account: Account) -> Option<Grant> {
        let key = (module.to_owned(), room.to_owned(), account);
        let known = self
            .decided
            .lock()
            .expect("the grants lock is never poisoned")
            .get(&key)
            .cloned();
        if let Some(grant) = known {
            return grant;
        }
        let specific = self.chain.grant(module, room, Some(account)).await;
        let grant = match specific {
            Some(grant) => Some(grant),
            None => self.chain.grant(module, room, None).await,
        };
        self.decided
            .lock()
            .expect("the grants lock is never poisoned")
            .insert(key, grant.clone());
        grant
    }

    /// Whether `account` may `act` on the track `name` in `module`'s `room`.
    pub async fn covers(
        &self,
        module: &str,
        room: &str,
        account: Account,
        act: Act,
        name: &str,
    ) -> bool {
        let Some(grant) = self.decided(module, room, account).await else {
            return false;
        };
        covered(&grant, act, name)
    }

    /// A block wrote under `module`'s `live/` table: every decision of the module is re-read.
    pub fn forget(&self, module: &str) {
        self.decided
            .lock()
            .expect("the grants lock is never poisoned")
            .retain(|(decided, _, _), _| decided != module);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_namespace_is_a_chain_a_module_and_a_room() {
        assert_eq!(
            place("net#0a1b2c3d/chat/general/sub"),
            Some(Place {
                chain: "net#0a1b2c3d",
                module: "chat",
                room: "general/sub",
            })
        );
        assert_eq!(place("net#0a1b2c3d/chat"), None);
        assert_eq!(place("net#0a1b2c3d//room"), None);
        assert_eq!(place("/chat/room"), None);
    }

    #[test]
    fn the_grants_side_of_the_row_decides_the_act() {
        let grant = Grant {
            publish: vec!["presence/*".into()],
            subscribe: vec!["*".into()],
        };
        assert!(covered(&grant, Act::Publish, "presence/42"));
        assert!(!covered(&grant, Act::Publish, "doc"));
        assert!(covered(&grant, Act::Subscribe, "doc"));
        assert!(!covered(&Grant::default(), Act::Subscribe, "doc"));
    }
}
