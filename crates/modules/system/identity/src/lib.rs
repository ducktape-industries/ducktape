use abi::{Env, Origin, Refusal, Scheme};
use guest::Program;
use modules::AccountNumber;
use modules::identity::{
    Account, Admission, CONSENT_NAMESPACE, Consent, Control, Key, Op, Query, Reference, Reply,
    Standing,
};
use modules::program::{bytes_key, conflict, invalid, not_found, u64_key, unauthorized};

const ACCOUNT: &str = "a/";
const OF_KEY: &str = "k/";
const GENERATION: &str = "g/";
const CONTROLLED: &str = "c/";
const NEXT: &[u8] = b"next";

struct Identity;

fn account_key(number: AccountNumber) -> Vec<u8> {
    u64_key(ACCOUNT, number)
}

fn of_key(key: &[u8]) -> Vec<u8> {
    bytes_key(OF_KEY, key)
}

fn generation_key(key: &[u8]) -> Vec<u8> {
    bytes_key(GENERATION, key)
}

fn controlled_prefix(by: AccountNumber) -> Vec<u8> {
    let mut key = u64_key(CONTROLLED, by);
    key.push(b'/');
    key
}

fn controlled_key(by: AccountNumber, number: AccountNumber) -> Vec<u8> {
    let mut key = controlled_prefix(by);
    key.extend_from_slice(&number.to_be_bytes());
    key
}

impl Program for Identity {
    fn execute(payload: &[u8]) -> Result<(), Refusal> {
        let env = guest::env();
        match abi::decode(payload)? {
            Op::Create { name, scheme } => create(&env, name, scheme),
            Op::AddKey {
                scheme,
                label,
                consent,
            } => add_key(&env, scheme, label, consent),
            Op::RemoveKey { key } => remove_key(&env, &key),
            Op::SetName { account, name } => set_name(&env, account, name),
            Op::SetProfile {
                account,
                avatar,
                bio,
            } => set_profile(&env, account, avatar, bio),
            Op::CreateProgram { name, controller } => create_program(&env, name, controller),
            Op::SetStanding { account, standing } => set_standing(&env, account, standing),
            Op::TransferControl { account, to } => transfer_control(&env, account, to),
            Op::Revoke { account } => revoke(&env, account),
        }
    }

    fn query(request: &[u8]) -> Result<(), Refusal> {
        let reply = match abi::decode(request)? {
            Query::Get { number } => Reply::Account(guest::record(account_key(number))?),
            Query::OfKey { key } => Reply::Number(guest::record(of_key(&key))?),
            Query::Generation { key } => Reply::Generation(generation(&key)?),
            Query::Resolve { references } => Reply::Resolved(
                references
                    .iter()
                    .map(resolve)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Query::List { page } => Reply::Accounts(
                guest::records::<Account>(page.scan(ACCOUNT.as_bytes()))?
                    .into_iter()
                    .map(|(_, account)| account)
                    .collect(),
            ),
            Query::Controlled { by, page } => {
                let keys = guest::scan(page.scan(&controlled_prefix(by)));
                let mut accounts = Vec::new();
                for entry in keys {
                    let number = u64::from_be_bytes(
                        entry.key[entry.key.len() - 8..]
                            .try_into()
                            .map_err(|_| invalid("a controlled key names no account"))?,
                    );
                    accounts.push(account(number)?);
                }
                Reply::Accounts(accounts)
            }
        };
        guest::reply(&reply);
        Ok(())
    }
}

fn account(number: AccountNumber) -> Result<Account, Refusal> {
    guest::record(account_key(number))?.ok_or_else(|| not_found(format!("account {number}")))
}

fn resolve(reference: &Reference) -> Result<Option<AccountNumber>, Refusal> {
    match reference {
        Reference::Account(number) => Ok(guest::get(account_key(*number))
            .is_some()
            .then_some(*number)),
        Reference::Key(key) => guest::record(of_key(key)),
    }
}

fn generation(key: &[u8]) -> Result<u64, Refusal> {
    Ok(guest::record(generation_key(key))?.unwrap_or(0))
}

fn next_number() -> Result<AccountNumber, Refusal> {
    let number: AccountNumber = guest::record(NEXT)?.unwrap_or(1);
    guest::put(NEXT, &(number + 1));
    Ok(number)
}

fn store(account: &Account) {
    guest::put(account_key(account.number), account);
}

fn admit_key(key: &[u8], number: AccountNumber) -> Result<(), Refusal> {
    let held = guest::get(of_key(key)).is_some();
    if held {
        return Err(conflict("this key already belongs to an account"));
    }
    guest::put(of_key(key), &number);
    guest::put(generation_key(key), &(generation(key)? + 1));
    Ok(())
}

fn named(name: String) -> Result<String, Refusal> {
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err(invalid("a name is not empty"));
    }
    Ok(name)
}

