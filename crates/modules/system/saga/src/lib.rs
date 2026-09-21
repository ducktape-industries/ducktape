use abi::{Env, Origin, Refusal, Scan};
use guest::Program;
use modules::program::{bytes_key, conflict, invalid, not_found, u64_key, unauthorized};
use modules::saga::{
    Assignment, Callback, Op, Outcome, Query, Reply, Saga, Status, Trigger, Usage, Work, owns,
};
use modules::{capability, valset};

const SAGA: &str = "s/";
const EXPIRY: &str = "x/";
const ASSIGNED: &str = "a/";

struct Sagas;

fn saga_key(id: &str) -> Vec<u8> {
    format!("{SAGA}{id}").into_bytes()
}

fn expiry_key(height: u64, id: &str) -> Vec<u8> {
    let mut key = u64_key(EXPIRY, height);
    key.push(b'/');
    key.extend_from_slice(id.as_bytes());
    key
}

fn assigned_prefix(node: &[u8]) -> Vec<u8> {
    let mut key = bytes_key(ASSIGNED, node);
    key.push(b'/');
    key
}

fn assigned_key(node: &[u8], id: &str) -> Vec<u8> {
    let mut key = assigned_prefix(node);
    key.extend_from_slice(id.as_bytes());
    key
}

impl Program for Sagas {
    fn execute(payload: &[u8]) -> Result<(), Refusal> {
        let env = guest::env();
        modules::acl::admit(&env)?;
        match abi::decode(payload)? {
            Op::Trigger(trigger) => start(&env, trigger),
            Op::Result {
                id,
                attempt,
                outcome,
                usage,
            } => result(&env, &id, attempt, outcome, usage),
            Op::Renew { id, attempt } => renew(&env, &id, attempt),
            Op::Reassign { id, attempt } => reassign(&env, &id, attempt),
            Op::Accept { id, attempt } => accept(&env, &id, attempt),
            Op::Crank => crank(&env),
            Op::Cancel { id } => cancel(&env, &id),
            Op::Prune { ids } => prune(&env, &ids),
        }
    }

    fn query(request: &[u8]) -> Result<(), Refusal> {
        let reply = match abi::decode(request)? {
            Query::Get { id } => Reply::Saga(guest::record(saga_key(&id))?.map(Box::new)),
            Query::NextExpiry => Reply::NextExpiry(next_expiry()?),
            Query::Assigned { node, page } => {
                let mut work = Vec::new();
                for entry in guest::scan(page.scan(&assigned_prefix(&node))) {
                    let id = String::from_utf8_lossy(&entry.key[assigned_prefix(&node).len()..]);
                    work.push(work_of(&saga(&id)?));
                }
                Reply::Work(work)
            }
            Query::Pending { page } => Reply::Sagas(
                guest::records::<Saga>(page.scan(SAGA.as_bytes()))?
                    .into_iter()
                    .map(|(_, saga)| saga)
                    .filter(|saga| saga.status == Status::Pending)
                    .collect(),
            ),
        };
        guest::reply(&reply);
        Ok(())
    }
}

fn saga(id: &str) -> Result<Saga, Refusal> {
    guest::record(saga_key(id))?.ok_or_else(|| not_found(format!("saga {id}")))
}

fn expiry_of(saga: &Saga) -> Option<u64> {
    if saga.status != Status::Pending {
        return None;
    }
    let lease = match &saga.assignment {
        Assignment::Leased { until, .. } => *until,
        Assignment::Unassigned => None,
    };
    match (lease, saga.trigger.deadline) {
        (Some(lease), Some(deadline)) => Some(lease.min(deadline)),
        (Some(lease), None) => Some(lease),
        (None, deadline) => deadline,
    }
}

fn assignee_of(saga: &Saga) -> Option<&[u8]> {
    if saga.status != Status::Pending {
        return None;
    }
    match &saga.assignment {
        Assignment::Leased { assignee, .. } => Some(assignee),
        Assignment::Unassigned => None,
    }
}

fn unindex(saga: &Saga) {
    if let Some(expiry) = expiry_of(saga) {
        guest::delete(expiry_key(expiry, &saga.trigger.id));
    }
    if let Some(assignee) = assignee_of(saga) {
        guest::delete(assigned_key(assignee, &saga.trigger.id));
    }
}

