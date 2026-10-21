//! The bridge's one loop: what attribution tells the account, settled in seq order into places,
//! and in each place a turn taken, drafted and posted in chat, one op at a time, with the state
//! file kept after every step so a restart lets go of what it held and hands open input again.
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;

use futures::channel::mpsc::UnboundedReceiver;
use futures::future::LocalBoxFuture;
use futures::stream::{FuturesUnordered, LocalBoxStream, SelectAll};
use futures::{FutureExt as _, StreamExt as _};
use serde_json::{Value, json};

use super::adapter::{Adapter, Event, Turn};
use super::chain::{Chain, Sent};
use super::heard::{self, Heard};
use super::kept::{self, Input, Kept, KeptPlace, Wake, Where};
use super::{ATTRIBUTION, CHAT, WAKE};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Taking,
    Waiting,
    Running,
    Ending,
}

enum Op {
    Undraft,
    Release,
    Take,
    Clear,
    Draft { text: String, doing: Option<String> },
    Post { key: String, text: String },
}

impl Op {
    fn name(&self) -> &'static str {
        match self {
            Op::Undraft => "undraft",
            Op::Release => "release",
            Op::Take => "take",
            Op::Clear => "WakeOn Clear",
            Op::Draft { .. } => "drafting",
            Op::Post { .. } => "post",
        }
    }

    fn json(&self, at: &Where) -> (&'static str, Value) {
        let (channel, thread) = (&at.channel, at.thread);
        match self {
            Op::Undraft => (CHAT, json!({ "undraft": { "channel_id": channel } })),
            Op::Release => (
                CHAT,
                json!({ "release": { "channel_id": channel, "thread": thread } }),
            ),
            Op::Take => (
                CHAT,
                json!({ "take": { "channel_id": channel, "thread": thread } }),
            ),
            Op::Clear => (
                WAKE,
                json!({ "wake_on": { "event": { "clear": { "channel_id": channel, "thread": thread } } } }),
            ),
            Op::Draft { text, doing } => (
                CHAT,
                json!({ "drafting": {
                    "channel_id": channel,
                    "thread": thread,
                    "text": text,
                    "anew": false,
                    "doing": doing,
                } }),
            ),
            Op::Post { key, text } => (
                CHAT,
                json!({ "post": {
                    "channel_id": channel,
                    "message_id": key,
                    "text": text,
                    "thread": thread,
                } }),
            ),
        }
    }
}

struct Place<T> {
    at: Where,
    session: Option<String>,
    held: bool,
    phase: Phase,
    turn: Option<T>,
    checking: Vec<Input>,
    posting: Vec<Input>,
    asked: VecDeque<Input>,
    waiting: VecDeque<Input>,
    ops: VecDeque<Op>,
    sending: bool,
    drafted: bool,
    began: bool,
}

impl<T> Place<T> {
    fn new(at: Where) -> Self {
        Place {
            at,
            session: None,
            held: false,
            phase: Phase::Idle,
            turn: None,
            checking: Vec::new(),
            posting: Vec::new(),
            asked: VecDeque::new(),
            waiting: VecDeque::new(),
            ops: VecDeque::new(),
            sending: false,
            drafted: false,
            began: false,
        }
    }

    fn open(&self) -> impl Iterator<Item = &Input> {
        self.checking
            .iter()
            .chain(&self.posting)
            .chain(&self.asked)
            .chain(&self.waiting)
    }

    fn take(&mut self) {
        self.phase = Phase::Taking;
        self.held = true;
        self.ops.push_back(Op::Take);
    }

    fn let_go(&mut self) {
        self.ops.push_back(Op::Undraft);
        self.ops.push_back(Op::Release);
    }

    fn draft(&mut self, text: String, doing: Option<String>) {
        let text = match self.drafted && !text.is_empty() {
            true => format!("\n\n{text}"),
            false => text,
        };
        self.drafted |= !text.is_empty();
        self.held = true;
        if let Some(Op::Draft {
            text: queued,
            doing: was,
        }) = self.ops.back_mut()
        {
            queued.push_str(&text);
            if doing.is_some() {
                *was = doing;
            }
            return;
        }
        self.ops.push_back(Op::Draft { text, doing });
    }

    fn holds_next(&self) -> bool {
        self.ops
            .iter()
            .any(|op| matches!(op, Op::Take | Op::Draft { .. }))
    }

    fn post(&mut self, input: Input, text: String) {
        self.ops.push_back(Op::Post {
            key: input.key.clone(),
            text,
        });
        self.posting.push(input);
    }
}

