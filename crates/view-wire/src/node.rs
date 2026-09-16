//! The widget tree itself: the [`Node`] enum every frame carries, and the
//! walks over it a host and the differ share.

use crate::*;
use serde::{Deserialize, Serialize};

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
    /// a visible placeholder. It takes the size its parent gives it: wrap it
    /// in a sized [`Node::Container`] to set one.
    Surface {
        key: String,
        name: String,
        args: Vec<SurfaceValue>,
        on_event: Option<u32>,
    },
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

    /// Every node in the tree, depth first, this one included.
    pub fn count(&self) -> usize {
        1 + self.children().iter().map(Node::count).sum::<usize>()
    }
}
