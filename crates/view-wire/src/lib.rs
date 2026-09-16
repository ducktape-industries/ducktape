//! The wire between a host and an Ice app running in wasm.
//!
//! The guest ships a WIDGET TREE, not a picture: every tick it returns the
//! [`Node`] its view built, with every value inlined — text, colours, sizes —
//! and the host's own toolkit does layout, render, fonts, IME, clipboard and
//! scroll. The guest never learns where anything landed, which is the point:
//! there is nothing in it to draw with.
//!
//! Interaction goes back as MEANING, not input. A button carries the index of
//! the message the guest queued for it this frame ([`Node::Button`]'s
//! `on_press`); the host sends [`Event::Message`] with that index and the
//! guest runs its own handler. A text field carries a handler index; the host
//! owns the text and sends [`Event::Input`] with what it now reads; a
//! multiline editor the same, with [`Event::EditorTransaction`]. A
//! checkbox, slider or pick list likewise carries a handler index and the
//! host sends the new value ([`Event::Toggle`], [`Event::Slide`],
//! [`Event::Select`]).
//!
//! The types here are the one definition of the format: the guest serializes
//! them and the host deserializes the same code, so a field neither side can
//! drop silently. A host that reads a frame from an untrusted module runs
//! [`sanitize`] first.

#[cfg(feature = "authored-tests")]
pub mod authored;
/// Exact bincode protocol implemented by this build. Bump on serialized shape changes.
/// This is independent of WIT signatures and the manifest text format.
pub const WIRE_EPOCH: u32 = 7;

pub mod manifest;
pub mod native;
#[cfg(feature = "schema")]
pub mod schema;
mod wit;
pub use wit::WIT;

mod sanitization;
pub use sanitization::SanitizeReport;

use serde::{Deserialize, Serialize};

mod background;
pub use background::{Background, ColorStop};
mod editor;
pub mod editor_document;
pub mod editor_presentation;
pub mod editor_rich;
pub mod editor_transaction;
pub use editor_transaction::{
    EditorBinding, EditorDecision, EditorEditKind, EditorFault, EditorHistoryEffect,
    EditorKeyClaim, EditorPatch, EditorPatchError, EditorRequest, EditorRequestInput,
    EditorResponse, EditorTransactionEvent, EditorTransactionId, MAX_EDITOR_PATCHES,
    patched_editor_text,
};

pub use editor::{EditorCursor, EditorPosition, EditorState, editor_lines};

mod image;
pub use image::{ImageData, ImageFilter, ViewerOptions, viewer_scale_bounds};

mod snapshot;
pub use snapshot::{MAX_SNAPSHOT_BYTES, Snapshot, SnapshotValue};

mod flex;
mod float;
pub use flex::{
    FlexBasis, FlexContentAlignment, FlexDirection, FlexItem, FlexItemAlignment, FlexLayout,
    FlexMargin, FlexMargins, FlexWrap,
};
pub use float::{FloatExpression, FloatOp, MAX_FLOAT_OPS};

mod combo;
pub use combo::{ComboIcon, ComboOptions};
mod pick;
pub use pick::{PickHandle, PickIcon, PickOptions};

mod tooltip;
pub use tooltip::{TooltipPosition, TooltipPreset, TooltipStyle};
mod qr;
pub use qr::{MAX_QR_CODES, MAX_QR_PAYLOAD_BYTES, Qr, QrCorrection, QrSize, QrVersion};
mod rich_text;
mod text;
pub use rich_text::RichSpan;
pub use text::{
    Align, FontFamily, FontStretch, FontStyle, LineHeight, NamedFont, Shaping, TextOptions,
    Wrapping,
};
mod button;
pub use button::{ButtonPreset, ButtonRecipe};
mod canvas;
mod list;
pub use canvas::{
    CanvasCommand, CanvasLineCap, CanvasLineJoin, CanvasSegment, CanvasShape, CanvasStroke,
    MAX_CANVAS_PARTS,
};
pub use list::ListKey;
mod query;
pub use query::{ContainerQuery, MAX_QUERY_OPS, QueryOp};

mod markdown;
pub use markdown::MarkdownDocument;
mod window;
pub use window::WindowCommand;

mod widget;
pub use widget::WidgetCommand;

mod surface;
pub use surface::{MAX_SURFACE_DEPTH, MAX_SURFACE_VALUES, SurfaceValue, sanitize_surface_event};

/// Clipboard addressed by `clipboard.read` (this value as payload) and
/// `clipboard.write` (`(ClipboardTarget, String)` as payload). Reads return
/// an encoded `Option<String>`; writes return an empty successful response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardTarget {
    Standard,
    Primary,
}

pub mod events;
pub mod keyboard;
pub mod mouse;
pub mod system;

/// Something the host tells the guest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// Observation only: widgets already handled this event once.
    Observation {
        event: events::Event,
        captured: bool,
    },
    /// A mouse interaction in logical coordinates local to the guest surface.
    Mouse { event: mouse::Event, captured: bool },
    /// A keyboard interaction after the mounted native widgets handled it.
    Keyboard {
        event: keyboard::Event,
        captured: bool,
    },
    /// The user activated the widget the guest gave this message index to
    /// (a button press, an input submit). Indices are per frame: they name
    /// entries in the table the guest filled while building the tree it
    /// last sent.
    Message(u32),
    /// A registered host surface emitted its declared result value.
    Surface { handler: u32, value: SurfaceValue },
    /// A text field's content changed. `handler` indexes the guest's
    /// per-frame input-handler table; `text` is the whole value the host now
    /// holds.
    Input { handler: u32, text: String },
    /// An editor's text or cursor changed. `reset` fences document replacements;
    /// `revision` orders host observations. Caret-only changes are included.
    /// Initial assignment, mirror repair and exact transfer acknowledgments.
    EditorDocument {
        handler: u32,
        message: editor_document::EditorDocumentMessage,
    },
    EditorRequest {
        handler: u32,
        request: EditorRequest,
    },
    EditorTransaction {
        handler: u32,
        event: EditorTransactionEvent,
    },
    /// A checkbox or toggler flipped. `handler` indexes the guest's
    /// per-frame handler table; `on` is the state it now shows.
    Toggle { handler: u32, on: bool },
    /// A slider moved to `value`.
    Slide { handler: u32, value: f32 },
    /// A pick list chose the option at `index` in the node's `options`.
    Select { handler: u32, index: u32 },
    /// A [`Node::Sensor`]'s child was measured: shown at, or resized to,
    /// `width` by `height` — the child's own laid-out size in logical
    /// pixels, never where it sits in the window. `handler` is the node's
    /// `on_show` or `on_resize`.
    ///
    /// Delivered after layout, like a DOM `ResizeObserver`: the host lays
    /// the tree out, the sensor reads its child's size, and the event goes
    /// to the guest on the next tick. A guest whose answer changes the
    /// tree so the child measures differently again is measured again;
    /// a host bounds how many times in a row that may drive a tick before
    /// it stops delivering and logs `sensor loop limit exceeded`.
    Size {
        handler: u32,
        width: f32,
        height: f32,
    },
    /// The pointer is at (`x`, `y`) inside a [`Node::MouseArea`], in the
    /// area's own coordinates — the DOM's `offsetX`/`offsetY`, never the
    /// window's. Carries a move (`on_move`) or a left press (`on_press_at`).
    ///
    /// A host sends at most ONE move per handler per redraw frame, the last
    /// position it saw, as a browser delivers one `pointermove` per frame:
    /// the pointer crosses a thousand pixels a second and every event is a
    /// guest tick. A press is never coalesced.
    Pointer { handler: u32, x: f32, y: f32 },
    /// Accumulated logical-pixel movement of a grabbed resize handle.
    Drag { handler: u32, dx: f64, dy: f64 },
    /// The wheel turned over a [`Node::MouseArea`] by (`dx`, `dy`), in
    /// pixels when `pixels` is set and in lines otherwise.
    Scroll {
        handler: u32,
        dx: f32,
        dy: f32,
        pixels: bool,
    },
    /// A scrollable's content offset in logical pixels and anchor-relative
    /// fractions, emitted only when its native viewport changes. No window
    /// coordinates cross the wire.
    ScrollOffset {
        handler: u32,
        x: f32,
        y: f32,
        relative_x: f32,
        relative_y: f32,
    },
    /// One answer to a [`Request`]. A one-shot request gets exactly one with
    /// `done`; a subscription gets many, the last one `done`.
    Response {
        id: u64,
        result: Result<Vec<u8>, String>,
        done: bool,
    },
    /// The host no longer holds the tree the guest is patching — a patch it
    /// could not apply, a tree it dropped — and wants the next frame whole.
    Resync,
}

/// Something the guest asked the host for. The guest never blocks on it: a
/// future (or stream) inside the guest waits for the matching
/// [`Event::Response`]s, which the host delivers on its own schedule.
///
/// `kind` is `<capability>.<operation>`; the host refuses a capability the
/// app's manifest did not declare.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub kind: String,
    pub payload: Vec<u8>,
}

/// What one tick of the guest produced.
///
/// The tree crosses one of three ways: whole in `root`; not at all, with
/// `unchanged` set; or as `patches` against the tree the host holds, with
/// `root` empty and `unchanged` clear.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    /// Advisory producer report, sticky when a producer sanitizes before encoding.
    /// Receivers must independently sanitize the received whole/applied tree.
    pub upstream_sanitization: SanitizeReport,
    #[serde(deserialize_with = "editor_transaction::decode_responses")]
    pub editor_decisions: Vec<EditorResponse>,
    /// One bounded document message, independent of display text budgets.
    #[serde(deserialize_with = "editor_document::decode_messages")]
    pub editor_documents: Vec<editor_document::EditorDocumentMessage>,
    /// The current subscription requests guest-local mouse observations.
    pub mouse_interest: bool,
    /// Live subscriptions opt into each copied event category.
    pub event_interest: events::Interest,
    /// The tree to show. `None` with `unchanged` set means "what you have";
    /// `None` otherwise means "what you have, with `patches` applied".
    pub root: Option<Node>,
    /// Edits to the tree the host holds, in order, when `root` is `None`
    /// and `unchanged` is clear. The host applies them with [`apply`].
    pub patches: Vec<Patch>,
    /// What the guest asked for while producing this frame.
    pub requests: Vec<Request>,
    /// Requests the guest stopped waiting on — a dropped future or stream.
    /// The host frees whatever it kept for them and sends no more answers.
    pub cancels: Vec<u64>,
    /// `root` is `None` because the tree is the one the guest sent last:
    /// the host keeps what it has instead of decoding it again. Requests and
    /// cancels still cross.
    pub unchanged: bool,
    /// The guest ran out of tick budget with work still ready — a task that
    /// yields more than one tick runs, a handler chain longer than one
    /// round — and wants the next tick now, not at the next event or answer.
    pub busy: bool,
}

/// One edit to the tree the host holds. `path` is the child index at every
/// level from the root down (`[]` is the root itself); children are
/// addressed by index at the moment the patch is applied, so a sequence
/// reads like edits to a live document. The vocabulary is a virtual DOM's
/// mutation list: replace, re-prop, insert, remove, move.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Patch {
    /// The subtree at `path` becomes `node`.
    Replace { path: Vec<u32>, node: Node },
    /// The node at `path` takes `node`'s own fields and keeps its children:
    /// `node` carries none (an empty list, or an empty stand-in per slot).
    Props { path: Vec<u32>, node: Node },
    /// `node` becomes child `index` of the list at `path`.
    Insert {
        path: Vec<u32>,
        index: u32,
        node: Node,
    },
    /// Child `index` of the list at `path` goes away.
    Remove { path: Vec<u32>, index: u32 },
    /// Child `from` of the list at `path` is taken out and put back at `to`.
    Move { path: Vec<u32>, from: u32, to: u32 },
}

