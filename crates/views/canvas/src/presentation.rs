use crate::*;
use ducktape_view_guest::{
    kit, slots,
    wire::{
        self, AlignX, AlignY, ButtonPreset, CanvasCommand as Draw, CanvasShape as Geometry, Length,
        Node, Rgba,
    },
};

fn pen(color: Rgba, width: f32) -> wire::CanvasStroke {
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
        stroke: Some(pen(color, width)),
    }
}
fn rectangle(
    position: [f32; 2],
    size: [f32; 2],
    fill: Option<Rgba>,
    border: Rgba,
    width: f32,
    radius: f32,
) -> Draw {
    Draw::Draw {
        shape: Geometry::Rectangle {
            position,
            size,
            radius: [radius; 4],
        },
        fill,
        even_odd: false,
        stroke: Some(pen(border, width)),
    }
}
fn ellipse(
    position: [f32; 2],
    size: [f32; 2],
    fill: Option<Rgba>,
    border: Rgba,
    width: f32,
) -> Draw {
    Draw::Draw {
        shape: Geometry::Path(vec![wire::CanvasSegment::Ellipse {
            center: [position[0] + size[0] / 2., position[1] + size[1] / 2.],
            radius: [size[0] / 2., size[1] / 2.],
            rotation: 0.,
            start: 0.,
            end: std::f32::consts::TAU,
        }]),
        fill,
        even_odd: false,
        stroke: Some(pen(border, width)),
    }
}
fn diamond(
    position: [f32; 2],
    size: [f32; 2],
    fill: Option<Rgba>,
    border: Rgba,
    width: f32,
) -> Draw {
    use wire::CanvasSegment as Segment;
    let middle = [position[0] + size[0] / 2., position[1] + size[1] / 2.];
    Draw::Draw {
        shape: Geometry::Path(vec![
            Segment::Move([middle[0], position[1]]),
            Segment::Line([position[0] + size[0], middle[1]]),
            Segment::Line([middle[0], position[1] + size[1]]),
            Segment::Line([position[0], middle[1]]),
            Segment::Close,
        ]),
        fill,
        even_odd: false,
        stroke: Some(pen(border, width)),
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
/// The finest the board's lattice is drawn at, in board units. Every step the
/// grid ever takes is this doubled, so a dot always stands on a coordinate a
/// reader could name.
const GRID: f32 = 32.;
/// The smallest type the board will draw. Under it letters stop being letters
/// and start being grey noise, so nothing is drawn at all.
const SMALLEST: f32 = 8.;
/// One card's words, as both the painter and the editor must lay them out.
pub(super) struct Lettering {
    /// Screen-space type size, already scaled by the camera.
    pub(super) size: f32,
    /// How far in from the card's box the words start, on every side.
    pub(super) inset: f32,
    /// Whether the words are being drawn at the size they were asked for.
    /// Type has a floor and a card does not, so far enough out the letters
    /// stop shrinking with the box they are in and a card fills up with a
    /// fragment of its first sentence. A card too small to read carries no
    /// words: a board zoomed right out is blocks of colour, which is what it
    /// is for at that distance.
    pub(super) legible: bool,
}
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
            let pos = self.screen(s.x as f32, s.y as f32);
            let size = [s.width as f32 * self.zoom, s.height as f32 * self.zoom];
            layers.push(self.text_gauge(inline, s, pos, size));
            layers.push(self.inline_editor(s, pos, size));
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
        let picker_open = self.picking_a_board();
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
                "Snap to other cards · hold Ctrl to bypass",
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
        if self.inline.is_some() {
            return None;
        }
        let count = self.selected.len();
        // With nothing picked the palette still says what the next shape will
        // be drawn in — and it was the one control on this panel you could not
        // reach, so the only way to choose a colour was to draw something in
        // the wrong one and recolour it. It stands on its own.
        if count == 0 {
            return Some(kit::sized(
                self.next_color(),
                Some(Length::Fixed(204.)),
                None,
            ));
        }
        let only = self.only_selected().and_then(|id| board.shapes.get(id));
        let name = match only {
            Some(record) => kind_name(record.shape.kind).to_owned(),
            None => format!("{count} selected"),
        };
        // a connector has no box to write in; the inspector does not offer one
        let writable = only.is_some_and(|record| !record.shape.kind.is_path());
        Some(kit::sized(
            self.inspector(name, count, writable),
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
                            // Past the limit the count stops being background
                            // information and becomes the only thing that
                            // matters, so it says what to do about it.
                            if length > boards::MAX_TEXT {
                                format!(
                                    "{} too long · Escape leaves the card as it was",
                                    length - boards::MAX_TEXT
                                )
                            } else {
                                format!("Enter for a new line · {length}/{}", boards::MAX_TEXT)
                            },
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
            Tool::Select => "Double-click to write · Alt-drag to duplicate",
            Tool::Hand => "Drag to explore · Release Space to return to your tool",
            Tool::Note => "Click to place a note and start typing",
            Tool::Rectangle => "Drag to draw a box · Shift-resize to keep proportions",
            Tool::Ellipse => "Drag to draw an ellipse · Shift-resize to keep proportions",
            Tool::Diamond => "Drag to draw a diamond · double-click it to write",
            Tool::Text => "Click to write · Double-click any shape to edit",
            Tool::Arrow => "Drag between shapes to connect them · Shift for straight runs",
            Tool::Line => "Drag to draw a line · Shift for straight runs",
            Tool::Draw => "Draw freehand · release to keep the stroke",
            Tool::Eraser => "Drag across what you want gone · release to erase",
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
    /// The colour the next shape will be drawn in, on its own, for when there
    /// is no selection to recolour.
    fn next_color(&self) -> Node {
        kit::card(
            "boards/next-color",
            kit::spaced(
                kit::column(
                    "boards/next-color-body",
                    [
                        kit::caption("boards/next-color-label", "New shape"),
                        self.swatches(),
                    ],
                ),
                6.,
            ),
        )
    }
    fn swatches(&self) -> Node {
        kit::spaced(
            kit::row(
                "boards/colors",
                (0..5).map(|color| swatch(color, self.palette == color)),
            ),
            4.,
        )
    }
    fn inspector(&self, name: String, count: usize, writable: bool) -> Node {
        let mut properties = vec![
            kit::heading("boards/selection-title", name),
            self.swatches(),
            kit::divider("boards/properties-rule"),
        ];
        if writable {
            properties.push(wide(action(
                "boards/edit-text",
                "Edit text",
                "Enter · double-click",
                Message::EditText,
                true,
            )));
        }
        // stacking, then arranging: the rows a canvas app keeps in its panel
        properties.push(kit::spaced(
            kit::row(
                "boards/stacking",
                [
                    tile(
                        "front",
                        "Bring to front",
                        "⌘ / Ctrl ]",
                        Message::Stack(true),
                    ),
                    tile("back", "Send to back", "⌘ / Ctrl [", Message::Stack(false)),
                    tile("copy", "Duplicate", "⌘ / Ctrl D", Message::Duplicate),
                ],
            ),
            4.,
        ));
        if count > 1 {
            properties.push(kit::divider("boards/arrange-rule"));
            properties.push(kit::caption("boards/arrange-label", "Arrange"));
            for (key, row) in [("x", ARRANGE_X.as_slice()), ("y", ARRANGE_Y.as_slice())] {
                properties.push(kit::spaced(
                    kit::row(
                        format!("boards/arrange-{key}"),
                        row.iter().map(|(how, label, name)| {
                            tile(name, label, "Arrange the selection", Message::Arrange(*how))
                        }),
                    ),
                    4.,
                ));
            }
            properties.push(kit::divider("boards/arrange-rule-b"));
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
            ("O / 5", "Ellipse"),
            ("D / 6", "Diamond"),
            ("A / 7", "Arrow"),
            ("L / 8", "Line"),
            ("P / 9", "Draw freehand"),
            ("T", "Text"),
            ("E", "Eraser"),
            ("Q", "Keep tool active"),
            ("Shift-click / drag", "Multiple selection"),
            ("Enter / double-click", "Edit text"),
            ("⌘ / Ctrl Enter", "Finish text / next note"),
            ("⌘ / Ctrl C · X · V", "Copy / cut / paste at pointer"),
            ("⌘ / Ctrl D · Alt drag", "Duplicate selection"),
            ("⌘ / Ctrl ] · [", "Bring to front / send to back"),
            ("⌘ / Ctrl Z · Shift Z", "Undo / redo"),
            ("Arrow · Shift Arrow", "Move 1 / 10 units"),
            ("⌘ / Ctrl + scroll", "Zoom at pointer"),
            ("F · Shift F · 0", "Fit board / selection / 100%"),
            ("Hold Ctrl", "Ignore snapping while dragging"),
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
    /// The scene, bottom to top: the grid, then every shape as its own layer
    /// with its words pinned straight over it, then one overlay for the marks
    /// that belong to the pointer rather than to the board.
    ///
    /// A shape is a layer of its own because the alternative — one canvas of
    /// bodies under one stack of labels — lets an earlier shape's text show
    /// through a later shape that covers it.
    fn canvas(&self, board: &Board) -> Node {
        let erasing = match &self.gesture {
            Gesture::Erase { swept, .. } => swept.clone(),
            _ => BTreeSet::new(),
        };
        let mut layers = Vec::new();
        // The host decodes a fixed number of geometry pieces per frame and
        // REFUSES the whole frame past it, so the budget is spent in priority
        // order: the pointer's marks first, then the shapes, each taking a
        // fair share of what is left, then the grid with the remainder.
        //
        // The marks come off the top rather than take what the shapes leave: a
        // board dense enough to spend the whole budget is exactly the board
        // where you cannot tell what is selected without them, and selection
        // chrome that disappears as the work grows reads as broken.
        let ordered = board.ordered();
        let mut budget = PARTS - MARKS;
        // Cards off screen cost no words, so the text allowance is shared out
        // among the ones actually drawn — on any ordinary board that is the
        // whole of every card's text, which is the point.
        let shown = ordered
            .iter()
            .filter(|(_, record)| self.on_screen(board, &record.shape).is_some())
            .count();
        let share = LETTERS / shown.max(1);
        for (index, (id, record)) in ordered.iter().enumerate() {
            let s = &record.shape;
            let Some(box_) = self.on_screen(board, s) else {
                continue;
            };
            let opacity = if erasing.contains(*id) { 0.25 } else { 1. };
            let allowance = budget / (ordered.len() - index);
            let mut body = Vec::new();
            self.paint(board, s, opacity, allowance, &mut body);
            if cost(&body) > budget {
                body.clear();
            }
            budget -= cost(&body);
            if !body.is_empty() {
                layers.push(Node::Canvas {
                    key: format!("boards/body/{id}"),
                    width: Some(Length::Fill),
                    height: Some(Length::Fill),
                    commands: body,
                });
            }
            if let Some(label) = self.label(id, s, box_, opacity, share) {
                layers.push(label);
            }
        }
        let mut top = Vec::new();
        self.paint_marks(board, MARKS, &mut top);
        let overrun = cost(&top).saturating_sub(MARKS);
        let grid = self.grid(budget.saturating_sub(overrun));
        let mut children = vec![Node::Canvas {
            key: "boards/grid".into(),
            width: Some(Length::Fill),
            height: Some(Length::Fill),
            commands: grid,
        }];
        children.extend(layers);
        children.push(Node::Canvas {
            key: "boards/overlay".into(),
            width: Some(Length::Fill),
            height: Some(Length::Fill),
            commands: top,
        });
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
        let sensor = Node::Sensor {
            key: "boards/viewport".into(),
            reset: None,
            // The first sight of the stage is also when the canvas takes the
            // keyboard; every later one is only a measurement.
            on_show: Some(slots::handler(Box::new(|(w, h)| {
                Some(Message::Mounted(w, h))
            }))),
            on_resize: Some(slots::handler(Box::new(|(w, h)| Some(Message::Size(w, h))))),
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
    /// The screen box a shape occupies, or nothing when it is off stage. A
    /// connector's box is the stroke it draws, which a bound end moves.
    fn on_screen(&self, board: &Board, s: &Shape) -> Option<[f32; 4]> {
        let world = if s.kind.is_path() {
            let path = interaction::stroke(board, s);
            span(&path)?
        } else {
            interaction::rect(s)
        };
        let a = self.screen(world[0], world[1]);
        let b = self.screen(world[2], world[3]);
        let margin = 48.;
        let shown = b[0] >= -margin
            && b[1] >= -margin
            && a[0] <= self.viewport[0] + margin
            && a[1] <= self.viewport[1] + margin;
        shown.then_some([a[0], a[1], b[0], b[1]])
    }
    /// A faint dot lattice that tracks the camera, drawn no denser than the
    /// parts it was given.
    pub(super) fn grid(&self, budget: usize) -> Vec<Draw> {
        let muted = Rgba(kit::palette().border_strong);
        // A lattice the camera moves over, not wallpaper stuck to the screen:
        // the step is always a whole number of board units, doubled until the
        // dots are far enough apart to read. So zooming out coarsens the ruler
        // by whole factors — every dot still stands on a round coordinate —
        // instead of stretching the same dots over ever larger distances.
        let mut step = GRID;
        while step * self.zoom < 24. {
            step *= 2.;
        }
        let mut spacing = step * self.zoom;
        let counts = |spacing: f32| {
            [
                (self.viewport[0] / spacing).ceil() as usize + 1,
                (self.viewport[1] / spacing).ceil() as usize + 1,
            ]
        };
        // widen the lattice rather than truncate it: half a grid reads as a bug
        while counts(spacing)[0] * counts(spacing)[1] > budget {
            if spacing > self.viewport[0].max(self.viewport[1]) {
                return Vec::new();
            }
            spacing *= 2.;
        }
        let [columns, rows] = counts(spacing);
        let mut dots = Vec::with_capacity(columns * rows);
        for column in 0..columns {
            for row in 0..rows {
                dots.push(Draw::Draw {
                    shape: Geometry::Circle {
                        center: [
                            self.camera[0].rem_euclid(spacing) + column as f32 * spacing,
                            self.camera[1].rem_euclid(spacing) + row as f32 * spacing,
                        ],
                        radius: 0.7,
                    },
                    fill: Some(muted),
                    even_odd: false,
                    stroke: None,
                });
            }
        }
        dots
    }
    /// One shape's body, within `budget` pieces of geometry. `opacity` is
    /// what the eraser has already swept.
    fn paint(&self, board: &Board, s: &Shape, opacity: f32, budget: usize, out: &mut Vec<Draw>) {
        let pos = self.screen(s.x as f32, s.y as f32);
        let size = [s.width as f32 * self.zoom, s.height as f32 * self.zoom];
        let body = |strength: f32| Rgba(alpha(fill(s.color), strength * opacity));
        let edge = |strength: f32| Rgba(alpha(tint(s.color), strength * opacity));
        let line_width = (1.5 * self.zoom).clamp(1., 8.);
        let radius = (6. * self.zoom).clamp(2., 20.);
        match s.kind {
            Kind::Note => out.push(rectangle(pos, size, Some(body(1.)), edge(0.35), 1., radius)),
            Kind::Rectangle => out.push(rectangle(
                pos,
                size,
                Some(body(0.45)),
                edge(1.),
                line_width,
                radius,
            )),
            Kind::Ellipse => out.push(ellipse(pos, size, Some(body(0.45)), edge(1.), line_width)),
            Kind::Diamond => out.push(diamond(pos, size, Some(body(0.45)), edge(1.), line_width)),
            // text carries no body: the words are the shape
            Kind::Text => {}
            Kind::Arrow | Kind::Line | Kind::Draw => {
                let world = interaction::stroke(board, s);
                let screen: Vec<_> = world.iter().map(|p| self.screen(p[0], p[1])).collect();
                self.paint_stroke(s.kind, &screen, edge(1.), budget, out);
            }
        }
    }
    fn paint_stroke(
        &self,
        kind: Kind,
        screen: &[[f32; 2]],
        color: Rgba,
        budget: usize,
        out: &mut Vec<Draw>,
    ) {
        if screen.len() < 2 {
            return;
        }
        // Samples finer than a pixel buy nothing; past that the budget decides.
        // The run costs one command plus a segment each, and an arrowhead two
        // more commands on top.
        let head = if kind == Kind::Arrow { 2 } else { 0 };
        let limit = budget.saturating_sub(1 + head);
        if limit < 2 {
            return;
        }
        let path = decimate(&interaction::thin(screen, 0.75), limit);
        let end = path[path.len() - 1];
        let weight = if kind == Kind::Draw { 2.5 } else { 1.8 };
        let width = (weight * self.zoom).clamp(1.2, 14.);
        out.push(Draw::Draw {
            shape: Geometry::Path(polyline(&path)),
            fill: None,
            even_odd: false,
            stroke: Some(pen(color, width)),
        });
        if kind != Kind::Arrow {
            return;
        }
        let before = path[path.len() - 2];
        let angle = (end[1] - before[1]).atan2(end[0] - before[0]);
        let head = (12. * self.zoom).clamp(7., 30.);
        for turn in [-0.5_f32, 0.5] {
            out.push(line(
                end,
                [
                    end[0] - head * (angle + turn).cos(),
                    end[1] - head * (angle + turn).sin(),
                ],
                color,
                width,
            ));
        }
    }
    /// How a card's words are laid out. The painter and the inline editor both
    /// ask, and both get this answer, so a label never changes size or jumps to
    /// a different corner the moment you start typing — which would read as the
    /// editor having its own opinion about the text rather than showing yours.
    /// Every card writes from the top of its box, because the native editor
    /// fills the box it is given and cannot be centred in it: words that sat in
    /// the middle when painted would jump to the top the instant a caret
    /// appeared. So the painter writes where the editor can.
    pub(super) fn lettering(&self, kind: Kind, size: [f32; 2]) -> Lettering {
        let plain = kind == Kind::Text;
        let edge = CARD_INSET * self.zoom;
        // an ellipse and a diamond pinch away from their corners, so their
        // words start further in — far enough to sit on the body, not beside it
        let pinch = match kind {
            Kind::Ellipse => 0.14,
            Kind::Diamond => 0.22,
            Kind::Note | Kind::Rectangle | Kind::Text => 0.,
            Kind::Arrow | Kind::Line | Kind::Draw => 0.,
        };
        let asked = if plain { 20. } else { 14. } * self.zoom;
        Lettering {
            size: asked.clamp(SMALLEST, 60.),
            inset: edge + size[0].min(size[1]) * pinch,
            legible: asked >= SMALLEST,
        }
    }
    /// A shape's words, pinned over its body and clipped to it.
    fn label(
        &self,
        id: &str,
        s: &Shape,
        box_: [f32; 4],
        opacity: f32,
        share: usize,
    ) -> Option<Node> {
        let p = kit::palette();
        let editing = self.inline.as_ref().is_some_and(|inline| inline.id == *id);
        if editing {
            return None;
        }
        let letters = self.lettering(s.kind, [box_[2] - box_[0], box_[3] - box_[1]]);
        if !letters.legible {
            return None;
        }
        // a connector's label rides its middle; it has no box to sit in
        if s.kind.is_path() {
            if s.text.is_empty() {
                return None;
            }
            // The plate it is written on rides the camera like everything else
            // on the board: a label that kept its pixels while the run under it
            // shrank would swallow the whole drawing at a distance.
            let plate = [180. * self.zoom, 40. * self.zoom];
            return Some(Node::Pin {
                key: format!("boards/path-label/{id}"),
                x: (box_[0] + box_[2]) / 2. - plate[0] / 2.,
                y: (box_[1] + box_[3]) / 2. - plate[1] / 2.,
                width: Some(Length::Fixed(plate[0])),
                height: Some(Length::Fixed(plate[1])),
                content: Box::new(kit::text_size(
                    kit::wrapping(kit::text(
                        format!("boards/path-text/{id}"),
                        excerpt(&s.text, share),
                    )),
                    letters.size,
                )),
            });
        }
        let blank = s.text.is_empty();
        // a blank sticky invites a word; a blank outline is a drawing, not a card
        let prompt = matches!(s.kind, Kind::Note | Kind::Text);
        if blank && !prompt {
            return None;
        }
        let text = if blank {
            "Write a thought…"
        } else {
            excerpt(&s.text, share)
        };
        let ink = if blank { p.faint } else { p.foreground };
        let label = kit::colored(
            kit::text_size(
                kit::wrapping(kit::text(format!("boards/label/{id}"), text)),
                letters.size,
            ),
            alpha(ink, opacity),
        );
        let mut clip = kit::container(format!("boards/label-clip/{id}"), label);
        if let Node::Container {
            clip: clipped,
            height,
            padding,
            align_y,
            ..
        } = &mut clip
        {
            *clipped = true;
            *height = Some(Length::Fill);
            *padding = Some(wire::Edges::all(letters.inset));
            *align_y = None;
        }
        Some(Node::Pin {
            key: format!("boards/pin/{id}"),
            x: box_[0],
            y: box_[1],
            width: Some(Length::Fixed((box_[2] - box_[0]).max(1.))),
            height: Some(Length::Fixed((box_[3] - box_[1]).max(1.))),
            content: Box::new(clip),
        })
    }
    /// What belongs to the pointer, not to the board: the selection, its
    /// handles, the snapping guides and whatever the current gesture is about
    /// to leave behind.
    fn paint_marks(&self, board: &Board, budget: usize, out: &mut Vec<Draw>) {
        let p = kit::palette();
        let accent = Rgba(p.accent);
        let ring = (6. * self.zoom).clamp(2., 20.);
        // What a press would take, drawn faintly so it reads as an answer and
        // not as a selection. Already-selected shapes wear the real ring.
        if let Some(id) = self
            .hover
            .as_ref()
            .filter(|id| !self.selected.contains(*id))
            && let Some(record) = board.shapes.get(id)
            && let Some(box_) = self.on_screen(board, &record.shape)
        {
            let inset = if record.shape.kind.is_path() { 4. } else { 0. };
            out.push(rectangle(
                [box_[0] - inset, box_[1] - inset],
                [
                    box_[2] - box_[0] + inset * 2.,
                    box_[3] - box_[1] + inset * 2.,
                ],
                None,
                Rgba(alpha(p.accent, 0.45)),
                1.,
                ring,
            ));
        }
        // leave the gesture and the guides their own room out of the budget
        let rings = budget.saturating_sub(32);
        for id in &self.selected {
            if cost(out) >= rings {
                break;
            }
            let Some(record) = board.shapes.get(id) else {
                continue;
            };
            let s = &record.shape;
            let Some(box_) = self.on_screen(board, s) else {
                continue;
            };
            let inset = if s.kind.is_path() { 4. } else { 0. };
            out.push(rectangle(
                [box_[0] - inset, box_[1] - inset],
                [
                    box_[2] - box_[0] + inset * 2.,
                    box_[3] - box_[1] + inset * 2.,
                ],
                None,
                accent,
                1.5,
                ring,
            ));
            let alone = self.selected.len() == 1 && self.inline.is_none();
            if !alone {
                continue;
            }
            let grip = |at: [f32; 2]| Draw::Draw {
                shape: Geometry::Rectangle {
                    position: [at[0] - 3.5, at[1] - 3.5],
                    size: [7., 7.],
                    radius: [1.5; 4],
                },
                fill: Some(Rgba(p.background)),
                even_odd: false,
                stroke: Some(pen(accent, 1.5)),
            };
            // A connector is taken by its ends, a card by its corners: the grip
            // a shape offers is the edit it can be given, and they differ.
            if s.kind.is_path() {
                let run = interaction::stroke(board, s);
                for end in [run.first(), run.last()].into_iter().flatten() {
                    out.push(grip(self.screen(end[0], end[1])));
                }
                continue;
            }
            if !interaction::free(s) {
                continue;
            }
            for corner in interaction::HANDLES {
                let world = interaction::corner_point(s, corner);
                out.push(grip(self.screen(world[0], world[1])));
            }
        }
        // Several shapes are one thing to hold, so they get one box to hold it
        // by. Without it a multiple selection is a scatter of rings that says
        // what is in it and nothing about what taking hold of it would do.
        if let Some((bounds, _)) = self.group(board) {
            let origin = self.screen(bounds[0], bounds[1]);
            let size = [bounds[2] * self.zoom, bounds[3] * self.zoom];
            out.push(rectangle(
                [origin[0] - 6., origin[1] - 6.],
                [size[0] + 12., size[1] + 12.],
                None,
                Rgba(alpha(p.accent, 0.7)),
                1.,
                2.,
            ));
            let grip = |at: [f32; 2]| Draw::Draw {
                shape: Geometry::Rectangle {
                    position: [at[0] - 3.5, at[1] - 3.5],
                    size: [7., 7.],
                    radius: [1.5; 4],
                },
                fill: Some(Rgba(p.background)),
                even_odd: false,
                stroke: Some(pen(accent, 1.5)),
            };
            for corner in interaction::HANDLES {
                let world = interaction::handle_point(bounds, corner);
                out.push(grip(self.screen(world[0], world[1])));
            }
        }
        for guide in &self.guides {
            out.push(line(
                self.screen(guide[0], guide[1]),
                self.screen(guide[2], guide[3]),
                accent,
                1.,
            ));
        }
        self.paint_gesture(board, out);
    }
    fn paint_gesture(&self, board: &Board, out: &mut Vec<Draw>) {
        let p = kit::palette();
        let accent = Rgba(p.accent);
        let wash = Some(Rgba(alpha(p.accent, 0.08)));
        match &self.gesture {
            Gesture::Marquee { start, point, .. } => {
                let b = interaction::points_rect(*start, *point);
                out.push(rectangle(
                    self.screen(b[0], b[1]),
                    [(b[2] - b[0]) * self.zoom, (b[3] - b[1]) * self.zoom],
                    wash,
                    accent,
                    1.,
                    2.,
                ));
            }
            Gesture::Create { kind, start, point } => {
                let shape = self.creation_shape(*kind, *start, *point);
                if kind.is_path() {
                    let screen: Vec<_> = interaction::path_points(&shape)
                        .iter()
                        .map(|q| self.screen(q[0], q[1]))
                        .collect();
                    self.paint_stroke(*kind, &screen, accent, 8, out);
                    return;
                }
                out.push(rectangle(
                    self.screen(shape.x as f32, shape.y as f32),
                    [
                        shape.width as f32 * self.zoom,
                        shape.height as f32 * self.zoom,
                    ],
                    wash,
                    accent,
                    1.,
                    (6. * self.zoom).clamp(2., 20.),
                ));
            }
            Gesture::Sketch { points } => {
                let screen: Vec<_> = points.iter().map(|q| self.screen(q[0], q[1])).collect();
                self.paint_stroke(
                    Kind::Draw,
                    &screen,
                    Rgba(tint(self.palette)),
                    boards::MAX_POINTS,
                    out,
                );
            }
            // An endpoint in hand says what it would take hold of: the card it
            // is over is ringed, so releasing is a decision you already made
            // rather than one you find out about afterwards.
            Gesture::Endpoint { point, shape, .. } => {
                let binds = shape.kind == Kind::Arrow;
                let Some(id) = binds
                    .then(|| self.topmost(board, *point, |s: &Shape| !s.kind.is_path()))
                    .flatten()
                else {
                    return;
                };
                let Some(record) = board.shapes.get(&id) else {
                    return;
                };
                let card = &record.shape;
                out.push(rectangle(
                    self.screen(card.x as f32, card.y as f32),
                    [
                        card.width as f32 * self.zoom,
                        card.height as f32 * self.zoom,
                    ],
                    wash,
                    accent,
                    2.,
                    (6. * self.zoom).clamp(2., 20.),
                ));
            }
            Gesture::Idle
            | Gesture::Pan { .. }
            | Gesture::Move { .. }
            | Gesture::Scale { .. }
            | Gesture::Nudge { .. }
            | Gesture::Resize { .. }
            | Gesture::Erase { .. } => {
                let _ = board;
            }
        }
    }
}
/// The host decodes at most `MAX_CANVAS_PARTS` (4096) pieces of geometry per
/// frame, shared across every canvas in it, and REFUSES a frame that exceeds
/// it. Everything the scene draws is spent out of this one budget.
const PARTS: usize = 3600;
/// Held back out of it for what belongs to the pointer — the selection, its
/// handles, the guides and the gesture in flight. Enough for a wide selection
/// and its grips, and it is taken before the shapes rather than after, so the
/// answer to "what am I holding" does not vanish on a busy board.
const MARKS: usize = 320;

/// What geometry costs against that budget: the host charges for the command
/// AND for every segment inside it.
fn cost(commands: &[Draw]) -> usize {
    commands
        .iter()
        .map(|command| {
            let segments = match command {
                Draw::Draw {
                    shape: Geometry::Path(path),
                    ..
                } => path.len(),
                _ => 0,
            };
            1 + segments
        })
        .sum()
}
/// Keep the ends and an even spread between them, so a stroke that cannot
/// afford every sample this frame still reads as the same line.
fn decimate(points: &[[f32; 2]], limit: usize) -> Vec<[f32; 2]> {
    if points.len() <= limit || limit < 2 {
        return points.to_vec();
    }
    let last = points.len() - 1;
    (0..limit)
        .map(|index| points[index * last / (limit - 1)])
        .collect()
}

fn span(points: &[[f32; 2]]) -> Option<[f32; 4]> {
    points
        .iter()
        .copied()
        .map(|p| [p[0], p[1], p[0], p[1]])
        .reduce(|a, b| {
            [
                a[0].min(b[0]),
                a[1].min(b[1]),
                a[2].max(b[2]),
                a[3].max(b[3]),
            ]
        })
}
/// A run of points as one path: a straight segment between two, and a round
/// one through the midpoints of a longer run, so a pen stroke reads as drawn.
fn polyline(points: &[[f32; 2]]) -> Vec<wire::CanvasSegment> {
    use wire::CanvasSegment as Segment;
    let mut path = vec![Segment::Move(points[0])];
    if points.len() == 2 {
        path.push(Segment::Line(points[1]));
        return path;
    }
    for pair in points.windows(2).skip(1) {
        path.push(Segment::Quadratic {
            control: pair[0],
            end: [
                (pair[0][0] + pair[1][0]) / 2.,
                (pair[0][1] + pair[1][1]) / 2.,
            ],
        });
    }
    path.push(Segment::Line(points[points.len() - 1]));
    path
}

/// The host decodes at most 64 KiB of text per frame and refuses the whole
/// frame past it. Most of that is the chrome's, which is short and fixed; the
/// rest is the board's to share out among the cards actually on screen.
const LETTERS: usize = 48 * 1024;
/// One card's share of that, given how many are on screen with it. A board of
/// a dozen cards gives every one of them room for its whole text — which is
/// the point: what a card shows and what its editor holds are the same words.
/// Only a screen packed past readability has to cut anything, and it cuts the
/// tail rather than the frame.
fn excerpt(text: &str, share: usize) -> &str {
    let mut end = text.len().min(share);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The tool bar, in the order a canvas app prints it: what points, what pans,
/// then the shapes, then the pen and what takes it back.
const TOOLS: [(Tool, &str, &str, &str); 11] = [
    (Tool::Select, "Select", "V", "select"),
    (Tool::Hand, "Pan", "H", "hand"),
    (Tool::Note, "Note", "N", "note"),
    (Tool::Rectangle, "Box", "R", "box"),
    (Tool::Ellipse, "Ellipse", "O", "ellipse"),
    (Tool::Diamond, "Diamond", "D", "diamond"),
    (Tool::Arrow, "Arrow", "A", "arrow"),
    (Tool::Line, "Line", "L", "line"),
    (Tool::Draw, "Draw", "P", "draw"),
    (Tool::Text, "Text", "T", "text"),
    (Tool::Eraser, "Eraser", "E", "eraser"),
];
/// The arrange rows, an axis each: the three edges to line up on, then the
/// even spread along the same axis.
const ARRANGE_X: [(Arrange, &str, &str); 4] = [
    (Arrange::Left, "Align left", "align-left"),
    (Arrange::CentreX, "Align centres", "align-centre-x"),
    (Arrange::Right, "Align right", "align-right"),
    (Arrange::SpreadX, "Spread across", "spread-x"),
];
const ARRANGE_Y: [(Arrange, &str, &str); 4] = [
    (Arrange::Top, "Align top", "align-top"),
    (Arrange::CentreY, "Align middles", "align-centre-y"),
    (Arrange::Bottom, "Align bottom", "align-bottom"),
    (Arrange::SpreadY, "Spread down", "spread-y"),
];
fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Note => "Sticky note",
        Kind::Rectangle => "Rectangle",
        Kind::Ellipse => "Ellipse",
        Kind::Diamond => "Diamond",
        Kind::Text => "Text",
        Kind::Arrow => "Arrow",
        Kind::Line => "Line",
        Kind::Draw => "Drawing",
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
        "ellipse" => "<ellipse cx='12' cy='12' rx='9' ry='7'/>",
        "diamond" => "<path d='M12 3 21 12 12 21 3 12z'/>",
        "line" => "<path d='M4 20 20 4'/>",
        "draw" => "<path d='M4 20h4L19 9l-4-4L4 16z'/><path d='m14 6 4 4M4 16l4 4'/>",
        "eraser" => "<path d='m13 4 7 7-8 8H7l-4-4z'/><path d='M8 9l7 7M11 19h9'/>",
        "text" => "<path d='M4 6V4h16v2M12 4v16M8 20h8'/>",
        "arrow" => "<path d='M4 19 20 4M10 4h10v10'/>",
        "lock" => {
            "<rect x='5' y='10' width='14' height='11' rx='3'/><path d='M8 10V7a4 4 0 0 1 8 0v3M12 14v3'/>"
        }
        // the arrange tiles: a rule on the edge the boxes line up against
        "align-left" => {
            "<path d='M3 3v18'/><rect x='6' y='5' width='14' height='5'/><rect x='6' y='14' width='9' height='5'/>"
        }
        "align-centre-x" => {
            "<path d='M12 3v18'/><rect x='5' y='5' width='14' height='5'/><rect x='8' y='14' width='8' height='5'/>"
        }
        "align-right" => {
            "<path d='M21 3v18'/><rect x='4' y='5' width='14' height='5'/><rect x='9' y='14' width='9' height='5'/>"
        }
        "align-top" => {
            "<path d='M3 3h18'/><rect x='5' y='6' width='5' height='14'/><rect x='14' y='6' width='5' height='9'/>"
        }
        "align-centre-y" => {
            "<path d='M3 12h18'/><rect x='5' y='5' width='5' height='14'/><rect x='14' y='8' width='5' height='8'/>"
        }
        "align-bottom" => {
            "<path d='M3 21h18'/><rect x='5' y='4' width='5' height='14'/><rect x='14' y='9' width='5' height='9'/>"
        }
        "spread-x" => "<path d='M3 3v18M21 3v18'/><rect x='10' y='7' width='4' height='10'/>",
        "spread-y" => "<path d='M3 3h18M3 21h18'/><rect x='7' y='10' width='10' height='4'/>",
        "front" => "<rect x='3' y='3' width='12' height='12' rx='2'/><path d='M9 21h12V9'/>",
        "back" => "<rect x='9' y='9' width='12' height='12' rx='2'/><path d='M15 3H3v12'/>",
        "copy" => "<rect x='9' y='9' width='12' height='12' rx='2'/><path d='M5 15H3V3h12v2'/>",
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
/// An icon-only square in an inspector row: the panel packs its arrangements
/// the way a canvas app does, six to a strip rather than six stacked labels.
fn tile(name: &str, label: &str, hint: &str, message: Message) -> Node {
    icon_button(
        &format!("boards/tile/{name}"),
        name,
        label,
        hint,
        message,
        true,
        false,
    )
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
    /// The card's words a second time, invisibly, laid out exactly the way the
    /// painter writes them and left to take whatever height they need. The
    /// host reports what that came to, and the card grows to it.
    ///
    /// The native editor cannot answer this itself: asked to lay out to its
    /// own content it shows one line of however many it holds, and there is no
    /// verb on the wire for measuring a document. So the gauge measures the
    /// label instead — the same text node, the same size, the same width and
    /// the same padding — which is the right authority anyway. The card ends
    /// up as tall as the words it will be *drawn* with.
    fn text_gauge(&self, inline: &Inline, shape: &Shape, pos: [f32; 2], size: [f32; 2]) -> Node {
        let letters = self.lettering(shape.kind, size);
        let words = inline.document.text();
        let measure = || {
            Some(slots::handler(Box::new(|(w, h)| {
                Some(Message::Measured(w, h))
            })))
        };
        let mut gauge = kit::colored(
            kit::text_size(
                kit::wrapping(kit::text("boards/gauge-text", excerpt(&words, LETTERS))),
                letters.size,
            ),
            alpha(kit::palette().foreground, 0.),
        );
        if let Node::Text { width, .. } = &mut gauge {
            // Narrower than the card by a margin, because the native editor
            // keeps room inside the box it is given that a plain label does
            // not: with the same words in the same column it takes one line
            // more than the label does, and a card measured on the label alone
            // comes up about half a line short of what the caret needs. The
            // margin rides the type size so a card's height is the same
            // whatever the camera is doing — a box that reflowed as you zoomed
            // would resize itself for looking at it.
            let column = size[0] - 2. * letters.inset - 2. * letters.size;
            *width = Some(Length::Fixed(column.max(40.)));
        }
        let mut padded = kit::container("boards/gauge-pad", gauge);
        if let Node::Container { padding, .. } = &mut padded {
            // The card is the words plus the room they are written in, so the
            // gauge carries the same inset and reports a card height, not a
            // text height.
            *padding = Some(wire::Edges::all(letters.inset));
        }
        Node::Pin {
            key: "boards/gauge-pin".into(),
            x: pos[0],
            y: pos[1],
            width: Some(Length::Fixed(size[0].max(1.))),
            // No height: this is the one node on the stage allowed to be as
            // tall as it likes, because its height is the answer.
            height: None,
            content: Box::new(Node::Sensor {
                key: format!("boards/gauge/{}", inline.id),
                reset: None,
                on_show: measure(),
                on_resize: measure(),
                on_hide: None,
                anticipate: None,
                delay: None,
                child: Box::new(padded),
            }),
        }
    }
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
            // Both claimed keys leave the card, and they are not the same
            // answer: ⌘Enter keeps what you wrote, Escape is the way out when
            // the board will not take it. The commit says which key asked, so
            // this is where they part.
            |event| match event {
                EditorTransactionEvent::Commit { origin, .. } => match origin {
                    Some(wire::EditorRequestInput::Key { key, .. })
                        if key.key == Key::Named(Named::Escape) =>
                    {
                        Some(Message::Cancel)
                    }
                    Some(_) => Some(Message::FinishText),
                    None => None,
                },
                _ => None,
            },
        )
        .register(std::convert::identity, Message::TextTransaction);
        // The card is already painted underneath, fill and outline and all; an
        // opaque field over it would replace the shape you are writing inside
        // with a plain rectangle for as long as you typed.
        let style = wire::InputStyle {
            active: wire::InputFace {
                background: Some(Rgba([0.; 4])),
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
        let letters = self.lettering(shape.kind, size);
        let editor = Node::Editor {
            key: format!("boards/editor/{}", inline.id),
            document,
            on_document,
            editable: true,
            placeholder: "Write a thought…".into(),
            width: Some((size[0] - 2. * letters.inset).max(40.)),
            // The editor fills the card. It cannot be asked to lay out to its
            // own content instead — a shrunk editor collapses to its first
            // line on the native side, which is how a card would learn to
            // hide five of the six lines it is holding.
            height: Some(Length::Fill),
            min_height: Some(letters.size * 1.6),
            max_height: None,
            options: Box::new(wire::EditorOptions {
                binding: Some(Box::new(binding)),
                size: Some(letters.size),
                padding: Some(0.),
                style,
                ..Default::default()
            }),
        };
        let layout = kit::sized(
            kit::container("boards/editor-layout", editor),
            Some(Length::Fill),
            Some(Length::Fill),
        );
        let inset = letters.inset;
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
                child: Box::new(layout),
            }),
        }
    }
}
