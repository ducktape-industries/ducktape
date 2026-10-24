use super::*;
use reads::{Preparation, PreparationRow, PreparationState, Product};

pub(super) fn key(id: Ticket) -> Vec<u8> {
    [namespace::PREPARATION_ROWS, &id.to_be_bytes()].concat()
}

fn preparation(view: &View<'_>, id: Ticket) -> Result<Option<PreparationRow>> {
    view.get(PARKED, &key(id))?
        .map(|bytes| abi::decode(&bytes).map_err(corrupt))
        .transpose()
}

impl<'a, E> Source<'a, E>
where
    E: Context + Spawner,
{
    pub fn preparer(&self, id: Ticket, made: u64, height: u64) -> Result<Option<Vec<u8>>> {
        self.preparation_responder(&self.committed.view(Vec::new()), id, made, height)
    }

    fn preparation_responder(
        &self,
        view: &View<'_>,
        id: Ticket,
        made: u64,
        height: u64,
    ) -> Result<Option<Vec<u8>>> {
        let policy = self
            .preparation_responder
            .filter(|policy| made >= policy.from);
        let Some(policy) = policy else {
            return assignee(view, id, height);
        };
        let Some(length) = view.get(NETWORK, namespace::EPOCH_LENGTH)? else {
            return Ok(None);
        };
        let length: u64 = abi::decode(&length).map_err(corrupt)?;
        let epoch = height.checked_div(length).unwrap_or_default();
        let Some(members) = view.get(NETWORK, &namespace::epoch(epoch))? else {
            return Ok(None);
        };
        let members: Vec<validators::Member> = abi::decode(&members).map_err(corrupt)?;
        Ok(members
            .into_iter()
            .find(|member| member.key == policy.key)
            .map(|member| member.key))
    }

    pub fn preparations(&self) -> Result<Vec<(Ticket, PreparationRow)>> {
        let view = self.committed.view(Vec::new());
        let mut open = Vec::new();
        for entry in view.scan(PARKED, &Scan::prefix(namespace::PREPARATION_ROWS))? {
            let id = entry
                .key
                .get(namespace::PREPARATION_ROWS.len()..)
                .and_then(|rest| <[u8; 8]>::try_from(rest).ok())
                .map(u64::from_be_bytes)
                .ok_or_else(|| Error::Corrupt("a preparation has no ticket key".into()))?;
            open.push((id, abi::decode(&entry.value).map_err(corrupt)?));
        }
        Ok(open)
    }

    pub async fn preparation_held(&self, bundle: u64, time: u64) -> Result<bool> {
        let height = self.height()?;
        let mut fuel = self.loaded.limits().fuel;
        self.preparation_bundle_held(
            bundle,
            height,
            time,
            &Overlay::default(),
            &Stage::default(),
            &mut fuel,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn preparation_bundle_held(
        &self,
        bundle: u64,
        height: u64,
        time: u64,
        overlay: &Overlay,
        stage: &Stage,
        fuel: &mut Option<u64>,
    ) -> Result<bool> {
        let asked = blob::contract::Query::Bundle { n: bundle };
        let answer = unit::query(
            self.world(height, time),
            vec![overlay],
            stage,
            &[],
            Origin::System,
            self.role(role::BLOBS)?.clone(),
            abi::encode(&asked),
            fuel,
        )
        .await?;
        let Ok(bytes) = answer else {
            return Ok(false);
        };
        let reply = abi::decode::<blob::contract::Reply>(&bytes);
        Ok(matches!(reply, Ok(blob::contract::Reply::Bundle(Some(bundle))) if bundle.held()))
    }

    pub(super) fn prepare_call(
        &self,
        requester: &str,
        id: Ticket,
        preparation: Preparation,
        height: u64,
        overlay: &mut Overlay,
    ) -> Result<Run> {
        let valid_chunk = preparation.chunk_bytes > 0;
        let valid_kind = blob::is_kind(&preparation.kind);
        let valid = valid_chunk && valid_kind;
        if !valid {
            let refusal = Refusal::new(
                reason::INVALID_INPUT,
                "preparation needs a nonzero chunk size and a blob kind",
            );
            return Ok(Run::finished(refused(refusal)));
        }
        let view = self.committed.view(vec![&*overlay]);
        if self
            .preparation_responder(&view, id, height, height)?
            .is_none()
        {
            return Ok(Run::finished(refused(reads::unserved())));
        }
        let row = PreparationRow {
            requester: requester.to_owned(),
            preparation,
            made: height,
            state: PreparationState::Asked,
        };
        overlay.set(PARKED, key(id), abi::encode(&row));
        overlay.set(PARKED, due_key(height, id), Vec::new());
        Ok(Run {
            receipt: kernel(Outcome::Applied { output: Vec::new() }),
            status: Status::Deferred,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn prepared_answer(
        &self,
        signer: &[u8],
        id: Ticket,
        at: u64,
        outcome: std::result::Result<Product, Refusal>,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Receipt> {
        let (open, member) = {
            let view = self.committed.view(vec![&*overlay]);
            let open = preparation(&view, id)?;
            let member = match &open {
                Some(open) => self.preparation_responder(&view, id, open.made, height)?,
                None => None,
            };
            (open, member)
        };
        let Some(mut open) = open else {
            return Ok(refused(not_open(id)));
        };
        if open.state != PreparationState::Asked {
            return Ok(refused(Refusal::new(
                reason::WRONG_STATE,
                "preparation is answered already",
            )));
        }
        if member.as_deref() != Some(signer) {
            return Ok(refused(Refusal::new(
                reason::UNAUTHORIZED,
                "preparation belongs to another member",
            )));
        }
        let in_bounds = open.made <= at && at <= height;
        if !in_bounds {
            return Ok(refused(Refusal::new(
                reason::INVALID_INPUT,
                "preparation answer is outside its asked and current heights",
            )));
        }
        let mut frame = self.open_frame(self.loaded.limits().fuel, None, overlay)?;
        let mut receipt = kernel(Outcome::Applied { output: Vec::new() });
        let awaited = self
            .awaited(id, height, time, overlay, stage, &mut frame)
            .await?;
        let product = match (awaited, outcome) {
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
            (true, Ok(product)) => Some(product),
        };
        let Some(product) = product else {
            close(id, open.made, overlay);
            self.close_frame(overlay, &frame);
            return Ok(receipt);
        };
        let mut entries = product.produced.clone();
        entries.push(product.reply.clone());
        let put = self
            .put_read_entries(
                &open.requester,
                &entries,
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
                let read = Read {
                    entry: product.reply,
                    at,
                };
                open.state = PreparationState::Putting {
                    bundle,
                    read,
                    produced: product.produced,
                };
                overlay.set(PARKED, key(id), abi::encode(&open));
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

    pub(super) async fn ready_preparation(
        &self,
        id: Ticket,
        height: u64,
        time: u64,
        overlay: &mut Overlay,
        stage: &mut Stage,
    ) -> Result<Receipt> {
        let open = preparation(&self.committed.view(vec![&*overlay]), id)?;
        let Some(open) = open else {
            return Ok(refused(not_open(id)));
        };
        let PreparationState::Putting { bundle, read, .. } = open.state else {
            return Ok(refused(Refusal::new(
                reason::WRONG_STATE,
                "preparation is not answered yet",
            )));
        };
        let mut frame = self.open_frame(self.loaded.limits().fuel, None, overlay)?;
        let held = self
            .preparation_bundle_held(bundle, height, time, overlay, stage, &mut frame.fuel)
            .await?;
        if !held {
            self.close_frame(overlay, &frame);
            return Ok(refused(Refusal::new(
                reason::WRONG_STATE,
                "the preparation bundle is not held yet",
            )));
        }
        let mut receipt = kernel(Outcome::Applied { output: Vec::new() });
        close(id, open.made, overlay);
        let awaited = self
            .awaited(id, height, time, overlay, stage, &mut frame)
            .await?;
        if awaited {
            self.hold(&open.requester, &read.entry.id, &row_key(id), overlay)?;
            self.resume_read(
                id,
                Ok(abi::encode(&read)),
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
}