/// Red, green, blue, alpha in `0.0..=1.0`. The guest resolves its own
/// palette; the host paints what it is told.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rgba(pub [f32; 4]);

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Length {
    Fill,
    FillPortion(u16),
    Shrink,
    Fixed(f32),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Edges {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Edges {
    pub const fn all(value: f32) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Border {
    /// Unspecified fields retain the host style, including earlier faces.
    pub color: Option<Rgba>,
    pub width: Option<f32>,
    /// top-left, top-right, bottom-right, bottom-left. `Some([0.0; 4])`
    /// explicitly squares the corners; `None` preserves their radius.
    pub radius: Option<[f32; 4]>,
}

/// Optional native shadow fields; omission retains the host style.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Shadow {
    pub color: Option<Rgba>,
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub blur: Option<f32>,
}

impl Shadow {
    fn sanitize(&mut self) {
        bound_color(&mut self.color);
        bound_optional(&mut self.blur);
        for value in [&mut self.x, &mut self.y].into_iter().flatten() {
            *value = if value.is_finite() {
                value.clamp(-MAX_PIXELS, MAX_PIXELS)
            } else {
                0.0
            };
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum AlignX {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum AlignY {
    Top,
    Center,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Axis {
    Column,
    Row,
}

/// Optional native row/column wrapping configuration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Wrap {
    pub spacing: Option<f32>,
    pub align: Option<AlignX>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ScrollDirection {
    Vertical,
    Horizontal,
    Both,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Weight {
    #[default]
    Normal,
    Medium,
    Semibold,
    Bold,
    Thin,
    ExtraLight,
    Light,
    ExtraBold,
    Black,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Font {
    pub monospace: bool,
    pub weight: Weight,
}

/// One state of a button or input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Face {
    pub background: Option<Rgba>,
    pub text: Option<Rgba>,
    pub border: Option<Border>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ButtonStyle {
    pub preset: ButtonPreset,
    pub recipe: Option<ButtonRecipe>,
    pub active: Face,
    pub hovered: Option<Face>,
    pub pressed: Option<Face>,
    pub disabled: Option<Face>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InputFace {
    pub icon: Option<Rgba>,
    pub background: Option<Rgba>,
    pub border: Option<Border>,
    pub value: Option<Rgba>,
    pub placeholder: Option<Rgba>,
    pub selection: Option<Rgba>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InputStyle {
    pub utility: InputFace,
    pub focus_border: Option<Rgba>,
    pub focused_hovered: Option<InputFace>,
    pub active: InputFace,
    pub hovered: Option<InputFace>,
    pub focused: Option<InputFace>,
    pub disabled: Option<InputFace>,
}

/// Copied input accessibility and native layout options.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InputOptions {
    pub label: String,
    pub description: Option<String>,
    pub disabled: bool,
    pub padding: Option<Edges>,
    pub text_size: Option<f32>,
    pub line_height: Option<f32>,
    pub align: Option<AlignX>,
    pub font: Option<NamedFont>,
}

/// Copied native multiline editor presentation; state faces share input semantics.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EditorOptions {
    pub rich: Option<Box<editor_rich::RichPresentation>>,
    pub binding: Option<Box<EditorBinding>>,
    pub presentation: Option<Box<editor_presentation::EditorPresentation>>,
    pub size: Option<f32>,
    pub padding: Option<f32>,
    pub line_height: Option<LineHeight>,
    pub wrapping: Option<Wrapping>,
    pub font: Option<NamedFont>,
    pub style: InputStyle,
}

impl InputStyle {
    fn sanitize(&mut self) {
        bound_color(&mut self.focus_border);
        for face in [
            Some(&mut self.utility),
            Some(&mut self.active),
            self.hovered.as_mut(),
            self.focused.as_mut(),
            self.focused_hovered.as_mut(),
            self.disabled.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            bound_color(&mut face.background);
            bound_border(&mut face.border);
            bound_color(&mut face.value);
            bound_color(&mut face.icon);
            bound_color(&mut face.placeholder);
            bound_color(&mut face.selection);
        }
    }
}

/// One state of a checkbox, toggler or radio: the box, track or ring; the
/// mark inside it (the check, the knob, the dot); the label; and the border
/// around the box or track. `None` leaves the host's theme.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ControlFace {
    pub background: Option<Rgba>,
    pub mark: Option<Rgba>,
    pub text: Option<Rgba>,
    pub border: Option<Border>,
}

/// A theme role a preset paints a control in.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Tone {
    Primary,
    Secondary,
    Success,
    Warning,
    Danger,
}

/// A checkbox's or toggler's faces, per state and per value. A hovered or
/// disabled face paints over the active face of the same value, so a state
/// that names only its difference inherits the rest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToggleStyle {
    /// A checkbox's preset; a switch has none and ignores it.
    pub tone: Option<Tone>,
    pub active_on: Option<ControlFace>,
    pub active_off: Option<ControlFace>,
    pub hovered_on: Option<ControlFace>,
    pub hovered_off: Option<ControlFace>,
    pub disabled_on: Option<ControlFace>,
    pub disabled_off: Option<ControlFace>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RadioStyle {
    pub active_on: Option<ControlFace>,
    pub active_off: Option<ControlFace>,
    pub hovered_on: Option<ControlFace>,
    pub hovered_off: Option<ControlFace>,
}

/// A native slider handle, copied independently for each interaction face.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum SliderHandleShape {
    Circle { radius: f32 },
    Rectangle { width: u16, border_radius: [f32; 4] },
}

/// One state of a slider: the rail's two halves and its border, and the
/// handle's colour, border and shape. Omitted fields retain the native style.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SliderFace {
    pub rail_start: Option<Rgba>,
    pub rail_end: Option<Rgba>,
    pub rail_width: Option<f32>,
    pub rail_border: Option<Border>,
    pub handle: Option<Rgba>,
    pub handle_border: Option<Border>,
    pub handle_shape: Option<SliderHandleShape>,
}

/// A hovered or dragged face paints over the active one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SliderStyle {
    pub active: Option<SliderFace>,
    pub hovered: Option<SliderFace>,
    pub dragged: Option<SliderFace>,
}

/// One state of a pick list's closed face.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PickFace {
    pub background: Option<Rgba>,
    pub text: Option<Rgba>,
    pub placeholder: Option<Rgba>,
    pub handle: Option<Rgba>,
    pub border: Option<Border>,
}

/// The menu a pick list opens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MenuFace {
    pub shadow: Shadow,
    pub background: Option<Rgba>,
    pub text: Option<Rgba>,
    pub border: Option<Border>,
    pub selected_text: Option<Rgba>,
    pub selected_background: Option<Rgba>,
}

/// Active overrides apply first; opened-hovered also inherits opened overrides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PickListStyle {
    pub active: Option<PickFace>,
    pub hovered: Option<PickFace>,
    pub opened: Option<PickFace>,
    pub opened_hovered: Option<PickFace>,
    pub menu: Option<MenuFace>,
}

/// Where a scroll's offset is measured from. `Keep` rests at the start and
/// holds the visible rows still when content lands above them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum ScrollAnchor {
    #[default]
    Start,
    End,
    Keep,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ContentFit {
    Contain,
    Cover,
    Fill,
    None,
    ScaleDown,
}

/// A rotation in radians: `Floating` keeps the layout the picture had
/// before it turned, `Solid` lays out the turned bounds.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Rotation {
    Floating(f32),
    Solid(f32),
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToggleKind {
    Checkbox,
    Switch,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ButtonContent {
    Label(String),
    #[serde(deserialize_with = "decode_child")]
    Child(Box<Node>),
}

/// One widget. `key` is the node's identity across frames — the
/// accessibility path the compiler already computes (`App/content/count`)
/// — which the host uses for widget state (focus, caret, scroll) and for
/// the accessibility tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Node {
    /// A payload encoded and painted by the host.
    Qr { key: String, code: Qr },
    /// Styled spans form one native paragraph; link clicks carry a String handler payload.
    RichText {
        key: String,
        #[serde(deserialize_with = "rich_text::decode_spans")]
        spans: Vec<RichSpan>,
        size: Option<f32>,
        color: Option<Rgba>,
        font: Font,
        width: Option<Length>,
        align_x: Option<AlignX>,
        options: TextOptions,
        on_link: Option<u32>,
    },
    /// Flex rules and item metadata, interpreted by the host's native layout engine.
    Flex {
        key: String,
        layout: FlexLayout,
        background: Option<Rgba>,
        border: Option<Border>,
        #[serde(deserialize_with = "flex::decode_items")]
        items: Vec<FlexItem>,
        #[serde(deserialize_with = "decode_children")]
        children: Vec<Node>,
    },
    /// A child positioned in this widget's local coordinates. The host lays it out.
    Pin {
        key: String,
        x: f32,
        y: f32,
        width: Option<Length>,
        height: Option<Length>,
        #[serde(deserialize_with = "decode_child")]
        content: Box<Node>,
    },
    /// Floating content whose translation is evaluated only by the host.
    Float {
        key: String,
        x: FloatExpression,
        y: FloatExpression,
        scale: f32,
        shadow: Shadow,
        radius: Option<[f32; 4]>,
        #[serde(deserialize_with = "decode_child")]
        content: Box<Node>,
    },
    /// Copied keyed rows; the host owns widget state and optional virtualization.
    KeyedColumn {
        key: String,
        #[serde(deserialize_with = "list::decode_optional_keys")]
        keys: Option<Vec<ListKey>>,
        background: Option<Rgba>,
        border: Option<Border>,
        spacing: Option<f32>,
        padding: Option<Edges>,
        width: Option<Length>,
        height: Option<Length>,
        max_width: Option<f32>,
        align: Option<AlignX>,
        virtual_row: Option<f32>,
        #[serde(deserialize_with = "decode_children")]
        children: Vec<Node>,
    },
    Container {
        shadow: Shadow,
        max_width: Option<f32>,
        max_height: Option<f32>,
        clip: bool,
        key: String,
        width: Option<Length>,
        height: Option<Length>,
        padding: Option<Edges>,
        align_x: Option<AlignX>,
        align_y: Option<AlignY>,
        background: Option<Background>,
        border: Option<Border>,
        /// Round the box to whole pixels; `None` is the host's default.
        snap: Option<bool>,
        #[serde(deserialize_with = "decode_child")]
        content: Box<Node>,
    },
    /// A grabbed divider: local movement deltas and native cursor; one child.
    ResizeHandle {
        key: String,
        on_press: Option<u32>,
        on_release: Option<u32>,
        on_drag: Option<u32>,
        cursor: Option<mouse::Cursor>,
        #[serde(deserialize_with = "decode_child")]
        content: Box<Node>,
    },
    /// A region that reports what the pointer does over its one child. The
    /// discrete routes carry per-frame message indices like a button's
    /// `on_press`; `on_move` and `on_press_at` carry a handler index the
    /// host answers with [`Event::Pointer`], `on_scroll` one it answers
    /// with [`Event::Scroll`]. The node paints nothing of its own.
    MouseArea {
        key: String,
        on_press: Option<u32>,
        on_release: Option<u32>,
        on_double_click: Option<u32>,
        on_right_press: Option<u32>,
        on_right_release: Option<u32>,
        on_middle_press: Option<u32>,
        on_middle_release: Option<u32>,
        on_enter: Option<u32>,
        on_exit: Option<u32>,
        on_move: Option<u32>,
        /// Fires for a left press even when the child took it — a button
        /// inside the area — where `on_press` does not.
        on_press_at: Option<u32>,
        on_scroll: Option<u32>,
        #[serde(deserialize_with = "decode_child")]
        content: Box<Node>,
    },
    Tooltip {
        key: String,
        position: TooltipPosition,
        gap: f32,
        padding: f32,
        delay_ms: u64,
        snap: bool,
        style: TooltipStyle,
        /// Content followed by tip; extra children are discarded by sanitization.
        #[serde(deserialize_with = "decode_children")]
        children: Vec<Node>,
    },
    Linear {
        max_width: Option<f32>,
        clip: bool,
        key: String,
        wrap: Option<Wrap>,
        axis: Axis,
        spacing: Option<f32>,
        padding: Option<Edges>,
        width: Option<Length>,
        height: Option<Length>,
        /// Cross-axis alignment of the children.
        align: Option<AlignX>,
        /// The surface behind the children: a layout paints nothing of its
        /// own, so this is a box drawn around it.
        background: Option<Rgba>,
        border: Option<Border>,
        #[serde(deserialize_with = "decode_children")]
        children: Vec<Node>,
    },
    /// Equal cells in rows of `columns`, or of as many as fit at `fluid`
    /// pixels each. A cell is `aspect` times as wide as it is tall unless
    /// `height` gives the rows a length to share; without either the host
    /// draws squares.
    Grid {
        key: String,
        columns: Option<u32>,
        /// The widest a cell may be; the column count follows the width.
        /// Wins over `columns`.
        fluid: Option<f32>,
        spacing: Option<f32>,
        padding: Option<Edges>,
        width: Option<Length>,
        height: Option<Length>,
        /// Horizontal pixels per vertical pixel of a cell.
        aspect: Option<f32>,
        background: Option<Rgba>,
        border: Option<Border>,
        #[serde(deserialize_with = "decode_children")]
        children: Vec<Node>,
    },
    /// Supplies widget-local dimensions to descendant container conditions.
    Responsive {
        key: String,
        width: Option<Length>,
        height: Option<Length>,
        #[serde(deserialize_with = "decode_child")]
        content: Box<Node>,
    },
    /// A guest-memoized subtree. Generation changes whenever cached content or
    /// its callable routes are rebuilt, including a rebuild after eviction.
    Lazy {
        key: String,
        generation: u64,
        #[serde(deserialize_with = "decode_child")]
        content: Box<Node>,
    },
    /// Splices selected children into the surrounding layout. It adds no box.
    When {
        key: String,
        condition: ContainerQuery,
        #[serde(deserialize_with = "decode_children")]
        children: Vec<Node>,
    },
    /// Watches its child's laid-out size. `on_show` hears the size when the
    /// child first comes into view (within `anticipate` pixels of it),
    /// `on_resize` every change after, both as [`Event::Size`]; `on_hide`
    /// is the message for leaving view. `delay` is milliseconds a size
    /// must hold before it is reported.
    Sensor {
        key: String,
        /// Copied continuity value for `key=`, independent of widget identity.
        reset: Option<SurfaceValue>,
        on_show: Option<u32>,
        on_resize: Option<u32>,
        on_hide: Option<u32>,
        anticipate: Option<f32>,
        delay: Option<f32>,
        #[serde(deserialize_with = "decode_child")]
        child: Box<Node>,
    },
    Scroll {
        on_scroll: Option<u32>,
        virtual_rows: bool,
        key: String,
        direction: ScrollDirection,
        width: Option<Length>,
        height: Option<Length>,
        /// No scroll bar is drawn; the content still scrolls.
        bar_hidden: bool,
        bar_width: Option<f32>,
        bar_margin: Option<f32>,
        scroller_width: Option<f32>,
        /// Space between the bar and the content, which shrinks the content.
        bar_spacing: Option<f32>,
        anchor_x: ScrollAnchor,
        anchor_y: ScrollAnchor,
        /// Follow content that grows while the reader sits at the end.
        auto_scroll: bool,
        background: Option<Rgba>,
        border: Option<Border>,
        #[serde(deserialize_with = "decode_child")]
        content: Box<Node>,
    },
    Text {
        options: TextOptions,
        key: String,
        content: String,
        size: Option<f32>,
        color: Option<Rgba>,
        font: Font,
        width: Option<Length>,
        align_x: Option<AlignX>,
    },
    /// A raster picture sent once per typed content hash.
    Image {
        key: String,
        hash: u64,
        data: Option<ImageData>,
        label: Option<String>,
        fit: Option<ContentFit>,
        rotation: Option<Rotation>,
        opacity: Option<f32>,
        filter: ImageFilter,
        width: Option<Length>,
        height: Option<Length>,
    },
    /// A native zoom/pan viewer sharing the raster picture cache and budgets.
    ImageViewer {
        key: String,
        hash: u64,
        data: Option<ImageData>,
        label: Option<String>,
        fit: Option<ContentFit>,
        filter: ImageFilter,
        width: Option<Length>,
        height: Option<Length>,
        options: ViewerOptions,
    },
    /// A vector picture. Its bytes cross ONCE: the frame that first shows a
    /// picture carries them under `hash`, and every frame after — a changed
    /// tree re-sends every node — names the hash alone. The host keeps what
    /// it decoded by hash for as long as the guest runs; a hash it has not
    /// seen draws as empty space of the node's size.
    Svg {
        key: String,
        /// Use the nearest button's final status text color at draw time.
        inherit_button_ink: bool,
        /// The guest's content hash of the picture: an opaque cache key,
        /// not something the host recomputes.
        hash: u64,
        /// The picture, on the first frame it is shown.
        bytes: Option<Vec<u8>>,
        /// The accessible name of the picture.
        label: Option<String>,
        /// A tint for the whole picture, over its own colours.
        color: Option<Rgba>,
        /// The tint while hovered: `None` keeps `color`, `Some(None)` drops
        /// the tint, `Some(Some(_))` is another one.
        hover: Option<Option<Rgba>>,
        fit: Option<ContentFit>,
        rotation: Option<Rotation>,
        /// `0.0..=1.0`; `None` is opaque.
        opacity: Option<f32>,
        width: Option<Length>,
        height: Option<Length>,
    },
    Input {
        options: InputOptions,
        key: String,
        placeholder: String,
        /// Copied document state, adopted by reset and host observation revision.
        value: String,
        on_input: u32,
        on_submit: Option<u32>,
        width: Option<Length>,
        secure: bool,
        style: Box<InputStyle>,
    },
    /// A multiline text editor. The host owns the `text_editor::Content` —
    /// native widget interaction — and the guest sees document state, unlike
    /// [`Node::Input`]. Presentation crosses as copied data.
    Editor {
        options: Box<EditorOptions>,
        key: String,
        placeholder: String,
        /// A shared logical document; its bytes travel only through a requested transfer.
        document: editor_document::EditorDocumentRef,
        /// Mutable guest state route, present even while editing is disabled.
        on_document: u32,
        editable: bool,
        /// Pixels; the editor fills its parent otherwise.
        width: Option<f32>,
        height: Option<Length>,
        min_height: Option<f32>,
        max_height: Option<f32>,
    },
    Button {
        key: String,
        content: ButtonContent,
        /// The accessible name of a button whose content is not a plain
        /// label.
        label: Option<String>,
        checked: Option<bool>,
        expanded: Option<bool>,
        description: Option<String>,
        /// `None` is a disabled button.
        on_press: Option<u32>,
        width: Option<Length>,
        height: Option<Length>,
        padding: Option<Edges>,
        style: ButtonStyle,
    },
    Space {
        width: Option<Length>,
        height: Option<Length>,
    },
    Rule {
        key: String,
        axis: Axis,
        thickness: f32,
        color: Option<Rgba>,
        /// The theme's weak rule colour instead of its strong one, under
        /// `color` when both are given.
        weak: bool,
        /// top-left, top-right, bottom-right, bottom-left.
        radius: Option<[f32; 4]>,
        /// Round the rule to whole pixels; `None` is the host's default.
        snap: Option<bool>,
    },
    /// A checkbox or a toggler: a labelled bool.
    Toggle {
        key: String,
        kind: ToggleKind,
        label: String,
        checked: bool,
        /// `None` is a disabled control.
        on_toggle: Option<u32>,
        width: Option<Length>,
        style: ToggleStyle,
    },
    /// One radio button. Its value is the guest's business: selecting it
    /// sends the message the guest queued for it.
    Radio {
        key: String,
        label: String,
        selected: bool,
        on_select: u32,
        width: Option<Length>,
        style: RadioStyle,
    },
    Slider {
        key: String,
        value: f32,
        min: f32,
        max: f32,
        step: f32,
        on_change: u32,
        on_release: Option<u32>,
        axis: Axis,
        width: Option<Length>,
        height: Option<Length>,
        style: SliderStyle,
    },
    ComboBox {
        key: String,
        state_key: String,
        options: Vec<String>,
        selected: Option<u32>,
        reset: u64,
        placeholder: String,
        on_select: u32,
        width: Option<Length>,
        settings: Box<ComboOptions>,
    },
    PickList {
        settings: Box<PickOptions>,
        key: String,
        /// Every option as the guest shows it; the host answers with an
        /// index into this list.
        options: Vec<String>,
        selected: Option<u32>,
        placeholder: Option<String>,
        on_select: u32,
        width: Option<Length>,
        style: PickListStyle,
    },
    Progress {
        key: String,
        value: f32,
        min: f32,
        max: f32,
        axis: Axis,
        length: Option<Length>,
        girth: Option<Length>,
        /// The theme role the bar is painted in; `background`, `bar` and
        /// `border` paint over it.
        tone: Option<Tone>,
        background: Option<Rgba>,
        bar: Option<Rgba>,
        border: Option<Border>,
    },
    /// Union-sized layers, or native base/under layering when `under` is nonzero.
    Stack {
        key: String,
        width: Option<Length>,
        height: Option<Length>,
        padding: Option<Edges>,
        background: Option<Rgba>,
        border: Option<Border>,
        clip: bool,
        under: u32,
        #[serde(deserialize_with = "decode_children")]
        children: Vec<Node>,
    },
    /// The host's draw-time base/reveal pair; `open` can hold the reveal visible.
    Hover {
        key: String,
        width: Option<Length>,
        height: Option<Length>,
        padding: Option<Edges>,
        background: Option<Rgba>,
        border: Option<Border>,
        tint: Option<Rgba>,
        radius: f32,
        open: bool,
        #[serde(deserialize_with = "decode_children")]
        children: Vec<Node>,
    },
    /// A base plus an optional modal layer. Closing removes the second child.
    Overlay {
        key: String,
        padding: f32,
        backdrop: Rgba,
        align_x: AlignX,
        align_y: AlignY,
        on_dismiss: Option<u32>,
        #[serde(deserialize_with = "decode_children")]
        children: Vec<Node>,
    },
    /// Bounded geometry painted by the host, in widget-local coordinates.
    Canvas {
        key: String,
        width: Option<Length>,
        height: Option<Length>,
        #[serde(deserialize_with = "canvas::decode_parts")]
        commands: Vec<CanvasCommand>,
    },
    /// A region the host paints itself: `name` picks a surface the
    /// embedding host registered, `args` are the typed values the guest
    /// hands it; `on_event` routes a returned value to its handler. The guest
    /// never sees what is drawn there, and the host repaints it on its own clock — a live video tile, a sweeping hand —
    /// without a guest tick. A name the host has not registered renders as
    /// a visible placeholder. It takes the size its parent gives it: an Ice
    /// `box w= h=` around the `extern` call sets it.
    Surface {
        key: String,
        name: String,
        args: Vec<SurfaceValue>,
        on_event: Option<u32>,
    },
}

/// Spends generic parameters on nothing: `<(&'a (), M, T) as Erase>::Node`
/// is [`Node`] for every `'a`, `M` and `T`. Generated code names its element
/// type `__IceElement<'a, Message, Theme>` for both targets, and a type
/// alias may not drop a parameter, so the tree target's alias projects
/// through this instead.
pub trait Erase {
    type Node;
}

impl<T: ?Sized> Erase for T {
    type Node = Node;
}

impl Node {
    /// The node an empty view renders as.
    pub fn empty() -> Self {
        Self::Space {
            width: None,
            height: None,
        }
    }

    /// Hashes the current copied subtree without allocating an encoded buffer.
    /// A host uses this after sanitization: shared frame budgets may change
    /// content even when a guest memo generation stays the same.
    pub fn fingerprint(&self) -> u64 {
        use std::hash::Hasher;
        struct Sink(std::hash::DefaultHasher);
        impl std::io::Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.write(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut sink = Sink(std::hash::DefaultHasher::new());
        bincode::serialize_into(&mut sink, self).expect("node fingerprint sink cannot fail");
        sink.0.finish()
    }

    pub fn key(&self) -> Option<&str> {
        match self {
            Self::Container { key, .. }
            | Self::ResizeHandle { key, .. }
            | Self::MouseArea { key, .. }
            | Self::Linear { key, .. }
            | Self::Grid { key, .. }
            | Self::KeyedColumn { key, .. }
            | Self::Flex { key, .. }
            | Self::Pin { key, .. }
            | Self::Float { key, .. }
            | Self::Responsive { key, .. }
            | Self::Lazy { key, .. }
            | Self::When { key, .. }
            | Self::Sensor { key, .. }
            | Self::Scroll { key, .. }
            | Self::Qr { key, .. }
            | Self::RichText { key, .. }
            | Self::Text { key, .. }
            | Self::Svg { key, .. }
            | Self::Image { key, .. }
            | Self::ImageViewer { key, .. }
            | Self::Input { key, .. }
            | Self::Editor { key, .. }
            | Self::Button { key, .. }
            | Self::Rule { key, .. }
            | Self::Toggle { key, .. }
            | Self::Radio { key, .. }
            | Self::Slider { key, .. }
            | Self::PickList { key, .. }
            | Self::ComboBox { key, .. }
            | Self::Progress { key, .. }
            | Self::Stack { key, .. }
            | Self::Hover { key, .. }
            | Self::Overlay { key, .. }
            | Self::Tooltip { key, .. }
            | Self::Canvas { key, .. }
            | Self::Surface { key, .. } => Some(key),
            Self::Space { .. } => None,
        }
    }

    /// The node's children in order. One arm per variant, here and in
    /// [`Node::children_mut`] and [`Node::child_list_mut`]: everything that
    /// walks, diffs or patches a tree goes through these three, so a new
    /// variant is a new arm in each and nothing else.
    pub fn children(&self) -> &[Node] {
        match self {
            Self::Container { content, .. }
            | Self::Pin { content, .. }
            | Self::Float { content, .. }
            | Self::Responsive { content, .. }
            | Self::Lazy { content, .. }
            | Self::Sensor { child: content, .. }
            | Self::ResizeHandle { content, .. }
            | Self::MouseArea { content, .. }
            | Self::Scroll { content, .. } => std::slice::from_ref(content),
            Self::Linear { children, .. }
            | Self::Grid { children, .. }
            | Self::Stack { children, .. }
            | Self::Hover { children, .. }
            | Self::Tooltip { children, .. }
            | Self::Overlay { children, .. }
            | Self::KeyedColumn { children, .. }
            | Self::Flex { children, .. }
            | Self::When { children, .. } => children,

            Self::Button {
                content: ButtonContent::Child(child),
                ..
            } => std::slice::from_ref(child),
            Self::Button { .. }
            | Self::Qr { .. }
            | Self::RichText { .. }
            | Self::Text { .. }
            | Self::Svg { .. }
            | Self::Image { .. }
            | Self::ImageViewer { .. }
            | Self::Input { .. }
            | Self::Editor { .. }
            | Self::Space { .. }
            | Self::Rule { .. }
            | Self::Toggle { .. }
            | Self::Radio { .. }
            | Self::Slider { .. }
            | Self::PickList { .. }
            | Self::ComboBox { .. }
            | Self::Progress { .. }
            | Self::Canvas { .. }
            | Self::Surface { .. } => &[],
        }
    }

    /// Runs `visit` on every node in the tree, depth first, this one first.
    pub fn for_each_mut(&mut self, visit: &mut impl FnMut(&mut Node)) {
        visit(self);
        for child in self.children_mut() {
            child.for_each_mut(visit);
        }
    }

    pub fn children_mut(&mut self) -> &mut [Node] {
        match self {
            Self::Container { content, .. }
            | Self::Pin { content, .. }
            | Self::Float { content, .. }
            | Self::Responsive { content, .. }
            | Self::Lazy { content, .. }
            | Self::Sensor { child: content, .. }
            | Self::ResizeHandle { content, .. }
            | Self::MouseArea { content, .. }
            | Self::Scroll { content, .. } => std::slice::from_mut(content),
            Self::Linear { children, .. }
            | Self::Grid { children, .. }
            | Self::Stack { children, .. }
            | Self::Hover { children, .. }
            | Self::Tooltip { children, .. }
            | Self::Overlay { children, .. }
            | Self::KeyedColumn { children, .. }
            | Self::Flex { children, .. }
            | Self::When { children, .. } => children,

            Self::Button {
                content: ButtonContent::Child(child),
                ..
            } => std::slice::from_mut(child),
            Self::Button { .. }
            | Self::Qr { .. }
            | Self::RichText { .. }
            | Self::Text { .. }
            | Self::Input { .. }
            | Self::Editor { .. }
            | Self::Space { .. }
            | Self::Rule { .. }
            | Self::Toggle { .. }
            | Self::Radio { .. }
            | Self::Slider { .. }
            | Self::PickList { .. }
            | Self::ComboBox { .. }
            | Self::Progress { .. }
            | Self::Svg { .. }
            | Self::Image { .. }
            | Self::ImageViewer { .. }
            | Self::Canvas { .. }
            | Self::Surface { .. } => &mut [],
        }
    }

    /// The children as a list that can grow and shrink, for the variants
    /// that hold one; a fixed-arity node (a container's one content) has
    /// none, and no patch may insert into, remove from or move within it.
    pub fn child_list_mut(&mut self) -> Option<&mut Vec<Node>> {
        match self {
            Self::Linear { children, .. }
            | Self::Grid { children, .. }
            | Self::KeyedColumn { children, .. }
            | Self::Flex { children, .. }
            | Self::Stack { children, .. }
            | Self::When { children, .. }
            | Self::Hover { children, .. }
            | Self::Tooltip { children, .. }
            | Self::Overlay { children, .. } => Some(children),
            Self::Container { .. }
            | Self::Pin { .. }
            | Self::Float { .. }
            | Self::Responsive { .. }
            | Self::Lazy { .. }
            | Self::Sensor { .. }
            | Self::ResizeHandle { .. }
            | Self::MouseArea { .. }
            | Self::Scroll { .. }
            | Self::Button { .. }
            | Self::Qr { .. }
            | Self::RichText { .. }
            | Self::Text { .. }
            | Self::Input { .. }
            | Self::Editor { .. }
            | Self::Space { .. }
            | Self::Rule { .. }
            | Self::Toggle { .. }
            | Self::Radio { .. }
            | Self::Slider { .. }
            | Self::PickList { .. }
            | Self::ComboBox { .. }
            | Self::Progress { .. }
            | Self::Svg { .. }
            | Self::Image { .. }
            | Self::ImageViewer { .. }
            | Self::Canvas { .. }
            | Self::Surface { .. } => None,
        }
    }

    /// Takes the children out, leaving an empty list or an empty stand-in
    /// per slot: what is left is the node's own fields, which is what a
    /// [`Patch::Props`] carries and what two nodes are compared by.
    fn detach(&mut self) -> Vec<Node> {
        match self.child_list_mut() {
            Some(list) => std::mem::take(list),
            None => self
                .children_mut()
                .iter_mut()
                .map(|slot| std::mem::replace(slot, Node::empty()))
                .collect(),
        }
    }

    /// Puts [`Node::detach`]ed children back. `None` when the arity does
    /// not fit, in which case nothing was moved.
    fn attach(&mut self, children: Vec<Node>) -> Option<()> {
        if let Some(list) = self.child_list_mut() {
            *list = children;
            return Some(());
        }
        let slots = self.children_mut();
        if slots.len() != children.len() {
            return None;
        }
        for (slot, child) in slots.iter_mut().zip(children) {
            *slot = child;
        }
        Some(())
    }

    /// Every node in the tree, depth first, this one included.
    pub fn count(&self) -> usize {
        1 + self.children().iter().map(Node::count).sum::<usize>()
    }
}

/// A tree deeper than this is cut off: a guest cannot make the host's
/// layout recurse without bound.
pub const MAX_DEPTH: usize = 64;
/// More nodes than this and the host stops reading: the widget tree of a
/// screen, not of a spreadsheet.
pub const MAX_NODES: usize = 8_192;
/// The longest string a single node may carry (text, placeholder, key).
pub const MAX_STRING_BYTES: usize = 64 << 10;
/// The most SHAPED text one frame may carry in total — every [`Node::Text`]
/// content, input or editor value and placeholder, and plain button label
/// together.
///
/// The per-string and per-node caps do not bound this: 128 strings of
/// [`MAX_STRING_BYTES`] are a legal 8 MiB frame, and the host reshapes all
/// of it on the window thread every time the guest changes a character.
/// Measured on a desk machine, that frame took 6.2 s and allocated 3.3 GiB;
/// held to one full-length string's worth of text it takes 69 ms and 26 MiB.
/// A screen shows a couple of kilobytes, so this leaves a guest that means
/// well thirty times what it needs while taking two orders of magnitude off
/// what a hostile one can spend.
///
/// It bounds bytes and nothing else. Two other costs live outside it and are
/// the host's to attack: [`MAX_NODES`] nodes cost around a hundred
/// milliseconds to lay out however short their text, and a byte of Hangul,
/// Han or emoji costs some twenty times a byte of ASCII to shape.
pub const MAX_TEXT_BYTES_PER_FRAME: usize = MAX_STRING_BYTES;
/// The most picture bytes one frame may carry in total, over every
/// [`Node::Svg`] or [`Node::Image`] that brings its payload. A picture that does not fit in
/// what is left is dropped whole, not cut: half an SVG is not an SVG, and
/// the host draws an unknown hash as empty space. A guest sends each
/// picture once, so this bounds what a frame can make the host parse, not
/// what an app can show over its life; an icon is a few kilobytes.
pub const MAX_PICTURE_BYTES_PER_FRAME: usize = 1 << 20;
/// The most options one [`Node::PickList`] may offer: a menu, not a table.
/// Each option is shaped text and spends the frame's text budget too.
pub const MAX_OPTIONS: usize = 256;

/// Maximum positional values supplied to one host surface.
pub const MAX_SURFACE_ARGS: usize = 256;
/// Text and spacing sizes are pixels; nothing on a screen needs more.
const MAX_PIXELS: f32 = 8192.0;
/// A text size, which is not a length: every glyph at it is rasterized and
/// cached, so a screenful of 8192 px text is an atlas no screen asked for.
const MAX_TEXT_PIXELS: f32 = 512.0;

/// Pulls a frame from an untrusted module into what the host is willing to
/// lay out: the tree is truncated past [`MAX_DEPTH`] and [`MAX_NODES`],
/// strings past [`MAX_STRING_BYTES`], shaped text past
/// [`MAX_TEXT_BYTES_PER_FRAME`] in total, picture bytes past
/// [`MAX_PICTURE_BYTES_PER_FRAME`] in total, text sizes to [`MAX_TEXT_PIXELS`],
/// every other size, colour and spacing clamped to a finite range, and a key
/// used twice moved off the one already taken. A frame from a well-behaved
/// guest passes through unchanged.
///
/// A frame that arrived as bytes has passed [`decode`] first, which refuses
/// one nested deeper than this walk goes. A frame that carries `patches`
/// instead of a tree is bounded by [`apply`], since every bound is on the
/// tree the patches make and only the host holds it.
pub fn sanitize(frame: &mut Frame) -> Result<SanitizeReport, &'static str> {
    let report = if let Some(root) = &mut frame.root {
        sanitize_tree(root)?
    } else {
        SanitizeReport::default()
    };
    frame.upstream_sanitization.merge(report);
    for request in &mut frame.requests {
        truncate_string(&mut request.kind);
    }
    Ok(report)
}

// Sanitization may shorten display text, but never an authoritative document.
fn text_amounts(root: &Node) -> Result<(usize, usize), &'static str> {
    let mut pending = vec![root];
    let mut surface_values = Vec::new();
    let mut references = Vec::new();
    let mut display = 0usize;
    while let Some(node) = pending.pop() {
        let mut add = |text: &str| display = display.saturating_add(text.len());
        match node {
            Node::Text { content, .. } => add(content),
            Node::RichText { spans, .. } => {
                for span in spans {
                    add(&span.content);
                }
            }
            Node::Input {
                value,
                placeholder,
                options,
                ..
            } => {
                add(value);
                add(placeholder);
                add(&options.label);
                if let Some(description) = &options.description {
                    add(description);
                }
            }
            Node::Editor {
                placeholder,
                options,
                ..
            } => {
                add(placeholder);
                if let Some(rich) = &options.rich {
                    for item in &rich.toolbar {
                        add(&item.label);
                    }
                }
            }
            Node::Image { label, .. }
            | Node::ImageViewer { label, .. }
            | Node::Svg { label, .. } => {
                if let Some(label) = label {
                    add(label);
                }
            }
            // Unknown surfaces display their name in the native placeholder.
            Node::Surface { name, args, .. } => {
                add(name);
                surface_values.extend(args);
            }
            Node::Button {
                content,
                label,
                description,
                ..
            } => {
                if let ButtonContent::Label(text) = content {
                    add(text);
                }
                if let Some(label) = label {
                    add(label);
                }
                if let Some(description) = description {
                    add(description);
                }
            }
            Node::Toggle { label, .. } | Node::Radio { label, .. } => add(label),
            Node::ComboBox {
                options,
                placeholder,
                ..
            } => {
                for option in options {
                    add(option);
                }
                add(placeholder);
            }
            Node::PickList {
                options,
                placeholder,
                ..
            } => {
                for option in options {
                    add(option);
                }
                if let Some(placeholder) = placeholder {
                    add(placeholder);
                }
            }
            _ => {}
        }
        if let Node::Editor {
            document, options, ..
        } = node
        {
            if let Some(rich) = &options.rich {
                rich.document.validate()?;
                if rich.toolbar.len() > editor_presentation::MAX_EDITOR_MENU_ITEMS {
                    return Err("rich toolbar limit");
                }
            }
            references.push(document);
        }
        pending.extend(node.children());
    }
    // Surface strings share the display budget (for example a code preview).
    // Record/type names are routing metadata, not the textual payload itself.
    while let Some(value) = surface_values.pop() {
        match value {
            SurfaceValue::Str(text) => display = display.saturating_add(text.len()),
            SurfaceValue::List(items) => surface_values.extend(items),
            SurfaceValue::Option(Some(item)) => surface_values.push(item.as_ref()),
            SurfaceValue::Record { fields, .. } => {
                surface_values.extend(fields.iter().map(|(_, value)| value));
            }
            SurfaceValue::Unit
            | SurfaceValue::Bool(_)
            | SurfaceValue::I64(_)
            | SurfaceValue::F64(_)
            | SurfaceValue::Option(None) => {}
        }
    }
    editor_document::validate_editor_document_refs(references.iter().copied())
        .map_err(|_| "invalid editor document references or budget")?;
    Ok((references.len(), display))
}