enum Done {
    Classified(u64, Result<Heard, String>),
    Checked {
        place: String,
        input: Input,
        answered: Result<bool, String>,
    },
    Sent {
        place: String,
        op: Op,
        sent: Result<Sent, String>,
    },
}

pub(crate) struct Bridge<'a, C, A: Adapter> {
    chain: &'a C,
    adapter: &'a A,
    path: &'a Path,
    fresh: bool,
    last: u64,
    after: Option<String>,
    wakes: BTreeMap<u64, Wake>,
    unclaimed: BTreeMap<u64, Input>,
    places: BTreeMap<String, Place<A::Turn>>,
    order: BTreeMap<u64, Option<Heard>>,
    pending: FuturesUnordered<LocalBoxFuture<'a, Done>>,
    turns: SelectAll<LocalBoxStream<'static, (String, Event)>>,
}

impl<'a, C: Chain, A: Adapter> Bridge<'a, C, A> {
    pub(crate) fn open(chain: &'a C, adapter: &'a A, path: &'a Path) -> Result<Self, String> {
        let kept = kept::load(path)?;
        let fresh = kept.is_none();
        let kept = kept.unwrap_or_default();
        let mut places = BTreeMap::new();
        for (key, kept) in kept.places {
            let mut place = Place::new(kept.at);
            place.session = kept.session;
            place.held = kept.held;
            place.checking = kept.open;
            places.insert(key, place);
        }
        Ok(Bridge {
            chain,
            adapter,
            path,
            fresh,
            last: kept.last,
            after: kept.after,
            wakes: kept.wakes,
            unclaimed: kept.unclaimed,
            places,
            order: BTreeMap::new(),
            pending: FuturesUnordered::new(),
            turns: SelectAll::new(),
        })
    }

    pub(crate) async fn run(
        &mut self,
        mut follow: LocalBoxStream<'_, Result<u64, String>>,
        mut armed: UnboundedReceiver<(u64, String)>,
        stop: impl Future<Output = ()>,
    ) -> Result<(), String> {
        self.restart();
        self.catch_up().await?;
        tokio::pin!(stop);
        loop {
            tokio::select! {
                heard = follow.next() => match heard {
                    Some(seq) => {
                        self.heard(seq?, None);
                    }
                    None => return Err("the node closed attribution's changes".into()),
                },
                Some(done) = self.pending.next() => self.done(done).await?,
                Some((place, event)) = self.turns.next() => self.turn(&place, event).await?,
                Some((wake, place)) = armed.next() => self.armed(wake, place).await?,
                () = &mut stop => return self.stop().await,
            }
        }
    }

    fn restart(&mut self) {
        let chain = self.chain;
        let keys: Vec<String> = self.places.keys().cloned().collect();
        for key in keys {
            let Some(place) = self.places.get_mut(&key) else {
                continue;
            };
            if place.held {
                place.let_go();
            }
            for input in place.checking.clone() {
                let place = key.clone();
                self.pending.push(
                    async move {
                        let answered = heard::answered(chain, &input.key).await;
                        Done::Checked {
                            place,
                            input,
                            answered,
                        }
                    }
                    .boxed_local(),
                );
            }
            self.pump(&key);
        }
    }

    async fn catch_up(&mut self) -> Result<(), String> {
        let me = self.chain.account();
        let mut cursor = self.after.clone();
        let mut settled = true;
        loop {
            let asked = json!({ "changes_to": { "recipient": me, "page": { "after": cursor, "limit": null } } });
            let reply = self.chain.query(ATTRIBUTION, asked).await?;
            let changes = &reply["changes"];
            for change in changes["items"].as_array().into_iter().flatten() {
                let seq = change["seq"]
                    .as_u64()
                    .ok_or("attribution answered a change with no seq")?;
                match self.fresh {
                    true => self.last = self.last.max(seq),
                    false => settled &= !self.heard(seq, Some(change.clone())),
                }
            }
            let Some(next) = changes["next"].as_str() else {
                break;
            };
            cursor = Some(next.to_owned());
            if settled {
                self.after = cursor.clone();
            }
        }
        self.fresh = false;
        self.save()
    }

    fn open_seq(&self, seq: u64) -> bool {
        self.places
            .values()
            .any(|place| place.open().any(|input| input.seq == seq))
    }

