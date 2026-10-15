//! Who may publish and subscribe in a room: the room's program wrote the answer as state under
//! the `live` table, and the node reads it where it reads any state, on the confirmed layer the
//! blocks it hears of are applied to. The row for the account decides; failing that the `*`
//! row; no row means no. Nothing is cached: a control message is rare, and a row read once per
//! message is never stale.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Semaphore;

use abi::live::{self, Grant};
use abi::{Origin, role::identity};
use host::Layer;
use live_wire::{Account, for_account, pattern_covers};

use crate::Shared;

/// Chain reads in flight at once over every session: a session's control messages queue here
/// rather than at the node lock.
const READS_IN_FLIGHT: usize = 64;

pub type Answered<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the relay reads from the chain: the account a key acts as, and a grant.
pub trait Chain: Send + Sync + 'static {
    /// The account the identity role answers for `key`; `None` when it holds none or does
    /// not act.
    fn account(&self, key: &[u8]) -> Answered<'_, Option<Account>>;
    /// The grant of `program` for `account` in `room`: its own row, else the `*` row, read
    /// together so no block falls between them. A row that does not decode is a deny.
    fn grant(&self, program: &str, room: &str, account: Account) -> Answered<'_, Option<Grant>>;
}

/// A grant row as the relay reads it: absent, or decoded with a row that is not a grant read
/// as the empty one.
fn decoded(row: Option<Vec<u8>>) -> Option<Grant> {
    let row = row?;
    Some(abi::decode(&row).unwrap_or_default())
}