fn sanitize_tree(root: &mut Node) -> Result<SanitizeReport, &'static str> {
    let (documents, before) = text_amounts(root)?;
    let mut budget = MAX_NODES;
    let mut budgets = Budgets {
        text: MAX_TEXT_BYTES_PER_FRAME,
        pictures: MAX_PICTURE_BYTES_PER_FRAME,
        surface_values: MAX_SURFACE_VALUES,
        canvas_parts: MAX_CANVAS_PARTS,
        qr_codes: MAX_QR_CODES,
    };
    let mut taken = Taken::new();
    sanitize_node(root, 0, &mut budget, &mut budgets, &mut taken);
    let (after_documents, after) = text_amounts(root)?;
    if after_documents != documents {
        return Err("frame budget would remove an editor document projection");
    }
    Ok(SanitizeReport {
        display_text_truncated: after < before,
    })
}

/// The most patches one frame may carry. A diff of a tree the host holds
/// needs at most one patch per node it keeps, and a guest past that sends
/// the tree whole; a host applying more would spend, per patch, a walk of
/// a path and a shift of a child list, which is a frame's worth of work at
/// this count already.
pub const MAX_PATCHES: usize = 1024;

/// Applies a patch frame to the tree the host holds, then pulls the result
/// inside every bound [`sanitize`] promises — a patch is the guest's, so an
/// inserted subtree can push the tree past [`MAX_NODES`] or [`MAX_DEPTH`]
/// or reuse a key the tree already has, and the bounds are on the whole.
///
/// `Err` names a patch the tree cannot take: a path to no node, an index
/// past a list, a list operation on a node with no list, a [`Patch::Props`]
/// whose arity is not the node's, or more patches than [`MAX_PATCHES`]. The
/// tree is then part-way through the sequence and not one the guest ever
/// sent: the host drops it and asks for a whole one with [`Event::Resync`].
pub fn apply(root: &mut Node, patches: Vec<Patch>) -> Result<SanitizeReport, &'static str> {
    if patches.len() > MAX_PATCHES {
        return Err("more patches than the host applies");
    }
    for patch in patches {
        apply_one(root, patch)?;
    }
    sanitize_tree(root)
}

