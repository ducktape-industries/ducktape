//! One client's connection, over QUIC or a websocket, as the relay sees it: a stream of
//! inbound events read by tasks that know the transport, and an outlet the relay writes to that
//! a task per connection turns into control messages, group streams and datagrams.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use bytes::Bytes;
use futures::{SinkExt as _, StreamExt as _};
use halo::{Class, Pacer};
use live_wire::{
    Alias, Control, DatagramHeader, GroupHeader, MAX_STREAM_OBJECT, Priority, WsFrame,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;

/// One control message's cap: a namespace, a track name and the fixed fields fit in it.
pub const CONTROL_BYTES: u32 = 1 << 10;
/// One websocket message's cap: a slice of one object, or a control message, with its framing.
pub const WS_MESSAGE: usize = MAX_STREAM_OBJECT + CONTROL_BYTES as usize;
/// Inbound events waiting for the session's loop; a reader waits when it is full, so the
/// transport's flow control holds the publisher back.
const INBOUND: usize = 1024;
/// Control messages and stream objects waiting for the connection; a session this far behind
/// is closed rather than fed from a growing queue.
const OUTBOUND: usize = 4096;
/// Objects waiting on one egress group stream; a group this far behind is reset.
const GROUP_QUEUE: usize = 256;
/// The control stream outranks every group stream of the connection.
const CONTROL_PRIORITY: i32 = 256;
/// A last message (a refusal, a GoAway) gets this long to reach a client that stopped reading;
/// then the connection is closed under it.
const LINGER: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum Inbound {
    Control(Control),
    Object {
        header: GroupHeader,
        object: u32,
        body: Bytes,
    },
    GroupEnd {
        alias: Alias,
        group: u64,
        complete: bool,
    },
    /// An object over its cap: the group is abandoned, the publisher refused, the session kept.
    Oversize {
        alias: Alias,
        group: u64,
    },
    Datagram {
        header: DatagramHeader,
        payload: Bytes,
    },
    /// Bytes that are not the wire: the session is closed as malformed.
    Garbled,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    Control(Control),
    Object {
        header: GroupHeader,
        object: u32,
        body: Bytes,
        /// The track's priority, which the egress sets its group stream to (the one source of
        /// a track's priority, no longer a per-group header field).
        priority: Priority,
    },
    /// A whole cached group replayed to a late joiner, as one unit: the writer writes every
    /// object on one group stream, so a large replay is not N bounded sends that overflow and
    /// reset the group or close the session. `end` is `Some(complete)` when the cached group is
    /// closed; `None` when it is still being written and live objects follow on the same stream.
    Group {
        header: GroupHeader,
        priority: Priority,
        objects: Arc<Vec<Bytes>>,
        end: Option<bool>,
    },
    GroupEnd {
        alias: Alias,
        group: u64,
        complete: bool,
    },
    Datagram {
        header: DatagramHeader,
        payload: Bytes,
    },
}

/// Datagram-mode objects waiting for a connection: the newest per track, older ones dropped,
/// which is a datagram's semantics done in software.
#[derive(Default)]
struct Latest {
    slots: Mutex<HashMap<Alias, (DatagramHeader, Bytes)>>,
    ready: Notify,
}

impl Latest {
    fn put(&self, header: DatagramHeader, payload: Bytes) -> bool {
        let replaced = self
            .slots
            .lock()
            .expect("the slots lock is never poisoned")
            .insert(header.alias, (header, payload))
            .is_some();
        self.ready.notify_one();
        replaced
    }

    fn take(&self) -> Vec<(DatagramHeader, Bytes)> {
        self.slots
            .lock()
            .expect("the slots lock is never poisoned")
            .drain()
            .map(|(_, datagram)| datagram)
            .collect()
    }
}

/// The relay's handle on one connection's egress.
#[derive(Clone)]
pub struct Outlet {
    ordered: mpsc::Sender<Out>,
    latest: Arc<Latest>,
    closing: watch::Sender<Option<&'static str>>,
}

impl std::fmt::Debug for Outlet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Outlet").finish()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Sent {
    Queued,
    /// A datagram took the slot of an older one for the same track.
    Superseded,
    /// The ordered queue is full: the connection is closed as behind.
    Behind,
}

