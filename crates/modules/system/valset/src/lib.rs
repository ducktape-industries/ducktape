use abi::{Refusal, Scan};
use guest::Program;
use modules::AUTHORITY;
use modules::program::{bytes_key, conflict, invalid};
use modules::valset::{Genesis, Membership, Op, Query, Reply, Standing};

const MEMBER: &str = "m/";
const KEY_LEN: usize = 32;

struct Valset;

fn key(member: &[u8]) -> Vec<u8> {
    bytes_key(MEMBER, member)
}

impl Program for Valset {
    fn init(params: &[u8]) -> Result<(), Refusal> {
        let genesis: Genesis = abi::decode(params)?;
        for member in genesis.validators {
            let membership = Membership {
                key: member.key,
                address: member.address,
                standing: Standing::Validator,
            };
            set(membership)?;
        }
        Ok(())
    }

    fn execute(payload: &[u8]) -> Result<(), Refusal> {
        let env = guest::env();
        modules::program::from(&env, AUTHORITY)?;
        match abi::decode(payload)? {
            Op::Set(membership) => set(membership),
            Op::Remove { key } => remove(&key),
        }
    }

    fn query(request: &[u8]) -> Result<(), Refusal> {
        let reply = match abi::decode(request)? {
            Query::Validators => Reply::Validators(
                memberships()?
                    .into_iter()
                    .filter(|membership| membership.standing == Standing::Validator)
                    .map(|membership| membership.key)
                    .collect(),
            ),
            Query::Members => {
                Reply::Members(memberships()?.iter().map(Membership::member).collect())
            }
            Query::Memberships => Reply::Memberships(memberships()?),
            Query::Membership { key: member } => Reply::Membership(guest::record(key(&member))?),
        };
        guest::reply(&reply);
        Ok(())
    }
}

fn memberships() -> Result<Vec<Membership>, Refusal> {
    Ok(guest::records::<Membership>(Scan::prefix(MEMBER))?
        .into_iter()
        .map(|(_, membership)| membership)
        .collect())
}

fn set(membership: Membership) -> Result<(), Refusal> {
    let key_is_ed25519 = membership.key.len() == KEY_LEN;
    if !key_is_ed25519 {
        return Err(invalid("a member key is a 32-byte ed25519 public key"));
    }
    let demotes = membership.standing == Standing::Resident;
    if demotes {
        unseat(&membership.key)?;
    }
    guest::put(key(&membership.key), &membership);
    Ok(())
}

fn remove(member: &[u8]) -> Result<(), Refusal> {
    unseat(member)?;
    guest::delete(key(member));
    Ok(())
}

fn unseat(member: &[u8]) -> Result<(), Refusal> {
    let seated = guest::record::<Membership>(key(member))?
        .is_some_and(|membership| membership.standing == Standing::Validator);
    if !seated {
        return Ok(());
    }
    let other_validators = memberships()?
        .iter()
        .any(|membership| membership.standing == Standing::Validator && membership.key != member);
    if !other_validators {
        return Err(conflict("the last validator cannot be unseated"));
    }
    Ok(())
}

guest::program!(Valset);