fn apply_one(root: &mut Node, patch: Patch) -> Result<(), &'static str> {
    let (path, edit) = match patch {
        Patch::Replace { path, node } => (path, Edit::Replace(node)),
        Patch::Props { path, node } => (path, Edit::Props(node)),
        Patch::Insert { path, index, node } => (path, Edit::Insert(index, node)),
        Patch::Remove { path, index } => (path, Edit::Remove(index)),
        Patch::Move { path, from, to } => (path, Edit::Move(from, to)),
    };
    let mut target = root;
    for index in path {
        target = target
            .children_mut()
            .get_mut(index as usize)
            .ok_or("a path to no node")?;
    }
    match edit {
        Edit::Replace(node) => *target = node,
        Edit::Props(mut node) => {
            let children = target.detach();
            node.attach(children).ok_or("props of another arity")?;
            *target = node;
        }
        Edit::Insert(index, node) => {
            let list = target.child_list_mut().ok_or("a list edit on no list")?;
            if index as usize > list.len() {
                return Err("an index past the list");
            }
            list.insert(index as usize, node);
        }
        Edit::Remove(index) => {
            let list = target.child_list_mut().ok_or("a list edit on no list")?;
            if index as usize >= list.len() {
                return Err("an index past the list");
            }
            list.remove(index as usize);
        }
        Edit::Move(from, to) => {
            let list = target.child_list_mut().ok_or("a list edit on no list")?;
            if from as usize >= list.len() || to as usize >= list.len() {
                return Err("an index past the list");
            }
            let node = list.remove(from as usize);
            list.insert(to as usize, node);
        }
    }
    Ok(())
}

/// A [`Patch`] with its path taken off.
enum Edit {
    Replace(Node),
    Props(Node),
    Insert(u32, Node),
    Remove(u32),
    Move(u32, u32),
}

/// The patches that turn `old` into `new`: `apply(old, diff(old, new))`
/// leaves `old == new`. Both are borrowed mutably only to compare a node's
/// own fields with its children set aside; each is put back as it was.
///
/// A list of children is matched by key — a keyed child that moved is a
/// [`Patch::Move`], one that left a [`Patch::Remove`], a new one a
/// [`Patch::Insert`] — and two lists of the same shape are matched by
/// position. Keys are what [`sanitize`] already makes unique on the host.
pub fn diff(old: &mut Node, new: &mut Node) -> Vec<Patch> {
    let mut patches = Vec::new();
    diff_node(old, new, &mut Vec::new(), &mut patches);
    patches
}

fn diff_node(old: &mut Node, new: &mut Node, path: &mut Vec<u32>, out: &mut Vec<Patch>) {
    if old == new {
        return;
    }
    let same_kind = std::mem::discriminant(old) == std::mem::discriminant(new);
    let same_arity = new.child_list_mut().is_some() || old.children().len() == new.children().len();
    if !(same_kind && same_arity) {
        out.push(Patch::Replace {
            path: path.clone(),
            node: new.clone(),
        });
        return;
    }
    let old_children = old.detach();
    let new_children = new.detach();
    if old != new {
        out.push(Patch::Props {
            path: path.clone(),
            node: new.clone(),
        });
    }
    let mut old_children = old_children;
    let mut new_children = new_children;
    match old.child_list_mut().is_some() {
        true => diff_list(&mut old_children, &mut new_children, path, out),
        false => {
            for (index, (old_child, new_child)) in
                old_children.iter_mut().zip(&mut new_children).enumerate()
            {
                path.push(index as u32);
                diff_node(old_child, new_child, path, out);
                path.pop();
            }
        }
    }
    old.attach(old_children).expect("its own children");
    new.attach(new_children).expect("its own children");
}

fn diff_list(old: &mut [Node], new: &mut [Node], path: &mut Vec<u32>, out: &mut Vec<Patch>) {
    let positional = old.len() == new.len()
        && old
            .iter()
            .zip(new.iter())
            .all(|(a, b)| match (a.key(), b.key()) {
                (Some(a), Some(b)) => a == b,
                (None, None) => std::mem::discriminant(a) == std::mem::discriminant(b),
                _ => false,
            });
    if positional {
        for (index, (old_child, new_child)) in old.iter_mut().zip(new.iter_mut()).enumerate() {
            path.push(index as u32);
            diff_node(old_child, new_child, path, out);
            path.pop();
        }
        return;
    }
    // A key that appears once on each side is a child that survives; every
    // other child — unkeyed, or a duplicate the host would rename — is
    // removed and inserted afresh.
    let unique = |nodes: &[Node]| -> std::collections::HashMap<String, usize> {
        let mut seen = std::collections::HashMap::new();
        for (index, node) in nodes.iter().enumerate() {
            if let Some(key) = node.key() {
                seen.entry(key.to_owned())
                    .and_modify(|at| *at = usize::MAX)
                    .or_insert(index);
            }
        }
        seen.retain(|_, at| *at != usize::MAX);
        seen
    };
    let old_keys = unique(old);
    let new_keys = unique(new);
    // The list as the host has it after the patches so far: old indices.
    let mut live: Vec<usize> = Vec::with_capacity(new.len());
    for (index, node) in old.iter().enumerate() {
        let survives = node
            .key()
            .is_some_and(|key| old_keys.contains_key(key) && new_keys.contains_key(key));
        match survives {
            true => live.push(index),
            false => out.push(Patch::Remove {
                path: path.clone(),
                index: live.len() as u32,
            }),
        }
    }
    for (index, new_child) in new.iter_mut().enumerate() {
        let wanted = new_child
            .key()
            .filter(|key| new_keys.contains_key(*key))
            .and_then(|key| old_keys.get(key).copied());
        let Some(wanted) = wanted else {
            out.push(Patch::Insert {
                path: path.clone(),
                index: index as u32,
                node: new_child.clone(),
            });
            live.insert(index, usize::MAX);
            continue;
        };
        let at = live[index..]
            .iter()
            .position(|old_index| *old_index == wanted)
            .expect("a surviving child is still live")
            + index;
        if at != index {
            out.push(Patch::Move {
                path: path.clone(),
                from: at as u32,
                to: index as u32,
            });
            live.remove(at);
            live.insert(index, wanted);
        }
        path.push(index as u32);
        diff_node(&mut old[wanted], new_child, path, out);
        path.pop();
    }
}

/// Every key claimed in one tree, each with the suffix its next duplicate
/// will try. Remembering the suffix is what keeps a tree of one key linear:
/// searching upward from `#2` on every duplicate walks past every earlier
/// one, and a screen of eight thousand nodes sharing a key — the guest's to
/// send — took nine seconds of the window thread that way.
type Taken = std::collections::HashMap<String, usize>;

/// A key already used in this tree, made unique. A key is the node's
/// identity — its widget state, its focus target, its accessibility id, and
/// the [`Node::Input`] whose text the host owns — so two nodes sharing one
/// share all of that: typing in either edits both. The tree the guest sent
/// is kept, with the later node moved off the taken key.
fn claim(key: &mut String, taken: &mut Taken) {
    truncate_string(key);
    let Some(mut nth) = taken.get(key.as_str()).copied() else {
        taken.insert(key.clone(), 2);
        return;
    };
    // The guest may itself have sent `key#2`: a suffix already taken is
    // skipped, and the count moves past it for good.
    let mut unique = format!("{key}#{nth}");
    while taken.contains_key(unique.as_str()) {
        nth += 1;
        unique = format!("{key}#{nth}");
    }
    taken.insert(std::mem::replace(key, unique.clone()), nth + 1);
    taken.insert(unique, 2);
}

/// What is left of a frame's per-frame byte budgets while its tree is walked.
struct Budgets {
    qr_codes: usize,
    canvas_parts: usize,
    surface_values: usize,
    text: usize,
    pictures: usize,
}

/// Truncates one shaped string to what is left of the frame's text budget
/// and spends what survives. Nodes are walked in tree order, so a frame past
/// the budget keeps its head and loses its tail.
fn spend_text(text: &mut String, text_budget: &mut usize) {
    truncate_to(text, (*text_budget).min(MAX_STRING_BYTES));
    *text_budget -= text.len();
}

/// Spends a picture's bytes from the frame's picture budget, or drops them
/// whole when they do not fit.
fn spend_svg(bytes: &mut Option<Vec<u8>>, svg_budget: &mut usize) {
    match bytes {
        Some(picture) if picture.len() <= *svg_budget => *svg_budget -= picture.len(),
        _ => *bytes = None,
    }
}

