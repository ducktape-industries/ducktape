//! The deployable view must fit the desktop's memory and per-frame fuel limits.
use boards::{Board, Change, Reply, Shape};
use ducktape_view_guest::{testing, wire};
use wasmtime::{
    Engine, Store,
    component::{Component, Linker, TypedFunc},
};
struct Guest {
    store: Store<wasmtime::StoreLimits>,
    tick: TypedFunc<(Vec<u8>,), (Vec<u8>,)>,
    tree: Option<wire::Node>,
}
impl Guest {
    fn new() -> Self {
        let path =
            std::env::var("CANVAS_VIEW_WASM").expect("set CANVAS_VIEW_WASM to the built component");
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).unwrap();
        let component = Component::from_file(&engine, path).unwrap();
        let mut store = Store::new(
            &engine,
            wasmtime::StoreLimitsBuilder::new()
                .memory_size(64 << 20)
                .build(),
        );
        store.limiter(|limits| limits);
        store.set_fuel(100_000_000).unwrap();
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
        instance
            .get_typed_func::<(bool,), ()>(&mut store, "init")
            .unwrap()
            .call(&mut store, (false,))
            .unwrap();
        let tick = instance.get_typed_func(&mut store, "tick").unwrap();
        Self {
            store,
            tick,
            tree: None,
        }
    }
    fn frame(&mut self, events: Vec<wire::Event>) -> wire::Frame {
        self.store.set_fuel(100_000_000).unwrap();
        let (bytes,) = self
            .tick
            .call(&mut self.store, (wire::encode(&events),))
            .expect("view tick fits desktop fuel and memory budgets");
        assert!(bytes.len() <= 8 << 20);
        let mut frame: wire::Frame = wire::decode(&bytes).unwrap();
        if let Some(root) = &frame.root {
            self.tree = Some(root.clone());
        } else if !frame.patches.is_empty() {
            wire::apply(self.tree.as_mut().unwrap(), frame.patches.clone()).unwrap();
        }
        frame.root = self.tree.clone();
        assert!(!wire::sanitize(&mut frame).unwrap().display_text_truncated);
        frame
    }
    fn answer(&mut self, frame: &wire::Frame, kind: &str, bytes: Vec<u8>) -> wire::Frame {
        let id = frame
            .requests
            .iter()
            .find(|r| r.kind == kind)
            .unwrap_or_else(|| panic!("missing {kind}: {:?}", frame.requests))
            .id;
        self.frame(vec![wire::Event::Response {
            id,
            result: Ok(bytes),
            done: kind != "canvas.props",
        }])
    }
}
fn load(board: Board) -> (Guest, wire::Frame) {
    let mut guest = Guest::new();
    let frame = guest.frame(Vec::new());
    let frame = guest.answer(
        &frame,
        "canvas.props",
        br#"{"connected":true,"dark":false,"chain":"test"}"#.to_vec(),
    );
    let list =
        || serde_json::to_vec(&Reply::List([("room".into(), "Planning".into())].into())).unwrap();
    let frame = guest.answer(&frame, "rpc.query", list());
    // Selecting the first board starts its own subscribed read.
    let frame = guest.answer(&frame, "rpc.query", list());
    let frame = guest.answer(
        &frame,
        "rpc.query",
        serde_json::to_vec(&Reply::Board(Some(board))).unwrap(),
    );
    (guest, frame)
}
#[test]
fn full_board_renders_and_pan_stays_within_desktop_limits() {
    let mut board = Board::new("Planning".into(), "owner".into()).unwrap();
    for i in 0..boards::MAX_SHAPES {
        board = board
            .changed(&Change::Create {
                id: format!("card-{i}"),
                shape: Shape {
                    text: "한글🦆".repeat(200),
                    ..Default::default()
                },
            })
            .unwrap();
    }
    let (mut guest, frame) = load(board);
    let frame = guest.frame(testing::press(&frame, "Pan"));
    let wire::Node::MouseArea {
        on_press_at: Some(position),
        on_press: Some(press),
        on_move: Some(movement),
        ..
    } = testing::find(&frame, "boards/canvas").unwrap()
    else {
        panic!("canvas mouse area")
    };
    let position = *position;
    let press = *press;
    let movement = *movement;
    let frame = guest.frame(vec![
        wire::Event::Pointer {
            handler: position,
            x: 200.,
            y: 200.,
        },
        wire::Event::Message(press),
    ]);
    let _ = frame;
    guest.frame(vec![wire::Event::Pointer {
        handler: movement,
        x: 260.,
        y: 240.,
    }]);
}
#[test]
fn placing_a_note_draws_before_the_submit_receipt() {
    let (mut guest, frame) = load(Board::new("Planning".into(), "owner".into()).unwrap());
    let frame = guest.frame(testing::press(&frame, "Note"));
    let wire::Node::MouseArea {
        on_press_at: Some(position),
        on_press: Some(press),
        on_release: Some(release),
        ..
    } = testing::find(&frame, "boards/canvas").unwrap()
    else {
        panic!("canvas mouse area")
    };
    let frame = guest.frame(vec![
        wire::Event::Pointer {
            handler: *position,
            x: 200.,
            y: 200.,
        },
        wire::Event::Message(*press),
        wire::Event::Message(*release),
    ]);
    let frame = guest.answer(&frame, "host.id", b"note-1".to_vec());
    assert!(frame.requests.iter().any(|r| r.kind == "op.submit"));
    assert!(testing::find(&frame, "boards/editor/note-1").is_some());
    assert!(
        testing::texts(&frame)
            .iter()
            .any(|text| text.contains("Editing text"))
    );
}

#[test]
fn full_board_selection_move_fits_a_native_frame() {
    use wire::keyboard::*;
    let mut board = Board::new("Planning".into(), "owner".into()).unwrap();
    for i in 0..boards::MAX_SHAPES {
        board = board
            .changed(&Change::Create {
                id: format!("card-{i}"),
                shape: Shape {
                    text: "한글🦆".repeat(200),
                    ..Default::default()
                },
            })
            .unwrap();
    }
    let (mut guest, _) = load(board);
    let press = |key: Key, control| wire::Event::Keyboard {
        captured: false,
        event: Event::Press {
            state: KeyState {
                modified_key: key.clone(),
                key,
                physical_key: Physical::Unidentified(NativeCode::Unidentified),
                location: Location::Standard,
                modifiers: Modifiers {
                    control,
                    ..Default::default()
                },
            },
            text: None,
            repeat: false,
        },
    };
    guest.frame(vec![press(Key::Character("a".into()), true)]);
    let frame = guest.frame(vec![press(Key::Named(Named::ArrowRight), false)]);
    assert!(frame.requests.iter().any(|r| r.kind == "op.submit"));
}
