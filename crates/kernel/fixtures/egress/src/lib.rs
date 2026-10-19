//! An egress program for the node's tests: a key's `Serve` records what it serves, a request goes to
//! the first node serving it, whose `Answer` is kept under `answer_key` (a stream's events and end
//! under `event_key` and `end_key`); `Register` makes its sender the owner of `lend/<service>`.
pub const NODE: &[u8] = b"node/";
pub const OWNER: &[u8] = b"owner/";
pub const EVENT: &[u8] = b"event/";
pub const END: &[u8] = b"end/";
pub const REQUEST: &[u8] = b"request/";
pub const ANSWER: &[u8] = b"answer/";
pub const NEXT: &[u8] = b"next";

pub fn node_key(key: &[u8]) -> Vec<u8> {
    [NODE, key].concat()
}

pub fn request_key(id: u64) -> Vec<u8> {
    [REQUEST, &id.to_be_bytes()].concat()
}

pub fn answer_key(id: u64) -> Vec<u8> {
    [ANSWER, &id.to_be_bytes()].concat()
}

pub fn owner_key(service: &str) -> Vec<u8> {
    [OWNER, service.as_bytes()].concat()
}

pub fn events_of(id: u64) -> Vec<u8> {
    [EVENT, &id.to_be_bytes()].concat()
}

pub fn event_key(id: u64, seq: u64) -> Vec<u8> {
    [events_of(id).as_slice(), &seq.to_be_bytes()].concat()
}

