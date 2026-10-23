// Signed and unsigned queries, checked against the newest applied anchor before selecting a read layer.

use abi::role::validators::{self, QueryPolicy};
use abi::{Origin, ProgramId, Refusal, reason, role};
use host::{Layer, Source, Submission};
use tokio::sync::watch;

use crate::view::{Current, Reading};
use crate::wire::{Ask, Query};
use crate::{Context, offload};

enum VerifiedAsk {
    Signed(Submission),
    Unsigned { target: ProgramId, payload: Vec<u8> },
}

struct Asking {
    origin: Origin,
    target: ProgramId,
    payload: Vec<u8>,
}

pub(crate) async fn answer<E: Context>(
    current: &Current<E>,
    tips: watch::Receiver<u64>,
    finalized: &watch::Receiver<u64>,
    query: Query,
    network: Vec<u8>,
    now: impl FnOnce() -> u64 + Send,
) -> crate::Result<Result<(ProgramId, host::Queried), Refusal>> {
    let layer = query.layer;
    let asking = match query.ask {
        Ask::Signed(frame) => {
            let submission = match offload(move || node::verify_query(&frame, &network)).await? {
                Ok(submission) => submission,
                Err(refusal) => return Ok(Err(refusal)),
            };
            VerifiedAsk::Signed(submission)
        }
        Ask::Unsigned { target, payload } => VerifiedAsk::Unsigned { target, payload },
    };
    let newest = match &asking {
        VerifiedAsk::Signed(submission) => {
            current
                .for_anchor(tips, finalized, submission.anchor.height)
                .await?
        }
        VerifiedAsk::Unsigned { .. } => current.view()?,
    };
    let reading = match layer {
        Layer::Confirmed => Reading::Confirmed(newest.clone()),
        Layer::Preconfirmed => current.reading(layer)?,
    };
    let time = now();
    offload(move || {
        let source = reading.source();
        let anchors = newest.source();
        tokio::runtime::Handle::current().block_on(asked(&source, &anchors, layer, asking, time))
    })
    .await?
    .map_err(crate::Error::from)
}

async fn asked<E: Context>(
    source: &Source<'_, E>,
    newest: &Source<'_, E>,
    layer: Layer,
    ask: VerifiedAsk,
    time: u64,
) -> host::Result<Result<(ProgramId, host::Queried), Refusal>> {
    let asking = match ask {
        VerifiedAsk::Signed(submission) => signed_query(newest, submission)?,
        VerifiedAsk::Unsigned { target, payload } => {
            unsigned_query(source, layer, time, target, payload).await?
        }
    };
    match asking {
        Ok(Asking {
            origin,
            target,
            payload,
        }) => {
            let queried = source
                .queried(layer, time, origin, &target, payload)
                .await?;
            Ok(Ok((target, queried)))
        }
        Err(refusal) => Ok(Err(refusal)),
    }
}

fn signed_query<E: Context>(
    source: &Source<'_, E>,
    submission: Submission,
) -> host::Result<Result<Asking, Refusal>> {
    if let Err(refusal) = source.fresh(&submission)? {
        return Ok(Err(refusal));
    }
    Ok(Ok(Asking {
        origin: Origin::External(submission.signer),
        target: submission.target,
        payload: submission.payload,
    }))
}

async fn unsigned_query<E: Context>(
    source: &Source<'_, E>,
    layer: Layer,
    time: u64,
    target: ProgramId,
    payload: Vec<u8>,
) -> host::Result<Result<Asking, Refusal>> {
    let policy = match query_policy(source, layer, time).await? {
        Ok(policy) => policy,
        Err(refusal) => return Ok(Err(refusal)),
    };
    match policy {
        QueryPolicy::Open => Ok(Ok(Asking {
            origin: Origin::unsigned(),
            target,
            payload,
        })),
        QueryPolicy::Signed => Ok(Err(Refusal::new(
            reason::UNAUTHORIZED,
            "this network answers only signed queries",
        ))),
    }
}

fn unanswered(refusal: Refusal) -> Refusal {
    Refusal::new(
        reason::UNAUTHORIZED,
        format!(
            "the validators program does not answer QueryPolicy, so this network answers only \
             signed queries: {}: {}",
            refusal.reason, refusal.sentence
        ),
    )
}

async fn query_policy<E: Context>(
    source: &Source<'_, E>,
    layer: Layer,
    time: u64,
) -> host::Result<Result<QueryPolicy, Refusal>> {
    let bound = source.role(role::VALIDATORS)?;
    let asked = abi::encode(&validators::Query::QueryPolicy);
    let answer = source
        .query(layer, time, Origin::System, bound, asked)
        .await?;
    Ok(answer
        .map_err(unanswered)
        .and_then(|bytes| match abi::decode(&bytes)? {
            validators::Reply::QueryPolicy(policy) => Ok(policy),
            validators::Reply::Validators(_)
            | validators::Reply::Members(_)
            | validators::Reply::HaloCleared(_) => Err(Refusal::new(
                reason::INVALID_INPUT,
                "the validators program answered QueryPolicy with another reply",
            )),
        }))
}

#[cfg(test)]
mod tests;
