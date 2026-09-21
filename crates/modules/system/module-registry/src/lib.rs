use abi::{HashKind, Refusal, Scan};
use guest::Program;
use modules::AUTHORITY;
use modules::module_registry::{CODE_KIND, Change, Entry, Genesis, Op, Query, Reply, Scheduled};
use modules::program::{conflict, invalid, not_found, u64_key};

const PROGRAM: &str = "p/";
const SCHEDULE: &str = "s/";
const FOLDED: &[u8] = b"folded";

struct Modules;

fn program_key(program: &str) -> Vec<u8> {
    format!("{PROGRAM}{program}").into_bytes()
}

fn schedule_key(height: u64, program: &str) -> Vec<u8> {
    let mut key = u64_key(SCHEDULE, height);
    key.push(b'/');
    key.extend_from_slice(program.as_bytes());
    key
}

impl Program for Modules {
    fn init(params: &[u8]) -> Result<(), Refusal> {
        let genesis: Genesis = abi::decode(params)?;
        for entry in genesis.programs {
            guest::put(program_key(&entry.program), &entry);
        }
        guest::put(FOLDED, &0u64);
        Ok(())
    }

    fn execute(payload: &[u8]) -> Result<(), Refusal> {
        let env = guest::env();
        fold(env.height)?;
        match abi::decode(payload)? {
            Op::Publish { body } => publish(body),
            Op::Schedule(scheduled) => schedule(&env, scheduled),
            Op::Cancel { height, program } => cancel(&env, height, &program),
        }
    }

    fn query(request: &[u8]) -> Result<(), Refusal> {
        let env = guest::env();
        let reply = match abi::decode(request)? {
            Query::At(height) => Reply::Programs(at(height)?),
            Query::Scheduled => Reply::Scheduled(scheduled()?),
            Query::Program(program) => Reply::Program(
                at(env.height)?
                    .into_iter()
                    .find(|entry| entry.program == program),
            ),
        };
        guest::reply(&reply);
        Ok(())
    }
}

fn publish(body: Vec<u8>) -> Result<(), Refusal> {
    let id = guest::blob_put(HashKind::Sha256, CODE_KIND, body)?;
    guest::output(abi::encode(&id));
    Ok(())
}

fn schedule(env: &abi::Env, scheduled: Scheduled) -> Result<(), Refusal> {
    modules::program::from(env, AUTHORITY)?;
    let in_the_future = scheduled.height > env.height;
    if !in_the_future {
        return Err(invalid(format!(
            "a change lands at a later block than {}",
            env.height
        )));
    }
    if let Change::Set(entry) = &scheduled.change {
        let code_is_here = guest::blob_stat(entry.code).is_some();
        if !code_is_here {
            return Err(not_found(format!("code {:?} is not published", entry.code)));
        }
    }
    let key = schedule_key(scheduled.height, scheduled.change.program());
    let taken = guest::get(&key).is_some();
    if taken {
        return Err(conflict(format!(
            "{} already changes at {}",
            scheduled.change.program(),
            scheduled.height
        )));
    }
    guest::put(key, &scheduled.change);
    Ok(())
}

fn cancel(env: &abi::Env, height: u64, program: &str) -> Result<(), Refusal> {
    modules::program::from(env, AUTHORITY)?;
    let key = schedule_key(height, program);
    let pending = guest::get(&key).is_some();
    if !pending {
        return Err(not_found(format!("{program} does not change at {height}")));
    }
    guest::delete(key);
    Ok(())
}

fn fold(height: u64) -> Result<(), Refusal> {
    let folded: u64 = guest::record(FOLDED)?.unwrap_or(0);
    let nothing_new = folded >= height;
    if nothing_new {
        return Ok(());
    }
    for (key, change) in due(height)? {
        apply(&change);
        guest::delete(key);
    }
    guest::put(FOLDED, &height);
    Ok(())
}

fn apply(change: &Change) {
    match change {
        Change::Set(entry) => guest::put(program_key(&entry.program), entry),
        Change::Remove(program) => guest::delete(program_key(program)),
    }
}

fn due(height: u64) -> Result<Vec<(Vec<u8>, Change)>, Refusal> {
    let past_due = u64_key(SCHEDULE, height + 1);
    guest::records(Scan::range(SCHEDULE.as_bytes().to_vec(), Some(past_due)))
}

fn at(height: u64) -> Result<Vec<Entry>, Refusal> {
    let mut entries: Vec<Entry> = guest::records::<Entry>(Scan::prefix(PROGRAM))?
        .into_iter()
        .map(|(_, entry)| entry)
        .collect();
    for (_, change) in due(height)? {
        match change {
            Change::Set(entry) => {
                entries.retain(|running| running.program != entry.program);
                entries.push(entry);
            }
            Change::Remove(program) => entries.retain(|running| running.program != program),
        }
    }
    entries.sort_by(|a, b| a.program.cmp(&b.program));
    Ok(entries)
}

fn scheduled() -> Result<Vec<Scheduled>, Refusal> {
    guest::records::<Change>(Scan::prefix(SCHEDULE))?
        .into_iter()
        .map(|(key, change)| {
            let height = height_of(&key)?;
            Ok(Scheduled { height, change })
        })
        .collect()
}

fn height_of(key: &[u8]) -> Result<u64, Refusal> {
    let bytes = key
        .get(SCHEDULE.len()..SCHEDULE.len() + 8)
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .ok_or_else(|| invalid("a schedule key names no height"))?;
    Ok(u64::from_be_bytes(bytes))
}

guest::program!(Modules);