    fn heard(&mut self, seq: u64, change: Option<Value>) -> bool {
        if seq <= self.last || self.order.contains_key(&seq) || self.open_seq(seq) {
            return false;
        }
        self.order.insert(seq, None);
        let chain = self.chain;
        self.pending.push(
            async move {
                let heard = match change {
                    Some(change) => heard::classify(chain, change).await,
                    None => heard::fetch(chain, seq).await,
                };
                Done::Classified(seq, heard)
            }
            .boxed_local(),
        );
        true
    }

    async fn done(&mut self, done: Done) -> Result<(), String> {
        match done {
            Done::Classified(seq, heard) => {
                self.order.insert(seq, Some(heard?));
                while self
                    .order
                    .first_key_value()
                    .is_some_and(|(_, heard)| heard.is_some())
                {
                    let Some((seq, Some(heard))) = self.order.pop_first() else {
                        break;
                    };
                    self.last = self.last.max(seq);
                    self.apply(seq, heard).await;
                }
            }
            Done::Checked {
                place,
                input,
                answered,
            } => {
                let answered = answered?;
                let Some(at) = self.places.get_mut(&place).map(|kept| {
                    kept.checking.retain(|open| open.key != input.key);
                    kept.at.clone()
                }) else {
                    return Ok(());
                };
                if !answered {
                    self.hand(at, input).await;
                }
            }
            Done::Sent { place, op, sent } => self.sent(&place, op, sent?).await,
        }
        self.save()
    }

    async fn apply(&mut self, seq: u64, heard: Heard) {
        match heard {
            Heard::Input { at, input } => {
                let open = self
                    .places
                    .values()
                    .any(|place| place.open().any(|open| open.key == input.key));
                if !open {
                    self.hand(at, input).await;
                }
            }
            Heard::Woke { wake, input } => match self.wakes.remove(&wake) {
                Some(Wake::Clear(place)) => self.cleared(&place),
                Some(Wake::Model(place)) => self.woke(&place, input).await,
                None => {
                    self.unclaimed.insert(wake, input);
                }
            },
            Heard::Ignored(why) => eprintln!("bridge: change {seq}: {why}"),
        }
    }

    async fn armed(&mut self, wake: u64, place: String) -> Result<(), String> {
        match self.unclaimed.remove(&wake) {
            Some(input) => self.woke(&place, input).await,
            None => {
                self.wakes.insert(wake, Wake::Model(place));
            }
        }
        self.save()
    }

    async fn woke(&mut self, place: &str, input: Input) {
        match self.places.get(place).map(|place| place.at.clone()) {
            Some(at) => self.hand(at, input).await,
            None => eprintln!("bridge: a wake was armed from {place}, which this bridge never ran"),
        }
    }

    fn cleared(&mut self, key: &str) {
        let Some(place) = self.places.get_mut(key) else {
            return;
        };
        if place.phase == Phase::Waiting {
            place.take();
            self.pump(key);
        }
    }

    async fn hand(&mut self, at: Where, input: Input) {
        let key = at.key();
        let place = self
            .places
            .entry(key.clone())
            .or_insert_with(|| Place::new(at));
        place.waiting.push_back(input);
        if place.phase == Phase::Idle {
            place.take();
        }
        self.pump(&key);
    }

    fn pump(&mut self, key: &str) {
        let Some(place) = self.places.get_mut(key) else {
            return;
        };
        if place.sending {
            return;
        }
        let Some(op) = place.ops.pop_front() else {
            return;
        };
        place.sending = true;
        let (program, said) = op.json(&place.at);
        let chain = self.chain;
        let place = key.to_owned();
        self.pending.push(
            async move {
                let sent = chain.send(program, said).await;
                Done::Sent { place, op, sent }
            }
            .boxed_local(),
        );
    }

    async fn sent(&mut self, key: &str, op: Op, sent: Sent) {
        let Some(place) = self.places.get_mut(key) else {
            return;
        };
        place.sending = false;
        if let Sent::Rejected { reason, sentence } = &sent {
            eprintln!(
                "bridge: {} in {key} was refused: {reason}: {sentence}",
                op.name()
            );
        }
        let mut start = false;
        match (op, sent) {
            (Op::Take, Sent::Applied(_)) => {
                place.held = true;
                start = true;
            }
            (Op::Take, Sent::Rejected { reason, .. }) if reason == "busy" => {
                place.phase = Phase::Waiting;
                place.ops.push_back(Op::Clear);
            }
            (Op::Clear, Sent::Applied(wake)) => match wake.as_u64() {
                Some(wake) if self.unclaimed.remove(&wake).is_some() => place.take(),
                Some(wake) => {
                    self.wakes.insert(wake, Wake::Clear(key.to_owned()));
                }
                None => eprintln!("bridge: wake answered WakeOn in {key} with no id"),
            },
            (Op::Take | Op::Clear, Sent::Rejected { .. }) => {
                place.waiting.clear();
                place.phase = Phase::Idle;
                place.held = false;
            }
            (Op::Post { key: answer, .. }, _) => {
                place.posting.retain(|input| input.key != answer);
                place.held = place.holds_next();
            }
            (Op::Release, _) => place.held = place.holds_next(),
            (Op::Undraft | Op::Draft { .. }, _) => {}
        }
        if start {
            self.start(key).await;
        }
        self.pump(key);
    }