fn sanitize_node(
    node: &mut Node,
    depth: usize,
    budget: &mut usize,
    budgets: &mut Budgets,
    taken: &mut Taken,
) {
    // The caller guarantees one node of budget; a node too deep spends it
    // on the empty node that stands in for it.
    *budget -= 1;
    if depth >= MAX_DEPTH {
        *node = Node::empty();
        return;
    }
    match node {
        Node::Container {
            shadow,
            max_width,
            max_height,
            key,
            padding,
            border,
            background,
            ..
        } => {
            claim(key, taken);
            bound_optional(max_width);
            bound_optional(max_height);
            shadow.sanitize();
            bound_edges(padding);
            bound_border(border);
            if let Some(background) = background {
                background.sanitize();
            }
        }
        Node::Linear {
            max_width,
            key,
            wrap,
            spacing,
            padding,
            background,
            border,
            ..
        } => {
            claim(key, taken);
            bound_optional(max_width);
            bound_optional(spacing);
            if let Some(wrap) = wrap {
                bound_optional(&mut wrap.spacing);
            }
            bound_edges(padding);
            bound_color(background);
            bound_border(border);
        }
        Node::Flex {
            key,
            layout,
            background,
            border,
            items,
            children,
        } => {
            claim(key, taken);
            layout.sanitize();
            bound_color(background);
            bound_border(border);
            children.truncate(MAX_NODES);
            items.resize(children.len(), FlexItem::default());
            for item in items {
                item.sanitize();
            }
        }
        Node::KeyedColumn {
            key,
            keys,
            background,
            border,
            spacing,
            padding,
            max_width,
            virtual_row,
            children,
            ..
        } => {
            claim(key, taken);
            bound_color(background);
            bound_border(border);
            bound_optional(spacing);
            bound_edges(padding);
            bound_optional(max_width);
            if let Some(estimate) = virtual_row {
                *estimate = bounded(*estimate).max(1.0);
            }
            let count = keys
                .as_ref()
                .map_or(children.len(), |keys| keys.len().min(children.len()))
                .min(MAX_NODES);
            if let Some(keys) = keys {
                keys.truncate(count);
            }
            children.truncate(count);
        }
        Node::Grid {
            key,
            fluid,
            spacing,
            padding,
            aspect,
            background,
            border,
            ..
        } => {
            claim(key, taken);
            bound_optional(fluid);
            bound_optional(spacing);
            bound_edges(padding);
            bound_optional(aspect);
            bound_color(background);
            bound_border(border);
        }
        Node::Sensor {
            key,
            reset,
            anticipate,
            delay,
            ..
        } => {
            claim(key, taken);
            if let Some(value) = reset
                && !value.bound(0, &mut budgets.surface_values, &mut budgets.text, false)
            {
                *reset = None;
            }
            bound_optional(anticipate);
            if let Some(delay) = delay {
                *delay = finite(*delay).max(0.0);
            }
        }
        Node::ResizeHandle { key, .. }
        | Node::MouseArea { key, .. }
        | Node::Responsive { key, .. }
        | Node::Lazy { key, .. } => claim(key, taken),
        Node::Stack {
            key,
            padding,
            background,
            border,
            under,
            ..
        } => {
            claim(key, taken);
            bound_edges(padding);
            bound_color(background);
            bound_border(border);
            *under = (*under).min(MAX_NODES as u32);
        }
        Node::Hover {
            key,
            padding,
            background,
            border,
            tint,
            radius,
            children,
            ..
        } => {
            claim(key, taken);
            bound_edges(padding);
            bound_color(background);
            bound_border(border);
            bound_color(tint);
            *radius = bounded(*radius);
            children.truncate(2);
        }
        Node::Float {
            key,
            scale,
            shadow,
            radius,
            ..
        } => {
            claim(key, taken);
            *scale = finite(*scale).clamp(f32::EPSILON, MAX_PIXELS);
            shadow.sanitize();
            if let Some(corners) = radius {
                for corner in corners {
                    *corner = bounded(*corner);
                }
            }
        }
        Node::Pin { key, x, y, .. } => {
            claim(key, taken);
            *x = finite(*x).clamp(-MAX_PIXELS, MAX_PIXELS);
            *y = finite(*y).clamp(-MAX_PIXELS, MAX_PIXELS);
        }
        Node::Tooltip {
            key,
            gap,
            padding,
            delay_ms,
            style,
            children,
            ..
        } => {
            claim(key, taken);
            *gap = bounded(*gap);
            *padding = bounded(*padding);
            style.sanitize();
            *delay_ms = (*delay_ms).min(60_000);
            children.truncate(2);
        }
        Node::Overlay {
            key,
            padding,
            backdrop,
            children,
            ..
        } => {
            claim(key, taken);
            *padding = bounded(*padding);
            children.truncate(2);
            for channel in &mut backdrop.0 {
                *channel = finite(*channel).clamp(0.0, 1.0);
            }
        }

        Node::Canvas { key, commands, .. } => {
            claim(key, taken);
            canvas::sanitize(commands, &mut budgets.canvas_parts);
        }
        Node::When { key, condition, .. } => {
            claim(key, taken);
            condition.sanitize();
        }
        Node::Scroll {
            key,
            bar_width,
            bar_margin,
            scroller_width,
            bar_spacing,
            background,
            border,
            ..
        } => {
            claim(key, taken);
            for number in [bar_width, bar_margin, scroller_width, bar_spacing] {
                bound_optional(number);
            }
            bound_color(background);
            bound_border(border);
        }
        Node::Qr { key, code } => {
            claim(key, taken);
            code.sanitize(&mut budgets.text, &mut budgets.qr_codes);
        }
        Node::RichText {
            key,
            spans,
            size,
            color,
            options,
            ..
        } => {
            claim(key, taken);
            options.sanitize(&mut budgets.text);
            rich_text::sanitize(spans, &mut budgets.text, budget);
            if let Some(size) = size {
                *size = bounded(*size).min(MAX_TEXT_PIXELS);
            }
            bound_color(color);
        }
        Node::Text {
            options,
            key,
            content,
            size,
            color,
            ..
        } => {
            claim(key, taken);
            options.sanitize(&mut budgets.text);
            spend_text(content, &mut budgets.text);
            // Tracking expands graphemes into native widgets. Charge a conservative
            // scalar count against the same host node budget before rendering.
            if options.tracking > 0.0 {
                if let Some((end, _)) = content.char_indices().nth(*budget) {
                    content.truncate(end);
                }
                *budget = budget.saturating_sub(content.chars().count());
            }
            if let Some(size) = size {
                *size = bounded(*size).min(MAX_TEXT_PIXELS);
            }
            bound_color(color);
        }
        Node::ImageViewer {
            key,
            data,
            label,
            options,
            ..
        } => {
            claim(key, taken);
            ImageData::sanitize(data, &mut budgets.pictures);
            if let Some(label) = label {
                truncate_string(label);
            }
            options.sanitize();
        }
        Node::Image {
            key,
            data,
            label,
            rotation,
            opacity,
            ..
        } => {
            claim(key, taken);
            ImageData::sanitize(data, &mut budgets.pictures);
            if let Some(label) = label {
                truncate_string(label);
            }
            if let Some(Rotation::Floating(radians) | Rotation::Solid(radians)) = rotation {
                *radians = finite(*radians);
            }
            if let Some(opacity) = opacity {
                *opacity = finite(*opacity).clamp(0.0, 1.0);
            }
        }
        Node::Svg {
            key,
            bytes,
            label,
            color,
            hover,
            rotation,
            opacity,
            ..
        } => {
            claim(key, taken);
            spend_svg(bytes, &mut budgets.pictures);
            if let Some(label) = label {
                truncate_string(label);
            }
            bound_color(color);
            if let Some(hover) = hover {
                bound_color(hover);
            }
            if let Some(Rotation::Floating(radians) | Rotation::Solid(radians)) = rotation {
                *radians = finite(*radians);
            }
            if let Some(opacity) = opacity {
                *opacity = bounded(*opacity).min(1.0);
            }
        }
        Node::Input {
            key,
            placeholder,
            value,
            options,
            style,
            ..
        } => {
            claim(key, taken);
            spend_text(placeholder, &mut budgets.text);
            spend_text(value, &mut budgets.text);
            spend_text(&mut options.label, &mut budgets.text);
            if let Some(description) = &mut options.description {
                spend_text(description, &mut budgets.text);
            }
            bound_edges(&mut options.padding);
            if let Some(size) = &mut options.text_size {
                *size = bounded(*size).clamp(f32::EPSILON, MAX_TEXT_PIXELS);
            }
            if let Some(height) = &mut options.line_height {
                *height = bounded(*height).clamp(f32::EPSILON, MAX_PIXELS / MAX_TEXT_PIXELS);
            }
            if let Some(font) = &mut options.font {
                font.sanitize(&mut budgets.text);
            }

            style.sanitize();
        }
        Node::Editor {
            options,
            key,
            placeholder,
            width,
            min_height,
            max_height,
            ..
        } => {
            if let Some(presentation) = &mut options.presentation {
                presentation.sanitize(&mut budgets.text);
            }
            if let Some(rich) = &mut options.rich {
                for item in &mut rich.toolbar {
                    spend_text(&mut item.label, &mut budgets.text);
                }
            }
            bound_optional(&mut options.size);
            if let Some(size) = &mut options.size {
                *size = size.clamp(f32::EPSILON, MAX_TEXT_PIXELS);
            }
            bound_optional(&mut options.padding);
            if let Some(line_height) = &mut options.line_height {
                line_height.sanitize();
            }
            options.style.sanitize();
            claim(key, taken);
            spend_text(placeholder, &mut budgets.text);
            if let Some(font) = &mut options.font {
                font.sanitize(&mut budgets.text);
            }
            bound_optional(width);
            bound_optional(min_height);
            bound_optional(max_height);
        }
        Node::Button {
            key,
            content,
            label,
            description,
            padding,
            style,
            ..
        } => {
            claim(key, taken);
            if let ButtonContent::Label(label) = content {
                spend_text(label, &mut budgets.text);
            }
            if let Some(label) = label {
                truncate_string(label);
            }
            if let Some(description) = description {
                spend_text(description, &mut budgets.text);
            }
            if let Some(recipe) = &mut style.recipe {
                recipe.sanitize(&mut budgets.text);
            }
            bound_edges(padding);
            for face in [
                Some(&mut style.active),
                style.hovered.as_mut(),
                style.pressed.as_mut(),
                style.disabled.as_mut(),
            ]
            .into_iter()
            .flatten()
            {
                bound_color(&mut face.background);
                bound_color(&mut face.text);
                bound_border(&mut face.border);
            }
        }
        Node::Space { .. } => {}
        Node::Rule {
            key,
            thickness,
            color,
            radius,
            ..
        } => {
            claim(key, taken);
            *thickness = bounded(*thickness);
            bound_color(color);
            if let Some(radius) = radius {
                for corner in radius {
                    *corner = bounded(*corner);
                }
            }
        }
        Node::Toggle {
            key, label, style, ..
        } => {
            claim(key, taken);
            spend_text(label, &mut budgets.text);
            for face in [
                &mut style.active_on,
                &mut style.active_off,
                &mut style.hovered_on,
                &mut style.hovered_off,
                &mut style.disabled_on,
                &mut style.disabled_off,
            ]
            .into_iter()
            .flatten()
            {
                bound_control_face(face);
            }
        }
        Node::Radio {
            key, label, style, ..
        } => {
            claim(key, taken);
            spend_text(label, &mut budgets.text);
            for face in [
                &mut style.active_on,
                &mut style.active_off,
                &mut style.hovered_on,
                &mut style.hovered_off,
            ]
            .into_iter()
            .flatten()
            {
                bound_control_face(face);
            }
        }
        Node::Slider {
            key,
            value,
            min,
            max,
            step,
            style,
            ..
        } => {
            claim(key, taken);
            for number in [value, min, max, step] {
                *number = finite(*number);
            }
            for face in [&mut style.active, &mut style.hovered, &mut style.dragged]
                .into_iter()
                .flatten()
            {
                bound_color(&mut face.rail_start);
                bound_color(&mut face.rail_end);
                bound_optional(&mut face.rail_width);
                bound_border(&mut face.rail_border);
                bound_color(&mut face.handle);
                bound_border(&mut face.handle_border);
                if let Some(shape) = &mut face.handle_shape {
                    let radii: &mut [f32] = match shape {
                        SliderHandleShape::Circle { radius } => std::slice::from_mut(radius),
                        SliderHandleShape::Rectangle { border_radius, .. } => border_radius,
                    };
                    for radius in radii {
                        *radius = bounded(*radius);
                    }
                }
            }
        }
        Node::ComboBox {
            key,
            state_key,
            options,
            selected,
            placeholder,
            settings,
            ..
        } => {
            claim(key, taken);
            spend_text(state_key, &mut budgets.text);
            settings.sanitize(&mut budgets.text);
            options.truncate(MAX_OPTIONS);
            for option in options.iter_mut() {
                spend_text(option, &mut budgets.text);
            }
            spend_text(placeholder, &mut budgets.text);
            if selected.is_some_and(|index| index as usize >= options.len()) {
                *selected = None;
            }
        }
        Node::PickList {
            settings,
            key,
            options,
            selected,
            placeholder,
            style,
            ..
        } => {
            claim(key, taken);
            settings.sanitize(&mut budgets.text);
            for face in [
                &mut style.active,
                &mut style.hovered,
                &mut style.opened,
                &mut style.opened_hovered,
            ]
            .into_iter()
            .flatten()
            {
                bound_color(&mut face.background);
                bound_color(&mut face.text);
                bound_color(&mut face.placeholder);
                bound_color(&mut face.handle);
                bound_border(&mut face.border);
            }
            if let Some(menu) = &mut style.menu {
                menu.shadow.sanitize();
                bound_color(&mut menu.background);
                bound_color(&mut menu.text);
                bound_border(&mut menu.border);
                bound_color(&mut menu.selected_text);
                bound_color(&mut menu.selected_background);
            }
            options.truncate(MAX_OPTIONS);
            for option in options.iter_mut() {
                spend_text(option, &mut budgets.text);
            }
            if let Some(placeholder) = placeholder {
                spend_text(placeholder, &mut budgets.text);
            }
            if selected.is_some_and(|index| index as usize >= options.len()) {
                *selected = None;
            }
        }
        Node::Progress {
            key,
            value,
            min,
            max,
            background,
            bar,
            border,
            ..
        } => {
            claim(key, taken);
            for number in [value, min, max] {
                *number = finite(*number);
            }
            bound_color(background);
            bound_color(bar);
            bound_border(border);
        }
        Node::Surface {
            key, name, args, ..
        } => {
            claim(key, taken);
            spend_text(name, &mut budgets.text);
            args.truncate(MAX_SURFACE_ARGS);
            let mut kept = 0;
            for value in args.iter_mut() {
                if budgets.surface_values == 0 {
                    break;
                }
                if !value.bound(0, &mut budgets.surface_values, &mut budgets.text, false) {
                    *value = SurfaceValue::Unit;
                }
                kept += 1;
            }
            args.truncate(kept);
        }
    }
    for length in lengths_mut(node) {
        if let Length::Fixed(value) = length {
            *value = bounded(*value);
        }
    }
    // Children past the budget are dropped, not stood in for: a layout of
    // ten thousand rows becomes its first rows, which is what a host can
    // lay out, rather than ten thousand empty nodes it still has to walk.
    if let Node::Linear { children, .. }
    | Node::Grid { children, .. }
    | Node::Stack { children, .. }
    | Node::KeyedColumn { children, .. }
    | Node::Flex { children, .. }
    | Node::When { children, .. } = node
    {
        let mut kept = 0;
        for child in children.iter_mut() {
            if *budget == 0 {
                break;
            }
            sanitize_node(child, depth + 1, budget, budgets, taken);
            kept += 1;
        }
        children.truncate(kept);
        if let Node::KeyedColumn {
            keys: Some(keys), ..
        } = node
        {
            keys.truncate(kept);
        }
        if let Node::Flex { items, .. } = node {
            items.truncate(kept);
        }
        return;
    }
    for child in node.children_mut() {
        if *budget == 0 {
            *child = Node::empty();
            continue;
        }
        sanitize_node(child, depth + 1, budget, budgets, taken);
    }
}

fn lengths_mut(node: &mut Node) -> Vec<&mut Length> {
    let slots: Vec<&mut Option<Length>> = match node {
        Node::Container { width, height, .. }
        | Node::Linear { width, height, .. }
        | Node::Grid { width, height, .. }
        | Node::KeyedColumn { width, height, .. }
        | Node::Pin { width, height, .. }
        | Node::Responsive { width, height, .. }
        | Node::Stack { width, height, .. }
        | Node::Hover { width, height, .. }
        | Node::Scroll { width, height, .. }
        | Node::Button { width, height, .. }
        | Node::Svg { width, height, .. }
        | Node::Image { width, height, .. }
        | Node::ImageViewer { width, height, .. }
        | Node::Slider { width, height, .. }
        | Node::Canvas { width, height, .. }
        | Node::Space { width, height } => vec![width, height],
        Node::Progress { length, girth, .. } => vec![length, girth],
        Node::Editor { height, .. } => vec![height],
        Node::RichText { width, .. }
        | Node::Text { width, .. }
        | Node::Input { width, .. }
        | Node::Toggle { width, .. }
        | Node::Radio { width, .. }
        | Node::PickList { width, .. }
        | Node::ComboBox { width, .. } => vec![width],
        Node::Qr { .. }
        | Node::Rule { .. }
        | Node::Lazy { .. }
        | Node::Flex { .. }
        | Node::Sensor { .. }
        | Node::ResizeHandle { .. }
        | Node::MouseArea { .. }
        | Node::Overlay { .. }
        | Node::Tooltip { .. }
        | Node::When { .. }
        | Node::Float { .. }
        | Node::Surface { .. } => Vec::new(),
    };
    slots.into_iter().flatten().collect()
}

/// Cuts `text` down to [`MAX_STRING_BYTES`] on a char boundary, in place.
/// Shared by [`sanitize`] (a guest's outbound frame) and the host's inbound
/// edit path (a user's keystroke or paste into an [`Node::Input`]) — one
/// bound on any string either side of the wire sends the other.
pub fn truncate_string(text: &mut String) {
    truncate_to(text, MAX_STRING_BYTES);
}

