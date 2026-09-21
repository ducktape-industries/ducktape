use std::collections::BTreeMap;

use abi::{Cause, Env, ItemRef, Origin, ProgramId, Refusal};
use guest::Program;
use wire::dispatch::{
    Admission, Contract, Dispatch, Manifest, Op, Outcome, Query, Recipe, Reply, Routing, Spec,
    Status, saga_id,
};
use wire::program::{conflict, invalid, not_found, u64_key, unauthorized};
use wire::{capability, saga};

const RECIPE: &str = "r/";
const DISPATCH: &str = "d/";
const TRIGGER: &str = "i/";

struct Dispatcher;

fn recipe_key(id: &str) -> Vec<u8> {
    format!("{RECIPE}{id}").into_bytes()
}

fn dispatch_prefix(receiver: &str) -> Vec<u8> {
    format!("{DISPATCH}{receiver}/").into_bytes()
}

fn dispatch_key(receiver: &str, id: &str) -> Vec<u8> {
    format!("{DISPATCH}{receiver}/{id}").into_bytes()
}

fn trigger_key(item: u64) -> Vec<u8> {
    u64_key(TRIGGER, item)
}

impl Program for Dispatcher {
    fn execute(payload: &[u8]) -> Result<(), Refusal> {
        let env = guest::env();
        match &env.cause {
            Cause::Direct => op(&env, payload),
            Cause::Delivery(_) => delivered(&env, payload),
            Cause::Completion { item, outcome } => completed(&env, item, outcome),
        }
    }

    fn query(request: &[u8]) -> Result<(), Refusal> {
        let reply = match abi::decode(request)? {
            Query::Recipe { id } => Reply::Recipe(guest::record(recipe_key(&id))?),
            Query::Recipes { page } => Reply::Recipes(
                guest::records::<Recipe>(page.scan(RECIPE.as_bytes()))?
                    .into_iter()
                    .map(|(_, recipe)| recipe)
                    .collect(),
            ),
            Query::Dispatch { receiver, id } => {
                Reply::Dispatch(guest::record(dispatch_key(&receiver, &id))?)
            }
            Query::Dispatches { receiver, page } => Reply::Dispatches(
                guest::records::<Dispatch>(page.scan(&dispatch_prefix(&receiver)))?
                    .into_iter()
                    .map(|(_, dispatch)| dispatch)
                    .collect(),
            ),
        };
        guest::reply(&reply);
        Ok(())
    }
}

fn delivered(env: &Env, payload: &[u8]) -> Result<(), Refusal> {
    let from_saga = env.origin == Origin::Program(saga::PROGRAM.into());
    match from_saga {
        true => callback(env, abi::decode(payload)?),
        false => op(env, payload),
    }
}

fn op(env: &Env, payload: &[u8]) -> Result<(), Refusal> {
    wire::acl::admit(env)?;
    match abi::decode(payload)? {
        Op::SetRecipe { id, manifest } => set_recipe(env, id, manifest),
        Op::RemoveRecipe { id } => remove_recipe(env, &id),
        Op::Dispatch {
            id,
            recipe,
            payload,
            demands,
            admission,
        } => dispatch(env, id, &recipe, payload, demands, admission),
        Op::Cancel { id } => cancel(env, &id),
        Op::Reassign { id, attempt } => reassign(env, &id, attempt),
    }
}

fn recipe(id: &str) -> Result<Recipe, Refusal> {
    guest::record(recipe_key(id))?.ok_or_else(|| not_found(format!("recipe {id}")))
}

fn set_recipe(env: &Env, id: String, manifest: Manifest) -> Result<(), Refusal> {
    if !capability::tag_is_well_formed(&manifest.capability) {
        return Err(invalid(format!(
            "{:?} is not a capability tag",
            manifest.capability
        )));
    }
    if manifest.attempts == 0 {
        return Err(invalid("a recipe makes at least one attempt"));
    }
    let pins_nobody = matches!(&manifest.routing, Routing::Pinned(node) if node.is_empty());
    if pins_nobody {
        return Err(invalid("a pinned route names a node key"));
    }
    let recipe = match guest::record::<Recipe>(recipe_key(&id))? {
        Some(existing) => {
            if existing.owner != env.origin {
                return Err(unauthorized(format!(
                    "recipe {id} belongs to another origin"
                )));
            }
            Recipe {
                manifest,
                updated_at: env.height,
                ..existing
            }
        }
        None => Recipe {
            id: id.clone(),
            owner: env.origin.clone(),
            manifest,
            created_at: env.height,
            updated_at: env.height,
        },
    };
    guest::put(recipe_key(&id), &recipe);
    Ok(())
}

fn remove_recipe(env: &Env, id: &str) -> Result<(), Refusal> {
    let recipe = recipe(id)?;
    if recipe.owner != env.origin {
        return Err(unauthorized(format!(
            "recipe {id} belongs to another origin"
        )));
    }
    guest::delete(recipe_key(id));
    Ok(())
}

