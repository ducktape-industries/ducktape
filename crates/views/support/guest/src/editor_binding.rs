//! Guest-local decisions borrow the canonical document owned by application state.
use crate::{Editor, slots, wire};
use std::rc::Rc;

pub use wire::EditorDecision;

#[derive(Clone, Copy, Debug)]
pub struct EditorStateView<'a> {
    pub text: &'a str,
    pub cursor: wire::EditorCursor,
    pub reset: u64,
    pub text_revision: u64,
    pub revision: u64,
}
impl<'a> EditorStateView<'a> {
    fn new(text: &'a str, reference: &wire::editor_document::EditorDocumentRef) -> Self {
        Self {
            text,
            cursor: reference.cursor,
            reset: reference.reset,
            text_revision: reference.text_revision,
            revision: reference.revision,
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct EditorKeyRequest<'a> {
    pub id: &'a wire::EditorTransactionId,
    pub state: EditorStateView<'a>,
    pub key: &'a wire::keyboard::KeyState,
    pub repeat: bool,
    pub input_time_ms: u64,
}
#[derive(Clone, Copy, Debug)]
pub struct EditorInteractionRequest<'a> {
    pub id: &'a wire::EditorTransactionId,
    pub state: EditorStateView<'a>,
    pub action: &'a wire::editor_presentation::EditorInteraction,
    pub input_time_ms: u64,
}
#[derive(Clone, Copy, Debug)]
pub enum EditorTransactionEvent<'a> {
    Interaction {
        id: &'a wire::EditorTransactionId,
        state: EditorStateView<'a>,
        action: &'a wire::editor_presentation::EditorInteraction,
        input_time_ms: u64,
    },
    Commit {
        id: &'a wire::EditorTransactionId,
        origin: Option<&'a wire::EditorRequestInput>,
        before: EditorStateView<'a>,
        after: EditorStateView<'a>,
        kind: wire::EditorEditKind,
        history: wire::EditorHistoryEffect,
        input_time_ms: u64,
    },
    Fault {
        id: &'a wire::EditorTransactionId,
        reason: wire::EditorFault,
    },
    Cancelled {
        id: &'a wire::EditorTransactionId,
    },
}

