//! Immutable proposals, reserved completion markers and an indexed outbox.
use super::action_requests::{
    ActionRequest, Prepared, Publication, RequestScope, canonical_action_payload,
};
use super::receipts::View;
use super::*;
use sdk::{Ack, CallId, Cause, DeliveryOutcome, Hop, PendingItem};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Decision {
    Awaiting,
    Claimed { call: CallId },
    Completed { call: CallId },
    Rejected,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    publication: Publication,
    decision: Decision,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Queue {
    head: Option<u64>,
    tail: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueItem {
    request_id: String,
    next: Option<u64>,
}

fn body_key(id: &str) -> String {
    format!("action/body/{id}")
}
fn marker_key(id: &str) -> String {
    format!("action/marker/{id}")
}
fn reason_key(id: &str) -> String {
    format!("action/reason/{id}")
}
fn item_key(item: u64) -> String {
    format!("action/item/{item}")
}
const QUEUE_KEY: &str = "action/queue";

fn marker_bytes(marker: &Marker, capacity: usize) -> Result<Vec<u8>, Error> {
    let encoded = sdk::wire::encode(marker);
    let fits = encoded
        .len()
        .checked_add(8)
        .is_some_and(|size| size <= capacity);
    if !fits {
        return Err(Error::Module(
            "action completion exceeds its reserved marker".into(),
        ));
    }
    let mut bytes = vec![0; capacity];
    bytes[..8].copy_from_slice(&(encoded.len() as u64).to_le_bytes());
    bytes[8..8 + encoded.len()].copy_from_slice(&encoded);
    Ok(bytes)
}

fn decode_marker(bytes: &[u8]) -> Result<Marker, Error> {
    let prefix: [u8; 8] = bytes
        .get(..8)
        .ok_or_else(|| Error::Module("truncated action marker".into()))?
        .try_into()
        .expect("eight bytes");
    let length = usize::try_from(u64::from_le_bytes(prefix))
        .map_err(|_| Error::Module("action marker length overflow".into()))?;
    let end = length
        .checked_add(8)
        .ok_or_else(|| Error::Module("action marker length overflow".into()))?;
    let encoded = bytes
        .get(8..end)
        .ok_or_else(|| Error::Module("truncated action marker body".into()))?;
    let canonical_padding = bytes[end..].iter().all(|byte| *byte == 0);
    if !canonical_padding {
        return Err(Error::Module("invalid action marker padding".into()));
    }
    sdk::wire::decode(encoded).map_err(Error::Module)
}

impl RunsModule {
    pub(super) async fn action_request(&self, id: &str) -> Result<Option<ActionRequest>, Error> {
        let Some(bytes) = self.receipts.get(&body_key(id)).await? else {
            return Ok(None);
        };
        let mut request: ActionRequest = sdk::wire::decode(&bytes).map_err(Error::Module)?;
        let bytes = self
            .receipts
            .get(&marker_key(id))
            .await?
            .ok_or_else(|| Error::Module("action has no reserved completion marker".into()))?;
        let marker = decode_marker(&bytes)?;
        request.publication = marker.publication;
        request.view.status = match marker.decision {
            Decision::Awaiting => ActionStatus::AwaitingProgram,
            Decision::Claimed { call } | Decision::Completed { call } => {
                ActionStatus::Claimed { call }
            }
            Decision::Rejected => {
                let reason = self
                    .receipts
                    .get(&reason_key(id))
                    .await?
                    .map(|bytes| sdk::wire::decode(&bytes).map_err(Error::Module))
                    .transpose()?
                    .unwrap_or_else(|| {
                        "action rejected; its diagnostic exceeded the receipt bound".into()
                    });
                ActionStatus::Rejected { reason }
            }
        };
        Ok(Some(request))
    }

    async fn action_queue(&self, view: View) -> Result<Queue, Error> {
        let bytes = self.receipts.read(QUEUE_KEY, view).await?;
        bytes
            .map(|bytes| sdk::wire::decode(&bytes).map_err(Error::Module))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    async fn action_queue_item(&self, item: u64, view: View) -> Result<QueueItem, Error> {
        let key = item_key(item);
        let bytes = self.receipts.read(&key, view).await?;
        sdk::wire::decode(
            &bytes.ok_or_else(|| Error::Module("missing action publication queue item".into()))?,
        )
        .map_err(Error::Module)
    }

    pub(super) async fn stage_action_marker(
        &mut self,
        request: &ActionRequest,
    ) -> Result<(), Error> {
        let id = &request.view.request_id;
        let reserved = self
            .receipts
            .get(&marker_key(id))
            .await?
            .ok_or_else(|| Error::Module("action has no reserved marker".into()))?;
        let decision = match &request.view.status {
            ActionStatus::AwaitingProgram => Decision::Awaiting,
            ActionStatus::Claimed { call } => Decision::Claimed { call: call.clone() },
            ActionStatus::Completed { call, .. } => Decision::Completed { call: call.clone() },
            ActionStatus::Rejected { reason } => {
                let bytes = sdk::wire::encode(reason);
                if bytes.len() <= sdk::MAX_STORE_VALUE_BYTES {
                    self.receipts.stage(reason_key(id), bytes)?;
                }
                Decision::Rejected
            }
        };
        let marker = Marker {
            publication: request.publication.clone(),
            decision,
        };
        self.receipts
            .stage(marker_key(id), marker_bytes(&marker, reserved.len())?)
    }

    pub(super) async fn stage_action_request(
        &mut self,
        entry: &PendingState,
        id: String,
        scope: RequestScope,
        prepared: Prepared,
    ) -> Result<(), Error> {
        let Prepared { message, receipt } = prepared;
        let lane = match &scope {
            RequestScope::Session { .. } => LaneKind::Live,
            RequestScope::Result => LaneKind::Final,
        };
        let payload =
            canonical_action_payload(sdk::wire::decode(&message.payload).map_err(Error::Module)?);
        let view = ActionRequestView {
            request_id: id.clone(),
            account: entry.account,
            generation: entry.generation,
            run_id: entry.run_id.clone(),
            operation: receipt.operation,
            result: canonical_action_payload(receipt.result),
            target: message.target,
            payload,
            status: ActionStatus::AwaitingProgram,
        };
        if self.action_request(&id).await?.is_some() {
            return Err(Error::Module(
                "action request id is already bound to earlier work".into(),
            ));
        }
        let item = self
            .staged_next_action_item
            .unwrap_or(self.next_action_item);
        let next = item
            .checked_add(1)
            .ok_or_else(|| Error::Module("action delivery counter exhausted".into()))?;
        self.model(&entry.agent_id)
            .ok_or_else(|| Error::Module("run model no longer exists".into()))?;
        let record = ActionRequest {
            view,
            item,
            cause: entry.cause.clone(),
            publication: Publication::Queued,
            scope,
            model_id: entry.agent_id.clone(),
            invocation: receipt.invocation,
        };
        // Agent invocation names consist of account/sequence, both u64. Claim
        // authenticates that exact invocation before storing its call id.
        let largest = Marker {
            publication: Publication::Delivered { digest: [255; 32] },
            decision: Decision::Completed {
                call: CallId {
                    requester: self.agent.clone(),
                    invocation: format!("{}/{}", u64::MAX, u64::MAX),
                    step: u64::MAX,
                },
            },
        };
        let capacity = 8 + sdk::wire::encode(&largest).len();
        let marker = Marker {
            publication: Publication::Queued,
            decision: Decision::Awaiting,
        };
        self.receipts
            .stage(body_key(&id), sdk::wire::encode(&record))?;
        self.receipts
            .stage(marker_key(&id), marker_bytes(&marker, capacity)?)?;
        let mut queue = self.action_queue(View::Live).await?;
        if let Some(tail) = queue.tail {
            let mut previous = self.action_queue_item(tail, View::Live).await?;
            previous.next = Some(item);
            self.receipts
                .stage(item_key(tail), sdk::wire::encode(&previous))?;
        } else {
            queue.head = Some(item);
        }
        queue.tail = Some(item);
        self.receipts.stage(
            item_key(item),
            sdk::wire::encode(&QueueItem {
                request_id: id.clone(),
                next: None,
            }),
        )?;
        self.receipts
            .stage(QUEUE_KEY.into(), sdk::wire::encode(&queue))?;
        self.staged_next_action_item = Some(next);
        // the one writer of a run's actions is the one writer of its `Acted`
        // facts: whatever lane admitted the request, the journal carries it.
        self.conversation_track_action(None, &entry.run_id, id.clone())
            .await?;
        self.record(
            &entry.run_id,
            RunFact::Acted {
                request_id: id,
                lane,
                operation: record.view.operation.clone(),
                result: record.view.result.clone(),
            },
        );
        Ok(())
    }

    pub(super) async fn action_deliveries(&self) -> Result<Vec<PendingItem>, Error> {
        let mut next = self.action_queue(View::Committed).await?.head;
        let mut deliveries = Vec::new();
        while let Some(item) = next {
            if deliveries.len() == sdk::MAX_DELIVERIES_PER_BLOCK {
                break;
            }
            let queued = self.action_queue_item(item, View::Committed).await?;
            let bytes = self
                .receipts
                .committed(&body_key(&queued.request_id))
                .await?
                .ok_or_else(|| Error::Module("queued action has no body".into()))?;
            let request: ActionRequest = sdk::wire::decode(&bytes).map_err(Error::Module)?;
            let reference = sdk::ItemRef {
                source: self.id.clone(),
                item,
            };
            deliveries.push(PendingItem {
                item,
                target: self.id.clone(),
                payload: encode_msg(&RunsMsg::PublishActionRequest {
                    request_id: queued.request_id,
                }),
                cause: Cause::Chain {
                    root: request.cause.root_for_item(&reference),
                    hop: Hop::Delivery(reference),
                },
            });
            next = queued.next;
        }
        Ok(deliveries)
    }

    pub(super) async fn acknowledge_action(
        &mut self,
        ctx: &dyn Ctx,
        ack: &Ack,
    ) -> Result<(), Error> {
        let authentic = ctx.env().origin == Origin::System && ack.target == self.id;
        if !authentic {
            return Err(Error::Module(
                "action acknowledgment requires the host finalizer".into(),
            ));
        }
        let queued = self.action_queue_item(ack.item, View::Live).await?;
        let mut request = self
            .action_request(&queued.request_id)
            .await?
            .ok_or_else(|| Error::Module("unknown action delivery".into()))?;
        let digest: [u8; 32] = Sha256::digest(sdk::wire::encode(&ack.outcome)).into();
        if let Publication::Delivered { digest: previous } = request.publication {
            if previous == digest {
                return Ok(());
            }
            return Err(Error::Module(
                "conflicting action delivery acknowledgment".into(),
            ));
        }
        let mut queue = self.action_queue(View::Live).await?;
        if queue.head != Some(ack.item) {
            return Err(Error::Module(
                "action acknowledgment is not the queue head".into(),
            ));
        }
        queue.head = queued.next;
        if queue.head.is_none() {
            queue.tail = None;
        }
        request.publication = Publication::Delivered { digest };
        match &ack.outcome {
            DeliveryOutcome::Applied => {}
            DeliveryOutcome::Failed { reason } => {
                request.view.status = ActionStatus::Rejected {
                    reason: reason.clone(),
                }
            }
            DeliveryOutcome::Unrepresentable => {
                request.view.status = ActionStatus::Rejected {
                    reason: "action publication was not representable".into(),
                }
            }
        }
        self.stage_action_marker(&request).await?;
        self.wake_conversation_for_run(ctx, &request.view.run_id)
            .await?;
        self.receipts
            .stage(QUEUE_KEY.into(), sdk::wire::encode(&queue))
    }
}
