use crate::*;
use ducktape_view_guest::{
    kit, slots,
    wire::{
        self, AlignX, AlignY, ButtonPreset, CanvasCommand as Draw, CanvasShape as Geometry, Length,
        Node, Rgba,
    },
};

fn pen(color: Rgba, width: f32, dash: Dash) -> wire::CanvasStroke {
    wire::CanvasStroke {
        color,
        width,
        cap: wire::CanvasLineCap::Round,
        join: wire::CanvasLineJoin::Round,
        // The rhythm is measured in pen widths, not pixels, so a dash reads the
        // same on a hairline at 30% zoom as on a fat stroke at 300%. A list in
        // absolute units would be a solid line at one end of the range and a
        // row of dots at the other.
        dash: match dash {
            Dash::Solid => Vec::new(),
            Dash::Dashed => vec![width * 3., width * 2.5],
        },
        dash_offset: 0,
    }
}
/// The board's own chrome — rings, guides, previews — is never dashed and never
/// hollow: a broken selection ring would read as a shape somebody drew.
const CHROME: Dash = Dash::Solid;
/// What the pen's weight does to a line — a multiplier, not a width. The width
/// a shape is drawn at is already two facts: how big it reads at this zoom, and
/// what KIND it is (a note's border is a hairline, a freehand nib is fat). The
/// weight is a third, independent of both, so it multiplies what those two
/// decided rather than replacing it.
///
/// Which is also why `Medium` is exactly 1: every board drawn before a shape
/// could carry a weight reads back at the width it was drawn at, to the pixel.
/// How a run is drawn, resolved: the colour it came out as, and the two pen
/// properties the painter has still to apply. One value because every caller
/// carries all three together and not one of them ever varies alone — and
/// because a run's painter threading them as three arguments is how it ended up
/// at eight.
#[derive(Clone, Copy)]
struct Ink {
    color: Rgba,
    dash: Dash,
    weight: Weight,
}
fn heft(weight: Weight) -> f32 {
    match weight {
        Weight::Thin => 0.5,
        Weight::Medium => 1.,
        Weight::Thick => 2.,
        Weight::Heavy => 3.5,
    }
}
fn line(from: [f32; 2], to: [f32; 2], color: Rgba, width: f32) -> Draw {
    Draw::Draw {
        shape: Geometry::Line { from, to },
        fill: None,
        even_odd: false,
        stroke: Some(pen(color, width, CHROME)),
    }
}
fn rectangle(
    position: [f32; 2],
    size: [f32; 2],
    fill: Option<Rgba>,
    border: Rgba,
    width: f32,
    radius: f32,
    dash: Dash,
) -> Draw {
    Draw::Draw {
        shape: Geometry::Rectangle {
            position,
            size,
            radius: [radius; 4],
        },
        fill,
        even_odd: false,
        stroke: Some(pen(border, width, dash)),
    }
}
fn ellipse(
    position: [f32; 2],
    size: [f32; 2],
    fill: Option<Rgba>,
    border: Rgba,
    width: f32,
    dash: Dash,
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
        stroke: Some(pen(border, width, dash)),
    }
}
fn diamond(
    position: [f32; 2],
    size: [f32; 2],
    fill: Option<Rgba>,
    border: Rgba,
    width: f32,
    dash: Dash,
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
        stroke: Some(pen(border, width, dash)),
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
/// The plate a connector's words sit on, in board units. A connector has no
/// box of its own to write in — its rectangle is only the span of its samples
/// — so its label rides a plate of this size at the middle of the run. The
/// painter draws it, the editor opens over it, and a press inside it takes the
/// connector, so the size is stated once here.
pub(super) const PLATE: [f32; 2] = [200., 56.];
/// The smallest box the board will take for a card, as `validate_shape` in the
/// boards module states it. A shape outside the limits is refused whole, so a
/// card being fitted to its words has to ask for a box inside them: asking for
/// one pixel less than the floor is not a card fitted to the floor, it is a
/// card that quietly stopped being fitted at all.
pub(super) const MIN_CARD: [i32; 2] = [40, 32];
/// The smallest type the board will draw. Under it letters stop being letters
/// and start being grey noise, so nothing is drawn at all.
const SMALLEST: f32 = 8.;
/// What an empty card says when it is waiting for a word.
const PROMPT: &str = "Write a thought…";
/// Whether a blank shape of this kind is a card waiting for a word or a
/// drawing that was never going to carry one. A blank sticky invites a word; a
/// blank outline is a drawing, and a connector with nothing written on it is
/// just a line.
///
/// Only the painter asks. A card you are already writing in needs no invitation
/// — it has a caret — and the one place a prompt could not fit was the box of a
/// text shape that has no words to be as wide as yet.
fn invites_a_word(kind: Kind) -> bool {
    matches!(kind, Kind::Note | Kind::Text)
}
/// How far apart a card's lines sit, as a multiple of the type size. The
/// painter and the editor each have a default and they are NOT the same
/// number, so a card whose lines were set by neither drew them tight while
/// you typed and loose once you stopped. Both read this one instead.
const LEADING: f32 = 1.4;
/// The strip at the right of a field that the host's soft-wrapping input will
/// not wrap into — room it keeps for the caret to stand past the last glyph of
/// a line. A painted label keeps none, so a field handed the label's column
/// exactly wraps ten pixels early: every line loses whatever word straddles
/// that strip, and the words rewrap the moment you stop typing.
///
/// Pixels and not a multiple of the type size, because that is what it is on
/// the other side: a fixed reserve the field takes whatever the camera or the
/// writing is doing. Scaling it would make a card rewrap for being zoomed.
///
/// Ten of those pixels are the reserve itself. The eleventh is the pixel the
/// two layouts can round apart, and it is spent in this direction on purpose:
/// a field that breaks a line the label keeps shows more lines than the card
/// was measured to hold and clips the last of them, where a field that keeps a
/// line the label breaks only leaves the card a line taller than it needed.
///
/// This closes the column, not the last few pixels of it. The field and the
/// label are two different text engines on the host side and they do not
/// measure the same sentence to quite the same width, so a word whose end
/// falls within a pixel or three of the column can still land on either side
/// of the break. What it does close is the whole-word disagreement that used
/// to show on every card that wrapped at all.
pub(super) const WRAP_RESERVE: f32 = 11.;
/// One card's words, as both the painter and the editor must lay them out.
pub(super) struct Lettering {
    /// Screen-space type size, already scaled by the camera.
    pub(super) size: f32,
    /// The distance from one line of those words to the next, in pixels.
    pub(super) leading: f32,
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
/// The width of the zoom readout. Fixed on purpose — a readout that resized as
/// you zoomed would shuffle the whole camera island sideways under the pointer
/// that was zooming — which means it has to be wide enough for the WIDEST
/// label the zoom can reach, not the ones that happened to be on screen when it
/// was measured. Digits are not all one width: `1` is narrow, so `100%` fitted
/// the old 52 and `200%` and `800%` did not, and those two are the ones a user
/// asks for on purpose (Shift-F on a single shape lands on exactly 200%,
/// holding zoom-in settles at exactly 800%). Measured on a live board at both.
/// Widen it again if `zoom_at`'s clamp ever passes 8.
const ZOOM_READOUT: f32 = 64.;
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
            && let Some(live) = board.as_ref()
            && let Some(record) = live.shapes.get(&inline.id)
        {
            let s = &record.shape;
            // The same box the painter writes this shape's words in, so the
            // editor opens exactly over the label it replaces — for a
            // connector that is the plate at the middle of the run, which is
            // nowhere near the rectangle its samples were stored with.
            let (pos, size) = self.writing_box(live, s, self.card_box(live, s));
            layers.push(self.text_gauge(inline, s, pos, size));
            let (caret, room) = self.caret_box(inline, s, pos, size);
            layers.push(self.inline_editor(s, caret, room));
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
    /// What the readout says. Named because `ZOOM_READOUT` is sized to hold
    /// its widest reading and nothing else states what that is: the zoom's own
    /// clamp does, one call away, and a test that walks the zoom to both ends
    /// is what keeps the two facts together.
    pub(super) fn zoom_label(&self) -> String {
        format!("{}%", (self.zoom * 100.).round())
    }
    /// Bottom-left: the camera, then history, then snapping.
    fn camera_island(&self) -> Node {
        let controls = [
            action("boards/zoom-out", "−", "Zoom out", Message::Zoom(0.8), true),
            kit::sized(
                action(
                    "boards/zoom",
                    &self.zoom_label(),
                    "Reset zoom · 0",
                    Message::ResetZoom,
                    true,
                ),
                Some(Length::Fixed(ZOOM_READOUT)),
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
        // Every shape on the board can be written in, a connector included:
        // its words ride the run on a plate instead of sitting in a box, which
        // changes where they are DRAWN and not whether they exist. The panel
        // is the only place that says a shape can be labelled at all, so
        // leaving a connector out of it left `Enter` as the one way to find
        // out — and left its text size, which the painter honours, with no
        // control anywhere that could change it. What a connector genuinely
        // has no room for is an alignment, and `inspector` withholds that row
        // on its own.
        let writable = only.map(|record| &record.shape);
        // A control that cannot do anything is worse than a missing one: it
        // says the shape has a property it does not have. A run has no body to
        // empty and a text shape has neither a body nor an outline, so each row
        // appears only when something in the selection can answer it. The
        // answer SHOWN is the pen — what a new shape would be drawn with —
        // because a mixed selection has no single one and the pen is what
        // pressing the button would set them all to.
        let picked = || self.selected.iter().filter_map(|id| board.shapes.get(id));
        let any_body = picked().any(|record| has_body(record.shape.kind));
        let any_outline = picked().any(|record| has_outline(record.shape.kind));
        Some(kit::sized(
            self.inspector(name, count, writable, any_body, any_outline),
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
                            if length > boards_wire::MAX_TEXT {
                                format!(
                                    "{} too long · shorten it, or Escape to leave it behind",
                                    length - boards_wire::MAX_TEXT
                                )
                            } else {
                                format!("Enter for a new line · {length}/{}", boards_wire::MAX_TEXT)
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
            Tool::Select => "Double-click to write · Alt or ⌘⇧ drag to duplicate",
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
                        // The whole pen, not just its colour: choosing to draw
                        // outlines is a decision you make before drawing, and
                        // having to draw one filled and empty it afterwards is
                        // the same complaint the colour row was added for.
                        fill_row(self.pen.fill),
                        dash_row(self.pen.dash),
                        weight_row(self.pen.weight),
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
                (0..5).map(|color| swatch(color, self.pen.color == color)),
            ),
            4.,
        )
    }
    fn inspector(
        &self,
        name: String,
        count: usize,
        writable: Option<&Shape>,
        any_body: bool,
        any_outline: bool,
    ) -> Node {
        let mut properties = vec![
            kit::heading("boards/selection-title", name),
            self.swatches(),
        ];
        if any_body {
            properties.push(fill_row(self.pen.fill));
        }
        if any_outline {
            properties.push(dash_row(self.pen.dash));
            // Same gate as the dash: both rows are about a line, and the one
            // kind with no line has no use for either.
            properties.push(weight_row(self.pen.weight));
        }
        properties.push(kit::divider("boards/properties-rule"));
        if let Some(shape) = writable {
            properties.push(wide(action(
                "boards/edit-text",
                "Edit text",
                "Enter · double-click",
                Message::EditText,
                true,
            )));
            properties.push(text_size_row(shape.text_size));
            // A text shape IS its words: the box is the block, so there is no
            // room in it for an alignment to move them into and the row would
            // be a control that does nothing. A card has a box wider than its
            // writing, which is the whole of what aligning means.
            if middling(shape.kind) {
                properties.push(align_row(shape.align));
            }
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
            ("⌘ / Ctrl D · Alt or ⌘⇧ drag", "Duplicate selection"),
            ("⌘ / Ctrl ] · [", "Bring to front / send to back"),
            ("⌘ / Ctrl G · Shift G", "Group / ungroup selection"),
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
    pub(super) fn canvas_color(&self) -> [f32; 4] {
        kit::palette().background
    }
    pub(super) fn status(&self) -> String {
        if self.inline.is_some() {
            return "Editing text…".into();
        }
        // "1 change", never "1 changes". One change is the common case — one
        // shape drawn, one colour picked — so the broken reading was the one a
        // user got most of the time, on the chip whose whole job is to say the
        // work is being kept. Counted once here rather than at each arm, so
        // the three states stay one family however they are worded.
        let changes = match self.pending.len() {
            1 => "1 change".to_owned(),
            many => format!("{many} changes"),
        };
        let nothing_pending = self.pending.is_empty();
        match &self.delivery {
            Delivery::Failed(_) => "Not saved".into(),
            Delivery::Sending => format!("Saving {changes}…"),
            Delivery::Idle => match nothing_pending {
                true => "Saved".into(),
                false => format!("{changes} waiting"),
            },
        }
    }
    pub(super) fn screen(&self, x: f32, y: f32) -> [f32; 2] {
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
            if let Some(label) = self.label(board, id, s, box_, opacity, share) {
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
    pub(super) fn on_screen(&self, board: &Board, s: &Shape) -> Option<[f32; 4]> {
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
        // An emptied shape is its outline and nothing else: what is behind it
        // shows through, which is the whole point of drawing a box around a
        // cluster of notes rather than over them.
        let body = |strength: f32| match s.fill {
            Fill::Solid => Some(Rgba(alpha(fill(s.color), strength * opacity))),
            Fill::None => None,
        };
        let edge = |strength: f32| Rgba(alpha(tint(s.color), strength * opacity));
        // The weight multiplies AFTER the zoom clamp, not before it. Folded in
        // first, a thick line at 300% would hit the ceiling the clamp is there
        // to enforce and come out the same width as a heavy one — the two steps
        // a user picked between collapsing into one exactly when the board is
        // zoomed in far enough to tell them apart.
        let heft = heft(s.weight);
        let line_width = (1.5 * self.zoom).clamp(1., 8.) * heft;
        let radius = (6. * self.zoom).clamp(2., 20.);
        match s.kind {
            Kind::Note => out.push(rectangle(
                pos,
                size,
                body(1.),
                edge(0.35),
                heft,
                radius,
                s.dash,
            )),
            Kind::Rectangle => out.push(rectangle(
                pos,
                size,
                body(0.45),
                edge(1.),
                line_width,
                radius,
                s.dash,
            )),
            Kind::Ellipse => out.push(ellipse(pos, size, body(0.45), edge(1.), line_width, s.dash)),
            Kind::Diamond => out.push(diamond(pos, size, body(0.45), edge(1.), line_width, s.dash)),
            // text carries no body: the words are the shape
            Kind::Text => {}
            Kind::Arrow | Kind::Line | Kind::Draw => {
                let world = interaction::stroke(board, s);
                let screen: Vec<_> = world.iter().map(|p| self.screen(p[0], p[1])).collect();
                let ink = Ink {
                    color: edge(1.),
                    dash: s.dash,
                    weight: s.weight,
                };
                self.paint_stroke(s.kind, &screen, ink, budget, out);
            }
        }
    }
    fn paint_stroke(
        &self,
        kind: Kind,
        screen: &[[f32; 2]],
        ink: Ink,
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
        // A connector bent by hand has exactly one interior sample and it is a
        // handle somebody put where they wanted the line. Thinning could drop
        // it and the smoothing would ride past it either way, so a bent
        // connector is drawn through its samples instead.
        let bent = kind != Kind::Draw && screen.len() == 3;
        let path = match bent {
            true => screen.to_vec(),
            false => decimate(&interaction::thin(screen, 0.75), limit),
        };
        let end = path[path.len() - 1];
        // A freehand nib is fatter than a ruled line before anybody chooses a
        // weight; the weight scales whichever of the two this is.
        let nib = if kind == Kind::Draw { 2.5 } else { 1.8 };
        let width = (nib * self.zoom).clamp(1.2, 14.) * heft(ink.weight);
        out.push(Draw::Draw {
            shape: Geometry::Path(match bent {
                true => through(&path),
                false => polyline(&path),
            }),
            fill: None,
            even_odd: false,
            stroke: Some(pen(ink.color, width, ink.dash)),
        });
        if kind != Kind::Arrow {
            // The head is the one part of a run that is never broken: a dashed
            // arrowhead is two short ticks that read as noise beside the line
            // rather than as the thing the line is pointing at.
            return;
        }
        // The head points the way the line arrives, which on a curve is the way
        // its last control point leaves — not the way the sample before it lies.
        let before = match bent {
            true => bend_control(&path),
            false => path[path.len() - 2],
        };
        let angle = (end[1] - before[1]).atan2(end[0] - before[0]);
        let head = (12. * self.zoom).clamp(7., 30.);
        for turn in [-0.5_f32, 0.5] {
            out.push(line(
                end,
                [
                    end[0] - head * (angle + turn).cos(),
                    end[1] - head * (angle + turn).sin(),
                ],
                ink.color,
                width,
            ));
        }
    }
    /// How a card's words are laid out. The painter and the inline editor both
    /// ask, and both get this answer, so a label never changes size or jumps to
    /// a different corner the moment you start typing — which would read as the
    /// editor having its own opinion about the text rather than showing yours.
    /// The native editor writes from the top-left of the box it is given and
    /// cannot be aligned inside it, so wherever the painter puts a card's words
    /// the editor has to be able to put them too: the box the caret lives in
    /// carries the alignment instead of the words, and it is sized and placed
    /// off this same answer.
    pub(super) fn lettering(&self, s: &Shape, size: [f32; 2]) -> Lettering {
        let plain = s.kind == Kind::Text;
        let edge = CARD_INSET * self.zoom;
        // an ellipse and a diamond pinch away from their corners, so their
        // words start further in — far enough to sit on the body, not beside it
        let pinch = match s.kind {
            Kind::Ellipse => 0.14,
            Kind::Diamond => 0.22,
            Kind::Note | Kind::Rectangle | Kind::Text => 0.,
            Kind::Arrow | Kind::Line | Kind::Draw => 0.,
        };
        // A card's label is smaller than a text shape's writing at the same
        // step — a label sits inside a box that has other things in it, and
        // the writing IS the thing — and the step multiplies whichever it is,
        // so choosing a size says the same thing on both.
        let asked = if plain { 20. } else { 14. } * step(s.text_size) * self.zoom;
        let drawn = asked.clamp(SMALLEST, 60.);
        Lettering {
            size: drawn,
            leading: drawn * LEADING,
            inset: edge + size[0].min(size[1]) * pinch,
            legible: asked >= SMALLEST,
        }
    }
    /// The box a shape's words are written in, on screen: where the painter
    /// writes them and where the editor opens over them. A card writes inside
    /// its own outline. A connector has none to write in, so its words ride a
    /// plate at the middle of the run — the middle of where the run is drawn
    /// now, which a bound end moves every time the card it holds does.
    /// A shape's box on screen: where it is being drawn now if the board can
    /// say, and where it was stored otherwise.
    pub(super) fn card_box(&self, board: &Board, s: &Shape) -> [f32; 4] {
        let origin = self.screen(s.x as f32, s.y as f32);
        let stored = [
            origin[0],
            origin[1],
            origin[0] + s.width as f32 * self.zoom,
            origin[1] + s.height as f32 * self.zoom,
        ];
        self.on_screen(board, s).unwrap_or(stored)
    }
    /// Where the caret lives on screen right now, for the shape being written
    /// on. The painter opens the editor here and the pointer asks the same
    /// question of the same function, so a press either lands in the field or
    /// is known not to have.
    pub(super) fn typing_box(&self, board: &Board, s: &Shape) -> ([f32; 2], [f32; 2]) {
        let Some(inline) = &self.inline else {
            return ([0.; 2], [0.; 2]);
        };
        let (pos, size) = self.writing_box(board, s, self.card_box(board, s));
        self.caret_box(inline, s, pos, size)
    }
    pub(super) fn writing_box(
        &self,
        board: &Board,
        s: &Shape,
        box_: [f32; 4],
    ) -> ([f32; 2], [f32; 2]) {
        if !s.kind.is_path() {
            return (
                [box_[0], box_[1]],
                [(box_[2] - box_[0]).max(1.), (box_[3] - box_[1]).max(1.)],
            );
        }
        // The plate rides the camera like everything else on the board: one
        // that kept its pixels while the run under it shrank would swallow the
        // whole drawing at a distance. Where it sits is the hit test's answer
        // and not a second opinion — that is the difference between a
        // double-click that opens a label and one that opens the board.
        let plate = [PLATE[0] * self.zoom, PLATE[1] * self.zoom];
        let at = interaction::plate(&interaction::stroke(board, s));
        let middle = self.screen((at[0] + at[2]) / 2., (at[1] + at[3]) / 2.);
        (
            [middle[0] - plate[0] / 2., middle[1] - plate[1] / 2.],
            plate,
        )
    }
    /// The box the caret lives in while you type, which is the room the words
    /// are given for a card and the plate they hug for a connector. A card is
    /// written across the whole card, so the two are the same thing; a
    /// connector's words are centred on its line once saved, and an editor
    /// given the whole room would write them from the room's left edge and
    /// throw them a hundred units across the board the moment you were done.
    /// The host does not centre text inside an editor, so the box is placed
    /// where the words are instead and writing starts at its left edge. That
    /// is the one thing the box has to do twice: say where the words START and
    /// say where they BREAK. A box set against a card that is not left-aligned
    /// therefore reaches past the column by the slack the alignment spent, and
    /// a line whose last word falls in that slack is a line the editor keeps
    /// and the label breaks. It is a fraction of a word on one card in a
    /// handful, where reading the wrap off the measurement was a whole word on
    /// every card that wrapped at all — and it closes for good the day the wire
    /// can align an editor's text, which is the only way one box stops having
    /// to answer both questions.
    pub(super) fn caret_box(
        &self,
        inline: &Inline,
        s: &Shape,
        pos: [f32; 2],
        room: [f32; 2],
    ) -> ([f32; 2], [f32; 2]) {
        let kind = s.kind;
        // A shape's words sit in the middle of it, so the editor's box is the
        // words' own box placed in the middle rather than the whole card: the
        // editor writes from the top-left of whatever box it is given and there
        // is no verb on the wire for aligning it, so the box IS the alignment.
        // Until the first measurement lands there is nothing to centre on and
        // the box is the room itself.
        let Some(words) = inline.wide else {
            return (pos, room);
        };
        let letters = self.lettering(s, room);
        let wide = words * self.zoom;
        // A text shape IS its words: its corner is where you put it, so they
        // start there and the box is only the room they need to be written in.
        // It has no column to wrap against, so the margin past the last glyph
        // is room for the caret and nothing else wraps because of it.
        if kind == Kind::Text {
            return (pos, [(wide + margin(&letters)).max(1.), room[1]]);
        }
        // The box is as wide as the COLUMN the card wraps in — not as wide as
        // the words came out in it. `wide` is where the words happened to land,
        // which is always a little short of where they were allowed to: a line
        // ends at the last word that fitted, and the room left after it is room
        // the next word needed and could not have. Wrapping the editor at that
        // measurement hands it a column narrower than the label's by exactly
        // that leftover, so the editor breaks a line the label keeps whole and
        // the words rewrap the moment you stop typing. It also feeds the
        // measurement back into the thing being measured, which is a box that
        // can only ever walk shut.
        //
        // The column does not depend on the words, so both sides wrap in the
        // same place by construction and neither can drift.
        //
        // A connector shares it too. Its plate IS the room it was given, which
        // is what `column` answers for a path, and the box took the MEASUREMENT
        // instead for as long as the two questions this box answers were read
        // as one: the words are centred on the line by where the box STARTS,
        // never by how wide it is. A box sized to the words was a field forty
        // pixels wide against a plate that wrapped at a hundred and seventy-six
        // — the padding subtracted once by the plate and again by the pin — so
        // a single word re-wrapped the moment you clicked into it.
        // …plus the strip the field keeps for its caret, so that what is left
        // to wrap in is the column itself and not the column less the reserve.
        let width = column(kind, room[0], &letters, self.zoom) + WRAP_RESERVE;
        // It is the WORDS that are set against the card, not the box around
        // them: the box carries a margin the painter's block does not, and
        // aligning the box would spend that margin pushing the words off the
        // edge they were asked to sit on.
        let slack = room[0] - wide;
        let x = pos[0]
            + match s.align {
                Align::Start => 0.,
                Align::Middle => (slack / 2.).max(0.),
                Align::End => slack.max(0.),
            };
        // A connector's plate is centred in the room the same way, so it takes
        // the same drop: the plate hugs the words and the room is the widest
        // label a line may carry, so its top is the top of the ROOM only while
        // the words fill it. Left out of this, a label sat a few pixels above
        // the plate that replaced it.
        let Some(tall) = inline.grown else {
            return ([x, pos[1]], [width, room[1]]);
        };
        // A card's box is DRAWN, so its words never start above it however many
        // of them there are — the card grows to hold them instead. A plate is
        // not drawn until it is saved and it only floats over the run, so a
        // label too tall for the room it was allowed still centres on the line
        // and hangs off both ends. Clamping it to the room's top is what left a
        // two-line label three pixels above the plate it became.
        let floats = !middling(kind);
        let room_to_spare = (room[1] - tall * self.zoom) / 2.;
        let down = match floats {
            true => room_to_spare,
            false => room_to_spare.max(0.),
        };
        ([x, pos[1] + down], [width, room[1] - down])
    }
    /// The box a text shape's words have come to, in board units: the words
    /// themselves plus the margin the caret needs around them. It follows them
    /// in both directions because the measurement it reads does not depend on
    /// the box — a text shape has no column, so nothing about its size can feed
    /// back into the size of its words.
    pub(super) fn hugged_width(&self, inline: &Inline, shape: &Shape) -> i32 {
        let Some(words) = inline.wide else {
            return shape.width;
        };
        let room = [
            shape.width as f32 * self.zoom,
            shape.height as f32 * self.zoom,
        ];
        let letters = self.lettering(shape, room);
        let hugged = words + margin(&letters) / self.zoom;
        hugged
            .ceil()
            .clamp(MIN_CARD[0] as f32, boards_wire::MAX_SIZE as f32) as i32
    }
    /// A shape's words, pinned over its body and clipped to it.
    fn label(
        &self,
        board: &Board,
        id: &str,
        s: &Shape,
        box_: [f32; 4],
        opacity: f32,
        share: usize,
    ) -> Option<Node> {
        let p = kit::palette();
        let riding_a_line = s.kind.is_path();
        let editing = self.inline.as_ref().filter(|inline| inline.id == *id);
        // A card keeps its own body under the caret — fill, outline and all —
        // so the painter draws nothing for the shape being written in and the
        // editor's words are the only ones on it. A connector has no body: its
        // plate IS its label, so drawing nothing took the wash out from under
        // the words the moment you clicked in and put it back when you left.
        // The plate stays, sized to the words being TYPED and drawn in no ink
        // at all, and the editor lays those words over it.
        let typed = match editing {
            Some(inline) if riding_a_line => Some(inline.document.text()),
            Some(_) => return None,
            None => None,
        };
        let letters = self.lettering(s, [box_[2] - box_[0], box_[3] - box_[1]]);
        if !letters.legible {
            return None;
        }
        let label = match &typed {
            // The SOURCE, laid out the way the gauge lays it out: the editor
            // holds the markers and the plate has to be as wide as what can be
            // seen in it, which is the wider of the two.
            Some(words) => headed(
                &format!("boards/label/{id}"),
                excerpt(words, share),
                &letters,
                alpha(p.foreground, 0.),
            ),
            None => {
                let blank = s.text.is_empty();
                if blank && !invites_a_word(s.kind) {
                    return None;
                }
                let text = if blank {
                    PROMPT
                } else {
                    excerpt(&s.text, share)
                };
                let ink = alpha(if blank { p.faint } else { p.foreground }, opacity);
                written(id, text, &letters, ink)
            }
        };
        let (pos, size) = self.writing_box(board, s, box_);
        let body = match riding_a_line {
            // The plate's whole job is to rub the run out from under the
            // words, and only the paper's own colour does that. A washed
            // surface is a chip printed OVER the line instead — eight points
            // of grey on a white board, which is a piece of UI sitting among
            // the drawing. It comes from `canvas_color` so the wash and what
            // it washes over can never be two decisions.
            true => plate(
                id,
                label,
                &letters,
                alpha(self.canvas_color(), opacity),
                size,
            ),
            false => card_words(
                id,
                label,
                &letters,
                s.align,
                middling(s.kind),
                column(s.kind, size[0], &letters, self.zoom),
            ),
        };
        Some(Node::Pin {
            key: format!("boards/pin/{id}"),
            x: pos[0],
            y: pos[1],
            width: Some(Length::Fixed(size[0])),
            height: Some(Length::Fixed(size[1])),
            content: Box::new(body),
        })
    }
    /// What belongs to the pointer, not to the board: the selection, its
    /// handles, the snapping guides and whatever the current gesture is about
    /// to leave behind.
    pub(super) fn paint_marks(&self, board: &Board, budget: usize, out: &mut Vec<Draw>) {
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
                CHROME,
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
                CHROME,
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
                stroke: Some(pen(accent, 1.5, CHROME)),
            };
            // A connector is taken by its ends, a card by its corners: the grip
            // a shape offers is the edit it can be given, and they differ.
            if s.kind.is_path() {
                let run = interaction::stroke(board, s);
                for end in [run.first(), run.last()].into_iter().flatten() {
                    out.push(grip(self.screen(end[0], end[1])));
                }
                // The bend is offered the same way whether it has been used or
                // not, so it is drawn hollow: it is a place the line will go,
                // not a place the line is.
                if let Some((_, _, at)) = self.bend(s, &run) {
                    let at = self.screen(at[0], at[1]);
                    out.push(Draw::Draw {
                        shape: Geometry::Rectangle {
                            position: [at[0] - 3., at[1] - 3.],
                            size: [6., 6.],
                            radius: [3.; 4],
                        },
                        fill: Some(Rgba(p.background)),
                        even_odd: false,
                        stroke: Some(pen(Rgba(alpha(p.accent, 0.6)), 1.5, CHROME)),
                    });
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
                CHROME,
            ));
            let grip = |at: [f32; 2]| Draw::Draw {
                shape: Geometry::Rectangle {
                    position: [at[0] - 3.5, at[1] - 3.5],
                    size: [7., 7.],
                    radius: [1.5; 4],
                },
                fill: Some(Rgba(p.background)),
                even_odd: false,
                stroke: Some(pen(accent, 1.5, CHROME)),
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
    /// The card an arrow's end would take if it were let go here, ringed. An
    /// end in hand says what it is about to hold, so releasing is a decision
    /// you already made rather than one you find out about afterwards.
    fn ring_the_card(&self, board: &Board, kind: Kind, point: [f32; 2], out: &mut Vec<Draw>) {
        let p = kit::palette();
        let Some(bond) = self.holding(board, kind, point) else {
            return;
        };
        let Some(record) = board.shapes.get(&bond.card) else {
            return;
        };
        let card = &record.shape;
        out.push(rectangle(
            self.screen(card.x as f32, card.y as f32),
            [
                card.width as f32 * self.zoom,
                card.height as f32 * self.zoom,
            ],
            Some(Rgba(alpha(p.accent, 0.08))),
            Rgba(p.accent),
            2.,
            (6. * self.zoom).clamp(2., 20.),
            CHROME,
        ));
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
                    CHROME,
                ));
            }
            Gesture::Create { kind, start, point } => {
                let shape = self.creation_shape(*kind, *start, *point);
                if kind.is_path() {
                    // An arrow being drawn says what it would take hold of at
                    // both ends, the same way one being re-routed does: the
                    // cards it is about to bind are ringed while you drag, not
                    // reported by the line jumping to their edges after you let
                    // go.
                    for end in [*start, *point] {
                        self.ring_the_card(board, *kind, end, out);
                    }
                    // And it is drawn as the shape it would become, stopping at
                    // the borders of the cards it is taking rather than running
                    // on into them and snapping back when you let go. Below the
                    // threshold there is no shape yet, so the raw run stands in.
                    let run = self.drawn_shape(*kind, *start, *point).map_or_else(
                        || interaction::path_points(&shape),
                        |bound| interaction::stroke(board, &bound),
                    );
                    let screen: Vec<_> = run.iter().map(|q| self.screen(q[0], q[1])).collect();
                    let ink = Ink {
                        color: accent,
                        dash: shape.dash,
                        weight: shape.weight,
                    };
                    self.paint_stroke(*kind, &screen, ink, 8, out);
                    return;
                }
                // A card is drawn AS the card it will be, in the ink it will
                // keep: an ellipse is an ellipse while you make it. A blue box
                // that turns into an ellipse when you let go is a preview of
                // the gesture, not of the shape, and it tells you nothing
                // about what you are about to put on the board.
                self.paint(board, &shape, 0.8, PREVIEW, out);
                // And nothing else. A shape that draws its own outline needs
                // no box drawn around it: over an ellipse that box is a second
                // outline belonging to no shape, and while the ellipse is
                // small the two read as one rounded rectangle that turns into
                // a circle as it grows — which is the jump this preview was
                // put here to remove.
                let its_own_outline = shape.kind != Kind::Text;
                if its_own_outline {
                    return;
                }
                // A text shape has no body: the ring is the only thing that
                // says how much room it is taking. No wash under it either —
                // a tint would report a colour the words will not be in.
                out.push(rectangle(
                    self.screen(shape.x as f32, shape.y as f32),
                    [
                        shape.width as f32 * self.zoom,
                        shape.height as f32 * self.zoom,
                    ],
                    None,
                    accent,
                    1.,
                    (6. * self.zoom).clamp(2., 20.),
                    CHROME,
                ));
            }
            Gesture::Sketch { points } => {
                let screen: Vec<_> = points.iter().map(|q| self.screen(q[0], q[1])).collect();
                // A stroke is drawn in the pen it will be kept in, and the pen
                // a new shape gets is the board's current one.
                let ink = Ink {
                    color: Rgba(tint(self.pen.color)),
                    dash: self.pen.dash,
                    weight: self.pen.weight,
                };
                self.paint_stroke(Kind::Draw, &screen, ink, boards_wire::MAX_POINTS, out);
            }
            // Only an end reaches for a card; a bend crossing one binds nothing
            // and must not say that it would.
            Gesture::Endpoint {
                point, shape, end, ..
            } if interaction::reaches_for_a_card(shape, *end) => {
                self.ring_the_card(board, shape.kind, *point, out)
            }
            Gesture::Endpoint { .. } => {}
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
/// Out of that, what the shape in flight may spend drawing itself. A card
/// costs one piece; only a stroke can want more, and a stroke being drawn is
/// already thinned to what the board will store.
const PREVIEW: usize = 8;

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
/// The control point that makes one quadratic pass exactly through the sample
/// between its ends: a quadratic sits halfway between its control and the chord
/// at t=½, so the control is twice the sample less the chord's middle.
fn bend_control(points: &[[f32; 2]]) -> [f32; 2] {
    let [first, middle, last] = points[..] else {
        return points[points.len().saturating_sub(2)];
    };
    [
        2. * middle[0] - (first[0] + last[0]) / 2.,
        2. * middle[1] - (first[1] + last[1]) / 2.,
    ]
}
/// A curve THROUGH the sample it was bent by rather than near it. `polyline`'s
/// smoothing treats every sample as a control and rides past it, which is right
/// for ink — a pen's jitter should not be honoured — and wrong for a connector,
/// where the one interior sample is a handle somebody placed. A line that does
/// not go where the handle went is a handle that does not work.
fn through(points: &[[f32; 2]]) -> Vec<wire::CanvasSegment> {
    use wire::CanvasSegment as Segment;
    let [first, _, last] = points[..] else {
        return polyline(points);
    };
    vec![
        Segment::Move(first),
        Segment::Quadratic {
            control: bend_control(points),
            end: last,
        },
    ]
}
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
/// A card's words: the top-left of its own box, clipped to it, because a card
/// is a page and a page fills from its corner.
/// The room a card keeps around its column, because the native editor keeps
/// room inside the box it is given that a plain label does not: with the same
/// words in the same column it takes one line more than the label does. It
/// rides the type size so a card's height is the same whatever the camera is
/// doing — a box that reflowed as you zoomed would resize itself for being
/// looked at.
///
/// The label, the gauge that measures it, the caret and the box a text shape
/// hugs its words with all read this one description of it.
pub(super) fn margin(letters: &Lettering) -> f32 {
    2. * letters.size
}
/// What one step of the type ladder multiplies the writing by. Four steps and
/// not a slider: the size decides the column a card wraps in and the box a
/// text shape hugs, so every step has to leave writing a board can hold and a
/// box can be fitted to.
pub(super) fn step(text_size: TextSize) -> f32 {
    match text_size {
        TextSize::Small => 0.72,
        TextSize::Medium => 1.,
        TextSize::Large => 1.45,
        TextSize::Huge => 2.1,
    }
}
/// The column a shape's words are written in, on screen.
pub(super) fn column(kind: Kind, room: f32, letters: &Lettering, zoom: f32) -> f32 {
    match kind {
        // A text shape has no column. It IS its words: as wide as the longest
        // line, breaking only where you broke it. Wrapping it inside its own
        // box would make the box the column, and a box that is its own column
        // walks itself shut — each measurement narrower than the one that sized
        // the box it was measured in.
        Kind::Text => boards_wire::MAX_SIZE as f32 * zoom,
        Kind::Note | Kind::Rectangle | Kind::Ellipse | Kind::Diamond => {
            (room - margin(letters)).max(40. + 2. * letters.inset)
        }
        // A connector's plate is the room, and the room is already the widest
        // label a line is allowed to carry.
        Kind::Arrow | Kind::Line | Kind::Draw => room,
    }
}
/// Whether a shape's words belong in the middle of it. A card drawn as a box
/// carries its label in the centre, the way every canvas app does; a text shape
/// IS its words, so its corner is where you put it and they start there.
///
/// One answer, read by the painter and by the caret, because a label that is
/// centred when it is drawn and top-left when it is typed in is the same defect
/// as one that moves when you save it.
pub(super) fn middling(kind: Kind) -> bool {
    !kind.is_path() && kind != Kind::Text
}
/// Whether this kind is painted behind its outline at all. A run is a stroke
/// with nothing inside it and a text shape is its words, so emptying either
/// changes nothing anyone can see.
pub(super) fn has_body(kind: Kind) -> bool {
    middling(kind)
}
/// Whether this kind draws a line that could be broken — a card's border and a
/// run's stroke both do; a text shape draws neither.
pub(super) fn has_outline(kind: Kind) -> bool {
    kind != Kind::Text
}
/// Set the line spacing on a run of words. Absolute and not a ratio: the
/// editor multiplies a ratio by the size it was told, the painter by the size
/// the window carries, and the two only agree on a number of pixels.
fn led(mut node: Node, letters: &Lettering) -> Node {
    let height = Some(wire::LineHeight::Absolute(letters.leading));
    match &mut node {
        Node::Text { options, .. } | Node::RichText { options, .. } => options.line_height = height,
        _ => panic!("led requires text"),
    }
    node
}
/// A card's words as the board draws them: what was typed, with its markers
/// spent. See [`crate::markdown`] for the vocabulary.
///
/// A card wearing no marker is drawn the way the board has always drawn one —
/// ONE text node, one paragraph, one wrap. Only a card that actually wears a
/// marker becomes a column of styled lines, because a column is several
/// paragraphs and several paragraphs do not wrap quite like one.
fn written(id: &str, text: &str, letters: &Lettering, ink: [f32; 4]) -> Node {
    let lines = markdown::read(text);
    if !markdown::marked(&lines) {
        let plain = kit::text_size(
            kit::wrapping(kit::text(format!("boards/label/{id}"), text)),
            letters.size,
        );
        return kit::colored(led(plain, letters), ink);
    }
    let drawn = lines
        .iter()
        .enumerate()
        .map(|(row, line)| written_line(format!("boards/label/{id}/{row}"), line, letters, ink));
    let mut block = kit::column(format!("boards/label/{id}"), drawn);
    if let Node::Linear { spacing, .. } = &mut block {
        // The line height already holds the lines apart. A gap on top of it
        // would be a second one, and the card's lines would drift out of step
        // with the same lines under the caret.
        *spacing = Some(0.);
    }
    block
}
/// One line of a marked-up card.
fn written_line(key: String, line: &markdown::Line, letters: &Lettering, ink: [f32; 4]) -> Node {
    let size = letters.size * line.scale;
    let mut spans: Vec<wire::RichSpan> = Vec::new();
    if !line.lead.is_empty() {
        spans.push(inked(
            &line.lead,
            markdown::Face::default(),
            line.heavy,
            size,
            ink,
        ));
    }
    for run in &line.runs {
        spans.push(inked(&run.text, run.face, line.heavy, size, ink));
    }
    // A blank line is a blank line and not a line that vanished: a paragraph
    // with nothing in it is nothing tall, so it is given a space to be tall
    // with.
    if spans.is_empty() {
        spans.push(inked(" ", markdown::Face::default(), false, size, ink));
    }
    let words = Node::RichText {
        key,
        spans,
        size: Some(size),
        color: Some(wire::Rgba(ink)),
        font: Default::default(),
        width: Some(Length::Fill),
        align_x: None,
        options: wire::TextOptions {
            wrapping: Some(wire::Wrapping::WordOrGlyph),
            ..Default::default()
        },
        on_link: None,
    };
    // A heading is taller than the body it heads, and its lines have to be
    // held that much further apart or a wrapped one writes over itself.
    let stepped = Lettering {
        size,
        leading: letters.leading * line.scale,
        inset: letters.inset,
        legible: letters.legible,
    };
    led(words, &stepped)
}
/// The text exactly as typed — markers and all — but laid out line by line at
/// the sizes the card will draw those lines at. Nothing styles it beyond that:
/// this is what the gauge measures, and a measurement only wants the geometry.
fn headed(key: &str, text: &str, letters: &Lettering, ink: [f32; 4]) -> Node {
    let lines = markdown::read(text);
    let sized = lines.iter().any(|line| line.scale != 1.);
    if !sized {
        let plain = kit::text_size(
            kit::wrapping(kit::text(format!("{key}-text"), text)),
            letters.size,
        );
        return kit::colored(led(plain, letters), ink);
    }
    let drawn = text
        .split('\n')
        .zip(&lines)
        .enumerate()
        .map(|(row, (source, line))| {
            let stepped = Lettering {
                size: letters.size * line.scale,
                leading: letters.leading * line.scale,
                inset: letters.inset,
                legible: letters.legible,
            };
            // A blank line is still a line tall, here as on the card.
            let held = if source.is_empty() { " " } else { source };
            let words = kit::text_size(
                kit::wrapping(kit::text(format!("{key}-text/{row}"), held)),
                stepped.size,
            );
            kit::colored(led(words, &stepped), ink)
        });
    let mut block = kit::column(format!("{key}-block"), drawn);
    if let Node::Linear { spacing, .. } = &mut block {
        *spacing = Some(0.);
    }
    block
}
/// One run of a line, in the face its markers asked for.
fn inked(
    text: &str,
    face: markdown::Face,
    heavy: bool,
    size: f32,
    ink: [f32; 4],
) -> wire::RichSpan {
    let p = kit::palette();
    let bold = face.bold || heavy;
    wire::RichSpan {
        content: text.to_owned(),
        size: Some(size),
        font: Some(wire::NamedFont {
            family: match face.code {
                true => wire::FontFamily::Monospace,
                false => wire::FontFamily::SansSerif,
            },
            weight: match bold {
                true => wire::Weight::Bold,
                false => wire::Weight::Normal,
            },
            stretch: wire::FontStretch::Normal,
            style: match face.italic {
                true => wire::FontStyle::Italic,
                false => wire::FontStyle::Normal,
            },
        }),
        color: Some(wire::Rgba(ink)),
        // Code wears a plate so it reads as code and not as a font someone
        // chose. No padding: a plate that pushed its neighbours apart would
        // move the words either side of it off the line they share.
        background: face
            .code
            .then(|| wire::Rgba(alpha(p.surface_raised, ink[3] * 0.9))),
        strikethrough: face.struck,
        ..Default::default()
    }
}
fn card_words(
    id: &str,
    words: Node,
    letters: &Lettering,
    align: Align,
    middling: bool,
    room: f32,
) -> Node {
    let mut held = kit::container(format!("boards/label-box/{id}"), words);
    if let Node::Container {
        padding,
        width,
        max_width,
        ..
    } = &mut held
    {
        // The card is the words plus the room they are written in.
        *padding = Some(wire::Edges::all(letters.inset));
        // The BLOCK of words is as wide as its longest line and no wider, so
        // that the middle of the block is somewhere the caret can reach. The
        // lines inside it stay where they fall: centring each line would look
        // better and the caret could not follow it — the native editor writes
        // from the left edge of the box it is given and there is no verb on the
        // wire for aligning it, so a line centred here would be a line that
        // jumped the moment you clicked on it.
        //
        // Wrapping rides the box's max width rather than the text's own width,
        // because a text told to fill cannot shrink, and a block that cannot
        // shrink has no middle of its own — nor any width for a text shape to
        // take as its own.
        *width = Some(Length::Shrink);
        *max_width = Some(room.max(1.));
    }
    let mut clip = kit::container(format!("boards/label-clip/{id}"), held);
    if let Node::Container {
        clip: clipped,
        width,
        height,
        align_x,
        align_y,
        ..
    } = &mut clip
    {
        *clipped = true;
        // The card itself, so the block has something to be in the middle of.
        *width = Some(Length::Fill);
        *height = Some(Length::Fill);
        // Where the BLOCK of words sits against the card. The lines inside it
        // keep falling from its left edge, because that is the one thing the
        // native editor can also do: a line centred here would be a line that
        // jumped the moment you clicked on it.
        //
        // A text shape IS its words, so its block is its box and there is
        // nothing for this to move — its corner is where you put it.
        let set = match align {
            Align::Start => wire::AlignX::Left,
            Align::Middle => wire::AlignX::Center,
            Align::End => wire::AlignX::Right,
        };
        *align_x = middling.then_some(set);
        *align_y = middling.then_some(wire::AlignY::Center);
    }
    clip
}
/// A connector's words on a plate at the middle of its run: the plate hugs the
/// words rather than filling the room set aside for them, because the room is
/// sized for the longest label a line could carry and a plate that big would
/// rub out the line either side of a short one. The words are centred in that
/// room, which is what puts them ON the line instead of beside it.
fn plate(id: &str, words: Node, letters: &Lettering, wash: [f32; 4], room: [f32; 2]) -> Node {
    let mut riding = kit::container(format!("boards/plate/{id}"), words);
    if let Node::Container {
        clip: clipped,
        width,
        height,
        max_width,
        padding,
        background,
        align_x,
        align_y,
        ..
    } = &mut riding
    {
        *clipped = true;
        *width = Some(Length::Shrink);
        // No height: the plate is as tall as its words either way, and saying
        // Shrink here would ALSO refuse the stretch — which is how the plate
        // would stop obeying the centring its parent puts it on the line with.
        *height = None;
        *max_width = Some(room[0]);
        *padding = Some(wire::Edges::all(letters.inset));
        *background = Some(wire::Background::Color(Rgba(wash)));
        *align_x = Some(wire::AlignX::Center);
        *align_y = Some(wire::AlignY::Center);
    }
    let mut middle = kit::container(format!("boards/plate-centre/{id}"), riding);
    if let Node::Container {
        height,
        align_x,
        align_y,
        ..
    } = &mut middle
    {
        *height = Some(Length::Fill);
        *align_x = Some(wire::AlignX::Center);
        *align_y = Some(wire::AlignY::Center);
    }
    middle
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
        // how a card's words are set: lines of writing flush to one side
        "text-start" => "<path d='M4 6h16M4 11h10M4 16h13M4 21h8'/>",
        "text-middle" => "<path d='M4 6h16M7 11h10M5 16h14M8 21h8'/>",
        "text-end" => "<path d='M4 6h16M10 11h10M7 16h13M12 21h8'/>",
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
        // whether the body behind the outline is painted: a hatched box and an
        // empty one. Hatching rather than a solid square because every icon
        // here is drawn in strokes and a filled one would read as a different
        // family of control.
        "fill-solid" => {
            "<rect x='4' y='4' width='16' height='16' rx='3'/><path d='M5 11 11 5M5 17 17 5M8 19 19 8M14 20 20 14'/>"
        }
        "fill-none" => "<rect x='4' y='4' width='16' height='16' rx='3'/>",
        // whether the outline is unbroken
        "dash-solid" => "<path d='M3 12h18'/>",
        "dash-dashed" => "<path d='M3 12h4M10 12h4M17 12h4'/>",
        // how heavy it is: the same line the dash row draws, four times, each
        // overriding the sheet's stroke width with its own. The icon IS the
        // thing it sets, which is the only way a weight control can be read
        // without a label.
        "weight-thin" => "<path d='M3 12h18' stroke-width='1'/>",
        "weight-medium" => "<path d='M3 12h18' stroke-width='2'/>",
        "weight-thick" => "<path d='M3 12h18' stroke-width='3.5'/>",
        "weight-heavy" => "<path d='M3 12h18' stroke-width='5.5'/>",
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
/// The four steps of the type ladder, the one this shape is at checked.
fn text_size_row(current: TextSize) -> Node {
    kit::spaced(
        kit::row(
            "boards/text-size",
            [
                (TextSize::Small, "small", "A", "Small", 10.),
                (TextSize::Medium, "medium", "A", "Medium", 13.),
                (TextSize::Large, "large", "A", "Large", 17.),
                (TextSize::Huge, "huge", "A", "Huge", 21.),
            ]
            .map(|(step, key, letter, label, size)| {
                letter_button(
                    key,
                    letter,
                    label,
                    size,
                    Message::Lettering(step),
                    step == current,
                )
            }),
        ),
        4.,
    )
}
/// Whether the shape's body is painted, the one it is set to checked.
fn fill_row(current: Fill) -> Node {
    kit::spaced(
        kit::row(
            "boards/fill",
            [
                (Fill::Solid, "fill-solid", "Filled"),
                (Fill::None, "fill-none", "Outline only"),
            ]
            .map(|(fill, name, label)| {
                icon_button(
                    &format!("boards/fill/{name}"),
                    name,
                    label,
                    "Whether the shape is painted behind its outline",
                    Message::Painting(fill),
                    true,
                    fill == current,
                )
            }),
        ),
        4.,
    )
}
/// Whether the shape's outline is unbroken, the one it is set to checked.
fn dash_row(current: Dash) -> Node {
    kit::spaced(
        kit::row(
            "boards/dash",
            [
                (Dash::Solid, "dash-solid", "Solid line"),
                (Dash::Dashed, "dash-dashed", "Dashed line"),
            ]
            .map(|(dash, name, label)| {
                icon_button(
                    &format!("boards/dash/{name}"),
                    name,
                    label,
                    "Whether the outline is drawn unbroken",
                    Message::Outline(dash),
                    true,
                    dash == current,
                )
            }),
        ),
        4.,
    )
}
/// How heavy the shape's line is, the one it is set to checked. Four steps in
/// one row, like the lettering ladder, because they are the same kind of
/// choice: a size picked off a scale rather than a state toggled.
fn weight_row(current: Weight) -> Node {
    kit::spaced(
        kit::row(
            "boards/weight",
            [
                (Weight::Thin, "weight-thin", "Thin line"),
                (Weight::Medium, "weight-medium", "Medium line"),
                (Weight::Thick, "weight-thick", "Thick line"),
                (Weight::Heavy, "weight-heavy", "Heavy line"),
            ]
            .map(|(weight, name, label)| {
                icon_button(
                    &format!("boards/weight/{name}"),
                    name,
                    label,
                    "How heavy the line is",
                    Message::Stroke(weight),
                    true,
                    weight == current,
                )
            }),
        ),
        4.,
    )
}
/// Where this card's words sit across it, the one it is set to checked.
fn align_row(current: Align) -> Node {
    kit::spaced(
        kit::row(
            "boards/text-align",
            [
                (Align::Start, "text-start", "Align left"),
                (Align::Middle, "text-middle", "Align centre"),
                (Align::End, "text-end", "Align right"),
            ]
            .map(|(align, name, label)| {
                icon_button(
                    &format!("boards/align/{name}"),
                    name,
                    label,
                    "Where this card's words sit across it",
                    Message::Align(align),
                    true,
                    align == current,
                )
            }),
        ),
        4.,
    )
}
/// A 28px control labelled with a letter rather than a glyph, checked when
/// `on`, the letter drawn at the size it stands for. Four steps of type size
/// are four sizes of the same letter, and no icon says that as plainly as the
/// letter itself does.
fn letter_button(
    key: &str,
    letter: &str,
    label: &str,
    size: f32,
    message: Message,
    on: bool,
) -> Node {
    let glyph = kit::nowrap(kit::text_size(
        kit::text(format!("boards/letter/{key}"), letter),
        size,
    ));
    let mut node = kit::button_child(
        format!("boards/text-size/{key}"),
        glyph,
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
        *checked = Some(on);
        *width = Some(Length::Fixed(28.));
        *height = Some(Length::Fixed(28.));
        *padding = Some(wire::Edges::all(2.));
        *description = Some("How big this shape's words are".into());
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
        let letters = self.lettering(shape, size);
        let words = inline.document.text();
        // The answer comes back a frame later, against the layout THIS frame
        // set up. The zoom it was laid out at travels with it, because the
        // camera can move in between and a measurement divided by the zoom
        // that arrived after it is a card that grew for stepping back from it.
        let laid_out_at = self.zoom;
        let measure = || {
            Some(slots::handler(Box::new(move |(w, h)| {
                Some(Message::Measured(laid_out_at, w, h))
            })))
        };
        // The SOURCE, at the sizes the card will draw it: the editor holds
        // the markers and the card spends them, so the source is the wider of
        // the two and a heading is the taller. Measuring the one against the
        // other's sizes is the box that fits both, and a card whose first
        // line is a heading neither clips the words under the caret nor the
        // bigger ones it gets back.
        let gauge = headed(
            "boards/gauge",
            excerpt(&words, LETTERS),
            &letters,
            alpha(kit::palette().foreground, 0.),
        );
        // Both halves of the answer, from one measurement: how TALL the words
        // are in the column they are given, and how WIDE the longest of them
        // came out. The card needs the height to grow to and the caret needs
        // the width to be centred on, and a gauge held to a fixed width can
        // only answer the first — it reports the column back whatever is
        // written in it. So the gauge shrinks to its words and wraps at the
        // column instead, which is the same wrap and therefore the same height.
        let room = column(shape.kind, size[0], &letters, self.zoom);
        let mut padded = kit::container("boards/gauge-pad", gauge);
        if let Node::Container {
            padding,
            width,
            max_width,
            ..
        } = &mut padded
        {
            // The card is the words plus the room they are written in, so the
            // gauge carries the same inset and reports a card height, not a
            // text height.
            *padding = Some(wire::Edges::all(letters.inset));
            *width = Some(Length::Shrink);
            *max_width = Some(room.max(1.));
        }
        Node::Pin {
            key: "boards/gauge-pin".into(),
            x: pos[0],
            y: pos[1],
            // Neither width nor height: this is the one node on the stage
            // allowed to be exactly as big as it likes, because its size IS the
            // answer. A pin held to the card's width hands that width straight
            // back down to a container that shrinks, and the gauge reports the
            // column it was given rather than the words in it. The column lives
            // on the container's max_width instead, which wraps the words
            // without stretching them.
            width: None,
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
            // Both claimed keys leave the card keeping what you wrote, and
            // they still part here: ⌘Enter is only ever a way out of the
            // writing, while Escape also answers for everything else the board
            // is holding — a tool, a selection, a menu — so it goes through
            // the one path that knows about all of them.
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
        let letters = self.lettering(shape, size);
        let editor = Node::Editor {
            key: format!("boards/editor/{}", inline.id),
            document,
            on_document,
            editable: true,
            // No prompt: the caret is the invitation once you are in the card,
            // and the prompt is what got clipped. A text shape is as wide as
            // its words, so one that has none yet is a box at its floor — too
            // narrow to hold "Write a thought…", which arrived cut off mid-word
            // as the first thing the text tool ever showed you. The painted
            // card still says it, where the box is the card's own and nothing
            // can crop it.
            placeholder: String::new(),
            width: Some((size[0] - 2. * letters.inset).max(40.)),
            // The editor fills the card. It cannot be asked to lay out to its
            // own content instead — a shrunk editor collapses to its first
            // line on the native side, which is how a card would learn to
            // hide five of the six lines it is holding.
            height: Some(Length::Fill),
            min_height: Some(letters.leading),
            max_height: None,
            options: Box::new(wire::EditorOptions {
                binding: Some(Box::new(binding)),
                size: Some(letters.size),
                line_height: Some(wire::LineHeight::Absolute(letters.leading)),
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
