//! Reads a program asks the kernel for (`$reads`): the call parks its frame, the member the epoch
//! assigns runs the query off the block path and answers with the reply's blob entry, and once the
//! blobs role holds that blob the kernel holds it for the caller and resumes the frame.
mod preparation;

use abi::role::awaits::{self, Ticket};
use abi::role::reads::{self, Entry, Op, Read, Row, State};
use abi::role::validators;
use abi::{BlobId, Cause, Env, Origin, Outcome, ProgramCall, Receipt, Refusal, Scan, reason, role};
use blob::Stage;
use commonware_runtime::Spawner;
use commonware_storage::Context;
use state::{Overlay, View};

use crate::parked::Waiter;
use crate::parking::{Run, Status};
use crate::unit::{self, Frame};
use crate::{BLOBS, Error, NETWORK, PARKED, Result, Source, corrupt, namespace, rejected};

fn row_key(id: Ticket) -> Vec<u8> {
    [namespace::READ_ROWS, &id.to_be_bytes()].concat()
}

fn due_key(made: u64, id: Ticket) -> Vec<u8> {
    let due = made.saturating_add(reads::WAIT);
    [namespace::READS_DUE, &due.to_be_bytes(), &id.to_be_bytes()].concat()
}

fn close(id: Ticket, made: u64, overlay: &mut Overlay) {
    overlay.delete(PARKED, row_key(id));
    overlay.delete(PARKED, preparation::key(id));
    overlay.delete(PARKED, due_key(made, id));
}

fn row(view: &View<'_>, id: Ticket) -> Result<Option<Row>> {
    view.get(PARKED, &row_key(id))?
        .map(|bytes| abi::decode(&bytes).map_err(corrupt))
        .transpose()
}

fn assignee(view: &View<'_>, id: Ticket, height: u64) -> Result<Option<Vec<u8>>> {
    let Some(length) = view.get(NETWORK, namespace::EPOCH_LENGTH)? else {
        return Ok(None);
    };
    let length: u64 = abi::decode(&length).map_err(corrupt)?;
    let epoch = height.checked_div(length).unwrap_or_default();
    let Some(members) = view.get(NETWORK, &namespace::epoch(epoch))? else {
        return Ok(None);
    };
    let members: Vec<validators::Member> = abi::decode(&members).map_err(corrupt)?;
    let keys: Vec<Vec<u8>> = members.into_iter().map(|member| member.key).collect();
    Ok(reads::assigned(id, &keys).map(<[u8]>::to_vec))
}

fn kernel(outcome: Outcome) -> Receipt {
    Receipt {
        program: reads::PROGRAM.to_owned(),
        outcome,
        events: Vec::new(),
        nested: Vec::new(),
    }
}

fn refused(refusal: Refusal) -> Receipt {
    rejected(reads::PROGRAM, refusal)
}

struct Answer {
    id: Ticket,
    at: u64,
    outcome: std::result::Result<Entry, Refusal>,
}

fn due_id(key: &[u8]) -> Result<Ticket> {
    key.get(namespace::READS_DUE.len() + 8..)
        .and_then(|rest| <[u8; 8]>::try_from(rest).ok())
        .map(u64::from_be_bytes)
        .ok_or_else(|| Error::Corrupt("a read's due row is keyed by no read id".into()))
}

fn not_open(id: Ticket) -> Refusal {
    Refusal::new(reason::NOT_FOUND, format!("no read {id} is open"))
}

fn bundle_of(put: &Outcome) -> std::result::Result<u64, Refusal> {
    match put {
        Outcome::Applied { output } => abi::decode(output).map_err(|refusal| {
            Refusal::new(
                reason::UNEXPECTED_REPLY,
                format!("the blobs role answered a put with {}", refusal.sentence),
            )
        }),
        Outcome::Rejected(refusal) => Err(refusal.clone()),
    }
}

