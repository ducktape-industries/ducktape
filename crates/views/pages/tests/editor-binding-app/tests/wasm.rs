//! Actual Wasm execution under the desktop's document/frame limits.
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};
use ui_lang_wire as wire;
use wasmtime::{
    Engine, Store,
    component::{Component, Linker, TypedFunc},
};

const FUEL: u64 = 100_000_000;
struct Guest {
    engine: Engine,
    store: Store<wasmtime::StoreLimits>,
    tick: TypedFunc<(Vec<u8>,), (Vec<u8>,)>,
    max_tick: Duration,
    max_fuel: u64,
}
impl Guest {
    fn open() -> Self {
        let path = std::env::var("PAGES_EDITOR_WASM")
            .expect("bundle the Pages editor fixture and set PAGES_EDITOR_WASM");
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true).epoch_interruption(true);
        let engine = Engine::new(&config).unwrap();
        let component = Component::from_file(&engine, path).unwrap();
        let limits = wasmtime::StoreLimitsBuilder::new()
            .memory_size(64 << 20)
            .build();
        let mut store = Store::new(&engine, limits);
        store.limiter(|limits| limits);
        store.set_fuel(FUEL).unwrap();
        store.set_epoch_deadline(1);
        let mut linker = Linker::new(&engine);
        linker
            .root()
            .func_wrap(
                "panicked",
                |_, (message,): (String,)| -> wasmtime::Result<()> {
                    Err(wasmtime::Error::msg(message))
                },
            )
            .unwrap();
        linker.define_unknown_imports_as_traps(&component).unwrap();
        let instance = linker.instantiate(&mut store, &component).unwrap();
        let init = instance
            .get_typed_func::<(bool,), ()>(&mut store, "init")
            .unwrap();
        init.call(&mut store, (false,)).unwrap();
        let tick = instance
            .get_typed_func::<(Vec<u8>,), (Vec<u8>,)>(&mut store, "tick")
            .unwrap();
        Self {
            engine,
            store,
            tick,
            max_tick: Duration::ZERO,
            max_fuel: 0,
        }
    }
    fn frame(&mut self, events: Vec<wire::Event>) -> wire::Frame {
        self.store.set_fuel(FUEL).unwrap();
        self.store.set_epoch_deadline(1);
        let (done, wait) = mpsc::channel();
        let engine = self.engine.clone();
        let deadline = std::thread::spawn(move || {
            if matches!(
                wait.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                engine.increment_epoch();
            }
        });
        let start = Instant::now();
        let result = self.tick.call(&mut self.store, (wire::encode(&events),));
        let elapsed = start.elapsed();
        let _ = done.send(());
        deadline.join().unwrap();
        let (bytes,) = result.expect("Pages tick must fit the desktop's 100ms/fuel boundary");
        self.max_tick = self.max_tick.max(elapsed);
        self.max_fuel = self.max_fuel.max(FUEL - self.store.get_fuel().unwrap());
        assert!(bytes.len() <= 8 << 20);
        wire::decode(&bytes).unwrap()
    }
}

fn markdown(blocks: usize, paragraphs: usize) -> String {
    let paragraph = "A substantial paragraph keeps its complete source and ordinary body text. "
        .repeat(paragraphs);
    let block = format!(
        "## Heading\n- [ ] 한글 paragraph with **bold** and _emphasis_.\n  - Nested text and https://example.com/page\n```\nlet value = 42;\n```\n> Quoted paragraph\n{paragraph}\n"
    );
    format!("A large document\n{}", block.repeat(blocks))
}

