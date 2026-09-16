//! Native editor projections of guest-owned documents. A key waits for its
//! decision, and an accepted edit waits for the guest's observed revision.
//! Transfer assemblers and patch validation are the wire contract's own code.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use ui_lang_wire as wire;
use wire::editor_document::{
    EditorDocumentMessage as DocumentMessage, EditorDocumentRef, EditorTransferId,
    EditorTransferReceiver, EditorTransferSender,
    MAX_EDITOR_LIVE_BYTES, MAX_EDITOR_PROJECTION_BYTES, editor_changed_span,
};

#[derive(Clone)]
pub struct EditorStore(Arc<Mutex<Store>>);

struct Store {
    instance: u64,
    serial: u64,
    epoch: Instant,
    fields: HashMap<String, Field>,
    documents: HashMap<String, Document>,
    incoming: Option<Incoming>,
    outgoing: Option<Outgoing>,
    events: Vec<wire::Event>,
    fault: Option<String>,
}

#[derive(Clone)]
struct Field {
    reference: EditorDocumentRef,
    handler: u32,
    options: wire::EditorOptions,
    placeholder: String,
    editable: bool,
}

struct Document {
    reference: EditorDocumentRef,
    text: Option<Arc<str>>,
    queue: VecDeque<Work>,
    queued_bytes: usize,
    phase: Phase,
}

enum Phase {
    Ready,
    Decision { request: wire::EditorRequest, since: Instant },
    Acknowledgment { revision: u64 },
    Fault,
}

struct Work {
    key: String,
    sequence: u64,
    at: u64,
    input: Input,
}

enum Input {
    Native(NativeEdit),
    Request(wire::EditorRequestInput),
}

/// Offsets relative to the previous caret let typing queued behind a structural
/// guest key follow that key's new caret, rather than overwrite its result.
struct NativeEdit {
    start: isize,
    end: isize,
    replacement: String,
    caret: isize,
    anchor: Option<isize>,
    kind: wire::EditorEditKind,
}

struct Incoming {
    id: EditorTransferId,
    target: EditorDocumentRef,
    handler: u32,
    receiver: EditorTransferReceiver,
}

struct Outgoing {
    handler: u32,
    sender: EditorTransferSender,
}

#[derive(Clone)]
struct Projection {
    reference: EditorDocumentRef,
    text: Option<Arc<str>>,
    options: wire::EditorOptions,
    placeholder: String,
    editable: bool,
    pending: bool,
    fault: Option<String>,
}

impl EditorStore {
    pub fn new(instance: u64) -> Self {
        Self(Arc::new(Mutex::new(Store { instance, serial: 0, epoch: Instant::now(),
            fields: HashMap::new(), documents: HashMap::new(), incoming: None,
            outgoing: None, events: Vec::new(), fault: None })))
    }

    fn lock(&self) -> MutexGuard<'_, Store> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Validate the complete candidate before changing the accepted projections.
    pub fn validate(&self, root: &wire::Node) -> Result<(), String> {
        let mut fields = HashMap::new();
        collect(root, &mut fields)?;
        wire::editor_document::validate_editor_document_refs(fields.values().map(|field| &field.reference))
            .map_err(|error| format!("invalid editor references: {error:?}"))?;
        self.lock().validate_budget(&fields)
    }

    /// A replacement may reuse immutable document bytes only when its restored
    /// reference names exactly the same projection. The old store is untouched.
    pub fn retain_restored_projections(&self, old: &Self, root: &wire::Node) -> Result<(), String> {
        self.validate(root)?;
        if old.pending() { return Err("the previous editor has pending work".into()); }
        if !self.ready()? { return Err("replacement editor documents are incomplete".into()); }
        let old = old.lock();
        old.check()?;
        let mut restored = self.lock();
        for (id, document) in &mut restored.documents {
            let Some(previous) = old.documents.get(id) else { continue; };
            let identical = document.reference == previous.reference && document.text == previous.text;
            if identical { document.text = previous.text.clone(); }
        }
        restored.check()
    }

