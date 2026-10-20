// The `/v1/watch` socket: one program's changes and the client's standing queries. It pulls what
// the blocks since its last message wrote, merged, only once it can send, and runs again at that
// view each standing query whose read set those writes touched, sending what moved with the block.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use abi::{Origin, ProgramId, Refusal, reason};
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, close_code};
use commonware_cryptography::{Hasher as _, Sha256, sha256};
use futures::stream::SplitSink;
use futures::{SinkExt as _, StreamExt as _, pin_mut};
use host::{ReadSet, Submission, Written};

use crate::changes::{Merged, Pull};
use crate::view::View;
use crate::wire::{Outcome, Pushed, Watch};
use crate::{Context, Daemon, offload};

pub(crate) struct Watching<E: Context> {
    pub(crate) pull: Pull<E>,
    pub(crate) daemon: Arc<Daemon<E>>,
}

#[derive(Clone)]
struct Asked {
    signer: Vec<u8>,
    target: ProgramId,
    payload: Vec<u8>,
}

impl From<Submission> for Asked {
    fn from(submission: Submission) -> Asked {
        Asked {
            signer: submission.signer,
            target: submission.target,
            payload: submission.payload,
        }
    }
}

struct Standing {
    asked: Asked,
    reads: ReadSet,
    at: u64,
    sent: Option<sha256::Digest>,
}

enum Ran {
    Answered(Vec<u8>, ReadSet),
    Refused(Refusal, ReadSet),
    Moved(Option<ReadSet>),
}

enum Next<E: Context> {
    Pulled(Merged<E>),
    Ask { id: u32, frame: Vec<u8> },
    Drop(u32),
    Pong(Bytes),
    Pinged,
    Malformed,
    Stopping,
    Stopped,
    Ended,
}

enum Flow {
    Watching,
    Pulled,
    Stopped,
    Ended,
}

struct Socket<E: Context> {
    client: SplitSink<WebSocket, Message>,
    pull: Pull<E>,
    daemon: Arc<Daemon<E>>,
    standing: BTreeMap<u32, Standing>,
    stopping: bool,
}

pub(crate) async fn serve<E: Context>(socket: WebSocket, watching: Watching<E>) {
    let Watching { pull, daemon } = watching;
    let (client, mut frames) = socket.split();
    let mut pulling = pull.next();
    let mut socket = Socket {
        client,
        pull,
        daemon,
        standing: BTreeMap::new(),
        stopping: false,
    };
    loop {
        let next = tokio::select! {
            dropped = &mut socket.pull.cut, if !socket.stopping => match dropped {
                Ok(()) => Next::Stopping,
                Err(_) => Next::Ended,
            },
            merged = &mut pulling => merged.map_or(Next::Stopped, Next::Pulled),
            frame = frames.next() => heard(frame),
        };
        let flow = match next {
            Next::Pulled(merged) => socket.pulled(merged).await,
            Next::Ask { id, frame } => socket.ask(id, frame).await,
            Next::Drop(id) => socket.dropped(id),
            Next::Pong(echo) => socket.pong(&echo),
            Next::Pinged => Flow::Watching,
            Next::Malformed => socket.malformed().await,
            Next::Stopping => socket.stop(),
            Next::Stopped => Flow::Stopped,
            Next::Ended => Flow::Ended,
        };
        match flow {
            Flow::Watching => {}
            Flow::Pulled => pulling = socket.pull.next(),
            Flow::Stopped => return crate::server::goodbye(socket.client).await,
            Flow::Ended => return,
        }
    }
}

impl<E: Context> Socket<E> {
    async fn pulled(&mut self, merged: Merged<E>) -> Flow {
        let Merged {
            height,
            root,
            view,
            writes,
            written,
            closes,
        } = merged;
        let due: Vec<(u32, Asked)> = self
            .standing
            .iter()
            .filter(|(_, standing)| standing.moved_by(height, &written))
            .map(|(id, standing)| (*id, standing.asked.clone()))
            .collect();
        let time = self.daemon.now();
        let ran = rerun(height, view, due, time).await;
        let answers: Vec<(u32, Outcome)> = ran
            .into_iter()
            .filter_map(|(id, ran)| self.land(id, height, ran))
            .collect();
        let writes: Vec<(Vec<u8>, Option<Vec<u8>>)> = writes.into_iter().collect();
        let quiet = writes.is_empty() && answers.is_empty();
        if quiet {
            return pulled_on(self.send(ping(height)).await);
        }
        let block = Pushed::Block {
            height,
            root,
            writes,
            answers,
        };
        let sent = self.send(binary(&block)).await;
        let pings_too = closes && matches!(sent, Flow::Watching);
        if !pings_too {
            return pulled_on(sent);
        }
        pulled_on(self.send(ping(height)).await)
    }

    fn land(&mut self, id: u32, height: u64, ran: Ran) -> Option<(u32, Outcome)> {
        let standing = self.standing.get_mut(&id)?;
        let outcome = standing.landed(height, ran)?;
        Some((id, outcome))
    }

