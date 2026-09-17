// This fixture exercises the source registry, not the app navigation consumers.
#[allow(dead_code, unused_imports)]
#[path = "support/source.rs"]
mod source_registry;
// The actual generated guest, driven through its editor transaction handlers.
// Native editor layout/painting is owned by the runtime's separate host tests.
use ducktape_view_guest::{testing, wire};
use pages_editor_binding_fixture::{boot_native, restore_native, snapshot_native, tick_native};
use wire::keyboard::{Key, KeyState, Location, Modifiers, Named, NativeCode, Physical};
use wire::{
    EditorCursor, EditorDecision, EditorEditKind, EditorHistoryEffect, EditorPosition,
    EditorTransactionEvent, EditorTransactionId, Event, Frame, Node,
};

struct Guest {
    root: Node,
    text: String,
    sequence: u64,
    pending_origin: Option<(EditorTransactionId, wire::EditorRequestInput)>,
}
impl Guest {
    fn boot() -> Self {
        boot_native();
        let root = tick_native(vec![]).root.expect("first tree");
        Self {
            root,
            text: "- 한글".into(),
            sequence: 0,
            pending_origin: None,
        }
    }
    fn tick(&mut self, events: Vec<Event>) -> Frame {
        let frame = tick_native(events);
        if let Some(root) = &frame.root {
            self.root = root.clone();
        } else if !frame.unchanged {
            wire::apply(&mut self.root, frame.patches.clone()).expect("valid tree patch");
        }
        frame
    }
    fn editor(
        &self,
    ) -> (
        wire::editor_document::EditorDocumentRef,
        wire::EditorBinding,
    ) {
        let frame = Frame {
            root: Some(self.root.clone()),
            ..Default::default()
        };
        let Some(Node::Editor {
            document, options, ..
        }) = testing::find(&frame, "PagesEditorFixture/document")
        else {
            panic!("fixture editor missing: {:?}", self.root);
        };
        (
            document.clone(),
            *options.binding.clone().expect("real generated binding"),
        )
    }
    fn id(&mut self) -> EditorTransactionId {
        self.sequence += 1;
        let (d, _) = self.editor();
        EditorTransactionId {
            instance: 1,
            document: d.document,
            reset: d.reset,
            sequence: self.sequence,
            attempt: 0,
            text_revision: d.text_revision,
            revision: d.revision,
        }
    }
    fn request(&mut self, key: Key, shift: bool) -> wire::EditorResponse {
        let (state, binding) = self.editor();
        let command = matches!(key, Key::Character(_));
        let state_key = KeyState {
            key: key.clone(),
            modified_key: key,
            physical_key: Physical::Unidentified(NativeCode::Unidentified),
            location: Location::Standard,
            modifiers: Modifiers {
                shift,
                control: command,
                ..Default::default()
            },
        };
        assert!(
            binding
                .claims
                .iter()
                .any(|claim| claim.matches(&state_key, false)),
            "host would claim this key"
        );
        let id = self.id();
        let input = wire::EditorRequestInput::Key {
            key: state_key,
            repeat: false,
        };
        self.pending_origin = Some((id.clone(), input.clone()));
        let frame = self.tick(vec![Event::EditorRequest {
            handler: binding.on_request,
            request: wire::EditorRequest {
                id,
                state,
                input,
                input_time_ms: self.sequence * 1000,
            },
        }]);
        let [response] = frame.editor_decisions.as_slice() else {
            panic!("one decision: {frame:?}")
        };
        response.clone()
    }
    fn interaction(
        &mut self,
        action: wire::editor_presentation::EditorInteraction,
    ) -> wire::EditorResponse {
        let (state, binding) = self.editor();
        let id = self.id();
        let input = wire::EditorRequestInput::Interaction { action };
        self.pending_origin = Some((id.clone(), input.clone()));
        let frame = self.tick(vec![Event::EditorRequest {
            handler: binding.on_request,
            request: wire::EditorRequest {
                id,
                state,
                input,
                input_time_ms: self.sequence * 1000,
            },
        }]);
        let [response] = frame.editor_decisions.as_slice() else {
            panic!("one interaction decision: {frame:?}")
        };
        response.clone()
    }
    fn menu(&self) -> Option<wire::editor_presentation::EditorMenu> {
        let frame = Frame {
            root: Some(self.root.clone()),
            ..Default::default()
        };
        let Some(Node::Editor { options, .. }) =
            testing::find(&frame, "PagesEditorFixture/document")
        else {
            panic!("editor")
        };
        options
            .presentation
            .as_ref()
            .and_then(|paint| paint.affordances.menu.clone())
    }
    fn cancel(&mut self, id: EditorTransactionId) {
        let (state, binding) = self.editor();
        self.tick(vec![Event::EditorTransaction {
            handler: binding.on_event,
            event: EditorTransactionEvent::Cancelled { id, state },
        }]);
    }
    fn commit(
        &mut self,
        id: EditorTransactionId,
        text: String,
        cursor: EditorCursor,
        effect: EditorHistoryEffect,
        kind: EditorEditKind,
    ) {
        let (before, binding) = self.editor();
        let mut after = before.clone();
        after.revision += 1;
        after.text_revision += u64::from(self.text != text);
        after.cursor = cursor;
        after.byte_len = text.len() as u32;
        let patches = wire::editor_document::editor_changed_span(&self.text, &text)
            .expect("native-representable patch");
        let origin = self
            .pending_origin
            .take()
            .filter(|(pending, _)| pending == &id)
            .map(|(_, input)| input);
        self.tick(vec![Event::EditorTransaction {
            handler: binding.on_event,
            event: EditorTransactionEvent::Commit {
                id,
                origin,
                before,
                after,
                patches,
                kind,
                history: effect,
                input_time_ms: self.sequence * 1000,
            },
        }]);
        self.text = text;
        let frame = Frame {
            root: Some(self.root.clone()),
            ..Default::default()
        };
        if self.text.len() < 64000 {
            assert!(
                testing::has_text(&frame, &self.text),
                "accepted state reaches rendered echo before history route"
            );
        } else {
            assert_eq!(self.editor().0.byte_len as usize, self.text.len());
        }
    }
    fn apply(&mut self, response: wire::EditorResponse) {
        let EditorDecision::Apply {
            patches,
            cursor,
            history,
        } = response.decision
        else {
            panic!("expected Apply")
        };
        let text = wire::editor_transaction::patched_editor_text(&self.text, &patches, cursor)
            .expect("native accepts patch endpoints");
        self.commit(
            response.id,
            text,
            cursor,
            history,
            EditorEditKind::GuestPatch,
        );
    }
    fn caret(&mut self, cursor: EditorCursor) {
        let id = self.id();
        self.commit(
            id,
            self.text.clone(),
            cursor,
            EditorHistoryEffect::Native,
            EditorEditKind::Cursor,
        );
    }
}
fn at(line: u32, column: u32) -> EditorCursor {
    EditorCursor {
        position: EditorPosition { line, column },
        selection: None,
    }
}