    pub fn replace(&self, root: &wire::Node) -> Result<(), String> {
        let mut fields = HashMap::new();
        collect(root, &mut fields)?;
        wire::editor_document::validate_editor_document_refs(fields.values().map(|f| &f.reference))
            .map_err(|error| format!("invalid editor references: {error:?}"))?;
        let mut store = self.lock();
        store.validate_budget(&fields)?;
        store.replace(fields);
        store.check()
    }

    /// Called after adopting the complete root, including unchanged-tree frames.
    pub fn frame(&self, frame: &wire::Frame) -> Result<(), String> {
        let mut store = self.lock();
        if !frame.busy { store.acknowledge(); }
        for message in &frame.editor_documents { store.document_message(message); }
        for response in &frame.editor_decisions { store.decide(response); }
        store.pump();
        store.check()
    }

    pub fn drain(&self) -> Vec<wire::Event> {
        let mut store = self.lock();
        store.pump();
        std::mem::take(&mut store.events)
    }

    pub fn ready(&self) -> Result<bool, String> {
        let store = self.lock();
        store.check()?;
        Ok(store.incoming.is_none() && store.documents.values().all(|d| d.text.is_some()))
    }

    pub fn pending(&self) -> bool {
        let store = self.lock();
        store.incoming.is_some() || store.outgoing.is_some() || !store.events.is_empty()
            || store.documents.values().any(|d| !d.queue.is_empty() || !matches!(d.phase, Phase::Ready))
    }

    fn projection(&self, key: &str) -> Option<Projection> {
        let store = self.lock();
        let field = store.fields.get(key)?;
        let document = store.documents.get(&field.reference.document)?;
        Some(Projection { reference: document.reference.clone(), text: document.text.clone(),
            options: field.options.clone(), placeholder: field.placeholder.clone(),
            editable: field.editable, pending: !document.queue.is_empty(), fault: store.fault.clone() })
    }

    fn request(&self, key: &str, input: wire::EditorRequestInput) {
        let mut store = self.lock();
        store.enqueue(key, Input::Request(input));
        store.pump();
    }

    // NativeModuleView applies the guest's event-interest mask on dispatch.
    // These observations never mutate the authoritative editor document.
    fn observe_ime(&self, events: Vec<wire::Event>) {
        self.lock().events.extend(events);
    }

    fn native(&self, key: &str, before: &str, previous: wire::EditorCursor,
        after: &str, next: wire::EditorCursor, kind: wire::EditorEditKind) {
        let result = native_edit(before, previous, after, next, kind);
        let mut store = self.lock();
        match result {
            Ok(edit) => store.enqueue(key, Input::Native(edit)),
            Err(error) => store.fault = Some(error),
        }
        store.pump();
    }
}

fn collect(node: &wire::Node, fields: &mut HashMap<String, Field>) -> Result<(), String> {
    if let wire::Node::Editor { key, document, on_document, options, placeholder, editable, .. } = node {
        let duplicate = fields.insert(key.clone(), Field { reference: document.clone(),
            handler: *on_document, options: (**options).clone(), placeholder: placeholder.clone(), editable: *editable }).is_some();
        if duplicate { return Err("duplicate editor projection key".into()); }
    }
    for child in node.children() { collect(child, fields)?; }
    Ok(())
}

impl Store {
    fn check(&self) -> Result<(), String> {
        self.fault.clone().map_or(Ok(()), Err)
    }

    fn next(&mut self) -> u64 {
        self.serial = self.serial.checked_add(1).expect("editor sequence exhausted");
        self.serial
    }

    fn validate_budget(&self, fields: &HashMap<String, Field>) -> Result<(), String> {
        let mut logical = HashMap::new();
        let mut projections = 0usize;
        for field in fields.values() {
            let reference = &field.reference;
            let bytes = self.documents.get(&reference.document)
                .filter(|d| d.reference.reset == reference.reset)
                .and_then(|d| d.text.as_ref()).map_or(reference.byte_len as usize, |text| text.len());
            logical.insert(reference.document.clone(), bytes);
            projections = projections.checked_add(bytes).ok_or("editor projection budget")?;
        }
        let total = logical.values().try_fold(0usize, |sum, n| sum.checked_add(*n))
            .ok_or("editor document budget")?;
        let exceeds_budget = total > MAX_EDITOR_LIVE_BYTES || projections > MAX_EDITOR_PROJECTION_BYTES;
        if exceeds_budget { return Err("editor live document budget exceeded".into()); }
        Ok(())
    }