fn create(env: &Env, name: String, scheme: Scheme) -> Result<(), Refusal> {
    let signer = modules::program::external(env)?;
    let number = next_number()?;
    admit_key(&signer, number)?;
    store(&Account {
        number,
        name: named(name)?,
        control: Control::Keys(vec![Key {
            scheme,
            key: signer,
            label: None,
            added_at: env.time,
        }]),
        avatar: None,
        bio: None,
        updated_at: env.time,
    });
    guest::output(abi::encode(&number));
    Ok(())
}

fn add_key(
    env: &Env,
    scheme: Scheme,
    label: Option<String>,
    consent: Consent,
) -> Result<(), Refusal> {
    let signer = modules::program::external(env)?;
    let mut account = account(consent.account)?;
    let Control::Keys(keys) = &mut account.control else {
        return Err(conflict("a program account holds no keys"));
    };
    let authorizer = keys
        .iter()
        .find(|key| key.key == consent.key)
        .ok_or_else(|| unauthorized("the consenting key is not on this account"))?;
    let expired = env.time > consent.expires_at;
    if expired {
        return Err(unauthorized("the consent has expired"));
    }
    let admission = Admission {
        network: env.network.clone(),
        scheme,
        key: signer.clone(),
        generation: generation(&signer)?,
        account: consent.account,
        expires_at: consent.expires_at,
    };
    let consented = guest::verify(
        authorizer.scheme,
        consent.key.clone(),
        CONSENT_NAMESPACE,
        admission.preimage(),
        consent.proof,
    )?;
    if !consented {
        return Err(unauthorized("the consent does not verify"));
    }
    admit_key(&signer, consent.account)?;
    keys.push(Key {
        scheme,
        key: signer,
        label,
        added_at: env.time,
    });
    keys.sort_by(|a, b| a.key.cmp(&b.key));
    account.updated_at = env.time;
    store(&account);
    Ok(())
}

fn remove_key(env: &Env, key: &[u8]) -> Result<(), Refusal> {
    let signer = modules::program::external(env)?;
    let mut account = account_of_key(&signer)?;
    let Control::Keys(keys) = &mut account.control else {
        return Err(conflict("a program account holds no keys"));
    };
    let remover_added_at = keys
        .iter()
        .find(|held| held.key == signer)
        .map(|held| held.added_at)
        .ok_or_else(|| unauthorized("the signer is not on this account"))?;
    let removed = keys
        .iter()
        .find(|held| held.key == key)
        .ok_or_else(|| not_found("that key is not on this account"))?;
    let last = keys.len() == 1;
    if last {
        return Err(conflict("an account keeps its last key"));
    }
    let senior = removed.added_at < remover_added_at;
    if senior {
        return Err(unauthorized("a key removes only itself or a junior key"));
    }
    keys.retain(|held| held.key != key);
    guest::delete(of_key(key));
    account.updated_at = env.time;
    store(&account);
    Ok(())
}

fn account_of_key(key: &[u8]) -> Result<Account, Refusal> {
    let number: AccountNumber =
        guest::record(of_key(key))?.ok_or_else(|| unauthorized("this key holds no account"))?;
    account(number)
}

fn acts_for(env: &Env, account: &Account) -> Result<(), Refusal> {
    let acts = match (&env.origin, &account.control) {
        (Origin::External(key), Control::Keys(_)) => account.holds(key),
        (Origin::Program(program), Control::Program { executor, .. }) => program == executor,
        _ => false,
    };
    if !acts {
        return Err(unauthorized(format!(
            "{:?} does not act for account {}",
            env.origin, account.number
        )));
    }
    Ok(())
}

