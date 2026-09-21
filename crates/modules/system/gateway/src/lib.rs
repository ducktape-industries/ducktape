use abi::{Refusal, Scan};
use guest::Program;
use wire::gateway::{
    Audience, Credential, CredentialKind, Definition, Handle, Op, Query, Reply, Route,
    handle_is_well_formed, label_is_well_formed,
};
use wire::program::{conflict, invalid, not_found, u64_key, unauthorized};
use wire::{AccountNumber, identity};

const HANDLE: &str = "h/";
const NAME: &str = "n/";
const ROUTE: &str = "r/";
const CREDENTIAL: &str = "c/";
const GRANTED: &str = "g/";
const KEY_LEN: usize = 32;

struct Gateway;

fn handle_key(account: AccountNumber) -> Vec<u8> {
    u64_key(HANDLE, account)
}

fn name_key(handle: &str) -> Vec<u8> {
    format!("{NAME}{handle}").into_bytes()
}

fn route_prefix(account: AccountNumber) -> Vec<u8> {
    let mut key = u64_key(ROUTE, account);
    key.push(b'/');
    key
}

fn route_key(account: AccountNumber, name: Option<&str>) -> Vec<u8> {
    let mut key = route_prefix(account);
    key.extend_from_slice(name.unwrap_or_default().as_bytes());
    key
}

fn credential_prefix(account: AccountNumber) -> Vec<u8> {
    let mut key = u64_key(CREDENTIAL, account);
    key.push(b'/');
    key
}

fn credential_key(account: AccountNumber, name: &str) -> Vec<u8> {
    let mut key = credential_prefix(account);
    key.extend_from_slice(name.as_bytes());
    key
}

fn granted_prefix(to: AccountNumber) -> Vec<u8> {
    let mut key = u64_key(GRANTED, to);
    key.push(b'/');
    key
}

fn granted_key(to: AccountNumber, account: AccountNumber, name: &str) -> Vec<u8> {
    let mut key = granted_prefix(to);
    key.extend_from_slice(&account.to_be_bytes());
    key.push(b'/');
    key.extend_from_slice(name.as_bytes());
    key
}

impl Program for Gateway {
    fn execute(payload: &[u8]) -> Result<(), Refusal> {
        let env = guest::env();
        wire::acl::admit(&env)?;
        let signer = wire::program::external(&env)?;
        let account = identity::account_of(&signer)?
            .ok_or_else(|| unauthorized("this key holds no account"))?;
        match abi::decode(payload)? {
            Op::SetHandle { handle } => set_handle(account, handle),
            Op::SetRoute { name, definition } => set_route(account, name, definition),
            Op::SetCredential {
                name,
                kind,
                publisher,
                seal_key,
            } => set_credential(account, name, kind, publisher, seal_key),
            Op::RemoveCredential { name } => remove_credential(account, &name),
            Op::GrantCredential { name, to } => grant(account, &name, to),
            Op::RevokeCredential { name, from } => revoke(account, &name, from),
        }
    }

    fn query(request: &[u8]) -> Result<(), Refusal> {
        let reply = match abi::decode(request)? {
            Query::Resolve { handle } => Reply::Resolved(guest::record(name_key(&handle))?),
            Query::Handle { account } => Reply::Handle(guest::record(handle_key(account))?),
            Query::Handles => Reply::Handles(
                guest::records::<AccountNumber>(Scan::prefix(NAME))?
                    .into_iter()
                    .map(|(key, account)| Handle {
                        handle: String::from_utf8_lossy(&key[NAME.len()..]).into_owned(),
                        account,
                    })
                    .collect(),
            ),
            Query::Route { account, name } => {
                Reply::Route(guest::record(route_key(account, name.as_deref()))?)
            }
            Query::Routes { account } => Reply::Routes(
                guest::records::<Route>(Scan::prefix(route_prefix(account)))?
                    .into_iter()
                    .map(|(_, route)| route)
                    .collect(),
            ),
            Query::Credential { account, name } => {
                Reply::Credential(guest::record(credential_key(account, &name))?)
            }
            Query::Credentials { account } => Reply::Credentials(
                guest::records::<Credential>(Scan::prefix(credential_prefix(account)))?
                    .into_iter()
                    .map(|(_, credential)| credential)
                    .collect(),
            ),
            Query::Granted { to } => {
                let mut credentials = Vec::new();
                for entry in guest::scan(Scan::prefix(granted_prefix(to))) {
                    let rest = &entry.key[granted_prefix(to).len()..];
                    let account = u64::from_be_bytes(
                        rest[..8]
                            .try_into()
                            .map_err(|_| invalid("a grant key names no account"))?,
                    );
                    let name = String::from_utf8_lossy(&rest[9..]).into_owned();
                    credentials.push(credential(account, &name)?);
                }
                Reply::Credentials(credentials)
            }
        };
        guest::reply(&reply);
        Ok(())
    }
}