    fn replace(&mut self, fields: HashMap<String, Field>) {
        let removed: Vec<_> = self.documents.keys().filter(|name|
            !fields.values().any(|f| &f.reference.document == *name)).cloned().collect();
        for name in removed { self.cancel(&name); self.documents.remove(&name); }
        for field in fields.values() {
            let reference = &field.reference;
            let changed_reset = self.documents.get(&reference.document)
                .is_some_and(|d| d.reference.reset != reference.reset);
            if changed_reset { self.cancel(&reference.document); self.documents.remove(&reference.document); }
            self.documents.entry(reference.document.clone()).or_insert_with(|| Document {
                reference: reference.clone(), text: None, queue: VecDeque::new(), queued_bytes: 0, phase: Phase::Ready });
        }
        self.fields = fields;
        let stale_incoming = self.incoming.as_ref().is_some_and(|incoming|
            self.documents.get(&incoming.id.document).is_none_or(|d| d.reference.reset != incoming.id.reset));
        if stale_incoming { self.incoming = None; }
        let stale_outgoing = self.outgoing.as_ref().is_some_and(|outgoing|
            self.documents.get(&outgoing.sender.id().document).is_none_or(|d| d.reference.reset != outgoing.sender.id().reset));
        if stale_outgoing { self.outgoing = None; }
    }

    fn cancel(&mut self, name: &str) {
        let Some(document) = self.documents.get_mut(name) else { return; };
        for work in document.queue.drain(..) {
            let Some(binding) = self.fields.get(&work.key).and_then(|f| f.options.binding.as_ref()) else { continue; };
            self.events.push(wire::Event::EditorTransaction { handler: binding.on_event,
                event: wire::EditorTransactionEvent::Cancelled {
                    id: transaction_id(self.instance, &document.reference, work.sequence), state: document.reference.clone() } });
        }
        document.queued_bytes = 0;
    }

    fn enqueue(&mut self, key: &str, input: Input) {
        if self.fault.is_some() { return; }
        let Some(field) = self.fields.get(key).cloned() else { return; };
        let allowed = field.editable || matches!(input, Input::Request(wire::EditorRequestInput::Interaction { .. }));
        if !allowed { return; }
        let sequence = self.next();
        let at = self.epoch.elapsed().as_millis() as u64;
        let Some(document) = self.documents.get_mut(&field.reference.document) else { return; };
        if document.text.is_none() { return; }
        let bytes = match &input { Input::Native(edit) => edit.replacement.len(), Input::Request(request) => wire::encode(request).len() };
        let overflow = document.queue.len() >= 128 || document.queued_bytes.saturating_add(bytes) > wire::editor_transaction::MAX_EDITOR_INPUT_BYTES;
        if overflow {
            self.fault = Some("editor input queue is full; document retained".into());
            self.events.push(wire::Event::EditorTransaction { handler: field.options.binding.as_ref().map_or(0, |b| b.on_event),
                event: wire::EditorTransactionEvent::Fault {
                    id: transaction_id(self.instance, &document.reference, sequence), state: document.reference.clone(), reason: wire::EditorFault::Overflow } });
            return;
        }
        document.queued_bytes += bytes;
        document.queue.push_back(Work { key: key.into(), sequence, at, input });
    }

    fn pump(&mut self) {
        if self.fault.is_some() { return; }
        self.send_mirror();
        self.request_document();
        let names: Vec<_> = self.documents.keys().cloned().collect();
        for name in names { self.pump_document(&name); }
    }

    fn request_document(&mut self) {
        let transfer_active = self.incoming.is_some() || self.outgoing.is_some();
        if transfer_active { return; }
        let Some(field) = self.fields.values().find(|f| self.documents.get(&f.reference.document).is_some_and(|d| d.text.is_none())).cloned() else { return; };
        let id = EditorTransferId { instance: self.instance, document: field.reference.document.clone(), reset: field.reference.reset, serial: self.next(), attempt: 0 };
        let target = field.reference;
        let receiver = match EditorTransferReceiver::new(id.clone(), target.clone()) {
            Ok(receiver) => receiver,
            Err(error) => { self.fault = Some(format!("invalid editor transfer: {error:?}")); return; }
        };
        self.events.push(wire::Event::EditorDocument { handler: field.handler,
            message: DocumentMessage::Request { id: id.clone(), target: target.clone() } });
        self.incoming = Some(Incoming { id, target, handler: field.handler, receiver });
    }

