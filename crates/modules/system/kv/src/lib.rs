use abi::Refusal;
use guest::Program;
use wire::kv::{Op, Query, Reply};

struct Kv;

impl Program for Kv {
    fn execute(payload: &[u8]) -> Result<(), Refusal> {
        let env = guest::env();
        wire::acl::admit(&env)?;
        match abi::decode(payload)? {
            Op::Set { key, value } => guest::set(key, value),
            Op::Delete { key } => guest::delete(key),
        }
        Ok(())
    }

    fn query(request: &[u8]) -> Result<(), Refusal> {
        let reply = match abi::decode(request)? {
            Query::Get { key } => Reply::Value(guest::get(key)),
            Query::List { prefix, page } => Reply::Entries(guest::scan(page.scan(&prefix))),
        };
        guest::reply(&reply);
        Ok(())
    }
}

guest::program!(Kv);
