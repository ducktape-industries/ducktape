use super::*;
use wire::keyboard::{Event, Key, Named};

impl BoardsView {
    pub(super) fn only_selected(&self) -> Option<&String> {
        (self.selected.len() == 1)
            .then(|| self.selected.first())
            .flatten()
    }
    /// The box around a selection of more than one, and the shapes inside it
    /// that a scale may move — a connector holding a card is not one of them,
    /// because its box is dictated by the cards it names and it will follow
    /// them without being told.
    ///
    /// `None` for a selection of one: that shape wears its own handles, and a
    /// second box around a single outline would read as two selections.
    pub(super) fn group(&self, board: &Board) -> Option<([f32; 4], BTreeMap<String, Shape>)> {
        if self.selected.len() < 2 || self.inline.is_some() {
            return None;
        }
        let mut shapes = BTreeMap::new();
        let mut bounds = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for id in &self.selected {
            let Some(record) = board.shapes.get(id) else {
                continue;
            };
            let s = &record.shape;
            bounds = [
                bounds[0].min(s.x as f32),
                bounds[1].min(s.y as f32),
                bounds[2].max((s.x + s.width) as f32),
                bounds[3].max((s.y + s.height) as f32),
            ];
            if free(s) {
                shapes.insert(id.clone(), s.clone());
            }
        }
        if shapes.is_empty() {
            return None;
        }
        Some((
            [
                bounds[0],
                bounds[1],
                bounds[2] - bounds[0],
                bounds[3] - bounds[1],
            ],
            shapes,
        ))
    }
    /// The box a handle drag leaves behind, given the box it started from.
    /// The same arithmetic a single shape's resize uses, over a group's box:
    /// the handle moves, the opposite side stays, and a sign of zero on an
    /// axis pins that axis entirely.
    fn dragged_box(
        &self,
        bounds: [f32; 4],
        corner: [i32; 2],
        delta: [f32; 2],
        least: [f32; 2],
    ) -> [f32; 4] {
        let mut size = [
            (bounds[2] + delta[0] * corner[0] as f32).clamp(least[0], boards::MAX_SIZE as f32),
            (bounds[3] + delta[1] * corner[1] as f32).clamp(least[1], boards::MAX_SIZE as f32),
        ];
        if self.modifiers.shift && bounds[2] > 0. && bounds[3] > 0. {
            let ratio = bounds[2] / bounds[3];
            size[0] = size[0].max(size[1] * ratio).min(boards::MAX_SIZE as f32);
            size[1] = (size[0] / ratio).clamp(least[1], boards::MAX_SIZE as f32);
        }
        [
            bounds[0]
                + if corner[0] < 0 {
                    bounds[2] - size[0]
                } else {
                    0.
                },
            bounds[1]
                + if corner[1] < 0 {
                    bounds[3] - size[1]
                } else {
                    0.
                },
            size[0],
            size[1],
        ]
    }
    /// Every member re-placed into the box the handle drew, keeping the share
    /// of it that it had. Two changes per shape and not a third: a card's
    /// samples are already stored against its own box, so a run scales with
    /// the box it is given and needs no re-routing.
    fn scaled(
        &self,
        corner: [i32; 2],
        start: [f32; 2],
        point: [f32; 2],
        bounds: [f32; 4],
        shapes: &BTreeMap<String, Shape>,
    ) -> Vec<Change> {
        let delta = [point[0] - start[0], point[1] - start[1]];
        if delta[0].hypot(delta[1]) * self.zoom < 3. {
            return Vec::new();
        }
        // The group may not shrink past the point where its smallest card
        // would stop being a card — the module refuses that shape, and a
        // refused change would drop the whole gesture rather than this member.
        let floor = shapes.values().fold([0_f32; 2], |a, s| {
            let least = least(s);
            let share = |axis: usize, span: f32, size: f32| {
                if size <= 0. {
                    0.
                } else {
                    least[axis] * span / size
                }
            };
            [
                a[0].max(share(0, bounds[2], s.width as f32)),
                a[1].max(share(1, bounds[3], s.height as f32)),
            ]
        });
        let drawn = self.dragged_box(bounds, corner, delta, floor);
        let scale = |axis: usize| {
            if bounds[2 + axis] > 0. {
                drawn[2 + axis] / bounds[2 + axis]
            } else {
                1.
            }
        };
        let (sx, sy) = (scale(0), scale(1));
        let mut changes = Vec::new();
        for (id, s) in shapes {
            let x = drawn[0] + (s.x as f32 - bounds[0]) * sx;
            let y = drawn[1] + (s.y as f32 - bounds[1]) * sy;
            let least = least(s);
            changes.push(Change::Move {
                id: id.clone(),
                x: coordinate(x),
                y: coordinate(y),
            });
            changes.push(Change::Resize {
                id: id.clone(),
                width: (s.width as f32 * sx).round().max(least[0]) as i32,
                height: (s.height as f32 * sy).round().max(least[1]) as i32,
            });
        }
        changes
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
        let arrow = matches!(
            state.key,
            Key::Named(Named::ArrowLeft | Named::ArrowRight | Named::ArrowUp | Named::ArrowDown)
        );
        if !arrow {
            return Task::none();
        }
        self.settle()
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
        // Any key that is not the one being held ends the hold, so a nudge
        // reaches the board before whatever comes next reads it.
        let arrow = key.starts_with("Arrow");
        let settled = if arrow { Task::none() } else { self.settle() };
        let acted = match (command, key.as_str()) {
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
        };
        Task::batch([settled, acted])
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
    pub(super) fn topmost(
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
        let save = Task::batch([self.settle(), self.finish_text()]);
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
            if shape.kind.is_path() {
                // A connector is grabbed by its ends — that is where it
                // reaches for a card, and the samples between them are the
                // run itself rather than places to take hold of it.
                let run = stroke(&board, shape);
                let ends = [0, run.len().saturating_sub(1)];
                for end in ends {
                    let Some(target) = run.get(end) else { continue };
                    if (point[0] - target[0]).hypot(point[1] - target[1]) <= 9. / self.zoom {
                        self.gesture = Gesture::Endpoint {
                            id,
                            end,
                            point,
                            shape: shape.clone(),
                        };
                        return Task::none();
                    }
                }
            }
            if free(shape) {
                for corner in HANDLES {
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
        // Several shapes are taken by the box drawn around them, not by any
        // one of their own outlines: the handles belong to the group.
        if let Some((bounds, shapes)) = self.group(&board) {
            for corner in HANDLES {
                let target = handle_point(bounds, corner);
                if (point[0] - target[0]).hypot(point[1] - target[1]) <= 9. / self.zoom {
                    self.gesture = Gesture::Scale {
                        corner,
                        start: point,
                        point,
                        bounds,
                        shapes,
                    };
                    return Task::none();
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
            last: point,
        };
        Task::none()
    }
    pub(super) fn on_double_click(&mut self) -> Task<Message> {
        self.gesture = Gesture::Idle;
        if let Some(id) = self.hit(self.world(self.cursor)) {
            self.selected = [id].into();
            self.begin_text()
        } else {
            // A double-click on open board writes, the way a canvas app does.
            // It goes through the same creation the tools use, so the shape
            // lands centred on the pointer and in the colour the palette is
            // showing — a second, private idea of a new shape would drift.
            let p = self.world(self.cursor);
            self.mint_shape(self.creation_shape(Kind::Text, p, p))
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
        // The pen is the one tool you reach for to make several marks in a
        // row, so it keeps itself; every other tool hands back to Select the
        // way a canvas app does, unless the lock says otherwise.
        let one_shot = kind != Kind::Draw;
        if one_shot && !self.tool_locked {
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
        // It asks along the whole step, not just where the step landed: a
        // quick sweep reports a handful of far-apart samples, and testing only
        // those leaves untouched shapes the stroke visibly went through.
        let swept_now: BTreeSet<String> = match &self.gesture {
            Gesture::Erase { last, .. } => {
                let step = 6. / self.zoom;
                let span = (point[0] - last[0]).hypot(point[1] - last[1]);
                let stops = (span / step).ceil().clamp(1., 64.) as usize;
                (0..=stops)
                    .filter_map(|i| {
                        let t = i as f32 / stops as f32;
                        self.hit([
                            last[0] + (point[0] - last[0]) * t,
                            last[1] + (point[1] - last[1]) * t,
                        ])
                    })
                    .collect()
            }
            _ => BTreeSet::new(),
        };
        // With nothing in hand, say what a press would take. The eraser and the
        // arrow point at a card the way Select does; the shape tools are going
        // to draw, so nothing under the pointer is theirs to highlight.
        let picks = matches!(
            self.tool,
            Tool::Select | Tool::Eraser | Tool::Arrow | Tool::Line
        );
        self.hover = match (&self.gesture, picks) {
            (Gesture::Idle, true) => self.hit(point),
            _ => None,
        };
        match &mut self.gesture {
            // The pointer moving says nothing about a key being held down.
            Gesture::Idle | Gesture::Nudge { .. } => {}
            Gesture::Pan { start, camera } => {
                self.camera = [camera[0] + x - start[0], camera[1] + y - start[1]]
            }
            Gesture::Move { point: p, .. }
            | Gesture::Resize { point: p, .. }
            | Gesture::Scale { point: p, .. }
            | Gesture::Create { point: p, .. }
            | Gesture::Endpoint { point: p, .. } => *p = point,
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
            Gesture::Erase { swept, last } => {
                swept.extend(swept_now);
                *last = point;
            }
            Gesture::Marquee {
                start,
                point: p,
                previous,
            } => {
                *p = point;
                let bounds = points_rect(*start, point);
                self.selected = previous.clone();
                if let Some(board) = &board {
                    // A rubber band asks what it went over, not what may be
                    // dragged: a bound arrow is drawn where its cards put it,
                    // so that is the geometry it is caught by. Sweeping a
                    // diagram must take its edges or a recolour misses them.
                    self.selected.extend(
                        board
                            .shapes
                            .iter()
                            .filter(|(_, r)| intersects(bounds, drawn_rect(board, &r.shape)))
                            .map(|(id, _)| id.clone()),
                    );
                }
            }
        }
        Task::none()
    }
    /// The run a dragged endpoint makes: the carried sample follows the
    /// pointer, the rest keep their places, and the end in hand holds no card
    /// — it takes one again only where it is let go, which `held` names.
    fn routed(
        &self,
        id: &str,
        end: usize,
        point: [f32; 2],
        shape: &Shape,
        held: Option<String>,
    ) -> Option<Change> {
        let mut run = path_points(shape);
        let last = run.len().checked_sub(1)?;
        let sample = run.get_mut(end)?;
        *sample = point;
        let next = self.path_shape(shape.kind, &run);
        let carried = |mine: bool, standing: &Option<String>| {
            if mine { held.clone() } else { standing.clone() }
        };
        let from = carried(end == 0, &shape.from);
        let to = carried(end == last, &shape.to);
        // both ends on one card is a loop the board cannot draw, so the end in
        // hand stands on its own point rather than stealing the other's card
        let looped = from.is_some() && from == to;
        Some(Change::Route {
            id: id.to_owned(),
            x: next.x,
            y: next.y,
            width: next.width,
            height: next.height,
            points: next.points,
            from: if looped && end == 0 { None } else { from },
            to: if looped && end != 0 { None } else { to },
        })
    }
    pub(super) fn gesture_changes(&self) -> Vec<Change> {
        match &self.gesture {
            Gesture::Endpoint {
                id,
                end,
                point,
                shape,
            } => self
                .routed(id, *end, *point, shape, None)
                .into_iter()
                .collect(),
            Gesture::Scale {
                corner,
                start,
                point,
                bounds,
                shapes,
            } => self.scaled(*corner, *start, *point, *bounds, shapes),
            Gesture::Nudge { shapes, offset } => shapes
                .iter()
                .map(|(id, s)| Change::Move {
                    id: id.clone(),
                    x: coordinate((s.x + offset[0]) as f32),
                    y: coordinate((s.y + offset[1]) as f32),
                })
                .collect(),
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
                // A card is never smaller than a card; a run may be perfectly
                // flat, and clamping a horizontal line to a card's minimum
                // would jump it thirty-two units the moment a handle moved.
                let least = if shape.kind.is_path() {
                    [0., 0.]
                } else {
                    [40., 32.]
                };
                let mut width = (shape.width as f32 + delta[0] * corner[0] as f32)
                    .clamp(least[0], boards::MAX_SIZE as f32);
                let mut height = (shape.height as f32 + delta[1] * corner[1] as f32)
                    .clamp(least[1], boards::MAX_SIZE as f32);
                if self.modifiers.shift && shape.width > 0 && shape.height > 0 {
                    let ratio = shape.width as f32 / shape.height as f32;
                    width = width.max(height * ratio).min(boards::MAX_SIZE as f32);
                    height = (width / ratio).clamp(least[1], boards::MAX_SIZE as f32);
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
        let mut b = points_rect(start, point);
        let dragged = (point[0] - start[0]).hypot(point[1] - start[1]) * self.zoom >= 4.;
        // Shift draws a square box — a circle from the ellipse, a regular
        // diamond — by growing the short side to the long one, away from the
        // corner the drag started at, so the anchor under the press stays put.
        if dragged && self.modifiers.shift {
            let side = (b[2] - b[0]).max(b[3] - b[1]);
            let grow = |low: f32, high: f32, anchor: f32| {
                if anchor <= low {
                    [low, low + side]
                } else {
                    [high - side, high]
                }
            };
            [b[0], b[2]] = grow(b[0], b[2], start[0]);
            [b[1], b[3]] = grow(b[1], b[3], start[1]);
        }
        let (w, h) = match kind {
            Kind::Note => (220., 180.),
            Kind::Rectangle => (240., 140.),
            Kind::Ellipse => (200., 200.),
            Kind::Diamond => (200., 160.),
            Kind::Text => (280., 96.),
            Kind::Arrow | Kind::Line | Kind::Draw => (200., 140.),
        };
        // A click has no box to take, so the default one lands centred on the
        // pointer: a shape that appeared below and right of where you clicked
        // would read as having missed.
        let size = if dragged {
            [
                (b[2] - b[0]).round().clamp(40., 4000.),
                (b[3] - b[1]).round().clamp(32., 4000.),
            ]
        } else {
            [w, h]
        };
        let corner = if dragged {
            [b[0], b[1]]
        } else {
            [start[0] - size[0] / 2., start[1] - size[1] / 2.]
        };
        Shape {
            kind,
            color: self.palette,
            x: coordinate(corner[0]),
            y: coordinate(corner[1]),
            width: size[0] as i32,
            height: size[1] as i32,
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
    /// Where an endpoint is let go decides what it holds: dropped on a card an
    /// arrow takes it, dropped on the board it stands on its own point. A line
    /// and a stroke never bind, so they only ever move their sample.
    fn on_routed(&mut self, id: &str, end: usize, point: [f32; 2], shape: &Shape) -> Task<Message> {
        let Some(board) = self.visible() else {
            return Task::none();
        };
        let binds = shape.kind == Kind::Arrow;
        let held = binds
            .then(|| self.topmost(&board, point, |s: &Shape| !s.kind.is_path()))
            .flatten();
        let Some(change) = self.routed(id, end, point, shape, held) else {
            return Task::none();
        };
        self.edit(change)
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
            Gesture::Erase { swept, .. } => self.on_swept(swept),
            Gesture::Endpoint {
                id,
                end,
                point,
                shape,
            } => self.on_routed(&id, end, point, &shape),
            Gesture::Idle
            | Gesture::Pan { .. }
            | Gesture::Marquee { .. }
            | Gesture::Move { .. }
            | Gesture::Scale { .. }
            | Gesture::Nudge { .. }
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
        // A card the board will not take must still be one you can leave.
        // Done keeps your words and asks you to shorten them; Escape is the
        // other answer to that — put the card back the way it was and let go.
        let overlong = self
            .inline
            .as_ref()
            .is_some_and(|inline| inline.document.text().len() > boards::MAX_TEXT);
        if overlong {
            self.inline = None;
            self.error.clear();
            return self.hand_back_focus();
        }
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
            grown: None,
        });
        Task::none()
    }
    /// What the host says the card's words come to, laid out at the size and
    /// width the painter writes them in. It arrives in screen pixels because
    /// that is what was measured; the board is in board units, so the zoom
    /// comes back out of it here.
    pub(super) fn on_measured(&mut self, _width: f32, height: f32) -> Task<Message> {
        let Some(inline) = &mut self.inline else {
            return Task::none();
        };
        let needed = height / self.zoom;
        inline.grown = Some(inline.grown.map_or(needed, |seen| seen.max(needed)));
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
            // Say by how much, and say the way out. Refusing to save without
            // either is a card you cannot leave: every way out of the editor
            // ends here, so a reader who does not know Escape discards is
            // stuck deleting characters against a limit nobody named.
            self.error = format!(
                "This card holds {} bytes and there are {}. Shorten it, or press Escape to \
                 leave the card as it was.",
                boards::MAX_TEXT,
                text.len()
            );
            self.inline = Some(inline);
            return Task::none();
        }
        // The card was drawn at the height its words need for as long as the
        // editor was open. Saving keeps that height: a card that snapped back
        // to clipping the moment you clicked away would have been lying the
        // whole time you were typing. `self.inline` is already taken, so this
        // reads the board as it will be without the editor over it.
        let grow = self
            .visible()
            .and_then(|board| self.grown_change(&board, &inline));
        let changed = text != inline.original;
        if (changed || grow.is_some()) && self.pending.len() >= 64 {
            self.error =
                "Waiting for earlier edits to save. Retry saving before closing this card.".into();
            self.inline = Some(inline);
            return Task::none();
        }
        // Words are the whole of a text shape. One left with none is an empty
        // hit box — invisible, still in the way of a click and still caught by
        // a rubber band — so abandoning a text shape empty leaves nothing
        // behind, the way it does on any canvas. A sticky with no words is
        // still a sticky, and stays.
        let vanished = self.kind_of(&inline.id) == Some(Kind::Text) && text.trim().is_empty();
        if vanished {
            self.selected.remove(&inline.id);
            return Task::batch([
                self.edit(Change::Delete { id: inline.id }),
                self.hand_back_focus(),
            ]);
        }
        let mut changes = Vec::new();
        if changed {
            changes.push(Change::Text {
                id: inline.id,
                text,
            });
        }
        changes.extend(grow);
        Task::batch([self.edit_many(changes), self.hand_back_focus()])
    }
    /// The canvas takes the keyboard back, so the next key is a shortcut
    /// rather than a character nothing is listening for.
    fn hand_back_focus(&self) -> Task<Message> {
        ducktape_view_guest::widget::perform(wire::WidgetCommand::Focus {
            target: "boards/canvas-layout".into(),
        })
    }
    fn kind_of(&self, id: &str) -> Option<Kind> {
        Some(self.visible()?.shapes.get(id)?.shape.kind)
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
        if let Gesture::Nudge { offset, .. } = &mut self.gesture {
            offset[0] += x;
            offset[1] += y;
            return Task::none();
        }
        let Some(board) = self.visible() else {
            return Task::none();
        };
        let shapes: BTreeMap<String, Shape> = self
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
        if shapes.is_empty() {
            return Task::none();
        }
        self.gesture = Gesture::Nudge {
            shapes,
            offset: [x, y],
        };
        Task::none()
    }
    /// What the keyboard has in hand, handed to the board. A gesture the
    /// keyboard drives has no button coming up to end it, so it is ended by
    /// whatever starts the next one — the key going up, another key, or the
    /// pointer.
    pub(super) fn settle(&mut self) -> Task<Message> {
        if !matches!(self.gesture, Gesture::Nudge { .. }) {
            return Task::none();
        }
        let changes = self.gesture_changes();
        self.gesture = Gesture::Idle;
        self.edit_many(changes)
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
        // Snap against the board on screen, not the last one consensus agreed
        // on: a card you drew a second ago is on screen and is exactly what you
        // want to line the next one up with.
        let Some(board) = self.visible() else {
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
/// A card's smallest size, which the module enforces, and a run's, which it
/// does not — a straight horizontal line is legitimately zero high.
pub(super) fn least(s: &Shape) -> [f32; 2] {
    if s.kind.is_path() {
        [0., 0.]
    } else {
        [40., 32.]
    }
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
    // Each outline is a different curve, and an arrow that stops at a box
    // around a circle stops visibly short of it. The same ray, scaled to
    // where it leaves the outline the card is actually drawn with.
    let half = [
        (s.width as f32 / 2.).max(0.001),
        (s.height as f32 / 2.).max(0.001),
    ];
    let unit = [d[0] / half[0], d[1] / half[1]];
    let scale = match s.kind {
        Kind::Ellipse => 1. / unit[0].hypot(unit[1]).max(0.001),
        Kind::Diamond => 1. / (unit[0].abs() + unit[1].abs()).max(0.001),
        Kind::Note | Kind::Rectangle | Kind::Text => {
            (1. / unit[0].abs().max(0.001)).min(1. / unit[1].abs().max(0.001))
        }
        Kind::Arrow | Kind::Line | Kind::Draw => 1.,
    };
    let scale = scale.min(1.);
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
/// The box a shape actually occupies on the board. A card's is its own; a
/// connector's is the span of the stroke it draws, which for a bound end is
/// wherever its card put it rather than where its samples say.
pub(super) fn drawn_rect(board: &Board, s: &Shape) -> [f32; 4] {
    if !s.kind.is_path() {
        return rect(s);
    }
    let run = stroke(board, s);
    let Some(first) = run.first() else {
        return rect(s);
    };
    run.iter()
        .fold([first[0], first[1], first[0], first[1]], |a, p| {
            [
                a[0].min(p[0]),
                a[1].min(p[1]),
                a[2].max(p[0]),
                a[3].max(p[1]),
            ]
        })
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
    handle_point([s.x as f32, s.y as f32, s.width as f32, s.height as f32], c)
}
/// Where a handle sits on a box given as origin and size. A sign of zero on an
/// axis is the middle of it — the edge handle that changes the other axis and
/// leaves this one exactly where it was.
pub(super) fn handle_point(box_: [f32; 4], c: [i32; 2]) -> [f32; 2] {
    let along = |start: f32, size: f32, sign: i32| match sign {
        s if s > 0 => start + size,
        0 => start + size / 2.,
        _ => start,
    };
    [along(box_[0], box_[2], c[0]), along(box_[1], box_[3], c[1])]
}
/// The eight places a box can be taken by: its four corners, which change both
/// axes at once, and the middle of its four edges, which change one. A canvas
/// app offers both, because "make this wider" and "make this bigger" are
/// different edits and a corner can only express the second.
pub(super) const HANDLES: [[i32; 2]; 8] = [
    [-1, -1],
    [0, -1],
    [1, -1],
    [1, 0],
    [1, 1],
    [0, 1],
    [-1, 1],
    [-1, 0],
];
fn line_distance(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let d = [b[0] - a[0], b[1] - a[1]];
    let t = (((p[0] - a[0]) * d[0] + (p[1] - a[1]) * d[1])
        / (d[0] * d[0] + d[1] * d[1]).max(0.001))
    .clamp(0., 1.);
    (p[0] - a[0] - t * d[0]).hypot(p[1] - a[1] - t * d[1])
}
