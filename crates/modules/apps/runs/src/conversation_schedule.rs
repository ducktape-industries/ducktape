//! Named one-shot timers. Crank is driven by committed consensus time, not a host clock.
use super::*;
const SCHEDULE_QUEUE: &str = "conversation/schedule_queue";
const NEXT_SCHEDULE_DUE: &str = "conversation/next_schedule_due";
fn schedule_key(id: &str, slot: &str) -> String {
    format!("{}/{}", key("schedule", id), dispatch_id_for(slot))
}
fn schedule_index(id: &str) -> String {
    key("schedules", id)
}
#[derive(Clone, Serialize, Deserialize)]
struct ScheduledRef {
    conversation_id: String,
    schedule_id: String,
    operation_id: String,
    due_at: u64,
}

pub(super) fn validate_queue(records: &crate::receipts::Records) -> Result<(), String> {
    let Some(bytes) = records.get(SCHEDULE_QUEUE) else {
        return Ok(());
    };
    let queue: Vec<ScheduledRef> = sdk::wire::decode(bytes)?;
    let head: Option<u64> = sdk::wire::decode(
        records
            .get(NEXT_SCHEDULE_DUE)
            .ok_or_else(|| "missing conversation timer head".to_string())?,
    )?;
    if head != queue.first().map(|entry| entry.due_at) {
        return Err("conversation timer head mismatch".into());
    }
    let ordered = queue.windows(2).all(|pair| {
        (
            pair[0].due_at,
            &pair[0].conversation_id,
            &pair[0].schedule_id,
        ) < (
            pair[1].due_at,
            &pair[1].conversation_id,
            &pair[1].schedule_id,
        )
    });
    if !ordered {
        return Err("conversation timer queue is not ordered".into());
    }
    for entry in queue {
        let bytes = records
            .get(&schedule_key(&entry.conversation_id, &entry.schedule_id))
            .ok_or_else(|| "missing queued conversation timer".to_string())?;
        let schedule: ConversationSchedule = sdk::wire::decode(bytes)?;
        let exact = schedule.operation_id == entry.operation_id
            && schedule.status
                == ConversationScheduleStatus::Pending {
                    due_at: entry.due_at,
                };
        if !exact {
            return Err("conversation timer queue record mismatch".into());
        }
    }
    Ok(())
}

pub(super) fn validate_for_conversation(
    records: &crate::receipts::Records,
    state: &ConversationView,
) -> Result<(), String> {
    let Some(bytes) = records.get(&schedule_index(&state.conversation_id)) else {
        return Ok(());
    };
    let slots: Vec<String> = sdk::wire::decode(bytes)?;
    let ordered = slots.windows(2).all(|pair| pair[0] < pair[1]);
    if slots.len() > 64 || !ordered {
        return Err("invalid conversation schedule index".into());
    }
    for slot in slots {
        let bytes = records
            .get(&schedule_key(&state.conversation_id, &slot))
            .ok_or_else(|| "missing conversation schedule record".to_string())?;
        let schedule: ConversationSchedule = sdk::wire::decode(bytes)?;
        let bound =
            schedule.conversation_id == state.conversation_id && schedule.schedule_id == slot;
        if !bound {
            return Err("conversation schedule identity mismatch".into());
        }
        let invalid_receipt = matches!(schedule.status, ConversationScheduleStatus::Fired { sequence } if sequence == 0 || sequence > state.admitted_cursor);
        if invalid_receipt {
            return Err("conversation timer has no admitted event".into());
        }
    }
    Ok(())
}