impl<E: crate::Context> Chain for Shared<E> {
    fn account(&self, key: &[u8]) -> Answered<'_, Option<Account>> {
        let key = key.to_vec();
        Box::pin(async move {
            let node = self.lock().await;
            let identity = node.host().roles().identity.clone();
            let asked = abi::encode(&identity::Query::Account(key));
            let answered = node
                .query(Layer::Confirmed, Origin::System, &identity, asked)
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

    fn grant(&self, program: &str, room: &str, account: Account) -> Answered<'_, Option<Grant>> {
        let own = live::key(room, Some(account));
        let everyone = live::key(room, None);
        let program = program.to_owned();
        Box::pin(async move {
            let node = self.lock().await;
            let view = node.view(Layer::Confirmed);
            let own = decoded(view.get(&program, &own).ok().flatten());
            match own {
                Some(grant) => Some(grant),
                None => decoded(view.get(&program, &everyone).ok().flatten()),
            }
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Publish,
    Subscribe,
    /// Hearing a room's announces: any subscribe pattern in the room is enough, and the
    /// watcher hears the tracks its patterns cover.
    Watch,
}

/// Where a track lives, as its namespace spells it: `<chain>/<program>/<room>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place<'a> {
    pub chain: &'a str,
    pub program: &'a str,
    pub room: &'a str,
}

/// Whether a key a block wrote is a grant row, so the relay re-reads the program's grants:
/// `live/<room>/<who>`, under `abi::live::TABLE`.
pub fn is_grant_row(key: &[u8]) -> bool {
    let table = live::TABLE.as_bytes();
    key.starts_with(table) && key.get(table.len()) == Some(&b'/')
}

pub fn place(namespace: &str) -> Option<Place<'_>> {
    let (chain, rest) = namespace.split_once('/')?;
    let (program, room) = rest.split_once('/')?;
    let whole = !chain.is_empty() && !program.is_empty() && !room.is_empty();
    whole.then_some(Place {
        chain,
        program,
        room,
    })
}

/// Whether the grant lets `account` `act` on the track `name`, each pattern read as it reads
/// for that account (`{account}` its number).
pub fn covered(grant: &Grant, act: Act, name: &str, account: Account) -> bool {
    let patterns = match act {
        Act::Publish => &grant.publish,
        Act::Subscribe => &grant.subscribe,
        Act::Watch => return !grant.subscribe.is_empty(),
    };
    patterns
        .iter()
        .any(|pattern| pattern_covers(&for_account(pattern, account), name))
}

/// The subscribe patterns `account` hears a room's announces through, as they read for it,
/// `None` when it may not watch.
pub fn watch_patterns(grant: Option<&Grant>, account: Account) -> Option<Vec<String>> {
    let grant = grant?;
    let may_watch = covered(grant, Act::Watch, "*", account);
    may_watch.then(|| {
        grant
            .subscribe
            .iter()
            .map(|pattern| for_account(pattern, account))
            .collect()
    })
}

/// The chain's grants, read per request, and a count of the blocks that moved them so a
/// request decided across one is re-read.
pub struct Grants {
    chain: Arc<dyn Chain>,
    generation: AtomicU64,
    identity: AtomicU64,
    reads: Semaphore,
}

impl Grants {
    pub fn new(chain: Arc<dyn Chain>) -> Grants {
        Grants {
            chain,
            generation: AtomicU64::new(0),
            identity: AtomicU64::new(0),
            reads: Semaphore::new(READS_IN_FLIGHT),
        }
    }

    /// The account the chain answers for `key`, read under a permit.
    pub async fn account(&self, key: &[u8]) -> Option<Account> {
        let _read = self
            .reads
            .acquire()
            .await
            .expect("the reads semaphore is never closed");
        self.chain.account(key).await
    }

    /// The grant `account` holds in `program`'s `room`, if the program wrote one, read under a
    /// permit.
    pub async fn grant(&self, program: &str, room: &str, account: Account) -> Option<Grant> {
        let _read = self
            .reads
            .acquire()
            .await
            .expect("the reads semaphore is never closed");
        self.chain.grant(program, room, account).await
    }

    /// Moves when a block wrote grants: a decision read before and acted on after is re-read.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn moved(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Moves when a block wrote the identity table: an account read for a session before and
    /// registered after is re-checked, the mirror of `generation` for the handshake's account.
    pub fn identity_generation(&self) -> u64 {
        self.identity.load(Ordering::Acquire)
    }

    pub fn identity_moved(&self) {
        self.identity.fetch_add(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_namespace_is_a_chain_a_program_and_a_room() {
        assert_eq!(
            place("net#0a1b2c3d/chat/general/sub"),
            Some(Place {
                chain: "net#0a1b2c3d",
                program: "chat",
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
        assert!(covered(&grant, Act::Publish, "presence/42", 42));
        assert!(!covered(&grant, Act::Publish, "doc", 42));
        assert!(covered(&grant, Act::Subscribe, "doc", 42));
        assert!(covered(&grant, Act::Watch, "*", 42));
        assert!(!covered(&Grant::default(), Act::Subscribe, "doc", 42));
        assert!(!covered(&Grant::default(), Act::Watch, "*", 42));
    }

    #[test]
    fn an_account_pattern_covers_its_owner_and_no_one_else() {
        let room = Grant {
            publish: vec!["echo/{account}".into(), "board/{account}/*".into()],
            subscribe: vec!["*".into()],
        };
        assert!(covered(&room, Act::Publish, "echo/42", 42));
        assert!(covered(&room, Act::Publish, "board/42/0b", 42));
        assert!(!covered(&room, Act::Publish, "echo/42", 13));
        assert!(!covered(&room, Act::Publish, "board/42/0b", 13));
        assert!(covered(&room, Act::Subscribe, "echo/42", 13));
        let inbox = Grant {
            publish: Vec::new(),
            subscribe: vec!["inbox/{account}".into(), "lobby".into()],
        };
        assert_eq!(
            watch_patterns(Some(&inbox), 42),
            Some(vec!["inbox/42".to_owned(), "lobby".to_owned()]),
            "a watcher hears its own names alone"
        );
    }

    #[test]
    fn a_row_that_is_not_a_grant_denies_rather_than_falls_through() {
        assert_eq!(decoded(None), None);
        assert_eq!(
            decoded(Some(vec![0xff, 0xff, 0xff])),
            Some(Grant::default())
        );
        let open = abi::encode(&Grant::all());
        assert_eq!(decoded(Some(open)), Some(Grant::all()));
    }
}
