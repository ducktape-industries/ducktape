use super::*;
use wire::keyboard::{Event, Key, Named};

impl BoardsView {
    pub(super) fn only_selected(&self) -> Option<&String> {
        (self.selected.len() == 1)
            .then(|| self.selected.first())
            .flatten()
    }
    pub(super) fn on_key(&mut self, event: Event, captured: bool) -> Task<Message> {
        match event {
            Event::Modifiers(modifiers) => self.on_modifiers(modifiers),
            Event::Release(state) => self.on_key_release(state),
            Event::Press { state, repeat, .. } => self.on_key_press(state, repeat, captured),
        }
    }
    fn on_modifiers(&mut self, modifiers: wire::keyboard::Modifiers) -> Task<Message> {
        self.modifiers = modifiers;
        Task::none()
    }
    fn on_key_release(&mut self, state: wire::keyboard::KeyState) -> Task<Message> {
        self.modifiers = state.modifiers;
        if state.key == Key::Named(Named::Space) {
            self.space_pan = false;
        }
        Task::none()
    }
    fn on_key_press(
        &mut self,
        state: wire::keyboard::KeyState,
        repeat: bool,
        captured: bool,
    ) -> Task<Message> {
        self.modifiers = state.modifiers;
        // Native text fields and the IME own their keys. Editor exit keys are
        // explicit post-IME claims, not a second window-level shortcut handler.
        let typing_or_modified = captured || self.inline.is_some() || state.modifiers.alt;
        if typing_or_modified {
            return Task::none();
        }
        let command = ducktape_view_guest::keyboard::command(state.modifiers);
        let shift = state.modifiers.shift;
        let key = match &state.key {
            Key::Character(text) => text.to_ascii_lowercase(),
            Key::Named(named) => format!("{named:?}"),
            _ => return Task::none(),
        };
        let help_key = key == "?" || (key == "/" && shift);
        if help_key && !command && !repeat {
            return self.on_help();
        }
        if self.help {
            let dismiss = key == "Escape";
            return if dismiss {
                self.on_help()
            } else {
                Task::none()
            };
        }
        match (command, key.as_str()) {
            (true, "a") => self.on_select_all(),
            (true, "c") if !repeat => self.on_copy(),
            (true, "x") if !repeat => self.on_cut(),
            (true, "v") if !repeat => self.on_paste(),
            (true, "]") if !repeat => self.on_stack(true),
            (true, "[") if !repeat => self.on_stack(false),
            (true, "d") if !repeat => self.on_duplicate(),
            (true, "z") if shift => self.on_redo(),
            (true, "z") => self.on_undo(),
            (true, "y") => self.on_redo(),
            (true, "Enter") if !repeat => self.on_quick_note(),
            (false, "Space") => {
                self.space_pan = true;
                Task::none()
            }
            (false, "Escape") => self.on_cancel(),
            (false, "Delete" | "Backspace") => self.on_delete(),
            (false, "Enter") if !repeat => self.begin_text(),
            (false, "ArrowLeft") => self.nudge(if shift { -10 } else { -1 }, 0),
            (false, "ArrowRight") => self.nudge(if shift { 10 } else { 1 }, 0),
            (false, "ArrowUp") => self.nudge(0, if shift { -10 } else { -1 }),
            (false, "ArrowDown") => self.nudge(0, if shift { 10 } else { 1 }),
            (false, "v" | "1") if !repeat => self.on_tool(Tool::Select),
            (false, "h" | "2") if !repeat => self.on_tool(Tool::Hand),
            (false, "n" | "3") if !repeat => self.on_tool(Tool::Note),
            (false, "r" | "4") if !repeat => self.on_tool(Tool::Rectangle),
            (false, "o" | "5") if !repeat => self.on_tool(Tool::Ellipse),
            (false, "d" | "6") if !repeat => self.on_tool(Tool::Diamond),
            (false, "a" | "7") if !repeat => self.on_tool(Tool::Arrow),
            (false, "l" | "8") if !repeat => self.on_tool(Tool::Line),
            (false, "p" | "9") if !repeat => self.on_tool(Tool::Draw),
            (false, "t") if !repeat => self.on_tool(Tool::Text),
            (false, "e") if !repeat => self.on_tool(Tool::Eraser),
            (false, "q") if !repeat => self.on_lock_tool(),
            (false, "?") if !repeat => self.on_help(),
            (false, "0") => self.on_reset_zoom(),
            (false, "f") if shift => self.on_fit_selection(),
            (false, "f") => self.on_fit(),
            (_, "+" | "=") => self.on_zoom(1.25),
            (_, "-" | "_") => self.on_zoom(0.8),
            _ => Task::none(),
        }
    }
    pub(super) fn on_tool(&mut self, tool: Tool) -> Task<Message> {
        let save = self.finish_text();
        if self.inline.is_some() {
            return save;
        }
        self.tool = tool;
        self.gesture = Gesture::Idle;
        self.guides.clear();
        self.help = false;
        save
    }
    pub(super) fn on_lock_tool(&mut self) -> Task<Message> {
        self.tool_locked = !self.tool_locked;
        Task::none()
    }
    pub(super) fn on_help(&mut self) -> Task<Message> {
        self.help = !self.help;
        Task::none()
    }
    pub(super) fn on_board_picker(&mut self) -> Task<Message> {
        self.board_picker = !self.board_picker;
        Task::none()
    }
    pub(super) fn on_snap(&mut self) -> Task<Message> {
        self.snap = !self.snap;
        self.guides.clear();
        Task::none()
    }
    pub(super) fn world(&self, point: [f32; 2]) -> [f32; 2] {
        [
            (point[0] - self.camera[0]) / self.zoom,
            (point[1] - self.camera[1]) / self.zoom,
        ]
    }
    /// The topmost shape under a world point: a card by its own outline, a
    /// connector by the stroke it actually draws.
    pub(super) fn hit(&self, point: [f32; 2]) -> Option<String> {
        let board = self.visible()?;
        self.topmost(&board, point, |_| true)
    }
    fn topmost(
        &self,
        board: &Board,
        point: [f32; 2],
        eligible: impl Fn(&Shape) -> bool,
    ) -> Option<String> {
        // a stroke is thin: give it the same grab margin on screen at any zoom
        let reach = 8. / self.zoom;
        board.ordered().iter().rev().find_map(|(id, record)| {
            let s = &record.shape;
            if !eligible(s) {
                return None;
            }
            let touched = if s.kind.is_path() {
                stroke(board, s)
                    .windows(2)
                    .any(|step| line_distance(point, step[0], step[1]) <= reach)
            } else {
                covers(s, point)
            };
            touched.then(|| (*id).clone())
        })
    }
    pub(super) fn on_position(&mut self, x: f32, y: f32) -> Task<Message> {
        self.cursor = [x, y];
        Task::none()
    }
    pub(super) fn on_begin(&mut self) -> Task<Message> {
        self.on_press(self.cursor[0], self.cursor[1])
    }
    pub(super) fn on_middle_down(&mut self) -> Task<Message> {
        self.gesture = Gesture::Pan {
            start: self.cursor,
            camera: self.camera,
        };
        Task::none()
    }
    pub(super) fn on_press(&mut self, x: f32, y: f32) -> Task<Message> {
        self.cursor = [x, y];
        let editing_here = self.inline.as_ref().is_some_and(|inline| {
            self.visible()
                .and_then(|board| board.shapes.get(&inline.id).cloned())
                .is_some_and(|record| contains(rect(&record.shape), self.world([x, y])))
        });
        if editing_here {
            return Task::none();
        }
        let save = self.finish_text();
        if self.inline.is_some() {
            return save;
        }
        self.help = false;
        self.board_picker = false;
        let panning = self.space_pan || self.tool == Tool::Hand;
        if panning {
            self.gesture = Gesture::Pan {
                start: [x, y],
                camera: self.camera,
            };
            return save;
        }
        let point = self.world([x, y]);
        let action = match self.tool {
            Tool::Select => self.select_press(point),
            Tool::Hand => Task::none(),
            Tool::Eraser => self.erase_press(point),
            Tool::Draw => self.sketch_press(point),
            Tool::Note => self.create_press(Kind::Note, point),
            Tool::Rectangle => self.create_press(Kind::Rectangle, point),
            Tool::Ellipse => self.create_press(Kind::Ellipse, point),
            Tool::Diamond => self.create_press(Kind::Diamond, point),
            Tool::Text => self.create_press(Kind::Text, point),
            Tool::Arrow => self.create_press(Kind::Arrow, point),
            Tool::Line => self.create_press(Kind::Line, point),
        };
        Task::batch([save, action])
    }
    fn select_press(&mut self, point: [f32; 2]) -> Task<Message> {
        let Some(board) = self.visible() else {
            return Task::none();
        };
        self.selected.retain(|id| board.shapes.contains_key(id));
        // Handles sit outside a card: hit them before ordinary card selection.
        if let Some(id) = self.only_selected().cloned() {
            let shape = &board.shapes[&id].shape;
            if free(shape) {
                for corner in [[-1, -1], [1, -1], [-1, 1], [1, 1]] {
                    let target = corner_point(shape, corner);
                    if (point[0] - target[0]).hypot(point[1] - target[1]) <= 9. / self.zoom {
                        self.gesture = Gesture::Resize {
                            id,
                            corner,
                            start: point,
                            point,
                            shape: shape.clone(),
                        };
                        return Task::none();
                    }
                }
            }
        }
        let Some(id) = self.hit(point) else {
            let previous = if self.modifiers.shift {
                self.selected.clone()
            } else {
                BTreeSet::new()
            };
            self.selected = previous.clone();
            self.gesture = Gesture::Marquee {
                start: point,
                point,
                previous,
            };
            return Task::none();
        };
        if self.modifiers.shift {
            if !self.selected.remove(&id) {
                self.selected.insert(id);
            }
            self.gesture = Gesture::Idle;
            return Task::none();
        }
        if !self.selected.contains(&id) {
            self.selected = [id.clone()].into();
        }
        self.palette = board.shapes[&id].shape.color;
        let shapes = self
            .selected
            .iter()
            .filter_map(|id| {
                board
                    .shapes
                    .get(id)
                    .filter(|r| free(&r.shape))
                    .map(|r| (id.clone(), r.shape.clone()))
            })
            .collect();
        self.gesture = Gesture::Move {
            start: point,
            point,
            shapes,
        };
        Task::none()
    }
    fn create_press(&mut self, kind: Kind, point: [f32; 2]) -> Task<Message> {
        self.gesture = Gesture::Create {
            kind,
            start: point,
            point,
        };
        Task::none()
    }
    fn sketch_press(&mut self, point: [f32; 2]) -> Task<Message> {
        self.selected.clear();
        self.gesture = Gesture::Sketch {
            points: vec![point],
        };
        Task::none()
    }
    fn erase_press(&mut self, point: [f32; 2]) -> Task<Message> {
        self.selected.clear();
        self.gesture = Gesture::Erase {
            swept: self.hit(point).into_iter().collect(),
        };
        Task::none()
    }
    pub(super) fn on_double_click(&mut self) -> Task<Message> {
        self.gesture = Gesture::Idle;
        if let Some(id) = self.hit(self.world(self.cursor)) {
            self.selected = [id].into();
            self.begin_text()
        } else {
            let p = self.world(self.cursor);
            self.mint_shape(Shape {
                kind: Kind::Text,
                x: coordinate(p[0]),
                y: coordinate(p[1]),
                width: 260,
                height: 96,
                ..Default::default()
            })
        }
    }
    pub(super) fn mint_shape(&self, shape: Shape) -> Task<Message> {
        let epoch = self.epoch;
        let board = self.current.clone();
        Task::future(async move { Message::Minted(epoch, board, shape, host::mint().await) })
    }
    pub(super) fn on_minted(
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
        let kind = shape.kind;
        self.selected = [id.clone()].into();
        let edit = self.edit(Change::Create {
            id: id.clone(),
            shape,
        });
        if !self
            .visible()
            .is_some_and(|board| board.shapes.contains_key(&id))
        {
            return edit;
        }
        if !self.tool_locked {
            self.tool = Tool::Select;
        }
        let typing = matches!(kind, Kind::Note | Kind::Text);
        if typing {
            Task::batch([edit, self.begin_text()])
        } else {
            edit
        }
    }
    pub(super) fn on_move(&mut self, x: f32, y: f32) -> Task<Message> {
        self.cursor = [x, y];
        let mut point = self.world([x, y]);
        self.guides.clear();
        if let Gesture::Move { start, shapes, .. } = &self.gesture {
            let delta = [point[0] - start[0], point[1] - start[1]];
            if self.modifiers.shift {
                if delta[0].abs() > delta[1].abs() {
                    point[1] = start[1];
                } else {
                    point[0] = start[0];
                }
            }
            if self.snap && !self.modifiers.control && !self.modifiers.shift {
                let (offset, guides) =
                    self.snap_delta(shapes, [point[0] - start[0], point[1] - start[1]]);
                point = [start[0] + offset[0], start[1] + offset[1]];
                self.guides = guides;
            }
        }
        let board = self.visible();
        // The eraser is the one gesture that asks what is under the pointer,
        // and the hit test needs the board this borrow is about to lend out.
        let swept_now = matches!(self.gesture, Gesture::Erase { .. })
            .then(|| self.hit(point))
            .flatten();
        match &mut self.gesture {
            Gesture::Idle => {}
            Gesture::Pan { start, camera } => {
                self.camera = [camera[0] + x - start[0], camera[1] + y - start[1]]
            }
            Gesture::Move { point: p, .. }
            | Gesture::Resize { point: p, .. }
            | Gesture::Create { point: p, .. } => *p = point,
            Gesture::Sketch { points } => {
                // one sample per couple of screen pixels; the release thins
                // the run down to what the shape of the stroke needs
                let far = points.last().is_none_or(|last| {
                    (point[0] - last[0]).hypot(point[1] - last[1]) * self.zoom >= 2.
                });
                if far && points.len() < MAX_SAMPLES {
                    points.push(point);
                }
            }
            Gesture::Erase { swept } => swept.extend(swept_now),
            Gesture::Marquee {
                start,
                point: p,
                previous,
            } => {
                *p = point;
                let bounds = points_rect(*start, point);
                self.selected = previous.clone();
                if let Some(board) = &board {
                    self.selected.extend(
                        board
                            .shapes
                            .iter()
                            .filter(|(_, r)| free(&r.shape) && intersects(bounds, rect(&r.shape)))
                            .map(|(id, _)| id.clone()),
                    );
                }
            }
        }
        Task::none()
    }
    pub(super) fn gesture_changes(&self) -> Vec<Change> {
        match &self.gesture {
            Gesture::Move {
                start,
                point,
                shapes,
            } => {
                let delta = [point[0] - start[0], point[1] - start[1]];
                if delta[0].hypot(delta[1]) * self.zoom < 3. {
                    return Vec::new();
                }
                shapes
                    .iter()
                    .map(|(id, s)| Change::Move {
                        id: id.clone(),
                        x: coordinate(s.x as f32 + delta[0]),
                        y: coordinate(s.y as f32 + delta[1]),
                    })
                    .collect()
            }
            Gesture::Resize {
                id,
                corner,
                start,
                point,
                shape,
            } => {
                let delta = [point[0] - start[0], point[1] - start[1]];
                if delta[0].hypot(delta[1]) * self.zoom < 3. {
                    return Vec::new();
                }
                let mut width = (shape.width as f32 + delta[0] * corner[0] as f32)
                    .clamp(40., boards::MAX_SIZE as f32);
                let mut height = (shape.height as f32 + delta[1] * corner[1] as f32)
                    .clamp(32., boards::MAX_SIZE as f32);
                if self.modifiers.shift {
                    let ratio = shape.width as f32 / shape.height as f32;
                    width = width.max(height * ratio).min(boards::MAX_SIZE as f32);
                    height = (width / ratio).clamp(32., boards::MAX_SIZE as f32);
                }
                let x = shape.x
                    + if corner[0] < 0 {
                        shape.width - width.round() as i32
                    } else {
                        0
                    };
                let y = shape.y
                    + if corner[1] < 0 {
                        shape.height - height.round() as i32
                    } else {
                        0
                    };
                vec![
                    Change::Move {
                        id: id.clone(),
                        x,
                        y,
                    },
                    Change::Resize {
                        id: id.clone(),
                        width: width.round() as i32,
                        height: height.round() as i32,
                    },
                ]
            }
            Gesture::Idle
            | Gesture::Pan { .. }
            | Gesture::Marquee { .. }
            | Gesture::Sketch { .. }
            | Gesture::Erase { .. }
            | Gesture::Create { .. } => Vec::new(),
        }
    }
    /// What a drag with a creating tool would leave behind. A card takes the
    /// dragged box, or a comfortable default when the press was a click; a
    /// connector is the run between the two points and never a click.
    pub(super) fn creation_shape(&self, kind: Kind, start: [f32; 2], point: [f32; 2]) -> Shape {
        if kind.is_path() {
            return self.segment_shape(kind, start, point);
        }
        let b = points_rect(start, point);
        let dragged = (point[0] - start[0]).hypot(point[1] - start[1]) * self.zoom >= 4.;
        let (w, h) = match kind {
            Kind::Note => (220, 180),
            Kind::Rectangle => (240, 140),
            Kind::Ellipse => (200, 200),
            Kind::Diamond => (200, 160),
            Kind::Text => (280, 96),
            Kind::Arrow | Kind::Line | Kind::Draw => (200, 140),
        };
        Shape {
            kind,
            color: self.palette,
            x: coordinate(b[0]),
            y: coordinate(b[1]),
            width: if dragged {
                (b[2] - b[0]).round().clamp(40., 4000.) as i32
            } else {
                w
            },
            height: if dragged {
                (b[3] - b[1]).round().clamp(32., 4000.) as i32
            } else {
                h
            },
            ..Default::default()
        }
    }
    fn segment_shape(&self, kind: Kind, start: [f32; 2], point: [f32; 2]) -> Shape {
        let end = if self.modifiers.shift {
            straighten(start, point)
        } else {
            point
        };
        self.path_shape(kind, &[start, end])
    }
    /// A connector out of world points: the box is their span, the samples
    /// are relative to it, so a later move carries them and a resize scales
    /// them without the board ever rewriting the path.
    fn path_shape(&self, kind: Kind, points: &[[f32; 2]]) -> Shape {
        let b = points
            .iter()
            .fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |a, p| {
                [
                    a[0].min(p[0]),
                    a[1].min(p[1]),
                    a[2].max(p[0]),
                    a[3].max(p[1]),
                ]
            });
        // A run drawn wider than a shape may be is fitted whole rather than
        // clipped: scaling keeps the drawing, truncating loses its tail.
        let limit = boards::MAX_SIZE as f32;
        let fit = (limit / (b[2] - b[0]).max(limit)).min(limit / (b[3] - b[1]).max(limit));
        let size = |span: f32| (span * fit).round().clamp(0., limit);
        Shape {
            kind,
            color: self.palette,
            x: coordinate(b[0]),
            y: coordinate(b[1]),
            width: size(b[2] - b[0]) as i32,
            height: size(b[3] - b[1]) as i32,
            points: points
                .iter()
                .map(|p| [size(p[0] - b[0]) as i32, size(p[1] - b[1]) as i32])
                .collect(),
            ..Default::default()
        }
    }
    /// An arrow drawn onto a card binds to it, so the connection survives the
    /// card moving. Both ends on one card is a free arrow, not a loop.
    fn bind(&self, shape: &mut Shape, start: [f32; 2], end: [f32; 2]) {
        let Some(board) = self.visible() else {
            return;
        };
        let card = |p| self.topmost(&board, p, |s: &Shape| !s.kind.is_path());
        shape.from = card(start);
        shape.to = card(end);
        if shape.from.is_some() && shape.from == shape.to {
            shape.from = None;
            shape.to = None;
        }
    }
    pub(super) fn on_release(&mut self) -> Task<Message> {
        let changes = self.gesture_changes();
        let gesture = std::mem::take(&mut self.gesture);
        self.guides.clear();
        match gesture {
            // Alt on a drag duplicates: the shapes it carried are planted
            // where the pointer let them go and the originals never moved,
            // which is the same picture as dragging a copy off them.
            Gesture::Move { start, point, .. } if self.modifiers.alt => {
                let Some(board) = self.visible() else {
                    return Task::none();
                };
                self.plant(
                    self.selection_shapes(&board),
                    [
                        coordinate(point[0] - start[0]),
                        coordinate(point[1] - start[1]),
                    ],
                )
            }
            Gesture::Create { kind, start, point } => self.on_created(kind, start, point),
            Gesture::Sketch { points } => self.on_sketched(&points),
            Gesture::Erase { swept } => self.on_swept(swept),
            Gesture::Idle
            | Gesture::Pan { .. }
            | Gesture::Marquee { .. }
            | Gesture::Move { .. }
            | Gesture::Resize { .. } => self.edit_many(changes),
        }
    }
    /// What a finished drag with a creating tool leaves on the board, or
    /// nothing when the gesture was too small to have meant anything.
    pub(super) fn drawn_shape(
        &self,
        kind: Kind,
        start: [f32; 2],
        point: [f32; 2],
    ) -> Option<Shape> {
        let mut shape = self.creation_shape(kind, start, point);
        if !kind.is_path() {
            return Some(shape);
        }
        let drawn = (point[0] - start[0]).hypot(point[1] - start[1]) * self.zoom >= 8.;
        if !drawn {
            return None;
        }
        if kind == Kind::Arrow {
            self.bind(&mut shape, start, point);
        }
        Some(shape)
    }
    fn on_created(&mut self, kind: Kind, start: [f32; 2], point: [f32; 2]) -> Task<Message> {
        self.drawn_shape(kind, start, point)
            .map_or_else(Task::none, |shape| self.mint_shape(shape))
    }
    /// What the pen leaves behind: the run thinned to the board's budget, or
    /// a dot when the pen was tapped rather than drawn with.
    pub(super) fn sketched_shape(&self, points: &[[f32; 2]]) -> Option<Shape> {
        let first = points.first().copied()?;
        let mut kept = simplify(points, 1.2 / self.zoom);
        if kept.len() < 2 {
            let dot = 1. / self.zoom;
            kept = vec![first, [first[0] + dot, first[1] + dot]];
        }
        Some(self.path_shape(Kind::Draw, &kept))
    }
    fn on_sketched(&mut self, points: &[[f32; 2]]) -> Task<Message> {
        self.sketched_shape(points)
            .map_or_else(Task::none, |shape| self.mint_shape(shape))
    }
    fn on_swept(&mut self, swept: BTreeSet<String>) -> Task<Message> {
        self.selected.retain(|id| !swept.contains(id));
        self.edit_many(swept.into_iter().map(|id| Change::Delete { id }).collect())
    }
    pub(super) fn on_cancel(&mut self) -> Task<Message> {
        if self.inline.is_some() {
            return self.finish_text();
        }
        if matches!(self.gesture, Gesture::Idle) {
            self.selected.clear();
            self.tool = Tool::Select;
        }
        self.gesture = Gesture::Idle;
        self.space_pan = false;
        self.guides.clear();
        self.help = false;
        self.board_picker = false;
        Task::none()
    }
    pub(super) fn on_wheel(&mut self, x: f32, y: f32, pixels: bool) -> Task<Message> {
        let scale = if pixels { 1. } else { 32. };
        if self.modifiers.control || self.modifiers.logo {
            return self.zoom_at((-y * scale * 0.004).exp(), self.cursor);
        }
        self.camera[0] += x * scale;
        self.camera[1] += y * scale;
        Task::none()
    }
    pub(super) fn zoom_at(&mut self, factor: f32, anchor: [f32; 2]) -> Task<Message> {
        let world = self.world(anchor);
        self.zoom = (self.zoom * factor).clamp(0.1, 8.);
        self.camera = [
            anchor[0] - world[0] * self.zoom,
            anchor[1] - world[1] * self.zoom,
        ];
        Task::none()
    }
    pub(super) fn on_zoom(&mut self, factor: f32) -> Task<Message> {
        self.zoom_at(factor, [self.viewport[0] / 2., self.viewport[1] / 2.])
    }
    pub(super) fn on_reset_zoom(&mut self) -> Task<Message> {
        self.on_zoom(1. / self.zoom)
    }
    pub(super) fn on_fit(&mut self) -> Task<Message> {
        self.fit(false)
    }
    pub(super) fn on_fit_selection(&mut self) -> Task<Message> {
        self.fit(true)
    }
    fn fit(&mut self, selected: bool) -> Task<Message> {
        let Some(board) = self.visible() else {
            return Task::none();
        };
        let shapes: Vec<_> = board
            .shapes
            .iter()
            .filter(|(id, r)| free(&r.shape) && (!selected || self.selected.contains(*id)))
            .map(|(_, r)| &r.shape)
            .collect();
        let Some(b) = bounds(shapes.into_iter()) else {
            self.camera = [80., 80.];
            self.zoom = 1.;
            return Task::none();
        };
        self.zoom = ((self.viewport[0] - 200.) / (b[2] - b[0]).max(1.))
            .min((self.viewport[1] - 200.) / (b[3] - b[1]).max(1.))
            .clamp(0.1, 2.);
        self.camera = [
            (self.viewport[0] - (b[2] - b[0]) * self.zoom) / 2. - b[0] * self.zoom,
            (self.viewport[1] - (b[3] - b[1]) * self.zoom) / 2. - b[1] * self.zoom,
        ];
        Task::none()
    }
    pub(super) fn on_size(&mut self, w: f32, h: f32) -> Task<Message> {
        self.viewport = [w.max(1.), h.max(1.)];
        Task::none()
    }
    pub(super) fn begin_text(&mut self) -> Task<Message> {
        let Some(id) = self.only_selected().cloned() else {
            return Task::none();
        };
        let Some(board) = self.visible() else {
            return Task::none();
        };
        let Some(record) = board.shapes.get(&id) else {
            return Task::none();
        };
        if record.shape.kind.is_path() || self.inline.is_some() {
            return Task::none();
        }
        let text = record.shape.text.clone();
        self.gesture = Gesture::Idle;
        self.inline = Some(Inline {
            id: id.clone(),
            original: text.clone(),
            document: Editor::new(text),
        });
        Task::none()
    }
    pub(super) fn focus_text(&self) -> Task<Message> {
        let Some(inline) = &self.inline else {
            return Task::none();
        };
        let id = inline.id.clone();
        let command = wire::WidgetCommand::Focus {
            target: format!("boards/editor/{id}"),
        };
        Task::future(async move {
            Message::FocusResult(
                id,
                ducktape_view_guest::host::request("host.widget", &wire::encode(&command))
                    .await
                    .map(|_| ()),
            )
        })
    }
    pub(super) fn on_focus_result(
        &mut self,
        id: String,
        result: Result<(), String>,
    ) -> Task<Message> {
        let still_editing = self.inline.as_ref().is_some_and(|inline| inline.id == id);
        if !still_editing {
            return Task::none();
        }
        let Err(error) = result else {
            return Task::none();
        };
        // Native layout and document hydration can replace the requesting frame.
        // Reissue against the current frame only while the same editor is open;
        // the host still enforces its original frame-scoped capability check.
        let replaced_frame = error == "widget request belongs to a replaced frame";
        if replaced_frame {
            return self.focus_text();
        }
        self.error = "Could not focus the text editor. Click inside the card to continue.".into();
        Task::none()
    }
    pub(super) fn on_text_transaction(
        &mut self,
        transaction: ducktape_view_guest::EditorTransaction<Message>,
    ) -> Task<Message> {
        let Some(inline) = &mut self.inline else {
            return Task::none();
        };
        transaction
            .apply(&mut inline.document)
            .map_or_else(Task::none, Task::done)
    }
    pub(super) fn on_text_document(
        &mut self,
        document: ducktape_view_guest::EditorDocumentUpdate,
    ) -> Task<Message> {
        if let Some(inline) = &mut self.inline {
            document.apply(&mut inline.document);
        }
        Task::none()
    }
    pub(super) fn finish_text(&mut self) -> Task<Message> {
        let Some(inline) = self.inline.take() else {
            return Task::none();
        };
        let text = inline.document.text();
        if text.len() > boards::MAX_TEXT {
            self.error = format!(
                "This card holds up to {} bytes of text. Shorten it before leaving.",
                boards::MAX_TEXT
            );
            self.inline = Some(inline);
            return Task::none();
        }
        let changed = text != inline.original;
        if changed && self.pending.len() >= 64 {
            self.error =
                "Waiting for earlier edits to save. Retry saving before closing this card.".into();
            self.inline = Some(inline);
            return Task::none();
        }
        let save = if changed {
            self.edit(Change::Text {
                id: inline.id,
                text,
            })
        } else {
            Task::none()
        };
        Task::batch([
            save,
            ducktape_view_guest::widget::perform(wire::WidgetCommand::Focus {
                target: "boards/canvas-layout".into(),
            }),
        ])
    }
    pub(super) fn on_color(&mut self, color: u8) -> Task<Message> {
        self.palette = color;
        self.edit_many(
            self.selected
                .iter()
                .map(|id| Change::Color {
                    id: id.clone(),
                    color,
                })
                .collect(),
        )
    }
    pub(super) fn on_delete(&mut self) -> Task<Message> {
        let changes = self
            .selected
            .iter()
            .map(|id| Change::Delete { id: id.clone() })
            .collect();
        let task = self.edit_many(changes);
        self.selected.clear();
        self.inline = None;
        task
    }
    pub(super) fn on_select_all(&mut self) -> Task<Message> {
        if let Some(board) = self.visible() {
            self.selected = board.shapes.keys().cloned().collect();
        }
        Task::none()
    }
    fn nudge(&mut self, x: i32, y: i32) -> Task<Message> {
        let Some(board) = self.visible() else {
            return Task::none();
        };
        self.edit_many(
            self.selected
                .iter()
                .filter_map(|id| {
                    board
                        .shapes
                        .get(id)
                        .filter(|r| free(&r.shape))
                        .map(|r| Change::Move {
                            id: id.clone(),
                            x: coordinate((r.shape.x + x) as f32),
                            y: coordinate((r.shape.y + y) as f32),
                        })
                })
                .collect(),
        )
    }
    pub(super) fn on_undo(&mut self) -> Task<Message> {
        if self.pending.len() >= 64 {
            return Task::none();
        }
        let Some(history) = self.undo.last().cloned() else {
            return Task::none();
        };
        let before = self.pending.len();
        let task = self.enqueue_many(history.undo.clone());
        if self.pending.len() > before {
            self.undo.pop();
            self.redo.push(history);
        }
        task
    }
    pub(super) fn on_redo(&mut self) -> Task<Message> {
        if self.pending.len() >= 64 {
            return Task::none();
        }
        let Some(history) = self.redo.last().cloned() else {
            return Task::none();
        };
        let before = self.pending.len();
        let task = self.enqueue_many(history.redo.clone());
        if self.pending.len() > before {
            self.redo.pop();
            self.undo.push(history);
        }
        task
    }
    /// The selection as a copyable set: in stacking order, with the cards
    /// before the connectors that name them, and without any connector whose
    /// ends do not both land inside the set — that arrow has nothing on the
    /// far end to be copied against.
    fn selection_shapes(&self, board: &Board) -> Vec<(String, Shape)> {
        let picked: BTreeSet<_> = self
            .selected
            .iter()
            .filter(|id| board.shapes.contains_key(*id))
            .cloned()
            .collect();
        let whole = |s: &Shape| {
            [&s.from, &s.to]
                .into_iter()
                .flatten()
                .all(|id| picked.contains(id))
        };
        let mut shapes: Vec<_> = board
            .ordered()
            .into_iter()
            .filter(|(id, r)| picked.contains(*id) && whole(&r.shape))
            .map(|(id, r)| (id.clone(), r.shape.clone()))
            .collect();
        shapes.sort_by_key(|(_, s)| s.from.is_some() || s.to.is_some());
        shapes
    }
    /// Put a set of shapes on the board under fresh ids, shifted by `offset`.
    /// Every id is minted before anything is written, so a mint that fails
    /// leaves the board untouched rather than half-planted.
    fn plant(&mut self, shapes: Vec<(String, Shape)>, offset: [i32; 2]) -> Task<Message> {
        if shapes.is_empty() {
            return Task::none();
        }
        let Some(board) = self.visible() else {
            return Task::none();
        };
        if board.shapes.len() + shapes.len() > boards::MAX_SHAPES {
            self.error = "Not enough room on this board for that many shapes.".into();
            return Task::none();
        }
        let epoch = self.epoch;
        let current = self.current.clone();
        Task::future(async move {
            let mut ids = Vec::new();
            for _ in &shapes {
                match host::mint().await {
                    Ok(id) => ids.push(id),
                    Err(error) => {
                        return Message::Planted(epoch, current, shapes, offset, Err(error));
                    }
                }
            }
            Message::Planted(epoch, current, shapes, offset, Ok(ids))
        })
    }
    pub(super) fn on_planted(
        &mut self,
        epoch: u64,
        board: String,
        shapes: Vec<(String, Shape)>,
        offset: [i32; 2],
        ids: Result<Vec<String>, String>,
    ) -> Task<Message> {
        if epoch != self.epoch || board != self.current {
            return Task::none();
        }
        let ids = match ids {
            Ok(ids) => ids,
            Err(error) => {
                self.error = error;
                return Task::none();
            }
        };
        // a binding that named a copied card now names its copy; one that
        // named anything else was already dropped from the set
        let mapping: BTreeMap<_, _> = shapes
            .iter()
            .zip(&ids)
            .map(|((old, _), id)| (old.clone(), id.clone()))
            .collect();
        let changes = shapes
            .into_iter()
            .zip(&ids)
            .map(|((_, mut s), id)| {
                s.x = coordinate((s.x + offset[0]) as f32);
                s.y = coordinate((s.y + offset[1]) as f32);
                s.from = s.from.and_then(|id| mapping.get(&id).cloned());
                s.to = s.to.and_then(|id| mapping.get(&id).cloned());
                Change::Create {
                    id: id.clone(),
                    shape: s,
                }
            })
            .collect();
        let task = self.edit_many(changes);
        self.selected = ids.into_iter().collect();
        task
    }
    pub(super) fn on_duplicate(&mut self) -> Task<Message> {
        let Some(board) = self.visible() else {
            return Task::none();
        };
        self.plant(self.selection_shapes(&board), [24, 24])
    }
    /// The copy keeps the ids it was taken under, because a connector in the
    /// set names its cards by id and the paste remaps from exactly those.
    pub(super) fn on_copy(&mut self) -> Task<Message> {
        let Some(board) = self.visible() else {
            return Task::none();
        };
        self.clipboard = self.selection_shapes(&board);
        Task::none()
    }
    pub(super) fn on_cut(&mut self) -> Task<Message> {
        let copied = self.on_copy();
        Task::batch([copied, self.on_delete()])
    }
    /// Paste under the pointer: the copied set keeps its own arrangement,
    /// moved so its top-left corner meets the cursor.
    pub(super) fn on_paste(&mut self) -> Task<Message> {
        let shapes = self.clipboard.clone();
        let Some(b) = bounds(shapes.iter().map(|(_, s)| s)) else {
            return Task::none();
        };
        let at = self.world(self.cursor);
        let offset = [coordinate(at[0] - b[0]), coordinate(at[1] - b[1])];
        self.plant(shapes, offset)
    }
    /// Stacking: raising the selection puts it on top, and sinking it is
    /// raising everything else — one primitive, both directions.
    pub(super) fn on_stack(&mut self, front: bool) -> Task<Message> {
        let Some(board) = self.visible() else {
            return Task::none();
        };
        let order: Vec<String> = board
            .ordered()
            .into_iter()
            .map(|(id, _)| id.clone())
            .collect();
        let ids: Vec<String> = order
            .iter()
            .filter(|id| self.selected.contains(*id) == front)
            .cloned()
            .collect();
        let movable = !ids.is_empty() && ids.len() < order.len();
        if !movable {
            return Task::none();
        }
        self.edit(Change::Order { ids })
    }
    pub(super) fn on_quick_note(&mut self) -> Task<Message> {
        let p = self.world([self.viewport[0] / 2. - 110., self.viewport[1] / 2. - 90.]);
        let shape = self
            .only_selected()
            .and_then(|id| {
                self.visible()?.shapes.get(id).map(|r| Shape {
                    x: r.shape.x + r.shape.width + 40,
                    y: r.shape.y,
                    color: r.shape.color,
                    ..r.shape.clone()
                })
            })
            .unwrap_or(Shape {
                kind: Kind::Note,
                x: coordinate(p[0]),
                y: coordinate(p[1]),
                width: 220,
                height: 180,
                color: self.palette,
                ..Default::default()
            });
        self.mint_shape(Shape {
            text: String::new(),
            from: None,
            to: None,
            kind: Kind::Note,
            ..shape
        })
    }
    pub(super) fn on_template(&mut self) -> Task<Message> {
        let column = |name: &str, prompt: &str, at: i32, color: u8| {
            (
                name.to_owned(),
                Shape {
                    kind: Kind::Note,
                    x: at,
                    y: 80,
                    width: 240,
                    height: 200,
                    text: format!("{name}\n\n{prompt}"),
                    color,
                    ..Default::default()
                },
            )
        };
        self.plant(
            vec![
                column("Ideas", "What could we try?", 0, 0),
                column("Questions", "What do we need to learn?", 300, 1),
                column("Next steps", "Choose one thing to move forward.", 600, 2),
            ],
            [0, 0],
        )
    }
    /// Line a selection up, or spread it evenly. Every arrangement is a set
    /// of moves against the selection's own bounding box.
    pub(super) fn on_arrange(&mut self, how: Arrange) -> Task<Message> {
        let Some(board) = self.visible() else {
            return Task::none();
        };
        let mut picked: Vec<(String, Shape)> = self
            .selected
            .iter()
            .filter_map(|id| {
                board
                    .shapes
                    .get(id)
                    .filter(|r| free(&r.shape))
                    .map(|r| (id.clone(), r.shape.clone()))
            })
            .collect();
        let Some(b) = bounds(picked.iter().map(|(_, s)| s)) else {
            return Task::none();
        };
        let changes = match how {
            Arrange::SpreadX => spread(&mut picked, b, 0),
            Arrange::SpreadY => spread(&mut picked, b, 1),
            Arrange::Left => line_up(&picked, 0, |_| b[0]),
            Arrange::CentreX => line_up(&picked, 0, |size| (b[0] + b[2] - size) / 2.),
            Arrange::Right => line_up(&picked, 0, |size| b[2] - size),
            Arrange::Top => line_up(&picked, 1, |_| b[1]),
            Arrange::CentreY => line_up(&picked, 1, |size| (b[1] + b[3] - size) / 2.),
            Arrange::Bottom => line_up(&picked, 1, |size| b[3] - size),
        };
        self.edit_many(changes)
    }
    fn snap_delta(
        &self,
        shapes: &BTreeMap<String, Shape>,
        delta: [f32; 2],
    ) -> ([f32; 2], Vec<[f32; 4]>) {
        let Some(b) = bounds(shapes.values()) else {
            return (delta, Vec::new());
        };
        let Some(board) = &self.confirmed else {
            return (delta, Vec::new());
        };
        let mut best = [6. / self.zoom; 2];
        let mut adjustment = [0.; 2];
        let mut lines = [None, None];
        for (id, r) in &board.shapes {
            if shapes.contains_key(id) || r.shape.kind.is_path() {
                continue;
            }
            let target = rect(&r.shape);
            for axis in 0..2 {
                for moving in [b[axis], (b[axis] + b[axis + 2]) / 2., b[axis + 2]] {
                    for fixed in [
                        target[axis],
                        (target[axis] + target[axis + 2]) / 2.,
                        target[axis + 2],
                    ] {
                        let diff = fixed - (moving + delta[axis]);
                        if diff.abs() < best[axis] {
                            best[axis] = diff.abs();
                            adjustment[axis] = diff;
                            lines[axis] = Some(if axis == 0 {
                                [
                                    fixed,
                                    b[1].min(target[1]) - 20.,
                                    fixed,
                                    b[3].max(target[3]) + 20.,
                                ]
                            } else {
                                [
                                    b[0].min(target[0]) - 20.,
                                    fixed,
                                    b[2].max(target[2]) + 20.,
                                    fixed,
                                ]
                            });
                        }
                    }
                }
            }
        }
        (
            [delta[0] + adjustment[0], delta[1] + adjustment[1]],
            lines.into_iter().flatten().collect(),
        )
    }
}
/// The pen samples no more than this in one stroke; the release thins the run
/// down to the board's point budget before anything leaves the view.
const MAX_SAMPLES: usize = 4096;

