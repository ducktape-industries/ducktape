// A halo program for the kernel's tests: the lane table, changed only by a message from the
// program bound to the validators role. Ids go up from 0 and are never handed out again.

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::role::halo::{Class, Genesis, Lane, Op, Query as Ask, Reply};
    use abi::{Env, Origin, Refusal, Scan, reason};
    use program::{Execute, Program, Query, Reads};

    const LANES: &[u8] = b"l/";
    const NEXT: &[u8] = b"next";

    struct Halo;

    fn lane_key(name: &str) -> Vec<u8> {
        [LANES, name.as_bytes()].concat()
    }

    fn lane(ctx: &impl Reads, name: &str) -> Result<Option<Lane>, Refusal> {
        ctx.record(lane_key(name))
    }

    fn listed(ctx: &impl Reads, name: &str) -> Result<Lane, Refusal> {
        lane(ctx, name)?.ok_or_else(|| Refusal::new(reason::NOT_FOUND, "no such lane"))
    }

    fn open(ctx: &mut Execute, name: String, cap: Class, since: u64) -> Result<(), Refusal> {
        if lane(ctx, &name)?.is_some() {
            return Err(Refusal::new(reason::ALREADY_EXISTS, "the lane is listed"));
        }
        let next: u16 = ctx.record(NEXT)?.unwrap_or(0);
        let Ok(id) = u8::try_from(next) else {
            return Err(Refusal::new(
                reason::CAPACITY,
                "every lane id was handed out",
            ));
        };
        let lane = Lane {
            name,
            id,
            cap,
            enabled: true,
            since,
        };
        ctx.put(lane_key(&lane.name), &lane);
        ctx.put(NEXT, &(next + 1));
        Ok(())
    }

    fn set(ctx: &mut Execute, name: String, cap: Class, enabled: bool) -> Result<(), Refusal> {
        let set = Lane {
            cap,
            enabled,
            ..listed(ctx, &name)?
        };
        ctx.put(lane_key(&name), &set);
        Ok(())
    }

    fn close(ctx: &mut Execute, name: String) -> Result<(), Refusal> {
        listed(ctx, &name)?;
        ctx.delete(lane_key(&name));
        Ok(())
    }

    fn lanes(ctx: &Query) -> Result<Vec<Lane>, Refusal> {
        let mut lanes: Vec<Lane> = ctx
            .records(Scan::prefix(LANES))?
            .into_iter()
            .map(|(_, lane)| lane)
            .collect();
        lanes.sort_by_key(|lane| lane.id);
        Ok(lanes)
    }

    impl Program for Halo {
        fn init(ctx: &mut Execute, _env: &Env, params: &[u8]) -> Result<(), Refusal> {
            let genesis: Genesis = abi::decode(params)?;
            for (name, cap) in genesis.lanes {
                open(ctx, name, cap, 0)?;
            }
            Ok(())
        }

        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            let voted = env.origin == Origin::Program(env.roles.validators.clone());
            if !voted {
                return Err(Refusal::new(
                    reason::UNAUTHORIZED,
                    "only the validators' vote changes the lanes",
                ));
            }
            match abi::decode(payload)? {
                Op::Open { name, cap } => open(ctx, name, cap, env.height),
                Op::Set { name, cap, enabled } => set(ctx, name, cap, enabled),
                Op::Close { name } => close(ctx, name),
            }
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let reply = match abi::decode(request)? {
                Ask::Lanes => Reply::Lanes(lanes(ctx)?),
                Ask::Lane { name } => Reply::Lane(lane(ctx, &name)?),
            };
            ctx.respond(abi::encode(&reply));
            Ok(())
        }
    }

    program::program!(Halo);
}
