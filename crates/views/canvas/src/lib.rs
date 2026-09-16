//! Optimistic native canvas. The guest owns editing; gpui-kit owns rendering,
//! input and IME; the host signs field operations for consensus ordering.
mod host;
mod interaction;
mod markdown;
mod presentation;
use boards::{Align, Board, Change, Kind, Operation, Shape, TextSize};
use ducktape_view_guest::{Editor, wire};
use ducktape_view_guest::{Subscription, Task};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tool {
    #[default]
    Select,
    Hand,
    Note,
    Rectangle,
    Ellipse,
    Diamond,
    Text,
    Arrow,
    Line,
    Draw,
    Eraser,
}
/// What the inspector does to a selection of two or more. One tagged value,
/// so the arrangement is decided once and carried out in one place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Arrange {
    Left,
    CentreX,
    Right,
    Top,
    CentreY,
    Bottom,
    SpreadX,
    SpreadY,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
enum Gesture {
    #[default]
    Idle,
    Pan {
        start: [f32; 2],
        camera: [f32; 2],
    },
    Move {
        start: [f32; 2],
        point: [f32; 2],
        shapes: BTreeMap<String, Shape>,
    },
    Resize {
        id: String,
        corner: [i32; 2],
        start: [f32; 2],
        point: [f32; 2],
        shape: Shape,
    },
    /// A whole selection in hand, by one handle of the box drawn around it.
    /// Every shape keeps its place and its share of that box as the box
    /// changes, which is what makes several shapes scale as one object rather
    /// than as several that happen to be moving at the same time.
    Scale {
        corner: [i32; 2],
        start: [f32; 2],
        point: [f32; 2],
        bounds: [f32; 4],
        shapes: BTreeMap<String, Shape>,
    },
    /// A held arrow key. The selection moves under it as you hold it and the
    /// board hears about it once, when you let go: a key repeating thirty
    /// times a second is one intention, not thirty edits to be undone one at
    /// a time and thirty rounds to consensus for a shape that ended up an
    /// inch away.
    Nudge {
        shapes: BTreeMap<String, Shape>,
        offset: [i32; 2],
    },
    Marquee {
        start: [f32; 2],
        point: [f32; 2],
        previous: BTreeSet<String>,
    },
    Create {
        kind: Kind,
        start: [f32; 2],
        point: [f32; 2],
    },
    /// The pen, sampling world points until the button comes up.
    Sketch {
        points: Vec<[f32; 2]>,
    },
    /// The eraser, gathering what it has swept over; the board changes once,
    /// on release, so one sweep is one undo step. `last` is where the previous
    /// sample landed, because the sweep erases along the step and not only at
    /// its end.
    Erase {
        swept: BTreeSet<String>,
        last: [f32; 2],
    },
    /// One end of a connector, in hand. `end` indexes the sample being carried;
    /// the rest of the run keeps its shape, and the end lets go of any card it
    /// held the moment it moves, taking a new one only where it lands.
    Endpoint {
        id: String,
        end: usize,
        point: [f32; 2],
        shape: Shape,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Inline {
    id: String,
    original: String,
    #[serde(with = "editor_codec")]
    document: Editor,
    /// The height in board units the words in this card need, as the host
    /// measured them, and never less than it has already been asked for. The
    /// card is drawn at least this tall for as long as the editor is open and
    /// keeps the height when the card is saved; leaving by Escape drops it
    /// with the words that asked for it.
    ///
    /// On a card it only ever grows within one sitting: a round shape's inset
    /// is taken off the shorter side and so widens as the card gets taller, and
    /// keeping the high mark settles that in one step instead of letting it
    /// creep. On a text shape it tracks the words down as well, because a text
    /// shape has no box of its own — it IS its words — and the box it gives
    /// back is board you could not otherwise click through.
    grown: Option<f32>,
    /// The width in board units the words on a connector's plate take, as the
    /// host measured them. A card is written across the whole card, but a
    /// connector's words hug a plate centred on its line, so the box the caret
    /// lives in has to hug them too — otherwise the words sit at the left of
    /// the room set aside for them while you type and jump to the middle of
    /// the line the moment you stop. It tracks the words down as well as up: a
    /// plate that kept the width of a phrase you deleted would rub out the line
    /// for no one, and it is also the width a text shape hugs its words with.
    wide: Option<f32>,
}
mod editor_codec {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        editor: &Editor,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        editor.snapshot().serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Editor, D::Error> {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        Editor::restore(&bytes).ok_or_else(|| serde::de::Error::custom("invalid editor snapshot"))
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
enum Delivery {
    #[default]
    Idle,
    Sending,
    Failed(String),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct History {
    undo: Vec<Change>,
    redo: Vec<Change>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct BoardsView {
    session: host::Session,
    epoch: u64,
    catalog: BTreeMap<String, String>,
    current: String,
    confirmed: Option<Board>,
    pending: VecDeque<Operation>,
    delivery: Delivery,
    error: String,
    title: String,
    selected: BTreeSet<String>,
    inline: Option<Inline>,
    modifiers: wire::keyboard::Modifiers,
    space_pan: bool,
    tool_locked: bool,
    palette: u8,
    help: bool,
    board_picker: bool,
    snap: bool,
    guides: Vec<[f32; 4]>,
    /// The line each axis of the drag in hand is currently held to. A drag
    /// keeps a line it has taken until the hand is clearly past it, so a guide
    /// answers what you are doing instead of blinking on and off at whatever
    /// speed the hand happens to be moving.
    held: [Option<interaction::Hold>; 2],
    /// What the pointer is over with nothing in hand. A canvas answers before
    /// you commit — without it every click is a guess about what you will hit.
    hover: Option<String>,
    /// What was copied, kept by the view: the host opens no clipboard door to
    /// a guest, so a cut travels between this network's boards and no further.
    /// The ids ride along because a connector in the set names its cards by
    /// id, and the paste remaps from exactly those.
    clipboard: Vec<(String, Shape)>,
    cameras: BTreeMap<String, ([f32; 2], f32)>,
    tool: Tool,
    camera: [f32; 2],
    zoom: f32,
    viewport: [f32; 2],
    cursor: [f32; 2],
    gesture: Gesture,
    undo: Vec<History>,
    redo: Vec<History>,
}
#[derive(Clone, Debug)]
pub enum Message {
    Session(Result<host::Session, String>),
    Read(u64, String, Result<host::Reading, String>),
    Delivered(
        u64,
        String,
        Result<(), String>,
        Result<host::Reading, String>,
    ),
    Minted(u64, String, Shape, Result<String, String>),
    BoardMinted(u64, String, Result<String, String>),
    CreateBoard,
    Key(wire::keyboard::Event, bool),
    DoubleClick,
    EditText,
    FocusText,
    /// The gauge's answer: the zoom it was laid out at, then its width and
    /// height in pixels at that zoom.
    Measured(f32, f32, f32),
    Mounted(f32, f32),
    FocusResult(String, Result<(), String>),
    MiddleDown,
    FinishText,
    TextTransaction(ducktape_view_guest::EditorTransaction<Message>),
    TextDocument(ducktape_view_guest::EditorDocumentUpdate),
    Duplicate,
    Copy,
    Cut,
    Paste,
    Stack(bool),
    Planted(
        u64,
        String,
        Vec<(String, Shape)>,
        [i32; 2],
        Result<Vec<String>, String>,
    ),
    LockTool,
    Help,
    BoardPicker,
    Snap,
    SelectAll,
    FitSelection,
    ResetZoom,
    Arrange(Arrange),
    QuickNote,
    Template,

    Title(String),
    Open(String),
    Tool(Tool),
    Press(f32, f32),
    Position(f32, f32),
    Begin,
    Move(f32, f32),
    Release,
    Cancel,
    Wheel(f32, f32, bool),
    Zoom(f32),
    Fit,
    Size(f32, f32),
    Color(u8),
    Align(Align),
    Lettering(TextSize),
    Delete,
    Undo,
    Redo,
    Retry,
    DiscardPending,
}
impl BoardsView {
    const PREFERRED_WINDOW_SIZE: &'static str = "none";
    fn boot() -> (Self, Task<Message>) {
        (
            Self {
                session: host::Session::default(),
                epoch: 0,
                catalog: BTreeMap::new(),
                current: String::new(),
                confirmed: None,
                pending: VecDeque::new(),
                delivery: Delivery::Idle,
                error: String::new(),
                title: String::new(),
                selected: BTreeSet::new(),
                inline: None,
                modifiers: Default::default(),
                space_pan: false,
                tool_locked: false,
                palette: 0,
                help: false,
                board_picker: false,
                snap: true,
                guides: Vec::new(),
                held: [None, None],
                hover: None,
                clipboard: Vec::new(),
                cameras: BTreeMap::new(),
                tool: Tool::Select,
                camera: [80., 80.],
                zoom: 1.,
                viewport: [800., 600.],
                cursor: [0., 0.],
                gesture: Gesture::Idle,
                undo: Vec::new(),
                redo: Vec::new(),
            },
            Task::none(),
        )
    }
    fn snapshot(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|e| e.to_string())
    }
    fn restore(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|e| e.to_string())
    }
    fn subscription(&self) -> Subscription<Message> {
        let session = Subscription::batch([
            host::session().map(Message::Session),
            Subscription::filter_events(|event| match event {
                wire::Event::Keyboard { event, captured } => {
                    Some(Message::Key(event.clone(), *captured))
                }
                _ => None,
            }),
        ]);
        if !self.session.connected {
            return session;
        }
        Subscription::batch([
            session,
            host::watch(self.current.clone(), self.epoch)
                .map(|(epoch, id, result)| Message::Read(epoch, id, result)),
        ])
    }
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Session(result) => self.on_session(result),
            Message::Read(epoch, id, result) => self.on_read(epoch, id, result),
            Message::Delivered(epoch, id, result, reading) => {
                self.on_delivered(epoch, id, result, reading)
            }
            Message::Minted(epoch, board, shape, id) => self.on_minted(epoch, board, shape, id),
            Message::BoardMinted(epoch, title, id) => self.on_board_minted(epoch, title, id),
            Message::CreateBoard => self.on_create_board(),
            Message::Key(event, captured) => self.on_key(event, captured),
            Message::DoubleClick => self.on_double_click(),
            Message::EditText => self.begin_text(),
            Message::FocusText => self.focus_text(),
            Message::Measured(zoom, width, height) => self.on_measured(zoom, width, height),
            Message::Mounted(width, height) => self.on_mounted(width, height),
            Message::FocusResult(id, result) => self.on_focus_result(id, result),
            Message::MiddleDown => self.on_middle_down(),
            Message::FinishText => self.finish_text(),
            Message::TextTransaction(transaction) => self.on_text_transaction(transaction),
            Message::TextDocument(document) => self.on_text_document(document),
            Message::Duplicate => self.on_duplicate(),
            Message::Copy => self.on_copy(),
            Message::Cut => self.on_cut(),
            Message::Paste => self.on_paste(),
            Message::Stack(front) => self.on_stack(front),
            Message::Planted(epoch, board, shapes, offset, ids) => {
                self.on_planted(epoch, board, shapes, offset, ids)
            }
            Message::LockTool => self.on_lock_tool(),
            Message::Help => self.on_help(),
            Message::BoardPicker => self.on_board_picker(),
            Message::Snap => self.on_snap(),
            Message::SelectAll => self.on_select_all(),
            Message::FitSelection => self.on_fit_selection(),
            Message::ResetZoom => self.on_reset_zoom(),
            Message::Arrange(how) => self.on_arrange(how),
            Message::QuickNote => self.on_quick_note(),
            Message::Template => self.on_template(),
            Message::Title(title) => self.on_title(title),
            Message::Open(id) => self.on_open(id),
            Message::Tool(tool) => self.on_tool(tool),
            Message::Press(x, y) => self.on_press(x, y),
            Message::Position(x, y) => self.on_position(x, y),
            Message::Begin => self.on_begin(),
            Message::Move(x, y) => self.on_move(x, y),
            Message::Release => self.on_release(),
            Message::Cancel => self.on_cancel(),
            Message::Wheel(x, y, lines) => self.on_wheel(x, y, lines),
            Message::Zoom(factor) => self.on_zoom(factor),
            Message::Fit => self.on_fit(),
            Message::Size(w, h) => self.on_size(w, h),
            Message::Color(color) => self.on_color(color),
            Message::Align(align) => self.on_align(align),
            Message::Lettering(text_size) => self.on_lettering(text_size),
            Message::Delete => self.on_delete(),
            Message::Undo => self.on_undo(),
            Message::Redo => self.on_redo(),
            Message::Retry => self.on_retry(),
            Message::DiscardPending => self.on_discard_pending(),
        }
    }
    fn on_session(&mut self, result: Result<host::Session, String>) -> Task<Message> {
        let next = match result {
            Ok(next) => next,
            Err(error) => {
                self.error = error;
                return Task::none();
            }
        };
        let changed_network = next.chain != self.session.chain;
        if changed_network {
            if !self.pending.is_empty() || self.inline.is_some() {
                self.session.connected = false;
                self.error = "Return to the previous network to finish saving this board.".into();
                return Task::none();
            }
            self.epoch += 1;
            self.current.clear();
            self.catalog.clear();
            self.confirmed = None;
            self.selected.clear();
            self.undo.clear();
            self.redo.clear();
            self.gesture = Gesture::Idle;
            self.cameras.clear();
            self.space_pan = false;
        }
        self.session = next;
        self.pump()
    }
    fn on_read(
        &mut self,
        epoch: u64,
        id: String,
        result: Result<host::Reading, String>,
    ) -> Task<Message> {
        let standing = epoch == self.epoch && id == self.current;
        if !standing {
            return Task::none();
        }
        match result {
            Ok(reading) => {
                self.catalog = reading.catalog;
                if reading.board.is_none() && self.pending.is_empty() {
                    self.confirmed = None;
                }
                if let Some(board) = reading.board {
                    let first_visit =
                        self.confirmed.is_none() && !self.cameras.contains_key(&self.current);
                    let newer = self
                        .confirmed
                        .as_ref()
                        .is_none_or(|old| board.revision >= old.revision);
                    if newer {
                        self.confirmed = Some(board);
                        if first_visit {
                            self.on_fit();
                        }
                    }
                }
                self.forget_a_vanished_card();
                if self.current.is_empty()
                    && let Some(id) = self.catalog.keys().next().cloned()
                {
                    return self.on_open(id);
                }
            }
            Err(error) => self.error = error,
        }
        Task::none()
    }
    fn on_delivered(
        &mut self,
        epoch: u64,
        id: String,
        result: Result<(), String>,
        reading: Result<host::Reading, String>,
    ) -> Task<Message> {
        let standing = epoch == self.epoch && id == self.current;
        if !standing {
            return Task::none();
        }
        match result {
            Ok(()) => {
                let acknowledged = self.pending.pop_front();
                self.delivery = Delivery::Idle;
                match reading {
                    Ok(reading) => {
                        self.on_read(epoch, id, Ok(reading));
                    }
                    Err(error) => {
                        if let Some(operation) = acknowledged
                            && let Some(board) = &self.confirmed
                            && let Ok(mut next) = apply_operation(board, &operation)
                        {
                            // This is a local fallback, not a claimed remote revision.
                            next.revision = board.revision;
                            self.confirmed = Some(next);
                        }
                        self.error = format!("Saved; could not refresh: {error}");
                    }
                }
                self.pump()
            }
            Err(error) => {
                self.delivery = Delivery::Failed(error);
                Task::none()
            }
        }
    }
    fn pump(&mut self) -> Task<Message> {
        let ready = self.session.connected && matches!(self.delivery, Delivery::Idle);
        if !ready {
            return Task::none();
        }
        let Some(operation) = self.pending.front().cloned() else {
            return Task::none();
        };
        self.delivery = Delivery::Sending;
        let epoch = self.epoch;
        let id = self.current.clone();
        Task::future(async move {
            let result = host::submit(operation).await;
            let reading = host::read(&id).await;
            Message::Delivered(epoch, id, result, reading)
        })
    }
    fn on_retry(&mut self) -> Task<Message> {
        self.delivery = Delivery::Idle;
        self.pump()
    }
    fn on_discard_pending(&mut self) -> Task<Message> {
        if !matches!(self.delivery, Delivery::Failed(_)) {
            return Task::none();
        }
        self.pending.clear();
        self.inline = None;
        self.gesture = Gesture::Idle;
        self.confirmed = None;
        self.error.clear();
        self.delivery = Delivery::Idle;
        self.undo.clear();
        self.redo.clear();
        self.selected.clear();
        self.epoch += 1;
        Task::none()
    }
    /// The board as this view's own edits leave it, with nothing the pointer
    /// is in the middle of folded in. Everything a gesture reads to decide
    /// what it is about to do reads THIS: a gesture asking the board its own
    /// preview had already changed would be answering itself, and asking
    /// [`Self::visible`] from inside [`Self::gesture_changes`] does not even
    /// terminate.
    fn settled(&self) -> Option<Board> {
        let mut board = self.confirmed.clone()?;
        for operation in &self.pending {
            if let Ok(next) = apply_operation(&board, operation) {
                board = next;
            }
        }
        Some(board)
    }
    fn visible(&self) -> Option<Board> {
        let mut board = self.settled()?;
        if let Ok(next) = board.changed_many(&self.gesture_changes()) {
            board = next;
        }
        let growing = self
            .inline
            .as_ref()
            .and_then(|inline| self.grown_change(&board, inline));
        if let Some(change) = growing
            && let Ok(next) = board.changed(&change)
        {
            board = next;
        }
        Some(board)
    }
    /// The card being written in, drawn tall enough to hold the words it is
    /// holding — or nothing, when it already is. A card that cannot show what
    /// you just typed is the same defect whether the words are clipped or the
    /// editor scrolls them out of sight, so the card grows under the caret and
    /// keeps the height when it is saved.
    ///
    /// A card never shrinks. Deleting a line leaves the room it made, the way a
    /// box you dragged wider stays wide.
    ///
    /// A text shape does, in both directions, because a text shape has no box
    /// of its own — it IS its words, and a box left standing around words that
    /// are no longer there is empty board you cannot click through, cannot draw
    /// over, and that the alignment guides line the next shape up against.
    fn grown_change(&self, board: &Board, inline: &Inline) -> Option<Change> {
        let grown = inline.grown?;
        let shape = &board.shapes.get(&inline.id)?.shape;
        // A connector's box is the span of its run, not a box anyone chose, so
        // there is nothing here to grow: its label rides a plate of its own.
        if shape.kind.is_path() {
            return None;
        }
        // Clamped to what the board will take: a shape outside the limits is
        // refused whole, so a card fitted below the floor would not be fitted
        // at all rather than fitted as far as the floor.
        let needed = grown
            .ceil()
            .clamp(presentation::MIN_CARD[1] as f32, boards::MAX_SIZE as f32)
            as i32;
        let hugging = shape.kind == Kind::Text;
        let height = match hugging {
            true => needed,
            false => needed.max(shape.height),
        };
        let width = match hugging {
            true => self.hugged_width(inline, shape),
            false => shape.width,
        };
        let moved = width != shape.width || height != shape.height;
        moved.then(|| Change::Resize {
            id: inline.id.clone(),
            width,
            height,
        })
    }
    fn enqueue_many(&mut self, changes: Vec<Change>) -> Task<Message> {
        if changes.is_empty() {
            return Task::none();
        }
        let Some(board) = self.visible() else {
            return Task::none();
        };
        if let Err(error) = board.changed_many(&changes) {
            self.error = error;
            return Task::none();
        }
        self.error.clear();
        self.pending.push_back(Operation::Batch {
            board: self.current.clone(),
            changes,
        });
        self.pump()
    }
    fn edit(&mut self, change: Change) -> Task<Message> {
        self.edit_many(vec![change])
    }
    fn edit_many(&mut self, changes: Vec<Change>) -> Task<Message> {
        if changes.is_empty() {
            return Task::none();
        }
        if self.pending.len() >= 64 {
            self.error =
                "Waiting for earlier edits to save. Retry saving before adding more changes."
                    .into();
            return Task::none();
        }
        let Some(board) = self.visible() else {
            return Task::none();
        };
        if let Err(error) = board.changed_many(&changes) {
            self.error = error;
            return Task::none();
        }
        let mut undo = Vec::new();
        let mut redo = Vec::new();
        for change in changes {
            let before = inverse(&board, &change);
            if before.as_slice() == [change.clone()] {
                continue;
            }
            let mut next_undo = before;
            next_undo.extend(undo);
            undo = next_undo;
            redo.push(change);
        }
        if redo.is_empty() {
            return Task::none();
        }
        undo.sort_by_key(
            |change| matches!(change, Change::Create { shape, .. } if shape.kind == Kind::Arrow),
        );
        undo.dedup();
        self.undo.push(History {
            undo,
            redo: redo.clone(),
        });
        if self.undo.len() > 64 {
            self.undo.remove(0);
        }
        self.redo.clear();
        let queued = self.enqueue_many(redo);
        self.forget_a_vanished_card();
        queued
    }
    /// A card can go out from under the caret: Undo is live while you write,
    /// another writer can delete the shape, and a refreshed read can arrive
    /// without it. The editor stops being drawn at once — but the view went on
    /// believing it was open, which dropped every key on the board and left
    /// the save to fail against an id nothing answers to.
    fn forget_a_vanished_card(&mut self) {
        let Some(inline) = &self.inline else {
            return;
        };
        let still_there = self
            .settled()
            .is_some_and(|board| board.shapes.contains_key(&inline.id));
        if still_there {
            return;
        }
        self.inline = None;
    }
    fn on_create_board(&mut self) -> Task<Message> {
        let allowed =
            self.session.connected && self.pending.is_empty() && !self.title.trim().is_empty();
        if !allowed {
            return Task::none();
        }
        let epoch = self.epoch;
        let title = self.title.trim().to_owned();
        Task::future(async move { Message::BoardMinted(epoch, title, host::mint().await) })
    }
    fn on_board_minted(
        &mut self,
        epoch: u64,
        title: String,
        id: Result<String, String>,
    ) -> Task<Message> {
        let obsolete = epoch != self.epoch || !self.pending.is_empty();
        if obsolete {
            return Task::none();
        }
        let id = match id {
            Ok(id) => id,
            Err(error) => {
                self.error = error;
                return Task::none();
            }
        };
        let board = match Board::new(title.clone(), String::new()) {
            Ok(board) => board,
            Err(error) => {
                self.error = error;
                return Task::none();
            }
        };
        self.current = id.clone();
        self.confirmed = Some(board);
        self.selected.clear();
        self.catalog.insert(id.clone(), title.clone());
        self.title.clear();
        self.undo.clear();
        self.redo.clear();
        self.pending.push_back(Operation::Create { id, title });
        Task::batch([self.pump(), self.take_the_keyboard()])
    }
    fn on_title(&mut self, title: String) -> Task<Message> {
        self.title = title;
        Task::none()
    }
    fn on_open(&mut self, id: String) -> Task<Message> {
        if !self.pending.is_empty() {
            return Task::none();
        }
        if self.inline.is_some() {
            return self.finish_text();
        }
        self.cameras
            .insert(self.current.clone(), (self.camera, self.zoom));
        (self.camera, self.zoom) = self.cameras.get(&id).copied().unwrap_or(([80., 80.], 1.));
        self.current = id;
        self.board_picker = false;
        self.confirmed = None;
        self.selected.clear();
        self.error.clear();

        self.gesture = Gesture::Idle;
        self.undo.clear();
        self.redo.clear();
        self.take_the_keyboard()
    }
}

fn coordinate(value: f32) -> i32 {
    value
        .round()
        .clamp(-(boards::MAX_COORD as f32), boards::MAX_COORD as f32) as i32
}
fn inverse(board: &Board, change: &Change) -> Vec<Change> {
    match change {
        Change::Create { id, .. } => vec![Change::Delete { id: id.clone() }],
        Change::Move { id, .. } => board
            .shapes
            .get(id)
            .map(|r| {
                vec![Change::Move {
                    id: id.clone(),
                    x: r.shape.x,
                    y: r.shape.y,
                }]
            })
            .unwrap_or_default(),
        Change::Resize { id, .. } => board
            .shapes
            .get(id)
            .map(|r| {
                vec![Change::Resize {
                    id: id.clone(),
                    width: r.shape.width,
                    height: r.shape.height,
                }]
            })
            .unwrap_or_default(),
        Change::Text { id, .. } => board
            .shapes
            .get(id)
            .map(|r| {
                vec![Change::Text {
                    id: id.clone(),
                    text: r.shape.text.clone(),
                }]
            })
            .unwrap_or_default(),
        // A re-route restates the whole run, so the run as it stands puts it
        // back — box, samples and bindings together, in one step.
        Change::Route { id, .. } => board
            .shapes
            .get(id)
            .map(|r| {
                vec![Change::Route {
                    id: id.clone(),
                    x: r.shape.x,
                    y: r.shape.y,
                    width: r.shape.width,
                    height: r.shape.height,
                    points: r.shape.points.clone(),
                    from: r.shape.from.clone(),
                    to: r.shape.to.clone(),
                }]
            })
            .unwrap_or_default(),
        Change::Align { id, .. } => board
            .shapes
            .get(id)
            .map(|r| {
                vec![Change::Align {
                    id: id.clone(),
                    align: r.shape.align,
                }]
            })
            .unwrap_or_default(),
        Change::TextSize { id, .. } => board
            .shapes
            .get(id)
            .map(|r| {
                vec![Change::TextSize {
                    id: id.clone(),
                    text_size: r.shape.text_size,
                }]
            })
            .unwrap_or_default(),
        Change::Color { id, .. } => board
            .shapes
            .get(id)
            .map(|r| {
                vec![Change::Color {
                    id: id.clone(),
                    color: r.shape.color,
                }]
            })
            .unwrap_or_default(),
        // A re-stack names what rises; the stack as it stands puts it all back.
        Change::Order { .. } => vec![Change::Order {
            ids: board
                .ordered()
                .into_iter()
                .map(|(id, _)| id.clone())
                .collect(),
        }],
        Change::Delete { id } => {
            let Some(record) = board.shapes.get(id) else {
                return Vec::new();
            };
            let mut restore = vec![Change::Create {
                id: id.clone(),
                shape: record.shape.clone(),
            }];
            restore.extend(
                board
                    .shapes
                    .iter()
                    .filter(|(_, r)| {
                        r.shape.from.as_ref() == Some(id) || r.shape.to.as_ref() == Some(id)
                    })
                    .map(|(id, r)| Change::Create {
                        id: id.clone(),
                        shape: r.shape.clone(),
                    }),
            );
            restore
        }
    }
}
ducktape_view_guest::export_app!(
    BoardsView,
    "Boards",
    "A shared canvas for the workspace's ideas, notes and connections.",
    ["canvas"]
);
#[cfg(test)]
mod tests;

fn apply_operation(board: &Board, operation: &Operation) -> Result<Board, String> {
    match operation {
        Operation::Edit { change, .. } => board.changed(change),
        Operation::Batch { changes, .. } => board.changed_many(changes),
        Operation::Create { .. } => Ok(board.clone()),
    }
}