impl Outlet {
    pub fn send(&self, out: Out) -> Sent {
        if let Out::Datagram { header, payload } = out {
            return match self.latest.put(header, payload) {
                true => Sent::Superseded,
                false => Sent::Queued,
            };
        }
        match self.ordered.try_send(out) {
            Ok(()) => Sent::Queued,
            Err(_) => {
                self.close("behind");
                Sent::Behind
            }
        }
    }

    pub fn close(&self, reason: &'static str) {
        self.closing.send_if_modified(|closing| {
            let first = closing.is_none();
            if first {
                *closing = Some(reason);
            }
            first
        });
    }

    pub fn closed(&self) -> Option<&'static str> {
        *self.closing.borrow()
    }
}

/// What a connection's writer task takes from the outlet next.
enum Drained {
    Out(Out),
    Datagrams(Vec<(DatagramHeader, Bytes)>),
    Close,
}

struct Egress {
    ordered: mpsc::Receiver<Out>,
    latest: Arc<Latest>,
    closing: watch::Receiver<Option<&'static str>>,
}

impl Egress {
    /// What was queued before a close is still sent: a `GoAway` reaches the client.
    async fn next(&mut self) -> Drained {
        loop {
            if let Ok(out) = self.ordered.try_recv() {
                return Drained::Out(out);
            }
            let ready = self.latest.ready.notified();
            let pending = self.latest.take();
            if !pending.is_empty() {
                return Drained::Datagrams(pending);
            }
            tokio::select! {
                out = self.ordered.recv() => {
                    return match out {
                        Some(out) => Drained::Out(out),
                        None => Drained::Close,
                    };
                }
                _ = self.closing.wait_for(|closing| closing.is_some()) => {
                    // A close follows what was sent before it; that goes out first.
                    if let Ok(out) = self.ordered.try_recv() {
                        return Drained::Out(out);
                    }
                    return Drained::Close;
                }
                () = ready => {}
            }
        }
    }
}

/// An outlet no connection drains, for a step that is tested without one.
#[cfg(test)]
pub fn unconnected() -> Outlet {
    outlet().0
}

fn outlet() -> (Outlet, Egress) {
    let (ordered, receiving) = mpsc::channel(OUTBOUND);
    let latest = Arc::new(Latest::default());
    let (closing, closed) = watch::channel(None);
    let outlet = Outlet {
        ordered,
        latest: latest.clone(),
        closing,
    };
    let egress = Egress {
        ordered: receiving,
        latest,
        closing: closed,
    };
    (outlet, egress)
}

/// A connection as the session loop reads and writes it, whatever carries it. Dropping it
/// closes the connection: the writer gets `LINGER` to put out the last message, then the
/// connection is closed under it.
pub struct Link {
    pub inbound: mpsc::Receiver<Inbound>,
    pub outlet: Outlet,
    pub datagrams: bool,
    pub path: &'static str,
    pub closing: watch::Receiver<Option<&'static str>>,
    readers: Vec<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    connection: Option<quinn::Connection>,
}