/// A shape the pointer moves and resizes on its own. A bound connector has no
/// geometry of its own to drag: it follows the cards its ends name.
pub(super) fn free(s: &Shape) -> bool {
    s.from.is_none() && s.to.is_none()
}
fn axis_start(s: &Shape, axis: usize) -> f32 {
    if axis == 0 { s.x as f32 } else { s.y as f32 }
}
fn axis_size(s: &Shape, axis: usize) -> f32 {
    if axis == 0 {
        s.width as f32
    } else {
        s.height as f32
    }
}
/// Move every shape onto one line along `axis`; `place` says where a shape of
/// a given size starts on it.
fn line_up(picked: &[(String, Shape)], axis: usize, place: impl Fn(f32) -> f32) -> Vec<Change> {
    picked
        .iter()
        .map(|(id, s)| {
            let at = coordinate(place(axis_size(s, axis)));
            Change::Move {
                id: id.clone(),
                x: if axis == 0 { at } else { s.x },
                y: if axis == 1 { at } else { s.y },
            }
        })
        .collect()
}
/// Even gaps along `axis`, the selection's own extent kept. Two shapes have
/// only one gap and nothing to even out.
fn spread(picked: &mut [(String, Shape)], b: [f32; 4], axis: usize) -> Vec<Change> {
    if picked.len() < 3 {
        return Vec::new();
    }
    picked.sort_by(|a, c| axis_start(&a.1, axis).total_cmp(&axis_start(&c.1, axis)));
    let filled: f32 = picked.iter().map(|(_, s)| axis_size(s, axis)).sum();
    let gap = (b[axis + 2] - b[axis] - filled) / (picked.len() - 1) as f32;
    let mut at = b[axis];
    picked
        .iter()
        .map(|(id, s)| {
            let start = coordinate(at);
            at += axis_size(s, axis) + gap;
            Change::Move {
                id: id.clone(),
                x: if axis == 0 { start } else { s.x },
                y: if axis == 1 { start } else { s.y },
            }
        })
        .collect()
}
pub(super) fn center(s: &Shape) -> [f32; 2] {
    [
        s.x as f32 + s.width as f32 / 2.,
        s.y as f32 + s.height as f32 / 2.,
    ]
}
/// Whether a card's own outline covers a point — the box for a note, the
/// inscribed ellipse or diamond for the shapes that only fill part of it.
pub(super) fn covers(s: &Shape, p: [f32; 2]) -> bool {
    let unit = [
        (p[0] - s.x as f32) / (s.width as f32).max(1.) * 2. - 1.,
        (p[1] - s.y as f32) / (s.height as f32).max(1.) * 2. - 1.,
    ];
    match s.kind {
        Kind::Note | Kind::Rectangle | Kind::Text => contains(rect(s), p),
        Kind::Ellipse => unit[0] * unit[0] + unit[1] * unit[1] <= 1.,
        Kind::Diamond => unit[0].abs() + unit[1].abs() <= 1.,
        Kind::Arrow | Kind::Line | Kind::Draw => false,
    }
}
/// A path's samples in world units. They are stored against the box they were
/// drawn in, so a move carries them and a resize scales them.
pub(super) fn path_points(s: &Shape) -> Vec<[f32; 2]> {
    let span = s.points.iter().fold([0_f32; 2], |a, p| {
        [a[0].max(p[0] as f32), a[1].max(p[1] as f32)]
    });
    let scale = |axis: usize, size: i32| {
        if span[axis] > 0. {
            size as f32 / span[axis]
        } else {
            1.
        }
    };
    let (sx, sy) = (scale(0, s.width), scale(1, s.height));
    s.points
        .iter()
        .map(|p| [s.x as f32 + p[0] as f32 * sx, s.y as f32 + p[1] as f32 * sy])
        .collect()
}
/// The polyline a connector actually draws: its samples, with a bound end
/// pulled onto the border of the card it names.
pub(super) fn stroke(board: &Board, s: &Shape) -> Vec<[f32; 2]> {
    let mut path = path_points(s);
    if path.len() < 2 {
        return Vec::new();
    }
    let card = |key: &Option<String>| {
        key.as_ref()
            .and_then(|k| board.shapes.get(k))
            .map(|r| r.shape.clone())
    };
    let (from, to) = (card(&s.from), card(&s.to));
    let last = path.len() - 1;
    let toward_start = to.as_ref().map_or(path[last], center);
    let toward_end = from.as_ref().map_or(path[0], center);
    if let Some(card) = &from {
        path[0] = border_point(card, toward_start);
    }
    if let Some(card) = &to {
        path[last] = border_point(card, toward_end);
    }
    path
}
/// Where a ray out of a card's centre leaves it, so a connector stops at the
/// edge instead of burying its head in the card.
pub(super) fn border_point(s: &Shape, toward: [f32; 2]) -> [f32; 2] {
    let c = center(s);
    let d = [toward[0] - c[0], toward[1] - c[1]];
    let scale = (s.width as f32 / 2. / d[0].abs().max(0.001))
        .min(s.height as f32 / 2. / d[1].abs().max(0.001))
        .min(1.);
    [c[0] + d[0] * scale, c[1] + d[1] * scale]
}
/// Shift on a connector: the nearest eighth turn, so runs come out straight
/// or squarely diagonal.
fn straighten(start: [f32; 2], point: [f32; 2]) -> [f32; 2] {
    let d = [point[0] - start[0], point[1] - start[1]];
    let step = std::f32::consts::FRAC_PI_4;
    let angle = (d[1].atan2(d[0]) / step).round() * step;
    let length = d[0].hypot(d[1]);
    [
        start[0] + length * angle.cos(),
        start[1] + length * angle.sin(),
    ]
}
/// Ramer–Douglas–Peucker, run at a coarser tolerance until the stroke fits
/// the board's point budget. A pen samples far more than a shape needs.
fn simplify(points: &[[f32; 2]], tolerance: f32) -> Vec<[f32; 2]> {
    let mut tolerance = tolerance.max(0.01);
    loop {
        let kept = thin(points, tolerance);
        if kept.len() <= boards::MAX_POINTS {
            return kept;
        }
        tolerance *= 2.;
    }
}
pub(super) fn thin(points: &[[f32; 2]], tolerance: f32) -> Vec<[f32; 2]> {
    if points.len() < 3 {
        return points.to_vec();
    }
    let mut keep = vec![false; points.len()];
    let last = points.len() - 1;
    keep[0] = true;
    keep[last] = true;
    let mut spans = vec![(0, last)];
    while let Some((start, end)) = spans.pop() {
        let Some((index, distance)) = (start + 1..end)
            .map(|i| (i, line_distance(points[i], points[start], points[end])))
            .max_by(|a, b| a.1.total_cmp(&b.1))
        else {
            continue;
        };
        if distance <= tolerance {
            continue;
        }
        keep[index] = true;
        spans.push((start, index));
        spans.push((index, end));
    }
    points
        .iter()
        .zip(keep)
        .filter_map(|(p, kept)| kept.then_some(*p))
        .collect()
}
pub(super) fn rect(s: &Shape) -> [f32; 4] {
    [
        s.x as f32,
        s.y as f32,
        (s.x + s.width) as f32,
        (s.y + s.height) as f32,
    ]
}
pub(super) fn bounds<'a>(shapes: impl Iterator<Item = &'a Shape>) -> Option<[f32; 4]> {
    shapes.map(rect).reduce(|a, b| {
        [
            a[0].min(b[0]),
            a[1].min(b[1]),
            a[2].max(b[2]),
            a[3].max(b[3]),
        ]
    })
}
pub(super) fn points_rect(a: [f32; 2], b: [f32; 2]) -> [f32; 4] {
    [
        a[0].min(b[0]),
        a[1].min(b[1]),
        a[0].max(b[0]),
        a[1].max(b[1]),
    ]
}
pub(super) fn contains(b: [f32; 4], p: [f32; 2]) -> bool {
    p[0] >= b[0] && p[1] >= b[1] && p[0] <= b[2] && p[1] <= b[3]
}
fn intersects(a: [f32; 4], b: [f32; 4]) -> bool {
    a[0] <= b[2] && a[2] >= b[0] && a[1] <= b[3] && a[3] >= b[1]
}
pub(super) fn corner_point(s: &Shape, c: [i32; 2]) -> [f32; 2] {
    [
        s.x as f32 + if c[0] > 0 { s.width as f32 } else { 0. },
        s.y as f32 + if c[1] > 0 { s.height as f32 } else { 0. },
    ]
}
fn line_distance(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let d = [b[0] - a[0], b[1] - a[1]];
    let t = (((p[0] - a[0]) * d[0] + (p[1] - a[1]) * d[1])
        / (d[0] * d[0] + d[1] * d[1]).max(0.001))
    .clamp(0., 1.);
    (p[0] - a[0] - t * d[0]).hypot(p[1] - a[1] - t * d[1])
}