fn set_name(env: &Env, number: AccountNumber, name: String) -> Result<(), Refusal> {
    let mut account = account(number)?;
    acts_for(env, &account)?;
    account.name = named(name)?;
    account.updated_at = env.time;
    store(&account);
    Ok(())
}

fn set_profile(
    env: &Env,
    number: AccountNumber,
    avatar: Option<abi::BlobId>,
    bio: Option<String>,
) -> Result<(), Refusal> {
    let mut account = account(number)?;
    acts_for(env, &account)?;
    account.avatar = avatar;
    account.bio = bio
        .map(|bio| bio.trim().to_owned())
        .filter(|bio| !bio.is_empty());
    account.updated_at = env.time;
    store(&account);
    Ok(())
}

fn create_program(env: &Env, name: String, controller: AccountNumber) -> Result<(), Refusal> {
    let executor = modules::program::program(env)?;
    let controlling = account(controller)?;
    if !controlling.live() {
        return Err(conflict(format!("account {controller} is not live")));
    }
    let number = next_number()?;
    store(&Account {
        number,
        name: named(name)?,
        control: Control::Program {
            executor,
            controller,
            standing: Standing::Active,
        },
        avatar: None,
        bio: None,
        updated_at: env.time,
    });
    guest::set(controlled_key(controller, number), Vec::new());
    guest::output(abi::encode(&number));
    Ok(())
}

fn set_standing(env: &Env, number: AccountNumber, standing: Standing) -> Result<(), Refusal> {
    let program = modules::program::program(env)?;
    let mut account = account(number)?;
    let Control::Program {
        executor,
        standing: current,
        ..
    } = &mut account.control
    else {
        return Err(conflict(format!(
            "account {number} is not a program account"
        )));
    };
    let executes = *executor == program;
    if !executes {
        return Err(unauthorized(format!(
            "{program} does not execute account {number}"
        )));
    }
    *current = standing;
    account.updated_at = env.time;
    store(&account);
    Ok(())
}

fn controller_of(account: &Account) -> Result<AccountNumber, Refusal> {
    match &account.control {
        Control::Program { controller, .. } => Ok(*controller),
        Control::Keys(_) | Control::Revoked { .. } => Err(conflict(format!(
            "account {} is not a live program account",
            account.number
        ))),
    }
}

fn controls(env: &Env, controlled: &Account) -> Result<AccountNumber, Refusal> {
    let controller = controller_of(controlled)?;
    acts_for(env, &account(controller)?)?;
    Ok(controller)
}

fn transfer_control(env: &Env, number: AccountNumber, to: AccountNumber) -> Result<(), Refusal> {
    let mut account = account(number)?;
    let controller = controls(env, &account)?;
    let target = self::account(to)?;
    if !target.live() {
        return Err(conflict(format!("account {to} is not live")));
    }
    let circular = to == number || ancestry(to)?.contains(&number);
    if circular {
        return Err(conflict(format!(
            "account {to} is controlled through account {number}"
        )));
    }
    let Control::Program {
        controller: current,
        ..
    } = &mut account.control
    else {
        return Err(conflict(format!(
            "account {number} is not a program account"
        )));
    };
    *current = to;
    guest::delete(controlled_key(controller, number));
    guest::set(controlled_key(to, number), Vec::new());
    account.updated_at = env.time;
    store(&account);
    Ok(())
}

fn ancestry(mut number: AccountNumber) -> Result<Vec<AccountNumber>, Refusal> {
    let mut ancestors = Vec::new();
    loop {
        let Control::Program { controller, .. } = account(number)?.control else {
            return Ok(ancestors);
        };
        ancestors.push(controller);
        number = controller;
    }
}

fn revoke(env: &Env, number: AccountNumber) -> Result<(), Refusal> {
    let mut account = account(number)?;
    let controller = controls(env, &account)?;
    account.control = Control::Revoked { controller };
    account.updated_at = env.time;
    store(&account);
    Ok(())
}

guest::program!(Identity);