    async fn ask(&mut self, id: u32, frame: Vec<u8>) -> Flow {
        self.standing.remove(&id);
        let Ok(view) = self.daemon.view() else {
            return Flow::Stopped;
        };
        let height = view.tip().height;
        let network = self.daemon.descriptor.id();
        let time = self.daemon.now();
        let answered = offload(move || asked_first(&view, &network, time, &frame)).await;
        let outcome = match answered {
            Ok(Ok((asked, ran))) => self.stand(id, height, asked, ran),
            Ok(Err(refusal)) => Outcome::Refused(refusal),
            Err(_) => Outcome::Refused(unserved()),
        };
        let answer = Pushed::Answer {
            id,
            height,
            outcome,
        };
        self.send(binary(&answer)).await
    }

    fn stand(&mut self, id: u32, height: u64, asked: Asked, ran: Ran) -> Outcome {
        if let Ran::Refused(refusal, _) = ran {
            return Outcome::Refused(refusal);
        }
        let mut standing = Standing {
            asked,
            reads: ReadSet::default(),
            at: height,
            sent: None,
        };
        let outcome = standing.landed(height, ran).unwrap_or(Outcome::Moved);
        self.standing.insert(id, standing);
        outcome
    }

    fn dropped(&mut self, id: u32) -> Flow {
        self.standing.remove(&id);
        Flow::Watching
    }

    fn pong(&self, echo: &[u8]) -> Flow {
        if let Ok(height) = <[u8; 8]>::try_from(echo) {
            self.pull
                .read
                .fetch_max(u64::from_le_bytes(height), Ordering::Relaxed);
        }
        Flow::Watching
    }

    fn stop(&mut self) -> Flow {
        self.stopping = true;
        Flow::Watching
    }

    async fn malformed(&mut self) -> Flow {
        let refused = CloseFrame {
            code: close_code::PROTOCOL,
            reason: "a frame that is not a Watch".into(),
        };
        self.send(Message::Close(Some(refused))).await;
        Flow::Ended
    }

    async fn send(&mut self, message: Message) -> Flow {
        let send = self.client.send(message);
        pin_mut!(send);
        loop {
            tokio::select! {
                sent = &mut send => return sent.map_or(Flow::Ended, |()| Flow::Watching),
                dropped = &mut self.pull.cut, if !self.stopping => match dropped {
                    Ok(()) => self.stopping = true,
                    Err(_) => return Flow::Ended,
                },
            }
        }
    }
}

async fn rerun<E: Context>(
    height: u64,
    view: Option<Arc<View<E>>>,
    due: Vec<(u32, Asked)>,
    time: u64,
) -> Vec<(u32, Ran)> {
    let ids: Vec<u32> = due.iter().map(|(id, _)| *id).collect();
    let runnable = view.filter(|_| !due.is_empty());
    let Some(view) = runnable else {
        return moved(&ids);
    };
    let reran = offload(move || {
        let started = Instant::now();
        let ran: Vec<(u32, Ran)> = due
            .into_iter()
            .map(|(id, asked)| (id, run(&view, time, &asked)))
            .collect();
        (ran, started.elapsed())
    })
    .await;
    let Ok((ran, held)) = reran else {
        return moved(&ids);
    };
    tracing::debug!(
        target: "ducktape::http",
        event = "watch_reran",
        height,
        count = ids.len() as u64,
        held_ms = held.as_millis() as u64,
        "standing queries ran again at a block's view"
    );
    ran
}

impl Standing {
    fn moved_by(&self, height: u64, written: &Written) -> bool {
        let older = self.at < height;
        older && self.reads.touched(written)
    }

    fn landed(&mut self, height: u64, ran: Ran) -> Option<Outcome> {
        self.at = height;
        match ran {
            Ran::Answered(bytes, reads) => self.answered(bytes, reads),
            Ran::Refused(refusal, reads) => self.refused(refusal, reads),
            Ran::Moved(reads) => self.moved(reads),
        }
    }

    fn answered(&mut self, bytes: Vec<u8>, reads: ReadSet) -> Option<Outcome> {
        self.reads = reads;
        let digest = Sha256::hash(&[bytes.as_slice()]);
        let unchanged = self.sent.as_ref() == Some(&digest);
        self.sent = Some(digest);
        (!unchanged).then_some(Outcome::Answered(bytes))
    }

    fn refused(&mut self, refusal: Refusal, reads: ReadSet) -> Option<Outcome> {
        self.reads = reads;
        self.sent = None;
        Some(Outcome::Refused(refusal))
    }

    fn moved(&mut self, reads: Option<ReadSet>) -> Option<Outcome> {
        self.reads.union(widened(reads, &self.asked.target));
        self.sent = None;
        Some(Outcome::Moved)
    }
}

fn heard<E: Context>(frame: Option<Result<Message, axum::Error>>) -> Next<E> {
    match frame {
        Some(Ok(Message::Binary(bytes))) => decoded(&bytes),
        Some(Ok(Message::Pong(echo))) => Next::Pong(echo),
        Some(Ok(Message::Ping(_))) => Next::Pinged,
        Some(Ok(Message::Text(_))) => Next::Malformed,
        Some(Ok(Message::Close(_)) | Err(_)) | None => Next::Ended,
    }
}