impl<'a, E> Source<'a, E>
where
    E: Context + Spawner,
{
    pub fn reads(&self) -> Result<Vec<(Ticket, Row)>> {
        let view = self.committed.view(Vec::new());
        let mut open = Vec::new();
        for entry in view.scan(PARKED, &Scan::prefix(namespace::READ_ROWS))? {
            let id = entry
                .key
                .get(namespace::READ_ROWS.len()..)
                .and_then(|rest| <[u8; 8]>::try_from(rest).ok())
                .map(u64::from_be_bytes)
                .ok_or_else(|| Error::Corrupt("a read's row is keyed by no read id".into()))?;
            open.push((id, abi::decode(&entry.value).map_err(corrupt)?));
        }
        Ok(open)
    }

    pub fn reader(&self, id: Ticket, height: u64) -> Result<Option<Vec<u8>>> {
        assignee(&self.committed.view(Vec::new()), id, height)
    }

    pub async fn holds(&self, blob: &BlobId, time: u64) -> Result<bool> {
        let height = self.height()?;
        let mut fuel = self.loaded.limits().fuel;
        unit::held(
            self.world(height, time),
            Vec::new(),
            &Stage::default(),
            &[],
            blob,
            &mut fuel,
        )
        .await
    }

    pub(crate) fn reads_at(&self, target: &str, height: u64) -> bool {
        target == reads::PROGRAM && height >= self.reads_from
    }

    pub(crate) fn read_call(
        &self,
        program: &str,
        ticket: Ticket,
        payload: &[u8],
        height: u64,
        overlay: &mut Overlay,
    ) -> Result<Run> {
        let (target, query) = match abi::decode::<Op>(payload) {
            Ok(Op::Read { target, query }) => (target, query),
            Ok(Op::Prepare {
                body,
                hash,
                kind,
                chunk_bytes,
            }) => {
                let preparation = reads::Preparation {
                    body,
                    hash,
                    kind,
                    chunk_bytes,
                };
                return self.prepare_call(program, ticket, preparation, height, overlay);
            }
            Ok(Op::Answer { .. } | Op::Ready { .. } | Op::PreparedAnswer { .. }) => {
                let refusal = Refusal::new(
                    reason::UNSUPPORTED,
                    "a program asks $reads to read; only a member answers a read",
                );
                return Ok(Run::finished(refused(refusal)));
            }
            Err(refusal) => return Ok(Run::finished(refused(refusal))),
        };
        let unserved = {
            let view = self.committed.view(vec![&*overlay]);
            assignee(&view, ticket, height)?.is_none()
        };
        if unserved {
            return Ok(Run::finished(refused(reads::unserved())));
        }
        let row = Row {
            requester: program.to_owned(),
            target,
            query,
            made: height,
            state: State::Asked,
        };
        overlay.set(PARKED, row_key(ticket), abi::encode(&row));
        overlay.set(PARKED, due_key(height, ticket), Vec::new());
        Ok(Run {
            receipt: kernel(Outcome::Applied { output: Vec::new() }),
            status: Status::Deferred,
        })
    }

    pub(crate) async fn answer_read(
        &self,
        signer: &[u8],
        payload: &[u8],
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Receipt> {
        match abi::decode::<Op>(payload) {
            Err(refusal) => Ok(refused(refusal)),
            Ok(Op::Read { .. } | Op::Prepare { .. }) => Ok(refused(Refusal::new(
                reason::UNSUPPORTED,
                "a read is a program's call to $reads, never a frame of its own",
            ))),
            Ok(Op::Answer { id, at, outcome }) => {
                let answer = Answer { id, at, outcome };
                self.answered(signer, answer, height, time, overlay, stage)
                    .await
            }
            Ok(Op::Ready { id }) => self.ready(id, height, time, overlay, stage).await,
            Ok(Op::PreparedAnswer { id, at, outcome }) => {
                self.prepared_answer(signer, id, at, outcome, height, time, overlay, stage)
                    .await
            }
        }
    }

    async fn answered(
        &self,
        signer: &[u8],
        answer: Answer,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Receipt> {
        let Answer { id, at, outcome } = answer;
        let (open, member) = {
            let view = self.committed.view(vec![&*overlay]);
            (row(&view, id)?, assignee(&view, id, height)?)
        };
        let Some(mut open) = open else {
            return Ok(refused(not_open(id)));
        };
        if open.state != State::Asked {
            let answered = format!("read {id} is answered already");
            return Ok(refused(Refusal::new(reason::WRONG_STATE, answered)));
        }
        if member.as_deref() != Some(signer) {
            let theirs = format!("read {id} is another member's to answer");
            return Ok(refused(Refusal::new(reason::UNAUTHORIZED, theirs)));
        }
        let read_since_asked = open.made <= at;
        let read_by_now = at <= height;
        let in_bounds = read_since_asked && read_by_now;
        if !in_bounds {
            let made = open.made;
            let stale = format!(
                "read {id} was asked at {made}; an answer read at {at} is outside {made}..={height}"
            );
            return Ok(refused(Refusal::new(reason::INVALID_INPUT, stale)));
        }
        let mut frame = self.open_frame(self.loaded.limits().fuel, None, overlay)?;
        let mut receipt = kernel(Outcome::Applied { output: Vec::new() });
        let awaited = self
            .awaited(id, height, time, overlay, stage, &mut frame)
            .await?;
        let entry = match (awaited, outcome) {
            (false, _) => None,
            (true, Err(refusal)) => {
                self.resume_read(
                    id,
                    Err(refusal),
                    height,
                    time,
                    &mut receipt,
                    overlay,
                    stage,
                    &mut frame,
                )
                .await?;
                None
            }
            (true, Ok(entry)) => Some(entry),
        };
        let Some(entry) = entry else {
            close(id, open.made, overlay);
            self.close_frame(overlay, &frame);
            return Ok(receipt);
        };
        let put = self
            .put_read(
                &open.requester,
                &entry,
                height,
                time,
                overlay,
                stage,
                &mut frame,
            )
            .await?;
        let bundle = bundle_of(&put.outcome);
        receipt.nested.push(put);
        match bundle {
            Ok(bundle) => {
                let read = Read { entry, at };
                open.state = State::Putting { bundle, read };
                overlay.set(PARKED, row_key(id), abi::encode(&open));
            }
            Err(refusal) => {
                close(id, open.made, overlay);
                self.resume_read(
                    id,
                    Err(refusal),
                    height,
                    time,
                    &mut receipt,
                    overlay,
                    stage,
                    &mut frame,
                )
                .await?;
            }
        }
        self.close_frame(overlay, &frame);
        Ok(receipt)
    }

    async fn ready(
        &self,
        id: Ticket,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Receipt> {
        let open = {
            let view = self.committed.view(vec![&*overlay]);
            row(&view, id)?
        };
        let Some(open) = open else {
            return self
                .ready_preparation(id, height, time, overlay, stage)
                .await;
        };
        let State::Putting { read, .. } = open.state else {
            let early = format!("read {id} is not answered yet");
            return Ok(refused(Refusal::new(reason::WRONG_STATE, early)));
        };
        let mut frame = self.open_frame(self.loaded.limits().fuel, None, overlay)?;
        let held = unit::held(
            self.world(height, time),
            vec![&*overlay],
            &*stage,
            &[],
            &read.entry.id,
            &mut frame.fuel,
        )
        .await?;
        if !held {
            let early = format!("the blobs role does not hold read {id}'s blob yet");
            return Ok(refused(Refusal::new(reason::WRONG_STATE, early)));
        }
        let mut receipt = kernel(Outcome::Applied { output: Vec::new() });
        close(id, open.made, overlay);
        let awaited = self
            .awaited(id, height, time, overlay, stage, &mut frame)
            .await?;
        if awaited {
            self.hold(&open.requester, &read.entry.id, &row_key(id), overlay)?;
            let answer = Ok(abi::encode(&read));
            self.resume_read(
                id,
                answer,
                height,
                time,
                &mut receipt,
                overlay,
                stage,
                &mut frame,
            )
            .await?;
        }
        self.close_frame(overlay, &frame);
        Ok(receipt)
    }

    pub(crate) async fn expire_reads(
        &self,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<()> {
        if height < self.reads_from {
            return Ok(());
        }
        let past = height.saturating_add(1).to_be_bytes();
        let hi = [namespace::READS_DUE, &past].concat();
        let due = self
            .committed
            .view(vec![&*overlay])
            .scan(PARKED, &Scan::range(namespace::READS_DUE, Some(hi)))?;
        if due.is_empty() {
            return Ok(());
        }
        for entry in due {
            let id = due_id(&entry.key)?;
            overlay.delete(PARKED, entry.key);
            overlay.delete(PARKED, row_key(id));
            overlay.delete(PARKED, preparation::key(id));
            let mut frame = self.open_frame(self.loaded.limits().fuel, None, overlay)?;
            let mut receipt = kernel(Outcome::Applied { output: Vec::new() });
            let awaited = self
                .awaited(id, height, time, overlay, stage, &mut frame)
                .await?;
            if awaited {
                let expired = Err(reads::expired(id));
                self.resume_read(
                    id,
                    expired,
                    height,
                    time,
                    &mut receipt,
                    overlay,
                    stage,
                    &mut frame,
                )
                .await?;
            }
            self.close_frame(overlay, &frame);
        }
        Ok(())
    }

    async fn awaited(
        &self,
        id: Ticket,
        height: u64,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
        frame: &mut Frame,
    ) -> Result<bool> {
        let asked = awaits::Query::Ticket { ticket: id };
        let reply = unit::query(
            self.world(height, time),
            vec![overlay],
            stage,
            &[],
            Origin::System,
            self.role(role::AWAITS)?.clone(),
            abi::encode(&asked),
            &mut frame.fuel,
        )
        .await?;
        let row = reply.and_then(|bytes| abi::decode::<awaits::Reply>(&bytes));
        Ok(matches!(row, Ok(awaits::Reply::Ticket(Some(_)))))
    }

    #[allow(clippy::too_many_arguments)]
    async fn put_read(
        &self,
        requester: &str,
        entry: &Entry,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
    ) -> Result<Receipt> {
        self.put_read_entries(
            requester,
            std::slice::from_ref(entry),
            height,
            time,
            overlay,
            stage,
            frame,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn put_read_entries(
        &self,
        requester: &str,
        entries: &[Entry],
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
    ) -> Result<Receipt> {
        let blobs = self.role(role::BLOBS)?.clone();
        let sender = match self
            .account_of(requester, height, time, overlay, stage, frame)
            .await?
        {
            Ok(sender) => sender,
            Err(refusal) => return Ok(rejected(&blobs, refusal)),
        };
        let put = blob::contract::Op::Put {
            blobs: entries
                .iter()
                .map(|entry| blob::contract::Entry {
                    id: entry.id,
                    len: entry.len,
                    kind: entry.kind.clone(),
                })
                .collect(),
        };
        let env = self.env(&blobs, requester, sender, Cause::Direct, height, time);
        let call = ProgramCall::Execute(abi::encode(&put));
        let ran =
            Box::pin(self.run_frame(&blobs, call, env, Waiter::Nobody, overlay, stage, frame, 1))
                .await?;
        Ok(ran.receipt)
    }

    fn hold(
        &self,
        requester: &str,
        blob: &BlobId,
        line: &[u8],
        overlay: &mut Overlay,
    ) -> Result<()> {
        let place = self.loaded.place(requester).to_owned();
        let key = namespace::blob(blob);
        let roster = self.committed.view(vec![&*overlay]).get(BLOBS, &key)?;
        overlay.set(BLOBS, namespace::line(&place, line), namespace::blob(blob));
        overlay.set(BLOBS, namespace::on(blob, &place, line), Vec::new());
        match namespace::changed(roster.as_deref(), &place, namespace::Change::List)? {
            namespace::Roster::Unchanged => {}
            namespace::Roster::Written(value) => overlay.set(BLOBS, key, value),
            namespace::Roster::Dropped => overlay.delete(BLOBS, key),
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn resume_read(
        &self,
        id: Ticket,
        answer: awaits::Answer,
        height: u64,
        time: u64,
        receipt: &mut Receipt,
        overlay: &mut Overlay,
        stage: &mut Stage,
        frame: &mut Frame,
    ) -> Result<()> {
        let env = Env {
            network: self.network.to_vec(),
            height,
            time,
            me: reads::PROGRAM.to_owned(),
            origin: Origin::System,
            sender: None,
            roles: self.roles.clone(),
            cause: Cause::Direct,
        };
        let resolve = awaits::Op::Resolve { ticket: id, answer };
        let told = self
            .tell_awaits(Origin::System, resolve, &env, overlay, stage, frame, 1)
            .await?;
        receipt.nested.push(told);
        Ok(())
    }
}
