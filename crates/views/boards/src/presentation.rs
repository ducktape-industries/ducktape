use crate::*;
use ducktape_view_guest::{
    kit, slots,
    wire::{
        self, AlignX, AlignY, ButtonPreset, CanvasCommand as Draw, CanvasShape as Geometry, Length,
        Node, Rgba,
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
            radius: [6.; 4],
        },
        fill,
        even_odd: false,
        stroke: Some(stroke(border, width)),
    }
}
/// The five card hues, softened onto the app's cool greys: a fill per
/// appearance and the tint its border, connector and ink are drawn in.
const LIGHT_FILLS: [[f32; 4]; 5] = [
    [0.99, 0.95, 0.80, 1.],
    [0.87, 0.91, 0.99, 1.],
    [0.87, 0.95, 0.90, 1.],
    [0.92, 0.90, 0.99, 1.],
    [0.99, 0.90, 0.88, 1.],
];
const DARK_FILLS: [[f32; 4]; 5] = [
    [0.33, 0.30, 0.16, 1.],
    [0.17, 0.22, 0.34, 1.],
    [0.16, 0.29, 0.22, 1.],
    [0.26, 0.22, 0.36, 1.],
    [0.36, 0.20, 0.19, 1.],
];
const LIGHT_TINTS: [[f32; 4]; 5] = [
    [0.62, 0.50, 0.10, 1.],
    [0.25, 0.42, 0.75, 1.],
    [0.20, 0.52, 0.32, 1.],
    [0.48, 0.36, 0.72, 1.],
    [0.78, 0.32, 0.28, 1.],
];
const DARK_TINTS: [[f32; 4]; 5] = [
    [0.90, 0.78, 0.40, 1.],
    [0.55, 0.70, 0.98, 1.],
    [0.50, 0.82, 0.62, 1.],
    [0.75, 0.65, 0.98, 1.],
    [0.98, 0.62, 0.58, 1.],
];
fn fill(index: u8) -> [f32; 4] {
    let fills = if kit::is_dark() {
        DARK_FILLS
    } else {
        LIGHT_FILLS
    };
    fills[index as usize]
}
fn tint(index: u8) -> [f32; 4] {
    let tints = if kit::is_dark() {
        DARK_TINTS
    } else {
        LIGHT_TINTS
    };
    tints[index as usize]
}
fn alpha(mut color: [f32; 4], alpha: f32) -> [f32; 4] {
    color[3] = alpha;
    color
}
/// The inset a card keeps around its text, in board units.
const CARD_INSET: f32 = 12.;
/// The inset every island keeps from the stage's edge.
const ISLAND: f32 = 12.;
/// The side of an icon-only tool.
const TOOL: f32 = 36.;
impl BoardsView {
    /// The canvas edge to edge, and over it the islands a canvas app keeps
    /// at its corners: the board menu top-left, the tools top-centre, the
    /// inspector top-right, the camera and history bottom-left, help
    /// bottom-right, the tool hint along the bottom edge.
    pub(crate) fn view(&self) -> Node {
        kit::set_dark(self.session.dark);
        let board = self.visible();
        let w = self.viewport[0].max(640.);
        let h = self.viewport[1].max(480.);
        let compact = w < 960.;
        let editing = self.inline.is_some();
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
        if let Some(board) = &board {
            let hint_shown = !editing && !compact;
            if hint_shown {
                layers.push(pin(
                    "boards/hint-pin",
                    0.,
                    h - 26.,
                    w,
                    centered_caption("boards/hint", self.hint()),
                ));
            }
            layers.extend(self.empty_prompt(board));
        }
        let mut stage = Node::Stack {
            key: "boards/stage".into(),
            width: Some(Length::Fill),
            height: Some(Length::Fill),
            padding: None,
            background: Some(Rgba(self.canvas_color())),
            border: None,
            clip: true,
            under: 0,
            children: layers,
        };
        // Every island sits on an overlay, never a pin: the host stops a
        // press on an overlay's surface, where a pinned card lets it fall
        // through to the canvas underneath and the gesture it starts
        // re-renders the card out from under the click.
        let picker_open = self.board_picker || board.is_none();
        let dismiss = (picker_open && board.is_some()).then_some(Message::BoardPicker);
        stage = float(
            "boards/menu-float",
            stage,
            self.menu_island(&board, picker_open),
            AlignX::Left,
            AlignY::Top,
            ISLAND,
            dismiss,
        );
        if let Some(board) = &board {
            let tools_y = if compact { AlignY::Bottom } else { AlignY::Top };
            stage = float(
                "boards/tools-float",
                stage,
                self.tool_island(),
                AlignX::Center,
                tools_y,
                ISLAND,
                None,
            );
            if let Some(inspector) = self.inspector_island(board) {
                stage = float(
                    "boards/properties-float",
                    stage,
                    inspector,
                    AlignX::Right,
                    AlignY::Top,
                    ISLAND,
                    None,
                );
            }
            stage = float(
                "boards/camera-float",
                stage,
                self.camera_island(),
                AlignX::Left,
                AlignY::Bottom,
                ISLAND,
                None,
            );
            stage = float(
                "boards/help-float",
                stage,
                self.help_island(),
                AlignX::Right,
                AlignY::Bottom,
                ISLAND,
                None,
            );
            if let Some(inline) = &self.inline {
                let strip_y = if compact { AlignY::Top } else { AlignY::Bottom };
                stage = float(
                    "boards/typing-float",
                    stage,
                    self.typing_strip(inline),
                    AlignX::Center,
                    strip_y,
                    ISLAND,
                    None,
                );
            }
        }
        if let Some(notice) = self.notice() {
            let card = kit::sized(notice, Some(Length::Fixed(480.)), None);
            stage = float(
                "boards/notice-float",
                stage,
                card,
                AlignX::Center,
                AlignY::Top,
                ISLAND + TOOL + 20.,
                None,
            );
        }
        if self.help {
            let card = kit::sized(self.help_card(), Some(Length::Fixed(440.)), None);
            stage = modal("boards/help-modal", stage, card, Message::Help);
        }
        stage
    }
    /// Top-left: the board's name and its save state; open, the board list.
    fn menu_island(&self, board: &Option<Board>, open: bool) -> Node {
        let title = board.as_ref().map_or("Boards", |b| b.title.as_str());
        let head = kit::spaced(
            kit::centered_row(
                "boards/menu-head",
                [
                    action(
                        "boards/switcher",
                        &format!("{title}  ▾"),
                        "Choose a board",
                        Message::BoardPicker,
                        board.is_some(),
                    ),
                    kit::nowrap(kit::caption("boards/sync", self.status())),
                ],
            ),
            6.,
        );
        if !open {
            return island("boards/menu", head);
        }
        let mut rows = vec![head, kit::divider("boards/menu-rule")];
        rows.extend(self.picker());
        kit::sized(
            island(
                "boards/menu",
                kit::spaced(kit::column("boards/menu-body", rows), 6.),
            ),
            Some(Length::Fixed(260.)),
            None,
        )
    }
    /// Top-centre: the lock, then the tools, icon-only with their keys.
    fn tool_island(&self) -> Node {
        let mut tools = vec![
            tool_button_message(
                "Keep tool",
                "Q",
                "lock",
                Message::LockTool,
                self.tool_locked,
            ),
            rule("boards/tools-rule"),
        ];
        for (tool, label, key, icon_name) in TOOLS {
            tools.push(tool_button(tool, label, key, icon_name, self.tool == tool));
        }
        island(
            "boards/tools",
            kit::spaced(kit::centered_row("boards/tools-row", tools), 2.),
        )
    }
    /// Bottom-left: the camera, then history, then snapping.
    fn camera_island(&self) -> Node {
        let controls = [
            action("boards/zoom-out", "−", "Zoom out", Message::Zoom(0.8), true),
            kit::sized(
                action(
                    "boards/zoom",
                    &format!("{}%", (self.zoom * 100.).round()),
                    "Reset zoom · 0",
                    Message::ResetZoom,
                    true,
                ),
                Some(Length::Fixed(52.)),
                None,
            ),
            action("boards/zoom-in", "+", "Zoom in", Message::Zoom(1.25), true),
            action("boards/fit", "Fit", "Fit board · F", Message::Fit, true),
            rule("boards/camera-rule-a"),
            icon_button(
                "boards/undo",
                "undo",
                "Undo",
                "⌘ / Ctrl Z",
                Message::Undo,
                !self.undo.is_empty(),
                false,
            ),
            icon_button(
                "boards/redo",
                "redo",
                "Redo",
                "⌘ / Ctrl Shift Z",
                Message::Redo,
                !self.redo.is_empty(),
                false,
            ),
            rule("boards/camera-rule-b"),
            checked(
                "boards/snap",
                "Snap",
                "Snap to other cards · hold Alt to bypass",
                Message::Snap,
                self.snap,
            ),
        ];
        island(
            "boards/camera",
            kit::spaced(kit::centered_row("boards/camera-row", controls), 2.),
        )
    }
    /// Bottom-right: the keyboard help.
    fn help_island(&self) -> Node {
        island(
            "boards/help-button",
            icon_button(
                "boards/help",
                "help",
                "Keyboard shortcuts",
                "?",
                Message::Help,
                true,
                self.help,
            ),
        )
    }
    /// Top-right, while cards are chosen: what can be done to them.
    fn inspector_island(&self, board: &Board) -> Option<Node> {
        let count = self.selected.len();
        let shown = count > 0 && self.inline.is_none();
        if !shown {
            return None;
        }
        let name = if count == 1 {
            self.only_selected()
                .and_then(|id| board.shapes.get(id))
                .map_or("Selection", |r| kind_name(r.shape.kind))
                .to_owned()
        } else {
            format!("{count} selected")
        };
        Some(kit::sized(
            self.inspector(name, count),
            Some(Length::Fixed(204.)),
            None,
        ))
    }
    /// Along the bottom while a card is being written: the newline rule,
    /// the length, and the way out.
    fn typing_strip(&self, inline: &Inline) -> Node {
        let length = inline.document.text().len();
        let strip = island(
            "boards/typing",
            kit::spaced(
                kit::centered_row(
                    "boards/typing-row",
                    [
                        kit::nowrap(kit::caption(
                            "boards/typing-hint",
                            format!("Enter for a new line · {length}/{}", boards::MAX_TEXT),
                        )),
                        kit::spacer(),
                        action(
                            "boards/done",
                            "Done",
                            "⌘ / Ctrl Enter",
                            Message::FinishText,
                            true,
                        ),
                    ],
                ),
                8.,
            ),
        );
        kit::sized(strip, Some(Length::Fixed(280.)), None)
    }
    /// The board list under the switcher: a name to create, the rest to open.
    fn picker(&self) -> Vec<Node> {
        let idle = self.pending.is_empty() && self.inline.is_none();
        let mut list = vec![
            kit::input(
                "boards/new-title",
                "Name a new board",
                &self.title,
                slots::handler(Box::new(|s| Some(Message::Title(s)))),
                Some(slots::message(Message::CreateBoard)),
            ),
            wide(button(
                "boards/new",
                "Create board",
                "Create a shared board",
                Message::CreateBoard,
                self.session.connected && idle && !self.title.trim().is_empty(),
                ButtonPreset::Primary,
            )),
        ];
        if !self.catalog.is_empty() {
            list.push(kit::divider("boards/picker-rule"));
            list.push(kit::caption("boards/shared", "Shared with this network"));
        }
        list.extend(self.catalog.iter().map(|(id, title)| {
            wide(action(
                &format!("boards/open/{id}"),
                title,
                "Open board",
                Message::Open(id.clone()),
                idle,
            ))
        }));
        list
    }
    fn hint(&self) -> &'static str {
        match self.tool {
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
        }
    }
    /// The prompt an empty board shows, centred on the stage.
    fn empty_prompt(&self, board: &Board) -> Option<Node> {
        if !board.shapes.is_empty() {
            return None;
        }
        let empty = kit::spaced(
            kit::column(
                "boards/empty-body",
                [
                    kit::empty_state(
                        "boards/empty",
                        "Make space for your next idea",
                        "Drop a note, sketch a flow, or start with a simple layout.",
                    ),
                    kit::padded(
                        kit::spaced(
                            kit::row(
                                "boards/empty-actions",
                                [
                                    button(
                                        "boards/first-note",
                                        "Add a note",
                                        "N · Click anywhere",
                                        Message::QuickNote,
                                        true,
                                        ButtonPreset::Primary,
                                    ),
                                    action(
                                        "boards/template",
                                        "Start a brainstorm",
                                        "Ideas, questions, next steps",
                                        Message::Template,
                                        true,
                                    ),
                                ],
                            ),
                            8.,
                        ),
                        wire::Edges {
                            top: 0.,
                            right: 24.,
                            bottom: 24.,
                            left: 24.,
                        },
                    ),
                ],
            ),
            0.,
        );
        Some(float(
            "boards/empty-float",
            kit::space(Some(Length::Fill), Some(Length::Fill)),
            kit::sized(
                kit::card("boards/empty-card", empty),
                Some(Length::Fixed(420.)),
                None,
            ),
            AlignX::Center,
            AlignY::Center,
            24.,
            None,
        ))
    }
    fn inspector(&self, name: String, count: usize) -> Node {
        let mut properties = vec![
            kit::heading("boards/selection-title", name),
            kit::spaced(
                kit::row(
                    "boards/colors",
                    (0..5).map(|color| swatch(color, self.palette == color)),
                ),
                4.,
            ),
            kit::divider("boards/properties-rule"),
        ];
        if count == 1 {
            properties.push(wide(action(
                "boards/edit-text",
                "Edit text",
                "Enter · double-click",
                Message::EditText,
                true,
            )));
        }
        properties.push(wide(action(
            "boards/duplicate",
            "Duplicate",
            "⌘ / Ctrl D",
            Message::Duplicate,
            true,
        )));
        if count > 1 {
            properties.push(wide(action(
                "boards/align-left",
                "Align left",
                "Align selected cards",
                Message::Align(false),
                true,
            )));
            properties.push(wide(action(
                "boards/align-top",
                "Align top",
                "Align selected cards",
                Message::Align(true),
                true,
            )));
        }
        properties.push(wide(button(
            "boards/delete",
            "Delete",
            "Delete / Backspace",
            Message::Delete,
            true,
            ButtonPreset::Danger,
        )));
        kit::card(
            "boards/properties",
            kit::spaced(kit::column("boards/properties-body", properties), 6.),
        )
    }
    fn notice(&self) -> Option<Node> {
        let notice = match &self.delivery {
            Delivery::Failed(error) => format!("Your changes are kept here. {error}"),
            _ => {
                if self.error.is_empty() {
                    return None;
                }
                self.error.clone()
            }
        };
        let mut children = vec![kit::wrapping(kit::text("boards/error", notice))];
        if matches!(self.delivery, Delivery::Failed(_)) {
            children.push(kit::spaced(
                kit::row(
                    "boards/notice-actions",
                    [
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
                    ],
                ),
                8.,
            ));
        }
        Some(kit::notice(
            "boards/notice",
            kit::spaced(kit::column("boards/notice-body", children), 8.),
            kit::Tone::Danger,
        ))
    }
    fn help_card(&self) -> Node {
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
        let mut rows = vec![
            kit::heading("boards/help-title", "Keyboard shortcuts"),
            kit::divider("boards/help-rule"),
        ];
        rows.extend(shortcuts.into_iter().enumerate().map(|(i, (key, label))| {
            kit::centered_row(
                format!("boards/key/{i}"),
                [
                    kit::sized(
                        kit::nowrap(kit::text(format!("boards/key-label/{i}"), label)),
                        Some(Length::Fill),
                        None,
                    ),
                    kit::nowrap(kit::mono(format!("boards/key-combo/{i}"), key)),
                ],
            )
        }));
        rows.push(kit::divider("boards/help-rule-b"));
        rows.push(wide(action(
            "boards/help-close",
            "Got it",
            "Close shortcuts",
            Message::Help,
            true,
        )));
        kit::card(
            "boards/help-panel",
            kit::spaced(kit::column("boards/help-body", rows), 6.),
        )
    }
    fn canvas_color(&self) -> [f32; 4] {
        kit::palette().background
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
        let p = kit::palette();
        let muted = Rgba(p.border_strong);
        let accent = Rgba(p.accent);
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
                Rgba(tint(s.color))
            };
            commands.push(line(start, end, color, 1.5));
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
                    1.5,
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
                Kind::Note => Some(Rgba(fill(s.color))),
                Kind::Rectangle => Some(Rgba(alpha(fill(s.color), 0.45))),
                Kind::Text | Kind::Arrow => None,
            };
            let border = match (selected, s.kind) {
                (true, _) => accent,
                (false, Kind::Note) => Rgba(alpha(tint(s.color), 0.35)),
                (false, _) => Rgba(tint(s.color)),
            };
            if s.kind != Kind::Text || selected {
                commands.push(rectangle(
                    pos,
                    size,
                    fill,
                    border,
                    if selected { 1.5 } else { 1. },
                ));
            }
            if selected && self.inline.is_none() && self.selected.len() == 1 {
                for corner in [[-1, -1], [1, -1], [-1, 1], [1, 1]] {
                    let world = interaction::corner_point(s, corner);
                    let handle = self.screen(world[0], world[1]);
                    commands.push(Draw::Draw {
                        shape: Geometry::Rectangle {
                            position: [handle[0] - 3.5, handle[1] - 3.5],
                            size: [7., 7.],
                            radius: [1.5; 4],
                        },
                        fill: Some(Rgba(p.background)),
                        even_odd: false,
                        stroke: Some(stroke(accent, 1.5)),
                    });
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
            let label = kit::colored(
                kit::text_size(
                    kit::wrapping(kit::text(format!("boards/label/{id}"), text)),
                    (if s.kind == Kind::Text { 20. } else { 14. } * self.zoom).clamp(8., 60.),
                ),
                if s.text.is_empty() {
                    p.faint
                } else {
                    p.foreground
                },
            );
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
                x: pos[0] + CARD_INSET * self.zoom,
                y: pos[1] + CARD_INSET * self.zoom,
                width: Some(Length::Fixed(
                    (size[0] - 2. * CARD_INSET * self.zoom).max(1.),
                )),
                height: Some(Length::Fixed(
                    (size[1] - 2. * CARD_INSET * self.zoom).max(1.),
                )),
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
                Some(Rgba(alpha(p.accent, 0.08))),
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
                Some(Rgba(alpha(p.accent, 0.08))),
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
fn pin(key: &str, x: f32, y: f32, width: f32, content: Node) -> Node {
    Node::Pin {
        key: key.into(),
        x,
        y,
        width: Some(Length::Fixed(width)),
        height: None,
        content: Box::new(content),
    }
}
/// A card over the stage: the host's overlay, whose surface keeps a press
/// from reaching the canvas under it and whose layer, given `dismiss`, closes
/// the card on a press anywhere else.
fn float(
    key: &str,
    base: Node,
    card: Node,
    align_x: AlignX,
    align_y: AlignY,
    inset: f32,
    dismiss: Option<Message>,
) -> Node {
    Node::Overlay {
        key: key.into(),
        padding: inset,
        backdrop: Rgba([0.; 4]),
        align_x,
        align_y,
        on_dismiss: dismiss.map(slots::message),
        children: vec![base, card],
    }
}
/// An island: a card at a stage corner, its controls packed tight.
fn island(key: &str, content: Node) -> Node {
    kit::padded(kit::card(key, content), wire::Edges::all(4.))
}
/// A card over a shaded stage that a press anywhere else closes.
fn modal(key: &str, base: Node, card: Node, dismiss: Message) -> Node {
    Node::Overlay {
        key: key.into(),
        padding: 24.,
        backdrop: Rgba([0., 0., 0., 0.18]),
        align_x: AlignX::Center,
        align_y: AlignY::Center,
        on_dismiss: Some(slots::message(dismiss)),
        children: vec![base, card],
    }
}
/// A caption centred across the width it is given.
fn centered_caption(key: &str, content: &str) -> Node {
    let mut node = kit::nowrap(kit::caption(key, content));
    if let Node::Text { align_x, .. } = &mut node {
        *align_x = Some(AlignX::Center);
    }
    node
}
/// A short vertical hairline between two clusters of a bar.
fn rule(key: &str) -> Node {
    kit::sized(
        kit::container(format!("{key}/box"), kit::vertical_divider(key)),
        None,
        Some(Length::Fixed(16.)),
    )
}
fn wide(node: Node) -> Node {
    kit::sized(node, Some(Length::Fill), None)
}
fn button(
    key: &str,
    label: &str,
    hint: &str,
    message: Message,
    enabled: bool,
    preset: ButtonPreset,
) -> Node {
    let mut node = kit::button(key, label, enabled.then(|| slots::message(message)), preset);
    if let Node::Button {
        description,
        height,
        padding,
        ..
    } = &mut node
    {
        *description = Some(hint.into());
        *height = Some(Length::Fixed(28.));
        *padding = Some(wire::Edges {
            top: 0.,
            right: 8.,
            bottom: 0.,
            left: 8.,
        });
    }
    node
}
/// A quiet action: the bar's and the inspector's default control.
fn action(key: &str, label: &str, hint: &str, message: Message, enabled: bool) -> Node {
    button(key, label, hint, message, enabled, ButtonPreset::Subtle)
}
/// A quiet action that reads as on or off.
fn checked(key: &str, label: &str, hint: &str, message: Message, on: bool) -> Node {
    let mut node = action(key, label, hint, message, true);
    if let Node::Button { checked, .. } = &mut node {
        *checked = Some(on);
    }
    node
}
fn icon(name: &str) -> Node {
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
        "undo" => "<path d='M9 14 4 9l5-5'/><path d='M4 9h10a5 5 0 0 1 0 10h-3'/>",
        "redo" => "<path d='m15 14 5-5-5-5'/><path d='M20 9H10a5 5 0 0 0 0 10h3'/>",
        "help" => {
            "<circle cx='12' cy='12' r='9'/><path d='M9.5 9.5a2.5 2.5 0 1 1 3.5 2.3c-.7.4-1 1-1 1.7M12 17h.01'/>"
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
        color: None,
        hover: None,
        fit: None,
        rotation: None,
        opacity: None,
        width: Some(Length::Fixed(16.)),
        height: Some(Length::Fixed(16.)),
    }
}
fn tool_button(tool: Tool, label: &str, key: &str, name: &str, selected: bool) -> Node {
    tool_button_message(label, key, name, Message::Tool(tool), selected)
}
/// An icon-only tool: a `TOOL` square, checked when chosen, its key in the
/// corner the way a canvas app prints it, its name in the tooltip.
fn tool_button_message(
    label: &str,
    key: &str,
    name: &str,
    message: Message,
    selected: bool,
) -> Node {
    let p = kit::palette();
    let hint = kit::weighted(
        kit::colored(
            kit::text_size(
                kit::nowrap(kit::text(format!("boards/tool-key/{name}"), key)),
                9.,
            ),
            if selected {
                p.accent_foreground
            } else {
                p.muted
            },
        ),
        wire::Weight::Medium,
    );
    let mut glyph = kit::container(format!("boards/tool-glyph/{name}"), icon(name));
    if let Node::Container {
        align_x,
        align_y,
        height,
        padding,
        ..
    } = &mut glyph
    {
        *align_x = Some(AlignX::Center);
        *align_y = Some(AlignY::Center);
        *height = Some(Length::Fill);
        // the glyph sits a little up and left of centre, clear of its key
        *padding = Some(wire::Edges {
            top: 0.,
            right: 6.,
            bottom: 6.,
            left: 0.,
        });
    }
    let face = Node::Stack {
        key: format!("boards/tool-face/{name}"),
        width: Some(Length::Fill),
        height: Some(Length::Fill),
        padding: None,
        background: None,
        border: None,
        clip: false,
        under: 0,
        children: vec![
            glyph,
            Node::Pin {
                key: format!("boards/tool-key-pin/{name}"),
                x: TOOL - 13.,
                y: TOOL - 16.,
                width: Some(Length::Fixed(12.)),
                height: None,
                content: Box::new(hint),
            },
        ],
    };
    let mut node = kit::button_child(
        format!("boards/tool/{name}"),
        face,
        Some(slots::message(message)),
        ButtonPreset::Subtle,
    );
    if let Node::Button {
        label: accessible,
        checked,
        width,
        height,
        padding,
        description,
        ..
    } = &mut node
    {
        *accessible = Some(label.into());
        *checked = Some(selected);
        *width = Some(Length::Fixed(TOOL));
        *height = Some(Length::Fixed(TOOL));
        *padding = Some(wire::Edges::all(0.));
        *description = Some(format!("{label} · {key}"));
    }
    Node::Tooltip {
        key: format!("boards/tool-tip/{name}"),
        position: wire::TooltipPosition::Bottom,
        gap: 6.,
        padding: 6.,
        delay_ms: 350,
        snap: false,
        style: Default::default(),
        children: vec![
            node,
            kit::caption(
                format!("boards/tool-tip-text/{name}"),
                format!("{label} · {key}"),
            ),
        ],
    }
}
/// A 28px icon-only control with a name and a hint, checked when `on`.
fn icon_button(
    key: &str,
    name: &str,
    label: &str,
    hint: &str,
    message: Message,
    enabled: bool,
    on: bool,
) -> Node {
    let mut node = kit::button_child(
        key,
        icon(name),
        enabled.then(|| slots::message(message)),
        ButtonPreset::Subtle,
    );
    if let Node::Button {
        label: accessible,
        checked,
        width,
        height,
        padding,
        description,
        ..
    } = &mut node
    {
        *accessible = Some(label.into());
        *checked = Some(on);
        *width = Some(Length::Fixed(28.));
        *height = Some(Length::Fixed(28.));
        *padding = Some(wire::Edges::all(6.));
        *description = Some(hint.into());
    }
    node
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
        *width = Some(Length::Fixed(16.));
        *height = Some(Length::Fixed(16.));
        *background = Some(wire::Background::Color(Rgba(fill(color))));
        *border = Some(wire::Border {
            radius: Some([8.; 4]),
            width: Some(if selected { 2. } else { 1. }),
            color: Some(Rgba(if selected {
                kit::palette().accent
            } else {
                alpha(tint(color), 0.5)
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
        *width = Some(Length::Fixed(24.));
        *height = Some(Length::Fixed(24.));
        *padding = Some(wire::Edges::all(0.));
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
            fill(shape.color)
        } else {
            self.canvas_color()
        };
        let style = wire::InputStyle {
            active: wire::InputFace {
                background: Some(Rgba(color)),
                value: Some(Rgba(kit::palette().foreground)),
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
            width: Some((size[0] - 2. * CARD_INSET * self.zoom).max(40.)),
            height: Some(Length::Fill),
            min_height: Some(32.),
            max_height: None,
            options: Box::new(wire::EditorOptions {
                binding: Some(Box::new(binding)),
                size: Some((14. * self.zoom).clamp(10., 42.)),
                padding: Some(0.),
                style,
                ..Default::default()
            }),
        };
        let inset = CARD_INSET * self.zoom;
        Node::Pin {
            key: "boards/editor-pin".into(),
            x: pos[0] + inset,
            y: pos[1] + inset,
            width: Some(Length::Fixed((size[0] - 2. * inset).max(40.))),
            height: Some(Length::Fixed((size[1] - 2. * inset).max(32.))),
            content: Box::new(Node::Sensor {
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
            }),
        }
    }
}
