//! An awaits program for the kernel's tests: it opens a ticket for each call a parked frame
//! waits on, and hands the frame its answer when the called program resolves it.

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::role::access::{Need, Resource};
    use abi::role::awaits::{Answer, Call, FrameId, Op, Query as Ask, Reply, Ticket, TicketRow};
    use abi::{Env, Origin, ProgramId, Refusal, Scan, reason};
    use program::{Execute, Program, Query, Reads};

    struct Awaits;

    fn ticket_key(ticket: Ticket) -> Vec<u8> {
        [b"ticket/".as_slice(), &ticket.to_be_bytes()].concat()
    }

    fn frame_prefix(frame: FrameId) -> Vec<u8> {
        [b"frame/".as_slice(), &frame.to_be_bytes()].concat()
    }

    fn frame_key(frame: FrameId, ticket: Ticket) -> Vec<u8> {
        [frame_prefix(frame), ticket.to_be_bytes().to_vec()].concat()
    }

    fn from_the_kernel(env: &Env) -> Result<(), Refusal> {
        let kernel = env.origin == Origin::System;
        match kernel {
            true => Ok(()),
            false => Err(Refusal::new(
                reason::UNAUTHORIZED,
                "only the kernel parks a frame",
            )),
        }
    }

    fn park(
        ctx: &mut Execute,
        env: &Env,
        frame: FrameId,
        program: ProgramId,
        calls: Vec<Call>,
    ) -> Result<(), Refusal> {
        from_the_kernel(env)?;
        for call in calls {
            let row = TicketRow {
                ticket: call.ticket,
                frame,
                caller: program.clone(),
                target: call.target,
                opened: env.height,
            };
            ctx.put(ticket_key(call.ticket), &row);
            ctx.set(frame_key(frame, call.ticket), Vec::new());
        }
        let waits = !ctx.scan(Scan::prefix(frame_prefix(frame))).is_empty();
        match waits {
            true => Ok(()),
            false => Err(Refusal::new(
                reason::WRONG_STATE,
                format!("frame {frame} awaits nothing"),
            )),
        }
    }

    fn close_ticket(ctx: &mut Execute, row: &TicketRow) {
        ctx.delete(ticket_key(row.ticket));
        ctx.delete(frame_key(row.frame, row.ticket));
    }

    fn hand(ctx: &mut Execute, row: TicketRow, answer: Answer) -> Result<(), Refusal> {
        close_ticket(ctx, &row);
        ctx.resume(row.frame, row.ticket, answer)
    }

    fn resolve(
        ctx: &mut Execute,
        env: &Env,
        ticket: Ticket,
        answer: Answer,
    ) -> Result<(), Refusal> {
        let Some(row) = ctx.record::<TicketRow>(ticket_key(ticket))? else {
            return Ok(());
        };
        let by_target = env.origin == Origin::Program(row.target.clone());
        let by_the_kernel = env.origin == Origin::System;
        let answers = by_target || by_the_kernel;
        if !answers {
            return Err(Refusal::new(
                reason::UNAUTHORIZED,
                format!("only {} answers call {ticket}", row.target),
            ));
        }
        hand(ctx, row, answer)
    }

    fn close(ctx: &mut Execute, env: &Env, frame: FrameId) -> Result<(), Refusal> {
        from_the_kernel(env)?;
        let prefix = frame_prefix(frame);
        for entry in ctx.scan(Scan::prefix(&prefix)) {
            let ticket = entry.key[prefix.len()..]
                .try_into()
                .map(u64::from_be_bytes)
                .map_err(|_| Refusal::new(reason::PROTOCOL, "a frame index key"))?;
            ctx.delete(ticket_key(ticket));
            ctx.delete(entry.key);
        }
        Ok(())
    }

    impl Program for Awaits {
        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            match abi::decode(payload)? {
                Op::Park {
                    frame,
                    program,
                    calls,
                } => park(ctx, env, frame, program, calls),
                Op::Resolve { ticket, answer } => resolve(ctx, env, ticket, answer),
                Op::Close { frame } => close(ctx, env, frame),
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
                Ask::Ticket { ticket } => Reply::Ticket(ctx.record(ticket_key(ticket))?),
                Ask::Owed { .. } => {
                    return Err(Refusal::new(
                        reason::UNSUPPORTED,
                        "the fixture lists no tickets",
                    ));
                }
            };
            ctx.respond(abi::encode(&reply));
            Ok(())
        }
    }

    program::program!(Awaits);
}
