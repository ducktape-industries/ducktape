//! Frames that await: the host follows a run that parked on calls, keeps the frame in `$parked`
//! when no answer is ready, and resumes it when the awaits role hands an answer back.
use std::collections::VecDeque;

use abi::role::awaits::{self, Answer, Call, FrameId, Ticket};
use abi::{Cause, Env, Origin, Outcome, Principal, ProgramCall, Receipt, Refusal, reason, role};
use blob::Stage;
use commonware_runtime::Spawner;
use commonware_storage::Context;
use runtime::{Image, OUT_OF_FUEL, Suspended};
use state::{Checkpoint, Overlay};

use crate::Source;
use crate::parked::{self, Parked, Waiter};
use crate::unit::{self, Called, Frame, Resumed, refusal_of};
use crate::{MAX_DEPTH, Result, rejected};

pub const PAGE_FUEL: u64 = 4_096;

pub enum Status {
    Finished,
    Deferred,
    Parked,
}

pub struct Run {
    pub receipt: Receipt,
    pub status: Status,
}

impl Run {
    pub fn finished(receipt: Receipt) -> Run {
        Run {
            receipt,
            status: Status::Finished,
        }
    }
}

pub struct Segment {
    pub receipt: Receipt,
    pub calls: Vec<Called>,
    pub deferred: bool,
    pub parked: Option<Suspended>,
}

impl Segment {
    pub fn finished(receipt: Receipt) -> Segment {
        Segment {
            receipt,
            calls: Vec::new(),
            deferred: false,
            parked: None,
        }
    }
}

pub struct Start {
    pub frame: Option<FrameId>,
    pub waiter: Waiter,
    pub checkpoint: Checkpoint,
}

#[derive(Default)]
struct Placed {
    ready: VecDeque<(Ticket, Answer)>,
    open: Vec<Call>,
}

fn answer_of(outcome: Outcome) -> Answer {
    match outcome {
        Outcome::Applied { output } => Ok(output),
        Outcome::Rejected(refusal) => Err(refusal),
    }
}

fn charge(frame: &mut Frame, image: &Image) -> std::result::Result<(), Refusal> {
    let cost = image.pages.len() as u64 * PAGE_FUEL;
    let Some(left) = frame.fuel else {
        return Ok(());
    };
    let affordable = left > cost;
    frame.fuel = Some(left.saturating_sub(cost));
    match affordable {
        true => Ok(()),
        false => Err(Refusal::new(reason::TRAP, OUT_OF_FUEL)),
    }
}