pub fn end_key(id: u64) -> Vec<u8> {
    [END, &id.to_be_bytes()].concat()
}

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::role::access::{Need, Ownership, Party, Resource};
    use abi::role::egress::{
        Answer, Delivery, Event, NodeRow, Op, Query as Ask, Reply, Request, RequestRow, StreamEnd,
    };
    use abi::{Env, Origin, Principal, Refusal, Scan, reason};
    use program::{Execute, Program, Query, Reads};

    use crate::{
        NEXT, NODE, REQUEST, answer_key, end_key, event_key, node_key, owner_key, request_key,
    };

    struct Egress;

    fn signer(env: &Env) -> Result<&[u8], Refusal> {
        match &env.origin {
            Origin::External(key) => Ok(key),
            Origin::Program(_) | Origin::System => {
                Err(Refusal::new(reason::UNAUTHORIZED, "a node signs this op"))
            }
        }
    }

    fn register(ctx: &mut Execute, env: &Env, service: String) -> Result<(), Refusal> {
        let Some(Principal::Account(owner)) = env.sender else {
            return Err(Refusal::new(
                reason::UNAUTHORIZED,
                "an account registers a service",
            ));
        };
        ctx.put(owner_key(&service), &owner);
        Ok(())
    }

    fn elected(ctx: &mut Execute, env: &Env, id: u64) -> Result<(), Refusal> {
        let Some(row) = ctx.record::<RequestRow>(request_key(id))? else {
            return Err(Refusal::new(reason::NOT_FOUND, format!("no request {id}")));
        };
        if signer(env)? != row.node.as_slice() {
            return Err(Refusal::new(
                reason::UNAUTHORIZED,
                format!("request {id} is another node's"),
            ));
        }
        Ok(())
    }

    fn events(ctx: &mut Execute, env: &Env, events: Vec<Event>) -> Result<(), Refusal> {
        for event in events {
            elected(ctx, env, event.id)?;
            ctx.put(event_key(event.id, event.seq), &event.data);
        }
        Ok(())
    }

    fn end_stream(ctx: &mut Execute, env: &Env, id: u64, end: StreamEnd) -> Result<(), Refusal> {
        elected(ctx, env, id)?;
        ctx.delete(request_key(id));
        ctx.put(end_key(id), &end);
        Ok(())
    }

    fn serve(ctx: &mut Execute, env: &Env, services: Vec<String>) -> Result<(), Refusal> {
        let key = signer(env)?.to_vec();
        let row = NodeRow {
            key: key.clone(),
            services,
            since: env.height,
        };
        ctx.put(node_key(&key), &row);
        Ok(())
    }

    fn request(
        ctx: &mut Execute,
        env: &Env,
        request: Request,
        delivery: Delivery,
    ) -> Result<(), Refusal> {
        let service = request.service().to_owned();
        let nodes: Vec<(Vec<u8>, NodeRow)> = ctx.records(Scan::prefix(NODE))?;
        let Some((_, serving)) = nodes
            .into_iter()
            .find(|(_, row)| row.services.contains(&service))
        else {
            return Err(Refusal::new(
                reason::NOT_FOUND,
                format!("no node serves {service}"),
            ));
        };
        let id: u64 = ctx.record(NEXT)?.unwrap_or(0);
        let requester = match &env.origin {
            Origin::Program(program) => program.clone(),
            Origin::External(_) | Origin::System => env.me.clone(),
        };
        let row = RequestRow {
            id,
            requester,
            request,
            delivery,
            node: serving.key,
            made: env.height,
            expiry: 0,
        };
        ctx.put(request_key(id), &row);
        ctx.put(NEXT, &(id + 1));
        ctx.output(abi::encode(&id));
        Ok(())
    }

    fn answer(ctx: &mut Execute, env: &Env, id: u64, answer: Answer) -> Result<(), Refusal> {
        let Some(row) = ctx.record::<RequestRow>(request_key(id))? else {
            return Err(Refusal::new(reason::NOT_FOUND, format!("no request {id}")));
        };
        if signer(env)? != row.node.as_slice() {
            return Err(Refusal::new(
                reason::UNAUTHORIZED,
                format!("request {id} is another node's"),
            ));
        }
        ctx.delete(request_key(id));
        ctx.put(answer_key(id), &answer);
        Ok(())
    }

    impl Program for Egress {
        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            match abi::decode(payload)? {
                Op::Serve { services } => serve(ctx, env, services),
                Op::Request {
                    request: asked,
                    delivery,
                    ..
                } => request(ctx, env, asked, delivery),
                Op::Answer { id, answer: given } => answer(ctx, env, id, given),
                Op::Register { service } => register(ctx, env, service),
                Op::Events(given) => events(ctx, env, given),
                Op::EndStream { id, end, .. } => end_stream(ctx, env, id, end),
                Op::Assign { .. } | Op::Expire { .. } => Err(Refusal::new(
                    reason::UNSUPPORTED,
                    "this fixture neither assigns nor expires",
                )),
            }
        }

        fn access(_ctx: &mut Query, _env: &Env, _payload: &[u8]) -> Result<Vec<Need>, Refusal> {
            Ok(Vec::new())
        }

        fn resource(ctx: &mut Query, _env: &Env, key: &str) -> Result<Resource, Refusal> {
            let Some(service) = key.strip_prefix("lend/") else {
                return Ok(Resource::default());
            };
            let owner: Option<u64> = ctx.record(owner_key(service))?;
            Ok(Resource {
                owner: owner.map(Party::Account),
                default: Some(Ownership::Owner),
            })
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let reply = match abi::decode(request)? {
                Ask::Assigned { key } => {
                    let rows: Vec<(Vec<u8>, RequestRow)> = ctx.records(Scan::prefix(REQUEST))?;
                    let assigned = rows
                        .into_iter()
                        .map(|(_, row)| row)
                        .filter(|row| row.node == key)
                        .collect();
                    Reply::Requests(assigned)
                }
                Ask::Node { key } => Reply::Node(ctx.record(node_key(&key))?),
                Ask::Serving { service } => {
                    let nodes: Vec<(Vec<u8>, NodeRow)> = ctx.records(Scan::prefix(NODE))?;
                    let serving = nodes
                        .into_iter()
                        .map(|(_, row)| row)
                        .filter(|row| row.services.contains(&service))
                        .map(|row| row.key)
                        .collect();
                    Reply::Serving(serving)
                }
            };
            ctx.reply(&reply);
            Ok(())
        }
    }

    program::program!(Egress);
}