fn set_handle(account: AccountNumber, handle: Option<String>) -> Result<(), Refusal> {
    if let Some(previous) = guest::record::<String>(handle_key(account))? {
        guest::delete(name_key(&previous));
        guest::delete(handle_key(account));
    }
    let Some(handle) = handle else {
        return Ok(());
    };
    if !handle_is_well_formed(&handle) {
        return Err(invalid(format!("{handle:?} is not a handle")));
    }
    let taken = guest::get(name_key(&handle)).is_some();
    if taken {
        return Err(conflict(format!("{handle} belongs to another account")));
    }
    guest::put(name_key(&handle), &account);
    guest::put(handle_key(account), &handle);
    Ok(())
}

fn set_route(
    account: AccountNumber,
    name: Option<String>,
    definition: Option<Definition>,
) -> Result<(), Refusal> {
    let label_malformed = name
        .as_deref()
        .is_some_and(|label| !label_is_well_formed(label));
    if label_malformed {
        return Err(invalid(format!("{name:?} is not a route label")));
    }
    let Some(definition) = definition else {
        guest::delete(route_key(account, name.as_deref()));
        return Ok(());
    };
    well_formed(&definition)?;
    guest::put(
        route_key(account, name.as_deref()),
        &Route {
            account,
            name,
            definition,
        },
    );
    Ok(())
}

fn well_formed(definition: &Definition) -> Result<(), Refusal> {
    let publisher_is_a_node = definition.publisher.len() == KEY_LEN;
    if !publisher_is_a_node {
        return Err(invalid("a publisher is a 32-byte node key"));
    }
    let methods_sorted = definition
        .policy
        .methods
        .windows(2)
        .all(|pair| pair[0] < pair[1]);
    if !methods_sorted {
        return Err(invalid("methods are listed once each, in order"));
    }
    if let Audience::Accounts(accounts) = &definition.policy.audience {
        let accounts_sorted = accounts.windows(2).all(|pair| pair[0] < pair[1]);
        let accounts_live = accounts.iter().all(|account| *account != 0);
        if !accounts_sorted || !accounts_live {
            return Err(invalid(
                "an audience lists live accounts once each, in order",
            ));
        }
    }
    Ok(())
}

fn credential(account: AccountNumber, name: &str) -> Result<Credential, Refusal> {
    guest::record(credential_key(account, name))?
        .ok_or_else(|| not_found(format!("credential {name} of account {account}")))
}

fn set_credential(
    account: AccountNumber,
    name: String,
    kind: CredentialKind,
    publisher: Vec<u8>,
    seal_key: [u8; 32],
) -> Result<(), Refusal> {
    if name.is_empty() {
        return Err(invalid("a credential has a name"));
    }
    let publisher_is_a_node = publisher.len() == KEY_LEN;
    if !publisher_is_a_node {
        return Err(invalid("a publisher is a 32-byte node key"));
    }
    if let Some(previous) = guest::record::<Credential>(credential_key(account, &name))? {
        for to in previous.grants {
            guest::delete(granted_key(to, account, &name));
        }
    }
    guest::put(
        credential_key(account, &name),
        &Credential {
            account,
            name,
            kind,
            publisher,
            seal_key,
            grants: Vec::new(),
        },
    );
    Ok(())
}

fn remove_credential(account: AccountNumber, name: &str) -> Result<(), Refusal> {
    let credential = credential(account, name)?;
    for to in credential.grants {
        guest::delete(granted_key(to, account, name));
    }
    guest::delete(credential_key(account, name));
    Ok(())
}

fn grant(account: AccountNumber, name: &str, to: AccountNumber) -> Result<(), Refusal> {
    let mut credential = credential(account, name)?;
    if identity::account(to)?.is_none() {
        return Err(not_found(format!("account {to}")));
    }
    credential.grants.retain(|granted| *granted != to);
    credential.grants.push(to);
    credential.grants.sort_unstable();
    guest::put(credential_key(account, name), &credential);
    guest::set(granted_key(to, account, name), Vec::new());
    Ok(())
}

fn revoke(account: AccountNumber, name: &str, from: AccountNumber) -> Result<(), Refusal> {
    let mut credential = credential(account, name)?;
    credential.grants.retain(|granted| *granted != from);
    guest::put(credential_key(account, name), &credential);
    guest::delete(granted_key(from, account, name));
    Ok(())
}

guest::program!(Gateway);