fn load(text: &str) -> (Guest, wire::Frame) {
    use wire::editor_document::{EditorDocumentRef, EditorTransferId, EditorTransferSender};
    let mut guest = Guest::open();
    let initial = guest.frame(vec![]);
    let frame = guest.frame(ducktape_view_guest::testing::press(
        &initial,
        "Load document",
    ));
    let request = frame
        .requests
        .iter()
        .find(|r| r.kind == "fixture.document")
        .expect("actual source subscription");
    let identity: pages_editor_binding_fixture::fixture_source::DocumentIdentity =
        wire::decode(&request.payload).unwrap();
    let source = EditorDocumentRef {
        document: identity.document.clone(),
        reset: identity.reset,
        revision: 0,
        text_revision: 0,
        cursor: wire::EditorCursor::default(),
        byte_len: text.len() as u32,
    };
    let id = EditorTransferId {
        instance: 1,
        document: identity.document,
        reset: identity.reset,
        serial: request.id,
        attempt: 0,
    };
    let mut sender = EditorTransferSender::new(id, source.clone()).unwrap();
    let request_id = request.id;
    let mut root = frame.root.or(initial.root).unwrap();
    while let Some(transfer) = sender.next_frame(&source, &text).unwrap() {
        let frame = guest.frame(vec![ducktape_view_guest::testing::item(
            request_id,
            &wire::encode(&transfer),
        )]);
        if let Some(next) = frame.root {
            root = next;
        } else if !frame.unchanged {
            wire::apply(&mut root, frame.patches).unwrap();
        }
    }
    let mut frame = wire::Frame {
        root: Some(root),
        ..Default::default()
    };
    assert!(!wire::sanitize(&mut frame).unwrap().display_text_truncated);
    (guest, frame)
}

#[test]
fn a_large_markdown_document_keeps_its_actual_wasm_presentation_within_the_host_limits() {
    let text = markdown(200, 18);
    assert!(text.len() > 256 * 1024);
    let (guest, frame) = load(&text);
    let Some(wire::Node::Editor {
        document, options, ..
    }) = ducktape_view_guest::testing::find(&frame, "PagesEditorFixture/document")
    else {
        panic!("actual editor")
    };
    assert_eq!(document.byte_len as usize, text.len());
    let paint = options
        .presentation
        .as_ref()
        .expect("formatting must not disappear");
    paint.validate(&text).unwrap();
    assert!(paint.spans.len() > 4_000);
    assert!(
        paint.formats.iter().any(|format| format.size == Some(0.01)),
        "hidden source markers remain geometric spans"
    );
    assert!(
        paint
            .formats
            .iter()
            .any(|format| format.line_background.is_some()),
        "code/callout/comment line paint remains present"
    );
    assert!(paint.affordances.hits.iter().any(|hit| hit.tag == 1));
    assert!(paint.affordances.hits.iter().any(|hit| hit.tag == 2));
    eprintln!(
        "pages markdown bytes={} spans={} formats={} max_tick_ms={:.3} max_fuel={}",
        text.len(),
        paint.spans.len(),
        paint.formats.len(),
        guest.max_tick.as_secs_f64() * 1000.0,
        guest.max_fuel
    );
}

