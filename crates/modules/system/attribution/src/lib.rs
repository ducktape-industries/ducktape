use std::collections::BTreeMap;

use abi::{Env, ProgramId, Refusal, Scan};
use guest::Program;
use wire::attribution::{
    Actor, Change, Kind, Op, Query, Reason, Relation, Relations, Reply, Source, Transfer, Update,
};
use wire::program::{conflict, invalid, u64_key};
use wire::{AccountNumber, Page};

const OBJECT: &str = "o/";
const CHANGE: &str = "c/";
const TO: &str = "t/";
const OF: &str = "s/";
const SUBSCRIBER: &str = "sub/";
const SEQ: &[u8] = b"seq";

struct Attribution;

fn source_path(source: &Source) -> String {
    format!(
        "{}/{}/{}",
        source.program, source.object.kind, source.object.id
    )
}

fn object_key(source: &Source) -> Vec<u8> {
    format!("{OBJECT}{}", source_path(source)).into_bytes()
}

fn change_key(seq: u64) -> Vec<u8> {
    u64_key(CHANGE, seq)
}

fn to_prefix(recipient: AccountNumber) -> Vec<u8> {
    let mut key = u64_key(TO, recipient);
    key.push(b'/');
    key
}

fn of_prefix(source: &Source) -> Vec<u8> {
    format!("{OF}{}/", source_path(source)).into_bytes()
}

fn indexed(mut prefix: Vec<u8>, seq: u64) -> Vec<u8> {
    prefix.extend_from_slice(&seq.to_be_bytes());
    prefix
}

fn subscriber_key(program: &str) -> Vec<u8> {
    format!("{SUBSCRIBER}{program}").into_bytes()
}

impl Program for Attribution {
    fn execute(payload: &[u8]) -> Result<(), Refusal> {
        let env = guest::env();
        let program = wire::program::program(&env)?;
        match abi::decode(payload)? {
            Op::Attribute(update) => attribute(&env, &program, update),
            Op::AttributeBatch { updates } => {
                for update in updates {
                    attribute(&env, &program, update)?;
                }
                Ok(())
            }
            Op::Subscribe => {
                guest::set(subscriber_key(&program), Vec::new());
                Ok(())
            }
        }
    }

    fn query(request: &[u8]) -> Result<(), Refusal> {
        let reply = match abi::decode(request)? {
            Query::Relations { source } => Reply::Relations(guest::record(object_key(&source))?),
            Query::Changes { page } => Reply::Changes(
                guest::records::<Change>(page.scan(CHANGE.as_bytes()))?
                    .into_iter()
                    .map(|(_, change)| change)
                    .collect(),
            ),
            Query::ChangesTo { recipient, page } => {
                Reply::Changes(indexed_changes(to_prefix(recipient), &page)?)
            }
            Query::ChangesOf { source, page } => {
                Reply::Changes(indexed_changes(of_prefix(&source), &page)?)
            }
            Query::Subscribers => Reply::Subscribers(subscribers()),
        };
        guest::reply(&reply);
        Ok(())
    }
}

fn subscribers() -> Vec<ProgramId> {
    guest::scan(Scan::prefix(SUBSCRIBER))
        .into_iter()
        .map(|entry| String::from_utf8_lossy(&entry.key[SUBSCRIBER.len()..]).into_owned())
        .collect()
}

fn indexed_changes(prefix: Vec<u8>, page: &Page) -> Result<Vec<Change>, Refusal> {
    let mut changes = Vec::new();
    for entry in guest::scan(page.scan(&prefix)) {
        let seq = u64::from_be_bytes(
            entry.key[entry.key.len() - 8..]
                .try_into()
                .map_err(|_| invalid("an index key names no change"))?,
        );
        let change: Change = guest::record(change_key(seq))?
            .ok_or_else(|| conflict(format!("change {seq} is indexed but missing")))?;
        changes.push(change);
    }
    Ok(changes)
}