    fn pump_document(&mut self, name: &str) {
        let Some(document) = self.documents.get_mut(name) else { return; };
        if let Phase::Decision { since, .. } = &document.phase {
            if since.elapsed().as_secs() >= 5 {
                document.phase = Phase::Fault;
                self.fault = Some("editor key decision timed out; input retained".into());
            }
            return;
        }
        if !matches!(document.phase, Phase::Ready) { return; }
        let Some(front) = document.queue.front() else { return; };
        let Some(field) = self.fields.get(&front.key) else { return; };
        let Some(binding) = field.options.binding.as_ref() else {
            self.fault = Some("editor has no guest transaction binding".into()); return;
        };
        match &front.input {
            Input::Request(input) => {
                let request = wire::EditorRequest { id: transaction_id(self.instance, &document.reference, front.sequence),
                    state: document.reference.clone(), input: input.clone(), input_time_ms: front.at };
                self.events.push(wire::Event::EditorRequest { handler: binding.on_request, request: request.clone() });
                document.phase = Phase::Decision { request, since: Instant::now() };
            }
            Input::Native(edit) => {
                let Some(text) = document.text.as_ref() else { return; };
                let change = apply_native(text, document.reference.cursor, edit);
                match change {
                    Ok((patches, cursor)) => self.commit(name, patches, cursor, wire::EditorHistoryEffect::Native, None),
                    Err(error) => self.fault = Some(error),
                }
            }
        }
    }

    fn commit(&mut self, name: &str, patches: Vec<wire::EditorPatch>, cursor: wire::EditorCursor,
        history: wire::EditorHistoryEffect, origin: Option<wire::EditorRequestInput>) {
        let Some(document) = self.documents.get(name) else { return; };
        let Some(text) = document.text.as_ref() else { return; };
        let next = match wire::patched_editor_text(text, &patches, cursor) {
            Ok(text) => text,
            Err(error) => { self.fault = Some(format!("invalid editor decision: {error:?}")); return; }
        };
        let new_len = next.len();
        let mut candidate = self.fields.clone();
        for field in candidate.values_mut().filter(|f| f.reference.document == name) { field.reference.byte_len = new_len as u32; }
        let others = self.documents.iter().filter(|(key, _)| key.as_str() != name)
            .map(|(_, d)| d.text.as_ref().map_or(d.reference.byte_len as usize, |t| t.len())).sum::<usize>();
        let projected = candidate.values().map(|f| if f.reference.document == name { new_len } else {
            self.documents.get(&f.reference.document).and_then(|d| d.text.as_ref()).map_or(f.reference.byte_len as usize, |t| t.len()) }).sum::<usize>();
        let exceeds = others + new_len > MAX_EDITOR_LIVE_BYTES || projected > MAX_EDITOR_PROJECTION_BYTES;
        if exceeds { self.fault = Some("editor edit exceeds live document budget".into()); return; }
        let document = self.documents.get_mut(name).expect("document checked");
        let Some(front) = document.queue.front() else { return; };
        let Some(binding) = self.fields.get(&front.key).and_then(|f| f.options.binding.as_ref()) else { return; };
        let before = document.reference.clone();
        let changed = document.text.as_ref().is_some_and(|t| t.as_ref() != next);
        let mut after = before.clone();
        after.cursor = cursor;
        after.byte_len = next.len() as u32;
        after.revision = after.revision.saturating_add(1);
        if changed { after.text_revision = after.text_revision.saturating_add(1); }
        let kind = match &front.input {
            Input::Native(edit) => edit.kind,
            Input::Request(_) => match history { wire::EditorHistoryEffect::Undo => wire::EditorEditKind::Undo,
                wire::EditorHistoryEffect::Redo => wire::EditorEditKind::Redo, _ => wire::EditorEditKind::GuestPatch },
        };
        self.events.push(wire::Event::EditorTransaction { handler: binding.on_event,
            event: wire::EditorTransactionEvent::Commit { id: transaction_id(self.instance, &before, front.sequence),
                origin, before, after: after.clone(), patches, kind, history, input_time_ms: front.at } });
        document.text = Some(Arc::from(next));
        document.reference = after.clone();
        document.phase = Phase::Acknowledgment { revision: after.revision };
    }

