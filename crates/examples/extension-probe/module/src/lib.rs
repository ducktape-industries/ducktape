//! A separately built application policy. The first network account manages
//! this example's membership; that rule belongs to this guest, never the host.
use serde::{Deserialize, Serialize};

wit_bindgen::generate!({ world: "module", path: "../../../module-sdk/wit" });
use ducktape::module::host;

#[derive(Default, Serialize, Deserialize)]
struct State {
    members: Vec<u64>,
    count: u64,
    last: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Configure { members: Vec<u64> },
    Record { text: String },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Query {
    Authorize { account: u64, text: String },
    State,
}

fn refused(reason: impl ToString) -> host::Error {
    host::Error::Rejected(reason.to_string())
}

fn load() -> Result<State, host::Error> {
    host::state_get(b"state")
        .map(|bytes| serde_json::from_slice(&bytes).map_err(refused))
        .unwrap_or_else(|| Ok(State::default()))
}

fn authorized(state: &State, account: u64, text: &str) -> bool {
    let member = state.members.contains(&account);
    let bounded = !text.is_empty() && text.len() <= 128;
    let application_rule = match cfg!(feature = "replacement") {
        true => text.starts_with('#'),
        false => true,
    };
    member && bounded && application_rule
}

fn caller() -> Result<u64, host::Error> {
    let host::Origin::External(key) = host::get_env().origin else {
        return Err(refused("a user signature is required"));
    };
    let query =
        serde_json::to_vec(&serde_json::json!({"of_key": {"key": key}})).map_err(refused)?;
    let answer = host::query_module("identity", &query)?;
    let answer: serde_json::Value = serde_json::from_slice(&answer).map_err(refused)?;
    answer["account"]["number"]
        .as_u64()
        .ok_or_else(|| refused("caller has no account"))
}

fn decide(state: State, account: u64, operation: Operation) -> Result<State, host::Error> {
    match operation {
        Operation::Configure { members } => configure(state, account, members),
        Operation::Record { text } => record(state, account, text),
    }
}

fn configure(mut state: State, account: u64, members: Vec<u64>) -> Result<State, host::Error> {
    let manages_example = account == 1;
    let valid = members.len() <= 16 && members.iter().all(|member| *member > 0);
    let permitted = manages_example && valid;
    if !permitted {
        return Err(refused("membership configuration refused"));
    }
    state.members = members;
    Ok(state)
}

fn record(mut state: State, account: u64, text: String) -> Result<State, host::Error> {
    let permitted = authorized(&state, account, &text);
    if !permitted {
        return Err(refused("application policy refused"));
    }
    state.count = state
        .count
        .checked_add(1)
        .ok_or_else(|| refused("counter full"))?;
    state.last = text;
    Ok(state)
}

struct Component;

impl Guest for Component {
    fn shape() -> host::ModuleShape {
        host::ModuleShape {
            backing: host::Backing::Map,
            config: Vec::new(),
            committed_queries: true,
        }
    }

    fn initialize(_parameters: Vec<u8>) -> Result<(), host::Error> {
        Ok(())
    }

    fn finalize_block() -> Result<(), host::Error> {
        Ok(())
    }

    fn pending_items() -> Result<Vec<host::PendingItem>, host::Error> {
        Ok(Vec::new())
    }

    fn acknowledge(_ack: host::Ack) -> Result<(), host::Error> {
        Err(refused("no pending work"))
    }

    fn execute(payload: Vec<u8>) -> Result<(), host::Error> {
        let operation: Operation = serde_json::from_slice(&payload).map_err(refused)?;
        let account = caller()?;
        let state = decide(load()?, account, operation)?;
        host::state_set(b"state", &serde_json::to_vec(&state).map_err(refused)?);
        host::emit_event("changed", &[]);
        Ok(())
    }

    fn query(request: Vec<u8>) -> Result<Vec<u8>, host::Error> {
        let query: Query = serde_json::from_slice(&request).map_err(refused)?;
        let state = load()?;
        match query {
            Query::Authorize { account, text } => {
                serde_json::to_vec(&authorized(&state, account, &text)).map_err(refused)
            }
            Query::State => serde_json::to_vec(&state).map_err(refused),
        }
    }
}

export!(Component);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership_and_message_policy_are_owned_by_the_deployed_code() {
        let state = State {
            members: vec![1, 9],
            ..Default::default()
        };
        assert!(authorized(&state, 9, "#hello"));
        assert!(!authorized(&state, 8, "#hello"));
        assert!(!authorized(&state, 9, ""));
        assert!(!authorized(&state, 9, &"#".repeat(129)));
        assert_eq!(
            authorized(&state, 9, "hello"),
            !cfg!(feature = "replacement")
        );
    }
}