fn store(saga: &Saga) {
    guest::put(saga_key(&saga.trigger.id), saga);
    if let Some(expiry) = expiry_of(saga) {
        guest::set(expiry_key(expiry, &saga.trigger.id), Vec::new());
    }
    if let Some(assignee) = assignee_of(saga) {
        guest::set(assigned_key(assignee, &saga.trigger.id), Vec::new());
    }
}

fn work_of(saga: &Saga) -> Work {
    Work {
        id: saga.trigger.id.clone(),
        attempt: saga.attempt,
        spec: saga.trigger.spec.clone(),
        deadline: saga.trigger.deadline,
        assignee: assignee_of(saga).map(<[u8]>::to_vec),
    }
}

fn announce(saga: &Saga) {
    guest::event(abi::encode(&work_of(saga)));
}

fn settle(saga: &mut Saga, outcome: Outcome) {
    unindex(saga);
    saga.status = Status::Settled(outcome.clone());
    store(saga);
    if let Some(reply_to) = &saga.trigger.reply_to {
        guest::emit(
            reply_to.clone(),
            abi::encode(&Callback {
                id: saga.trigger.id.clone(),
                correlation: saga.trigger.correlation.clone(),
                outcome,
            }),
        );
    }
}

fn pool(trigger: &Trigger) -> Result<Vec<Vec<u8>>, Refusal> {
    match &trigger.capability {
        Some(tag) => capability::providers(tag, &trigger.demands),
        None => match guest::ask(valset::PROGRAM, &valset::Query::Validators)? {
            valset::Reply::Validators(validators) => Ok(validators),
            other => Err(Refusal::new(
                modules::reason::PROTOCOL,
                format!("valset answered Validators with {other:?}"),
            )),
        },
    }
}

fn rendezvous(trigger: &Trigger, attempt: u32, pool: Vec<Vec<u8>>) -> Option<Vec<u8>> {
    pool.into_iter().min_by_key(|node| {
        let mut preimage = trigger.id.as_bytes().to_vec();
        preimage.extend_from_slice(&attempt.to_be_bytes());
        preimage.extend_from_slice(node);
        guest::sha256(preimage)
    })
}

fn lease(env: &Env, trigger: &Trigger) -> Option<u64> {
    let until = trigger.lease.map(|blocks| env.height + blocks)?;
    Some(match trigger.deadline {
        Some(deadline) => until.min(deadline),
        None => until,
    })
}

fn assign(env: &Env, saga: &Saga) -> Result<Assignment, Refusal> {
    let assignee = match &saga.trigger.pinned {
        Some(pinned) => Some(pinned.clone()),
        None => rendezvous(&saga.trigger, saga.attempt, pool(&saga.trigger)?),
    };
    Ok(match assignee {
        Some(assignee) => Assignment::Leased {
            assignee,
            until: lease(env, &saga.trigger),
        },
        None => Assignment::Unassigned,
    })
}

fn start(env: &Env, trigger: Trigger) -> Result<(), Refusal> {
    if !owns(&env.origin, &trigger.id) {
        return Err(unauthorized(format!(
            "{} is outside this origin's namespace",
            trigger.id
        )));
    }
    if trigger.attempts == 0 {
        return Err(invalid("a saga makes at least one attempt"));
    }
    let pins_nobody = trigger.pinned.as_ref().is_some_and(Vec::is_empty);
    if pins_nobody {
        return Err(invalid("a pinned assignee is a node key"));
    }
    let exists = guest::get(saga_key(&trigger.id)).is_some();
    if exists {
        return Ok(());
    }
    let mut saga = Saga {
        trigger,
        origin: env.origin.clone(),
        opened_at: env.height,
        attempt: 1,
        assignment: Assignment::Unassigned,
        status: Status::Pending,
        usage: Usage::default(),
    };
    saga.assignment = assign(env, &saga)?;
    store(&saga);
    announce(&saga);
    Ok(())
}

fn pending(id: &str, attempt: u32) -> Result<Saga, Refusal> {
    let saga = saga(id)?;
    if saga.status != Status::Pending {
        return Err(Refusal::new(
            modules::reason::CLOSED,
            format!("saga {id} is settled"),
        ));
    }
    let current = saga.attempt == attempt;
    if !current {
        return Err(conflict(format!(
            "saga {id} is on attempt {}, not {attempt}",
            saga.attempt
        )));
    }
    Ok(saga)
}

fn holder(env: &Env, saga: &Saga) -> Result<(), Refusal> {
    let holds = match (&env.origin, &saga.assignment) {
        (Origin::External(key), Assignment::Leased { assignee, .. }) => key == assignee,
        _ => false,
    };
    if !holds {
        return Err(unauthorized("only the attempt's assignee may do this"));
    }
    Ok(())
}