    fn acknowledge(&mut self) {
        for document in self.documents.values_mut() {
            let Phase::Acknowledgment { revision } = document.phase else { continue; };
            let observed = self.fields.values().any(|f| f.reference.document == document.reference.document
                && f.reference.reset == document.reference.reset && f.reference.revision >= revision);
            if !observed { continue; }
            if let Some(work) = document.queue.pop_front() {
                let bytes = match work.input { Input::Native(edit) => edit.replacement.len(), Input::Request(request) => wire::encode(&request).len() };
                document.queued_bytes = document.queued_bytes.saturating_sub(bytes);
            }
            document.phase = Phase::Ready;
        }
    }

    fn decide(&mut self, response: &wire::EditorResponse) {
        let name = &response.id.document;
        let Some(document) = self.documents.get(name) else { return; };
        let Phase::Decision { request, .. } = &document.phase else { return; };
        if response.id != request.id { return; }
        let request = request.clone();
        match &response.decision {
            wire::EditorDecision::Apply { patches, cursor, history } => self.commit(name, patches.clone(), *cursor, *history, Some(request.input)),
            wire::EditorDecision::Noop => self.noop(name, request),
            wire::EditorDecision::DefaultEditorAction => self.default_action(name, request),
        }
    }

    fn noop(&mut self, name: &str, request: wire::EditorRequest) {
        let wire::EditorRequestInput::Interaction { action } = &request.input else {
            self.commit(name, vec![], request.state.cursor, wire::EditorHistoryEffect::Native, Some(request.input)); return;
        };
        let Some(document) = self.documents.get_mut(name) else { return; };
        let Some(front) = document.queue.front() else { return; };
        let Some(binding) = self.fields.get(&front.key).and_then(|f| f.options.binding.as_ref()) else { return; };
        self.events.push(wire::Event::EditorTransaction { handler: binding.on_event,
            event: wire::EditorTransactionEvent::Interaction { id: request.id, state: request.state,
                action: action.clone(), input_time_ms: request.input_time_ms } });
        document.phase = Phase::Acknowledgment { revision: document.reference.revision };
    }

    fn default_action(&mut self, name: &str, request: wire::EditorRequest) {
        let Some(document) = self.documents.get(name) else { return; };
        let Some(text) = document.text.as_ref() else { return; };
        let wire::EditorRequestInput::Key { key, .. } = &request.input else {
            self.fault = Some("editor interaction cannot request a native key action".into()); return;
        };
        match native_key(text, document.reference.cursor, key) {
            Ok((patches, cursor)) => self.commit(name, patches, cursor, wire::EditorHistoryEffect::Native, Some(request.input)),
            Err(error) => self.fault = Some(error),
        }
    }

    fn document_message(&mut self, message: &DocumentMessage) {
        match message {
            DocumentMessage::Request { id, target } => self.mirror_requested(id, target),
            DocumentMessage::Transfer(transfer) => self.transferred(transfer),
            DocumentMessage::Acknowledged { id } => self.mirror_acknowledged(id),
            DocumentMessage::Failed { id, reason } => self.transfer_failed(id, *reason),
        }
    }

    fn mirror_requested(&mut self, id: &EditorTransferId, target: &EditorDocumentRef) {
        if self.outgoing.is_some() || self.incoming.is_some() { return; }
        let Some(document) = self.documents.get(&id.document) else { return; };
        let Phase::Decision { request, .. } = &document.phase else { return; };
        let accepted = id.instance == self.instance && id.serial == request.id.sequence
            && id.attempt == request.id.attempt && id.reset == document.reference.reset && target == &document.reference;
        if !accepted { return; }
        let Some(front) = document.queue.front() else { return; };
        let Some(field) = self.fields.get(&front.key) else { return; };
        match EditorTransferSender::new(id.clone(), target.clone()) {
            Ok(sender) => self.outgoing = Some(Outgoing { handler: field.handler, sender }),
            Err(error) => self.fault = Some(format!("invalid editor mirror request: {error:?}")),
        }
    }