fn well_formed(update: &Update) -> Result<(), Refusal> {
    let object_named = !update.object.kind.is_empty() && !update.object.id.is_empty();
    if !object_named {
        return Err(invalid("an object has a kind and an id"));
    }
    let slashed = update.object.kind.contains('/') || update.object.id.contains('/');
    if slashed {
        return Err(invalid("an object kind or id carries no '/'"));
    }
    let actor_named = match &update.actor {
        Actor::Account(number) => *number != 0,
        Actor::Key(key) => !key.is_empty(),
        Actor::Program(program) => !program.is_empty(),
        Actor::System => true,
    };
    if !actor_named {
        return Err(invalid(
            "an actor is a live account, a key, a program or the system",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for relation in &update.relations {
        if relation.recipient == 0 {
            return Err(invalid("a recipient is a live account"));
        }
        let unnamed = matches!(&relation.reason, Reason::Defined(name) if name.is_empty());
        if unnamed {
            return Err(invalid("a defined reason has a name"));
        }
        let distinct = seen.insert((relation.recipient, relation.reason.clone()));
        if !distinct {
            return Err(invalid(format!(
                "account {} relates twice for {:?}",
                relation.recipient, relation.reason
            )));
        }
    }
    Ok(())
}

type Held = BTreeMap<(AccountNumber, Reason), Vec<u8>>;

fn held(relations: &[Relation]) -> Held {
    relations
        .iter()
        .map(|relation| {
            (
                (relation.recipient, relation.reason.clone()),
                relation.detail.clone(),
            )
        })
        .collect()
}

fn attribute(env: &Env, program: &str, update: Update) -> Result<(), Refusal> {
    well_formed(&update)?;
    let source = Source {
        program: program.to_owned(),
        object: update.object.clone(),
    };
    let previous = guest::record::<Relations>(object_key(&source))?;
    let stale = previous
        .as_ref()
        .is_some_and(|previous| update.revision <= previous.revision);
    if stale {
        return Err(conflict(format!(
            "{} was already reported at revision {}",
            source_path(&source),
            update.revision
        )));
    }
    let before = previous
        .map(|previous| held(&previous.relations))
        .unwrap_or_default();
    let after = held(&update.relations);
    let mut kinds: BTreeMap<(AccountNumber, Reason), Kind> = BTreeMap::new();
    for key in after.keys() {
        if !before.contains_key(key) {
            kinds.insert(key.clone(), Kind::Added);
        }
    }
    for key in before.keys() {
        if !after.contains_key(key) {
            kinds.insert(key.clone(), Kind::Withdrawn);
        }
    }
    for Transfer { reason, from, to } in &update.transfers {
        let withdrawn = kinds.get(&(*from, reason.clone())) == Some(&Kind::Withdrawn);
        let added = kinds.get(&(*to, reason.clone())) == Some(&Kind::Added);
        let matches_the_diff = withdrawn && added;
        if !matches_the_diff {
            return Err(invalid(format!(
                "the transfer of {reason:?} from {from} to {to} matches no withdrawal and addition"
            )));
        }
        kinds.insert((*from, reason.clone()), Kind::TransferredOut { to: *to });
        kinds.insert((*to, reason.clone()), Kind::TransferredIn { from: *from });
    }
    let mut relations = update.relations;
    relations.sort_by(|a, b| (a.recipient, &a.reason).cmp(&(b.recipient, &b.reason)));
    guest::put(
        object_key(&source),
        &Relations {
            source: source.clone(),
            revision: update.revision,
            relations,
        },
    );
    let subscribers = subscribers();
    let mut seq: u64 = guest::record(SEQ)?.unwrap_or(0);
    for ((recipient, reason), kind) in kinds {
        seq += 1;
        let detail = after
            .get(&(recipient, reason.clone()))
            .cloned()
            .unwrap_or_default();
        let change = Change {
            seq,
            source: source.clone(),
            revision: update.revision,
            recipient,
            reason,
            kind,
            detail,
            actor: update.actor.clone(),
            cause: env.cause.clone(),
            height: env.height,
        };
        guest::put(change_key(seq), &change);
        guest::set(indexed(to_prefix(recipient), seq), Vec::new());
        guest::set(indexed(of_prefix(&source), seq), Vec::new());
        for subscriber in &subscribers {
            guest::emit(subscriber.clone(), abi::encode(&change));
        }
    }
    guest::put(SEQ, &seq);
    Ok(())
}

guest::program!(Attribution);