fn dispatch(
    env: &Env,
    id: String,
    recipe_id: &str,
    payload: Vec<u8>,
    demands: BTreeMap<String, u64>,
    admission: Admission,
) -> Result<(), Refusal> {
    let receiver = wire::program::program(env)?;
    let recipe = recipe(recipe_id)?;
    for (dimension, amount) in &demands {
        if !capability::tag_is_well_formed(dimension) {
            return Err(invalid(format!(
                "{dimension:?} is not a resource dimension"
            )));
        }
        if *amount == 0 {
            return Err(invalid(format!("{dimension:?} demands nothing; omit it")));
        }
    }
    let exists = guest::get(dispatch_key(&receiver, &id)).is_some();
    if exists {
        return Ok(());
    }
    let saga = saga_id(&receiver, &id);
    let spec = Spec {
        receiver: receiver.clone(),
        id: id.clone(),
        capability: recipe.manifest.capability.clone(),
        payload,
        demands: demands.clone(),
        admission,
    };
    let pinned = match &recipe.manifest.routing {
        Routing::Capability => None,
        Routing::Pinned(node) => Some(node.clone()),
    };
    let item = guest::call(
        saga::PROGRAM,
        abi::encode(&saga::Op::Trigger(saga::Trigger {
            id: saga.clone(),
            spec: abi::encode(&spec),
            reply_to: Some(wire::dispatch::PROGRAM.into()),
            correlation: abi::encode(&(receiver.clone(), id.clone())),
            deadline: recipe.manifest.deadline.map(|blocks| env.height + blocks),
            attempts: recipe.manifest.attempts,
            lease: recipe.manifest.lease,
            capability: Some(recipe.manifest.capability.clone()),
            demands,
            pinned,
        })),
    );
    guest::put(trigger_key(item.item), &(receiver.clone(), id.clone()));
    guest::put(
        dispatch_key(&receiver, &id),
        &Dispatch {
            id,
            recipe: recipe.id,
            receiver,
            status: Status::Running { saga },
            created_at: env.height,
            updated_at: env.height,
        },
    );
    Ok(())
}

fn running(env: &Env, id: &str) -> Result<Option<(Dispatch, String)>, Refusal> {
    let receiver = wire::program::program(env)?;
    let Some(dispatch) = guest::record::<Dispatch>(dispatch_key(&receiver, id))? else {
        return Ok(None);
    };
    Ok(match &dispatch.status {
        Status::Running { saga } => {
            let saga = saga.clone();
            Some((dispatch, saga))
        }
        Status::Delivered { .. } => None,
    })
}

fn cancel(env: &Env, id: &str) -> Result<(), Refusal> {
    let Some((_, saga)) = running(env, id)? else {
        return Ok(());
    };
    guest::emit(saga::PROGRAM, abi::encode(&saga::Op::Cancel { id: saga }));
    Ok(())
}

fn reassign(env: &Env, id: &str, attempt: u32) -> Result<(), Refusal> {
    let Some((_, saga)) = running(env, id)? else {
        return Ok(());
    };
    guest::emit(
        saga::PROGRAM,
        abi::encode(&saga::Op::Reassign { id: saga, attempt }),
    );
    Ok(())
}

fn judge(contract: Contract, outcome: saga::Outcome) -> Result<Vec<u8>, String> {
    let bytes = match outcome {
        saga::Outcome::Done(bytes) => bytes,
        saga::Outcome::Failed(error) => return Err(error),
        saga::Outcome::TimedOut => return Err("timed out".into()),
        saga::Outcome::Cancelled => return Err("cancelled".into()),
    };
    match contract {
        Contract::Bytes => Ok(bytes),
        Contract::Json => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(_) => Ok(bytes),
            Err(error) => Err(format!("the output is not one JSON value: {error}")),
        },
    }
}

fn callback(env: &Env, callback: saga::Callback) -> Result<(), Refusal> {
    let (receiver, id): (ProgramId, String) = abi::decode(&callback.correlation)?;
    let mut dispatch = guest::record::<Dispatch>(dispatch_key(&receiver, &id))?
        .ok_or_else(|| not_found(format!("dispatch {receiver}/{id}")))?;
    let Status::Running { saga } = &dispatch.status else {
        return Err(conflict(format!("dispatch {receiver}/{id} was delivered")));
    };
    let answers_this_saga = *saga == callback.id;
    if !answers_this_saga {
        return Err(conflict(format!(
            "dispatch {receiver}/{id} runs {saga}, not {}",
            callback.id
        )));
    }
    let contract = recipe(&dispatch.recipe)
        .map(|recipe| recipe.manifest.contract)
        .unwrap_or(Contract::Bytes);
    deliver(env, &mut dispatch, judge(contract, callback.outcome));
    Ok(())
}

fn deliver(env: &Env, dispatch: &mut Dispatch, outcome: Result<Vec<u8>, String>) {
    dispatch.status = Status::Delivered {
        outcome: outcome.clone(),
    };
    dispatch.updated_at = env.height;
    guest::put(dispatch_key(&dispatch.receiver, &dispatch.id), dispatch);
    guest::emit(
        dispatch.receiver.clone(),
        abi::encode(&Outcome {
            id: dispatch.id.clone(),
            recipe: dispatch.recipe.clone(),
            outcome,
        }),
    );
}

fn completed(env: &Env, item: &ItemRef, outcome: &abi::Outcome) -> Result<(), Refusal> {
    let (receiver, id): (ProgramId, String) = guest::record(trigger_key(item.item))?
        .ok_or_else(|| not_found(format!("no dispatch awaits item {}", item.item)))?;
    guest::delete(trigger_key(item.item));
    let abi::Outcome::Rejected(refusal) = outcome else {
        return Ok(());
    };
    let mut dispatch = guest::record::<Dispatch>(dispatch_key(&receiver, &id))?
        .ok_or_else(|| not_found(format!("dispatch {receiver}/{id}")))?;
    deliver(
        env,
        &mut dispatch,
        Err(format!("saga refused the trigger: {refusal}")),
    );
    Ok(())
}

guest::program!(Dispatcher);
