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
            // Only what the box will move is inside the box. A connector held
            // at BOTH ends is carried by the cards it names, so it is not a
            // member — and its stored rectangle is left where it was drawn,
            // which the run itself long since left. Counting it stretched the
            // box up to a corner nothing stood in.
            if !draggable(s) {
                continue;
            }
            // What a half-held connector brings to the box is the run on the
            // board, not the rectangle it was stored with: one end is wherever
            // its card is now.
            let box_ = drawn_rect(board, s);
            bounds = [
                bounds[0].min(box_[0]),
                bounds[1].min(box_[1]),
                bounds[2].max(box_[2]),
                bounds[3].max(box_[3]),
            ];
            shapes.insert(id.clone(), s.clone());
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
            (bounds[2] + delta[0] * corner[0] as f32).clamp(least[0], boards_wire::MAX_SIZE as f32),
            (bounds[3] + delta[1] * corner[1] as f32).clamp(least[1], boards_wire::MAX_SIZE as f32),
        ];
        if self.modifiers.shift && bounds[2] > 0. && bounds[3] > 0. {
            let ratio = bounds[2] / bounds[3];
            size[0] = size[0]
                .max(size[1] * ratio)
                .min(boards_wire::MAX_SIZE as f32);
            size[1] = (size[0] / ratio).clamp(least[1], boards_wire::MAX_SIZE as f32);
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
        // The board menu has a name field in it, and a board is named with the
        // same letters the tools answer to: every character of "Grid" picked a
        // tool up as it went past, and the board you made landed on a canvas
        // holding the ellipse. While the menu is up the canvas is not
        // listening — Escape closes it and nothing else reaches past it.
        if self.picking_a_board() {
            let dismiss = key == "Escape";
            return if dismiss {
                self.on_cancel()
            } else {
                Task::none()
            };
        }
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
            // The unshifted key moves one place and the shifted one goes all
            // the way, which is what every editor with four of these does.
            // The shifted bracket is named as the brace it types, not as a
            // bracket with a modifier: a host folds Shift into a symbol and
            // hands the modifier back cleared, the same reason "?" is bound
            // above rather than shift and "/".
            (true, "}") if !repeat => self.on_stack(Stacking::Front),
            (true, "]") if !repeat => self.on_stack(Stacking::Forward),
            (true, "{") if !repeat => self.on_stack(Stacking::Back),
            (true, "[") if !repeat => self.on_stack(Stacking::Backward),
            (true, "d") if !repeat => self.on_duplicate(),
            (true, "g") if shift && !repeat => self.on_group(false),
            (true, "g") if !repeat => self.on_group(true),
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
            // tldraw and excalidraw both answer to O for the oval, and the
            // toolbar prints that one. C is what a hand reaches for anyway —
            // it is the word, and nothing else on the board wants the key.
            (false, "o" | "c" | "5") if !repeat => self.on_tool(Tool::Ellipse),
            (false, "d" | "6") if !repeat => self.on_tool(Tool::Diamond),
            (false, "a" | "7") if !repeat => self.on_tool(Tool::Arrow),
            (false, "l" | "8") if !repeat => self.on_tool(Tool::Line),
            (false, "p" | "9") if !repeat => self.on_tool(Tool::Draw),
            (false, "t") if !repeat => self.on_tool(Tool::Text),
            (false, "e") if !repeat => self.on_tool(Tool::Eraser),
            (false, "q") if !repeat => self.on_lock_tool(),
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
        self.stop_guiding();
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
    /// Whether the board menu is the thing on screen: either you opened it, or
    /// there is no board to draw on and it is all there is. The painter asks so
    /// it knows to draw the menu open; the keyboard asks so it knows the canvas
    /// is not the one being typed at.
    pub(super) fn picking_a_board(&self) -> bool {
        self.board_picker || self.confirmed.is_none()
    }
    pub(super) fn on_board_picker(&mut self) -> Task<Message> {
        // Leave the card before the menu opens over it. The menu carries text
        // boxes of its own and a card's editor holds the keyboard while it is
        // open, so every letter typed into the menu's name box went into the
        // CARD — over the whole of what it said, because a card opens with its
        // words selected — while the rows that would have got you out sat dark.
        // `on_open` already takes this door one press later; this is the same
        // door, at the moment the menu is asked for.
        let leaving = self.finish_text();
        if self.inline.is_some() {
            // The card refused to be left — too long, or too many edits still
            // queued. `finish_text` has said which; the menu does not open over
            // an answer nobody would then be able to read.
            return leaving;
        }
        self.board_picker = !self.board_picker;
        if self.board_picker {
            self.name_the_board_we_are_on();
            return leaving;
        }
        Task::batch([leaving, self.take_the_keyboard()])
    }
    /// The rename box names the board the view is on, and nothing else.
    ///
    /// Called wherever the view ARRIVES at a board rather than only where the
    /// menu opens: seeding it at the menu alone left the box holding the last
    /// board's name when a board was made from the menu, which stays open —
    /// the chip said one name, the box under it said another, and Rename was
    /// live and would have taken the wrong one. It also drops a half-typed name
    /// you walked away from, which must never come back as an offer.
    pub(super) fn name_the_board_we_are_on(&mut self) {
        self.rename = self
            .confirmed
            .as_ref()
            .map(|board| board.title.clone())
            .unwrap_or_default();
    }
    pub(super) fn on_snap(&mut self) -> Task<Message> {
        self.snap = !self.snap;
        self.stop_guiding();
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
                let run = stroke(board, s);
                let on_the_run = run
                    .windows(2)
                    .any(|step| line_distance(point, step[0], step[1]) <= reach);
                // The words on a connector are part of it. A label you can
                // read but not press would be the one piece of a drawing you
                // cannot take hold of — and it is the piece a pointer goes
                // for, being the only part of an arrow bigger than a line.
                on_the_run || labelled(s, &run, point)
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
    /// The box a shape's words are written in, in board units: a card's own
    /// outline, or the plate a connector's words ride at the middle of its run.
    /// A press inside it is a press in the text you are writing rather than a
    /// press on the board — which is the whole difference between a
    /// double-click that opens a card and one that opens it and shuts it again.
    fn writing_area(&self, board: &Board, s: &Shape) -> [f32; 4] {
        if !s.kind.is_path() {
            return rect(s);
        }
        plate(&stroke(board, s))
    }
    /// What a press on the card being written on means, or `None` when the
    /// press landed somewhere else on the board.
    ///
    /// The native field is only as big as the words in it — that is what puts
    /// them where the alignment says — so most of a card being written on is
    /// board, not field. A press there used to do nothing whatsoever: no
    /// caret, no focus, and on a big sticky holding two words that is almost
    /// the whole card. It is a press on the writing, so it goes back to the
    /// writing, at the end of it, which is also the one gesture that gets the
    /// focus back when something else on screen has taken it.
    fn writing_under(&self, at: [f32; 2]) -> Option<Task<Message>> {
        let inline = self.inline.as_ref()?;
        let board = self.visible()?;
        let record = board.shapes.get(&inline.id)?;
        let on_the_card = contains(self.writing_area(&board, &record.shape), self.world(at));
        if !on_the_card {
            return None;
        }
        let (pos, room) = self.typing_box(&board, &record.shape);
        let on_the_words = contains([pos[0], pos[1], pos[0] + room[0], pos[1] + room[1]], at);
        if on_the_words {
            // The field has it: it puts the caret under the pointer itself,
            // which is more than any message from here could say.
            return Some(Task::none());
        }
        let target = format!("boards/editor/{}", inline.id);
        Some(Task::batch([
            self.focus_text(),
            ducktape_view_guest::widget::perform(wire::WidgetCommand::CursorEnd { target }),
        ]))
    }
    pub(super) fn on_press(&mut self, x: f32, y: f32) -> Task<Message> {
        self.cursor = [x, y];
        if let Some(written_on) = self.writing_under([x, y]) {
            return written_on;
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
                // And bent in the middle. The handle is on the line whether
                // the connector has a bend yet or not: taking one that is not
                // there puts it there, which is how a straight arrow becomes a
                // curved one without a separate verb for it.
                if let Some((bent, end, target)) = self.bend(shape, &run)
                    && (point[0] - target[0]).hypot(point[1] - target[1]) <= 9. / self.zoom
                {
                    self.gesture = Gesture::Endpoint {
                        id,
                        end,
                        point,
                        shape: bent,
                    };
                    return Task::none();
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
        // Shift on its own adds a shape to the selection and takes it back
        // out. Shift as HALF of the duplicate chord is not a selection verb at
        // all: that press has to become a drag like any other, or ⌘⇧ has
        // nothing to plant and the board prints a gesture that quietly
        // deselects instead.
        let extending = self.modifiers.shift && !self.planting();
        if extending {
            // A group goes in and out of a selection whole, the same way it is
            // picked whole: shift-clicking one member of a group you already
            // hold puts the WHOLE group down, not a hole in the middle of it.
            let mates = with_group_mates(&board, [id.clone()]);
            match self.selected.contains(&id) {
                true => self.selected.retain(|held| !mates.contains(held)),
                false => self.selected.extend(mates),
            }
            self.gesture = Gesture::Idle;
            return Task::none();
        }
        if !self.selected.contains(&id) {
            self.selected = with_group_mates(&board, [id.clone()]);
        }
        // Picking a shape takes up the pen it was drawn with — all of it. An
        // eyedropper that took the colour and left the rest made "match that
        // one" work for one third of how a shape looks and quietly not for the
        // other two, and made the panel describe a shape nobody had selected.
        self.pen = Pen::of(&board.shapes[&id].shape);
        let shapes = self
            .selected
            .iter()
            .filter_map(|id| {
                board
                    .shapes
                    .get(id)
                    .filter(|r| draggable(&r.shape))
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
            // It goes through the same creation the tools use, so the words
            // start where the pointer is and in the colour the palette is
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
    /// How far the board should slide this tick, or nothing at all. A gesture
    /// with a point in hand, held near an edge of the stage, carries the board
    /// under it — which is how a card goes further than one screen without
    /// being put down, panned to, and picked up again.
    ///
    /// It is asked every tick rather than remembered, so letting go, moving out
    /// of the margin and dropping the gesture all stop it by the same answer.
    pub(super) fn drift(&self) -> Option<[f32; 2]> {
        let carries_a_point = match &self.gesture {
            Gesture::Move { .. }
            | Gesture::Resize { .. }
            | Gesture::Scale { .. }
            | Gesture::Create { .. }
            | Gesture::Marquee { .. }
            | Gesture::Sketch { .. }
            | Gesture::Erase { .. }
            | Gesture::Endpoint { .. } => true,
            // The hand tool is already moving the camera and a drift would move
            // it twice; a nudge is a held key with no pointer in it; and idle
            // is the board sitting still, which is most of the time.
            Gesture::Idle | Gesture::Pan { .. } | Gesture::Nudge { .. } => false,
        };
        if !carries_a_point {
            return None;
        }
        let step = [
            edge_step(self.cursor[0], self.viewport[0]),
            edge_step(self.cursor[1], self.viewport[1]),
        ];
        let clear_of_both_margins = step[0].abs() + step[1].abs() < f32::EPSILON;
        match clear_of_both_margins {
            true => None,
            false => Some(step),
        }
    }
    /// A tick of the drift: move the camera, then re-apply the gesture at the
    /// same SCREEN point. The shape in hand stays under the cursor and the
    /// world moves beneath it, which is what makes it read as carrying the card
    /// off the edge rather than as the card sliding away from you. Re-applying
    /// through `on_move` is also what keeps the guides, the snapping and the
    /// eraser's sweep true while the board is moving.
    pub(super) fn on_drift(&mut self) -> Task<Message> {
        let Some(step) = self.drift() else {
            return Task::none();
        };
        self.camera = [self.camera[0] + step[0], self.camera[1] + step[1]];
        self.on_move(self.cursor[0], self.cursor[1])
    }
    pub(super) fn on_move(&mut self, x: f32, y: f32) -> Task<Message> {
        self.cursor = [x, y];
        let mut point = self.world([x, y]);
        // The lines this move is held to are decided once, at the end, so the
        // guides drawn and the lines held can never say different things.
        let mut lined_up: Option<Snapped> = None;
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
                let snapped = self.snap_delta(shapes, [point[0] - start[0], point[1] - start[1]]);
                point = [start[0] + snapped.delta[0], start[1] + snapped.delta[1]];
                lined_up = Some(snapped);
            }
        }
        // A corner being drawn or dragged lines up with the board the way a
        // moving card does. tldraw and excalidraw show the same guides while
        // you create and resize as they do while you move, and without them a
        // shape had to be drawn roughly and then nudged into place afterwards.
        let lining_up = self.snap && !self.modifiers.control && !self.modifiers.shift;
        let nudged = match (&self.gesture, lining_up) {
            // A stroke, a line and an arrow place their ends by what they
            // reach for — a card takes the end whole, ringed while you hold it
            // — so a corner guide has nothing to say about them.
            (Gesture::Create { kind, .. }, true) => {
                (!kind.is_path()).then(|| self.corner_nudge(point, [true; 2], &|_| false))
            }
            (
                Gesture::Resize {
                    id,
                    corner,
                    start,
                    shape,
                    ..
                },
                true,
            ) => Some(self.corner_nudge(
                carried(corner_point(shape, *corner), *start, point),
                travelling(*corner),
                &|other| other == id,
            )),
            (
                Gesture::Scale {
                    corner,
                    start,
                    bounds,
                    shapes,
                    ..
                },
                true,
            ) => Some(self.corner_nudge(
                carried(handle_point(*bounds, *corner), *start, point),
                travelling(*corner),
                &|other| shapes.contains_key(other),
            )),
            _ => None,
        };
        if let Some(snapped) = nudged {
            point = [point[0] + snapped.delta[0], point[1] + snapped.delta[1]];
            lined_up = Some(snapped);
        }
        let (guides, held) = match lined_up {
            Some(snapped) => (snapped.guides, snapped.held),
            None => (Vec::new(), [None, None]),
        };
        self.guides = guides;
        self.held = held;
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
                    let swept = board
                        .shapes
                        .iter()
                        .filter(|(_, r)| intersects(bounds, drawn_rect(board, &r.shape)))
                        .map(|(id, _)| id.clone());
                    // Touching any member takes the group, so a band drawn
                    // across half a diagram does not tear a group in two.
                    self.selected.extend(with_group_mates(board, swept));
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
        board: &Board,
        id: &str,
        end: usize,
        point: [f32; 2],
        shape: &Shape,
        held: Option<boards_wire::Bond>,
    ) -> Option<Change> {
        let mut run = path_points(shape);
        let last = run.len().checked_sub(1)?;
        let sample = run.get_mut(end)?;
        *sample = point;
        let run = self.straightened(run);
        let carried = |mine: bool, kept: &Option<boards_wire::Bond>| {
            if mine { held.clone() } else { kept.clone() }
        };
        let from = carried(end == 0, &shape.from);
        let to = carried(end == last, &shape.to);
        // both ends on one card is a loop the board cannot draw, so the end in
        // hand stands on its own point rather than stealing the other's card
        let looped = boards_wire::held(&from).is_some()
            && boards_wire::held(&from) == boards_wire::held(&to);
        let from = if looped && end == 0 { None } else { from };
        let to = if looped && end != 0 { None } else { to };
        let next = self.path_shape(shape.kind, &standing(board, &run, &from, &to));
        Some(Change::Route {
            id: id.to_owned(),
            x: next.x,
            y: next.y,
            width: next.width,
            height: next.height,
            points: next.points,
            from,
            to,
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
                .endpoint_change(id, *end, *point, shape)
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
                    .clamp(least[0], boards_wire::MAX_SIZE as f32);
                let mut height = (shape.height as f32 + delta[1] * corner[1] as f32)
                    .clamp(least[1], boards_wire::MAX_SIZE as f32);
                if self.modifiers.shift && shape.width > 0 && shape.height > 0 {
                    let ratio = shape.width as f32 / shape.height as f32;
                    width = width.max(height * ratio).min(boards_wire::MAX_SIZE as f32);
                    height = (width / ratio).clamp(least[1], boards_wire::MAX_SIZE as f32);
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
            // One line and the room it is written in. Words are the whole of a
            // text shape, so a box made taller than its words is dead space
            // that still answers a click; it grows under the caret as you type
            // and stops where you stop.
            Kind::Text => (280., 60.),
            Kind::Arrow | Kind::Line | Kind::Draw => (200., 140.),
        };
        let size = if dragged {
            [
                (b[2] - b[0]).round().clamp(40., 4000.),
                (b[3] - b[1]).round().clamp(32., 4000.),
            ]
        } else {
            [w, h]
        };
        // Where a click with no box to take leaves the shape. A card lands
        // centred on the pointer: one that appeared below and right of where
        // you clicked would read as having missed. Words are the other way
        // round — you click where the sentence should START, the way a text
        // cursor works everywhere else — so a text shape's corner is the
        // pointer and it runs away from it.
        let writing = kind == Kind::Text;
        let corner = match (dragged, writing) {
            (true, _) => [b[0], b[1]],
            (false, true) => start,
            (false, false) => [start[0] - size[0] / 2., start[1] - size[1] / 2.],
        };
        Shape {
            x: coordinate(corner[0]),
            y: coordinate(corner[1]),
            width: size[0] as i32,
            height: size[1] as i32,
            ..self.pen.shape(kind)
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
        let limit = boards_wire::MAX_SIZE as f32;
        let fit = (limit / (b[2] - b[0]).max(limit)).min(limit / (b[3] - b[1]).max(limit));
        let size = |span: f32| (span * fit).round().clamp(0., limit);
        Shape {
            x: coordinate(b[0]),
            y: coordinate(b[1]),
            width: size(b[2] - b[0]) as i32,
            height: size(b[3] - b[1]) as i32,
            points: points
                .iter()
                .map(|p| [size(p[0] - b[0]) as i32, size(p[1] - b[1]) as i32])
                .collect(),
            ..self.pen.shape(kind)
        }
    }
    /// An arrow drawn onto a card binds to it, so the connection survives the
    /// card moving. Both ends on one card is a free arrow, not a loop.
    fn bind(&self, shape: &mut Shape, start: [f32; 2], end: [f32; 2]) {
        let Some(board) = self.settled() else {
            return;
        };
        let card = |p| self.holding(&board, shape.kind, p);
        shape.from = card(start);
        shape.to = card(end);
        let one_card = boards_wire::held(&shape.from).is_some()
            && boards_wire::held(&shape.from) == boards_wire::held(&shape.to);
        if one_card {
            shape.from = None;
            shape.to = None;
        }
        let run = standing(&board, &path_points(shape), &shape.from, &shape.to);
        *shape = self.reboxed(shape, &run);
    }
    /// A bend put back on the line between its neighbours is not a bend. The
    /// run straightens rather than keep a sample nobody can see and nobody
    /// asked for — the same door out of a curve that the drag in was.
    ///
    /// Only a three-sample run, which is the one bend a connector has. A pen
    /// stroke's straight stretch is the drawing.
    fn straightened(&self, run: Vec<[f32; 2]>) -> Vec<[f32; 2]> {
        let [first, middle, last] = run[..] else {
            return run;
        };
        let flat = line_distance(middle, first, last) * self.zoom <= 6.;
        match flat {
            true => vec![first, last],
            false => run,
        }
    }
    /// A connector's bend: the shape it would have with one, which sample that
    /// is, and where the handle sits on the run as drawn. A connector without a
    /// bend gets one at the middle of its line, so the handle is in the same
    /// place whether or not it has been used yet — you drag it and the arrow
    /// curves, the way it does everywhere else.
    ///
    /// `None` for anything with a run of its own to keep: a pen stroke is all
    /// samples, and a bend handle in the middle of one would take hold of a
    /// piece of the drawing rather than reshape it.
    pub(super) fn bend(&self, s: &Shape, drawn: &[[f32; 2]]) -> Option<(Shape, usize, [f32; 2])> {
        if s.kind == Kind::Draw {
            return None;
        }
        let stored = path_points(s);
        if stored.len() != drawn.len() || stored.len() > 3 {
            return None;
        }
        if stored.len() == 3 {
            return Some((s.clone(), 1, drawn[1]));
        }
        let [first, last] = [*drawn.first()?, *drawn.last()?];
        // Words take the middle of a run, so the handle steps aside when there
        // are any: two things to take hold of in the same place is one of them
        // unreachable, and the words are the one you meant.
        let along = match s.text.is_empty() {
            true => 0.5,
            false => 0.25,
        };
        let middle = [
            first[0] + (last[0] - first[0]) * along,
            first[1] + (last[1] - first[1]) * along,
        ];
        // On a short connector even the quarter point is under the plate, and
        // there is no room for both: the words keep the line and the bend is
        // not offered at all rather than offered where it cannot be taken.
        if !s.text.is_empty() && contains(plate(drawn), middle) {
            return None;
        }
        // The run it gets is the one on the board, ends included. A bound end
        // STANDS at its card's centre and is DRAWN at its card's border, and a
        // bend measured against the centres would be measured against a line
        // nobody can see — it would never read as straight again once put
        // back. The ends are put back where they stand by [`standing`] when
        // the drag commits.
        Some((self.reboxed(s, &[first, middle, last]), 1, middle))
    }
    /// A connector given a different run, keeping everything about it that is
    /// not its shape. The box and the samples are one fact stated twice — the
    /// samples are stored against the box — so they are always replaced
    /// together.
    fn reboxed(&self, s: &Shape, run: &[[f32; 2]]) -> Shape {
        let boxed = self.path_shape(s.kind, run);
        Shape {
            x: boxed.x,
            y: boxed.y,
            width: boxed.width,
            height: boxed.height,
            points: boxed.points,
            ..s.clone()
        }
    }
    /// The card an endpoint over this point would take: a card for an arrow,
    /// nothing for a line or a stroke, which never bind.
    pub(super) fn holding(
        &self,
        board: &Board,
        kind: Kind,
        point: [f32; 2],
    ) -> Option<boards_wire::Bond> {
        if kind != Kind::Arrow {
            return None;
        }
        let id = self.topmost(board, point, |s: &Shape| !s.kind.is_path())?;
        let card = &board.shapes.get(&id)?.shape;
        Some(bond_at(&id, card, point))
    }
    /// What an endpoint in hand is doing to its connector right now. The drag
    /// and the release read it the same way, so the arrow snaps to the card's
    /// edge while you are still holding it rather than jumping there when you
    /// let go — what you are looking at IS what you are about to commit.
    pub(super) fn endpoint_change(
        &self,
        id: &str,
        end: usize,
        point: [f32; 2],
        shape: &Shape,
    ) -> Option<Change> {
        let board = self.settled()?;
        let held = reaches_for_a_card(shape, end)
            .then(|| self.holding(&board, shape.kind, point))
            .flatten();
        self.routed(&board, id, end, point, shape, held)
    }
    /// Where an endpoint is let go decides what it holds: dropped on a card an
    /// arrow takes it, dropped on the board it stands on its own point. A line
    /// and a stroke never bind, so they only ever move their sample.
    fn on_routed(&mut self, id: &str, end: usize, point: [f32; 2], shape: &Shape) -> Task<Message> {
        let Some(change) = self.endpoint_change(id, end, point, shape) else {
            return Task::none();
        };
        self.edit(change)
    }
    /// Whether the drag being let go of is planting a copy rather than moving
    /// the originals.
    ///
    /// Alt is the canvas-app answer and the one the board prints. ⌘⇧ is the
    /// one hands reach for, and it has to work too: a desktop that keeps
    /// Alt-drag for moving its own windows eats the gesture before the board
    /// ever sees it, and the writer is left with a chord that does nothing.
    /// Shift is otherwise a straight run, which is exactly what you want a
    /// planted copy to travel in anyway.
    fn planting(&self) -> bool {
        let reached_for = ducktape_view_guest::keyboard::command(self.modifiers);
        self.modifiers.alt || (reached_for && self.modifiers.shift)
    }
    pub(super) fn on_release(&mut self) -> Task<Message> {
        let changes = self.gesture_changes();
        let planting = self.planting();
        let gesture = std::mem::take(&mut self.gesture);
        self.stop_guiding();
        match gesture {
            // A held modifier turns a drag into a duplicate: the shapes it
            // carried are planted where the pointer let them go and the
            // originals never moved, which is the same picture as dragging a
            // copy off them.
            Gesture::Move { start, point, .. } if planting => {
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
        // `bind` asks the same question the drag was answering all along, and
        // answers nothing for a line or a stroke.
        self.bind(&mut shape, start, point);
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
        // A menu standing over the board is the first thing Escape answers,
        // and the only one: shutting a menu is not also dropping what it was
        // about to act on.
        if self.menu.is_some() {
            return self.on_close_menu();
        }
        // Escape stops the writing. It does not throw it away: EVERY door out
        // of a card keeps what you wrote, which is the answer tldraw and
        // excalidraw both give and the only safe one here. What you type lives
        // in the editor until a door commits it, so a door that discarded
        // would destroy a paragraph with no undo standing behind it — and
        // Escape is the key you press to mean "stop", not "undo everything".
        //
        // Nothing is lost by opening a card you did not mean to, either: a
        // card whose words came back unchanged is a card nothing is written
        // for, so there was never a change to escape from.
        //
        // The one exception is a card the board will not take at all. Every
        // door ends in the same save and that save refuses, so without a way
        // past it the writer is shut inside an editor that answers nothing —
        // and the banner has been saying so, by how much, for as long as it
        // has been true. A save that fails because earlier edits are still in
        // flight is NOT that: it clears on its own, so it keeps the words and
        // asks for a retry.
        if let Some(inline) = self.inline.as_ref() {
            let refused_outright = inline.document.text().len() > boards_wire::MAX_TEXT;
            if !refused_outright {
                return self.finish_text();
            }
            let inline = self.inline.take().expect("the editor is open");
            self.error.clear();
            // Words are the whole of a text shape, and these are not being
            // kept, so one that had none before goes back to not existing.
            let never_written =
                self.kind_of(&inline.id) == Some(Kind::Text) && inline.original.trim().is_empty();
            if !never_written {
                return self.hand_back_focus();
            }
            self.selected.remove(&inline.id);
            return Task::batch([
                self.edit(Change::Delete { id: inline.id }),
                self.hand_back_focus(),
            ]);
        }
        if matches!(self.gesture, Gesture::Idle) {
            self.selected.clear();
            self.tool = Tool::Select;
        }
        self.gesture = Gesture::Idle;
        self.space_pan = false;
        self.stop_guiding();
        self.help = false;
        self.board_picker = false;
        self.take_the_keyboard()
    }
    pub(super) fn on_wheel(&mut self, x: f32, y: f32, pixels: bool) -> Task<Message> {
        let scale = if pixels { 1. } else { 32. };
        // A wheel turned away from you moves the board down and zooms IN, the
        // way it does in a browser and in every canvas app. The delta is how
        // far the CONTENT moves, so the two agree on one sign.
        //
        // One notch of a mouse wheel is three lines — 96 of these units — and
        // the step is chosen against THAT: about a seventh of a turn each, so
        // a handful of notches walks the range rather than crossing it. A
        // trackpad sends the same units by the pixel and lands, by the same
        // rate, on a smooth glide instead of a jump.
        if self.modifiers.control || self.modifiers.logo {
            return self.zoom_at((y * scale * 0.0015).exp(), self.cursor);
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
    /// The whole board, brought onto the stage.
    pub(super) fn on_fit(&mut self) -> Task<Message> {
        let Some(board) = self.visible() else {
            return Task::none();
        };
        let Some(b) = framed(&board, &|_| true) else {
            // A board with nothing on it has nothing to frame, and home is the
            // only answer "show me everything" has left.
            self.camera = [80., 80.];
            self.zoom = 1.;
            return Task::none();
        };
        self.frame(b)
    }
    /// The selection, brought onto the stage.
    ///
    /// Nothing to frame is NOT a reason to move. The board went home at 100%
    /// when this was asked with an empty selection — and when it was asked of
    /// a connector held at both ends, which stands nowhere of its own — so a
    /// key pressed to look AT something threw away the place you were working
    /// in, and the camera is the one thing here that undo cannot bring back.
    pub(super) fn on_fit_selection(&mut self) -> Task<Message> {
        let Some(board) = self.visible() else {
            return Task::none();
        };
        let Some(b) = framed(&board, &|id| self.selected.contains(id)) else {
            return Task::none();
        };
        self.frame(b)
    }
    /// Put a box of board onto the stage, with a margin around it.
    fn frame(&mut self, b: [f32; 4]) -> Task<Message> {
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
        if self.inline.is_some() {
            return Task::none();
        }
        let text = record.shape.text.clone();
        self.gesture = Gesture::Idle;
        let mut document = Editor::new(text.clone());
        // Opening a written card SELECTS what it says, the way every canvas
        // app does: you go in to replace a label far more often than to add a
        // word to one, and the first letter typed should be the label. The
        // caret sits at the far end of the selection, so Right or a click
        // drops it there and adding to the card is the next keystroke. The
        // position is clamped into the text, so asking for the far end of the
        // last line asks for the end of the words whatever they are.
        document.move_to(wire::EditorCursor {
            position: wire::EditorPosition {
                line: u32::MAX,
                column: u32::MAX,
            },
            selection: (!text.is_empty()).then(wire::EditorPosition::default),
        });
        self.inline = Some(Inline {
            id: id.clone(),
            original: text,
            document,
            grown: None,
            wide: None,
        });
        Task::none()
    }
    /// What the host says the card's words come to, laid out at the size and
    /// width the painter writes them in. It arrives in screen pixels because
    /// that is what was measured; the board is in board units, so the zoom
    /// comes back out of it here — the zoom the gauge was LAID OUT at, which
    /// travels with the answer because the camera can move in between.
    pub(super) fn on_measured(&mut self, zoom: f32, width: f32, height: f32) -> Task<Message> {
        // A card keeps the tallest it has needed while you are in it: the words
        // that wanted the room may come back with the next key, and a card that
        // closed up under the caret would be a card that jumped as you deleted.
        // A text shape IS its words, so it follows them down as well as up.
        let hugging = self
            .inline
            .as_ref()
            .and_then(|inline| self.kind_of(&inline.id))
            == Some(Kind::Text);
        let Some(inline) = &mut self.inline else {
            return Task::none();
        };
        let needed = height / zoom;
        inline.grown = Some(match hugging {
            true => needed,
            false => inline.grown.map_or(needed, |seen| seen.max(needed)),
        });
        inline.wide = Some(width / zoom);
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
        if result.is_ok() {
            return Task::none();
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
        if text.len() > boards_wire::MAX_TEXT {
            // Say by how much, and say the way out. Every door out of the
            // editor ends here and this one refuses, so a reader who is not
            // told that Escape leaves anyway is shut in against a number
            // nobody named.
            self.error = format!(
                "This card holds {} bytes and there are {}. Shorten it, or press Escape to \
                 leave it behind.",
                boards_wire::MAX_TEXT,
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
    /// ⌘Enter out of a sticky: finish it and open the next one.
    ///
    /// The chord has always meant "next note" — pressed on the canvas it mints
    /// one beside the selection — but pressed inside the editor it only ever
    /// finished the writing, so reaching the next note took the same chord
    /// twice with nothing to say so. Writing a column of stickies is the whole
    /// reason the tool exists, and its rhythm is type-chord-type-chord; a
    /// doubled chord whose first press appears to do nothing is not a rhythm
    /// anybody builds muscle memory for.
    ///
    /// Only out of a NOTE. A chord that quietly spawned a sticky when you were
    /// labelling a rectangle would be worse than the friction it replaces.
    pub(super) fn finish_note(&mut self) -> Task<Message> {
        let Some(id) = self.inline.as_ref().map(|inline| inline.id.clone()) else {
            return Task::none();
        };
        let finish = self.finish_text();
        match self.chains_to_the_next_note(&id) {
            false => finish,
            true => Task::batch([finish, self.on_quick_note()]),
        }
    }
    /// Whether the finish just performed should open the next sticky: only out
    /// of a sticky, and only when the finish actually LET GO of the editor.
    /// A card too long to save, or one waiting on earlier edits, puts the
    /// editor back and says why — minting a note over that would throw the
    /// message away along with the place the caret was.
    ///
    /// Read after the finish, not before, because "did it let go" is the
    /// question and only the finish can answer it.
    pub(super) fn chains_to_the_next_note(&self, finished: &str) -> bool {
        self.inline.is_none() && self.kind_of(finished) == Some(Kind::Note)
    }
    /// The canvas takes the keyboard back, so the next key is a shortcut
    /// rather than a character nothing is listening for.
    fn hand_back_focus(&self) -> Task<Message> {
        ducktape_view_guest::widget::perform(wire::WidgetCommand::Focus {
            target: "boards/canvas-layout".into(),
        })
    }
    /// The keyboard belongs to the canvas whenever nothing else on the stage
    /// is being typed at. A board you have just opened, or just made, answers
    /// to the tool keys straight away: until now the only thing that ever
    /// pointed the keyboard at the canvas was leaving a card, so on a board
    /// you had not yet written in, N and R and Delete went nowhere.
    pub(super) fn take_the_keyboard(&self) -> Task<Message> {
        if self.inline.is_some() {
            return Task::none();
        }
        self.hand_back_focus()
    }
    /// The stage has appeared. Take its measure, and take the keyboard with
    /// it — a canvas nobody has clicked in yet is still the thing on screen.
    pub(super) fn on_mounted(&mut self, width: f32, height: f32) -> Task<Message> {
        let sized = self.on_size(width, height);
        Task::batch([sized, self.take_the_keyboard()])
    }
    fn kind_of(&self, id: &str) -> Option<Kind> {
        Some(self.visible()?.shapes.get(id)?.shape.kind)
    }
    pub(super) fn on_color(&mut self, color: u8) -> Task<Message> {
        self.pen.color = color;
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
    /// Whether the selection's bodies are painted, and what the next shape's
    /// will be. Like a colour and unlike an alignment: emptying a box is a
    /// choice about how you are drawing, not about the box in hand — you
    /// reach for it to outline a region and then outline three more.
    pub(super) fn on_fill(&mut self, fill: Fill) -> Task<Message> {
        self.pen.fill = fill;
        self.edit_many(
            self.selected
                .iter()
                .map(|id| Change::Fill {
                    id: id.clone(),
                    fill,
                })
                .collect(),
        )
    }
    /// Whether the selection's outlines are unbroken, and the next shape's.
    pub(super) fn on_dash(&mut self, dash: Dash) -> Task<Message> {
        self.pen.dash = dash;
        self.edit_many(
            self.selected
                .iter()
                .map(|id| Change::Dash {
                    id: id.clone(),
                    dash,
                })
                .collect(),
        )
    }
    pub(super) fn on_weight(&mut self, weight: Weight) -> Task<Message> {
        self.pen.weight = weight;
        self.edit_many(
            self.selected
                .iter()
                .map(|id| Change::Weight {
                    id: id.clone(),
                    weight,
                })
                .collect(),
        )
    }
    /// Which ends of the selection's arrows carry a head, and the next one's.
    /// Written to every shape picked and not only the arrows among them, the
    /// same way a fill is written to a line that will never paint one: the
    /// property is stored for every shape and read by the kinds that have it,
    /// so a mixed selection set to "both" and then drawn on comes out the way
    /// you asked rather than the way the first arrow happened to be.
    pub(super) fn on_heads(&mut self, heads: Heads) -> Task<Message> {
        self.pen.heads = heads;
        self.edit_many(
            self.selected
                .iter()
                .map(|id| Change::Heads {
                    id: id.clone(),
                    heads,
                })
                .collect(),
        )
    }
    /// Where the selection's words sit across the boxes they are written in.
    /// It is not remembered for the NEXT shape the way a colour is: a colour
    /// is a choice about the board, and how a card's words are set is a choice
    /// about that card's words.
    pub(super) fn on_align(&mut self, align: Align) -> Task<Message> {
        self.edit_many(
            self.selected
                .iter()
                .map(|id| Change::Align {
                    id: id.clone(),
                    align,
                })
                .collect(),
        )
    }
    pub(super) fn on_lettering(&mut self, text_size: TextSize) -> Task<Message> {
        let Some(board) = self.visible() else {
            return Task::none();
        };
        let mut changes = Vec::new();
        for id in &self.selected {
            changes.push(Change::TextSize {
                id: id.clone(),
                text_size,
            });
            let Some(record) = board.shapes.get(id) else {
                continue;
            };
            // A text shape IS its words, so its box is the size of them and
            // has to step with them: a line of type scales in both directions
            // at once, so the ratio between the two steps is the whole of it.
            // Nothing else here is measured, so nothing else can be fitted —
            // a card keeps the room it was given, wraps the bigger words in
            // the column it already has, and grows to them the next time it
            // is written in.
            let hugging = record.shape.kind == Kind::Text;
            if !hugging {
                continue;
            }
            let ratio = presentation::step(text_size) / presentation::step(record.shape.text_size);
            let fitted = |side: i32, floor: i32| {
                (side as f32 * ratio)
                    .ceil()
                    .clamp(floor as f32, boards_wire::MAX_SIZE as f32) as i32
            };
            changes.push(Change::Resize {
                id: id.clone(),
                width: fitted(record.shape.width, presentation::MIN_CARD[0]),
                height: fitted(record.shape.height, presentation::MIN_CARD[1]),
            });
        }
        self.edit_many(changes)
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
    /// Bind the selection into a group, or free whatever groups it touches.
    ///
    /// The name is the lowest id in the selection, which is already unique on
    /// the board and already a name the board can address. Minting a fresh one
    /// would need a source of names the view does not have, and a group's name
    /// is never read by anyone — only shared.
    pub(super) fn on_group(&mut self, binding: bool) -> Task<Message> {
        let Some(board) = self.visible() else {
            return Task::none();
        };
        let ids = self.grouping_ids(&board);
        if !self.grouping_would_hold(binding) {
            return Task::none();
        }
        let group = binding.then(|| ids[0].clone());
        self.edit(Change::Group { ids, group })
    }
    /// Whole groups, both ways. Picking a member already picks its group, so
    /// this is usually what the selection holds anyway — but a selection
    /// assembled some other way must not be able to bind or free HALF of a
    /// group, which would leave the rest carrying a name with nothing to share
    /// it with.
    fn grouping_ids(&self, board: &Board) -> Vec<String> {
        with_group_mates(board, self.selected.iter().cloned())
            .into_iter()
            .filter(|id| board.shapes.contains_key(id))
            .collect()
    }
    /// Whether binding the selection — or freeing it — would do anything. The
    /// menu asks so it can leave out a row that would do nothing, and
    /// `on_group` asks so the board is never sent a change it would refuse.
    pub(super) fn grouping_would_hold(&self, binding: bool) -> bool {
        let Some(board) = self.visible() else {
            return false;
        };
        let ids = self.grouping_ids(&board);
        match binding {
            // A group of one never reads differently from no group at all, and
            // the board refuses one — so the board would print an error at a
            // writer who only pressed the chord on a single card.
            true => ids.len() >= 2,
            // Nothing to free unless something in the selection is held.
            false => ids.iter().any(|id| board.shapes[id].shape.group.is_some()),
        }
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
                    .filter(|r| draggable(&r.shape))
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
                .all(|bond| picked.contains(&bond.card))
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
        if board.shapes.len() + shapes.len() > boards_wire::MAX_SHAPES {
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
        // A copy of a group is its OWN group. Copies that kept the original
        // name would join the group they were copied from, so picking the
        // original would pick its copies and moving it would drag them along.
        let regrouped: BTreeMap<String, String> = shapes
            .iter()
            .zip(&ids)
            .filter_map(|((old, s), id)| {
                Some((s.group.clone()?, mapping.get(old).unwrap_or(id).clone()))
            })
            .collect();
        let changes = shapes
            .into_iter()
            .zip(&ids)
            .map(|((_, mut s), id)| {
                s.x = coordinate((s.x + offset[0]) as f32);
                s.y = coordinate((s.y + offset[1]) as f32);
                s.group = s.group.and_then(|group| regrouped.get(&group).cloned());
                // A copy holds the COPIES of the cards it was bound to, at the
                // same places on them: an arrow planted beside its own cards
                // that still reached back to the originals would be a copy of
                // the picture with one line left behind in it.
                let replanted = |bond: boards_wire::Bond| {
                    mapping.get(&bond.card).map(|card| boards_wire::Bond {
                        card: card.clone(),
                        at: bond.at,
                    })
                };
                s.from = s.from.and_then(replanted);
                s.to = s.to.and_then(replanted);
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
    /// The secondary button, over whatever it landed on. A right-click on a
    /// shape OUTSIDE the selection takes that shape and nothing else, which is
    /// what makes "right-click, delete" delete the thing you pointed at rather
    /// than whatever was picked a minute ago. A right-click INSIDE the
    /// selection leaves it alone, so "pick three, right-click one, group"
    /// works. Both are what tldraw and excalidraw do.
    ///
    /// It asks the board what is under the cursor rather than reading `hover`:
    /// hover is only kept while a press would TAKE something, and the secondary
    /// button has an answer under every tool.
    pub(super) fn on_open_menu(&mut self) -> Task<Message> {
        // A card being written in owns the pointer; a menu over the words it
        // is editing would offer a cut that means the shape, not the writing.
        if self.inline.is_some() {
            return Task::none();
        }
        let under = self.hit(self.world(self.cursor));
        let points_somewhere_new = under.as_ref().is_some_and(|id| !self.selected.contains(id));
        if points_somewhere_new {
            self.selected = under.into_iter().collect();
        }
        // Nothing to offer is not a menu: an empty card under the cursor, and
        // a backdrop that eats the next press, is worse than the nothing that
        // happened before this existed.
        if self.menu_items().is_empty() {
            return Task::none();
        }
        self.menu = Some(self.cursor);
        Task::none()
    }
    pub(super) fn on_close_menu(&mut self) -> Task<Message> {
        self.menu = None;
        Task::none()
    }
    /// A row of the menu: close, then do the thing. One handler and not ten,
    /// so closing is written once.
    pub(super) fn on_menu_item(&mut self, item: MenuItem) -> Task<Message> {
        self.menu = None;
        match item {
            MenuItem::Cut => self.on_cut(),
            MenuItem::Copy => self.on_copy(),
            MenuItem::Paste => self.on_paste(),
            MenuItem::Duplicate => self.on_duplicate(),
            MenuItem::Front => self.on_stack(Stacking::Front),
            MenuItem::Forward => self.on_stack(Stacking::Forward),
            MenuItem::Backward => self.on_stack(Stacking::Backward),
            MenuItem::Back => self.on_stack(Stacking::Back),
            MenuItem::Group => self.on_group(true),
            MenuItem::Ungroup => self.on_group(false),
            MenuItem::SelectAll => self.on_select_all(),
            MenuItem::Delete => self.on_delete(),
        }
    }
    /// What the menu can offer over what is picked, in the order it lists them.
    /// Built from what would actually happen: a row that cannot do anything is
    /// left out rather than listed and dead, which is the rule the style panel
    /// already follows for its fill row.
    pub(super) fn menu_items(&self) -> Vec<MenuItem> {
        let Some(board) = self.visible() else {
            return Vec::new();
        };
        if self.selected.is_empty() {
            let pasteable = !self.clipboard.is_empty();
            let anything_to_take = !board.shapes.is_empty();
            let mut items = Vec::new();
            if pasteable {
                items.push(MenuItem::Paste);
            }
            if anything_to_take {
                items.push(MenuItem::SelectAll);
            }
            return items;
        }
        let mut items = vec![
            MenuItem::Cut,
            MenuItem::Copy,
            MenuItem::Duplicate,
            MenuItem::Front,
            MenuItem::Forward,
            MenuItem::Backward,
            MenuItem::Back,
        ];
        if self.grouping_would_hold(true) {
            items.push(MenuItem::Group);
        }
        if self.grouping_would_hold(false) {
            items.push(MenuItem::Ungroup);
        }
        items.push(MenuItem::Delete);
        items
    }
    /// Stacking, all four ways. The two ends are one primitive each — raising
    /// the selection puts it on top, and sinking it is raising everything else
    /// — and a single step is the whole stack restated with the selection one
    /// place along.
    pub(super) fn on_stack(&mut self, how: Stacking) -> Task<Message> {
        let Some(board) = self.visible() else {
            return Task::none();
        };
        let order: Vec<String> = board
            .ordered()
            .into_iter()
            .map(|(id, _)| id.clone())
            .collect();
        let mine = |id: &String| self.selected.contains(id);
        let anything_to_move = order.iter().any(&mine) && !order.iter().all(&mine);
        if !anything_to_move {
            return Task::none();
        }
        // `Change::Order` puts the ids it names on top, in the order given, and
        // leaves everything else in its own order underneath. The two ends say
        // that in one word each — name the selection, or name everything but.
        // A single step has to say the whole stack, because what changes is
        // where the selection sits INSIDE it.
        let ids = match how {
            Stacking::Front => order.iter().filter(|id| mine(id)).cloned().collect(),
            Stacking::Back => order.iter().filter(|id| !mine(id)).cloned().collect(),
            Stacking::Forward => stepped(&order, &mine, true),
            Stacking::Backward => stepped(&order, &mine, false),
        };
        let moved = ids != order && !ids.is_empty();
        if !moved {
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
                x: coordinate(p[0]),
                y: coordinate(p[1]),
                width: 220,
                height: 180,
                ..self.pen.shape(Kind::Note)
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
        let shapes = vec![
            column("Ideas", "What could we try?", 0, 0),
            column("Questions", "What do we need to learn?", 300, 1),
            column("Next steps", "Choose one thing to move forward.", 600, 2),
        ];
        let Some(offset) = self.centred_on_the_stage(&shapes) else {
            return Task::none();
        };
        self.plant(shapes, offset)
    }
    /// How far a set of shapes has to move to sit in the middle of what you are
    /// looking at. Its own answer and not `plant`'s, because `plant` awaits the
    /// host for a name per shape and nothing can be asserted about where they
    /// landed until it comes back — where they were SENT is decided here.
    ///
    /// The template's columns are laid out in world coordinates, and planting
    /// them AT those coordinates put the whole thing at a fixed spot on the
    /// board however far you had panned. So the one button that is supposed to
    /// show a first-time user what this is put three notes somewhere they were
    /// not looking, and read as doing nothing at all. "Add a note", beside it on
    /// the same card, has always placed in the middle of the stage.
    pub(super) fn centred_on_the_stage(&self, shapes: &[(String, Shape)]) -> Option<[i32; 2]> {
        let b = bounds(shapes.iter().map(|(_, s)| s))?;
        let middle = self.world([self.viewport[0] / 2., self.viewport[1] / 2.]);
        Some([
            coordinate(middle[0] - (b[0] + b[2]) / 2.),
            coordinate(middle[1] - (b[1] + b[3]) / 2.),
        ])
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
    /// Nothing is being lined up any more: the guides go and so do the lines
    /// they stood for. They are one fact and are dropped as one — a held line
    /// outliving its guide would silently pull the NEXT drag onto it.
    pub(super) fn stop_guiding(&mut self) {
        self.guides.clear();
        self.held = [None, None];
    }
    fn snap_delta(&self, shapes: &BTreeMap<String, Shape>, delta: [f32; 2]) -> Snapped {
        let Some(b) = bounds(shapes.values()) else {
            return Snapped::adrift(delta);
        };
        self.aligned(b, delta, [true; 2], &|id| shapes.contains_key(id))
    }
    /// The nudge that puts a corner in hand onto a line something already on
    /// the board stands on, and the guides that say which lines those are. A
    /// corner is a box of no size at all, so it asks exactly the question a
    /// moving card asks — which is the point: the guides that line a card up
    /// with its neighbours are the guides that should line up the one you are
    /// drawing, and drawing with no guides at all meant every new shape had to
    /// be nudged into place afterwards.
    fn corner_nudge(&self, at: [f32; 2], axes: [bool; 2], mine: &dyn Fn(&str) -> bool) -> Snapped {
        self.aligned([at[0], at[1], at[0], at[1]], [0.; 2], axes, mine)
    }
    /// The nudge that lines a box up with something already on the board, once
    /// the box, the axes it may travel on, and what to ignore are known.
    fn aligned(
        &self,
        b: [f32; 4],
        delta: [f32; 2],
        axes: [bool; 2],
        mine: &dyn Fn(&str) -> bool,
    ) -> Snapped {
        // Snap against the board on screen, not the last one consensus agreed
        // on: a card you drew a second ago is on screen and is exactly what you
        // want to line the next one up with.
        let Some(board) = self.visible() else {
            return Snapped::adrift(delta);
        };
        let capture = 6. / self.zoom;
        // A line already taken is KEPT until the hand is clearly past it. An
        // alignment tested afresh every frame engages and releases at whatever
        // speed the hand happens to be moving — a hand crossing six units in
        // one frame is aligned on half the frames and free on the rest — and a
        // guide that comes and goes every few pixels is a line flickering
        // across the whole board rather than an answer to what you are doing.
        let release = 3. * capture;
        let mut best = [capture; 2];
        let mut adjustment = [0.; 2];
        let mut held: [Option<Hold>; 2] = [None, None];
        for axis in (0..2).filter(|&axis| axes[axis]) {
            let Some(hold) = self.held[axis] else {
                continue;
            };
            let diff = nearest(b, axis, delta[axis], hold.fixed);
            let still_on_it = diff.abs() < release;
            if still_on_it {
                adjustment[axis] = diff;
                held[axis] = Some(hold);
            }
        }
        for (id, r) in &board.shapes {
            if mine(id) || r.shape.kind.is_path() {
                continue;
            }
            let target = rect(&r.shape);
            for axis in 0..2 {
                let free = axes[axis] && held[axis].is_none();
                if !free {
                    continue;
                }
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
                            held[axis] = Some(Hold {
                                fixed,
                                span: [target[1 - axis], target[3 - axis]],
                            });
                        }
                    }
                }
            }
        }
        let guides = (0..2)
            .filter_map(|axis| held[axis].map(|hold| hold.guide(axis, b)))
            .collect();
        Snapped {
            delta: [delta[0] + adjustment[0], delta[1] + adjustment[1]],
            guides,
            held,
        }
    }
}

/// A line a drag is currently held to: the coordinate it stands on, and how
/// far the shape that offered it reaches along that line, so the guide can be
/// redrawn as the held shape travels.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(super) struct Hold {
    fixed: f32,
    span: [f32; 2],
}

impl Hold {
    /// The guide, stretched to cover both the shape that offered the line and
    /// the box being lined up against it.
    fn guide(self, axis: usize, b: [f32; 4]) -> [f32; 4] {
        let low = b[1 - axis].min(self.span[0]) - 20.;
        let high = b[3 - axis].max(self.span[1]) + 20.;
        match axis {
            0 => [self.fixed, low, self.fixed, high],
            _ => [low, self.fixed, high, self.fixed],
        }
    }
}

/// What lining a box up came to: where it goes, the guides that say why, and
/// the lines it is now held to.
pub(super) struct Snapped {
    pub(super) delta: [f32; 2],
    pub(super) guides: Vec<[f32; 4]>,
    pub(super) held: [Option<Hold>; 2],
}

impl Snapped {
    /// Nothing to line up against: the box goes where it was going.
    fn adrift(delta: [f32; 2]) -> Self {
        Self {
            delta,
            guides: Vec::new(),
            held: [None, None],
        }
    }
}

/// The smallest move on `axis` that puts one of the box's three lines — its
/// two edges and its middle — onto `fixed`.
fn nearest(b: [f32; 4], axis: usize, delta: f32, fixed: f32) -> f32 {
    [b[axis], (b[axis] + b[axis + 2]) / 2., b[axis + 2]]
        .into_iter()
        .map(|moving| fixed - (moving + delta))
        .min_by(|left, right| left.abs().total_cmp(&right.abs()))
        .expect("a box has three lines on each axis")
}
/// The pen samples no more than this in one stroke; the release thins the run
/// down to the board's point budget before anything leaves the view.
const MAX_SAMPLES: usize = 4096;

/// Whether the point is on the plate a connector's words are written on. The
/// plate is stated in board units by the painter, and it is drawn at the middle
/// of the run rather than over the rectangle the samples were stored with, so
/// this reads the run the same way the painter does.
/// The point halfway ALONG a run, not the middle of the box drawn round it.
/// They are the same thing for a straight line and nothing like it for a bent
/// one: the box's centre of a curve is off in the open, and words written there
/// would be words beside the arrow rather than on it.
fn halfway(run: &[[f32; 2]]) -> Option<[f32; 2]> {
    let span = |step: &[[f32; 2]]| (step[1][0] - step[0][0]).hypot(step[1][1] - step[0][1]);
    let mut left: f32 = run.windows(2).map(span).sum::<f32>() / 2.;
    for step in run.windows(2) {
        let reach = span(step);
        if reach >= left {
            let part = if reach > 0. { left / reach } else { 0. };
            return Some([
                step[0][0] + (step[1][0] - step[0][0]) * part,
                step[0][1] + (step[1][1] - step[0][1]) * part,
            ]);
        }
        left -= reach;
    }
    run.first().copied()
}
/// Whether the sample in hand is one of the run's ends — the only two that can
/// take hold of a card. A bend in the middle is a bend, and dragging it across
/// a card binds nothing.
pub(super) fn reaches_for_a_card(s: &Shape, end: usize) -> bool {
    let last = path_points(s).len().saturating_sub(1);
    end == 0 || end == last
}
fn labelled(s: &Shape, run: &[[f32; 2]], point: [f32; 2]) -> bool {
    !s.text.is_empty() && contains(plate(run), point)
}
/// The plate a connector's words ride, in board units: the painter's size,
/// centred on the middle of the run as it is drawn now. A bound end follows the
/// card it holds, so this is nowhere near the rectangle the samples were stored
/// with, and everything that asks where the words are has to ask the run.
pub(super) fn plate(run: &[[f32; 2]]) -> [f32; 4] {
    let Some(middle) = halfway(run) else {
        return [0.; 4];
    };
    let reach = [presentation::PLATE[0] / 2., presentation::PLATE[1] / 2.];
    [
        middle[0] - reach[0],
        middle[1] - reach[1],
        middle[0] + reach[0],
        middle[1] + reach[1],
    ]
}
/// The stack with the selection moved one place, bottom-to-top the way
/// `Board::ordered` gives it.
///
/// Going up walks from the top down and going down walks from the bottom up,
/// so a shape this pass has already moved is never carried a second time by
/// the same pass. A run of selected shapes therefore travels together and
/// keeps its own order, and a selection already at that end comes back
/// unchanged — which the caller reads as nothing to send.
fn stepped(order: &[String], mine: &dyn Fn(&String) -> bool, up: bool) -> Vec<String> {
    let mut next = order.to_vec();
    let walk: Vec<usize> = match up {
        true => (0..next.len()).rev().collect(),
        false => (0..next.len()).collect(),
    };
    for i in walk {
        // usize::MAX out of the bottom, which the bound below refuses.
        let neighbour = match up {
            true => i + 1,
            false => i.wrapping_sub(1),
        };
        let swappable = neighbour < next.len() && mine(&next[i]) && !mine(&next[neighbour]);
        if swappable {
            next.swap(i, neighbour);
        }
    }
    next
}
/// A shape the pointer moves and resizes on its own. A bound connector has no
/// geometry of its own to drag: it follows the cards its ends name.
pub(super) fn free(s: &Shape) -> bool {
    s.from.is_none() && s.to.is_none()
}
/// Whether a drag has anything of this shape's own to carry. A card always
/// does. A connector only owns the ends no card is holding: one that is held
/// follows its card and always has, so an arrow bound at both ends has nothing
/// to move and moving it would be a lie. One bound at a SINGLE end still has a
/// far end standing on its own point, and a drag that left it where it was —
/// which is what refusing every bound connector did — was an arrow you could
/// select, see selected, and not move.
pub(super) fn draggable(s: &Shape) -> bool {
    s.from.is_none() || s.to.is_none()
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
/// Where on the board a bond points: the anchor it names, read back through
/// the card's box as the box is NOW. A card that moves carries its anchors
/// with it, and one that is resized keeps them in the same relative place —
/// which is the whole reason the bond stores a share of the box rather than a
/// point on the board.
pub(super) fn anchor_point(card: &Shape, at: [i32; 2]) -> [f32; 2] {
    let span = boards_wire::ANCHOR_SPAN as f32;
    [
        card.x as f32 + card.width as f32 * at[0] as f32 / span,
        card.y as f32 + card.height as f32 * at[1] as f32 / span,
    ]
}
/// The bond an end let go of at this point makes with this card: the point as
/// a share of the card's box. Dropped near the middle it comes out near the
/// middle, which is what an arrow usually means — so "remember where I put it"
/// costs nothing at the one place people do not care about it.
fn bond_at(card_id: &str, card: &Shape, point: [f32; 2]) -> boards_wire::Bond {
    let span = boards_wire::ANCHOR_SPAN as f32;
    let share = |value: f32, origin: i32, size: i32| {
        let size = (size as f32).max(1.);
        ((value - origin as f32) / size * span)
            .round()
            .clamp(0., span) as i32
    };
    boards_wire::Bond {
        card: card_id.to_owned(),
        at: [
            share(point[0], card.x, card.width),
            share(point[1], card.y, card.height),
        ],
    }
}
/// A connector's run with every bound end put where that end STANDS: the point
/// on the card its bond anchors to. Where the line MEETS the card is a
/// different question — the card's outline answers that, and answers it again
/// every time either of them moves. Storing the anchor makes the sample a
/// record of where the card WAS, which is the only thing that lets a bend
/// still be a bend of this line once the card moves.
///
/// Every write of a connector's run goes through here, so the sample and the
/// card agree at rest and differ afterwards by exactly the drag between them.
pub(super) fn standing(
    board: &Board,
    run: &[[f32; 2]],
    from: &Option<boards_wire::Bond>,
    to: &Option<boards_wire::Bond>,
) -> Vec<[f32; 2]> {
    let mut run = run.to_vec();
    let Some(last) = run.len().checked_sub(1) else {
        return run;
    };
    let at = |end: &Option<boards_wire::Bond>| {
        let bond = end.as_ref()?;
        let card = &board.shapes.get(&bond.card)?.shape;
        Some(anchor_point(card, bond.at))
    };
    if let Some(p) = at(from) {
        run[0] = p;
    }
    if let Some(p) = at(to) {
        run[last] = p;
    }
    run
}
/// A point carried from one chord onto another: the same place in the chord's
/// own frame — how far along it lies and how far off it stands, both as
/// fractions of the chord — read back off the chord that replaced it. Two
/// pairs of points name one turn and one stretch, so what is carried keeps its
/// shape instead of its coordinates.
fn reframed(stood: [[f32; 2]; 2], anchor: [[f32; 2]; 2], point: [f32; 2]) -> [f32; 2] {
    let axis = [stood[1][0] - stood[0][0], stood[1][1] - stood[0][1]];
    let span = axis[0] * axis[0] + axis[1] * axis[1];
    // A chord of no length is not a frame: there is nothing to measure against
    // and nothing to carry onto.
    if span < 0.001 {
        return point;
    }
    let off = [point[0] - stood[0][0], point[1] - stood[0][1]];
    let along = (off[0] * axis[0] + off[1] * axis[1]) / span;
    let across = (off[0] * -axis[1] + off[1] * axis[0]) / span;
    let next = [anchor[1][0] - anchor[0][0], anchor[1][1] - anchor[0][1]];
    [
        anchor[0][0] + next[0] * along - next[1] * across,
        anchor[0][1] + next[1] * along + next[0] * across,
    ]
}
/// The polyline a connector actually draws: its samples, with a bound end
/// pulled onto the border of the card it names.
pub(super) fn stroke(board: &Board, s: &Shape) -> Vec<[f32; 2]> {
    let mut path = path_points(s);
    if path.len() < 2 {
        return Vec::new();
    }
    let held = |end: &Option<boards_wire::Bond>| {
        let bond = end.as_ref()?;
        let card = board.shapes.get(&bond.card)?.shape.clone();
        Some((card, bond.at))
    };
    let (from, to) = (held(&s.from), held(&s.to));
    let last = path.len() - 1;
    // Where the ends stood when the run was written, and where they stand now.
    // For a free end those are the same point. For a bound end they differ by
    // however far the card has been dragged since — see [`standing`].
    let stood = [path[0], path[last]];
    let anchor = [
        from.as_ref()
            .map_or(stood[0], |(card, at)| anchor_point(card, *at)),
        to.as_ref()
            .map_or(stood[1], |(card, at)| anchor_point(card, *at)),
    ];
    // A bend belongs to the line and not to the board. Left at its stored
    // point it stops being a bend of this arrow the moment a card moves: the
    // ends swing away and the arc is left behind as a hook across the gap.
    // Carried in the chord's own frame it turns and stretches with the cards,
    // which is the only reading under which moving a card leaves the drawing
    // you made recognisable.
    if path.len() == 3 {
        path[1] = reframed(stood, anchor, path[1]);
    }
    [path[0], path[last]] = anchor;
    // A bound end leaves toward the next place the run actually goes: the bend
    // beside it, not the far end past it. A bent arrow used to leave its card
    // aimed at where it finishes, which on a curved run is not the way the
    // line goes at all — it left through one side and kinked back across the
    // card to reach its own bend. With no bend the next place IS the far end,
    // which now stands where it stands rather than where it was dropped.
    let toward_start = path[1];
    let toward_end = path[last - 1];
    if let Some((card, _)) = &from {
        path[0] = meeting(card, anchor[0], toward_start);
    }
    if let Some((card, _)) = &to {
        path[last] = meeting(card, anchor[1], toward_end);
    }
    path
}
/// How finely the crossing below is hunted. Twenty-four halvings put it within
/// a millionth of the run's length, which is far under a pixel at any zoom the
/// board allows — and the hunt costs a few dozen floats per bound end.
const CROSSINGS: u32 = 24;
/// Where a connector leaves the card it holds: the point at which the line out
/// of its anchor towards `toward` crosses the card's outline, so the run stops
/// at the edge instead of burying its head in the card.
///
/// The anchor is where the end was DROPPED and the crossing is where the line
/// is DRAWN, and they are different points on purpose: an arrow coming from
/// the left meets a card on its left side however near the right edge you let
/// go of it. What the anchor decides is the aim — let go near the top and the
/// run meets the card high, which is the whole of "it remembers where I put
/// it".
///
/// The outline is [`covers`], the same answer a press gets, rather than a
/// second per-kind formula that could disagree with it: the crossing is hunted
/// by halving the segment. An anchor the outline does not contain is not a
/// place on the card at all — only a hand-written board has one — so the end
/// falls back to aiming from the middle.
pub(super) fn meeting(s: &Shape, anchor: [f32; 2], toward: [f32; 2]) -> [f32; 2] {
    let mut inside = match covers(s, anchor) {
        true => anchor,
        false => center(s),
    };
    if covers(s, toward) {
        return inside;
    }
    let mut outside = toward;
    for _ in 0..CROSSINGS {
        let middle = [(inside[0] + outside[0]) / 2., (inside[1] + outside[1]) / 2.];
        match covers(s, middle) {
            true => inside = middle,
            false => outside = middle,
        }
    }
    inside
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
        if kept.len() <= boards_wire::MAX_POINTS {
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
/// The picked shapes, plus everything grouped with any of them.
///
/// A group is a name its members share, and nothing on the board addresses one
/// member alone: picking one picks all of them, which is what makes a group
/// move, recolour and delete as one thing without a single gesture knowing
/// that groups exist. Ids in no group stand for themselves.
pub(super) fn with_group_mates(
    board: &Board,
    picked: impl IntoIterator<Item = String>,
) -> BTreeSet<String> {
    let picked: BTreeSet<String> = picked.into_iter().collect();
    let held: BTreeSet<&str> = picked
        .iter()
        .filter_map(|id| board.shapes.get(id)?.shape.group.as_deref())
        .collect();
    if held.is_empty() {
        return picked;
    }
    board
        .shapes
        .iter()
        .filter(|(id, record)| {
            let named = picked.contains(*id);
            let mate = record
                .shape
                .group
                .as_deref()
                .is_some_and(|group| held.contains(group));
            named || mate
        })
        .map(|(id, _)| id.clone())
        .collect()
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
/// The box around the shapes on a board that a predicate keeps, or `None` when
/// it keeps nothing worth pointing a camera at.
///
/// A connector a card is holding at BOTH ends is never in it: what it stores is
/// the box consensus last wrote for it, not where the cards have since carried
/// it, so a stale box would aim the camera at empty board.
fn framed(board: &Board, mine: &dyn Fn(&str) -> bool) -> Option<[f32; 4]> {
    bounds(
        board
            .shapes
            .iter()
            .filter(|&(id, r)| free(&r.shape) && mine(id))
            .map(|(_, r)| &r.shape),
    )
}
pub(super) fn points_rect(a: [f32; 2], b: [f32; 2]) -> [f32; 4] {
    [
        a[0].min(b[0]),
        a[1].min(b[1]),
        a[0].max(b[0]),
        a[1].max(b[1]),
    ]
}
/// How far the camera moves along one axis this tick, from where the pointer
/// sits along it. Zero anywhere but the two margins at the ends.
///
/// The camera moves the OPPOSITE way to the reach: a pointer held at the right
/// edge is asking for the board further right, and the board comes left to meet
/// it.
///
/// Squared in the depth, not linear. Linear starts moving the moment the
/// pointer enters the margin, which drags the board every time a card is placed
/// near the edge on purpose; squared leaves the first third of the margin
/// almost still and gets going only when the pointer is genuinely pressed
/// against the edge. Both are a tuning, and this one was picked on a live
/// board.
pub(super) fn edge_step(at: f32, span: f32) -> f32 {
    /// How wide the band at each end of the stage is, in screen pixels.
    const MARGIN: f32 = 56.;
    /// The fastest the board travels, in screen pixels per tick. At a tick
    /// every 16ms that is about 750 a second — a 1280-wide stage crossed in
    /// under two, which is quick enough to carry a card somewhere and slow
    /// enough to stop where you meant to.
    const FASTEST: f32 = 12.;
    let into_the_near_edge = (MARGIN - at).clamp(0., MARGIN);
    let into_the_far_edge = (at - (span - MARGIN)).clamp(0., MARGIN);
    let depth = into_the_near_edge - into_the_far_edge;
    let ramp = (depth / MARGIN).abs().powi(2);
    FASTEST * ramp * depth.signum()
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
/// Where a handle taken at `start` has been carried to. A press takes a handle
/// from a few pixels away, so the handle is not under the pointer — it travels
/// with it. Lining up the finger instead of the corner would leave the corner
/// short by exactly the distance the press was off by.
pub(super) fn carried(handle: [f32; 2], start: [f32; 2], point: [f32; 2]) -> [f32; 2] {
    [
        handle[0] + point[0] - start[0],
        handle[1] + point[1] - start[1],
    ]
}
/// The axes a handle drag can travel on. An edge handle moves on one of them,
/// and a guide drawn on the other would promise an alignment the drag cannot
/// reach.
pub(super) fn travelling(corner: [i32; 2]) -> [bool; 2] {
    [corner[0] != 0, corner[1] != 0]
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
pub(super) fn line_distance(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let d = [b[0] - a[0], b[1] - a[1]];
    let t = (((p[0] - a[0]) * d[0] + (p[1] - a[1]) * d[1])
        / (d[0] * d[0] + d[1] * d[1]).max(0.001))
    .clamp(0., 1.);
    (p[0] - a[0] - t * d[0]).hypot(p[1] - a[1] - t * d[1])
}