fn trigger_origin(env: &Env, saga: &Saga) -> Result<(), Refusal> {
    if env.origin != saga.origin {
        return Err(unauthorized("only the saga's trigger origin may do this"));
    }
    Ok(())
}

fn retry(env: &Env, saga: &mut Saga, reason: String) -> Result<(), Refusal> {
    let exhausted = saga.attempt >= saga.trigger.attempts;
    if exhausted {
        settle(saga, Outcome::Failed(reason));
        return Ok(());
    }
    unindex(saga);
    saga.attempt += 1;
    saga.assignment = assign(env, saga)?;
    store(saga);
    announce(saga);
    Ok(())
}

fn add(usage: &mut Usage, reported: &Usage) {
    usage.input_tokens += reported.input_tokens;
    usage.cached_input_tokens += reported.cached_input_tokens;
    usage.cache_write_input_tokens += reported.cache_write_input_tokens;
    usage.output_tokens += reported.output_tokens;
    usage.reasoning_output_tokens += reported.reasoning_output_tokens;
}

fn result(
    env: &Env,
    id: &str,
    attempt: u32,
    outcome: Result<Vec<u8>, String>,
    usage: Usage,
) -> Result<(), Refusal> {
    let mut saga = pending(id, attempt)?;
    holder(env, &saga)?;
    add(&mut saga.usage, &usage);
    match outcome {
        Ok(bytes) => {
            settle(&mut saga, Outcome::Done(bytes));
            Ok(())
        }
        Err(error) => retry(env, &mut saga, error),
    }
}

fn renew(env: &Env, id: &str, attempt: u32) -> Result<(), Refusal> {
    let mut saga = pending(id, attempt)?;
    holder(env, &saga)?;
    unindex(&saga);
    if let Assignment::Leased { until, .. } = &mut saga.assignment {
        *until = lease(env, &saga.trigger);
    }
    store(&saga);
    Ok(())
}

fn reassign(env: &Env, id: &str, attempt: u32) -> Result<(), Refusal> {
    let mut saga = pending(id, attempt)?;
    trigger_origin(env, &saga)?;
    retry(env, &mut saga, "reassigned by the trigger origin".into())
}

fn accept(env: &Env, id: &str, attempt: u32) -> Result<(), Refusal> {
    let node = modules::program::external(env)?;
    let mut saga = pending(id, attempt)?;
    let open = saga.assignment == Assignment::Unassigned;
    if !open {
        return Ok(());
    }
    saga.assignment = Assignment::Leased {
        assignee: node,
        until: lease(env, &saga.trigger),
    };
    store(&saga);
    announce(&saga);
    Ok(())
}

fn crank(env: &Env) -> Result<(), Refusal> {
    let past = u64_key(EXPIRY, env.height + 1);
    for entry in guest::scan(Scan::range(EXPIRY.as_bytes().to_vec(), Some(past))) {
        let id = String::from_utf8_lossy(&entry.key[EXPIRY.len() + 9..]).into_owned();
        let mut saga = saga(&id)?;
        let timed_out = saga
            .trigger
            .deadline
            .is_some_and(|deadline| env.height >= deadline);
        if timed_out {
            settle(&mut saga, Outcome::TimedOut);
            continue;
        }
        retry(env, &mut saga, "the lease expired".into())?;
    }
    Ok(())
}

fn cancel(env: &Env, id: &str) -> Result<(), Refusal> {
    let mut saga = saga(id)?;
    trigger_origin(env, &saga)?;
    if saga.status != Status::Pending {
        return Ok(());
    }
    settle(&mut saga, Outcome::Cancelled);
    Ok(())
}

fn prune(env: &Env, ids: &[String]) -> Result<(), Refusal> {
    for id in ids {
        let saga = saga(id)?;
        trigger_origin(env, &saga)?;
        if saga.status == Status::Pending {
            return Err(conflict(format!("saga {id} is pending")));
        }
        guest::delete(saga_key(id));
    }
    Ok(())
}

fn next_expiry() -> Result<Option<u64>, Refusal> {
    let first = guest::scan(Scan::prefix(EXPIRY).limit(1))
        .into_iter()
        .next();
    Ok(first.map(|entry| {
        u64::from_be_bytes(
            entry.key[EXPIRY.len()..EXPIRY.len() + 8]
                .try_into()
                .expect("an expiry key carries its height"),
        )
    }))
}

guest::program!(Sagas);
