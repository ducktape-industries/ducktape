//! Optimistic native canvas. The guest owns editing; gpui-kit owns rendering,
//! input and IME; the host signs field operations for consensus ordering.
mod host;
mod presentation;
use boards::{Board, Change, Kind, Operation, Shape};
use ducktape_view_guest::{Subscription, Task};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tool {
    #[default]
    Select,
    Hand,
    Note,
    Rectangle,
    Text,
    Connect,
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
        id: String,
        start: [f32; 2],
        shape: Shape,
        point: [f32; 2],
    },
    Resize {
        id: String,
        start: [f32; 2],
        shape: Shape,
        point: [f32; 2],
    },
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
    selected: Option<String>,
    draft: String,
    tool: Tool,
    camera: [f32; 2],
    zoom: f32,
    viewport: [f32; 2],
    cursor: [f32; 2],
    gesture: Gesture,
    connection: Option<String>,
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
    Draft(String),
    ApplyText,
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
                selected: None,
                draft: String::new(),
                tool: Tool::Select,
                camera: [80., 80.],
                zoom: 1.,
                viewport: [800., 600.],
                cursor: [0., 0.],
                gesture: Gesture::Idle,
                connection: None,
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
        let session = host::session().map(Message::Session);
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
            Message::Draft(text) => self.on_draft(text),
            Message::ApplyText => self.on_apply_text(),
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
            if !self.pending.is_empty() {
                self.session.connected = false;
                self.error = "Return to the previous network to finish saving this board.".into();
                return Task::none();
            }
            self.epoch += 1;
            self.current.clear();
            self.catalog.clear();
            self.confirmed = None;
            self.selected = None;
            self.undo.clear();
            self.redo.clear();
            self.gesture = Gesture::Idle;
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
                    let newer = self
                        .confirmed
                        .as_ref()
                        .is_none_or(|old| board.revision >= old.revision);
                    if newer {
                        self.confirmed = Some(board);
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
                        if let Some(Operation::Edit { change, .. }) = acknowledged
                            && let Some(board) = &self.confirmed
                            && let Ok(mut next) = board.changed(&change)
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
        self.confirmed = None;
        self.error.clear();
        self.delivery = Delivery::Idle;
        self.undo.clear();
        self.redo.clear();
        self.selected = None;
        self.draft.clear();
        self.epoch += 1;
        Task::none()
    }
    fn visible(&self) -> Option<Board> {
        let mut board = self.confirmed.clone()?;
        for operation in &self.pending {
            if let Operation::Edit { change, .. } = operation
                && let Ok(next) = board.changed(change)
            {
                board = next;
            }
        }
        if let Some(change) = self.gesture_change()
            && let Ok(next) = board.changed(&change)
        {
            board = next;
        }
        Some(board)
    }
    fn enqueue(&mut self, change: Change) -> Task<Message> {
        let Some(board) = self.visible() else {
            return Task::none();
        };
        if let Err(error) = board.changed(&change) {
            self.error = error;
            return Task::none();
        }
        self.error.clear();
        self.pending.push_back(Operation::Edit {
            board: self.current.clone(),
            change,
        });
        self.pump()
    }
    fn edit(&mut self, change: Change) -> Task<Message> {
        if self.pending.len() >= 64 {
            self.error =
                "Waiting for earlier edits to save. Retry saving before adding more changes."
                    .into();
            return Task::none();
        }
        let Some(board) = self.visible() else {
            return Task::none();
        };
        if let Err(error) = board.changed(&change) {
            self.error = error;
            return Task::none();
        }
        let undo = inverse(&board, &change);
        if undo.as_slice() == [change.clone()] {
            return Task::none();
        }
        self.undo.push(History {
            undo,
            redo: vec![change.clone()],
        });
        if self.undo.len() > 64 {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.enqueue(change)
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
        self.selected = None;
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
        self.current = id;
        self.confirmed = None;
        self.selected = None;
        self.draft.clear();
        self.error.clear();
        self.camera = [80., 80.];
        self.zoom = 1.;
        self.gesture = Gesture::Idle;
        self.connection = None;
        self.undo.clear();
        self.redo.clear();
        Task::none()
    }
    fn on_tool(&mut self, tool: Tool) -> Task<Message> {
        self.tool = tool;
        self.gesture = Gesture::Idle;
        self.connection = None;
        Task::none()
    }
    fn world(&self, point: [f32; 2]) -> [f32; 2] {
        [
            (point[0] - self.camera[0]) / self.zoom,
            (point[1] - self.camera[1]) / self.zoom,
        ]
    }
    fn hit(&self, point: [f32; 2]) -> Option<String> {
        let board = self.visible()?;
        let card = board.shapes.iter().rev().find(|(_, r)| {
            let s = &r.shape;
            s.kind != Kind::Arrow
                && point[0] >= s.x as f32
                && point[1] >= s.y as f32
                && point[0] <= (s.x + s.width) as f32
                && point[1] <= (s.y + s.height) as f32
        });
        if let Some((id, _)) = card {
            return Some(id.clone());
        }
        board
            .shapes
            .iter()
            .rev()
            .find(|(_, record)| {
                let s = &record.shape;
                let (Some(from), Some(to)) = (&s.from, &s.to) else {
                    return false;
                };
                let (Some(a), Some(b)) = (board.shapes.get(from), board.shapes.get(to)) else {
                    return false;
                };
                let start = presentation::anchor(&a.shape, &b.shape);
                let b = presentation::anchor(&b.shape, &a.shape);
                let a = start;
                let delta = [b[0] - a[0], b[1] - a[1]];
                let length = delta[0] * delta[0] + delta[1] * delta[1];
                let t = (((point[0] - a[0]) * delta[0] + (point[1] - a[1]) * delta[1])
                    / length.max(0.001))
                .clamp(0., 1.);
                let distance =
                    (point[0] - a[0] - t * delta[0]).hypot(point[1] - a[1] - t * delta[1]);
                distance <= 8. / self.zoom
            })
            .map(|(id, _)| id.clone())
    }
    fn on_position(&mut self, x: f32, y: f32) -> Task<Message> {
        self.cursor = [x, y];
        Task::none()
    }
    fn on_begin(&mut self) -> Task<Message> {
        self.on_press(self.cursor[0], self.cursor[1])
    }
    fn on_press(&mut self, x: f32, y: f32) -> Task<Message> {
        let point = self.world([x, y]);
        self.cursor = [x, y];
        if self.tool == Tool::Hand {
            self.gesture = Gesture::Pan {
                start: [x, y],
                camera: self.camera,
            };
            return Task::none();
        }
        let Some(board) = self.visible() else {
            return Task::none();
        };
        let hit = self.hit(point);
        match self.tool {
            Tool::Select => {
                self.selected = hit.clone();
                let Some(id) = hit else {
                    self.draft.clear();
                    return Task::none();
                };
                let shape = board.shapes[&id].shape.clone();
                self.draft = shape.text.clone();
                if shape.kind == Kind::Arrow {
                    self.gesture = Gesture::Idle;
                    return Task::none();
                }
                let at_handle = (point[0] - (shape.x + shape.width) as f32).abs() < 14. / self.zoom
                    && (point[1] - (shape.y + shape.height) as f32).abs() < 14. / self.zoom;
                self.gesture = if at_handle {
                    Gesture::Resize {
                        id,
                        start: point,
                        point,
                        shape,
                    }
                } else {
                    Gesture::Move {
                        id,
                        start: point,
                        point,
                        shape,
                    }
                };
                Task::none()
            }
            Tool::Connect => {
                let Some(id) = hit else {
                    return Task::none();
                };
                let Some(from) = self.connection.take() else {
                    self.connection = Some(id);
                    return Task::none();
                };
                if from == id {
                    return Task::none();
                }
                self.mint_shape(Shape {
                    kind: Kind::Arrow,
                    from: Some(from),
                    to: Some(id),
                    ..Default::default()
                })
            }
            Tool::Note | Tool::Rectangle | Tool::Text => {
                let kind = match self.tool {
                    Tool::Note => Kind::Note,
                    Tool::Rectangle => Kind::Rectangle,
                    _ => Kind::Text,
                };
                let shape = Shape {
                    kind,
                    x: coordinate(point[0]),
                    y: coordinate(point[1]),
                    ..Default::default()
                };
                self.mint_shape(shape)
            }
            Tool::Hand => Task::none(),
        }
    }
    fn mint_shape(&self, shape: Shape) -> Task<Message> {
        let epoch = self.epoch;
        let board = self.current.clone();
        Task::future(async move { Message::Minted(epoch, board, shape, host::mint().await) })
    }
    fn on_minted(
        &mut self,
        epoch: u64,
        board: String,
        shape: Shape,
        id: Result<String, String>,
    ) -> Task<Message> {
        let standing = epoch == self.epoch && board == self.current;
        if !standing {
            return Task::none();
        }
        let id = match id {
            Ok(id) => id,
            Err(error) => {
                self.error = error;
                return Task::none();
            }
        };
        self.selected = Some(id.clone());
        self.draft = shape.text.clone();
        self.tool = Tool::Select;
        self.edit(Change::Create { id, shape })
    }
    fn on_move(&mut self, x: f32, y: f32) -> Task<Message> {
        self.cursor = [x, y];
        let point = self.world([x, y]);
        match &mut self.gesture {
            Gesture::Idle => {}
            Gesture::Pan { start, camera } => {
                self.camera = [camera[0] + x - start[0], camera[1] + y - start[1]]
            }
            Gesture::Move { point: latest, .. } | Gesture::Resize { point: latest, .. } => {
                *latest = point
            }
        }
        Task::none()
    }
    fn gesture_change(&self) -> Option<Change> {
        match &self.gesture {
            Gesture::Move {
                id,
                start,
                shape,
                point,
            } => Some(Change::Move {
                id: id.clone(),
                x: coordinate(shape.x as f32 + point[0] - start[0]),
                y: coordinate(shape.y as f32 + point[1] - start[1]),
            }),
            Gesture::Resize {
                id,
                start,
                shape,
                point,
            } => Some(Change::Resize {
                id: id.clone(),
                width: (shape.width as f32 + point[0] - start[0])
                    .round()
                    .clamp(40., boards::MAX_SIZE as f32) as i32,
                height: (shape.height as f32 + point[1] - start[1])
                    .round()
                    .clamp(32., boards::MAX_SIZE as f32) as i32,
            }),
            Gesture::Idle | Gesture::Pan { .. } => None,
        }
    }
    fn on_release(&mut self) -> Task<Message> {
        let change = self.gesture_change();
        self.gesture = Gesture::Idle;
        match change {
            Some(change) => self.edit(change),
            None => Task::none(),
        }
    }
    fn on_cancel(&mut self) -> Task<Message> {
        self.gesture = Gesture::Idle;
        self.connection = None;
        Task::none()
    }
    fn on_wheel(&mut self, x: f32, y: f32, lines: bool) -> Task<Message> {
        let scale = if lines { 1. } else { 32. };
        self.camera[0] += x * scale;
        self.camera[1] += y * scale;
        Task::none()
    }
    fn on_zoom(&mut self, factor: f32) -> Task<Message> {
        let anchor = [self.viewport[0] / 2., self.viewport[1] / 2.];
        let world = self.world(anchor);
        self.zoom = (self.zoom * factor).clamp(0.2, 3.);
        self.camera = [
            anchor[0] - world[0] * self.zoom,
            anchor[1] - world[1] * self.zoom,
        ];
        Task::none()
    }
    fn on_fit(&mut self) -> Task<Message> {
        let Some(board) = self.visible() else {
            return Task::none();
        };
        let mut bounds: Option<[f32; 4]> = None;
        for record in board
            .shapes
            .values()
            .filter(|r| r.shape.kind != Kind::Arrow)
        {
            let s = &record.shape;
            let rect = [
                s.x as f32,
                s.y as f32,
                (s.x + s.width) as f32,
                (s.y + s.height) as f32,
            ];
            bounds = Some(match bounds {
                None => rect,
                Some(b) => [
                    b[0].min(rect[0]),
                    b[1].min(rect[1]),
                    b[2].max(rect[2]),
                    b[3].max(rect[3]),
                ],
            });
        }
        let Some(b) = bounds else {
            self.camera = [80., 80.];
            self.zoom = 1.;
            return Task::none();
        };
        self.zoom = ((self.viewport[0] - 100.) / (b[2] - b[0]).max(1.))
            .min((self.viewport[1] - 100.) / (b[3] - b[1]).max(1.))
            .clamp(0.2, 3.);
        self.camera = [
            (self.viewport[0] - (b[2] - b[0]) * self.zoom) / 2. - b[0] * self.zoom,
            (self.viewport[1] - (b[3] - b[1]) * self.zoom) / 2. - b[1] * self.zoom,
        ];
        Task::none()
    }
    fn on_size(&mut self, w: f32, h: f32) -> Task<Message> {
        self.viewport = [w.max(1.), h.max(1.)];
        Task::none()
    }
    fn on_draft(&mut self, text: String) -> Task<Message> {
        self.draft = text;
        Task::none()
    }
    fn on_apply_text(&mut self) -> Task<Message> {
        let Some(id) = self.selected.clone() else {
            return Task::none();
        };
        self.edit(Change::Text {
            id,
            text: self.draft.clone(),
        })
    }
    fn on_color(&mut self, color: u8) -> Task<Message> {
        let Some(id) = self.selected.clone() else {
            return Task::none();
        };
        self.edit(Change::Color { id, color })
    }
    fn on_delete(&mut self) -> Task<Message> {
        let Some(id) = self.selected.take() else {
            return Task::none();
        };
        self.draft.clear();
        self.edit(Change::Delete { id })
    }
    fn on_undo(&mut self) -> Task<Message> {
        let Some(history) = self.undo.pop() else {
            return Task::none();
        };
        let tasks = history
            .undo
            .iter()
            .map(|change| self.enqueue(change.clone()))
            .collect::<Vec<_>>();
        self.redo.push(history);
        Task::batch(tasks)
    }
    fn on_redo(&mut self) -> Task<Message> {
        let Some(history) = self.redo.pop() else {
            return Task::none();
        };
        let tasks = history
            .redo
            .iter()
            .map(|change| self.enqueue(change.clone()))
            .collect::<Vec<_>>();
        self.undo.push(history);
        Task::batch(tasks)
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
