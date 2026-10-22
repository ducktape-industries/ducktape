//! Scheduled runs: at the end of each block the host runs every entry the scheduler role holds due
//! at that height, as the program that scheduled it, and hands the scheduler each run's receipt.
use abi::role::scheduler::{Entry, EntryId, Op, Query, Reply};
use abi::role::{self, identity};
use abi::{Cause, Env, Origin, Outcome, Principal, ProgramCall, Receipt, Refusal, reason};
use blob::Stage;
use commonware_runtime::Spawner;
use commonware_storage::Context;
use state::Overlay;

use crate::{Host, Result, STREAK_LOG_EVERY, rejected, unit};

struct Page {
    entries: Vec<Entry>,
    next: Option<EntryId>,
}

impl<E> Host<E>
where
    E: Context + Spawner,
{
    pub(crate) async fn run_scheduled(
        &self,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Vec<Refusal>> {
        let Some(scheduler) = self.roles.get(role::SCHEDULER).cloned() else {
            return Ok(Vec::new());
        };
        let mut refusals = Vec::new();
        let mut after = None;
        loop {
            let page = match self
                .due(&scheduler, height, after, time, overlay, stage)
                .await?
            {
                Ok(page) => page,
                Err(refusal) => {
                    refusals.push(refusal);
                    return Ok(refusals);
                }
            };
            for listed in page.entries {
                let refused = self
                    .run_listed(&scheduler, listed.id, height, time, overlay, stage)
                    .await?;
                refusals.extend(refused);
            }
            let Some(next) = page.next else {
                return Ok(refusals);
            };
            after = Some(next);
        }
    }

    pub(crate) fn log_scheduler(&mut self, height: u64, refusals: Vec<Refusal>) {
        let Some(first) = refusals.first() else {
            self.scheduler_streak = 0;
            return;
        };
        self.scheduler_streak += 1;
        let blocks = self.scheduler_streak;
        let due = blocks == 1 || blocks.is_multiple_of(STREAK_LOG_EVERY);
        if due {
            tracing::warn!(
                target: "ducktape::scheduler",
                event = "scheduler_refused",
                height,
                refusals = refusals.len(),
                blocks,
                reason = %first.reason,
            );
        }
    }

    async fn run_listed(
        &self,
        scheduler: &str,
        id: EntryId,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Option<Refusal>> {
        let entry = match self
            .entry(scheduler, id, height, time, overlay, stage)
            .await?
        {
            Ok(entry) => entry,
            Err(refusal) => return Ok(Some(refusal)),
        };
        let Some(entry) = entry else {
            return Ok(None);
        };
        let ran_already = entry.receipt.is_some();
        if ran_already {
            return Ok(None);
        }
        let receipt = self.run_entry(&entry, height, time, overlay, stage).await?;
        tracing::debug!(
            target: "ducktape::scheduler",
            event = "scheduled_ran",
            entry = entry.id,
            scheduler = %entry.scheduler,
            target_program = %entry.target,
            height,
            applied = matches!(receipt.outcome, Outcome::Applied { .. }),
        );
        let op = Op::Ran {
            entry: entry.id,
            receipt,
        };
        self.tell_scheduler(scheduler, op, height, time, overlay, stage)
            .await
    }

    async fn run_entry(
        &self,
        entry: &Entry,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Receipt> {
        let asked = identity::Query::OfProgram(entry.scheduler.clone());
        let fuel = &mut self.loaded.limits().fuel;
        let sender = match self
            .source()
            .account(asked, height, time, overlay, stage, fuel)
            .await?
        {
            Ok(account) => account.map(Principal::Account),
            Err(refusal) => return Ok(rejected(&entry.target, refusal)),
        };
        let cause = Cause::Scheduled { entry: entry.id };
        let env = self
            .source()
            .env(&entry.target, &entry.scheduler, sender, cause, height, time);
        let checkpoint = overlay.checkpoint();
        let call = ProgramCall::Execute(entry.payload.clone());
        let receipt = self
            .source()
            .run(&entry.target, call, env, overlay, stage)
            .await?;
        if let Outcome::Rejected(_) = receipt.outcome {
            overlay.restore(checkpoint);
        }
        Ok(receipt)
    }

    async fn tell_scheduler(
        &self,
        scheduler: &str,
        op: Op,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Option<Refusal>> {
        let env = Env {
            network: self.network.clone(),
            height,
            time,
            me: scheduler.to_owned(),
            origin: Origin::System,
            sender: Some(Principal::System),
            roles: self.roles.clone(),
            cause: Cause::Direct,
        };
        let call = ProgramCall::Execute(abi::encode(&op));
        let told = self
            .source()
            .run(scheduler, call, env, overlay, stage)
            .await?;
        match told.outcome {
            Outcome::Applied { .. } => Ok(None),
            Outcome::Rejected(refusal) => Ok(Some(refusal)),
        }
    }

    async fn due(
        &self,
        scheduler: &str,
        height: u64,
        after: Option<EntryId>,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
    ) -> Result<std::result::Result<Page, Refusal>> {
        let query = Query::Due {
            height,
            after,
            limit: None,
        };
        let reply = self
            .ask_scheduler(scheduler, &query, height, time, overlay, stage)
            .await?;
        Ok(reply.and_then(|reply| match reply {
            Reply::Due { entries, next } => Ok(Page { entries, next }),
            Reply::Entry(_) => Err(unexpected("Due")),
        }))
    }

    async fn entry(
        &self,
        scheduler: &str,
        id: EntryId,
        height: u64,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
    ) -> Result<std::result::Result<Option<Entry>, Refusal>> {
        let query = Query::Entry { id };
        let reply = self
            .ask_scheduler(scheduler, &query, height, time, overlay, stage)
            .await?;
        Ok(reply.and_then(|reply| match reply {
            Reply::Entry(entry) => Ok(entry),
            Reply::Due { .. } => Err(unexpected("Entry")),
        }))
    }

    async fn ask_scheduler(
        &self,
        scheduler: &str,
        query: &Query,
        height: u64,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
    ) -> Result<std::result::Result<Reply, Refusal>> {
        let reply = unit::query(
            self.world(height, time),
            vec![overlay],
            stage,
            &[],
            Origin::System,
            scheduler.to_owned(),
            abi::encode(query),
            &mut self.loaded.limits().fuel,
        )
        .await?;
        Ok(reply.and_then(|bytes| {
            abi::decode(&bytes).map_err(|refusal| {
                Refusal::new(
                    reason::UNEXPECTED_REPLY,
                    format!(
                        "the scheduler program answered {query:?} with {}",
                        refusal.sentence
                    ),
                )
            })
        }))
    }
}

fn unexpected(asked: &str) -> Refusal {
    Refusal::new(
        reason::UNEXPECTED_REPLY,
        format!("the scheduler program answered {asked} with another reply"),
    )
}
