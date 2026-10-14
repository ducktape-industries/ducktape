// An awaits program for the kernel's tests: it opens a ticket for each call a parked frame
// waits on, and hands the frame its answer when the called program resolves it or it expires.

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::role::awaits::{
        Answer, Call, EXPIRY_BLOCKS, FrameId, Op, Query as Ask, Reply, Ticket, TicketRow, expired,
    };
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
                deadline: env.height + EXPIRY_BLOCKS,
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
        if !by_target {
            return Err(Refusal::new(
                reason::UNAUTHORIZED,
                format!("only {} answers call {ticket}", row.target),
            ));
        }
        hand(ctx, row, answer)
    }

    fn expire(ctx: &mut Execute, env: &Env, ticket: Ticket) -> Result<(), Refusal> {
        let Some(row) = ctx.record::<TicketRow>(ticket_key(ticket))? else {
            return Err(Refusal::new(
                reason::NOT_FOUND,
                format!("call {ticket} is not open"),
            ));
        };
        let due = env.height >= row.deadline;
        if !due {
            return Err(Refusal::new(
                reason::NOT_YET,
                format!("call {ticket} expires at height {}", row.deadline),
            ));
        }
        hand(ctx, row, Err(expired(ticket, env.height)))
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
                Op::Expire { ticket } => expire(ctx, env, ticket),
                Op::Close { frame } => close(ctx, env, frame),
            }
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let reply = match abi::decode(request)? {
                Ask::Ticket { ticket } => Reply::Ticket(ctx.record(ticket_key(ticket))?),
                Ask::Owed { .. } | Ask::Overdue { .. } => {
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