fn truncate_to(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

fn bounded(value: f32) -> f32 {
    match value.is_nan() {
        true => 0.0,
        false => value.clamp(0.0, MAX_PIXELS),
    }
}

/// A number that is not a size: a slider or progress value is the app's,
/// so it is made finite and nothing more. The host clamps it into the range
/// it lays out.
/// A pixel measure that may point either way (a paint-only inset), bounded
/// on both sides; NaN reads as 0.
fn signed_bounded(value: f32) -> f32 {
    match value.is_nan() {
        true => 0.0,
        false => value.clamp(-MAX_PIXELS, MAX_PIXELS),
    }
}

fn finite(value: f32) -> f32 {
    match value.is_nan() {
        true => 0.0,
        false => value.clamp(f32::MIN, f32::MAX),
    }
}

fn bound_optional(value: &mut Option<f32>) {
    if let Some(value) = value {
        *value = bounded(*value);
    }
}

fn bound_edges(edges: &mut Option<Edges>) {
    if let Some(edges) = edges {
        edges.top = bounded(edges.top);
        edges.right = bounded(edges.right);
        edges.bottom = bounded(edges.bottom);
        edges.left = bounded(edges.left);
    }
}

fn bound_color(color: &mut Option<Rgba>) {
    if let Some(Rgba(channels)) = color {
        for channel in channels {
            *channel = match channel.is_nan() {
                true => 0.0,
                false => channel.clamp(0.0, 1.0),
            };
        }
    }
}

fn bound_control_face(face: &mut ControlFace) {
    bound_color(&mut face.background);
    bound_color(&mut face.mark);
    bound_color(&mut face.text);
    bound_border(&mut face.border);
}

fn bound_border(border: &mut Option<Border>) {
    if let Some(border) = border {
        bound_color(&mut border.color);
        if let Some(width) = &mut border.width {
            *width = bounded(*width);
        }
        for radius in border.radius.iter_mut().flatten() {
            *radius = bounded(*radius);
        }
    }
}

/// What one `decode` may build before it is refused: enough that
/// [`sanitize`]'s truncation still shapes any tree a real view sends, and
/// few enough that a hostile one cannot make the host allocate its way
/// through a frame's worth of nodes every tick.
const MAX_DECODED_NODES: usize = 16 * MAX_NODES;

/// Bounds what a decode may descend into, since decoding is recursive: a
/// [`Node`] holds its children and serde builds them from the inside out, so
/// a chain of containers is a chain of stack frames. The tree the host walks
/// afterwards — [`sanitize`], the renderer, `Drop` — recurses the same way,
/// which is why the limit is the door rather than each walk.
///
/// [`MAX_FRAME_BYTES`-sized](Frame) input is no protection: a chain deep
/// enough to overflow a host thread's stack is a few tens of kilobytes.
mod budget {
    use std::cell::Cell;

    use super::{MAX_DECODED_NODES, MAX_DEPTH};

    thread_local! {
        static DEPTH: Cell<usize> = const { Cell::new(0) };
        static NODES: Cell<usize> = const { Cell::new(0) };
    }

    /// One node being decoded. Descending past what the host walks, or
    /// building more nodes than it will hold, refuses the whole frame:
    /// there is no partial tree to keep, and a truncated one would be a
    /// tree the guest did not write.
    pub(super) struct Node(());

    impl Node {
        pub(super) fn enter() -> Result<Self, &'static str> {
            let depth = DEPTH.get() + 1;
            if depth > MAX_DEPTH {
                return Err("a tree deeper than the host renders");
            }
            spend(1)?;
            DEPTH.set(depth);
            Ok(Self(()))
        }
    }

    impl Drop for Node {
        fn drop(&mut self) {
            DEPTH.set(DEPTH.get().saturating_sub(1));
        }
    }

    /// Native paragraph spans share the same aggregate allocation allowance as nodes.
    pub(super) fn spend(count: usize) -> Result<(), &'static str> {
        let nodes = NODES.get().saturating_add(count);
        if nodes > MAX_DECODED_NODES {
            return Err("more nodes than the host holds");
        }
        NODES.set(nodes);
        Ok(())
    }

    /// A fresh budget for one top-level [`decode`](super::decode). The depth
    /// unwinds itself; the node count is what one frame may spend.
    pub(super) fn reset() {
        NODES.set(0);
    }
}

fn decode_child<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Box<Node>, D::Error> {
    let _node = budget::Node::enter().map_err(serde::de::Error::custom)?;
    Box::<Node>::deserialize(deserializer)
}

fn decode_children<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Node>, D::Error> {
    struct Children;

    impl<'de> serde::de::Visitor<'de> for Children {
        type Value = Vec<Node>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a list of nodes")
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut children: A,
        ) -> Result<Self::Value, A::Error> {
            let mut nodes = Vec::new();
            // The guard is held while one child is built and dropped before
            // the next: siblings share a depth, and each costs a node.
            while let Some(child) = {
                let _node = budget::Node::enter().map_err(serde::de::Error::custom)?;
                children.next_element::<Node>()?
            } {
                nodes.push(child);
            }
            Ok(nodes)
        }
    }

    deserializer.deserialize_seq(Children)
}

pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    bincode::serialize(value).expect("wire types are plain data")
}

