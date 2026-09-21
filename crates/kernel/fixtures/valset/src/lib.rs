use abi::valset::Member;
use borsh::{BorshDeserialize, BorshSerialize};

pub const KEY: &[u8] = b"seating";

#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Seating {
    pub validators: Vec<Member>,
    pub residents: Vec<Member>,
}

impl Seating {
    pub fn seat(&mut self, validators: Vec<Member>) {
        self.residents
            .retain(|resident| !validators.iter().any(|v| v.key == resident.key));
        self.validators = validators;
    }

    pub fn admit(&mut self, member: Member) {
        let held = self
            .validators
            .iter_mut()
            .chain(self.residents.iter_mut())
            .find(|held| held.key == member.key);
        match held {
            Some(held) => held.address = member.address,
            None => self.residents.push(member),
        }
    }

    pub fn members(&self) -> Vec<Member> {
        self.validators
            .iter()
            .chain(self.residents.iter())
            .cloned()
            .collect()
    }
}

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::{Cause, Env, Refusal, reason, valset};
    use guest::{Execute, Program, Query, Reads};

    use crate::{KEY, Seating};

    struct Valset;

    fn seating(ctx: &impl Reads) -> Result<Seating, Refusal> {
        match ctx.get(KEY) {
            Some(bytes) => abi::decode(&bytes),
            None => Ok(Seating::default()),
        }
    }

    impl Program for Valset {
        fn init(ctx: &mut Execute, _env: &Env, params: &[u8]) -> Result<(), Refusal> {
            let genesis: valset::Genesis = abi::decode(params)?;
            let mut seating = Seating::default();
            seating.seat(genesis.validators);
            ctx.set(KEY, abi::encode(&seating));
            Ok(())
        }

        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            let mut seating = seating(ctx)?;
            match &env.cause {
                Cause::Direct => seating.seat(abi::decode(payload)?),
                Cause::Delivery(_) => seating.admit(abi::decode(payload)?),
                Cause::Completion { .. } => {
                    return Err(Refusal::new(reason::UNSUPPORTED, "valset sends nothing"));
                }
            }
            ctx.set(KEY, abi::encode(&seating));
            Ok(())
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let seating = seating(ctx)?;
            let reply = match abi::decode(request)? {
                valset::Query::Validators => valset::Reply::Validators(
                    seating
                        .validators
                        .into_iter()
                        .map(|member| member.key)
                        .collect(),
                ),
                valset::Query::Members => valset::Reply::Members(seating.members()),
            };
            ctx.respond(abi::encode(&reply));
            Ok(())
        }
    }

    guest::program!(Valset);
}