#[test]
fn korean_enter_cancelled_undo_and_snapshot_redo_use_the_real_guest_binding() {
    let mut guest = Guest::boot();
    guest.caret(at(0, 8));
    let enter = guest.request(Key::Named(Named::Enter), false);
    guest.apply(enter);
    assert_eq!(guest.text, "- 한글\n- ");
    assert_eq!(guest.editor().0.cursor, at(1, 2));
    let undo = guest.request(Key::Character("z".into()), false);
    let (state, binding) = guest.editor();
    guest.tick(vec![Event::EditorTransaction {
        handler: binding.on_event,
        event: EditorTransactionEvent::Cancelled { id: undo.id, state },
    }]);
    let undo_again = guest.request(Key::Character("z".into()), false);
    guest.apply(undo_again);
    assert_eq!(guest.text, "- 한글");
    assert_eq!(guest.editor().0.cursor, at(0, 8));
    let snapshot = snapshot_native().expect("settled history snapshots");
    restore_native(&snapshot, false).expect("restore history without init");
    guest.tick(vec![]);
    let redo = guest.request(Key::Character("z".into()), true);
    guest.apply(redo);
    assert_eq!(guest.text, "- 한글\n- ");
}

#[test]
fn native_emoji_edit_and_selected_undo_preserve_byte_columns_and_graphemes() {
    let mut guest = Guest::boot();
    let id = guest.id();
    guest.commit(
        id,
        "- 👍🏽".into(),
        at(0, 10),
        EditorHistoryEffect::Native,
        EditorEditKind::ImeCommit,
    );
    let selected = EditorCursor {
        position: EditorPosition {
            line: 0,
            column: 10,
        },
        selection: Some(EditorPosition { line: 0, column: 2 }),
    };
    guest.caret(selected);
    let id = guest.id();
    guest.commit(
        id,
        "- 🇰🇷".into(),
        at(0, 10),
        EditorHistoryEffect::NewGroup,
        EditorEditKind::Paste,
    );
    let undo = guest.request(Key::Character("z".into()), false);
    guest.apply(undo);
    assert_eq!(guest.text, "- 👍🏽");
    assert_eq!(guest.editor().0.cursor, selected);
    let redo = guest.request(Key::Character("z".into()), true);
    guest.apply(redo);
    assert_eq!(guest.text, "- 🇰🇷");
    assert_eq!(guest.editor().0.cursor.selection, None);
}

