use crate::*;
use ducktape_view_guest::{
    kit, slots,
    wire::{
        self, ButtonPreset, CanvasCommand as Draw, CanvasShape as Geometry, Length, Node, Rgba,
    },
};

fn stroke(color: Rgba, width: f32) -> wire::CanvasStroke {
    wire::CanvasStroke {
        color,
        width,
        cap: wire::CanvasLineCap::Round,
        join: wire::CanvasLineJoin::Round,
        dash: Vec::new(),
        dash_offset: 0,
    }
}
fn line(from: [f32; 2], to: [f32; 2], color: Rgba, width: f32) -> Draw {
    Draw::Draw {
        shape: Geometry::Line { from, to },
        fill: None,
        even_odd: false,
        stroke: Some(stroke(color, width)),
    }
}
fn rectangle(
    position: [f32; 2],
    size: [f32; 2],
    fill: Option<Rgba>,
    border: Rgba,
    width: f32,
) -> Draw {
    Draw::Draw {
        shape: Geometry::Rectangle {
            position,
            size,
            radius: [8.; 4],
        },
        fill,
        even_odd: false,
        stroke: Some(stroke(border, width)),
    }
}
const COLORS: [[f32; 4]; 5] = [
    [0.99, 0.88, 0.53, 1.],
    [0.70, 0.84, 0.99, 1.],
    [0.76, 0.91, 0.75, 1.],
    [0.90, 0.78, 0.98, 1.],
    [0.99, 0.76, 0.73, 1.],
];
impl BoardsView {
    pub(crate) fn view(&self) -> Node {
        kit::set_dark(self.session.dark);
        let board = self.visible();
        let w = self.viewport[0].max(640.);
        let h = self.viewport[1].max(480.);
        let compact = w < 960.;
        let mut layers = vec![
            self.canvas(
                &board
                    .clone()
                    .unwrap_or_else(|| Board::new("Board".into(), String::new()).unwrap()),
            ),
        ];
        if let Some(inline) = &self.inline
            && let Some(record) = board
                .as_ref()
                .and_then(|board| board.shapes.get(&inline.id))
        {
            let s = &record.shape;
            layers.push(self.inline_editor(
                s,
                self.screen(s.x as f32, s.y as f32),
                [s.width as f32 * self.zoom, s.height as f32 * self.zoom],
            ));
        }
        let title = board.as_ref().map_or("Your boards", |b| b.title.as_str());
        let heading = kit::column(
            "boards/identity",
            [
                kit::text_size(
                    kit::weighted(kit::text("boards/brand", "BOARDS"), wire::Weight::Semibold),
                    10.,
                ),
                action(
                    "boards/switcher",
                    title,
                    "Choose a board",
                    Message::BoardPicker,
                    true,
                ),
                kit::caption("boards/sync", self.status()),
            ],
        );
        layers.push(pin(
            "boards/identity-pin",
            18.,
            18.,
            228.,
            None,
            panel("boards/identity-panel", heading),
        ));
        if self.board_picker || board.is_none() {
            let idle = self.pending.is_empty() && self.inline.is_none();
            let mut list = vec![
                kit::input(
                    "boards/new-title",
                    "Name your board",
                    &self.title,
                    slots::handler(Box::new(|s| Some(Message::Title(s)))),
                    Some(slots::message(Message::CreateBoard)),
                ),
                action(
                    "boards/new",
                    "Create board",
                    "Create a shared board",
                    Message::CreateBoard,
                    self.session.connected && idle && !self.title.trim().is_empty(),
                ),
            ];
            list.extend(self.catalog.iter().map(|(id, title)| {
                action(
                    &format!("boards/open/{id}"),
                    title,
                    "Open board",
                    Message::Open(id.clone()),
                    idle,
                )
            }));
            list.push(kit::caption("boards/shared", "Shared with this network"));
            layers.push(pin(
                "boards/picker-pin",
                18.,
                132.,
                228.,
                None,
                panel("boards/picker", kit::column("boards/list", list)),
            ));
        }
        if board.is_some() {
            let mut tools = Vec::new();
            for (tool, label, key, icon_name) in TOOLS {
                tools.push(tool_button(tool, label, key, icon_name, self.tool == tool));
            }
            tools.push(tool_button_message(
                "Lock",
                "Q",
                "lock",
                Message::LockTool,
                self.tool_locked,
            ));
            let toolbar = kit::row("boards/tools", tools);
            layers.push(pin(
                "boards/tools-pin",
                (w - 420.) / 2.,
                if compact { 132. } else { 18. },
                420.,
                None,
                panel("boards/toolbar", toolbar),
            ));
            let count = self.selected.len();
            if count > 0 && self.inline.is_none() {
                let name = if count == 1 {
                    self.only_selected()
                        .and_then(|id| board.as_ref()?.shapes.get(id))
                        .map_or("Selection", |r| kind_name(r.shape.kind))
                        .to_owned()
                } else {
                    format!("{count} selected")
                };
                let mut properties = vec![
                    kit::heading("boards/selection-title", name),
                    kit::caption("boards/color-label", "Color"),
                    kit::row(
                        "boards/colors",
                        (0..5).map(|color| swatch(color, self.palette == color)),
                    ),
                ];
                if count == 1 {
                    properties.push(action(
                        "boards/edit-text",
                        "Edit text",
                        "Enter · double-click",
                        Message::EditText,
                        true,
                    ));
                }
                properties.push(action(
                    "boards/duplicate",
                    "Duplicate",
                    "⌘ / Ctrl D",
                    Message::Duplicate,
                    true,
                ));
                if count > 1 {
                    properties.push(action(
                        "boards/align-left",
                        "Align left",
                        "Align selected cards",
                        Message::Align(false),
                        true,
                    ));
                    properties.push(action(
                        "boards/align-top",
                        "Align top",
                        "Align selected cards",
                        Message::Align(true),
                        true,
                    ));
                }
                properties.push(action(
                    "boards/delete",
                    "Delete",
                    "Delete / Backspace",
                    Message::Delete,
                    true,
                ));
                layers.push(pin(
                    "boards/properties-pin",
                    18.,
                    160.,
                    196.,
                    None,
                    panel(
                        "boards/properties",
                        kit::column("boards/properties-body", properties),
                    ),
                ));
            }
            if let Some(inline) = &self.inline {
                let length = inline.document.text().len();
                layers.push(pin(
                    "boards/typing-pin",
                    (w - 350.) / 2.,
                    h - 124.,
                    350.,
                    None,
                    panel(
                        "boards/typing",
                        kit::row(
                            "boards/typing-row",
                            [
                                kit::caption(
                                    "boards/typing-hint",
                                    format!("Enter for a new line · {length}/{}", boards::MAX_TEXT),
                                ),
                                action(
                                    "boards/done",
                                    "Done",
                                    "⌘ / Ctrl Enter",
                                    Message::FinishText,
                                    true,
                                ),
                            ],
                        ),
                    ),
                ));
            }
            let footer = kit::row(
                "boards/history",
                [
                    action(
                        "boards/undo",
                        "Undo",
                        "⌘ / Ctrl Z",
                        Message::Undo,
                        !self.undo.is_empty(),
                    ),
                    action(
                        "boards/redo",
                        "Redo",
                        "⌘ / Ctrl Shift Z",
                        Message::Redo,
                        !self.redo.is_empty(),
                    ),
                    action(
                        "boards/help",
                        "?",
                        "Keyboard shortcuts",
                        Message::Help,
                        true,
                    ),
                ],
            );
            layers.push(pin(
                "boards/history-pin",
                18.,
                h - 66.,
                210.,
                None,
                panel("boards/history-panel", footer),
            ));
            layers.push(pin(
                "boards/zoom-pin",
                w - 288.,
                h - 66.,
                270.,
                None,
                panel(
                    "boards/zoom-panel",
                    kit::row(
                        "boards/zoom-controls",
                        [
                            action("boards/zoom-out", "−", "Zoom out", Message::Zoom(0.8), true),
                            action(
                                "boards/zoom",
                                &format!("{}%", (self.zoom * 100.).round()),
                                "Reset zoom · 0",
                                Message::ResetZoom,
                                true,
                            ),
                            action("boards/zoom-in", "+", "Zoom in", Message::Zoom(1.25), true),
                            action("boards/fit", "Fit", "Fit board · F", Message::Fit, true),
                        ],
                    ),
                ),
            ));
            let hint = match self.tool {
                Tool::Select => "Double-click to write · Shift-click to add to selection",
                Tool::Hand => "Drag to explore · Release Space to return to your tool",
                Tool::Note => "Click to place a note and start typing",
                Tool::Rectangle => "Drag to draw a box · Shift-resize to keep proportions",
                Tool::Text => "Click to write · Double-click any card to edit",
                Tool::Connect => {
                    if self.connection.is_some() {
                        "Choose the destination card · Esc to cancel"
                    } else {
                        "Choose two cards to connect"
                    }
                }
            };
            layers.push(pin(
                "boards/hint-pin",
                (w - 450.) / 2.,
                h - 28.,
                450.,
                Some(20.),
                kit::caption("boards/hint", hint),
            ));
            layers.push(pin(
                "boards/snap-pin",
                w - 118.,
                18.,
                100.,
                None,
                panel(
                    "boards/snap-panel",
                    action(
                        "boards/snap",
                        if self.snap { "Snap on" } else { "Snap off" },
                        "Hold Alt to bypass snapping",
                        Message::Snap,
                        true,
                    ),
                ),
            ));
            if board.as_ref().is_some_and(|b| b.shapes.is_empty()) {
                let empty = kit::column(
                    "boards/empty-body",
                    [
                        kit::title("boards/empty-title", "Make space for your next idea"),
                        kit::text(
                            "boards/empty-text",
                            "Drop a note, sketch a flow, or start with a simple layout.",
                        ),
                        action(
                            "boards/first-note",
                            "Add a note",
                            "N · Click anywhere",
                            Message::QuickNote,
                            true,
                        ),
                        action(
                            "boards/template",
                            "Start a brainstorm",
                            "Ideas, questions, next steps",
                            Message::Template,
                            true,
                        ),
                    ],
                );
                layers.push(pin(
                    "boards/empty-pin",
                    (w - 420.) / 2.,
                    (h - 180.) / 2.,
                    420.,
                    None,
                    panel("boards/empty", empty),
                ));
            }
        }
        let notice = match &self.delivery {
            Delivery::Failed(error) => Some(format!("Your changes are kept here. {error}")),
            _ => {
                if self.error.is_empty() {
                    None
                } else {
                    Some(self.error.clone())
                }
            }
        };
        if let Some(notice) = notice {
            let mut children = vec![kit::tone_text("boards/error", notice, kit::Tone::Danger)];
            if matches!(self.delivery, Delivery::Failed(_)) {
                children.extend([
                    action(
                        "boards/retry",
                        "Retry saving",
                        "Retry these changes",
                        Message::Retry,
                        self.session.connected,
                    ),
                    action(
                        "boards/discard",
                        "Use saved board",
                        "Discard local pending changes",
                        Message::DiscardPending,
                        true,
                    ),
                ]);
            }
            layers.push(pin(
                "boards/notice-pin",
                (w - 480.) / 2.,
                100.,
                480.,
                None,
                panel("boards/notice", kit::column("boards/notice-body", children)),
            ));
        }
        if self.help {
            let shortcuts = [
                ("V / 1", "Select"),
                ("H / 2 · hold Space", "Pan"),
                ("N / 3", "Sticky note"),
                ("R / 4", "Rectangle"),
                ("T / 5", "Text"),
                ("A / 6", "Connector"),
                ("Q", "Keep tool active"),
                ("Shift-click / drag", "Multiple selection"),
                ("Enter / double-click", "Edit text"),
                ("⌘ / Ctrl Enter", "Finish text / next note"),
                ("⌘ / Ctrl D", "Duplicate selection"),
                ("⌘ / Ctrl Z · Shift Z", "Undo / redo"),
                ("Arrow · Shift Arrow", "Move 1 / 10 units"),
                ("⌘ / Ctrl + scroll", "Zoom at pointer"),
                ("F · Shift F · 0", "Fit board / selection / 100%"),
                ("Esc", "Cancel the current gesture"),
            ];
            let mut rows = vec![kit::heading("boards/help-title", "Keyboard shortcuts")];
            rows.extend(shortcuts.into_iter().enumerate().map(|(i, (key, label))| {
                kit::row(
                    format!("boards/key/{i}"),
                    [
                        kit::sized(
                            kit::text(format!("boards/key-label/{i}"), label),
                            Some(Length::FillPortion(1)),
                            None,
                        ),
                        kit::caption(format!("boards/key-combo/{i}"), key),
                    ],
                )
            }));
            rows.push(action(
                "boards/help-close",
                "Got it",
                "Close shortcuts",
                Message::Help,
                true,
            ));
            layers.push(pin(
                "boards/help-pin",
                (w - 480.) / 2.,
                110.,
                480.,
                None,
                panel("boards/help-panel", kit::column("boards/help-body", rows)),
            ));
        }
        Node::Stack {
            key: "boards/root".into(),
            width: Some(Length::Fill),
            height: Some(Length::Fill),
            padding: None,
            background: Some(Rgba(self.canvas_color())),
            border: None,
            clip: true,
            under: 0,
            children: layers,
        }
    }
    fn canvas_color(&self) -> [f32; 4] {
        if self.session.dark {
            [0.075, 0.083, 0.105, 1.]
        } else {
            [0.975, 0.978, 0.985, 1.]
        }
    }
    fn status(&self) -> String {
        if self.inline.is_some() {
            return "Editing text…".into();
        }
        match &self.delivery {
            Delivery::Failed(_) => "Not saved".into(),
            Delivery::Sending => format!("Saving {} changes…", self.pending.len()),
            Delivery::Idle => {
                if self.pending.is_empty() {
                    "Saved".into()
                } else {
                    format!("{} changes waiting", self.pending.len())
                }
            }
        }
    }
    fn screen(&self, x: f32, y: f32) -> [f32; 2] {
        [
            x * self.zoom + self.camera[0],
            y * self.zoom + self.camera[1],
        ]
    }
    fn canvas(&self, board: &Board) -> Node {
        let muted = Rgba(if self.session.dark {
            [0.8, 0.82, 0.9, 0.15]
        } else {
            [0.28, 0.32, 0.45, 0.16]
        });
        let accent = Rgba([0.40, 0.35, 0.94, 1.]);
        let mut commands = Vec::new();
        let mut labels = Vec::new();
        let spacing = (32. * self.zoom).max(28.);
        for col in 0..((self.viewport[0] / spacing).ceil() as usize).min(60) {
            for row in 0..((self.viewport[1] / spacing).ceil() as usize).min(40) {
                let center = [
                    self.camera[0].rem_euclid(spacing) + col as f32 * spacing,
                    self.camera[1].rem_euclid(spacing) + row as f32 * spacing,
                ];
                commands.push(Draw::Draw {
                    shape: Geometry::Circle {
                        center,
                        radius: 0.7,
                    },
                    fill: Some(muted),
                    even_odd: false,
                    stroke: None,
                });
            }
        }
        for (id, record) in board.ordered() {
            let s = &record.shape;
            if s.kind != Kind::Arrow {
                continue;
            }
            let (Some(from), Some(to)) = (&s.from, &s.to) else {
                continue;
            };
            let (Some(a), Some(b)) = (board.shapes.get(from), board.shapes.get(to)) else {
                continue;
            };
            let start = anchor(&a.shape, &b.shape);
            let end = anchor(&b.shape, &a.shape);
            let start = self.screen(start[0], start[1]);
            let end = self.screen(end[0], end[1]);
            let color = if self.selected.contains(id) {
                accent
            } else {
                Rgba(ink(s.color))
            };
            commands.push(line(start, end, color, 2.));
            if !s.text.is_empty() {
                labels.push(Node::Pin {
                    key: format!("boards/arrow-label/{id}"),
                    x: (start[0] + end[0]) / 2.,
                    y: (start[1] + end[1]) / 2. - 20.,
                    width: Some(Length::Fixed(180.)),
                    height: Some(Length::Fixed(40.)),
                    content: Box::new(kit::wrapping(kit::text(
                        format!("boards/arrow-text/{id}"),
                        excerpt(&s.text),
                    ))),
                });
            }
            let angle = (end[1] - start[1]).atan2(end[0] - start[0]);
            for turn in [-0.5_f32, 0.5] {
                commands.push(line(
                    end,
                    [
                        end[0] - 12. * (angle + turn).cos(),
                        end[1] - 12. * (angle + turn).sin(),
                    ],
                    color,
                    2.,
                ));
            }
        }
        for (id, record) in board.ordered() {
            let s = &record.shape;
            if s.kind == Kind::Arrow {
                continue;
            }
            let pos = self.screen(s.x as f32, s.y as f32);
            let size = [s.width as f32 * self.zoom, s.height as f32 * self.zoom];
            let outside = pos[0] + size[0] < 0.
                || pos[1] + size[1] < 0.
                || pos[0] > self.viewport[0]
                || pos[1] > self.viewport[1];
            if outside {
                continue;
            }
            let selected = self.selected.contains(id) || self.connection.as_ref() == Some(id);
            let fill = match s.kind {
                Kind::Note => Some(Rgba(COLORS[s.color as usize])),
                Kind::Rectangle => {
                    let mut color = COLORS[s.color as usize];
                    color[3] = if self.session.dark { 0.24 } else { 0.28 };
                    Some(Rgba(color))
                }
                Kind::Text | Kind::Arrow => None,
            };
            let border = if selected { accent } else { Rgba(ink(s.color)) };
            if s.kind != Kind::Text || selected {
                commands.push(rectangle(
                    pos,
                    size,
                    fill,
                    border,
                    if selected { 1.5 } else { 0.6 },
                ));
            }
            if selected && self.inline.is_none() && self.selected.len() == 1 {
                for corner in [[-1, -1], [1, -1], [-1, 1], [1, 1]] {
                    let world = interaction::corner_point(s, corner);
                    let handle = self.screen(world[0], world[1]);
                    commands.push(rectangle(
                        [handle[0] - 4., handle[1] - 4.],
                        [8., 8.],
                        Some(Rgba([1.; 4])),
                        accent,
                        1.5,
                    ));
                }
            }
            if self.inline.as_ref().is_some_and(|inline| &inline.id == id) {
                continue;
            }
            let text = if s.text.is_empty() {
                "Write a thought…"
            } else {
                excerpt(&s.text)
            };
            let mut label = kit::text_size(
                kit::wrapping(kit::text(format!("boards/label/{id}"), text)),
                (if s.kind == Kind::Text { 26. } else { 17. } * self.zoom).clamp(8., 78.),
            );
            if s.kind == Kind::Note {
                label = kit::colored(label, [0.16, 0.15, 0.12, 1.]);
            } else {
                label = kit::colored(label, ink(s.color));
            }
            let mut clip = kit::container(format!("boards/label-clip/{id}"), label);
            if let Node::Container {
                clip: clipped,
                height,
                ..
            } = &mut clip
            {
                *clipped = true;
                *height = Some(Length::Fill);
            }
            labels.push(Node::Pin {
                key: format!("boards/pin/{id}"),
                x: pos[0] + 12. * self.zoom,
                y: pos[1] + 12. * self.zoom,
                width: Some(Length::Fixed((size[0] - 24. * self.zoom).max(1.))),
                height: Some(Length::Fixed((size[1] - 24. * self.zoom).max(1.))),
                content: Box::new(clip),
            });
        }
        for guide in &self.guides {
            commands.push(line(
                self.screen(guide[0], guide[1]),
                self.screen(guide[2], guide[3]),
                accent,
                1.,
            ));
        }
        if let Gesture::Marquee { start, point, .. } = &self.gesture {
            let b = interaction::points_rect(*start, *point);
            let pos = self.screen(b[0], b[1]);
            commands.push(rectangle(
                pos,
                [(b[2] - b[0]) * self.zoom, (b[3] - b[1]) * self.zoom],
                Some(Rgba([0.4, 0.35, 0.94, 0.08])),
                accent,
                1.,
            ));
        }
        if let Gesture::Create { kind, start, point } = &self.gesture {
            let shape = self.creation_shape(*kind, *start, *point);
            let pos = self.screen(shape.x as f32, shape.y as f32);
            commands.push(rectangle(
                pos,
                [
                    shape.width as f32 * self.zoom,
                    shape.height as f32 * self.zoom,
                ],
                Some(Rgba([0.4, 0.35, 0.94, 0.08])),
                accent,
                1.,
            ));
        }
        if let Some(from) = self.connection.as_ref().and_then(|id| board.shapes.get(id)) {
            let start = self.screen(
                (from.shape.x + from.shape.width / 2) as f32,
                (from.shape.y + from.shape.height / 2) as f32,
            );
            commands.push(line(start, self.cursor, accent, 1.5));
        }
        let mut children = vec![Node::Canvas {
            key: "boards/geometry".into(),
            width: Some(Length::Fill),
            height: Some(Length::Fill),
            commands,
        }];
        children.extend(labels);
        let scene = Node::Stack {
            key: "boards/scene".into(),
            width: Some(Length::Fill),
            height: Some(Length::Fill),
            padding: None,
            background: Some(Rgba(self.canvas_color())),
            border: None,
            clip: true,
            under: 0,
            children,
        };
        let mouse = Node::MouseArea {
            key: "boards/canvas".into(),
            on_press: Some(slots::message(Message::Begin)),
            on_press_at: Some(slots::handler(Box::new(|(x, y)| {
                Some(Message::Position(x, y))
            }))),
            on_release: Some(slots::message(Message::Release)),
            on_move: Some(slots::handler(Box::new(|(x, y)| Some(Message::Move(x, y))))),
            on_exit: Some(slots::message(Message::Release)),
            on_scroll: Some(slots::handler(Box::new(|(x, y, pixels)| {
                Some(Message::Wheel(x, y, pixels))
            }))),
            on_double_click: Some(slots::message(Message::DoubleClick)),
            on_right_press: None,
            on_right_release: None,
            on_middle_press: Some(slots::message(Message::MiddleDown)),
            on_middle_release: Some(slots::message(Message::Release)),
            on_enter: None,
            content: Box::new(scene),
        };
        let measure = || Some(slots::handler(Box::new(|(w, h)| Some(Message::Size(w, h)))));
        let sensor = Node::Sensor {
            key: "boards/viewport".into(),
            reset: None,
            on_show: measure(),
            on_resize: measure(),
            on_hide: None,
            anticipate: None,
            delay: None,
            // The native sensor takes dimensions from its immediate child.
            child: Box::new(kit::sized(
                kit::container("boards/mouse-layout", mouse),
                Some(Length::Fill),
                Some(Length::Fill),
            )),
        };
        kit::sized(
            kit::container("boards/canvas-layout", sensor),
            Some(Length::Fill),
            Some(Length::Fill),
        )
    }
}
pub(super) fn anchor(from: &Shape, to: &Shape) -> [f32; 2] {
    let center = [
        from.x as f32 + from.width as f32 / 2.,
        from.y as f32 + from.height as f32 / 2.,
    ];
    let delta = [
        to.x as f32 + to.width as f32 / 2. - center[0],
        to.y as f32 + to.height as f32 / 2. - center[1],
    ];
    let scale = (from.width as f32 / 2. / delta[0].abs().max(0.001))
        .min(from.height as f32 / 2. / delta[1].abs().max(0.001));
    [center[0] + delta[0] * scale, center[1] + delta[1] * scale]
}

