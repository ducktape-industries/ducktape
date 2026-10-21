// A validators program for the kernel's tests: it speaks the role's ops with no rules. Anyone
// enrolls as a resident, and any one vote enacts a promotion, a demotion or a removal at once,
// sends the halo role its op, or sets which queries the network answers.

pub const VALIDATORS: &[u8] = b"validators";
pub const RESIDENTS: &[u8] = b"residents";
pub const QUERIES: &[u8] = b"queries";

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::role::access::{Need, Resource};
    use abi::role::validators::{
        Genesis, Member, Motion, Op, Query as Ask, QueryPolicy, Reply, Voted,
    };
    use abi::{Env, Origin, Refusal, reason};
    use program::{Execute, Program, Query, Reads};

    use crate::{QUERIES, RESIDENTS, VALIDATORS};

    struct Valset;

    struct Roster {
        validators: Vec<Member>,
        residents: Vec<Member>,
        queries: QueryPolicy,
    }

    impl Roster {
        fn read(ctx: &impl Reads) -> Result<Roster, Refusal> {
            let list = |key| match ctx.get(key) {
                Some(bytes) => abi::decode(&bytes),
                None => Ok(Vec::new()),
            };
            Ok(Roster {
                validators: list(VALIDATORS)?,
                residents: list(RESIDENTS)?,
                queries: policy(ctx)?,
            })
        }

        fn write(&self, ctx: &mut Execute) {
            ctx.set(VALIDATORS, abi::encode(&self.validators));
            ctx.set(RESIDENTS, abi::encode(&self.residents));
            ctx.set(QUERIES, abi::encode(&self.queries));
        }

        fn take(&mut self, key: &[u8]) -> Option<Member> {
            let held = |list: &mut Vec<Member>| {
                let at = list.iter().position(|member| member.key == key)?;
                Some(list.remove(at))
            };
            held(&mut self.validators).or_else(|| held(&mut self.residents))
        }

        fn enroll(&mut self, key: Vec<u8>, address: Option<String>, halo: String) {
            match self.member(&key) {
                Some(member) => {
                    member.address = address;
                    member.halo = halo;
                }
                None => self.residents.push(Member { key, address, halo }),
            }
        }

        fn member(&mut self, key: &[u8]) -> Option<&mut Member> {
            self.validators
                .iter_mut()
                .chain(self.residents.iter_mut())
                .find(|member| member.key == key)
        }

        fn enact(&mut self, motion: Motion) -> Result<Option<Forwarded>, Refusal> {
            match motion {
                Motion::Promote { key } => {
                    let member = self.take(&key).ok_or_else(not_a_member)?;
                    self.validators.push(member);
                }
                Motion::Demote { key } => {
                    let member = self.take(&key).ok_or_else(not_a_member)?;
                    self.residents.push(member);
                }
                Motion::Remove { key } => {
                    self.take(&key).ok_or_else(not_a_member)?;
                }
                Motion::Role { role, op } => return Ok(Some(Forwarded { role, op })),
                Motion::Queries { policy } => self.queries = policy,
                Motion::Door { .. } => {
                    return Err(Refusal::new(
                        reason::UNSUPPORTED,
                        "the fixture valset has no door",
                    ));
                }
            }
            Ok(None)
        }
    }

    fn policy(ctx: &impl Reads) -> Result<QueryPolicy, Refusal> {
        let held = ctx.get(QUERIES).ok_or_else(|| {
            Refusal::new(
                reason::NOT_FOUND,
                "the fixture valset holds no query policy",
            )
        })?;
        abi::decode(&held)
    }

    struct Forwarded {
        role: String,
        op: Vec<u8>,
    }

    fn not_a_member() -> Refusal {
        Refusal::new(reason::NOT_FOUND, "the key is not a member")
    }

    impl Program for Valset {
        fn init(ctx: &mut Execute, _env: &Env, params: &[u8]) -> Result<(), Refusal> {
            let genesis: Genesis = abi::decode(params)?;
            ctx.set(VALIDATORS, abi::encode(&genesis.validators));
            ctx.set(QUERIES, abi::encode(&genesis.queries));
            Ok(())
        }

        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            let Origin::External(signer) = &env.origin else {
                return Err(Refusal::new(reason::UNAUTHORIZED, "a key signs every op"));
            };
            let mut roster = Roster::read(ctx)?;
            match abi::decode(payload)? {
                Op::Enroll { address, halo, .. } => {
                    if halo.is_empty() {
                        return Err(Refusal::new(
                            reason::INVALID_INPUT,
                            "a member names its halo",
                        ));
                    }
                    roster.enroll(signer.clone(), address, halo)
                }
                Op::SetHalo { halo } => {
                    let member = roster.member(signer).ok_or_else(not_a_member)?;
                    member.halo = halo;
                }
                Op::Leave => {
                    roster.take(signer).ok_or_else(not_a_member)?;
                }
                Op::Vote(motion) => {
                    if let Some(Forwarded { role, op }) = roster.enact(motion)? {
                        ctx.emit(env.roles.bound(&role)?.clone(), op, false);
                    }
                    ctx.output(abi::encode(&Voted::Enacted));
                }
            }
            roster.write(ctx);
            Ok(())
        }

        fn access(_ctx: &mut Query, _env: &Env, _payload: &[u8]) -> Result<Vec<Need>, Refusal> {
            Ok(Vec::new())
        }

        fn resource(_ctx: &mut Query, _env: &Env, _key: &str) -> Result<Resource, Refusal> {
            Ok(Resource::default())
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let roster = Roster::read(ctx)?;
            let reply = match abi::decode(request)? {
                Ask::Validators => Reply::Validators(
                    roster
                        .validators
                        .into_iter()
                        .map(|member| member.key)
                        .collect(),
                ),
                Ask::Members => Reply::Members([roster.validators, roster.residents].concat()),
                Ask::QueryPolicy => Reply::QueryPolicy(roster.queries),
            };
            ctx.respond(abi::encode(&reply));
            Ok(())
        }
    }

    program::program!(Valset);
}