/// Resolves when the outlet was closed by anyone: fan-out that found the session behind, a
/// re-check that found no account, the node going away.
pub async fn closed_elsewhere(closing: &mut watch::Receiver<Option<&'static str>>) -> &'static str {
    let closed = closing.wait_for(|closing| closing.is_some()).await;
    closed
        .ok()
        .and_then(|closing| *closing)
        .unwrap_or("relay_gone")
}

impl Drop for Link {
    fn drop(&mut self) {
        self.outlet.close("session_ended");
        for reader in &self.readers {
            reader.abort();
        }
        let reason = self.outlet.closed().unwrap_or("session_ended");
        if let Some(writer) = self.writer.take() {
            tokio::spawn(lingering(writer, self.connection.take(), reason));
        }
    }
}

/// The writer ends on its own once it put out what was queued before the close; one blocked by
/// a client that stopped reading is cut off at `LINGER`, and the connection with it.
async fn lingering(
    mut writer: JoinHandle<()>,
    connection: Option<quinn::Connection>,
    reason: &'static str,
) {
    tokio::select! {
        _ = &mut writer => {}
        () = tokio::time::sleep(LINGER) => writer.abort(),
    }
    if let Some(connection) = connection {
        connection.close(0u32.into(), reason.as_bytes());
    }
}

/// `len: u32` little-endian, then the bytes: how a control message and an object are framed.
pub async fn framed<W: tokio::io::AsyncWrite + Unpin>(
    stream: &mut W,
    bytes: &[u8],
) -> std::io::Result<()> {
    stream.write_u32_le(bytes.len() as u32).await?;
    stream.write_all(bytes).await
}

/// quinn's stream priority: 0 is our highest, so the higher the byte the lower the rank.
fn stream_priority(priority: Priority) -> i32 {
    i32::from(u8::MAX - priority)
}

// ---- QUIC ----

impl Link {
    /// A client's QUIC connection whose first bidirectional stream, the control stream, is open.
    pub fn quic(
        connection: quinn::Connection,
        control: (quinn::SendStream, quinn::RecvStream),
        pacer: Pacer,
    ) -> Link {
        let (control_send, control_recv) = control;
        let (inbound, receiving) = mpsc::channel(INBOUND);
        let (outlet, egress) = outlet();
        let closing = egress.closing.clone();
        let readers = vec![
            tokio::spawn(quic_control_reading(control_recv, inbound.clone())),
            tokio::spawn(quic_groups_reading(connection.clone(), inbound.clone())),
            tokio::spawn(quic_datagrams_reading(connection.clone(), inbound)),
        ];
        let writer = tokio::spawn(quic_writing(
            connection.clone(),
            control_send,
            egress,
            pacer,
        ));
        Link {
            inbound: receiving,
            datagrams: connection.max_datagram_size().is_some(),
            outlet,
            path: "quic",
            closing,
            readers,
            writer: Some(writer),
            connection: Some(connection),
        }
    }
}

async fn read_control(recv: &mut quinn::RecvStream) -> Result<Option<Control>, ()> {
    let len = match recv.read_u32_le().await {
        Ok(len) => len,
        Err(_) => return Ok(None),
    };
    let fits = len <= CONTROL_BYTES;
    if !fits {
        return Err(());
    }
    let mut bytes = vec![0u8; len as usize];
    recv.read_exact(&mut bytes).await.map_err(|_| ())?;
    abi::decode(&bytes).map(Some).map_err(|_| ())
}

async fn quic_control_reading(mut recv: quinn::RecvStream, inbound: mpsc::Sender<Inbound>) {
    loop {
        let event = match read_control(&mut recv).await {
            Ok(Some(control)) => Inbound::Control(control),
            Ok(None) => Inbound::Closed,
            Err(()) => Inbound::Garbled,
        };
        let ends = matches!(event, Inbound::Closed | Inbound::Garbled);
        let delivered = inbound.send(event).await.is_ok();
        if ends || !delivered {
            return;
        }
    }
}

async fn quic_groups_reading(connection: quinn::Connection, inbound: mpsc::Sender<Inbound>) {
    while let Ok(stream) = connection.accept_uni().await {
        tokio::spawn(quic_group_reading(stream, inbound.clone()));
    }
}

async fn quic_group_reading(mut stream: quinn::RecvStream, inbound: mpsc::Sender<Inbound>) {
    let mut header = [0u8; 12];
    if stream.read_exact(&mut header).await.is_err() {
        return;
    }
    let Ok(header) = abi::decode::<GroupHeader>(&header) else {
        let _ = inbound.send(Inbound::Garbled).await;
        return;
    };
    let mut object = 0u32;
    // FIN between objects is a clean end; a reset, or a FIN inside a prefix or an object,
    // leaves the group short. The first read error is the word: the stream says nothing after
    // it.
    let complete = loop {
        let len = match prefix(&mut stream).await {
            Prefix::Len(len) => len as usize,
            Prefix::Fin => break true,
            Prefix::Cut => break false,
        };
        let fits = len <= MAX_STREAM_OBJECT;
        if !fits {
            // The stream is stopped at the object that does not fit; the publisher hears why,
            // and the group ends short so no entry for it is left behind (C1-11).
            let _ = stream.stop(0u32.into());
            let _ = inbound
                .send(Inbound::Oversize {
                    alias: header.alias,
                    group: header.group,
                })
                .await;
            let _ = inbound
                .send(Inbound::GroupEnd {
                    alias: header.alias,
                    group: header.group,
                    complete: false,
                })
                .await;
            return;
        }
        let mut body = vec![0u8; len];
        if stream.read_exact(&mut body).await.is_err() {
            break false;
        }
        let delivered = inbound
            .send(Inbound::Object {
                header,
                object,
                body: body.into(),
            })
            .await
            .is_ok();
        if !delivered {
            return;
        }
        object += 1;
    };
    let _ = inbound
        .send(Inbound::GroupEnd {
            alias: header.alias,
            group: header.group,
            complete,
        })
        .await;
}

enum Prefix {
    Len(u32),
    /// The stream ended cleanly where a prefix would start.
    Fin,
    /// The stream ended inside a prefix, or was reset.
    Cut,
}

async fn prefix(stream: &mut quinn::RecvStream) -> Prefix {
    let mut bytes = [0u8; 4];
    match stream.read(&mut bytes[..1]).await {
        Ok(None) => return Prefix::Fin,
        Ok(Some(_)) => {}
        Err(_) => return Prefix::Cut,
    }
    match stream.read_exact(&mut bytes[1..]).await {
        Ok(()) => Prefix::Len(u32::from_le_bytes(bytes)),
        Err(_) => Prefix::Cut,
    }
}

async fn quic_datagrams_reading(connection: quinn::Connection, inbound: mpsc::Sender<Inbound>) {
    while let Ok(datagram) = connection.read_datagram().await {
        let Some(event) = datagram_arrived(datagram) else {
            continue;
        };
        if inbound.send(event).await.is_err() {
            return;
        }
    }
}

fn datagram_arrived(mut bytes: Bytes) -> Option<Inbound> {
    let fits = bytes.len() >= 16;
    if !fits {
        return None;
    }
    let header = bytes.split_to(16);
    let header: DatagramHeader = abi::decode(&header).ok()?;
    Some(Inbound::Datagram {
        header,
        payload: bytes,
    })
}

enum GroupOut {
    Object(Bytes),
    /// A whole cached group's objects, written in one go: a replay is one queue item, not one
    /// per object.
    Batch(Arc<Vec<Bytes>>),
    End { complete: bool },
}

struct Groups {
    open: HashMap<(Alias, u64), mpsc::Sender<GroupOut>>,
    /// Groups reset for falling behind: the rest of their objects are dropped quiet.
    abandoned: HashSet<(Alias, u64)>,
}

async fn quic_writing(
    connection: quinn::Connection,
    mut control: quinn::SendStream,
    mut egress: Egress,
    pacer: Pacer,
) {
    let _ = control.set_priority(CONTROL_PRIORITY);
    let mut groups = Groups {
        open: HashMap::new(),
        abandoned: HashSet::new(),
    };
    loop {
        match egress.next().await {
            Drained::Out(Out::Control(message)) => {
                if framed(&mut control, &abi::encode(&message)).await.is_err() {
                    return;
                }
            }
            Drained::Out(Out::Object {
                header,
                body,
                priority,
                ..
            }) => {
                quic_group_object(&connection, &pacer, &mut groups, header, priority, body);
            }
            Drained::Out(Out::Group {
                header,
                priority,
                objects,
                end,
            }) => {
                quic_group_batch(&connection, &pacer, &mut groups, header, priority, objects, end);
            }
            Drained::Out(Out::GroupEnd {
                alias,
                group,
                complete,
            }) => {
                groups.abandoned.remove(&(alias, group));
                if let Some(sending) = groups.open.remove(&(alias, group)) {
                    let _ = sending.try_send(GroupOut::End { complete });
                }
            }
            Drained::Out(Out::Datagram { .. }) => {}
            Drained::Datagrams(datagrams) => {
                for (header, payload) in datagrams {
                    let frame = [abi::encode(&header), payload.to_vec()].concat();
                    let _ = connection.send_datagram(frame.into());
                }
            }
            Drained::Close => {
                // The last control message reaches a reading client before the link that owns
                // the connection closes it; one that stopped reading is cut off at LINGER.
                let _ = control.finish();
                let _ = tokio::time::timeout(LINGER, control.stopped()).await;
                return;
            }
        }
    }
}

/// Queues the object on its group's stream, opening one for a new group; a group whose queue
/// overflows is reset.
fn quic_group_object(
    connection: &quinn::Connection,
    pacer: &Pacer,
    groups: &mut Groups,
    header: GroupHeader,
    priority: Priority,
    body: Bytes,
) {
    let key = (header.alias, header.group);
    if groups.abandoned.contains(&key) {
        return;
    }
    let sending = groups.open.entry(key).or_insert_with(|| {
        let (sending, queue) = mpsc::channel(GROUP_QUEUE);
        tokio::spawn(quic_group_writing(
            connection.clone(),
            pacer.clone(),
            header,
            priority,
            queue,
        ));
        sending
    });
    let queued = sending.try_send(GroupOut::Object(body)).is_ok();
    if !queued {
        groups.open.remove(&key);
        groups.abandoned.insert(key);
    }
}

/// A whole cached group to a late joiner, as one queue item: the group's stream is opened and
/// the batch handed to it; a still-open group's stream stays for live objects to follow.
fn quic_group_batch(
    connection: &quinn::Connection,
    pacer: &Pacer,
    groups: &mut Groups,
    header: GroupHeader,
    priority: Priority,
    objects: Arc<Vec<Bytes>>,
    end: Option<bool>,
) {
    let key = (header.alias, header.group);
    if groups.abandoned.contains(&key) {
        return;
    }
    let sending = groups.open.entry(key).or_insert_with(|| {
        let (sending, queue) = mpsc::channel(GROUP_QUEUE);
        tokio::spawn(quic_group_writing(
            connection.clone(),
            pacer.clone(),
            header,
            priority,
            queue,
        ));
        sending
    });
    if sending.try_send(GroupOut::Batch(objects)).is_err() {
        groups.open.remove(&key);
        groups.abandoned.insert(key);
        return;
    }
    // A closed group ends here; an open one keeps its stream for the live objects that follow.
    if let Some(complete) = end {
        if let Some(sending) = groups.open.remove(&key) {
            let _ = sending.try_send(GroupOut::End { complete });
        }
    }
}

/// One egress group stream: opened at the track's priority, paced at the client class, finished
/// when the group completes and reset when it is abandoned or its queue overflows.
async fn quic_group_writing(
    connection: quinn::Connection,
    pacer: Pacer,
    header: GroupHeader,
    priority: Priority,
    mut queue: mpsc::Receiver<GroupOut>,
) {
    let Ok(stream) = connection.open_uni().await else {
        return;
    };
    let _ = stream.set_priority(stream_priority(priority));
    let mut paced = pacer.pace(stream, Class::Client);
    if paced.write_all(&abi::encode(&header)).await.is_err() {
        return;
    }
    loop {
        match queue.recv().await {
            Some(GroupOut::Object(body)) => {
                if framed(&mut paced, &body).await.is_err() {
                    return;
                }
            }
            Some(GroupOut::Batch(objects)) => {
                for body in objects.iter() {
                    if framed(&mut paced, body).await.is_err() {
                        return;
                    }
                }
            }
            Some(GroupOut::End { complete: true }) => {
                let _ = paced.shutdown().await;
                return;
            }
            Some(GroupOut::End { complete: false }) | None => {
                let _ = paced.into_inner().reset(0u32.into());
                return;
            }
        }
    }
}

// ---- websocket ----

/// The upgrade every `/v1/live` door runs: messages and frames capped at `WS_MESSAGE`.
pub fn ws_upgrade(upgrade: WebSocketUpgrade) -> WebSocketUpgrade {
    upgrade
        .max_message_size(WS_MESSAGE)
        .max_frame_size(WS_MESSAGE)
}

impl Link {
    pub fn ws(socket: WebSocket) -> Link {
        let (sink, stream) = socket.split();
        let (inbound, receiving) = mpsc::channel(INBOUND);
        let (outlet, egress) = outlet();
        let closing = egress.closing.clone();
        let readers = vec![tokio::spawn(ws_reading(stream, inbound))];
        let writer = tokio::spawn(ws_writing(sink, egress));
        Link {
            inbound: receiving,
            outlet,
            datagrams: false,
            path: "ws",
            closing,
            readers,
            writer: Some(writer),
            connection: None,
        }
    }
}

type WsStream = futures::stream::SplitStream<WebSocket>;
type WsSink = futures::stream::SplitSink<WebSocket, Message>;

async fn ws_reading(mut stream: WsStream, inbound: mpsc::Sender<Inbound>) {
    let mut counting: HashMap<(Alias, u64), u32> = HashMap::new();
    while let Some(message) = stream.next().await {
        let events = match message {
            Ok(Message::Binary(bytes)) => ws_arrived(bytes, &mut counting),
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            Ok(Message::Text(_)) => vec![Inbound::Garbled],
            Ok(Message::Close(_)) | Err(_) => vec![Inbound::Closed],
        };
        for event in events {
            let ends = matches!(event, Inbound::Closed | Inbound::Garbled);
            let delivered = inbound.send(event).await.is_ok();
            if ends || !delivered {
                return;
            }
        }
    }
    let _ = inbound.send(Inbound::Closed).await;
}

/// The caps QUIC's framing enforces, enforced on the websocket's: a control message within
/// `CONTROL_BYTES`, an object within `MAX_STREAM_OBJECT`.
fn ws_arrived(bytes: Bytes, counting: &mut HashMap<(Alias, u64), u32>) -> Vec<Inbound> {
    let Ok(frame) = abi::decode::<WsFrame>(&bytes) else {
        return vec![Inbound::Garbled];
    };
    match frame {
        WsFrame::Control(control) => {
            let fits = bytes.len() <= CONTROL_BYTES as usize;
            match fits {
                true => vec![Inbound::Control(control)],
                false => vec![Inbound::Garbled],
            }
        }
        WsFrame::Group {
            header,
            objects,
            end,
        } => {
            let key = (header.alias, header.group);
            let next = counting.entry(key).or_default();
            let mut events = Vec::new();
            for body in objects {
                let fits = body.len() <= MAX_STREAM_OBJECT;
                if !fits {
                    counting.remove(&key);
                    events.push(Inbound::Oversize {
                        alias: header.alias,
                        group: header.group,
                    });
                    // The group ends short, so no entry for it is left behind (C1-11).
                    events.push(Inbound::GroupEnd {
                        alias: header.alias,
                        group: header.group,
                        complete: false,
                    });
                    return events;
                }
                let object = *next;
                *next += 1;
                events.push(Inbound::Object {
                    header,
                    object,
                    body: body.into(),
                });
            }
            if end {
                counting.remove(&key);
                events.push(Inbound::GroupEnd {
                    alias: header.alias,
                    group: header.group,
                    complete: true,
                });
            }
            events
        }
        WsFrame::Datagram { header, payload } => vec![Inbound::Datagram {
            header,
            payload: payload.into(),
        }],
    }
}

async fn ws_writing(mut sink: WsSink, mut egress: Egress) {
    loop {
        let frames = match egress.next().await {
            Drained::Out(Out::Control(control)) => vec![WsFrame::Control(control)],
            Drained::Out(Out::Object { header, body, .. }) => vec![WsFrame::Group {
                header,
                objects: vec![body.to_vec()],
                end: false,
            }],
            // A replayed group is one frame: all its objects, ending it when the cache holds it
            // closed and complete; a still-open group's live objects follow in later frames.
            Drained::Out(Out::Group {
                header,
                objects,
                end,
                ..
            }) => vec![WsFrame::Group {
                header,
                objects: objects.iter().map(|body| body.to_vec()).collect(),
                end: end == Some(true),
            }],
            // The frame has no word for an abandoned group: a subscriber sees the next group
            // start instead.
            Drained::Out(Out::GroupEnd {
                alias,
                group,
                complete: true,
            }) => vec![WsFrame::Group {
                header: GroupHeader { alias, group },
                objects: Vec::new(),
                end: true,
            }],
            Drained::Out(Out::GroupEnd {
                complete: false, ..
            }) => Vec::new(),
            Drained::Out(Out::Datagram { .. }) => Vec::new(),
            Drained::Datagrams(datagrams) => datagrams
                .into_iter()
                .map(|(header, payload)| WsFrame::Datagram {
                    header,
                    payload: payload.to_vec(),
                })
                .collect(),
            Drained::Close => {
                let _ = sink.close().await;
                return;
            }
        };
        for frame in frames {
            let sent = sink.send(Message::Binary(abi::encode(&frame).into())).await;
            if sent.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_datagram_slot_keeps_the_newest_per_track() {
        let (outlet, egress) = outlet();
        let header = |object| DatagramHeader {
            alias: 1,
            group: 0,
            object,
        };
        assert_eq!(
            outlet.send(Out::Datagram {
                header: header(1),
                payload: Bytes::from_static(&[1])
            }),
            Sent::Queued
        );
        assert_eq!(
            outlet.send(Out::Datagram {
                header: header(2),
                payload: Bytes::from_static(&[2])
            }),
            Sent::Superseded
        );
        assert_eq!(
            egress.latest.take(),
            vec![(header(2), Bytes::from_static(&[2]))]
        );
    }

    #[test]
    fn an_ordered_queue_that_overflows_closes_the_connection_as_behind() {
        let (outlet, _egress) = outlet();
        let message = Out::Control(Control::GoAway { in_ms: 1 });
        for _ in 0..OUTBOUND {
            assert_eq!(outlet.send(message.clone()), Sent::Queued);
        }
        assert_eq!(outlet.send(message), Sent::Behind);
        assert_eq!(outlet.closed(), Some("behind"));
    }

    #[test]
    fn a_websocket_group_numbers_its_objects_across_slices() {
        let header = GroupHeader {
            alias: 3,
            group: 7,
        };
        let mut counting = HashMap::new();
        let first = ws_arrived(
            abi::encode(&WsFrame::Group {
                header,
                objects: vec![vec![1], vec![2]],
                end: false,
            })
            .into(),
            &mut counting,
        );
        let second = ws_arrived(
            abi::encode(&WsFrame::Group {
                header,
                objects: vec![vec![3]],
                end: true,
            })
            .into(),
            &mut counting,
        );
        let objects: Vec<u32> = first
            .iter()
            .chain(&second)
            .filter_map(|event| match event {
                Inbound::Object { object, .. } => Some(*object),
                _ => None,
            })
            .collect();
        assert_eq!(objects, vec![0, 1, 2]);
        assert!(matches!(
            second.last(),
            Some(Inbound::GroupEnd { complete: true, .. })
        ));
        assert!(counting.is_empty());
    }

    #[test]
    fn a_websocket_frame_over_the_wire_caps_is_garbled_or_oversize() {
        let mut counting = HashMap::new();
        let control = WsFrame::Control(Control::Watch {
            request: 1,
            namespace: "n".repeat(CONTROL_BYTES as usize),
        });
        assert!(matches!(
            ws_arrived(abi::encode(&control).into(), &mut counting).as_slice(),
            [Inbound::Garbled]
        ));
        let header = GroupHeader {
            alias: 3,
            group: 7,
        };
        let group = WsFrame::Group {
            header,
            objects: vec![vec![0; MAX_STREAM_OBJECT + 1]],
            end: true,
        };
        assert!(matches!(
            ws_arrived(abi::encode(&group).into(), &mut counting).as_slice(),
            [
                Inbound::Oversize { alias: 3, group: 7 },
                Inbound::GroupEnd {
                    alias: 3,
                    group: 7,
                    complete: false,
                },
            ]
        ));
    }
}