fn decoded<E: Context>(bytes: &[u8]) -> Next<E> {
    match abi::decode::<Watch>(bytes) {
        Ok(Watch::Ask { id, frame }) => Next::Ask { id, frame },
        Ok(Watch::Drop { id }) => Next::Drop(id),
        Err(_) => Next::Malformed,
    }
}

fn asked_first<E: Context>(
    view: &View<E>,
    network: &[u8],
    time: u64,
    frame: &[u8],
) -> Result<(Asked, Ran), Refusal> {
    let submission = node::verify(frame, network)?;
    let fresh = view.source().fresh(&submission);
    let asked = Asked::from(submission);
    let ran = match fresh {
        Ok(Ok(())) => run(view, time, &asked),
        Ok(Err(refusal)) => return Err(refusal),
        Err(_) => Ran::Moved(None),
    };
    Ok((asked, ran))
}

fn run<E: Context>(view: &View<E>, time: u64, asked: &Asked) -> Ran {
    let source = view.source();
    let origin = Origin::External(asked.signer.clone());
    let answer = source.watch(time, origin, &asked.target, asked.payload.clone());
    match tokio::runtime::Handle::current().block_on(answer) {
        Ok((Ok(bytes), reads)) => Ran::Answered(bytes, reads),
        Ok((Err(refusal), reads)) => ran_refused(refusal, reads),
        Err(error) => unrun(&error),
    }
}

fn ran_refused(refusal: Refusal, reads: ReadSet) -> Ran {
    let out_of_fuel = refusal.reason == reason::TRAP && refusal.sentence == host::OUT_OF_FUEL;
    match out_of_fuel {
        true => Ran::Moved(Some(reads)),
        false => Ran::Refused(refusal, reads),
    }
}

fn unrun(error: &host::Error) -> Ran {
    tracing::debug!(
        target: "ducktape::http",
        event = "watch_moved",
        %error,
        reason = "host_error",
        "a standing query could not run, so its client asks it itself"
    );
    Ran::Moved(None)
}

fn widened(reads: Option<ReadSet>, target: &str) -> ReadSet {
    let mut reads = reads.unwrap_or_default();
    reads.whole(target);
    reads.point(host::PROGRAMS, target.as_bytes());
    reads
}

fn moved(ids: &[u32]) -> Vec<(u32, Ran)> {
    ids.iter().map(|id| (*id, Ran::Moved(None))).collect()
}

fn unserved() -> Refusal {
    Refusal::new(reason::UNSUPPORTED, "this node cannot serve the request")
}

fn pulled_on(flow: Flow) -> Flow {
    match flow {
        Flow::Watching | Flow::Pulled => Flow::Pulled,
        Flow::Stopped => Flow::Stopped,
        Flow::Ended => Flow::Ended,
    }
}

fn binary(pushed: &Pushed) -> Message {
    Message::Binary(abi::encode(pushed).into())
}

fn ping(height: u64) -> Message {
    Message::Ping(Bytes::copy_from_slice(&height.to_le_bytes()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use abi::{Refusal, reason};
    use host::{ReadSet, Written};

    use super::{Asked, Ran, Standing, ran_refused};
    use crate::wire::Outcome;

    fn standing() -> Standing {
        Standing {
            asked: Asked {
                signer: Vec::new(),
                target: "probe".into(),
                payload: Vec::new(),
            },
            reads: ReadSet::default(),
            at: 1,
            sent: None,
        }
    }

    fn wrote(program: &str, key: &[u8]) -> Written {
        Written::from([(program.to_owned(), BTreeSet::from([key.to_vec()]))])
    }

    fn answered(bytes: &[u8]) -> Ran {
        Ran::Answered(bytes.to_vec(), ReadSet::default())
    }

    #[test]
    fn a_rerun_sends_its_answer_only_when_the_bytes_change() {
        let mut standing = standing();
        let first = standing.landed(2, answered(b"x"));
        assert_eq!(first, Some(Outcome::Answered(b"x".to_vec())));
        assert_eq!(standing.landed(3, answered(b"x")), None);
        let other = standing.landed(4, answered(b"y"));
        assert_eq!(other, Some(Outcome::Answered(b"y".to_vec())));
    }

    #[test]
    fn a_rerun_out_of_fuel_is_moved_and_runs_again_on_any_later_write_to_its_program_or_code() {
        let mut standing = standing();
        let spent = Refusal::new(reason::TRAP, host::OUT_OF_FUEL);
        let ran = ran_refused(spent, ReadSet::default());
        assert_eq!(standing.landed(2, ran), Some(Outcome::Moved));
        assert!(!standing.moved_by(2, &wrote("probe", b"any")));
        assert!(standing.moved_by(3, &wrote("probe", b"any")));
        assert!(standing.moved_by(3, &wrote(host::PROGRAMS, b"probe")));
        assert!(!standing.moved_by(3, &wrote("other", b"any")));
        let refused = ran_refused(Refusal::new("probe", "no"), ReadSet::default());
        let landed = standing.landed(3, refused);
        assert!(matches!(landed, Some(Outcome::Refused(_))));
    }
}
