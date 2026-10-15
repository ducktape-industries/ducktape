//! The scheduler role for the kernel's tests: a program schedules a run of another program for a
//! later height, may cancel it before it runs, and the kernel stores each run's receipt on its entry.

#[cfg(target_arch = "wasm32")]
mod program {
    use abi::role::scheduler::{Entry, EntryId, Op, Query as Ask, Reply};
    use abi::{Env, Origin, ProgramId, Receipt, Refusal, Scan, reason};
    use program::{Execute, Program, Query, Reads};

    const NEXT: &[u8] = b"next";

    struct Scheduler;

    fn entry_key(id: EntryId) -> Vec<u8> {
        [b"entry/".as_slice(), &id.to_be_bytes()].concat()
    }

    fn due_prefix(height: u64) -> Vec<u8> {
        [b"due/".as_slice(), &height.to_be_bytes()].concat()
    }

    fn due_key(height: u64, id: EntryId) -> Vec<u8> {
        [due_prefix(height), id.to_be_bytes().to_vec()].concat()
    }

    fn scheduling_program(env: &Env) -> Result<ProgramId, Refusal> {
        let Origin::Program(program) = &env.origin else {
            return Err(Refusal::new(
                reason::UNAUTHORIZED,
                "only a program schedules or cancels a run, as itself",
            ));
        };
        Ok(program.clone())
    }

    fn entry(ctx: &impl Reads, id: EntryId) -> Result<Entry, Refusal> {
        ctx.record(entry_key(id))?
            .ok_or_else(|| Refusal::new(reason::NOT_FOUND, format!("no entry {id}")))
    }

    fn schedule(
        ctx: &mut Execute,
        env: &Env,
        height: u64,
        target: ProgramId,
        payload: Vec<u8>,
    ) -> Result<(), Refusal> {
        let scheduler = scheduling_program(env)?;
        let later = height > env.height;
        if !later {
            return Err(Refusal::new(
                reason::INVALID_INPUT,
                format!(
                    "a run is scheduled for a height after {}, not {height}",
                    env.height
                ),
            ));
        }
        let id: EntryId = ctx.record(NEXT)?.unwrap_or(0);
        let entry = Entry {
            id,
            scheduler,
            height,
            target,
            payload,
            receipt: None,
        };
        ctx.put(NEXT, &(id + 1));
        ctx.put(entry_key(id), &entry);
        ctx.set(due_key(height, id), Vec::new());
        ctx.output(abi::encode(&id));
        Ok(())
    }

    fn cancel(ctx: &mut Execute, env: &Env, id: EntryId) -> Result<(), Refusal> {
        let canceller = scheduling_program(env)?;
        let entry = entry(ctx, id)?;
        let theirs = entry.scheduler == canceller;
        if !theirs {
            return Err(Refusal::new(
                reason::UNAUTHORIZED,
                format!("entry {id} is {}'s to cancel", entry.scheduler),
            ));
        }
        let ran = entry.receipt.is_some();
        if ran {
            return Err(Refusal::new(reason::WRONG_STATE, format!("entry {id} ran")));
        }
        ctx.delete(entry_key(id));
        ctx.delete(due_key(entry.height, id));
        Ok(())
    }

    fn ran(ctx: &mut Execute, env: &Env, id: EntryId, receipt: Receipt) -> Result<(), Refusal> {
        let from_the_kernel = env.origin == Origin::System;
        if !from_the_kernel {
            return Err(Refusal::new(
                reason::UNAUTHORIZED,
                "only the kernel says an entry ran",
            ));
        }
        let mut entry = entry(ctx, id)?;
        let ran = entry.receipt.is_some();
        if ran {
            return Err(Refusal::new(reason::WRONG_STATE, format!("entry {id} ran")));
        }
        ctx.delete(due_key(entry.height, id));
        entry.receipt = Some(receipt);
        ctx.put(entry_key(id), &entry);
        Ok(())
    }

    fn due(
        ctx: &Query,
        height: u64,
        after: Option<EntryId>,
        limit: Option<u32>,
    ) -> Result<Reply, Refusal> {
        let mut scan = Scan::prefix(due_prefix(height));
        if let Some(after) = after {
            scan = scan.after(due_key(height, after));
        }
        if let Some(limit) = limit {
            scan = scan.limit(limit.into());
        }
        let prefix = due_prefix(height).len();
        let mut entries = Vec::new();
        for row in ctx.scan(scan) {
            let id = row.key[prefix..]
                .try_into()
                .map(EntryId::from_be_bytes)
                .map_err(|_| Refusal::new(reason::CORRUPT, "a due key ends in an entry id"))?;
            entries.push(entry(ctx, id)?);
        }
        let full = limit.is_some_and(|limit| entries.len() == limit as usize);
        let next = match full {
            true => entries.last().map(|entry| entry.id),
            false => None,
        };
        Ok(Reply::Due { entries, next })
    }

    impl Program for Scheduler {
        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal> {
            match abi::decode(payload)? {
                Op::Schedule {
                    height,
                    target,
                    payload,
                } => schedule(ctx, env, height, target, payload),
                Op::Cancel { entry } => cancel(ctx, env, entry),
                Op::Ran { entry, receipt } => ran(ctx, env, entry, receipt),
            }
        }

        fn query(ctx: &mut Query, _env: &Env, request: &[u8]) -> Result<(), Refusal> {
            let reply = match abi::decode(request)? {
                Ask::Due {
                    height,
                    after,
                    limit,
                } => due(ctx, height, after, limit)?,
                Ask::Entry { id } => Reply::Entry(ctx.record(entry_key(id))?),
            };
            ctx.respond(abi::encode(&reply));
            Ok(())
        }
    }

    program::program!(Scheduler);
}