    fn send_mirror(&mut self) {
        let Some(outgoing) = &mut self.outgoing else { return; };
        let Some(document) = self.documents.get(&outgoing.sender.id().document) else { self.outgoing = None; return; };
        let Some(text) = document.text.as_ref() else { return; };
        match outgoing.sender.next_frame(&document.reference, text) {
            Ok(Some(transfer)) => self.events.push(wire::Event::EditorDocument { handler: outgoing.handler, message: DocumentMessage::Transfer(transfer) }),
            Ok(None) => {},
            Err(error) => self.fault = Some(format!("editor mirror failed: {error:?}")),
        }
    }

    fn transferred(&mut self, transfer: &wire::editor_document::EditorTransfer) {
        let Some(incoming) = &mut self.incoming else { return; };
        if transfer.id() != &incoming.id { return; }
        match incoming.receiver.receive(transfer) {
            Ok(Some(text)) => {
                let incoming = self.incoming.take().expect("matching incoming transfer");
                let Some(document) = self.documents.get_mut(&incoming.id.document) else { return; };
                document.text = Some(Arc::from(text));
                document.reference = incoming.target;
                self.events.push(wire::Event::EditorDocument { handler: incoming.handler,
                    message: DocumentMessage::Acknowledged { id: incoming.id } });
            }
            Ok(None) => {},
            Err(error) => self.fault = Some(format!("editor document transfer failed: {error:?}")),
        }
    }

    fn mirror_acknowledged(&mut self, id: &EditorTransferId) {
        let matches = self.outgoing.as_ref().is_some_and(|outgoing| outgoing.sender.id() == id);
        if matches { self.outgoing = None; }
    }

    fn transfer_failed(&mut self, id: &EditorTransferId, reason: wire::editor_document::EditorTransferError) {
        let current = self.incoming.as_ref().is_some_and(|incoming| &incoming.id == id)
            || self.outgoing.as_ref().is_some_and(|outgoing| outgoing.sender.id() == id);
        if current { self.fault = Some(format!("editor document transfer refused: {reason:?}")); }
    }
}

fn transaction_id(instance: u64, state: &EditorDocumentRef, sequence: u64) -> wire::EditorTransactionId {
    wire::EditorTransactionId { instance, document: state.document.clone(), reset: state.reset,
        sequence, attempt: 0, text_revision: state.text_revision, revision: state.revision }
}

fn offset(text: &str, position: wire::EditorPosition) -> usize {
    let Some(line) = wire::editor_lines(text).nth(position.line as usize) else { return text.len(); };
    let start = line.as_ptr() as usize - text.as_ptr() as usize;
    start + (position.column as usize).min(line.len())
}

fn position(text: &str, mut at: usize) -> wire::EditorPosition {
    at = at.min(text.len());
    while !text.is_char_boundary(at) { at -= 1; }
    let (index, source) = wire::editor_lines(text).enumerate().take_while(|(_, line)| {
        line.as_ptr() as usize - text.as_ptr() as usize <= at
    }).last().expect("editor has at least one logical line");
    let start = source.as_ptr() as usize - text.as_ptr() as usize;
    wire::EditorPosition { line: index as u32, column: (at - start).min(source.len()) as u32 }
}

fn native_edit(before: &str, previous: wire::EditorCursor, after: &str, next: wire::EditorCursor,
    kind: wire::EditorEditKind) -> Result<NativeEdit, String> {
    let patches = editor_changed_span(before, after).map_err(|error| format!("native editor edit refused: {error:?}"))?;
    let caret = offset(before, previous.position) as isize;
    let patch = patches.into_iter().next().unwrap_or(wire::EditorPatch { start_byte: caret as u32, end_byte: caret as u32, replacement: String::new() });
    Ok(NativeEdit { start: patch.start_byte as isize - caret, end: patch.end_byte as isize - caret,
        caret: offset(after, next.position) as isize - patch.start_byte as isize,
        anchor: next.selection.map(|p| offset(after, p) as isize - patch.start_byte as isize),
        replacement: patch.replacement, kind })
}

