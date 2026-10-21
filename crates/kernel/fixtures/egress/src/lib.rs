//! An egress program for the node's tests: a key's `Serve` records what it serves, a request goes to
//! the first node serving its service, which answers it whole or streams it, and from its expiry
//! height (`made` plus its wait) anyone's `Expire` drops it and keeps a failed answer.
pub const NODE: &[u8] = b"node/";
pub const REQUEST: &[u8] = b"request/";
pub const ANSWER: &[u8] = b"answer/";
pub const NEXT: &[u8] = b"next";
pub const EVENT: &[u8] = b"event/";
pub const END: &[u8] = b"end/";

pub fn node_key(key: &[u8]) -> Vec<u8> {
    [NODE, key].concat()
}

pub fn request_key(id: u64) -> Vec<u8> {
    [REQUEST, &id.to_be_bytes()].concat()
}

pub fn answer_key(id: u64) -> Vec<u8> {
    [ANSWER, &id.to_be_bytes()].concat()
}

pub fn event_key(id: u64, seq: u64) -> Vec<u8> {
    [EVENT, &id.to_be_bytes(), &seq.to_be_bytes()].concat()
}

pub fn end_key(id: u64) -> Vec<u8> {
    [END, &id.to_be_bytes()].concat()
}

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::role::access::{Need, Resource};
    use abi::role::egress::{
        Answer, DEFAULT_WAIT, Delivery, Event, NodeRow, Op, Query as Ask, Reply, Request,
        RequestRow, StreamEnd,
    };
    use abi::{Env, Origin, Refusal, Scan, reason};
    use program::{Execute, Program, Query, Reads};

    use crate::{NEXT, NODE, REQUEST, answer_key, end_key, event_key, node_key, request_key};

    struct Egress;

    fn signer(env: &Env) -> Result<&[u8], Refusal> {
        match &env.origin {
            Origin::External(key) => Ok(key),
            Origin::Program(_) | Origin::System => {
                Err(Refusal::new(reason::UNAUTHORIZED, "a node signs this op"))
            }
        }
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
        wait: Option<u64>,
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
            expiry: env.height + wait.unwrap_or(DEFAULT_WAIT),
        };
        ctx.put(request_key(id), &row);
        ctx.put(NEXT, &(id + 1));
        ctx.output(abi::encode(&id));
        Ok(())
    }

    fn open(ctx: &mut Execute, id: u64) -> Result<RequestRow, Refusal> {
        let Some(row) = ctx.record::<RequestRow>(request_key(id))? else {
            return Err(Refusal::new(reason::NOT_FOUND, format!("no request {id}")));
        };
        Ok(row)
    }

    fn owned(ctx: &mut Execute, env: &Env, id: u64) -> Result<(), Refusal> {
        let row = open(ctx, id)?;
        if signer(env)? != row.node.as_slice() {
            return Err(Refusal::new(
                reason::UNAUTHORIZED,
                format!("request {id} is another node's"),
            ));
        }
        Ok(())
    }

    fn answer(ctx: &mut Execute, env: &Env, id: u64, answer: Answer) -> Result<(), Refusal> {
        owned(ctx, env, id)?;
        ctx.delete(request_key(id));
        ctx.put(answer_key(id), &answer);
        Ok(())
    }

    fn streamed(ctx: &mut Execute, env: &Env, events: Vec<Event>) -> Result<(), Refusal> {
        for event in events {
            owned(ctx, env, event.id)?;
            ctx.put(event_key(event.id, event.seq), &event.data);
        }
        Ok(())
    }

    fn ended(
        ctx: &mut Execute,
        env: &Env,
        id: u64,
        events: u64,
        end: StreamEnd,
    ) -> Result<(), Refusal> {
        owned(ctx, env, id)?;
        ctx.delete(request_key(id));
        ctx.put(end_key(id), &(events, end));
        Ok(())
    }

    fn expire(ctx: &mut Execute, env: &Env, id: u64) -> Result<(), Refusal> {
        let row = open(ctx, id)?;
        if env.height < row.expiry {
            return Err(Refusal::new(
                reason::INVALID_INPUT,
                format!("request {id} expires at {}", row.expiry),
            ));
        }
        ctx.delete(request_key(id));
        ctx.put(answer_key(id), &Answer::Failed("expired".to_owned()));
        Ok(())
    }

    impl Program for Egress {
        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            match abi::decode(payload)? {
                Op::Serve { services } => serve(ctx, env, services),
                Op::Request {
                    request: asked,
                    delivery,
                    wait,
                } => request(ctx, env, asked, delivery, wait),
                Op::Answer { id, answer: given } => answer(ctx, env, id, given),
                Op::Events(events) => streamed(ctx, env, events),
                Op::EndStream { id, events, end } => ended(ctx, env, id, events, end),
                Op::Expire { id } => expire(ctx, env, id),
                Op::Register { .. } | Op::Assign { .. } | Op::Unregister { .. } => {
                    Err(Refusal::new(
                        reason::UNSUPPORTED,
                        "this fixture serves, requests, answers, streams and expires",
                    ))
                }
            }
        }

        fn access(_ctx: &mut Query, _env: &Env, _payload: &[u8]) -> Result<Vec<Need>, Refusal> {
            Ok(Vec::new())
        }

        fn resource(_ctx: &mut Query, _env: &Env, _key: &str) -> Result<Resource, Refusal> {
            Ok(Resource::default())
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