#[test]
fn dense_markdown_keeps_all_bytes_and_undo_in_a_disclosed_plain_editor() {
    use wire::editor_document::*;
    use wire::keyboard::*;
    let text = markdown(900, 4);
    assert_eq!(text.len(), 404117);
    let (mut guest, frame) = load(&text);
    assert!(ducktape_view_guest::testing::has_text(
        &frame,
        "Formatting is unavailable for this document. Your text and undo history are preserved."
    ));
    let wire::Node::Editor {
        document: before,
        options,
        on_document,
        ..
    } = ducktape_view_guest::testing::find(&frame, "PagesEditorFixture/document").unwrap()
    else {
        panic!("editor")
    };
    let before = before.clone();
    let binding = options.binding.as_ref().unwrap().as_ref().clone();
    assert!(options.presentation.as_ref().unwrap().spans.is_empty());
    assert!(
        options
            .presentation
            .as_ref()
            .unwrap()
            .affordances
            .hits
            .is_empty()
    );
    let document_handler = *on_document;
    let id = wire::EditorTransactionId {
        instance: 1,
        document: before.document.clone(),
        reset: before.reset,
        sequence: 1,
        attempt: 0,
        text_revision: before.text_revision,
        revision: before.revision,
    };
    let edited = format!("X{text}");
    let mut after = before.clone();
    after.revision += 1;
    after.text_revision += 1;
    after.byte_len += 1;
    after.cursor.position.column = 1;
    let frame = guest.frame(vec![wire::Event::EditorTransaction {
        handler: binding.on_event,
        event: wire::EditorTransactionEvent::Commit {
            id: id.clone(),
            origin: None,
            before: before.clone(),
            after: after.clone(),
            patches: editor_changed_span(&text, &edited).unwrap(),
            kind: wire::EditorEditKind::Insert,
            history: wire::EditorHistoryEffect::Native,
            input_time_ms: 1000,
        },
    }]);
    let _ = frame;
    let key = KeyState {
        key: Key::Character("z".into()),
        modified_key: Key::Character("z".into()),
        physical_key: Physical::Unidentified(NativeCode::Unidentified),
        location: Location::Standard,
        modifiers: Modifiers {
            control: true,
            ..Default::default()
        },
    };
    let input = wire::EditorRequestInput::Key { key, repeat: false };
    let request_id = wire::EditorTransactionId {
        sequence: 2,
        text_revision: after.text_revision,
        revision: after.revision,
        ..id
    };
    let frame = guest.frame(vec![wire::Event::EditorRequest {
        handler: binding.on_request,
        request: wire::EditorRequest {
            id: request_id.clone(),
            state: after.clone(),
            input: input.clone(),
            input_time_ms: 2000,
        },
    }]);
    let [response] = frame.editor_decisions.as_slice() else {
        panic!("one Undo decision")
    };
    let wire::EditorDecision::Apply {
        patches,
        cursor,
        history,
    } = &response.decision
    else {
        panic!("Undo applies")
    };
    assert_eq!(*history, wire::EditorHistoryEffect::Undo);
    assert_eq!(
        wire::editor_transaction::patched_editor_text(&edited, patches, *cursor).unwrap(),
        text
    );
    let restored = EditorDocumentRef {
        revision: after.revision + 1,
        text_revision: after.text_revision + 1,
        ..before
    };
    guest.frame(vec![wire::Event::EditorTransaction {
        handler: binding.on_event,
        event: wire::EditorTransactionEvent::Commit {
            id: request_id,
            origin: Some(input),
            before: after,
            after: restored.clone(),
            patches: patches.clone(),
            kind: wire::EditorEditKind::GuestPatch,
            history: *history,
            input_time_ms: 2000,
        },
    }]);
    let transfer_id = EditorTransferId {
        instance: 1,
        document: restored.document.clone(),
        reset: restored.reset,
        serial: 99,
        attempt: 0,
    };
    let mut receiver = EditorTransferReceiver::new(transfer_id.clone(), restored.clone()).unwrap();
    let mut frame = guest.frame(vec![wire::Event::EditorDocument {
        handler: document_handler,
        message: EditorDocumentMessage::Request {
            id: transfer_id,
            target: restored,
        },
    }]);
    let mut actual = None;
    for _ in 0..20 {
        for message in frame.editor_documents {
            let EditorDocumentMessage::Transfer(transfer) = message else {
                panic!("source transfer")
            };
            if let Some(text) = receiver.receive(&transfer).unwrap() {
                actual = Some(text);
            }
        }
        if actual.is_some() {
            break;
        }
        frame = guest.frame(vec![]);
    }
    assert_eq!(
        actual.as_deref(),
        Some(text.as_str()),
        "all canonical bytes survive editing and Undo"
    );
    eprintln!(
        "dense markdown bytes={} max_tick_ms={:.3} max_fuel={}",
        text.len(),
        guest.max_tick.as_secs_f64() * 1000.0,
        guest.max_fuel
    );
}