// Keep every visible card below its share of the host's 64 KiB text budget.
// The inspector retains the full text for editing.
fn excerpt(text: &str) -> &str {
    let mut end = text.len().min(256);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn ink(index: u8) -> [f32; 4] {
    let color = COLORS[index as usize];
    let shade = if kit::is_dark() { 0.9 } else { 0.45 };
    [color[0] * shade, color[1] * shade, color[2] * shade, 1.]
}

const TOOLS: [(Tool, &str, &str, &str); 6] = [
    (Tool::Select, "Select", "V", "select"),
    (Tool::Hand, "Pan", "H", "hand"),
    (Tool::Note, "Note", "N", "note"),
    (Tool::Rectangle, "Box", "R", "box"),
    (Tool::Text, "Text", "T", "text"),
    (Tool::Connect, "Connect", "A", "arrow"),
];
fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Note => "Sticky note",
        Kind::Rectangle => "Rectangle",
        Kind::Text => "Text",
        Kind::Arrow => "Connector",
    }
}
fn pin(key: &str, x: f32, y: f32, width: f32, height: Option<f32>, content: Node) -> Node {
    Node::Pin {
        key: key.into(),
        x,
        y,
        width: Some(Length::Fixed(width)),
        height: height.map(Length::Fixed),
        content: Box::new(content),
    }
}
fn panel(key: &str, content: Node) -> Node {
    let mut node = kit::card(key, content);
    if let Node::Container {
        padding,
        shadow,
        border,
        ..
    } = &mut node
    {
        *padding = Some(wire::Edges::all(10.));
        *shadow = wire::Shadow {
            color: Some(Rgba([0.02, 0.03, 0.08, 0.08])),
            x: Some(0.),
            y: Some(3.),
            blur: Some(14.),
        };
        if let Some(border) = border {
            border.radius = Some([12.; 4]);
        }
    }
    node
}
fn action(key: &str, label: &str, hint: &str, message: Message, enabled: bool) -> Node {
    let mut node = kit::button(
        key,
        label,
        enabled.then(|| slots::message(message)),
        ButtonPreset::Secondary,
    );
    if let Node::Button {
        description,
        height,
        padding,
        ..
    } = &mut node
    {
        *description = Some(hint.into());
        *height = Some(Length::Fixed(34.));
        *padding = Some(wire::Edges::all(7.));
    }
    node
}
fn icon(name: &str, selected: bool) -> Node {
    let path = match name {
        "select" => "<path d='m5 3 14 9-7 1-3 7z'/>",
        "hand" => {
            "<path d='M8 12V6a2 2 0 0 1 4 0v5-7a2 2 0 0 1 4 0v7-5a2 2 0 0 1 4 0v8c0 5-3 7-7 7-3 0-5-2-7-5l-3-4a2 2 0 0 1 3-2l2 2z'/>"
        }
        "note" => "<path d='M4 3h16v12l-5 6H4z'/><path d='M15 21v-6h5M8 8h8M8 12h5'/>",
        "box" => "<rect x='4' y='4' width='16' height='16' rx='3'/>",
        "text" => "<path d='M4 6V4h16v2M12 4v16M8 20h8'/>",
        "arrow" => "<path d='M4 19 20 4M10 4h10v10'/>",
        "lock" => {
            "<rect x='5' y='10' width='14' height='11' rx='3'/><path d='M8 10V7a4 4 0 0 1 8 0v3M12 14v3'/>"
        }
        _ => "",
    };
    let bytes=format!("<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='none' stroke='#222222' stroke-width='1.7' stroke-linecap='round' stroke-linejoin='round'>{path}</svg>").into_bytes();
    let hash = bytes.iter().fold(14695981039346656037u64, |h, b| {
        (h ^ *b as u64).wrapping_mul(1099511628211)
    });
    Node::Svg {
        key: format!("boards/icon/{name}"),
        inherit_button_ink: true,
        hash,
        bytes: Some(bytes),
        label: None,
        color: Some(Rgba(if selected {
            kit::palette().primary_foreground
        } else {
            kit::palette().foreground
        })),
        hover: None,
        fit: None,
        rotation: None,
        opacity: None,
        width: Some(Length::Fixed(21.)),
        height: Some(Length::Fixed(21.)),
    }
}
fn tool_button(tool: Tool, label: &str, key: &str, name: &str, selected: bool) -> Node {
    tool_button_message(label, key, name, Message::Tool(tool), selected)
}
fn tool_button_message(
    label: &str,
    key: &str,
    name: &str,
    message: Message,
    selected: bool,
) -> Node {
    let content = kit::column(
        format!("boards/tool-content/{name}"),
        [
            icon(name, selected),
            kit::text_size(kit::text(format!("boards/tool-key/{name}"), key), 9.),
        ],
    );
    let mut node = kit::button_child(
        format!("boards/tool/{name}"),
        content,
        Some(slots::message(message)),
        if selected {
            ButtonPreset::Primary
        } else {
            ButtonPreset::Subtle
        },
    );
    if let Node::Button {
        label: accessible,
        checked,
        width,
        height,
        padding,
        style,
        description,
        ..
    } = &mut node
    {
        *accessible = Some(label.into());
        *checked = Some(selected);
        *width = Some(Length::Fixed(48.));
        *height = Some(Length::Fixed(52.));
        *padding = Some(wire::Edges::all(6.));
        *description = Some(format!("{label} · {key}"));
        if selected {
            style.active.background = Some(Rgba([0.89, 0.88, 1., 1.]));
            style.active.text = Some(Rgba([0.31, 0.25, 0.79, 1.]));
        }
        style.active.border = Some(wire::Border {
            radius: Some([8.; 4]),
            width: Some(0.),
            color: None,
        });
    }
    Node::Tooltip {
        key: format!("boards/tool-tip/{name}"),
        position: wire::TooltipPosition::Bottom,
        gap: 8.,
        padding: 8.,
        delay_ms: 350,
        snap: false,
        style: Default::default(),
        children: vec![
            node,
            kit::text(
                format!("boards/tool-tip-text/{name}"),
                format!("{label}  {key}"),
            ),
        ],
    }
}
fn swatch(color: u8, selected: bool) -> Node {
    let name = ["Yellow", "Blue", "Green", "Purple", "Coral"][color as usize];
    let mut circle = kit::container(
        format!("boards/swatch-fill/{color}"),
        kit::space(None, None),
    );
    if let Node::Container {
        width,
        height,
        background,
        border,
        ..
    } = &mut circle
    {
        *width = Some(Length::Fixed(20.));
        *height = Some(Length::Fixed(20.));
        *background = Some(wire::Background::Color(Rgba(COLORS[color as usize])));
        *border = Some(wire::Border {
            radius: Some([10.; 4]),
            width: Some(if selected { 2. } else { 1. }),
            color: Some(Rgba(if selected {
                [0.4, 0.35, 0.94, 1.]
            } else {
                [0.1, 0.1, 0.1, 0.12]
            })),
        });
    }
    let mut node = kit::button_child(
        format!("boards/color/{color}"),
        circle,
        Some(slots::message(Message::Color(color))),
        ButtonPreset::Subtle,
    );
    if let Node::Button {
        label,
        width,
        height,
        padding,
        ..
    } = &mut node
    {
        *label = Some(name.into());
        *width = Some(Length::Fixed(28.));
        *height = Some(Length::Fixed(32.));
        *padding = Some(wire::Edges::all(2.));
    }
    node
}
impl BoardsView {
    fn inline_editor(&self, shape: &Shape, pos: [f32; 2], size: [f32; 2]) -> Node {
        use ducktape_view_guest::{EditorBinding, EditorTransactionEvent};
        use wire::keyboard::{Key, Named};
        let inline = self.inline.as_ref().expect("editing shape");
        let (document, on_document) = inline
            .document
            .document(format!("boards:text:{}", inline.id), Message::TextDocument);
        let binding = EditorBinding::new(
            vec![
                wire::EditorKeyClaim {
                    key: Key::Named(Named::Escape),
                    modifiers: Default::default(),
                    command: false,
                },
                wire::EditorKeyClaim {
                    key: Key::Named(Named::Enter),
                    modifiers: Default::default(),
                    command: true,
                },
            ],
            |_| wire::EditorDecision::Noop,
            |event| match event {
                EditorTransactionEvent::Commit { origin, .. } => {
                    origin.is_some().then_some(Message::FinishText)
                }
                _ => None,
            },
        )
        .register(std::convert::identity, Message::TextTransaction);
        let color = if shape.kind == Kind::Note {
            COLORS[shape.color as usize]
        } else {
            self.canvas_color()
        };
        let style = wire::InputStyle {
            active: wire::InputFace {
                background: Some(Rgba(color)),
                value: Some(Rgba(if shape.kind == Kind::Note {
                    [0.12, 0.13, 0.17, 1.]
                } else {
                    ink(shape.color)
                })),
                border: Some(wire::Border {
                    width: Some(0.),
                    radius: Some([0.; 4]),
                    color: None,
                }),
                ..Default::default()
            },
            focus_border: Some(Rgba([0.; 4])),
            ..Default::default()
        };
        let editor = Node::Editor {
            key: format!("boards/editor/{}", inline.id),
            document,
            on_document,
            editable: true,
            placeholder: "Write a thought…".into(),
            width: Some((size[0] - 24. * self.zoom).max(40.)),
            height: Some(Length::Fill),
            min_height: Some(32.),
            max_height: None,
            options: Box::new(wire::EditorOptions {
                binding: Some(Box::new(binding)),
                size: Some((17. * self.zoom).clamp(10., 51.)),
                padding: Some(0.),
                style,
                ..Default::default()
            }),
        };
        pin(
            "boards/editor-pin",
            pos[0] + 12. * self.zoom,
            pos[1] + 12. * self.zoom,
            (size[0] - 24. * self.zoom).max(40.),
            Some((size[1] - 24. * self.zoom).max(32.)),
            Node::Sensor {
                key: format!("boards/editor-mount/{}", inline.id),
                reset: None,
                on_show: Some(slots::handler(Box::new(|_: (f32, f32)| {
                    Some(Message::FocusText)
                }))),
                on_resize: None,
                on_hide: None,
                anticipate: None,
                delay: None,
                child: Box::new(kit::sized(
                    kit::container("boards/editor-layout", editor),
                    Some(Length::Fill),
                    Some(Length::Fill),
                )),
            },
        )
    }
}