/// How many bytes [`encode`] would write, without writing them.
pub fn encoded_size<T: Serialize>(value: &T) -> u64 {
    bincode::serialized_size(value).expect("wire types are plain data")
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, String> {
    budget::reset();
    surface::reset_decode_budget();
    editor_transaction::reset_decode_budget();
    canvas::reset_decode_budget();
    bincode::deserialize(bytes).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(content: &str) -> Node {
        Node::Text {
            options: Default::default(),
            key: "App/t".into(),
            content: content.into(),
            size: Some(16.0),
            color: Some(Rgba([0.1, 0.2, 0.3, 1.0])),
            font: Font::default(),
            width: None,
            align_x: None,
        }
    }

    #[test]
    fn actual_display_truncation_report_survives_encoding_and_resanitizing() {
        let mut frame = Frame {
            root: Some(text(&"x".repeat(MAX_STRING_BYTES + 1))),
            ..Default::default()
        };
        let report = sanitize(&mut frame).unwrap();
        assert!(
            report.display_text_truncated,
            "actual shortened text must be reported"
        );
        assert_eq!(frame.upstream_sanitization, report);
        let mut received: Frame = decode(&encode(&frame)).unwrap();
        assert!(
            !sanitize(&mut received).unwrap().display_text_truncated,
            "receiver observes no additional shortening"
        );
        assert!(
            received.upstream_sanitization.display_text_truncated,
            "producer loss cannot disappear across a wire hop"
        );
        let mut small = Frame {
            root: Some(text("complete")),
            ..Default::default()
        };
        assert_eq!(sanitize(&mut small).unwrap(), SanitizeReport::default());
        assert_eq!(small.upstream_sanitization, SanitizeReport::default());
    }

    #[test]
    fn text_passed_to_a_host_surface_reports_actual_loss() {
        let mut frame = Frame {
            root: Some(Node::Surface {
                key: "preview".into(),
                name: "forge_code".into(),
                args: vec![SurfaceValue::Record {
                    name: "Preview".into(),
                    fields: vec![(
                        "text".into(),
                        SurfaceValue::Option(Some(Box::new(SurfaceValue::List(vec![
                            SurfaceValue::Str("x".repeat(MAX_STRING_BYTES)),
                        ])))),
                    )],
                }],
                on_event: None,
            }),
            ..Default::default()
        };
        assert!(
            sanitize(&mut frame).unwrap().display_text_truncated,
            "surface text spends the same frame budget and its loss must be reported"
        );
        assert!(!sanitize(&mut frame).unwrap().display_text_truncated);
    }

    #[test]
    fn applied_aggregate_text_and_rich_text_loss_is_reported_but_removal_is_not() {
        let rich = Node::RichText {
            key: "rich".into(),
            spans: vec![RichSpan {
                content: "y".repeat(MAX_TEXT_BYTES_PER_FRAME / 2),
                ..Default::default()
            }],
            size: None,
            color: None,
            font: Font::default(),
            width: None,
            align_x: None,
            options: Default::default(),
            on_link: None,
        };
        let mut root = Node::Linear {
            key: "root".into(),
            axis: Axis::Column,
            max_width: None,
            clip: false,
            wrap: None,
            spacing: None,
            padding: None,
            width: None,
            height: None,
            align: None,
            background: None,
            border: None,
            children: vec![text(&"x".repeat(MAX_TEXT_BYTES_PER_FRAME / 2 + 1))],
        };
        let report = apply(
            &mut root,
            vec![Patch::Insert {
                path: vec![],
                index: 1,
                node: rich,
            }],
        )
        .unwrap();
        assert!(
            report.display_text_truncated,
            "each node fits, but the applied aggregate loses tail text"
        );
        let (_, bytes) = text_amounts(&root).unwrap();
        assert_eq!(bytes, MAX_TEXT_BYTES_PER_FRAME);
        let report = apply(
            &mut root,
            vec![Patch::Remove {
                path: vec![],
                index: 0,
            }],
        )
        .unwrap();
        assert!(
            !report.display_text_truncated,
            "intentional removal precedes the measured sanitizer pass"
        );
    }

    #[test]
    fn shadow_sanitization_preserves_signed_offsets_and_bounds_untrusted_values() {
        let mut shadow = Shadow {
            color: Some(Rgba([f32::NAN, -1.0, 2.0, 0.5])),
            x: Some(-12.0),
            y: Some(f32::INFINITY),
            blur: Some(-1.0),
        };
        shadow.sanitize();
        assert_eq!(
            shadow,
            Shadow {
                color: Some(Rgba([0.0, 0.0, 1.0, 0.5])),
                x: Some(-12.0),
                y: Some(0.0),
                blur: Some(0.0)
            }
        );
        shadow.x = Some(-f32::MAX);
        shadow.y = Some(f32::MAX);
        shadow.blur = Some(f32::MAX);
        shadow.sanitize();
        assert_eq!(
            (shadow.x, shadow.y, shadow.blur),
            (Some(-MAX_PIXELS), Some(MAX_PIXELS), Some(MAX_PIXELS))
        );
    }

    fn document_reference(document: &str, byte_len: u32) -> editor_document::EditorDocumentRef {
        editor_document::EditorDocumentRef {
            document: document.into(),
            reset: 3,
            text_revision: 5,
            revision: 7,
            cursor: EditorCursor::default(),
            byte_len,
        }
    }

    fn editor(key: &str, placeholder: &str, document: editor_document::EditorDocumentRef) -> Node {
        Node::Editor {
            options: Default::default(),
            key: key.into(),
            placeholder: placeholder.into(),
            document,
            on_document: 1,
            editable: true,
            width: None,
            height: None,
            min_height: None,
            max_height: None,
        }
    }

    fn column(children: Vec<Node>) -> Node {
        Node::Linear {
            max_width: None,
            clip: false,
            wrap: None,
            key: "App/col".into(),
            axis: Axis::Column,
            spacing: Some(8.0),
            padding: None,
            width: Some(Length::Fill),
            height: None,
            align: None,
            background: None,
            border: None,
            children,
        }
    }

    #[test]
    fn sanitized_surfaces_share_the_decoders_value_budget() {
        let mut frame = Frame {
            root: Some(column(
                (0..20)
                    .map(|i| Node::Surface {
                        key: format!("surface-{i}"),
                        name: "many".into(),
                        args: vec![SurfaceValue::Unit; MAX_SURFACE_ARGS],
                        on_event: None,
                    })
                    .collect(),
            )),
            ..Frame::default()
        };
        sanitize(&mut frame).unwrap();
        let decoded = decode::<Frame>(&encode(&frame));
        assert!(decoded.is_ok(), "sanitized frame must decode: {decoded:?}");
    }

    #[test]
    fn surfaces_round_trip_patch_and_bound_their_arguments() {
        use SurfaceValue as V;
        let values = vec![
            V::Unit,
            V::Bool(true),
            V::I64(i64::MAX),
            V::F64(1.25),
            V::Str("link".into()),
        ];
        let node = Node::Surface {
            key: "view".into(),
            name: "preview".into(),
            args: values.clone(),
            on_event: Some(4),
        };
        assert_eq!(decode::<Node>(&encode(&node)).unwrap(), node);
        for value in values {
            let event = Event::Surface { handler: 4, value };
            assert_eq!(decode::<Event>(&encode(&event)).unwrap(), event);
        }
        let mut changed = node.clone();
        if let Node::Surface { args, on_event, .. } = &mut changed {
            args[1] = V::Bool(false);
            *on_event = Some(9);
        }
        let patches = diff(&mut node.clone(), &mut changed.clone());
        let mut applied = node;
        apply(&mut applied, patches).unwrap();
        assert_eq!(applied, changed);
        let mut frame = Frame {
            root: Some(Node::Surface {
                key: "view".into(),
                name: "preview".into(),
                args: std::iter::once(V::F64(f64::NAN))
                    .chain(std::iter::repeat_n(
                        V::Str("é".repeat(MAX_STRING_BYTES)),
                        MAX_SURFACE_ARGS + 1,
                    ))
                    .collect(),
                on_event: None,
            }),
            ..Frame::default()
        };
        sanitize(&mut frame).unwrap();
        let Node::Surface { name, args, .. } = frame.root.unwrap() else {
            unreachable!()
        };
        assert_eq!(args.len(), MAX_SURFACE_ARGS);
        assert_eq!(args[0], V::F64(0.0));
        let bytes = args
            .iter()
            .map(|value| match value {
                V::Str(text) => text.len(),
                _ => 0,
            })
            .sum::<usize>();
        assert!(bytes + name.len() <= MAX_TEXT_BYTES_PER_FRAME);
    }

    #[test]
    fn border_sanitization_preserves_absence_and_explicit_zero() {
        let absent = Some(Border {
            color: None,
            width: None,
            radius: None,
        });
        let mut bounded = absent;
        bound_border(&mut bounded);
        assert_eq!(bounded, absent);
        assert_eq!(decode::<Option<Border>>(&encode(&bounded)).unwrap(), absent);
        let mut explicit = Some(Border {
            color: Some(Rgba([0.0; 4])),
            width: Some(f32::NAN),
            radius: Some([-1.0; 4]),
        });
        bound_border(&mut explicit);
        let zero = Some(Border {
            color: Some(Rgba([0.0; 4])),
            width: Some(0.0),
            radius: Some([0.0; 4]),
        });
        assert_eq!(explicit, zero);
        assert_ne!(explicit, absent);
        assert_eq!(decode::<Option<Border>>(&encode(&explicit)).unwrap(), zero);
    }

    #[test]
    fn a_frame_round_trips() {
        let frame = Frame {
            upstream_sanitization: Default::default(),
            editor_decisions: Vec::new(),
            editor_documents: Vec::new(),
            mouse_interest: true,
            event_interest: Default::default(),
            root: Some(column(vec![
                text("hello"),
                Node::Button {
                    checked: None,
                    expanded: None,
                    description: None,
                    key: "App/b".into(),
                    content: ButtonContent::Label("Go".into()),
                    label: None,
                    on_press: Some(3),
                    width: None,
                    height: None,
                    padding: Some(Edges::all(4.0)),
                    style: ButtonStyle::default(),
                },
                Node::Input {
                    options: Default::default(),
                    key: "App/i".into(),
                    placeholder: "Name".into(),
                    value: "x".into(),
                    on_input: 0,
                    on_submit: Some(4),
                    width: Some(Length::Fixed(200.0)),
                    secure: false,
                    style: Box::default(),
                },
                Node::Editor {
                    options: Default::default(),
                    key: "App/e".into(),
                    placeholder: "Notes".into(),
                    document: document_reference("app:draft", 9),
                    on_document: 5,
                    editable: true,
                    width: None,
                    height: Some(Length::Fill),
                    min_height: Some(80.0),
                    max_height: None,
                },
            ])),
            patches: vec![Patch::Remove {
                path: vec![0, 1],
                index: 2,
            }],
            requests: vec![Request {
                id: 1,
                kind: "host.echo".into(),
                payload: b"hi".to_vec(),
            }],
            cancels: vec![2],
            unchanged: false,
            busy: false,
        };
        assert_eq!(decode::<Frame>(&encode(&frame)).unwrap(), frame);
        let events = vec![
            Event::Message(3),
            Event::Input {
                handler: 0,
                text: "xy".into(),
            },
            Event::EditorTransaction {
                handler: 5,
                event: EditorTransactionEvent::Commit {
                    origin: None,
                    id: EditorTransactionId {
                        instance: 1,
                        document: "app:draft".into(),
                        reset: 0,
                        sequence: 2,
                        attempt: 0,
                        text_revision: 1,
                        revision: 3,
                    },
                    before: document_reference("app:draft", 9),
                    after: document_reference("app:draft", 10),
                    patches: vec![EditorPatch {
                        start_byte: 9,
                        end_byte: 9,
                        replacement: "z".into(),
                    }],
                    kind: EditorEditKind::Insert,
                    history: EditorHistoryEffect::ExtendPrevious,
                    input_time_ms: 42,
                },
            },
            Event::Toggle {
                handler: 1,
                on: true,
            },
            Event::Slide {
                handler: 2,
                value: 0.5,
            },
            Event::Select {
                handler: 3,
                index: 1,
            },
            Event::Pointer {
                handler: 4,
                x: 12.5,
                y: 3.0,
            },
            Event::Scroll {
                handler: 5,
                dx: 0.0,
                dy: -1.0,
                pixels: false,
            },
            Event::ScrollOffset {
                handler: 6,
                x: 24.0,
                y: 50.0,
                relative_x: 0.2,
                relative_y: 0.1,
            },
            Event::Response {
                id: 1,
                result: Err("nope".into()),
                done: true,
            },
            Event::Resync,
        ];
        assert_eq!(decode::<Vec<Event>>(&encode(&events)).unwrap(), events);
    }

    fn keyed(key: &str, content: &str) -> Node {
        let mut node = text(content);
        let Node::Text { key: slot, .. } = &mut node else {
            panic!()
        };
        *slot = key.into();
        node
    }

    /// `diff` then `apply` is the identity on the new tree, and the patches
    /// are the ones a reader expects: a changed field is `Props`, a moved
    /// key is `Move`, a new key `Insert`, a gone key `Remove`, and a node
    /// of another kind `Replace`.
    #[test]
    fn a_diff_applied_to_the_old_tree_is_the_new_tree() {
        let old = column(vec![
            keyed("a", "one"),
            keyed("b", "two"),
            keyed("c", "three"),
            Node::Container {
                shadow: Default::default(),
                max_width: None,
                max_height: None,
                clip: false,
                key: "box".into(),
                width: None,
                height: None,
                padding: None,
                align_x: None,
                align_y: None,
                background: None,
                border: None,
                snap: None,
                content: Box::new(keyed("inner", "deep")),
            },
        ]);
        let mut new = column(vec![
            keyed("c", "three"),
            keyed("a", "one!"),
            keyed("d", "four"),
            Node::Container {
                shadow: Default::default(),
                max_width: None,
                max_height: None,
                clip: false,
                key: "box".into(),
                width: None,
                height: None,
                padding: Some(Edges::all(4.0)),
                align_x: None,
                align_y: None,
                background: None,
                border: None,
                snap: None,
                content: Box::new(Node::empty()),
            },
        ]);
        let mut applied = old.clone();
        let patches = diff(&mut applied.clone(), &mut new.clone());
        let kinds: Vec<&str> = patches
            .iter()
            .map(|patch| match patch {
                Patch::Replace { .. } => "replace",
                Patch::Props { .. } => "props",
                Patch::Insert { .. } => "insert",
                Patch::Remove { .. } => "remove",
                Patch::Move { .. } => "move",
            })
            .collect();
        assert_eq!(
            kinds,
            ["remove", "move", "props", "insert", "props", "replace"],
            "{patches:#?}"
        );
        // A container's props cross without its content.
        let Patch::Props { path, node } = &patches[4] else {
            panic!()
        };
        assert_eq!(path, &[3]);
        assert_eq!(node.children(), &[Node::empty()]);
        apply(&mut applied, patches).unwrap();
        assert_eq!(applied, new);
        // Nothing changed hands: both inputs of the diff are as they were.
        let mut untouched = old.clone();
        diff(&mut untouched, &mut new);
        assert_eq!(untouched, old);
        assert!(diff(&mut new.clone(), &mut new).is_empty());
    }

    #[test]
    fn a_patch_the_tree_cannot_take_is_refused() {
        let tree = column(vec![keyed("a", "one")]);
        let refused = |patch: Patch| apply(&mut tree.clone(), vec![patch]).unwrap_err();
        assert_eq!(
            refused(Patch::Remove {
                path: vec![7],
                index: 0
            }),
            "a path to no node"
        );
        assert_eq!(
            refused(Patch::Remove {
                path: vec![],
                index: 1
            }),
            "an index past the list"
        );
        assert_eq!(
            refused(Patch::Insert {
                path: vec![0],
                index: 0,
                node: Node::empty()
            }),
            "a list edit on no list"
        );
        assert_eq!(
            refused(Patch::Props {
                path: vec![0],
                node: button(ButtonContent::Child(Box::new(Node::empty())))
            }),
            "props of another arity"
        );
        let many = vec![
            Patch::Move {
                path: vec![],
                from: 0,
                to: 0
            };
            MAX_PATCHES + 1
        ];
        assert_eq!(
            apply(&mut tree.clone(), many).unwrap_err(),
            "more patches than the host applies"
        );
    }

    /// A patch is bounded like a tree: the result of applying it is inside
    /// every limit, however the patches were shaped.
    #[test]
    fn an_applied_patch_frame_is_a_sanitized_tree() {
        let mut tree = column(
            (0..MAX_NODES - 1)
                .map(|i| keyed(&i.to_string(), "x"))
                .collect(),
        );
        sanitize_tree(&mut tree).unwrap();
        assert_eq!(tree.count(), MAX_NODES);
        let mut deep = keyed("0", "leaf");
        for _ in 0..MAX_DEPTH {
            deep = column(vec![deep]);
        }
        apply(
            &mut tree,
            vec![
                Patch::Insert {
                    path: vec![],
                    index: 0,
                    node: keyed("1", &"y".repeat(MAX_STRING_BYTES + 1)),
                },
                Patch::Replace {
                    path: vec![1],
                    node: deep,
                },
            ],
        )
        .unwrap();
        assert!(tree.count() <= MAX_NODES, "{}", tree.count());
        let Node::Linear { children, .. } = &tree else {
            panic!()
        };
        // The inserted key was already in the tree: walked first now, it
        // keeps the key and the one that had it moves off.
        assert_eq!(children[0].key(), Some("1"));
        assert_eq!(children[2].key(), Some("1#2"));
        let Node::Text { content, .. } = &children[0] else {
            panic!()
        };
        assert_eq!(content.len(), MAX_STRING_BYTES);
        let mut depth = 0;
        let mut node = &children[1];
        while let Node::Linear { children, .. } = node {
            depth += 1;
            node = &children[0];
        }
        assert!(depth <= MAX_DEPTH, "{depth}");
    }

    #[test]
    fn a_well_behaved_frame_is_untouched() {
        let mut frame = Frame {
            root: Some(column(vec![text("hello")])),
            ..Frame::default()
        };
        let before = frame.clone();
        sanitize(&mut frame).unwrap();
        assert_eq!(frame, before);
    }

    #[test]
    fn a_frame_past_the_text_budget_keeps_its_head_and_loses_its_tail() {
        const NODES: usize = 8;
        const EACH: usize = MAX_TEXT_BYTES_PER_FRAME / 4;

        let mut frame = Frame {
            root: Some(column(
                (0..NODES).map(|_| text(&"é".repeat(EACH / 2))).collect(),
            )),
            ..Frame::default()
        };
        sanitize(&mut frame).unwrap();
        let Some(Node::Linear { children, .. }) = &frame.root else {
            panic!()
        };
        let shaped: Vec<usize> = children
            .iter()
            .map(|child| match child {
                Node::Text { content, .. } => content.len(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(shaped.iter().sum::<usize>(), MAX_TEXT_BYTES_PER_FRAME);
        assert_eq!(&shaped[..4], &[EACH; 4]);
        assert_eq!(&shaped[4..], &[0; NODES - 4]);
    }

    #[test]
    fn display_truncation_shortens_a_placeholder_and_never_a_document_reference() {
        let long = "x".repeat(2 * MAX_TEXT_BYTES_PER_FRAME);
        let document = document_reference(
            "app:draft",
            editor_document::MAX_EDITOR_DOCUMENT_BYTES as u32,
        );
        let mut frame = Frame {
            root: Some(column(vec![
                editor("App/e", &long, document.clone()),
                text(&long),
            ])),
            ..Frame::default()
        };
        assert_eq!(
            sanitize(&mut frame),
            Ok(SanitizeReport {
                display_text_truncated: true
            })
        );
        let Some(Node::Linear { children, .. }) = &frame.root else {
            panic!()
        };
        let Node::Editor {
            placeholder,
            document: kept,
            ..
        } = &children[0]
        else {
            panic!()
        };
        assert!(
            placeholder.len() < long.len(),
            "an editor placeholder is display text and spends the frame budget"
        );
        assert_eq!(
            kept, &document,
            "a 1 MiB document crosses as an exact reference, not as truncated text"
        );
    }

    #[test]
    fn repeated_editor_bindings_must_describe_one_identical_document() {
        let document = document_reference("app:draft", 32);
        let mut agreeing = Frame {
            root: Some(column(vec![
                editor("App/one", "a", document.clone()),
                editor("App/two", "b", document.clone()),
            ])),
            ..Frame::default()
        };
        assert_eq!(sanitize(&mut agreeing), Ok(SanitizeReport::default()));
        // A distinct logical document is independent, even at the same state.
        let mut independent = Frame {
            root: Some(column(vec![
                editor("App/one", "a", document.clone()),
                editor("App/two", "b", document_reference("app:notes", 32)),
            ])),
            ..Frame::default()
        };
        assert_eq!(sanitize(&mut independent), Ok(SanitizeReport::default()));
        let mut stale = document.clone();
        stale.revision -= 1;
        let mut conflicting = Frame {
            root: Some(column(vec![
                editor("App/one", "a", document),
                editor("App/two", "b", stale),
            ])),
            ..Frame::default()
        };
        assert_eq!(
            sanitize(&mut conflicting),
            Err("invalid editor document references or budget")
        );
    }

    #[test]
    fn a_document_projection_cannot_be_silently_removed_by_the_node_budget() {
        let mut deep = editor("App/e", "notes", document_reference("app:draft", 8));
        for _ in 0..MAX_DEPTH {
            deep = column(vec![deep]);
        }
        let mut frame = Frame {
            root: Some(deep),
            ..Frame::default()
        };
        assert_eq!(
            sanitize(&mut frame),
            Err("frame budget would remove an editor document projection")
        );
    }

    #[test]
    fn every_shaped_string_spends_the_same_budget() {
        let long = "x".repeat(MAX_TEXT_BYTES_PER_FRAME);
        let mut frame = Frame {
            root: Some(column(vec![
                Node::Input {
                    options: Default::default(),
                    key: "App/i".into(),
                    placeholder: long.clone(),
                    value: long.clone(),
                    on_input: 0,
                    on_submit: None,
                    width: None,
                    secure: false,
                    style: Box::default(),
                },
                Node::Button {
                    checked: None,
                    expanded: None,
                    description: Some("Details".into()),
                    key: "App/b".into(),
                    content: ButtonContent::Label(long.clone()),
                    label: Some(long),
                    on_press: None,
                    width: None,
                    height: None,
                    padding: None,
                    style: ButtonStyle::default(),
                },
                text("tail"),
            ])),
            ..Frame::default()
        };
        sanitize(&mut frame).unwrap();
        let Some(Node::Linear { children, .. }) = &frame.root else {
            panic!()
        };
        let Node::Input {
            placeholder, value, ..
        } = &children[0]
        else {
            panic!()
        };
        // The placeholder took the frame's whole budget and everything
        // shaped after it came out empty. The accessible name is not shaped,
        // so it answers to the per-string cap alone.
        assert_eq!(placeholder.len(), MAX_TEXT_BYTES_PER_FRAME);
        assert!(value.is_empty());
        let Node::Button {
            content,
            label,
            description,
            ..
        } = &children[1]
        else {
            panic!()
        };
        assert_eq!(*content, ButtonContent::Label(String::new()));
        assert_eq!(description.as_deref(), Some(""));
        assert_eq!(label.as_deref().map(str::len), Some(MAX_STRING_BYTES));
        assert_eq!(children[2], text(""));
    }

    #[test]
    fn a_hostile_frame_is_pulled_into_range() {
        let mut deep = text("leaf");
        for _ in 0..MAX_DEPTH + 10 {
            deep = column(vec![deep]);
        }
        let wide = column((0..MAX_NODES + 5).map(|_| text("x")).collect());
        let mut frame = Frame {
            root: Some(column(vec![
                Node::Text {
                    options: Default::default(),
                    key: "k".repeat(MAX_STRING_BYTES + 3),
                    content: "é".repeat(MAX_STRING_BYTES),
                    size: Some(f32::NAN),
                    color: Some(Rgba([2.0, -1.0, f32::INFINITY, 0.5])),
                    font: Font::default(),
                    width: Some(Length::Fixed(-5.0)),
                    align_x: None,
                },
                deep,
                wide,
            ])),
            ..Frame::default()
        };
        sanitize(&mut frame).unwrap();
        let root = frame.root.unwrap();
        // A container whose child fell past the budget keeps an empty
        // stand-in, one per level at most.
        assert!(root.count() <= MAX_NODES + MAX_DEPTH, "{}", root.count());
        let Node::Linear { children, .. } = &root else {
            panic!()
        };
        let Node::Text {
            key,
            content,
            size,
            color,
            width,
            ..
        } = &children[0]
        else {
            panic!("{:?}", children[0])
        };
        assert_eq!(key.len(), MAX_STRING_BYTES);
        assert!(content.len() <= MAX_STRING_BYTES && content.is_char_boundary(content.len()));
        assert_eq!(*size, Some(0.0));
        assert_eq!(*color, Some(Rgba([1.0, 0.0, 1.0, 0.5])));
        assert_eq!(*width, Some(Length::Fixed(0.0)));
    }

    #[test]
    fn depth_is_cut_before_the_host_recurses_into_it() {
        let mut deep = text("leaf");
        for _ in 0..MAX_DEPTH * 2 {
            deep = column(vec![deep]);
        }
        let mut frame = Frame {
            root: Some(deep),
            ..Frame::default()
        };
        sanitize(&mut frame).unwrap();
        let mut depth = 0;
        let mut node = frame.root.as_ref().unwrap();
        while let Node::Linear { children, .. } = node {
            depth += 1;
            node = &children[0];
        }
        assert!(depth <= MAX_DEPTH, "{depth}");
    }

    fn button(content: ButtonContent) -> Node {
        Node::Button {
            checked: None,
            expanded: None,
            description: None,
            key: "App/b".into(),
            content,
            label: None,
            on_press: Some(1),
            width: None,
            height: None,
            padding: None,
            style: ButtonStyle::default(),
        }
    }

    /// A button holding a node is the third way the tree recurses, and the
    /// only one that hangs off an enum's field rather than a struct's.
    #[test]
    fn a_button_holding_a_node_round_trips_and_counts_as_a_child() {
        let frame = Frame {
            root: Some(button(ButtonContent::Child(Box::new(text("inside"))))),
            ..Frame::default()
        };
        assert_eq!(decode::<Frame>(&encode(&frame)).unwrap(), frame);

        let mut nested = Node::empty();
        for _ in 0..MAX_DEPTH + 1 {
            nested = button(ButtonContent::Child(Box::new(nested)));
        }
        let bytes = encode(&Frame {
            root: Some(nested),
            ..Frame::default()
        });
        assert!(decode::<Frame>(&bytes).is_err());
    }

    fn mouse_area(key: &str, on_move: Option<u32>, content: Node) -> Node {
        Node::MouseArea {
            key: key.into(),
            on_press: Some(1),
            on_release: None,
            on_double_click: None,
            on_right_press: None,
            on_right_release: None,
            on_middle_press: None,
            on_middle_release: None,
            on_enter: Some(2),
            on_exit: None,
            on_move,
            on_press_at: None,
            on_scroll: Some(3),
            content: Box::new(content),
        }
    }

    /// A mouse area recurses like a container, is diffed as a node with
    /// one fixed child, and claims its key like every other node.
    #[test]
    fn a_mouse_area_round_trips_diffs_by_props_and_claims_its_key() {
        let frame = Frame {
            root: Some(mouse_area("App/m", Some(0), text("inside"))),
            ..Frame::default()
        };
        assert_eq!(decode::<Frame>(&encode(&frame)).unwrap(), frame);
        assert_eq!(frame.root.as_ref().unwrap().count(), 2);

        // A changed route index is a `Props` patch that keeps the child.
        let mut old = mouse_area("App/m", Some(0), text("inside"));
        let mut new = mouse_area("App/m", Some(7), text("inside"));
        let patches = diff(&mut old, &mut new);
        assert!(
            matches!(patches.as_slice(), [Patch::Props { path, .. }] if path.is_empty()),
            "{patches:?}"
        );
        apply(&mut old, patches).unwrap();
        assert_eq!(old, new);

        // Two areas on one key: the second is moved off it, its child kept.
        let mut frame = Frame {
            root: Some(column(vec![
                mouse_area("App/m", None, text("a")),
                mouse_area("App/m", None, text("b")),
            ])),
            ..Frame::default()
        };
        sanitize(&mut frame).unwrap();
        let Some(Node::Linear { children, .. }) = &frame.root else {
            panic!()
        };
        assert_eq!(children[1].key(), Some("App/m#2"));
        assert_eq!(children[1].children().len(), 1);
    }

    /// Building and encoding a chain this deep recurses as far as decoding
    /// it would, so the hostile frame is made where there is stack for it.
    fn deep_chain_bytes(depth: usize) -> Vec<u8> {
        std::thread::Builder::new()
            .stack_size(512 << 20)
            .spawn(move || {
                let mut node = Node::empty();
                for _ in 0..depth {
                    node = column(vec![node]);
                }
                let frame = Frame {
                    root: Some(node),
                    ..Frame::default()
                };
                let bytes = encode(&frame);
                // Dropping it recurses too, and this thread is the one with
                // the stack to do it.
                drop(frame);
                bytes
            })
            .expect("hostile frame thread")
            .join()
            .expect("hostile frame")
    }

    #[test]
    fn a_tree_the_host_would_not_walk_is_refused_before_it_is_built() {
        assert!(decode::<Frame>(&deep_chain_bytes(MAX_DEPTH - 1)).is_ok());
        let refused = decode::<Frame>(&deep_chain_bytes(MAX_DEPTH + 1)).unwrap_err();
        assert!(
            refused.contains("deeper than the host renders"),
            "{refused}"
        );
    }

    /// The bug this guards: a chain of a few thousand containers is a frame
    /// of ~100 KB — far inside any byte cap a host sets — and decoding it
    /// walked a host thread off its stack, aborting the process. A refusal
    /// is a message in one app's window; an overflow is every window gone.
    #[test]
    fn a_chain_that_overflowed_the_host_stack_is_an_error_not_a_crash() {
        let bytes = deep_chain_bytes(5_000);
        assert!(bytes.len() < 1 << 20, "{} bytes", bytes.len());
        assert!(decode::<Frame>(&bytes).is_err());
    }

    #[test]
    fn more_nodes_than_the_host_holds_is_refused() {
        let wide = column((0..MAX_DECODED_NODES + 2).map(|_| Node::empty()).collect());
        let bytes = encode(&Frame {
            root: Some(wide),
            ..Frame::default()
        });
        let refused = decode::<Frame>(&bytes).unwrap_err();
        assert!(
            refused.contains("more nodes than the host holds"),
            "{refused}"
        );
    }

    /// A frame is bytes a module wrote, so every byte of it is the guest's
    /// to choose. Whatever they say, `decode` answers rather than aborts.
    #[test]
    fn bytes_a_hostile_guest_could_write_are_answered_not_survived() {
        let sound = encode(&Frame {
            upstream_sanitization: Default::default(),
            editor_decisions: Vec::new(),
            editor_documents: Vec::new(),
            mouse_interest: false,
            event_interest: Default::default(),
            root: Some(column(vec![text("hello"), Node::empty()])),
            requests: vec![Request {
                id: 7,
                kind: "host.echo".into(),
                payload: b"hi".to_vec(),
            }],
            cancels: vec![1, 2],
            unchanged: false,
            busy: false,
            patches: Vec::new(),
        });
        for cut in 0..sound.len() {
            let _ = decode::<Frame>(&sound[..cut]);
        }
        for at in 0..sound.len() {
            for bit in 0..8 {
                let mut flipped = sound.clone();
                flipped[at] ^= 1 << bit;
                let _ = decode::<Frame>(&flipped);
            }
        }
    }

    #[test]
    fn a_key_used_twice_is_moved_off_the_one_already_taken() {
        let mut frame = Frame {
            root: Some(column(vec![text("one"), text("two"), text("three")])),
            ..Frame::default()
        };
        sanitize(&mut frame).unwrap();
        let Some(Node::Linear { children, .. }) = &frame.root else {
            panic!()
        };
        let keys: Vec<&str> = children.iter().filter_map(Node::key).collect();
        assert_eq!(keys, ["App/t", "App/t#2", "App/t#3"]);
    }

    /// A screen where every node claims the same key, and one where the
    /// guest pre-empted the suffixes: each duplicate must cost one lookup,
    /// not a walk past every earlier one. Measured, not asserted, in debug;
    /// in release a quadratic claim took nine seconds here and a linear one
    /// takes a few milliseconds.
    #[test]
    fn a_screen_of_one_key_is_claimed_in_linear_time() {
        let mut same = text("x");
        let Node::Text { key, .. } = &mut same else {
            panic!()
        };
        *key = "App/t".into();
        let mut children: Vec<Node> = (0..MAX_NODES - 1).map(|_| same.clone()).collect();
        // The guest sent `App/t#2` and `App/t#3` itself: the count skips them.
        for (child, taken) in children.iter_mut().zip(["App/t#2", "App/t#3"]) {
            let Node::Text { key, .. } = child else {
                panic!()
            };
            *key = taken.into();
        }
        let mut frame = Frame {
            root: Some(column(children)),
            ..Frame::default()
        };
        let started = std::time::Instant::now();
        sanitize(&mut frame).unwrap();
        let took = started.elapsed();
        let Some(Node::Linear { children, .. }) = &frame.root else {
            panic!()
        };
        let keys: std::collections::HashSet<&str> = children.iter().filter_map(Node::key).collect();
        assert_eq!(keys.len(), children.len(), "every key unique");
        assert_eq!(children[2].key(), Some("App/t"));
        assert_eq!(children[3].key(), Some("App/t#4"));
        if cfg!(not(debug_assertions)) {
            assert!(took < std::time::Duration::from_millis(200), "{took:?}");
        }
    }

    #[test]
    fn sensor_reset_values_share_the_frame_budget() {
        let sensor = |key: &str| Node::Sensor {
            key: key.into(),
            reset: Some(SurfaceValue::List(vec![SurfaceValue::Unit; 3000])),
            on_show: Some(3),
            on_resize: None,
            on_hide: None,
            anticipate: None,
            delay: None,
            child: Box::new(text("child")),
        };
        let mut frame = Frame {
            root: Some(column(vec![sensor("first"), sensor("second")])),
            ..Frame::default()
        };
        sanitize(&mut frame).unwrap();
        let Some(Node::Linear { children, .. }) = &frame.root else {
            panic!("column retained")
        };
        for (index, node) in children.iter().enumerate() {
            let Node::Sensor {
                key,
                reset,
                on_show,
                child,
                ..
            } = node
            else {
                panic!("sensor retained")
            };
            assert_eq!(
                reset.is_some(),
                index == 0,
                "individually valid reset values must share one frame budget"
            );
            assert_eq!(key, if index == 0 { "first" } else { "second" });
            assert_eq!(*on_show, Some(3));
            assert!(matches!(&**child, Node::Text { content, .. } if content == "child"));
        }
        assert!(
            decode::<Frame>(&encode(&frame)).is_ok(),
            "sanitized aggregate fits the decoder budget"
        );
    }

    #[test]
    fn a_sensor_is_pulled_into_range_and_keeps_its_child() {
        let mut frame = Frame {
            root: Some(Node::Sensor {
                key: "App/watch".into(),
                reset: None,
                on_show: Some(0),
                on_resize: Some(0),
                on_hide: Some(1),
                anticipate: Some(f32::INFINITY),
                delay: Some(-5.0),
                child: Box::new(text("a")),
            }),
            ..Frame::default()
        };
        sanitize(&mut frame).unwrap();
        let Some(Node::Sensor {
            anticipate,
            delay,
            child,
            ..
        }) = &frame.root
        else {
            panic!("{:?}", frame.root)
        };
        assert_eq!(*anticipate, Some(MAX_PIXELS));
        assert_eq!(*delay, Some(0.0));
        assert_eq!(**child, text("a"));
        assert_eq!(frame.root.as_ref().unwrap().count(), 2);
    }

    /// The form controls: a menu is cut to `MAX_OPTIONS` with a selection
    /// past the cut dropped, and a slider's numbers are made finite but not
    /// clamped like a size — a value of a million is the app's to send.
    #[test]
    fn form_controls_are_pulled_into_range() {
        let mut frame = Frame {
            root: Some(column(vec![
                Node::PickList {
                    settings: Default::default(),
                    key: "App/pick".into(),
                    options: (0..MAX_OPTIONS + 3).map(|i| i.to_string()).collect(),
                    selected: Some((MAX_OPTIONS + 1) as u32),
                    placeholder: Some("é".repeat(MAX_STRING_BYTES)),
                    on_select: 0,
                    width: Some(Length::Fixed(f32::INFINITY)),
                    style: PickListStyle::default(),
                },
                Node::Slider {
                    key: "App/slide".into(),
                    value: f32::NAN,
                    min: f32::NEG_INFINITY,
                    max: 1_000_000.0,
                    step: f32::INFINITY,
                    on_change: 1,
                    on_release: None,
                    axis: Axis::Row,
                    width: None,
                    height: None,
                    style: SliderStyle::default(),
                },
                Node::Toggle {
                    key: "App/pick".into(),
                    kind: ToggleKind::Switch,
                    label: "x".repeat(MAX_STRING_BYTES + 1),
                    checked: true,
                    on_toggle: None,
                    width: None,
                    style: ToggleStyle::default(),
                },
            ])),
            ..Frame::default()
        };
        sanitize(&mut frame).unwrap();
        let Some(Node::Linear { children, .. }) = &frame.root else {
            panic!()
        };
        let Node::PickList {
            options,
            selected,
            placeholder,
            width,
            ..
        } = &children[0]
        else {
            panic!("{:?}", children[0])
        };
        assert_eq!(options.len(), MAX_OPTIONS);
        assert_eq!(*selected, None);
        assert!(placeholder.as_ref().unwrap().len() <= MAX_STRING_BYTES);
        assert_eq!(*width, Some(Length::Fixed(MAX_PIXELS)));
        let Node::Slider {
            value,
            min,
            max,
            step,
            ..
        } = &children[1]
        else {
            panic!("{:?}", children[1])
        };
        assert_eq!(
            (*value, *min, *max, *step),
            (0.0, f32::MIN, 1_000_000.0, f32::MAX)
        );
        let Node::Toggle { key, label, .. } = &children[2] else {
            panic!("{:?}", children[2])
        };
        assert_eq!(key, "App/pick#2");
        assert!(label.len() <= MAX_STRING_BYTES);
    }

    /// A grid is bounded like a linear layout: its numbers are pulled into
    /// the pixel range and children past the node budget are dropped, not
    /// stood in for.
    #[test]
    fn a_grid_is_pulled_into_range_and_cut_like_a_linear_layout() {
        let mut frame = Frame {
            root: Some(Node::Grid {
                key: "App/cells".into(),
                columns: Some(u32::MAX),
                fluid: Some(f32::NAN),
                spacing: Some(-3.0),
                padding: Some(Edges::all(f32::INFINITY)),
                width: Some(Length::Fixed(f32::MAX)),
                height: None,
                aspect: Some(f32::NEG_INFINITY),
                background: None,
                border: None,
                children: (0..MAX_NODES + 5).map(|_| text("x")).collect(),
            }),
            ..Frame::default()
        };
        sanitize(&mut frame).unwrap();
        let Some(Node::Grid {
            columns,
            fluid,
            spacing,
            padding,
            width,
            aspect,
            children,
            ..
        }) = &frame.root
        else {
            panic!()
        };
        assert_eq!(*columns, Some(u32::MAX));
        assert_eq!(*fluid, Some(0.0));
        assert_eq!(*spacing, Some(0.0));
        assert_eq!(*padding, Some(Edges::all(MAX_PIXELS)));
        assert_eq!(*width, Some(Length::Fixed(MAX_PIXELS)));
        assert_eq!(*aspect, Some(0.0));
        assert_eq!(children.len(), MAX_NODES - 1);
        assert_eq!(frame.root.as_ref().unwrap().count(), MAX_NODES);
    }

    fn picture(bytes: Option<Vec<u8>>) -> Node {
        Node::Svg {
            inherit_button_ink: false,
            key: "App/icon".into(),
            hash: 7,
            bytes,
            label: None,
            color: None,
            hover: None,
            fit: None,
            rotation: None,
            opacity: None,
            width: Some(Length::Fixed(24.0)),
            height: Some(Length::Fixed(24.0)),
        }
    }

    #[test]
    fn svg_and_raster_images_share_the_frame_picture_budget() {
        let image = Node::Image {
            key: "App/raster".into(),
            hash: 8,
            data: Some(ImageData::Encoded(vec![
                0;
                MAX_PICTURE_BYTES_PER_FRAME / 2 + 1
            ])),
            label: None,
            fit: None,
            rotation: None,
            opacity: None,
            filter: ImageFilter::Linear,
            width: None,
            height: None,
        };
        let mut frame = Frame {
            root: Some(column(vec![
                picture(Some(vec![0; MAX_PICTURE_BYTES_PER_FRAME / 2])),
                image,
            ])),
            ..Frame::default()
        };
        sanitize(&mut frame).unwrap();
        let children = frame.root.as_ref().unwrap().children();
        assert!(matches!(children[0], Node::Svg { bytes: Some(_), .. }));
        assert!(
            matches!(children[1], Node::Image { data: None, .. }),
            "SVG consumption must reduce raster admission"
        );
        assert!(decode::<Frame>(&encode(&frame)).is_ok());
    }

    /// A picture past what is left of the frame's budget is dropped whole,
    /// never cut: half an SVG is not an SVG. The head of the frame keeps
    /// its pictures; a hash without bytes passes as the reference it is.
    #[test]
    fn a_frame_past_the_picture_budget_drops_whole_pictures_from_its_tail() {
        const EACH: usize = MAX_PICTURE_BYTES_PER_FRAME / 4 * 3;
        let mut frame = Frame {
            root: Some(column(vec![
                picture(Some(vec![b'<'; EACH])),
                picture(Some(vec![b'<'; EACH])),
                picture(None),
                picture(Some(vec![b'<'; MAX_PICTURE_BYTES_PER_FRAME / 4])),
            ])),
            ..Frame::default()
        };
        sanitize(&mut frame).unwrap();
        let Some(Node::Linear { children, .. }) = &frame.root else {
            panic!()
        };
        let carried: Vec<Option<usize>> = children
            .iter()
            .map(|child| match child {
                Node::Svg { bytes, .. } => bytes.as_ref().map(Vec::len),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            carried,
            [
                Some(EACH),
                None,
                None,
                Some(MAX_PICTURE_BYTES_PER_FRAME / 4)
            ]
        );
    }

    #[test]
    fn a_text_size_is_capped_where_a_length_is_not() {
        let mut huge = text("huge");
        let Node::Text { size, width, .. } = &mut huge else {
            panic!()
        };
        *size = Some(f32::MAX);
        *width = Some(Length::Fixed(f32::MAX));
        let mut frame = Frame {
            root: Some(huge),
            ..Frame::default()
        };
        sanitize(&mut frame).unwrap();
        let Some(Node::Text { size, width, .. }) = &frame.root else {
            panic!()
        };
        assert_eq!(*size, Some(MAX_TEXT_PIXELS));
        assert_eq!(*width, Some(Length::Fixed(MAX_PIXELS)));
    }
}