impl<'a, E> Source<'a, E>
where
    E: Context + Spawner,
{
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn follow(
        &self,
        program: &str,
        env: Env,
        start: Start,
        segment: Segment,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
        depth: u32,
    ) -> Result<Run> {
        let Segment {
            mut receipt,
            mut calls,
            mut deferred,
            parked: mut suspended,
        } = segment;
        let mut placed = Placed::default();
        loop {
            let Some(instance) = suspended.take() else {
                return self
                    .finish(
                        program, &env, start, receipt, deferred, overlay, stage, frame, depth,
                    )
                    .await;
            };
            self.place(
                program,
                &env,
                calls,
                &mut receipt,
                &mut placed,
                overlay,
                stage,
                frame,
                depth,
            )
            .await?;
            let Some((ticket, answer)) = placed.ready.pop_front() else {
                return self
                    .park(
                        program,
                        &env,
                        start,
                        receipt,
                        instance,
                        placed.open,
                        overlay,
                        stage,
                        frame,
                        depth,
                    )
                    .await;
            };
            let resume = ProgramCall::Resume { ticket, answer };
            let next = self
                .segment(
                    program,
                    &env,
                    Some(instance),
                    resume,
                    overlay,
                    stage,
                    frame,
                    depth,
                )
                .await?;
            receipt.outcome = next.receipt.outcome.clone();
            receipt.nested.push(next.receipt);
            calls = next.calls;
            deferred = next.deferred;
            suspended = next.parked;
            let refused = matches!(receipt.outcome, Outcome::Rejected(_));
            if refused {
                overlay.restore(start.checkpoint);
                unit::discard_unrostered(self.committed.reads(), overlay, stage)?;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn place(
        &self,
        program: &str,
        env: &Env,
        calls: Vec<Called>,
        receipt: &mut Receipt,
        placed: &mut Placed,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
        depth: u32,
    ) -> Result<()> {
        if calls.is_empty() {
            return Ok(());
        }
        let as_itself = calls.iter().any(|call| call.acting.is_none());
        let caller = self
            .own_sender(
                program, as_itself, env.height, env.time, overlay, stage, frame,
            )
            .await?;
        for Called {
            ticket,
            target,
            payload,
            acting,
        } in calls
        {
            let sender = self
                .sender(
                    program, acting, &caller, env.height, env.time, overlay, stage, frame,
                )
                .await?;
            let ran = match sender {
                Err(refusal) => Run::finished(rejected(&target, refusal)),
                Ok(sender) => {
                    let cause = Cause::Call { ticket };
                    let env = self.env(&target, program, sender, cause, env.height, env.time);
                    let call = ProgramCall::Execute(payload);
                    let waiter = Waiter::Call { ticket };
                    Box::pin(self.run_frame(
                        &target,
                        call,
                        env,
                        waiter,
                        overlay,
                        stage,
                        frame,
                        depth + 1,
                    ))
                    .await?
                }
            };
            let outcome = ran.receipt.outcome.clone();
            receipt.nested.push(ran.receipt);
            match ran.status {
                Status::Finished => placed.ready.push_back((ticket, answer_of(outcome))),
                Status::Deferred | Status::Parked => placed.open.push(Call { ticket, target }),
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn park(
        &self,
        program: &str,
        env: &Env,
        start: Start,
        mut receipt: Receipt,
        suspended: Suspended,
        calls: Vec<Call>,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
        depth: u32,
    ) -> Result<Run> {
        let image = match self.loaded.runtime().freeze(&suspended).await {
            Ok(image) => image,
            Err(fault) => {
                return self
                    .fail(
                        program,
                        env,
                        start,
                        receipt,
                        refusal_of(fault),
                        overlay,
                        stage,
                        frame,
                        depth,
                    )
                    .await;
            }
        };
        if let Err(refusal) = charge(frame, &image) {
            return self
                .fail(
                    program, env, start, receipt, refusal, overlay, stage, frame, depth,
                )
                .await;
        }
        let id = match start.frame {
            Some(id) => id,
            None => parked::take_frame(self.committed.reads(), overlay)?,
        };
        let park = awaits::Op::Park {
            frame: id,
            program: program.to_owned(),
            calls,
        };
        let told = self
            .tell_awaits(Origin::System, park, env, overlay, stage, frame, depth + 1)
            .await?;
        let outcome = told.outcome.clone();
        receipt.nested.push(told);
        if let Outcome::Rejected(refusal) = outcome {
            return self
                .fail(
                    program, env, start, receipt, refusal, overlay, stage, frame, depth,
                )
                .await;
        }
        let Some(code) = self.loaded.code(program) else {
            let gone = unit::unknown(program);
            return self
                .fail(
                    program, env, start, receipt, gone, overlay, stage, frame, depth,
                )
                .await;
        };
        let record = Parked {
            program: program.to_owned(),
            code,
            origin: env.origin.clone(),
            sender: env.sender.clone(),
            cause: env.cause.clone(),
            waiter: start.waiter,
        };
        parked::save(self.committed.reads(), overlay, id, record, image)?;
        Ok(Run {
            receipt,
            status: Status::Parked,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn fail(
        &self,
        program: &str,
        env: &Env,
        start: Start,
        mut receipt: Receipt,
        refusal: Refusal,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
        depth: u32,
    ) -> Result<Run> {
        overlay.restore(start.checkpoint);
        unit::discard_unrostered(self.committed.reads(), overlay, stage)?;
        receipt.outcome = Outcome::Rejected(refusal);
        self.finish(
            program, env, start, receipt, false, overlay, stage, frame, depth,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish(
        &self,
        program: &str,
        env: &Env,
        start: Start,
        mut receipt: Receipt,
        deferred: bool,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
        depth: u32,
    ) -> Result<Run> {
        let Some(id) = start.frame else {
            let status = match deferred {
                true => Status::Deferred,
                false => Status::Finished,
            };
            return Ok(Run { receipt, status });
        };
        parked::remove(self.committed.reads(), overlay, id, program)?;
        let close = awaits::Op::Close { frame: id };
        let closed = self
            .tell_awaits(Origin::System, close, env, overlay, stage, frame, depth + 1)
            .await?;
        receipt.nested.push(closed);
        if !deferred {
            let outcome = receipt.outcome.clone();
            self.answer(
                program,
                env,
                start.waiter,
                outcome,
                &mut receipt,
                overlay,
                stage,
                frame,
                depth,
            )
            .await?;
        }
        Ok(Run::finished(receipt))
    }

    #[allow(clippy::too_many_arguments)]
    async fn answer(
        &self,
        program: &str,
        env: &Env,
        waiter: Waiter,
        outcome: Outcome,
        receipt: &mut Receipt,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
        depth: u32,
    ) -> Result<()> {
        let answered = match waiter {
            Waiter::Nobody => return Ok(()),
            Waiter::Reply { emitter, item } => {
                self.complete(
                    &emitter,
                    program,
                    item,
                    outcome,
                    env.height,
                    env.time,
                    overlay,
                    stage,
                    frame,
                    depth + 1,
                )
                .await?
            }
            Waiter::Call { ticket } => {
                let answer = answer_of(outcome);
                let resolve = awaits::Op::Resolve { ticket, answer };
                let origin = Origin::Program(program.to_owned());
                self.tell_awaits(origin, resolve, env, overlay, stage, frame, depth + 1)
                    .await?
            }
        };
        receipt.nested.push(answered);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn tell_awaits(
        &self,
        origin: Origin,
        op: awaits::Op,
        env: &Env,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
        depth: u32,
    ) -> Result<Receipt> {
        let role = self.role(role::AWAITS)?.clone();
        let sender = match &origin {
            Origin::Program(program) => {
                self.account_of(program, env.height, env.time, overlay, stage, frame)
                    .await?
            }
            Origin::System | Origin::External(_) => Ok(Some(Principal::System)),
        };
        let sender = match sender {
            Ok(sender) => sender,
            Err(refusal) => return Ok(rejected(&role, refusal)),
        };
        let env = Env {
            network: self.network.to_vec(),
            height: env.height,
            time: env.time,
            me: role.clone(),
            origin,
            sender,
            roles: self.roles.clone(),
            cause: Cause::Direct,
        };
        let call = ProgramCall::Execute(abi::encode(&op));
        let ran = Box::pin(self.run_frame(
            &role,
            call,
            env,
            Waiter::Nobody,
            overlay,
            stage,
            frame,
            depth,
        ))
        .await?;
        Ok(ran.receipt)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn resume_parked(
        &self,
        resumed: Resumed,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
        depth: u32,
    ) -> Result<Receipt> {
        let Resumed {
            frame: id,
            ticket,
            answer,
        } = resumed;
        let view = self.committed.view(vec![&*overlay]);
        let Some((record, image)) = parked::load(&view, id)? else {
            let gone = Refusal::new(reason::NOT_FOUND, format!("frame {id} is not parked"));
            return Ok(rejected(self.role(role::AWAITS)?, gone));
        };
        let program = record.program.clone();
        let env = Env {
            network: self.network.to_vec(),
            height,
            time,
            me: program.clone(),
            origin: record.origin,
            sender: record.sender,
            roles: self.roles.clone(),
            cause: record.cause,
        };
        let start = Start {
            frame: Some(id),
            waiter: record.waiter,
            checkpoint: overlay.checkpoint(),
        };
        let receipt = Receipt {
            program: program.clone(),
            outcome: Outcome::Applied { output: Vec::new() },
            events: Vec::new(),
            nested: Vec::new(),
        };
        let too_deep = depth > MAX_DEPTH;
        if too_deep {
            let refusal = Refusal::new(
                reason::CAPACITY,
                format!("messages nest deeper than {MAX_DEPTH}"),
            );
            let ran = self
                .fail(
                    &program, &env, start, receipt, refusal, overlay, stage, frame, depth,
                )
                .await?;
            return Ok(ran.receipt);
        }
        let running = self.loaded.code(&program);
        let same_code = running == Some(record.code);
        let module = self.loaded.module(&program);
        let (true, Some(module)) = (same_code, module) else {
            let cancelled = awaits::cancelled(&program);
            let ran = self
                .fail(
                    &program, &env, start, receipt, cancelled, overlay, stage, frame, depth,
                )
                .await?;
            return Ok(ran.receipt);
        };
        if let Err(refusal) = charge(frame, &image) {
            let ran = self
                .fail(
                    &program, &env, start, receipt, refusal, overlay, stage, frame, depth,
                )
                .await?;
            return Ok(ran.receipt);
        }
        let suspended = match self
            .loaded
            .runtime()
            .thaw(module, &image, &mut frame.fuel)
            .await
        {
            Ok(suspended) => suspended,
            Err(fault) => {
                let refusal = refusal_of(fault);
                let ran = self
                    .fail(
                        &program, &env, start, receipt, refusal, overlay, stage, frame, depth,
                    )
                    .await?;
                return Ok(ran.receipt);
            }
        };
        let resume = ProgramCall::Resume { ticket, answer };
        let segment = self
            .segment(
                &program,
                &env,
                Some(suspended),
                resume,
                overlay,
                stage,
                frame,
                depth,
            )
            .await?;
        let ran = self
            .follow(&program, env, start, segment, overlay, stage, frame, depth)
            .await?;
        Ok(ran.receipt)
    }

    pub(crate) async fn cancel_parked(
        &self,
        program: &str,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Vec<Receipt>> {
        let frames = parked::of(&self.committed.view(vec![&*overlay]), program)?;
        let mut receipts = Vec::new();
        for id in frames {
            let Some(record) = parked::record(&self.committed.view(vec![&*overlay]), id)? else {
                continue;
            };
            let env = Env {
                network: self.network.to_vec(),
                height,
                time,
                me: program.to_owned(),
                origin: record.origin,
                sender: record.sender,
                roles: self.roles.clone(),
                cause: record.cause,
            };
            let start = Start {
                frame: Some(id),
                waiter: record.waiter,
                checkpoint: overlay.checkpoint(),
            };
            let receipt = rejected(program, awaits::cancelled(program));
            let mut frame = self.open_frame(self.loaded.limits().fuel, overlay)?;
            let ran = self
                .finish(
                    program, &env, start, receipt, false, overlay, stage, &mut frame, 0,
                )
                .await?;
            self.close_frame(overlay, &frame);
            receipts.push(ran.receipt);
        }
        Ok(receipts)
    }
}