fn apply_native(text: &str, cursor: wire::EditorCursor, edit: &NativeEdit) -> Result<(Vec<wire::EditorPatch>, wire::EditorCursor), String> {
    let caret = offset(text, cursor.position) as isize;
    let start = caret.checked_add(edit.start).filter(|n| *n >= 0).ok_or("queued editor range before document")? as usize;
    let end = caret.checked_add(edit.end).filter(|n| *n >= 0).ok_or("queued editor range before document")? as usize;
    let valid = start <= end && end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end);
    if !valid { return Err("queued editor range no longer valid; input retained".into()); }
    let patch = wire::EditorPatch { start_byte: start as u32, end_byte: end as u32, replacement: edit.replacement.clone() };
    let mut next = String::with_capacity(text.len() - (end - start) + edit.replacement.len());
    next.push_str(&text[..start]); next.push_str(&edit.replacement); next.push_str(&text[end..]);
    let mut cursor = wire::EditorCursor { position: position(&next, (start as isize + edit.caret).max(0) as usize),
        selection: edit.anchor.map(|anchor| position(&next, (start as isize + anchor).max(0) as usize)) };
    cursor.clamp(&next);
    let patches = if start == end && edit.replacement.is_empty() { vec![] } else { vec![patch] };
    Ok((patches, cursor))
}

fn native_key(text: &str, cursor: wire::EditorCursor, key: &wire::keyboard::KeyState) -> Result<(Vec<wire::EditorPatch>, wire::EditorCursor), String> {
    use wire::keyboard::{Key, Named};
    let caret = offset(text, cursor.position);
    let anchor = cursor.selection.map_or(caret, |p| offset(text, p));
    let mut start = caret.min(anchor);
    let end = caret.max(anchor);
    let replacement = match &key.key {
        Key::Named(Named::Enter) => "\n",
        Key::Named(Named::Tab) => "\t",
        Key::Named(Named::Backspace) => {
            if start == end && start > 0 {
                start -= 1;
                while start > 0 {
                    let patch = wire::EditorPatch { start_byte: start as u32, end_byte: end as u32, replacement: String::new() };
                    if text.is_char_boundary(start) && wire::patched_editor_text(text, &[patch], wire::EditorCursor::default()).is_ok() { break; }
                    start -= 1;
                }
            }
            ""
        }
        _ => return Err("guest requested unsupported native editor key".into()),
    };
    let mut next = text[..start].to_owned(); next.push_str(replacement); next.push_str(&text[end..]);
    let cursor = wire::EditorCursor { position: position(&next, start + replacement.len()), selection: None };
    Ok((vec![wire::EditorPatch { start_byte: start as u32, end_byte: end as u32, replacement: replacement.into() }], cursor))
}

/// One native keystroke as the guest's key state. Both mounts read the same
/// description: a chord a guest claimed is the same chord in either.
pub(super) fn key_state(key: &gpui_kit::Keystroke) -> wire::keyboard::KeyState {
    use wire::keyboard::{Key, Named};
    let logical = match key.key.as_str() {
        "enter" => Key::Named(Named::Enter),
        "tab" => Key::Named(Named::Tab),
        "backspace" => Key::Named(Named::Backspace),
        "delete" => Key::Named(Named::Delete),
        "escape" => Key::Named(Named::Escape),
        "up" => Key::Named(Named::ArrowUp),
        "down" => Key::Named(Named::ArrowDown),
        "left" => Key::Named(Named::ArrowLeft),
        "right" => Key::Named(Named::ArrowRight),
        _ => Key::Character(key.key.clone()),
    };
    wire::keyboard::KeyState {
        key: logical.clone(),
        modified_key: logical,
        physical_key: wire::keyboard::Physical::Unidentified(
            wire::keyboard::NativeCode::Unidentified,
        ),
        location: wire::keyboard::Location::Standard,
        modifiers: wire::keyboard::Modifiers {
            shift: key.modifiers.shift,
            control: key.modifiers.control,
            alt: key.modifiers.alt,
            logo: key.modifiers.platform,
        },
    }
}

#[path = "notion.rs"]
mod notion;
#[path = "text.rs"]
mod text;
pub use notion::{NOTION_DOCUMENT_KEY, NotionWireEditor, init as init_notion};
pub use text::{GUEST_EDITOR_CONTEXT, TextEditor};