#[test]
fn undoing_a_group_that_returned_to_its_original_text_still_consumes_the_step() {
    let mut guest = Guest::boot();
    guest.caret(at(0, 8));
    let enter = guest.request(Key::Named(Named::Enter), false);
    guest.apply(enter);
    let id = guest.id();
    guest.commit(
        id,
        "- 한글".into(),
        at(0, 8),
        EditorHistoryEffect::ExtendPrevious,
        EditorEditKind::Backspace,
    );
    let undo = guest.request(Key::Character("z".into()), false);
    guest.apply(undo);
    let no_second_step = guest.request(Key::Character("z".into()), false);
    assert_eq!(no_second_step.decision, EditorDecision::Noop);
}

#[test]
fn interrupted_bootstrap_keeps_the_old_document_and_restores_from_a_new_begin() {
    use wire::editor_document::{
        EditorDocumentMessage, EditorTransferId, EditorTransferReceiver, EditorTransferSender,
        MAX_EDITOR_DOCUMENT_BYTES,
    };
    let mut guest = Guest::boot();
    guest.caret(at(0, 8));
    let enter = guest.request(Key::Named(Named::Enter), false);
    guest.apply(enter);
    let previous = guest.editor().0;
    let shown = Frame {
        root: Some(guest.root.clone()),
        ..Default::default()
    };
    let frame = guest.tick(testing::press(&shown, "Load document"));
    let request = frame
        .requests
        .iter()
        .find(|request| request.kind == "fixture.document")
        .expect("document subscription");
    let identity: pages_editor_binding_fixture::fixture_source::DocumentIdentity =
        wire::decode(&request.payload).unwrap();
    let source = wire::editor_document::EditorDocumentRef {
        document: identity.document.clone(),
        reset: identity.reset,
        text_revision: 0,
        revision: 0,
        cursor: EditorCursor::default(),
        byte_len: MAX_EDITOR_DOCUMENT_BYTES as u32,
    };
    let first_request = request.id;
    let text = format!(
        "{}{}",
        "한".repeat(MAX_EDITOR_DOCUMENT_BYTES / 3),
        "x".repeat(MAX_EDITOR_DOCUMENT_BYTES % 3)
    );
    let id = EditorTransferId {
        instance: 1,
        document: source.document.clone(),
        reset: source.reset,
        serial: 1,
        attempt: 0,
    };
    let mut first = EditorTransferSender::new(id, source.clone()).unwrap();
    for _ in 0..3 {
        let transfer = first.next_frame(&source, &text).unwrap().unwrap();
        guest.tick(vec![testing::item(first_request, &wire::encode(&transfer))]);
        assert_eq!(
            guest.editor().0,
            previous,
            "partial bytes never replace the last committed document or caret"
        );
    }
    let snapshot = snapshot_native().expect("partial subscription state is disposable");
    restore_native(&snapshot, false).unwrap();
    let resumed = guest.tick(vec![]);
    assert_eq!(guest.editor().0, previous);
    let request = resumed
        .requests
        .iter()
        .find(|request| request.kind == "fixture.document")
        .expect("restored subscription asks from Begin");
    assert_eq!(request.payload, wire::encode(&identity));
    let resumed_id = request.id;
    let id = EditorTransferId {
        instance: 2,
        document: source.document.clone(),
        reset: source.reset,
        serial: 2,
        attempt: 0,
    };
    let mut sender = EditorTransferSender::new(id, source.clone()).unwrap();
    while let Some(transfer) = sender.next_frame(&source, &text).unwrap() {
        let bytes = wire::encode(&transfer);
        assert!(
            bytes.len() < 1024 * 1024,
            "each capability item fits the payload cap with metadata"
        );
        guest.tick(vec![testing::item(resumed_id, &bytes)]);
    }
    let (installed, _) = guest.editor();
    assert_eq!(installed.byte_len as usize, MAX_EDITOR_DOCUMENT_BYTES);
    let undo = guest.request(Key::Character("z".into()), false);
    assert_eq!(
        undo.decision,
        EditorDecision::Noop,
        "a replaced document cannot consume the previous document's history"
    );
    let (state, binding) = guest.editor();
    guest.tick(vec![Event::EditorTransaction {
        handler: binding.on_event,
        event: EditorTransactionEvent::Cancelled { id: undo.id, state },
    }]);
    guest.text = text.clone();
    let mut edited = text;
    edited.pop();
    edited.push('y');
    let id = guest.id();
    guest.commit(
        id,
        edited.clone(),
        at(0, edited.len() as u32),
        EditorHistoryEffect::Native,
        EditorEditKind::Paste,
    );
    let edited_reference = guest.editor().0;
    let snapshot = snapshot_native().expect("completed source and local edits snapshot");
    restore_native(&snapshot, false).unwrap();
    let resumed = guest.tick(vec![]);
    assert!(
        resumed
            .requests
            .iter()
            .all(|request| request.kind != "fixture.document"),
        "a completed source must not restart and overwrite unsaved guest edits"
    );
    assert_eq!(guest.editor().0, edited_reference);
    let (installed, _) = guest.editor();
    // Read the canonical guest bytes through the runtime's document transfer,
    // rather than trusting only the advertised length or a display echo.
    let frame = Frame {
        root: Some(guest.root.clone()),
        ..Default::default()
    };
    let Some(Node::Editor { on_document, .. }) =
        testing::find(&frame, "PagesEditorFixture/document")
    else {
        panic!("editor")
    };
    let handler = *on_document;
    let id = EditorTransferId {
        instance: 3,
        document: installed.document.clone(),
        reset: installed.reset,
        serial: 3,
        attempt: 0,
    };
    let mut receiver = EditorTransferReceiver::new(id.clone(), installed.clone()).unwrap();
    let mut frame = guest.tick(vec![Event::EditorDocument {
        handler,
        message: EditorDocumentMessage::Request {
            id: id.clone(),
            target: installed,
        },
    }]);
    let mut actual = None;
    for _ in 0..20 {
        for message in frame.editor_documents {
            let EditorDocumentMessage::Transfer(transfer) = message else {
                panic!("expected source transfer")
            };
            if let Some(complete) = receiver.receive(&transfer).unwrap() {
                actual = Some(complete);
            }
        }
        if actual.is_some() {
            break;
        }
        frame = guest.tick(vec![]);
    }
    assert_eq!(
        actual.as_deref(),
        Some(edited.as_str()),
        "all UTF8 bytes and later edits survive initial ingress and snapshot restart"
    );
    guest.tick(vec![Event::EditorDocument {
        handler,
        message: EditorDocumentMessage::Acknowledged { id },
    }]);
}