enum ScheduleInput {
    Set {
        schedule: ConversationSchedule,
        due_at: u64,
    },
    Cancel {
        schedule: ConversationSchedule,
    },
    Fire {
        schedule: ConversationSchedule,
        sequence: u64,
        now: u64,
    },
}
fn schedule_step(input: ScheduleInput) -> Result<ConversationSchedule, Error> {
    match input {
        ScheduleInput::Set { schedule, due_at } => schedule_set(schedule, due_at),
        ScheduleInput::Cancel { schedule } => schedule_cancel(schedule),
        ScheduleInput::Fire {
            schedule,
            sequence,
            now,
        } => schedule_fire(schedule, sequence, now),
    }
}
fn schedule_set(
    mut schedule: ConversationSchedule,
    due_at: u64,
) -> Result<ConversationSchedule, Error> {
    schedule.status = ConversationScheduleStatus::Pending { due_at };
    Ok(schedule)
}
fn schedule_cancel(mut schedule: ConversationSchedule) -> Result<ConversationSchedule, Error> {
    schedule.status = ConversationScheduleStatus::Cancelled;
    Ok(schedule)
}
fn schedule_fire(
    mut schedule: ConversationSchedule,
    sequence: u64,
    now: u64,
) -> Result<ConversationSchedule, Error> {
    let due =
        matches!(schedule.status, ConversationScheduleStatus::Pending { due_at } if due_at <= now);
    if !due {
        return Err(Error::Module("conversation schedule is not due".into()));
    }
    schedule.status = ConversationScheduleStatus::Fired { sequence };
    Ok(schedule)
}

