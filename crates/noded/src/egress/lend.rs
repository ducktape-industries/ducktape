//! Lending a service this node fronts to another network's node, and borrowing one: the borrower
//! opens one websocket to the lender and proves its node key; the lender asks access `Writers` on
//! egress `lend/<service>` before each request and calls its own base, so its secrets stay with it.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use abi::role::access::{self, Party};
use abi::role::egress::http::Head;
use abi::role::egress::{Answer, Delivery, RESERVED, Request, StreamEnd};
use abi::role::identity::{self, AccountNumber};
use abi::{Origin, Refusal, reason, role};
use axum::extract::ws::{Message, WebSocket};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_cryptography::ed25519;
use commonware_runtime::telemetry::metrics::{CounterFamily, EncodeLabelSet, raw};
use futures::channel::mpsc;
use futures::{SinkExt as _, Stream, StreamExt as _};
use host::Layer;
use node::Frame;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use url::Url;

use crate::egress::TARGET;
use crate::egress::call::{self, Caller};
use crate::egress::desk::Called;
use crate::egress::station::Serving;
use crate::wire::lend::{Answered, Ask, Down, Up};
use crate::wire::{LEND, route};
use crate::{Context, Daemon};

const BORROWER: &str = "ducktape-borrower";

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct Loan {
    account: String,
    service: String,
}

type Waiting = Mutex<Option<BTreeMap<u64, mpsc::UnboundedSender<Down>>>>;

type Slot = Arc<tokio::sync::Mutex<Option<Arc<Session>>>>;

struct Session {
    up: mpsc::UnboundedSender<Up>,
    waiting: Arc<Waiting>,
}

impl Session {
    fn open(&self) -> bool {
        !self.up.is_closed() && lock(&self.waiting).is_some()
    }

    fn wait(&self, id: u64, heard: mpsc::UnboundedSender<Down>) -> bool {
        let mut waiting = lock(&self.waiting);
        let Some(waiting) = waiting.as_mut() else {
            return false;
        };
        waiting.insert(id, heard);
        true
    }
}

pub struct Lending {
    key: ed25519::PrivateKey,
    sessions: Mutex<BTreeMap<Url, Slot>>,
    requests: CounterFamily<Loan>,
    sent: CounterFamily<Loan>,
}

impl Lending {
    pub fn new<E: Context>(context: &E, key: ed25519::PrivateKey) -> Lending {
        Lending {
            key,
            sessions: Mutex::default(),
            requests: context.register(
                "egress_lent_requests",
                "requests this node relayed for a borrowing account, by account and service",
                raw::Family::default(),
            ),
            sent: context.register(
                "egress_lent_bytes",
                "answer bytes this node relayed to a borrowing account, by account and service",
                raw::Family::default(),
            ),
        }
    }

    pub async fn borrow(
        &self,
        lender: &Url,
        caller: &Caller<'_>,
        request: &Request,
        delivery: &Delivery,
        body: Vec<u8>,
        mut event: impl FnMut(Vec<u8>),
    ) -> Called {
        let Request::Http { service, head, .. } = request else {
            return Called::Whole(Answer::Failed("a relay is never borrowed".to_owned()));
        };
        let session = match self.session(lender).await {
            Ok(session) => session,
            Err(reason) => {
                return Called::Whole(Answer::Failed(format!(
                    "the lending node at {lender} did not open a session: {reason}"
                )));
            }
        };
        let ended = format!("the session with the lending node at {lender} ended");
        let (heard, mut hearing) = mpsc::unbounded();
        if !session.wait(caller.id, heard) {
            return Called::Whole(Answer::Failed(ended));
        }
        let ask = Ask {
            id: caller.id,
            chain: caller.chain.to_owned(),
            program: caller.program.to_owned(),
            service: service.clone(),
            head: head.clone(),
            delivery: delivery.clone(),
            body,
        };
        if session.up.unbounded_send(Up::Ask(ask)).is_err() {
            return Called::Whole(Answer::Failed(ended));
        }
        let mut events: u64 = 0;
        while let Some(down) = hearing.next().await {
            match down {
                Down::Event { data, .. } => {
                    events += 1;
                    event(data);
                }
                Down::Answered {
                    answered: Answered::Whole(answer),
                    ..
                } => return Called::Whole(answer),
                Down::Answered {
                    answered: Answered::End(end),
                    ..
                } => return Called::End(end),
                Down::Challenge { .. } | Down::Welcome { .. } | Down::Refused(_) => {}
            }
        }
        match events {
            0 => Called::Whole(Answer::Failed(ended)),
            _ => Called::End(StreamEnd::Cut {
                reason: "lend_session_cut".to_owned(),
            }),
        }
    }

