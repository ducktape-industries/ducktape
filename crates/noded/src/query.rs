// What /v1/query answers: a signed ask verified under the query namespace and fresh at its anchor,
// or an unsigned one while the validators role's QueryPolicy is Open, run as the empty key.

use abi::role::validators::{self, QueryPolicy};
use abi::{Origin, ProgramId, Refusal, reason, role};
use host::{Layer, Source};

use crate::Context;
use crate::wire::{Ask, Query};

struct Asking {
    origin: Origin,
    target: ProgramId,
    payload: Vec<u8>,
}

pub(crate) async fn answer<E: Context>(
    source: &Source<'_, E>,
    newest: &Source<'_, E>,
    query: Query,
    network: &[u8],
    time: u64,
) -> host::Result<Result<(ProgramId, host::Queried), Refusal>> {
    let asking = match query.ask {
        Ask::Signed(frame) => signed_query(newest, &frame, network)?,
        Ask::Unsigned { target, payload } => {
            unsigned_query(source, query.layer, time, target, payload).await?
        }
    };
    match asking {
        Ok(Asking {
            origin,
            target,
            payload,
        }) => {
            let queried = source
                .queried(query.layer, time, origin, &target, payload)
                .await?;
            Ok(Ok((target, queried)))
        }
        Err(refusal) => Ok(Err(refusal)),
    }
}

fn signed_query<E: Context>(
    source: &Source<'_, E>,
    frame: &[u8],
    network: &[u8],
) -> host::Result<Result<Asking, Refusal>> {
    let submission = match node::verify_query(frame, network) {
        Ok(submission) => submission,
        Err(refusal) => return Ok(Err(refusal)),
    };
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
            validators::Reply::Validators(_) | validators::Reply::Members(_) => Err(Refusal::new(
                reason::INVALID_INPUT,
                "the validators program answered QueryPolicy with another reply",
            )),
        }))
}

#[cfg(test)]
mod tests {
    use abi::role::validators;
    use host::Layer;
    use node::Frame;

    use super::answer;
    use crate::view::tests::{NETWORK, TIME, founded};
    use crate::wire::{Ask, Query};

    #[test]
    fn a_query_anchored_at_the_newest_tip_is_answered_over_a_layer_a_block_behind() {
        founded(|founded| async move {
            let asked = abi::encode(&validators::Query::QueryPolicy);
            let anchor = founded.next.tip();
            let frame = Frame::sign_query(&founded.key, NETWORK, anchor, "valset", asked);
            let query = Query {
                layer: Layer::Preconfirmed,
                ask: Ask::Signed(frame.encode()),
            };
            let behind = founded.first.source();
            let newest = founded.next.source();
            match answer(&behind, &newest, query, NETWORK, TIME).await {
                Ok(Ok(_)) => {}
                Ok(Err(refusal)) => panic!("refused: {}: {}", refusal.reason, refusal.sentence),
                Err(error) => panic!("the query failed: {error}"),
            }
        });
    }
}