#[test]
fn actual_menu_edit_commits_after_accept_and_undo_survives_restore() {
    use wire::editor_presentation::{EditorGutterButton, EditorInteraction, EditorMenuAnchor};
    let mut guest = Guest::boot();
    let plus = || EditorInteraction::Gutter {
        line: 0,
        button: EditorGutterButton::Plus,
    };
    let cancelled = guest.interaction(plus());
    assert!(
        guest.menu().is_none(),
        "a proposal cannot open its future menu"
    );
    guest.cancel(cancelled.id);
    assert_eq!(guest.text, "- 한글");
    assert!(
        guest.menu().is_none(),
        "cancelled plus must not open a menu"
    );
    let accepted = guest.interaction(plus());
    guest.apply(accepted);
    assert_eq!(guest.text, "- 한글\n");
    let menu = guest.menu().expect("accepted plus opens the caret menu");
    assert_eq!(menu.anchor, EditorMenuAnchor::Caret);
    // The palette named, not counted. A bare length says a menu grew without
    // saying into what, so a deliberate row and an accidental duplicate read
    // the same; the tags tell them apart on sight.
    let tags: Vec<&str> = menu.items.iter().map(|item| item.tag.as_str()).collect();
    assert_eq!(
        tags,
        [
            "page", //
            "text", "h1", "h2", "h3", "todo", "bullet", "number", "toggle", "quote", "callout",
            "code", "divider", //
            "mention", "emoji", //
            "image",
        ],
        "the page row, the twelve turns, the two pickers and the picture"
    );
    let pick = || EditorInteraction::MenuPick { tag: "h1".into() };
    let cancelled = guest.interaction(pick());
    assert!(
        guest.menu().is_some(),
        "a pending pick keeps its current menu"
    );
    guest.cancel(cancelled.id);
    assert!(guest.menu().is_some(), "cancelled pick keeps the menu");
    let accepted = guest.interaction(pick());
    guest.apply(accepted);
    assert_eq!(guest.text, "- 한글\n# ");
    assert!(guest.menu().is_none());
    let snapshot = snapshot_native().unwrap();
    restore_native(&snapshot, false).unwrap();
    guest.tick(vec![]);
    let undo = guest.request(Key::Character("z".into()), false);
    guest.apply(undo);
    assert_eq!(
        guest.text, "- 한글\n",
        "pick was one undo group, not an authoritative reset"
    );
    let undo = guest.request(Key::Character("z".into()), false);
    guest.apply(undo);
    assert_eq!(guest.text, "- 한글");
}