impl RunsModule {
    pub(crate) async fn next_conversation_input_due(&self) -> Result<Option<u64>, Error> {
        Ok(self
            .conversation_read::<Option<u64>>(NEXT_SCHEDULE_DUE)
            .await?
            .flatten())
    }
    pub(crate) async fn conversation_schedules(
        &self,
        id: &str,
    ) -> Result<Vec<ConversationSchedule>, Error> {
        let slots: Vec<String> = self
            .conversation_read(&schedule_index(id))
            .await?
            .unwrap_or_default();
        let mut schedules = Vec::new();
        for slot in slots {
            let schedule = self
                .conversation_read(&schedule_key(id, &slot))
                .await?
                .ok_or_else(|| Error::Module("missing conversation schedule".into()))?;
            schedules.push(schedule);
        }
        Ok(schedules)
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn schedule_conversation_input(
        &mut self,
        ctx: &dyn Ctx,
        id: String,
        op: String,
        slot: String,
        after_secs: Option<u64>,
        input: ConversationInput,
    ) -> Result<(), Error> {
        let state = self.require_conversation(&id).await?;
        self.conversation_controller(ctx, &state).await?;
        require_coordinating_source(&state)?;
        let valid_slot = !slot.is_empty() && slot.len() <= MAX_REQUEST_ID_BYTES;
        if !valid_slot {
            return Err(Error::Module("invalid conversation schedule id".into()));
        }
        if matches!(input, ConversationInput::Chat { .. }) {
            return Err(Error::Module(
                "scheduled inputs cannot forge Chat snapshots".into(),
            ));
        }
        let payload = sdk::wire::encode(&("schedule", &slot, after_secs, &input));
        if self.operation_seen(&id, &op, &payload).await? {
            return Ok(());
        }
        let schedule = ConversationSchedule {
            conversation_id: id.clone(),
            schedule_id: slot.clone(),
            operation_id: op.clone(),
            actor: ctx.env().origin.clone(),
            input,
            status: ConversationScheduleStatus::Cancelled,
        };
        let transition = match after_secs {
            None => ScheduleInput::Cancel { schedule },
            Some(seconds) => {
                let unit = self.time_unit.ok_or_else(|| {
                    Error::Module("conversation scheduling requires genesis time_unit".into())
                })?;
                let duration = seconds.checked_mul(unit.per_second()).ok_or_else(|| {
                    Error::Module("conversation schedule duration overflow".into())
                })?;
                let due_at = ctx
                    .env()
                    .consensus_time
                    .checked_add(duration)
                    .ok_or_else(|| {
                        Error::Module("conversation schedule deadline overflow".into())
                    })?;
                ScheduleInput::Set { schedule, due_at }
            }
        };
        let schedule = schedule_step(transition)?;
        let mut slots: Vec<String> = self
            .conversation_read(&schedule_index(&id))
            .await?
            .unwrap_or_default();
        if !slots.contains(&slot) {
            if slots.len() >= 64 {
                return Err(Error::Module(
                    "conversation schedule allocation is full".into(),
                ));
            }
            slots.push(slot.clone());
            slots.sort();
        }
        let mut queue: Vec<ScheduledRef> = self
            .conversation_read(SCHEDULE_QUEUE)
            .await?
            .unwrap_or_default();
        queue.retain(|entry| entry.conversation_id != id || entry.schedule_id != slot);
        if let ConversationScheduleStatus::Pending { due_at } = schedule.status {
            queue.push(ScheduledRef {
                conversation_id: id.clone(),
                schedule_id: slot,
                operation_id: op.clone(),
                due_at,
            });
            queue.sort_by(|a, b| {
                (a.due_at, &a.conversation_id, &a.schedule_id).cmp(&(
                    b.due_at,
                    &b.conversation_id,
                    &b.schedule_id,
                ))
            });
        }
        let records = [
            (
                schedule_key(&id, &schedule.schedule_id),
                sdk::wire::encode(&schedule),
            ),
            (schedule_index(&id), sdk::wire::encode(&slots)),
            (SCHEDULE_QUEUE.into(), sdk::wire::encode(&queue)),
            (
                NEXT_SCHEDULE_DUE.into(),
                sdk::wire::encode(&queue.first().map(|entry| entry.due_at)),
            ),
            (op_key(&id, &op), payload),
        ];
        self.write_schedule_records(records)
    }
    fn write_schedule_records<const N: usize>(
        &mut self,
        records: [(String, Vec<u8>); N],
    ) -> Result<(), Error> {
        let fits = records
            .iter()
            .all(|(_, bytes)| bytes.len() <= sdk::MAX_STORE_VALUE_BYTES);
        if !fits {
            return Err(Error::Module(
                "conversation schedule exceeds store bound".into(),
            ));
        }
        for (key, bytes) in records {
            self.receipts.stage(key, bytes)?;
        }
        Ok(())
    }
    pub(crate) async fn crank_conversation_inputs(&mut self, ctx: &dyn Ctx) -> Result<(), Error> {
        let mut queue: Vec<ScheduledRef> = self
            .conversation_read(SCHEDULE_QUEUE)
            .await?
            .unwrap_or_default();
        let count = queue
            .iter()
            .take(32)
            .take_while(|entry| entry.due_at <= ctx.env().consensus_time)
            .count();
        if count == 0 {
            return Ok(());
        }
        let due: Vec<_> = queue.drain(..count).collect();
        for entry in due {
            let record_key = schedule_key(&entry.conversation_id, &entry.schedule_id);
            let schedule: ConversationSchedule = self
                .conversation_read(&record_key)
                .await?
                .ok_or_else(|| Error::Module("missing scheduled input".into()))?;
            let current = schedule.operation_id == entry.operation_id
                && schedule.status
                    == ConversationScheduleStatus::Pending {
                        due_at: entry.due_at,
                    };
            if !current {
                return Err(Error::Module(
                    "scheduled input index disagrees with its record".into(),
                ));
            }
            let state = self.require_conversation(&entry.conversation_id).await?;
            let sequence = state
                .admitted_cursor
                .checked_add(1)
                .ok_or_else(|| Error::Module("conversation input cursor exhausted".into()))?;
            let operation_id = format!(
                "timer/{}",
                dispatch_id_for(&format!("{}/{}", entry.schedule_id, entry.operation_id))
            );
            // Timer provenance is the account that configured this exact slot,
            // never the permissionless member that happened to crank it.
            let event = ConversationEvent {
                sequence,
                operation_id,
                actor: schedule.actor.clone(),
                input: schedule.input.clone(),
                admitted_at: ctx.env().height,
            };
            self.apply_conversation(ctx, &state, Input::Append(event))
                .await?;
            let schedule = schedule_step(ScheduleInput::Fire {
                schedule,
                sequence,
                now: ctx.env().consensus_time,
            })?;
            self.write_schedule_records([(record_key, sdk::wire::encode(&schedule))])?;
        }
        self.write_schedule_records([
            (SCHEDULE_QUEUE.into(), sdk::wire::encode(&queue)),
            (
                NEXT_SCHEDULE_DUE.into(),
                sdk::wire::encode(&queue.first().map(|entry| entry.due_at)),
            ),
        ])
    }
}