    async fn session(&self, lender: &Url) -> Result<Arc<Session>, String> {
        let slot = lock(&self.sessions)
            .entry(lender.clone())
            .or_default()
            .clone();
        let mut held = slot.lock().await;
        if let Some(session) = held.as_ref().filter(|session| session.open()) {
            return Ok(session.clone());
        }
        let session = Arc::new(open(lender, &self.key).await?);
        *held = Some(session.clone());
        Ok(session)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn open(lender: &Url, key: &ed25519::PrivateKey) -> Result<Session, String> {
    let mut address = lender.clone();
    let scheme = match lender.scheme() {
        "https" => "wss",
        _ => "ws",
    };
    address
        .set_scheme(scheme)
        .map_err(|()| format!("{lender} has no websocket address"))?;
    address.set_path(route::EGRESS_LEND);
    let unbounded = WebSocketConfig {
        max_message_size: None,
        max_frame_size: None,
        ..WebSocketConfig::default()
    };
    let (socket, _) =
        tokio_tungstenite::connect_async_with_config(address.as_str(), Some(unbounded), false)
            .await
            .map_err(|error| error.to_string())?;
    let (mut sink, mut stream) = socket.split();
    let Down::Challenge {
        network,
        anchor,
        nonce,
    } = heard(&mut stream).await?
    else {
        return Err("the lending node did not open with a challenge".to_owned());
    };
    let proof = Frame::sign(key, &network, anchor, None, LEND, nonce.to_vec()).encode();
    sink.send(binary(&Up::Proof(proof)))
        .await
        .map_err(|error| error.to_string())?;
    match heard(&mut stream).await? {
        Down::Welcome { account } => tracing::info!(
            target: TARGET,
            event = "egress_borrowing",
            lender = %lender.origin().ascii_serialization(),
            account,
            "this node borrows from a lending node as its account there"
        ),
        Down::Refused(refusal) => return Err(refusal.sentence),
        other => {
            return Err(format!(
                "the lending node answered the proof with {other:?}"
            ));
        }
    }
    let (up, mut outgoing) = mpsc::unbounded::<Up>();
    tokio::spawn(async move {
        while let Some(up) = outgoing.next().await {
            if sink.send(binary(&up)).await.is_err() {
                return;
            }
        }
    });
    let waiting = Arc::new(Mutex::new(Some(BTreeMap::new())));
    let routes = waiting.clone();
    tokio::spawn(async move {
        while let Ok(down) = heard(&mut stream).await {
            route(&routes, down);
        }
        lock(&routes).take();
    });
    Ok(Session { up, waiting })
}

fn binary(message: &impl BorshSerialize) -> tungstenite::Message {
    tungstenite::Message::Binary(abi::encode(message))
}

async fn heard<S>(stream: &mut S) -> Result<Down, String>
where
    S: Stream<Item = Result<tungstenite::Message, tungstenite::Error>> + Unpin,
{
    loop {
        match stream.next().await {
            Some(Ok(tungstenite::Message::Binary(bytes))) => {
                return abi::decode(&bytes).map_err(|refusal| refusal.sentence);
            }
            Some(Ok(tungstenite::Message::Close(_))) | None => {
                return Err("the lending node closed the session".to_owned());
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(error.to_string()),
        }
    }
}

fn route(waiting: &Waiting, down: Down) {
    let mut waiting = lock(waiting);
    let Some(waiting) = waiting.as_mut() else {
        return;
    };
    let heard = match &down {
        Down::Event { id, .. } => waiting.get(id).cloned(),
        Down::Answered { id, .. } => waiting.remove(id),
        Down::Challenge { .. } | Down::Welcome { .. } | Down::Refused(_) => None,
    };
    if let Some(heard) = heard {
        let _ = heard.unbounded_send(down);
    }
}

pub async fn lend_to<E: Context>(daemon: Arc<Daemon<E>>, socket: WebSocket) {
    let (mut sink, mut inbound) = socket.split();
    let (down, mut outgoing) = mpsc::unbounded::<Down>();
    let writer = tokio::spawn(async move {
        while let Some(message) = outgoing.next().await {
            let bytes = abi::encode(&message);
            if sink.send(Message::Binary(bytes.into())).await.is_err() {
                return;
            }
        }
    });
    let account = match proven(&daemon, &down, &mut inbound).await {
        Ok(account) => account,
        Err(refusal) => {
            tracing::debug!(
                target: TARGET,
                event = "egress_lend_refused",
                reason = %refusal.reason,
                sentence = %refusal.sentence,
                "a borrowing node was refused a session"
            );
            let _ = down.unbounded_send(Down::Refused(refusal));
            drop(down);
            let _ = writer.await;
            return;
        }
    };
    let _ = down.unbounded_send(Down::Welcome { account });
    tracing::info!(
        target: TARGET,
        event = "egress_lending",
        account,
        "a borrowing node opened a session"
    );
    let mut relays = tokio::task::JoinSet::new();
    loop {
        let message = tokio::select! {
            message = inbound.next() => message,
            Some(_) = relays.join_next() => continue,
        };
        let Some(Ok(message)) = message else {
            break;
        };
        let Message::Binary(bytes) = message else {
            continue;
        };
        let Ok(Up::Ask(ask)) = abi::decode::<Up>(&bytes) else {
            break;
        };
        let daemon = daemon.clone();
        let down = down.clone();
        relays.spawn(async move {
            let id = ask.id;
            let answered = relay(&daemon, account, ask, &down).await;
            let _ = down.unbounded_send(Down::Answered { id, answered });
        });
    }
    relays.abort_all();
}

async fn proven<E: Context>(
    daemon: &Daemon<E>,
    down: &mpsc::UnboundedSender<Down>,
    inbound: &mut (impl Stream<Item = Result<Message, axum::Error>> + Unpin),
) -> Result<AccountNumber, Refusal> {
    let anchor = daemon.view().map_err(stopped)?.tip();
    let mut nonce = [0u8; 32];
    rand_core::Rng::fill_bytes(&mut daemon.context.child("nonce"), &mut nonce);
    let network = daemon.descriptor.id();
    let challenge = Down::Challenge {
        network: network.clone(),
        anchor,
        nonce,
    };
    let _ = down.unbounded_send(challenge);
    let proof = loop {
        match inbound.next().await {
            Some(Ok(Message::Binary(bytes))) => break bytes,
            Some(Ok(_)) => {}
            Some(Err(_)) | None => {
                return Err(Refusal::new(
                    reason::INVALID_INPUT,
                    "the borrowing node left before its proof",
                ));
            }
        }
    };
    let proof: Up = abi::decode(&proof)?;
    let Up::Proof(frame) = proof else {
        return Err(Refusal::new(
            reason::INVALID_INPUT,
            "a session opens with a proof",
        ));
    };
    let submission = node::verify(&frame, &network)?;
    let answers = submission.target == LEND && submission.payload == nonce;
    if !answers {
        return Err(Refusal::new(
            reason::INVALID_INPUT,
            "the proof signs this session's nonce to $lend",
        ));
    }
    let asked = identity::Query::Account(submission.signer);
    let held: identity::Reply = ask(daemon, role::IDENTITY, &asked).await?;
    match held {
        identity::Reply::Account(Some(account)) => Ok(account),
        identity::Reply::Account(None) => Err(Refusal::new(
            reason::UNAUTHORIZED,
            "the borrowing node's key holds no account on this network",
        )),
        other => Err(Refusal::new(
            reason::CORRUPT,
            format!("identity answered an account with {other:?}"),
        )),
    }
}

fn stopped(error: impl std::fmt::Display) -> Refusal {
    Refusal::new(reason::WRONG_STATE, error.to_string())
}

async fn ask<E: Context, R: BorshDeserialize>(
    daemon: &Daemon<E>,
    role: &str,
    query: &impl BorshSerialize,
) -> Result<R, Refusal> {
    let view = daemon.view().map_err(stopped)?;
    let source = view.source();
    let program = source.roles().bound(role)?.clone();
    let asked = abi::encode(query);
    let answered = source
        .query(
            Layer::Confirmed,
            daemon.now(),
            Origin::System,
            &program,
            asked,
        )
        .await;
    match answered {
        Ok(Ok(bytes)) => abi::decode(&bytes),
        Ok(Err(refusal)) => Err(refusal),
        Err(error) => Err(stopped(error)),
    }
}

async fn lent<E: Context>(
    daemon: &Daemon<E>,
    account: AccountNumber,
    service: &str,
) -> Result<(), Refusal> {
    if RESERVED.contains(&service) {
        return Err(Refusal::new(
            reason::INVALID_INPUT,
            format!("{service} is egress's own service, never lent"),
        ));
    }
    let egress = daemon
        .view()
        .map_err(stopped)?
        .source()
        .roles()
        .bound(role::EGRESS)
        .cloned()?;
    let key = format!("lend/{service}");
    let asked = access::Query::Writers {
        program: egress,
        key: key.clone(),
    };
    let answered: access::Reply = ask(daemon, role::ACCESS, &asked).await?;
    let access::Reply::Writers(writers) = answered else {
        return Err(Refusal::new(
            reason::CORRUPT,
            "access answered Writers with something else",
        ));
    };
    match writers.allows(&[Party::Account(account)]) {
        true => Ok(()),
        false => Err(Refusal::new(
            reason::UNAUTHORIZED,
            format!(
                "{service} is not lent to account {account}: the list on egress {key} does not name it"
            ),
        )),
    }
}

async fn relay<E: Context>(
    daemon: &Daemon<E>,
    account: AccountNumber,
    ask: Ask,
    down: &mpsc::UnboundedSender<Down>,
) -> Answered {
    let Ask {
        id,
        chain,
        program,
        service,
        head,
        delivery,
        body,
    } = ask;
    if let Err(refusal) = lent(daemon, account, &service).await {
        return Answered::Whole(Answer::Failed(refusal.sentence));
    }
    let serving = daemon.egress.current();
    let Serving { config, clients } = serving.as_ref();
    let loan = Loan {
        account: account.to_string(),
        service: service.clone(),
    };
    let lending = &daemon.lending;
    lending.requests.get_or_create(&loan).inc();
    let named = config
        .services
        .get(&service)
        .and_then(|upstream| upstream.lend.get(&account))
        .cloned()
        .unwrap_or_default();
    let lent = lent_headers(account, named);
    let set_here = |name: &str| lent.iter().any(|(set, _)| set.eq_ignore_ascii_case(name));
    let headers = head
        .headers
        .into_iter()
        .filter(|(name, _)| !set_here(name))
        .collect();
    let head = Head { headers, ..head };
    let request = Request::Http {
        service,
        head,
        body: Vec::new(),
    };
    let caller = Caller {
        chain: &chain,
        program: &program,
        id,
        lent: &lent,
    };
    let event = |data: Vec<u8>| {
        lending.sent.get_or_create(&loan).inc_by(data.len() as u64);
        let _ = down.unbounded_send(Down::Event { id, data });
    };
    match call::answer(clients, config, &caller, &request, &delivery, body, event).await {
        Called::Whole(answer) => {
            if let Answer::Output(output) = &answer {
                lending
                    .sent
                    .get_or_create(&loan)
                    .inc_by(output.len() as u64);
            }
            Answered::Whole(answer)
        }
        Called::End(end) => Answered::End(end),
        Called::Event(_) => {
            Answered::Whole(Answer::Failed("the call answered with an event".to_owned()))
        }
    }
}

fn lent_headers(account: AccountNumber, named: Vec<(String, String)>) -> Vec<(String, String)> {
    std::iter::once((BORROWER.to_owned(), account.to_string()))
        .chain(named)
        .collect()
}