#[test]
fn fresh_guest_receives_latest_source_but_restored_guest_keeps_its_edits() {
    use source_registry::{DocumentIdentity, SourceStore};
    let mut sources = SourceStore::default();
    let identity = DocumentIdentity {
        document: "network-a/page-a/source-1".into(),
        reset: 1,
    };
    let original_marker = sources
        .show(identity.clone(), "old saved text", at(0, 0))
        .unwrap();
    let text = "unsaved 한글 👍🏽";
    let cursor = EditorCursor {
        position: EditorPosition {
            line: 0,
            column: text.len() as u32,
        },
        selection: Some(EditorPosition { line: 0, column: 8 }),
    };
    let marker = sources.show(identity, text, cursor).unwrap();
    assert_eq!(
        marker, original_marker,
        "typing must not replace the installed source identity"
    );
    let mut guest = Guest::boot();
    let shown = Frame {
        root: Some(guest.root.clone()),
        ..Default::default()
    };
    let frame = guest.tick(testing::press(&shown, "Load document"));
    let request = frame
        .requests
        .iter()
        .find(|r| r.kind == "fixture.document")
        .unwrap();
    assert_eq!(request.payload, marker);
    let request_id = request.id;
    let mut transfer = sources.transfer(&request.payload, 9, request_id).unwrap();
    while let Some(frame) = transfer.next(&sources).unwrap() {
        guest.tick(vec![testing::item(request_id, &wire::encode(&frame))]);
    }
    guest.text = text.into();
    assert_eq!(guest.editor().0.cursor, cursor);
    let frame = Frame {
        root: Some(guest.root.clone()),
        ..Default::default()
    };
    assert!(
        testing::has_text(&frame, text),
        "fresh instance must use latest synthetic source, not original saved bytes"
    );
    let snapshot = snapshot_native().unwrap();
    let before = guest.editor().0;
    restore_native(&snapshot, false).unwrap();
    let frame = guest.tick(vec![]);
    assert_eq!(guest.editor().0, before);
    assert!(
        frame.requests.iter().all(|r| r.kind != "fixture.document"),
        "restore must not re-bootstrap a completed source"
    );
}

#[test]
fn source_change_during_transfer_keeps_the_previous_guest_document() {
    use source_registry::{DocumentIdentity, SourceStore};
    let mut sources = SourceStore::default();
    let identity = DocumentIdentity {
        document: "network-a/page-a/source-1".into(),
        reset: 1,
    };
    sources
        .show(identity, &"x".repeat(100_000), at(0, 0))
        .unwrap();
    let mut guest = Guest::boot();
    let before = guest.editor().0;
    let shown = Frame {
        root: Some(guest.root.clone()),
        ..Default::default()
    };
    let frame = guest.tick(testing::press(&shown, "Load document"));
    let request = frame
        .requests
        .iter()
        .find(|r| r.kind == "fixture.document")
        .unwrap();
    let request_id = request.id;
    let mut transfer = sources.transfer(&request.payload, 10, request_id).unwrap();
    for _ in 0..2 {
        let frame = transfer.next(&sources).unwrap().unwrap();
        guest.tick(vec![testing::item(request_id, &wire::encode(&frame))]);
    }
    sources.clear(); // connection/navigation invalidates the live source immediately
    let frame = transfer.next(&sources).unwrap().unwrap();
    assert!(matches!(
        frame,
        wire::editor_document::EditorTransfer::Abort { .. }
    ));
    guest.tick(vec![testing::item(request_id, &wire::encode(&frame))]);
    assert_eq!(guest.editor().0, before);
    let frame = Frame {
        root: Some(guest.root.clone()),
        ..Default::default()
    };
    assert!(testing::has_text(
        &frame,
        "Document transfer could not be completed"
    ));
}