    async fn start(&mut self, key: &str) {
        let Some(place) = self.places.get_mut(key) else {
            return;
        };
        match self.adapter.start(&place.at, place.session.as_deref()) {
            Ok((mut turn, events)) => {
                let tag = key.to_owned();
                self.turns
                    .push(events.map(move |event| (tag.clone(), event)).boxed_local());
                place.phase = Phase::Running;
                place.drafted = false;
                place.began = false;
                if let Some(input) = place.waiting.pop_front() {
                    say(key, &mut turn, &input).await;
                    place.asked.push_back(input);
                }
                place.turn = Some(turn);
            }
            Err(why) => {
                if let Some(first) = place.waiting.pop_front() {
                    place.post(first, format!("I could not start a session here: {why}"));
                }
                place.waiting.clear();
                place.phase = Phase::Idle;
            }
        }
    }

    async fn turn(&mut self, key: &str, event: Event) -> Result<(), String> {
        let Some(place) = self.places.get_mut(key) else {
            return Ok(());
        };
        match event {
            Event::Session(session) => {
                place.session = Some(session);
                place.began = true;
            }
            Event::Text(text) => place.draft(text, None),
            Event::Doing(doing) => place.draft(String::new(), Some(doing)),
            Event::Answer(text) => {
                place.drafted = false;
                match place.asked.pop_front() {
                    Some(input) if !text.trim().is_empty() => place.post(input, text),
                    Some(_) => place.let_go(),
                    None => eprintln!("bridge: the session in {key} answered with nothing asked"),
                }
                if place.asked.is_empty()
                    && let Some(turn) = place.turn.as_mut()
                {
                    match place.waiting.pop_front() {
                        Some(input) => {
                            say(key, turn, &input).await;
                            place.asked.push_back(input);
                        }
                        None => {
                            turn.finish();
                            place.phase = Phase::Ending;
                        }
                    }
                }
            }
            Event::Ended(why) => {
                place.turn = None;
                let first = place.asked.pop_front();
                while let Some(unanswered) = place.asked.pop_back() {
                    place.waiting.push_front(unanswered);
                }
                if let Some(first) = first {
                    let before = match place.began {
                        true => "answered",
                        false => "started",
                    };
                    let text = format!("My session here ended ({why}) before it {before}.");
                    place.post(first, text);
                }
                if place.waiting.is_empty() && place.held && place.posting.is_empty() {
                    place.let_go();
                }
                match place.waiting.is_empty() {
                    true => place.phase = Phase::Idle,
                    false => place.take(),
                }
            }
        }
        self.pump(key);
        self.save()
    }

    async fn stop(&mut self) -> Result<(), String> {
        let mut running = 0;
        for place in self.places.values_mut() {
            if let Some(turn) = place.turn.as_mut() {
                turn.stop();
                running += 1;
            }
        }
        while running > 0 {
            match self.turns.next().await {
                Some((_, Event::Ended(_))) => running -= 1,
                Some(_) => {}
                None => break,
            }
        }
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        let places = self
            .places
            .iter()
            .filter(|(_, place)| {
                place.session.is_some() || place.held || place.open().next().is_some()
            })
            .map(|(key, place)| {
                let kept = KeptPlace {
                    at: place.at.clone(),
                    session: place.session.clone(),
                    held: place.held,
                    open: place.open().cloned().collect(),
                };
                (key.clone(), kept)
            })
            .collect();
        let kept = Kept {
            last: self.last,
            after: self.after.clone(),
            places,
            wakes: self.wakes.clone(),
            unclaimed: self.unclaimed.clone(),
        };
        kept::save(self.path, &kept)
    }
}

async fn say<T: Turn>(key: &str, turn: &mut T, input: &Input) {
    if let Err(why) = turn.say(&input.text).await {
        eprintln!("bridge: input to {key} was not said: {why}");
    }
}
