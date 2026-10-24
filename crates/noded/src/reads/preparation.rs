use super::*;
use abi::HashKind;
use abi::role::reads::{Preparation, PreparationRow, Prepared, Product};

pub(super) fn ask<E: Context>(
    daemon: &Arc<Daemon<E>>,
    id: u64,
    row: PreparationRow,
    answers: mpsc::UnboundedSender<Answered>,
) -> Handle<()> {
    let reader = daemon.clone();
    daemon.context.child("preparation").spawn(move |_| async move {
        let answered = match reader.view() {
            Ok(view) => {
                let at = view.tip().height;
                match offload(move || prepare(row.preparation)).await {
                    Ok(Ok((product, framed))) => Some((Op::PreparedAnswer { id, at, outcome: Ok(product) }, Some(framed))),
                    Ok(Err(refusal)) => Some((Op::PreparedAnswer { id, at, outcome: Err(refusal) }, None)),
                    Err(error) => {
                        tracing::warn!(target: TARGET, event = "preparation_unrun", id, reason = "node_failed", %error);
                        None
                    }
                }
            }
            Err(error) => {
                tracing::warn!(target: TARGET, event = "preparation_unread", id, reason = "node_failed", %error);
                None
            }
        };
        let _ = answers.unbounded_send((id, answered));
    })
}

fn entry(
    hash: HashKind,
    kind: &str,
    body: &[u8],
    framed: &mut Vec<u8>,
) -> std::result::Result<Entry, Refusal> {
    let bytes = blob::frame(kind, body)?;
    let id = blob::id_of(hash, &bytes).map_err(Refusal::from)?;
    let entry = Entry {
        id,
        len: bytes.len() as u64,
        kind: kind.to_owned(),
    };
    framed.extend_from_slice(&bytes);
    Ok(entry)
}

fn prepare(request: Preparation) -> std::result::Result<(Product, Framed), Refusal> {
    let chunk_bytes = usize::try_from(request.chunk_bytes).map_err(|_| {
        Refusal::new(
            reason::INVALID_INPUT,
            "chunk size is not addressable on this worker",
        )
    })?;
    if chunk_bytes == 0 {
        return Err(Refusal::new(reason::INVALID_INPUT, "chunk size is zero"));
    }
    if !blob::is_kind(&request.kind) {
        return Err(Refusal::new(
            reason::INVALID_INPUT,
            "invalid preparation blob kind",
        ));
    }
    let mut framed = Vec::new();
    let chunks = request
        .body
        .chunks(chunk_bytes)
        .map(|body| entry(request.hash, &request.kind, body, &mut framed))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let prepared = Prepared {
        size: request.body.len() as u64,
        chunks: chunks.clone(),
    };
    let reply = entry(
        HashKind::Sha256,
        reads::KIND,
        &abi::encode(&prepared),
        &mut framed,
    )?;
    let entries = chunks
        .iter()
        .chain(std::iter::once(&reply))
        .map(|entry| blob::contract::Entry {
            id: entry.id,
            len: entry.len,
            kind: entry.kind.clone(),
        })
        .collect();
    Ok((
        Product {
            reply,
            produced: chunks,
        },
        Framed { entries, framed },
    ))
}

#[cfg(test)]
mod tests;