type Decide = Rc<dyn for<'a> Fn(EditorKeyRequest<'a>) -> EditorDecision>;
type Interact = Rc<dyn for<'a> Fn(EditorInteractionRequest<'a>) -> EditorDecision>;
type Observe<P> = Rc<dyn for<'a> Fn(EditorTransactionEvent<'a>) -> Option<P>>;
pub struct EditorBinding<P> {
    claims: Vec<wire::EditorKeyClaim>,
    decide: Decide,
    interact: Option<Interact>,
    on_event: Observe<P>,
}
struct Callbacks<M> {
    decide: Decide,
    interact: Option<Interact>,
    on_event: Observe<M>,
}
impl<P: 'static> EditorBinding<P> {
    pub fn new(
        claims: Vec<wire::EditorKeyClaim>,
        decide: impl for<'a> Fn(EditorKeyRequest<'a>) -> EditorDecision + 'static,
        on_event: impl for<'a> Fn(EditorTransactionEvent<'a>) -> Option<P> + 'static,
    ) -> Self {
        assert!(
            claims.len() <= wire::editor_transaction::MAX_EDITOR_CLAIMS,
            "editor claim limit"
        );
        Self {
            claims,
            decide: Rc::new(decide),
            interact: None,
            on_event: Rc::new(on_event),
        }
    }
    pub fn on_interaction(
        mut self,
        decide: impl for<'a> Fn(EditorInteractionRequest<'a>) -> EditorDecision + 'static,
    ) -> Self {
        self.interact = Some(Rc::new(decide));
        self
    }
    pub fn register<M: 'static>(
        self,
        route: impl Fn(P) -> M + 'static,
        wrap: impl Fn(EditorTransaction<M>) -> M + 'static,
    ) -> wire::EditorBinding {
        let observe = self.on_event;
        let callbacks = Rc::new(Callbacks {
            decide: self.decide,
            interact: self.interact,
            on_event: Rc::new(move |event| observe(event).map(&route)),
        });
        // Existing handler storage already supplies bounded frame-local lifetime
        // and memo capture. No second callback registry or copied document.
        let map =
            slots::handler::<(), Rc<Callbacks<M>>>(Box::new(move |()| Some(callbacks.clone())));
        let wrap = Rc::new(wrap);
        let request_wrap = wrap.clone();
        let on_request = slots::handler::<wire::EditorRequest, M>(Box::new(move |request| {
            Some(request_wrap(EditorTransaction {
                event: Transaction::Request(request),
                map,
                message: std::marker::PhantomData,
            }))
        }));
        let on_event = slots::handler::<wire::EditorTransactionEvent, M>(Box::new(move |event| {
            Some(wrap(EditorTransaction {
                event: Transaction::Event(event),
                map,
                message: std::marker::PhantomData,
            }))
        }));
        wire::EditorBinding {
            authored: true,
            claims: self.claims,
            on_request,
            on_event,
        }
    }
}
impl EditorBinding<()> {
    pub fn plain<M: 'static>(
        wrap: impl Fn(EditorTransaction<M>) -> M + 'static,
    ) -> wire::EditorBinding {
        let mut binding = EditorBinding::<M>::new(
            Vec::new(),
            |_| EditorDecision::DefaultEditorAction,
            |_| None,
        )
        .register(std::convert::identity, wrap);
        binding.authored = false;
        binding
    }
}
#[derive(Clone, Debug)]
enum Transaction {
    Request(wire::EditorRequest),
    Event(wire::EditorTransactionEvent),
}
pub struct EditorTransaction<M> {
    event: Transaction,
    map: u32,
    message: std::marker::PhantomData<fn() -> M>,
}
impl<M> Clone for EditorTransaction<M> {
    fn clone(&self) -> Self {
        Self {
            event: self.event.clone(),
            map: self.map,
            message: std::marker::PhantomData,
        }
    }
}
impl<M> std::fmt::Debug for EditorTransaction<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("EditorTransaction")
            .field(&self.event)
            .finish()
    }
}
impl<M: 'static> EditorTransaction<M> {
    pub fn apply(self, editor: &mut Editor) -> Option<M> {
        let callbacks = slots::run_handler::<(), Rc<Callbacks<M>>>(self.map, ())?;
        match self.event {
            Transaction::Request(request) => {
                if !slots::editor_request_current(&request.id) {
                    return None;
                }
                if editor.document_reference(request.id.document.clone()) != request.state {
                    if request.state.reset == editor.reset_revision()
                        && let Err(reason) = slots::request_editor_mirror(&request)
                    {
                        slots::editor_document_failure(
                            wire::editor_document::EditorTransferId {
                                instance: request.id.instance,
                                document: request.id.document.clone(),
                                reset: request.id.reset,
                                serial: request.id.sequence,
                                attempt: request.id.attempt,
                            },
                            reason,
                        );
                    }
                    return None;
                }
                let state = EditorStateView::new(editor.text_ref(), &request.state);
                let decision = match &request.input {
                    wire::EditorRequestInput::Key { key, repeat } => {
                        (callbacks.decide)(EditorKeyRequest {
                            id: &request.id,
                            state,
                            key,
                            repeat: *repeat,
                            input_time_ms: request.input_time_ms,
                        })
                    }
                    wire::EditorRequestInput::Interaction { action } => callbacks
                        .interact
                        .as_ref()
                        .map_or(EditorDecision::Noop, |decide| {
                            decide(EditorInteractionRequest {
                                id: &request.id,
                                state,
                                action,
                                input_time_ms: request.input_time_ms,
                            })
                        }),
                };
                slots::editor_response(wire::EditorResponse {
                    id: request.id,
                    decision,
                });
                None
            }
            Transaction::Event(event) => {
                let id = match &event {
                    wire::EditorTransactionEvent::Interaction { id, .. }
                    | wire::EditorTransactionEvent::Commit { id, .. }
                    | wire::EditorTransactionEvent::Fault { id, .. }
                    | wire::EditorTransactionEvent::Cancelled { id, .. } => id,
                };
                if !slots::editor_matches_pending(id) {
                    return None;
                }
                let mapped = match &event {
                    wire::EditorTransactionEvent::Interaction {
                        state,
                        action,
                        input_time_ms,
                        ..
                    } => {
                        if editor.document_reference(id.document.clone()) != *state {
                            return None;
                        }
                        (callbacks.on_event)(EditorTransactionEvent::Interaction {
                            id,
                            state: EditorStateView::new(editor.text_ref(), state),
                            action,
                            input_time_ms: *input_time_ms,
                        })
                    }
                    wire::EditorTransactionEvent::Commit {
                        origin,
                        before,
                        after,
                        patches,
                        kind,
                        history,
                        input_time_ms,
                        ..
                    } => {
                        if before.document != id.document
                            || before.reset != id.reset
                            || before.text_revision != id.text_revision
                            || before.revision != id.revision
                        {
                            return None;
                        }
                        let old = editor.accept_patch(before, after, patches)?;
                        (callbacks.on_event)(EditorTransactionEvent::Commit {
                            id,
                            origin: origin.as_ref(),
                            before: EditorStateView::new(
                                old.as_deref().unwrap_or_else(|| editor.text_ref()),
                                before,
                            ),
                            after: EditorStateView::new(editor.text_ref(), after),
                            kind: *kind,
                            history: *history,
                            input_time_ms: *input_time_ms,
                        })
                    }
                    wire::EditorTransactionEvent::Fault { state, reason, .. } => {
                        if state.document != id.document
                            || state.reset != id.reset
                            || state.reset != editor.reset_revision()
                        {
                            return None;
                        }
                        (callbacks.on_event)(EditorTransactionEvent::Fault {
                            id,
                            reason: *reason,
                        })
                    }
                    wire::EditorTransactionEvent::Cancelled { state, .. } => {
                        if state.document != id.document || state.reset != id.reset {
                            return None;
                        }
                        (callbacks.on_event)(EditorTransactionEvent::Cancelled { id })
                    }
                };
                slots::editor_acknowledge(&event);
                mapped
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    fn id(attempt: u32) -> wire::EditorTransactionId {
        wire::EditorTransactionId {
            instance: 1,
            document: "app:draft".into(),
            reset: 0,
            sequence: 7,
            attempt,
            text_revision: 0,
            revision: 0,
        }
    }
    fn observer(calls: Rc<Cell<usize>>) -> u32 {
        let callbacks = Rc::new(Callbacks::<()> {
            decide: Rc::new(|_| EditorDecision::Noop),
            interact: None,
            on_event: Rc::new(move |_| {
                calls.set(calls.get() + 1);
                None
            }),
        });
        slots::handler::<(), Rc<Callbacks<()>>>(Box::new(move |()| Some(callbacks.clone())))
    }
    fn transaction(event: wire::EditorTransactionEvent, map: u32) -> EditorTransaction<()> {
        EditorTransaction {
            event: Transaction::Event(event),
            map,
            message: std::marker::PhantomData,
        }
    }
    fn commit(
        id: wire::EditorTransactionId,
        before: &Editor,
        text: &str,
    ) -> wire::EditorTransactionEvent {
        let reference = before.document_reference(id.document.clone());
        let mut after = reference.clone();
        after.byte_len = text.len() as u32;
        after.revision += 1;
        after.text_revision += u64::from(before.text_ref() != text);
        after.cursor.clamp(text);
        wire::EditorTransactionEvent::Commit {
            id,
            origin: None,
            before: reference,
            after,
            patches: wire::editor_document::editor_changed_span(before.text_ref(), text).unwrap(),
            kind: wire::EditorEditKind::GuestPatch,
            history: wire::EditorHistoryEffect::NewGroup,
            input_time_ms: 1,
        }
    }
    #[test]
    fn a_large_caret_commit_borrows_one_canonical_text_for_both_history_views() {
        let context = slots::Context::default();
        let _entered = context.enter();
        let mut editor = Editor::new("x".repeat(wire::editor_document::MAX_EDITOR_DOCUMENT_BYTES));
        let calls = Rc::new(Cell::new(0));
        let seen = calls.clone();
        let callbacks = Rc::new(Callbacks::<()> {
            decide: Rc::new(|_| EditorDecision::Noop),
            interact: None,
            on_event: Rc::new(move |event| {
                let EditorTransactionEvent::Commit { before, after, .. } = event else {
                    panic!("expected caret commit");
                };
                assert_eq!(
                    before.text.as_ptr(),
                    after.text.as_ptr(),
                    "metadata-only commit must borrow the same canonical text, not copy/compare one MiB"
                );
                assert_ne!(
                    before.cursor, after.cursor,
                    "before/after selection metadata stays distinct"
                );
                seen.set(seen.get() + 1);
                None
            }),
        });
        let map =
            slots::handler::<(), Rc<Callbacks<()>>>(Box::new(move |()| Some(callbacks.clone())));
        let before = editor.document_reference("app:draft".into());
        let mut after = before.clone();
        after.revision += 1;
        after.cursor.position.column = 1;
        transaction(
            wire::EditorTransactionEvent::Commit {
                id: id(1),
                origin: None,
                before,
                after,
                patches: vec![],
                kind: wire::EditorEditKind::Cursor,
                history: wire::EditorHistoryEffect::Native,
                input_time_ms: 42,
            },
            map,
        )
        .apply(&mut editor);
        assert_eq!(calls.get(), 1);
        assert_eq!(editor.cursor().position.column, 1);
    }

    #[test]
    fn cancellation_after_reset_notifies_without_replacing_the_new_document() {
        let context = slots::Context::default();
        let _entered = context.enter();
        let mut editor = Editor::new("old");
        let current = id(1);
        slots::editor_response(wire::EditorResponse {
            id: current.clone(),
            decision: EditorDecision::Noop,
        });
        let state = editor.document_reference(current.document.clone());
        let calls = Rc::new(Cell::new(0));
        let map = observer(calls.clone());
        editor.replace(Editor::new("new"), 0);
        transaction(
            wire::EditorTransactionEvent::Cancelled { id: current, state },
            map,
        )
        .apply(&mut editor);
        assert_eq!(calls.get(), 1, "retired identity gets cleanup after reset");
        assert_eq!(editor.text(), "new");
        assert_eq!(editor.reset_revision(), 1);
        assert!(!slots::editor_pending());
    }
    #[test]
    fn an_old_retry_cannot_commit_over_the_current_pending_attempt() {
        let context = slots::Context::default();
        let _entered = context.enter();
        let mut editor = Editor::new("before");
        slots::editor_response(wire::EditorResponse {
            id: id(2),
            decision: EditorDecision::Noop,
        });
        let calls = Rc::new(Cell::new(0));
        let map = observer(calls.clone());
        transaction(commit(id(1), &editor, "stale"), map).apply(&mut editor);
        assert_eq!(
            editor.text(),
            "before",
            "an old attempt must not replace document state"
        );
        assert_eq!(
            calls.get(),
            0,
            "stale retry must not run the history reducer"
        );
        assert!(
            slots::editor_pending(),
            "current attempt remains outstanding"
        );
        let valid = transaction(commit(id(2), &editor, "accepted"), map);
        valid.clone().apply(&mut editor);
        assert_eq!(editor.text(), "accepted");
        assert_eq!(calls.get(), 1);
        assert!(!slots::editor_pending());
        valid.apply(&mut editor);
        assert_eq!(
            calls.get(),
            1,
            "duplicate accepted commit does not repeat history"
        );
    }
    #[test]
    fn native_message_envelope_is_send_and_stale_commit_cannot_acknowledge() {
        fn is_send<T: Send>() {}
        is_send::<EditorTransaction<()>>();
        let context = slots::Context::default();
        let _entered = context.enter();
        let mut editor = Editor::new("before");
        slots::editor_response(wire::EditorResponse {
            id: id(1),
            decision: EditorDecision::Noop,
        });
        let calls = Rc::new(Cell::new(0));
        let map = observer(calls.clone());
        let event = commit(id(1), &editor, "after");
        let mut stale = event.clone();
        if let wire::EditorTransactionEvent::Commit { after, .. } = &mut stale {
            after.reset = 99;
        }
        transaction(stale, map).apply(&mut editor);
        assert!(slots::editor_pending());
        assert_eq!(calls.get(), 0);
        // Missing callback storage cannot accept or acknowledge a state update.
        transaction(event.clone(), u32::MAX).apply(&mut editor);
        assert_eq!(editor.text(), "before");
        assert!(slots::editor_pending());
        let valid = transaction(event, map);
        valid.clone().apply(&mut editor);
        assert_eq!(editor.text(), "after");
        assert_eq!(calls.get(), 1);
        assert!(!slots::editor_pending());
        valid.apply(&mut editor);
        assert_eq!(calls.get(), 1, "duplicate commit does not re-run history");
    }
}
