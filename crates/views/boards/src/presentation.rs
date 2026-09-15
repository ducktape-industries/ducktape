use crate::*;
use ducktape_view_guest::{
    kit, slots,
    wire::{
        self, ButtonPreset, CanvasCommand as Draw, CanvasShape as Geometry, Length, Node, Rgba,
    },
};

fn button(key: &str, label: &str, message: Message, enabled: bool) -> Node {
    kit::button(
        key,
        label,
        enabled.then(|| slots::message(message)),
        ButtonPreset::Secondary,
    )
}
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
        let idle = self.pending.is_empty();
        let mut sidebar = vec![
            kit::title("boards/title", "Boards"),
            kit::caption("boards/tagline", "A place to think together"),
            kit::input(
                "boards/new-title",
                "New board name",
                &self.title,
                slots::handler(Box::new(|text| Some(Message::Title(text)))),
                Some(slots::message(Message::CreateBoard)),
            ),
            button(
                "boards/new",
                "Create board",
                Message::CreateBoard,
                self.session.connected && idle && !self.title.trim().is_empty(),
            ),
        ];
        for (id, title) in &self.catalog {
            let mut entry = button(
                &format!("boards/open/{id}"),
                title,
                Message::Open(id.clone()),
                idle,
            );
            if let Node::Button { checked, .. } = &mut entry {
                *checked = Some(id == &self.current);
            }
            sidebar.push(entry);
        }
        sidebar.push(kit::caption("boards/shared", "Shared with this network"));
        let sidebar = kit::sized(
            kit::scroll("boards/list", kit::column("boards/sidebar", sidebar)),
            Some(Length::Fixed(190.)),
            Some(Length::Fill),
        );
        let mut content = Vec::new();
        let title = self
            .confirmed
            .as_ref()
            .map_or("Your workspace canvas", |board| board.title.as_str());
        content.push(kit::centered_row(
            "boards/header",
            [
                kit::sized(
                    kit::heading("boards/board-title", title),
                    Some(Length::Fill),
                    None,
                ),
                kit::caption("boards/sync", self.status()),
            ],
        ));
        if !self.session.connected {
            content.push(kit::empty_state(
                "boards/offline",
                "Not connected",
                "Choose a network to open its boards. Pending edits stay on this device.",
            ));
        }
        if !self.error.is_empty() {
            content.push(kit::tone_text(
                "boards/error",
                &self.error,
                kit::Tone::Danger,
            ));
        }
        if let Delivery::Failed(error) = &self.delivery {
            content.push(kit::tone_text(
                "boards/save-error",
                format!("Edits are kept on this device: {error}"),
                kit::Tone::Danger,
            ));
            content.push(button(
                "boards/discard",
                "Use saved board",
                Message::DiscardPending,
                true,
            ));
            content.push(button(
                "boards/retry",
                "Retry saving",
                Message::Retry,
                self.session.connected,
            ));
        }
        let Some(board) = self.visible() else {
            content.push(kit::empty_state(
                "boards/empty",
                "Make room for an idea",
                "Create a board, add notes, and connect them. Everyone on this network can edit.",
            ));
            return kit::page(
                "boards/root",
                [kit::sized(
                    kit::row(
                        "boards/layout",
                        [sidebar, kit::column("boards/content", content)],
                    ),
                    None,
                    Some(Length::Fill),
                )],
            );
        };
        let mut tools = Vec::new();
        for (tool, label) in [
            (Tool::Select, "Select"),
            (Tool::Hand, "Pan"),
            (Tool::Note, "Note"),
            (Tool::Rectangle, "Box"),
            (Tool::Text, "Text"),
            (Tool::Connect, "Connect"),
        ] {
            let mut control = button(
                &format!("boards/tool/{tool:?}"),
                label,
                Message::Tool(tool),
                true,
            );
            if let Node::Button { checked, .. } = &mut control {
                *checked = Some(self.tool == tool);
            }
            tools.push(control);
        }
        tools.extend([
            button("boards/undo", "Undo", Message::Undo, !self.undo.is_empty()),
            button("boards/redo", "Redo", Message::Redo, !self.redo.is_empty()),
            button("boards/zoom-out", "−", Message::Zoom(0.8), true),
            kit::caption("boards/zoom", format!("{}%", (self.zoom * 100.).round())),
            button("boards/zoom-in", "+", Message::Zoom(1.25), true),
            button("boards/fit", "Fit", Message::Fit, true),
        ]);
        content.push(kit::wrapped_row("boards/tools", tools));
        content.push(self.canvas(&board));
        if let Some(id) = &self.selected
            && let Some(record) = board.shapes.get(id)
        {
            let mut inspector = vec![
                kit::input(
                    "boards/text",
                    "Write a thought…",
                    &self.draft,
                    slots::handler(Box::new(|text| Some(Message::Draft(text)))),
                    Some(slots::message(Message::ApplyText)),
                ),
                button(
                    "boards/apply",
                    "Apply text",
                    Message::ApplyText,
                    self.draft != record.shape.text,
                ),
                button("boards/delete", "Delete", Message::Delete, true),
            ];
            for (color, label) in [
                (0, "Yellow"),
                (1, "Blue"),
                (2, "Green"),
                (3, "Purple"),
                (4, "Coral"),
            ] {
                let mut swatch = button(
                    &format!("boards/color/{color}"),
                    label,
                    Message::Color(color),
                    true,
                );
                if let Node::Button { checked, .. } = &mut swatch {
                    *checked = Some(record.shape.color == color);
                }
                inspector.push(swatch);
            }
            content.push(kit::wrapped_row("boards/inspector", inspector));
        }
        let hint = match self.tool {
            Tool::Select => {
                "Drag to move · Drag the bottom-right corner to resize · Edit text below"
            }
            Tool::Hand => "Drag the canvas to pan · Scroll to pan · Use + / − to zoom",
            Tool::Connect => {
                if self.connection.is_some() {
                    "Now choose the destination card"
                } else {
                    "Choose two cards to connect"
                }
            }
            Tool::Note | Tool::Rectangle | Tool::Text => "Click the canvas to place a new card",
        };
        content.push(kit::caption(
            "boards/hint",
            format!(
                "{hint} · {} / {} shapes",
                board.shapes.len(),
                boards::MAX_SHAPES
            ),
        ));
        kit::page(
            "boards/root",
            [kit::sized(
                kit::row(
                    "boards/layout",
                    [
                        sidebar,
                        kit::sized(
                            kit::column("boards/content", content),
                            Some(Length::Fill),
                            Some(Length::Fill),
                        ),
                    ],
                ),
                None,
                Some(Length::Fill),
            )],
        )
    }
    fn status(&self) -> String {
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
        let p = kit::palette();
        let muted = kit::rgba(p.border);
        let accent = kit::rgba(p.accent_foreground);
        let mut commands = Vec::new();
        let mut labels = Vec::new();
        // A bounded grid remains coarse at low zoom, avoiding a frame-sized mesh.
        let spacing = (32. * self.zoom).max(24.);
        for i in 0..((self.viewport[0] / spacing).ceil() as usize).min(160) {
            let x = self.camera[0].rem_euclid(spacing) + i as f32 * spacing;
            commands.push(line([x, 0.], [x, self.viewport[1]], muted, 0.4));
        }
        for i in 0..((self.viewport[1] / spacing).ceil() as usize).min(100) {
            let y = self.camera[1].rem_euclid(spacing) + i as f32 * spacing;
            commands.push(line([0., y], [self.viewport[0], y], muted, 0.4));
        }
        for (id, record) in &board.shapes {
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
            let color = if self.selected.as_ref() == Some(id) {
                accent
            } else {
                kit::rgba(p.muted)
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
        for (id, record) in &board.shapes {
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
            let selected =
                self.selected.as_ref() == Some(id) || self.connection.as_ref() == Some(id);
            let fill = match s.kind {
                Kind::Note => Some(Rgba(COLORS[s.color as usize])),
                Kind::Rectangle => Some(kit::rgba(p.surface)),
                Kind::Text | Kind::Arrow => None,
            };
            let border = if selected { accent } else { muted };
            commands.push(rectangle(
                pos,
                size,
                fill,
                border,
                if selected { 2. } else { 1. },
            ));
            if selected {
                commands.push(rectangle(
                    [pos[0] + size[0] - 8., pos[1] + size[1] - 8.],
                    [8., 8.],
                    Some(accent),
                    accent,
                    1.,
                ));
            }
            let text = if s.text.is_empty() {
                "Write a thought…"
            } else {
                excerpt(&s.text)
            };
            let mut label = kit::text_size(
                kit::wrapping(kit::text(format!("boards/label/{id}"), text)),
                (15. * self.zoom).clamp(8., 45.),
            );
            if s.kind == Kind::Note {
                label = kit::colored(label, [0.16, 0.15, 0.12, 1.]);
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
            background: Some(kit::rgba(p.background)),
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
            on_double_click: None,
            on_right_press: None,
            on_right_release: None,
            on_middle_press: None,
            on_middle_release: None,
            on_enter: None,
            content: Box::new(scene),
        };
        let measure = || Some(slots::handler(Box::new(|(w, h)| Some(Message::Size(w, h)))));
        Node::Sensor {
            key: "boards/viewport".into(),
            reset: None,
            on_show: measure(),
            on_resize: measure(),
            on_hide: None,
            anticipate: None,
            delay: None,
            child: Box::new(mouse),
        }
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
