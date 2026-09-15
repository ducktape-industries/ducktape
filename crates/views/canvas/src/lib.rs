//! Optimistic native canvas. The guest owns editing; gpui-kit owns rendering,
//! input and IME; the host signs field operations for consensus ordering.
mod host;
mod interaction;
mod presentation;
use boards::{Board, Change, Kind, Operation, Shape};
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
    /// on release, so one sweep is one undo step.
    Erase {
        swept: BTreeSet<String>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Inline {
    id: String,
    original: String,
    #[serde(with = "editor_codec")]
    document: Editor,
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
    FocusResult(String, Result<(), String>),
    MiddleDown,
    FinishText,
    TextTransaction(ducktape_view_guest::EditorTransaction<Message>),
    TextDocument(ducktape_view_guest::EditorDocumentUpdate),
    Duplicate,
    Duplicated(
        u64,
        String,
        Vec<(String, Shape)>,
        Result<Vec<String>, String>,
    ),
    LockTool,
    Help,
    BoardPicker,
    Snap,
    SelectAll,
    FitSelection,
    ResetZoom,
    Align(bool),
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
            Message::FocusResult(id, result) => self.on_focus_result(id, result),
            Message::MiddleDown => self.on_middle_down(),
            Message::FinishText => self.finish_text(),
            Message::TextTransaction(transaction) => self.on_text_transaction(transaction),
            Message::TextDocument(document) => self.on_text_document(document),
            Message::Duplicate => self.on_duplicate(),
            Message::Duplicated(epoch, board, shapes, ids) => {
                self.on_duplicated(epoch, board, shapes, ids)
            }
            Message::LockTool => self.on_lock_tool(),
            Message::Help => self.on_help(),
            Message::BoardPicker => self.on_board_picker(),
            Message::Snap => self.on_snap(),
            Message::SelectAll => self.on_select_all(),
            Message::FitSelection => self.on_fit_selection(),
            Message::ResetZoom => self.on_reset_zoom(),
            Message::Align(vertical) => self.on_align(vertical),
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
    fn visible(&self) -> Option<Board> {
        let mut board = self.confirmed.clone()?;
        for operation in &self.pending {
            if let Ok(next) = apply_operation(&board, operation) {
                board = next;
            }
        }
        if let Ok(next) = board.changed_many(&self.gesture_changes()) {
            board = next;
        }
        Some(board)
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
        self.enqueue_many(redo)
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
        self.pump()
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
        Task::none()
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
