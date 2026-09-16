//! GPUI rendering of the existing WASM tree. Widget identities retain native
//! input state; interaction uses the same semantic events the guests consume.

use gpui_kit::MouseUpEvent;
use gpui_kit::component::radio::Radio;
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{
    Disableable, Selectable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState},
};
use gpui_kit::component::{
    IndexPath,
    searchable_list::{SearchableListDelegate, SearchableListItem},
    select::{SearchableVec, Select, SelectEvent, SelectState},
};
use gpui_kit::{
    AnyElement, AnyView, App, AppContext as _, Bounds, BoxShadow, Context, CursorStyle, Div,
    Element, ElementId, Entity, EntityInputHandler as _, EventEmitter, FocusHandle, Focusable as _,
    FollowMode, FontWeight, GlobalElementId, HighlightStyle, HitboxBehavior, Hsla, Image,
    ImageFormat, InspectorElementId, InteractiveElement as _, IntoElement, LayoutId, ListAlignment,
    ListSizingBehavior, ListState, MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit,
    ParentElement as _, Pixels, Point, Render, RenderImage, ScrollDelta, ScrollHandle,
    ScrollWheelEvent, SharedString, Size, StatefulInteractiveElement as _, StrikethroughStyle,
    Styled, StyledImage as _, StyledText, Subscription, Task, TextLayout, UnderlineStyle, Window,
    auto, canvas, div, fill, img, point, px, relative, rgb, size, svg,
};
use std::collections::HashMap;
use std::sync::Arc;
use view_wire as wire;
use unicode_segmentation::UnicodeSegmentation;

struct RichSelection {
    handle: gpui_kit::base::TextSelectionHandle,
    _refresh: Subscription,
}

// Layout and hit-testing stay native. One participant receives every span's
// measured glyph run so copying concatenates source text, never visual padding.
struct RichParagraph {
    id: ElementId,
    content: AnyElement,
    layouts: Vec<(SharedString, TextLayout)>,
    handle: gpui_kit::base::TextSelectionHandle,
    selections: std::rc::Rc<std::cell::RefCell<Vec<Option<std::ops::Range<usize>>>>>,
}

impl IntoElement for RichParagraph {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for RichParagraph {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.content.request_layout(window, cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        // Register the containing hitbox before link hitboxes, so selectable
        // paragraph geometry cannot cover its own interactive spans.
        self.content.prepaint(window, cx);
        self.handle.register(
            gpui_kit::base::TextSelectionRegistration::new(hitbox, bounds).with_text_bounds(
                self.layouts
                    .iter()
                    .map(|(_, layout)| layout.bounds())
                    .collect(),
            ),
            window,
            cx,
        );
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let runs = self
            .layouts
            .iter()
            .enumerate()
            .map(|(index, (text, layout))| {
                gpui_kit::base::TextSelectionRun::new(text.clone(), layout.clone(), layout.bounds())
                    .with_document_order(index as u64)
            })
            .collect::<Vec<_>>();
        let projection = self.handle.update_runs(&runs, cx);
        *self.selections.borrow_mut() = projection.ranges().to_vec();
        self.content.paint(window, cx);
    }
}

fn paint_rich_selection(
    layout: &TextLayout,
    range: &std::ops::Range<usize>,
    window: &mut Window,
    cx: &App,
) {
    let (Some(start), Some(end)) = (
        layout.position_for_index(range.start),
        layout.position_for_index(range.end),
    ) else {
        return;
    };
    let height = layout.line_height();
    if height <= px(0.) {
        return;
    }
    let color = gpui_kit::base::Theme::global(cx).tokens.colors.selection;
    let mut y = start.y;
    while y <= end.y {
        let left = if y == start.y {
            start.x
        } else {
            layout.bounds().left()
        };
        let right = if y == end.y {
            end.x
        } else {
            layout.bounds().right()
        };
        window.paint_quad(fill(
            Bounds::from_corners(point(left, y), point(right, y + height)),
            color,
        ));
        y += height;
    }
}

struct RangeControl {
    state: Entity<SliderState>,
    bounds: [f32; 3],
    value: f32,
    on_change: u32,
    on_release: Option<u32>,
    _subscription: Subscription,
}

#[derive(Clone, Copy)]
enum ScrollRequest {
    Relative(f32, f32),
    Absolute(f32, f32),
    By(f32, f32),
    End,
    Key(u64),
}

struct SensorState {
    reset: Option<wire::SurfaceValue>,
    size: Option<Size<Pixels>>,
    on_hide: Option<u32>,
    on_show: Option<u32>,
    on_resize: Option<u32>,
    pending: Option<(Size<Pixels>, Task<()>)>,
}

#[derive(Clone)]
struct VirtualRow {
    key: String,
    content: wire::Node,
    gap: f32,
    estimated_height: f32,
}

struct VirtualScroll {
    state: ListState,
    rows: Vec<VirtualRow>,
    anchor: wire::ScrollAnchor,
    measured_width: Option<Pixels>,
}

struct EditorMount {
    view: EditorView,
    _subscription: Subscription,
}

/// An editor's `()` is "the store has events": forward them as this tree's.
fn drain_editor(store: &crate::editor::wire::EditorStore, cx: &mut Context<ViewTree>) {
    for event in store.drain() {
        cx.emit(event);
    }
}

/// Native editor primitives selected by the guest's explicit projection.
enum EditorView {
    Text(Entity<crate::editor::wire::TextEditor>),
    Rich(Entity<crate::editor::wire::RichWireEditor>),
}

impl EditorView {
    fn sync(&self, window: &mut Window, cx: &mut App) {
        match self {
            Self::Text(view) => view.update(cx, |editor, cx| editor.sync(window, cx)),
            Self::Rich(view) => view.update(cx, |editor, cx| editor.sync(window, cx)),
        }
    }

    fn widget_command(&self, command: &wire::WidgetCommand, window: &mut Window, cx: &mut App) {
        match self {
            Self::Text(view) => view.update(cx, |editor, cx| {
                editor.widget_command(command, window, cx);
            }),
            Self::Rich(view) => view.update(cx, |editor, cx| {
                editor.widget_command(command, window, cx);
            }),
        }
    }

    /// The guest echoes which editor held focus when the frame was built, so
    /// the field it named takes the caret back. The notion editor is one
    /// persistent entity: it keeps its own focus, and re-focusing it here
    /// would steal the caret from whatever the guest focused this frame.
    fn restore_focus(&self, key: &str, window: &mut Window, cx: &mut App) {
        let Self::Text(view) = self else {
            return;
        };
        let focus = wire::WidgetCommand::Focus {
            target: key.to_owned(),
        };
        view.update(cx, |editor, cx| {
            editor.widget_command(&focus, window, cx);
        });
    }

    fn is_focused(&self, window: &Window, cx: &App) -> bool {
        match self {
            Self::Text(view) => view.read(cx).is_focused(window, cx),
            Self::Rich(view) => view.read(cx).is_focused(window, cx),
        }
    }

    fn element(&self) -> AnyElement {
        match self {
            Self::Text(view) => view.clone().into_any_element(),
            Self::Rich(view) => view.clone().into_any_element(),
        }
    }
}

#[derive(Default)]
struct ViewerState {
    scale: f32,
    offset: Point<Pixels>,
    drag: Option<Point<Pixels>>,
}

#[derive(Clone)]
struct Choice {
    index: u32,
    label: String,
}

impl SearchableListItem for Choice {
    type Value = u32;
    fn title(&self) -> SharedString {
        self.label.clone().into()
    }
    fn value(&self) -> &u32 {
        &self.index
    }
}

type SearchQueryFn = Box<dyn Fn(&str, &mut App)>;

struct PickerChoices {
    items: SearchableVec<Choice>,
    query: SearchQueryFn,
}

impl SearchableListDelegate for PickerChoices {
    type Item = Choice;
    fn items_count(&self, section: usize) -> usize {
        self.items.items_count(section)
    }
    fn item(&self, index: IndexPath) -> Option<&Choice> {
        self.items.item(index)
    }
    fn position<V>(&self, value: &V) -> Option<IndexPath>
    where
        Self::Item: SearchableListItem<Value = V>,
        V: PartialEq,
    {
        self.items.position(value)
    }
    fn perform_search(&mut self, query: &str, window: &mut Window, cx: &mut App) -> Task<()> {
        (self.query)(query, cx);
        self.items.perform_search(query, window, cx)
    }
}

struct Picker {
    state: Entity<SelectState<PickerChoices>>,
    options: Vec<String>,
    selected: Option<u32>,
    handler: u32,
    input: Option<u32>,
    reset: Option<(String, u64)>,
    _subscription: Subscription,
}

struct Field {
    state: Entity<InputState>,
    on_input: u32,
    on_submit: Option<u32>,
    value: String,
    guest_value: String,
    placeholder: String,
    secure: bool,
    ime: Option<crate::module_view::input::ImeState>,
    _observer: Subscription,
    _subscription: Subscription,
}

#[derive(Default)]
pub(crate) struct NativePresentation {
    focused_container: Option<(String, std::mem::Discriminant<wire::Node>)>,
    inputs: HashMap<String, InputPresentation>,
    editors: HashMap<String, wire::editor_document::EditorDocumentRef>,
    scrolls: HashMap<String, ScrollPresentation>,
}

struct ScrollPresentation {
    direction: wire::ScrollDirection,
    anchors: (wire::ScrollAnchor, wire::ScrollAnchor),
    offset: Point<Pixels>,
    rows: Option<Vec<String>>,
}

struct InputPresentation {
    value: String,
    secure: bool,
    selection: std::ops::Range<usize>,
    focused: bool,
}

pub struct ViewTree {
    user_activation: std::cell::Cell<Option<u32>>,
    root: wire::Node,
    // Structural nodes enter the native focus path only on an explicit Focus request.
    focus_targets: HashMap<String, (std::mem::Discriminant<wire::Node>, FocusHandle)>,
    fields: HashMap<String, Field>,
    rich_selections: HashMap<String, RichSelection>,
    scrolls: HashMap<String, ScrollHandle>,
    lists: HashMap<String, VirtualScroll>,
    scroll_positions: HashMap<String, (Point<Pixels>, Point<Pixels>)>,
    pickers: HashMap<String, Picker>,
    drags: HashMap<String, Point<Pixels>>,
    containers: HashMap<String, [f64; 2]>,
    bounds: HashMap<String, Bounds<Pixels>>,
    sensors: HashMap<String, SensorState>,
    hovered: std::collections::HashSet<String>,
    ranges: HashMap<String, RangeControl>,
    images: HashMap<u64, Arc<RenderImage>>,
    viewers: HashMap<String, ViewerState>,
    vectors: HashMap<u64, Arc<[u8]>>,
    surfaces: HashMap<String, AnyView>,
    editor_store: Option<crate::editor::wire::EditorStore>,
    editors: HashMap<String, EditorMount>,
    mounted: std::collections::HashSet<String>,
    presentation: NativePresentation,
}

impl EventEmitter<wire::Event> for ViewTree {}

impl ViewTree {
    pub(crate) fn take_user_activation(&self, event: &wire::Event) -> Option<()> {
        let wire::Event::Message(message) = event else {
            return None;
        };
        let actual_click = self.user_activation.get() == Some(*message);
        if !actual_click {
            return None;
        }
        self.user_activation.take().map(|_| ())
    }

    #[allow(clippy::too_many_arguments)]
    fn virtual_scroll(
        &mut self,
        key: &str,
        rows: Vec<VirtualRow>,
        anchor: wire::ScrollAnchor,
        follow: bool,
        handler: Option<u32>,
        width: Option<wire::Length>,
        height: Option<wire::Length>,
        background: Option<wire::Rgba>,
        border: Option<wire::Border>,
        restored: Option<ScrollPresentation>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let restored = restored
            .filter(|saved| {
                saved.rows.as_ref().is_some_and(|keys| {
                    keys.iter()
                        .map(String::as_str)
                        .eq(rows.iter().map(|row| row.key.as_str()))
                })
            })
            .map(|saved| saved.offset);
        let list = self
            .lists
            .entry(key.to_owned())
            .or_insert_with(|| VirtualScroll {
                state: ListState::new(
                    0,
                    if anchor == wire::ScrollAnchor::End {
                        ListAlignment::Bottom
                    } else {
                        ListAlignment::Top
                    },
                    px(160.),
                ),
                rows: Vec::new(),
                anchor,
                measured_width: None,
            });
        let prefix = list
            .rows
            .iter()
            .zip(&rows)
            .take_while(|(old, new)| old.key == new.key)
            .count();
        let suffix = list.rows[prefix..]
            .iter()
            .rev()
            .zip(rows[prefix..].iter().rev())
            .take_while(|(old, new)| old.key == new.key)
            .count();
        let old_end = list.rows.len() - suffix;
        let new_end = rows.len() - suffix;
        if prefix != old_end || prefix != new_end {
            if list.rows.is_empty() {
                let estimate = rows
                    .first()
                    .map_or(44., |row| row.estimated_height + row.gap);
                list.state
                    .reset_with_uniform_height(rows.len(), px(estimate));
            } else {
                list.state.splice(prefix..old_end, new_end - prefix);
            }
        }
        for (index, row) in rows.iter().enumerate() {
            let old = if index < prefix {
                list.rows.get(index)
            } else if index >= new_end {
                list.rows.get(old_end + index - new_end)
            } else {
                None
            };
            if old.is_some_and(|old| old.content != row.content || old.gap != row.gap) {
                list.state.remeasure_items(index..index + 1);
            }
        }
        list.rows = rows;
        list.anchor = anchor;
        list.state.set_follow_mode(if follow {
            FollowMode::Tail
        } else {
            FollowMode::Normal
        });
        let state = list.state.clone();
        let weak = cx.entity().downgrade();
        let route = key.to_owned();
        state.set_scroll_handler(move |_, _, cx| {
            let Some(handler) = handler else {
                return;
            };
            let weak = weak.clone();
            let route = route.clone();
            // List invokes this callback while borrowing its layout state.
            // Read pixel measurements after that borrow ends, on this turn.
            cx.defer(move |cx| {
                let _ = weak.update(cx, |this, cx| {
                    let Some(list) = this.lists.get(&route) else {
                        return;
                    };
                    let maximum = f32::from(list.state.max_offset_for_scrollbar().y);
                    let offset = f32::from(list.state.scroll_px_offset_for_scrollbar().y);
                    let y = match anchor {
                        wire::ScrollAnchor::End => maximum + offset,
                        _ => -offset,
                    };
                    cx.emit(wire::Event::ScrollOffset {
                        handler,
                        x: 0.,
                        y,
                        relative_x: 0.,
                        relative_y: y / maximum.max(1.),
                    });
                });
            });
        });
        let weak = cx.entity().downgrade();
        let route = key.to_owned();
        let native = gpui_kit::list(state, move |index, window, cx| {
            weak.update(cx, |this, cx| {
                let Some(row) = this
                    .lists
                    .get(&route)
                    .and_then(|list| list.rows.get(index))
                    .cloned()
                else {
                    return div().into_any_element();
                };
                div()
                    .relative()
                    .w_full()
                    .pb(px(row.gap))
                    .child(this.node(&row.content, window, cx))
                    .child(this.measure(&row.key, cx))
                    .into_any_element()
            })
            .unwrap_or_else(|_| div().into_any_element())
        })
        .with_sizing_behavior(if height.is_some() {
            ListSizingBehavior::Auto
        } else {
            ListSizingBehavior::Infer
        })
        .w_full()
        .h_full();
        let route = key.to_owned();
        let weak = cx.entity().downgrade();
        let retain_estimates = canvas(
            move |bounds, _, cx| {
                let _ = weak.update(cx, |this, cx| {
                    let Some(list) = this.lists.get_mut(&route) else {
                        return;
                    };
                    if list.measured_width == Some(bounds.size.width) {
                        return;
                    }
                    list.measured_width = Some(bounds.size.width);
                    // GPUI invalidates every height hint on first prepaint and
                    // width changes. Restore the guest's estimate after that pass;
                    // measured rows keep their actual height via size_hint().
                    let estimate = list
                        .rows
                        .first()
                        .map_or(44., |row| row.estimated_height + row.gap);
                    list.state.clone().with_uniform_item_height(px(estimate));
                    if let Some(offset) = restored {
                        let maximum = list.state.max_offset_for_scrollbar();
                        list.state.set_offset_from_scrollbar(point(
                            px(0.),
                            offset.y.clamp(-maximum.y, px(0.)),
                        ));
                    }
                    cx.notify();
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        // The native list owns scrolling, including off-screen measurements;
        // the wire scroll remains the identity addressed by widget commands.
        decoration(
            dimensions(div().relative().min_h_0(), width, height),
            background,
            border,
        )
        .id(key.to_owned())
        .child(native)
        .child(retain_estimates)
        .child(self.measure(key, cx))
        .into_any_element()
    }

    #[cfg(test)]
    pub(crate) fn measured_bounds(&self, key: &str) -> Option<Bounds<Pixels>> {
        self.bounds.get(key).copied()
    }

    #[cfg(test)]
    pub(crate) fn input_presentation(
        &self,
        key: &str,
        window: &Window,
        cx: &App,
    ) -> Option<(String, usize, std::ops::Range<usize>, bool)> {
        let input = self.fields.get(key)?.state.read(cx);
        Some((
            input.value().to_string(),
            input.cursor(),
            input.selected_range(),
            input.focus_handle(cx).is_focused(window),
        ))
    }

    #[cfg(test)]
    pub(crate) fn scroll_offset(&self, key: &str) -> Option<Point<Pixels>> {
        self.lists
            .get(key)
            .map(|list| list.state.scroll_px_offset_for_scrollbar())
            .or_else(|| self.scrolls.get(key).map(ScrollHandle::offset))
    }

    pub fn new(root: wire::Node) -> Self {
        Self {
            user_activation: Default::default(),
            root,
            focus_targets: HashMap::new(),
            fields: HashMap::new(),
            rich_selections: HashMap::new(),
            scrolls: HashMap::new(),
            lists: HashMap::new(),
            scroll_positions: HashMap::new(),
            pickers: HashMap::new(),
            drags: HashMap::new(),
            containers: HashMap::new(),
            bounds: HashMap::new(),
            sensors: HashMap::new(),
            hovered: Default::default(),
            ranges: HashMap::new(),
            images: HashMap::new(),
            viewers: HashMap::new(),
            vectors: HashMap::new(),
            surfaces: HashMap::new(),
            editor_store: None,
            editors: HashMap::new(),
            mounted: Default::default(),
            presentation: NativePresentation::default(),
        }
    }

    pub fn set_editor_store(
        &mut self,
        store: crate::editor::wire::EditorStore,
        cx: &mut Context<Self>,
    ) {
        self.editors.clear();
        self.editor_store = Some(store);
        cx.notify();
    }

    pub fn set_surface(&mut self, key: String, surface: AnyView, cx: &mut Context<Self>) {
        self.surfaces.insert(key, surface);
        cx.notify();
    }

    pub fn surface_requests(&self) -> Vec<(String, String, Vec<wire::SurfaceValue>, Option<u32>)> {
        let mut requests = Vec::new();
        self.root.clone().for_each_mut(&mut |node| {
            if let wire::Node::Surface {
                key,
                name,
                args,
                on_event,
            } = node
            {
                requests.push((key.clone(), name.clone(), args.clone(), *on_event));
            }
        });
        requests
    }

    pub fn execute_widget_command(
        &mut self,
        mut command: wire::WidgetCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Vec<u8>, String> {
        use wire::WidgetCommand as C;
        command.validate()?;
        match command {
            C::Focused { target } => Ok(wire::encode(&self.target_focused(&target, window, cx))),
            C::FocusPrevious => self.focus_relative(false, window, cx),
            C::FocusNext => self.focus_relative(true, window, cx),
            C::EditorAction { ref target, .. }
            | C::Focus { ref target }
            | C::CursorFront { ref target }
            | C::CursorEnd { ref target }
            | C::Cursor { ref target, .. }
            | C::SelectAll { ref target }
            | C::Select { ref target, .. } => self.input_command(target, &command, window, cx),
            C::Snap { target, x, y } => {
                self.scroll_command(&target, ScrollRequest::Relative(x, y), cx)
            }
            C::SnapEnd { target } => self.scroll_command(&target, ScrollRequest::End, cx),
            C::ScrollTo { target, x, y } => {
                self.scroll_command(&target, ScrollRequest::Absolute(x, y), cx)
            }
            C::ScrollBy { target, x, y } => {
                self.scroll_command(&target, ScrollRequest::By(x, y), cx)
            }
            C::ScrollToKey { target, key } => {
                self.scroll_command(&target, ScrollRequest::Key(key), cx)
            }
        }
    }

    fn target_focused(&self, target: &str, window: &Window, cx: &App) -> bool {
        if !self.mounted.contains(target) {
            return false;
        }
        if let Some(field) = self.fields.get(target) {
            return field.state.read(cx).focus_handle(cx).is_focused(window);
        }
        if let Some((_, handle)) = self.focus_targets.get(target) {
            return handle.is_focused(window);
        }
        if let Some(picker) = self.pickers.get(target) {
            return picker
                .state
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx);
        }
        self.editors
            .get(target)
            .is_some_and(|editor| editor.view.is_focused(window, cx))
    }

    fn focus_relative(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Vec<u8>, String> {
        let mut targets = Vec::new();
        self.root.clone().for_each_mut(&mut |node| {
            let Some(key) = node.key() else {
                return;
            };
            let available = self.mounted.contains(key)
                && (self.fields.contains_key(key)
                    || self.pickers.contains_key(key)
                    || self.focus_targets.contains_key(key)
                    || self.editors.contains_key(key));
            if available {
                targets.push(key.to_owned());
            }
        });
        if targets.is_empty() {
            return Ok(wire::encode(&()));
        }
        let current = targets
            .iter()
            .position(|key| self.target_focused(key, window, cx));
        let index = match (current, forward) {
            (Some(index), true) => (index + 1) % targets.len(),
            (Some(index), false) => (index + targets.len() - 1) % targets.len(),
            (None, true) => 0,
            (None, false) => targets.len() - 1,
        };
        let target = &targets[index];
        self.input_command(
            target,
            &wire::WidgetCommand::Focus {
                target: target.clone(),
            },
            window,
            cx,
        )
    }

    fn input_command(
        &mut self,
        target: &str,
        command: &wire::WidgetCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Vec<u8>, String> {
        use wire::WidgetCommand as C;
        if !self.mounted.contains(target) {
            return Ok(wire::encode(&()));
        }
        if matches!(command, C::Focus { .. }) {
            let mut kind = None;
            self.root.clone().for_each_mut(&mut |node| {
                if node.key() == Some(target)
                    && matches!(
                        node,
                        wire::Node::Container { .. } | wire::Node::Linear { .. }
                    )
                {
                    kind = Some(std::mem::discriminant(node));
                }
            });
            if let Some(kind) = kind {
                let (_, handle) = self
                    .focus_targets
                    .entry(target.into())
                    .or_insert_with(|| (kind, cx.focus_handle()));
                handle.focus(window, cx);
                cx.notify();
                return Ok(wire::encode(&()));
            }
        }
        if let Some(editor) = self.editors.get(target) {
            editor.view.widget_command(command, window, cx);
            return Ok(wire::encode(&()));
        }
        if matches!(command, C::EditorAction { .. }) {
            return Err("editor action target is not a mounted editor".into());
        }
        if let Some(picker) = self.pickers.get(target) {
            if matches!(command, C::Focus { .. }) {
                picker
                    .state
                    .update(cx, |picker, cx| picker.focus(window, cx));
            }
            return Ok(wire::encode(&()));
        }
        let Some(field) = self.fields.get(target) else {
            return Ok(wire::encode(&()));
        };
        field.state.update(cx, |input, cx| {
            let text = input.value();
            let byte = |index: u32| {
                unicode_segmentation::UnicodeSegmentation::grapheme_indices(text.as_ref(), true)
                    .nth(index as usize)
                    .map_or(text.len(), |(offset, _)| offset)
            };
            match command {
                C::Focus { .. } => input.focus(window, cx),
                C::CursorFront { .. } => input.set_selected_range(0..0, cx),
                C::CursorEnd { .. } => input.set_selected_range(text.len()..text.len(), cx),
                C::Cursor { position, .. } => {
                    let offset = byte(*position);
                    input.set_selected_range(offset..offset, cx);
                }
                C::SelectAll { .. } => input.select_all(window, cx),
                C::Select { start, end, .. } => {
                    input.set_selected_range(byte(*start)..byte(*end), cx)
                }
                _ => {}
            }
        });
        Ok(wire::encode(&()))
    }

    fn scroll_command(
        &mut self,
        target: &str,
        request: ScrollRequest,
        cx: &mut Context<Self>,
    ) -> Result<Vec<u8>, String> {
        if !self.mounted.contains(target) {
            return Ok(wire::encode(&()));
        }
        if let Some(list) = self.lists.get(target) {
            let maximum = list.state.max_offset_for_scrollbar().y;
            let offset = list.state.scroll_px_offset_for_scrollbar().y;
            let from_anchor = |value: f32| match list.anchor {
                wire::ScrollAnchor::End => px(value) - maximum,
                _ => -px(value),
            };
            match request {
                ScrollRequest::Key(key) => {
                    let suffix = format!("/@row:{key}");
                    if let Some(index) = list.rows.iter().position(|row| row.key.ends_with(&suffix))
                    {
                        list.state.scroll_to_reveal_item(index);
                    }
                }
                ScrollRequest::End => list.state.scroll_to_end(),
                ScrollRequest::Relative(_, y) => list
                    .state
                    .set_offset_from_scrollbar(point(px(0.), from_anchor(y * f32::from(maximum)))),
                ScrollRequest::Absolute(_, y) => list
                    .state
                    .set_offset_from_scrollbar(point(px(0.), from_anchor(y))),
                ScrollRequest::By(_, y) => {
                    let sign = if list.anchor == wire::ScrollAnchor::End {
                        1.
                    } else {
                        -1.
                    };
                    list.state
                        .set_offset_from_scrollbar(point(px(0.), offset + px(sign * y)));
                }
            }
            cx.notify();
            return Ok(wire::encode(&()));
        }
        let Some(handle) = self.scrolls.get(target) else {
            return Ok(wire::encode(&()));
        };
        let maximum = handle.max_offset();
        let mut anchors = (wire::ScrollAnchor::Start, wire::ScrollAnchor::Start);
        let mut row = None;
        self.root.clone().for_each_mut(&mut |node| {
            let wire::Node::Scroll {
                key,
                content,
                anchor_x,
                anchor_y,
                ..
            } = node
            else {
                return;
            };
            if key != target {
                return;
            }
            anchors = (*anchor_x, *anchor_y);
            if let ScrollRequest::Key(requested) = request {
                content.for_each_mut(&mut |node| {
                    let wire::Node::KeyedColumn {
                        key,
                        keys: Some(keys),
                        ..
                    } = node
                    else {
                        return;
                    };
                    if keys.iter().any(|key| key.virtual_key() == requested) {
                        row = Some(format!("{key}/@row:{requested}"));
                    }
                });
            }
        });
        let from_anchor = |distance: f32, maximum: Pixels, anchor: wire::ScrollAnchor| match anchor
        {
            wire::ScrollAnchor::End => px(distance) - maximum,
            wire::ScrollAnchor::Start | wire::ScrollAnchor::Keep => -px(distance),
        };
        let next = match request {
            ScrollRequest::Relative(x, y) => point(
                from_anchor(x * f32::from(maximum.x), maximum.x, anchors.0),
                from_anchor(y * f32::from(maximum.y), maximum.y, anchors.1),
            ),
            ScrollRequest::Absolute(x, y) => point(
                from_anchor(x, maximum.x, anchors.0),
                from_anchor(y, maximum.y, anchors.1),
            ),
            ScrollRequest::By(x, y) => {
                let direction = |delta: f32, anchor: wire::ScrollAnchor| match anchor {
                    wire::ScrollAnchor::End => px(delta),
                    _ => -px(delta),
                };
                handle.offset() + point(direction(x, anchors.0), direction(y, anchors.1))
            }
            ScrollRequest::End => -maximum,
            ScrollRequest::Key(_) => {
                let Some(bounds) = row.and_then(|row| self.bounds.get(&row)) else {
                    return Ok(wire::encode(&()));
                };
                point(
                    handle.offset().x,
                    handle.offset().y - (bounds.origin.y - handle.bounds().origin.y),
                )
            }
        };
        handle.set_offset(point(
            next.x.clamp(-maximum.x, px(0.0)),
            next.y.clamp(-maximum.y, px(0.0)),
        ));
        cx.notify();
        Ok(wire::encode(&()))
    }

    pub fn replace(&mut self, mut root: wire::Node, cx: &mut Context<Self>) {
        let mut focusable = HashMap::new();
        let mut inputs = std::collections::HashSet::new();
        let mut scrolls = std::collections::HashSet::new();
        let mut pickers = std::collections::HashSet::new();
        let mut drags = std::collections::HashSet::new();
        let mut containers = std::collections::HashSet::new();
        let mut retained = std::collections::HashSet::new();
        let mut sensors = std::collections::HashSet::new();
        let mut live_keys = std::collections::HashSet::new();
        root.for_each_mut(&mut |node| {
            if let Some(key) = node.key() {
                live_keys.insert(key.to_owned());
                if matches!(
                    node,
                    wire::Node::Container { .. } | wire::Node::Linear { .. }
                ) {
                    focusable.insert(key.to_owned(), std::mem::discriminant(node));
                }
            }
            match node {
                wire::Node::Input { key, .. } => {
                    inputs.insert(key.clone());
                }
                wire::Node::Scroll { key, .. } => {
                    scrolls.insert(key.clone());
                }
                wire::Node::PickList { key, .. } | wire::Node::ComboBox { key, .. } => {
                    pickers.insert(key.clone());
                }
                wire::Node::ResizeHandle { key, .. } => {
                    drags.insert(key.clone());
                }
                wire::Node::Responsive { key, .. } => {
                    containers.insert(key.clone());
                }
                wire::Node::KeyedColumn {
                    key,
                    keys: Some(keys),
                    ..
                } => {
                    for identity in keys {
                        live_keys.insert(format!("{key}/@row:{}", identity.virtual_key()));
                    }
                }
                wire::Node::Sensor {
                    key,
                    reset,
                    on_show,
                    on_resize,
                    on_hide,
                    ..
                } => {
                    sensors.insert(key.clone());
                    if let Some(sensor) = self.sensors.get_mut(key) {
                        if sensor.reset != *reset {
                            sensor.reset = reset.clone();
                            sensor.size = None;
                            sensor.pending = None;
                        }
                        // Delayed measurements resolve these current-frame
                        // routes even before the next native draw runs.
                        sensor.on_show = *on_show;
                        sensor.on_resize = *on_resize;
                        sensor.on_hide = *on_hide;
                    }
                }
                wire::Node::Slider { key, .. }
                | wire::Node::Hover { key, .. }
                | wire::Node::Surface { key, .. }
                | wire::Node::Editor { key, .. } => {
                    retained.insert(key.clone());
                }
                wire::Node::Image {
                    hash,
                    data: Some(data),
                    ..
                } => {
                    self.remember_image(*hash, data);
                }
                wire::Node::ImageViewer {
                    key, hash, data, ..
                } => {
                    retained.insert(key.clone());
                    if let Some(data) = data {
                        self.remember_image(*hash, data);
                    }
                }
                wire::Node::Svg {
                    hash,
                    bytes: Some(bytes),
                    ..
                } => {
                    self.remember_vector(*hash, bytes);
                }
                _ => {}
            }
        });
        self.bounds.retain(|key, _| live_keys.contains(key));
        self.focus_targets
            .retain(|key, (kind, _)| focusable.get(key) == Some(kind));
        self.fields.retain(|key, _| inputs.contains(key));
        self.rich_selections
            .retain(|key, _| live_keys.contains(key));
        self.scrolls.retain(|key, _| scrolls.contains(key));
        self.lists.retain(|key, _| scrolls.contains(key));
        self.scroll_positions.retain(|key, _| scrolls.contains(key));
        self.pickers.retain(|key, _| pickers.contains(key));
        self.drags.retain(|key, _| drags.contains(key));
        self.containers.retain(|key, _| containers.contains(key));
        self.ranges.retain(|key, _| retained.contains(key));
        self.editors.retain(|key, _| retained.contains(key));
        self.viewers.retain(|key, _| retained.contains(key));
        self.hovered.retain(|key| retained.contains(key));
        self.surfaces.retain(|key, _| retained.contains(key));
        // on_hide observes viewport exit while mounted, not destruction.
        // Removed nodes own old-frame IDs; emitting one now could activate
        // an unrelated route in the replacement frame's handler table.
        self.sensors.retain(|key, _| sensors.contains(key));
        self.root = root;
        cx.notify();
    }

    /// Copied presentation only: no native entity, callback, handler id, or IME
    /// preedit crosses a guest generation. Document selection remains guest-owned.
    pub(crate) fn presentation(&self, window: &Window, cx: &App) -> NativePresentation {
        let inputs = self
            .fields
            .iter()
            .map(|(key, field)| {
                let input = field.state.read(cx);
                let range = input.selected_range();
                let selection = if input.cursor() == range.start {
                    range.end..range.start
                } else {
                    range
                };
                (
                    key.clone(),
                    InputPresentation {
                        value: input.value().to_string(),
                        secure: field.secure,
                        selection,
                        focused: input.focus_handle(cx).is_focused(window),
                    },
                )
            })
            .collect();
        let mut editors = HashMap::new();
        let mut scrolls = HashMap::new();
        self.root.clone().for_each_mut(&mut |node| {
            if let wire::Node::Scroll {
                key,
                direction,
                anchor_x,
                anchor_y,
                ..
            } = node
            {
                let offset = self
                    .lists
                    .get(key)
                    .map(|list| list.state.scroll_px_offset_for_scrollbar())
                    .or_else(|| self.scrolls.get(key).map(ScrollHandle::offset));
                if let Some(offset) = offset {
                    scrolls.insert(
                        key.clone(),
                        ScrollPresentation {
                            direction: *direction,
                            anchors: (*anchor_x, *anchor_y),
                            offset,
                            rows: self
                                .lists
                                .get(key)
                                .map(|list| list.rows.iter().map(|row| row.key.clone()).collect()),
                        },
                    );
                }
            }
            if let wire::Node::Editor { key, document, .. } = node {
                let focused = self
                    .editors
                    .get(key)
                    .is_some_and(|editor| editor.view.is_focused(window, cx));
                if focused {
                    editors.insert(key.clone(), document.clone());
                }
            }
        });
        NativePresentation {
            focused_container: self.focus_targets.iter().find_map(|(key, (kind, handle))| {
                handle.is_focused(window).then(|| (key.clone(), *kind))
            }),
            inputs,
            editors,
            scrolls,
        }
    }

    pub(crate) fn with_presentation(mut self, presentation: NativePresentation) -> Self {
        self.presentation = presentation;
        self
    }

    fn focusable_container(
        &mut self,
        node: &wire::Node,
        element: Div,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = node.key().expect("keyed container");
        let kind = std::mem::discriminant(node);
        let restore = self
            .presentation
            .focused_container
            .as_ref()
            .is_some_and(|(saved, saved_kind)| saved == key && *saved_kind == kind);
        if restore {
            self.presentation.focused_container = None;
            let (_, handle) = self
                .focus_targets
                .entry(key.into())
                .or_insert_with(|| (kind, cx.focus_handle()));
            handle.focus(window, cx);
        }
        match self.focus_targets.get(key) {
            Some((_, handle)) => element
                .id(SharedString::from(key.to_owned()))
                .track_focus(handle)
                .into_any_element(),
            None => element.into_any_element(),
        }
    }

    fn input(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Input {
            key,
            value,
            placeholder,
            secure,
            on_input,
            on_submit,
            options,
            width,
            style,
            ..
        } = node
        else {
            unreachable!()
        };
        if !self.fields.contains_key(key) {
            let presentation = self
                .presentation
                .inputs
                .remove(key)
                .filter(|saved| saved.value == *value && saved.secure == *secure);
            let state = cx.new(|cx| {
                let mut state = InputState::new(window, cx)
                    .placeholder(placeholder.clone())
                    .masked(*secure);
                state.set_value(value.clone(), window, cx);
                if let Some(saved) = presentation {
                    state.set_selected_range(saved.selection, cx);
                    if saved.focused {
                        state.focus(window, cx);
                    }
                }
                state
            });
            let input_key = key.clone();
            let observed_key = key.clone();
            let observer = cx.observe_in(&state, window, move |this, input, window, cx| {
                let Some(field) = this.fields.get_mut(&observed_key) else {
                    return;
                };
                let (text, marked, cursor, selection) = input.update(cx, |input, cx| {
                    let marked = input.marked_text_range(window, cx);
                    (
                        input.value().to_string(),
                        marked,
                        input.cursor(),
                        input.selected_range(),
                    )
                });
                for event in crate::module_view::input::ime_events(
                    &mut field.ime,
                    &text,
                    marked,
                    cursor,
                    selection,
                ) {
                    cx.emit(event);
                }
            });
            let subscription = cx.subscribe_in(&state, window, move |this, input, event, _, cx| {
                let Some(field) = this.fields.get_mut(&input_key) else {
                    return;
                };
                match event {
                    InputEvent::Change => {
                        let text = input.read(cx).value().to_string();
                        if text == field.value {
                            return;
                        }
                        field.value = text.clone();
                        cx.emit(wire::Event::Input {
                            handler: field.on_input,
                            text,
                        });
                    }
                    InputEvent::PressEnter { .. } => {
                        if let Some(message) = field.on_submit {
                            cx.emit(wire::Event::Message(message));
                        }
                    }
                    InputEvent::Focus | InputEvent::Blur => {}
                }
            });
            self.fields.insert(
                key.clone(),
                Field {
                    state,
                    on_input: *on_input,
                    on_submit: *on_submit,
                    value: value.clone(),
                    guest_value: value.clone(),
                    placeholder: placeholder.clone(),
                    secure: *secure,
                    ime: None,
                    _observer: observer,
                    _subscription: subscription,
                },
            );
        }
        let field = self.fields.get_mut(key).expect("field inserted");
        field.on_input = *on_input;
        field.on_submit = *on_submit;
        if field.guest_value != *value {
            field.guest_value = value.clone();
            if field.value != *value {
                field.value = value.clone();
                field
                    .state
                    .update(cx, |state, cx| state.set_value(value.clone(), window, cx));
            }
        }
        if field.placeholder != *placeholder {
            field.placeholder = placeholder.clone();
            field.state.update(cx, |state, cx| {
                state.set_placeholder(placeholder.clone(), window, cx)
            });
        }
        if field.secure != *secure {
            field.secure = *secure;
            field
                .state
                .update(cx, |state, cx| state.set_masked(*secure, window, cx));
        }
        let input = Input::new(&field.state)
            .id(key.clone())
            .aria_label(options.label.clone())
            .disabled(options.disabled);
        let face = match options.disabled {
            true => style.disabled.unwrap_or(style.active),
            false => style.active,
        };
        let mut input = decoration(
            pad(dimensions(input, *width, None), options.padding),
            style.utility.background.or(face.background),
            style.utility.border.or(face.border),
        );
        if let Some(color) = style.utility.value.or(face.value) {
            input = input.text_color(rgba(color));
        }
        if let Some(size) = options.text_size {
            input = input.text_size(px(size));
        }
        if let Some(height) = options.line_height {
            input = input.line_height(relative(height));
        }
        if let Some(font) = &options.font {
            input = input.font_weight(font_weight(font.weight));
        }
        input.into_any_element()
    }

    fn node(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(key) = node.key() {
            self.mounted.insert(key.to_owned());
        }
        // One arm, one method: this dispatcher is on the recursion chain for
        // every nesting level the wire allows (`wire::MAX_DEPTH`), and an
        // unoptimised build gives a function the stack of ALL its arms at
        // once — inlined bodies here once cost 570 KiB a level and overflowed
        // the main thread at a depth of fourteen.
        use wire::Node;
        match node {
            Node::Text { .. } => self.text(node, cx),
            Node::Space { width, height } => dimensions(div(), *width, *height).into_any_element(),
            Node::Linear { .. } => self.linear(node, window, cx),
            Node::KeyedColumn { .. } => self.keyed_column(node, window, cx),
            Node::Container { .. } => self.container(node, window, cx),
            Node::Scroll { .. } => self.scroll(node, window, cx),
            Node::Button { .. } => self.button(node, window, cx),
            Node::Input { .. } => self.input(node, window, cx),
            Node::PickList { .. } | Node::ComboBox { .. } => self.picker(node, window, cx),
            Node::Toggle { .. } => self.toggle(node, cx),
            Node::Radio {
                key,
                label,
                selected,
                on_select,
                ..
            } => {
                let message = *on_select;
                Radio::new(key.clone())
                    .label(label.clone())
                    .checked(*selected)
                    .on_click(
                        cx.listener(move |_, _, _, cx| cx.emit(wire::Event::Message(message))),
                    )
                    .into_any_element()
            }
            Node::Rule {
                axis,
                thickness,
                color,
                ..
            } => {
                let element = div().bg(color.map(rgba).unwrap_or_else(|| {
                    gpui_kit::component::Theme::global(cx).color_tokens().border
                }));
                match axis {
                    wire::Axis::Column => element.w(px(*thickness)).h_full().into_any_element(),
                    wire::Axis::Row => element.h(px(*thickness)).w_full().into_any_element(),
                }
            }
            Node::Lazy { content, .. } => self.node(content, window, cx),
            Node::ResizeHandle { .. } => self.resize_handle(node, window, cx),
            Node::Responsive { .. } => self.responsive(node, window, cx),
            Node::When { .. } => self.when(node, window, cx),
            Node::Sensor { .. } => self.sensor(node, window, cx),
            Node::MouseArea { .. } => self.mouse_area(node, window, cx),
            Node::Slider { .. } => self.slider(node, window, cx),
            Node::RichText { .. } => self.rich_text(node, window, cx),
            Node::Flex { .. } => self.flex(node, window, cx),
            Node::Grid { .. } => self.grid(node, window, cx),
            Node::Hover { .. } => self.hover(node, window, cx),
            Node::Tooltip { .. } => self.tooltip(node, window, cx),
            Node::Float { .. } => self.float(node, window, cx),
            Node::Image { .. } => self.picture(node, window),
            Node::ImageViewer { .. } => self.image_viewer(node, window, cx),
            Node::Svg { .. } => self.vector(node, window),
            Node::Canvas { .. } => self.drawing(node, cx),
            Node::Qr { code, .. } => qr(code),
            Node::Surface { key, name, .. } => match self.surfaces.get(key) {
                Some(surface) => surface.clone().into_any_element(),
                None => div()
                    .child(format!("Unavailable host surface: {name}"))
                    .into_any_element(),
            },
            Node::Stack { .. } => self.stack(node, window, cx),
            Node::Overlay { .. } => self.overlay(node, window, cx),
            Node::Progress { .. } => self.progress(node, cx),
            Node::Pin { .. } => self.pin(node, window, cx),
            Node::Editor { .. } => self.editor(node, window, cx),
        }
    }

    fn text(&mut self, node: &wire::Node, cx: &mut Context<Self>) -> AnyElement {
        let wire::Node::Text {
            key,
            content,
            size,
            color,
            width,
            font,
            align_x,
            options,
            ..
        } = node
        else {
            unreachable!()
        };
        let mut element = text_options(
            dimensions(div().min_w_0().max_w_full(), *width, options.height),
            *font,
            *align_x,
            options,
        )
        .child(content.clone());
        let intrinsic_label = options.wrapping == Some(wire::Wrapping::None)
            && matches!(width, None | Some(wire::Length::Shrink));
        if intrinsic_label {
            // Shrink-sized labels keep their natural width; a Fill
            // sibling takes the remaining space, not their letters.
            element = element.flex_shrink_0();
        }
        if let Some(size) = size {
            element = element.text_size(px(*size));
        }
        if let Some(color) = color {
            element = element.text_color(rgba(*color));
        }
        #[cfg(test)]
        {
            element = element.relative().child(self.measure(key, cx));
        }
        #[cfg(not(test))]
        let _ = (key, cx);
        element.into_any_element()
    }

    fn linear(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Linear {
            axis,
            spacing,
            padding,
            width,
            height,
            background,
            border,
            children,
            align,
            max_width,
            clip,
            wrap,
            ..
        } = node
        else {
            unreachable!()
        };
        let mut element = dimensions(div().flex(), *width, *height);
        element = match axis {
            wire::Axis::Column => element.flex_col(),
            wire::Axis::Row => element.flex_row(),
        };
        if let Some(gap) = spacing {
            element = element.gap(px(*gap));
        }
        if let Some(width) = max_width {
            element = element.max_w(px(*width));
        }
        if *clip {
            element = element.overflow_hidden();
        }
        if wrap.is_some() {
            element = element.flex_wrap();
        }
        element = cross_align(element, *align);
        element = decoration(pad(element, *padding), *background, *border);
        for child in children {
            element = element.child(self.node(child, window, cx));
        }
        self.focusable_container(node, element, window, cx)
    }

    fn keyed_column(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::KeyedColumn {
            key,
            keys,
            spacing,
            padding,
            width,
            height,
            background,
            border,
            children,
            align,
            max_width,
            ..
        } = node
        else {
            unreachable!()
        };
        let mut element = decoration(
            pad(
                dimensions(div().flex().flex_col(), *width, *height),
                *padding,
            ),
            *background,
            *border,
        );
        if let Some(gap) = spacing {
            element = element.gap(px(*gap));
        }
        element = cross_align(element, *align);
        if let Some(width) = max_width {
            element = element.max_w(px(*width));
        }
        for (index, child) in children.iter().enumerate() {
            let content = self.node(child, window, cx);
            let identity = keys.as_ref().and_then(|keys| keys.get(index));
            element = match identity {
                Some(identity) => {
                    let row = format!("{key}/@row:{}", identity.virtual_key());
                    element.child(
                        div()
                            .relative()
                            .child(content)
                            .child(self.measure(&row, cx)),
                    )
                }
                None => element.child(content),
            };
        }
        element.into_any_element()
    }

    fn container(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Container {
            key,
            content,
            width,
            height,
            padding,
            border,
            background,
            max_width,
            max_height,
            clip,
            align_x,
            align_y,
            shadow,
            ..
        } = node
        else {
            unreachable!()
        };
        let color = match background {
            Some(wire::Background::Color(color)) => Some(*color),
            _ => None,
        };
        let mut element = shadows(
            decoration(
                pad(
                    dimensions(div().relative().flex(), *width, *height),
                    *padding,
                ),
                color,
                *border,
            ),
            *shadow,
        );
        element = horizontal_align(element, *align_x);
        element = vertical_align(element, *align_y);
        if let Some(width) = max_width {
            element = element.max_w(px(*width));
        }
        if let Some(height) = max_height {
            element = element.max_h(px(*height));
        }
        if *clip {
            element = element.overflow_hidden();
        }
        let element = element
            .child(self.node(content, window, cx))
            .child(self.measure(key, cx));
        self.focusable_container(node, element, window, cx)
    }

    fn scroll(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Scroll {
            key,
            content,
            width,
            height,
            direction,
            anchor_x,
            anchor_y,
            auto_scroll,
            on_scroll,
            background,
            border,
            bar_hidden,
            ..
        } = node
        else {
            unreachable!()
        };
        let restored = self.presentation.scrolls.remove(key).filter(|saved| {
            saved.direction == *direction && saved.anchors == (*anchor_x, *anchor_y)
        });
        if *direction == wire::ScrollDirection::Vertical
            && let Some(rows) = virtual_rows(content)
        {
            return self.virtual_scroll(
                key,
                rows,
                *anchor_y,
                *auto_scroll,
                *on_scroll,
                *width,
                *height,
                *background,
                *border,
                restored,
                cx,
            );
        }
        let handle = self.scrolls.entry(key.clone()).or_default().clone();
        let element = decoration(
            dimensions(div().relative(), *width, *height),
            *background,
            *border,
        )
        .id(key.clone())
        .track_scroll(&handle);
        let element = match direction {
            wire::ScrollDirection::Vertical => element.overflow_y_scroll(),
            wire::ScrollDirection::Horizontal => element.overflow_x_scroll(),
            wire::ScrollDirection::Both => element.overflow_scroll(),
        };
        let route = key.clone();
        let anchors = (*anchor_x, *anchor_y);
        let follow = *auto_scroll;
        let handler = *on_scroll;
        let weak = cx.entity().downgrade();
        let observe = canvas(
            |_, _, _| (),
            move |_, _, _, cx| {
                let maximum = handle.max_offset();
                let offset = handle.offset();
                let _ = weak.update(cx, |this, cx| {
                    let previous = this.scroll_positions.get(&route).copied();
                    let restored = restored
                        .as_ref()
                        .filter(|saved| previous.is_none() && saved.rows.is_none());
                    let mut next = offset;
                    for (position, maximum, previous, anchor) in [
                        (
                            &mut next.x,
                            maximum.x,
                            previous.map(|(offset, max)| (offset.x, max.x)),
                            anchors.0,
                        ),
                        (
                            &mut next.y,
                            maximum.y,
                            previous.map(|(offset, max)| (offset.y, max.y)),
                            anchors.1,
                        ),
                    ] {
                        let at_end = previous
                            .is_some_and(|(offset, max)| f32::from(offset + max).abs() < 2.0);
                        let initialize_end =
                            previous.is_none() && anchor == wire::ScrollAnchor::End;
                        if initialize_end || (follow && at_end) {
                            *position = -maximum;
                        } else if anchor == wire::ScrollAnchor::Keep
                            && let Some((offset, old_maximum)) = previous
                            && offset < px(0.0)
                        {
                            *position =
                                (*position - (maximum - old_maximum)).clamp(-maximum, px(0.0));
                        }
                    }
                    if let Some(saved) = restored {
                        next = point(
                            saved.offset.x.clamp(-maximum.x, px(0.)),
                            saved.offset.y.clamp(-maximum.y, px(0.)),
                        );
                    }
                    if next != offset {
                        handle.set_offset(next);
                        cx.notify();
                    }
                    let changed =
                        previous.is_none_or(|(offset, max)| offset != next || max != maximum);
                    this.scroll_positions.insert(route.clone(), (next, maximum));
                    if changed && let Some(handler) = handler {
                        let distance =
                            |offset: Pixels, maximum: Pixels, anchor: wire::ScrollAnchor| {
                                match anchor {
                                    wire::ScrollAnchor::End => f32::from(maximum + offset),
                                    _ => -f32::from(offset),
                                }
                            };
                        let x = distance(next.x, maximum.x, anchors.0);
                        let y = distance(next.y, maximum.y, anchors.1);
                        let relative_x = x / f32::from(maximum.x).max(1.0);
                        let relative_y = y / f32::from(maximum.y).max(1.0);
                        cx.emit(wire::Event::ScrollOffset {
                            handler,
                            x,
                            y,
                            relative_x,
                            relative_y,
                        });
                    }
                });
            },
        )
        .absolute()
        .inset_0();
        let content = element.child(self.node(content, window, cx)).child(observe);
        let handle = self.scrolls[key].clone();
        let scrollbar = match direction {
            wire::ScrollDirection::Vertical => None,
            wire::ScrollDirection::Horizontal => Some(Scrollbar::horizontal(&handle)),
            wire::ScrollDirection::Both => Some(Scrollbar::new(&handle)),
        }
        .filter(|_| !bar_hidden);
        let Some(scrollbar) = scrollbar else {
            return content.into_any_element();
        };
        dimensions(div().relative(), *width, *height)
            .child(content)
            .child(
                div().absolute().inset_0().child(
                    scrollbar
                        .id(format!("{key}/scrollbar"))
                        .viewport_from_layout()
                        .mode(ScrollbarMode::Always),
                ),
            )
            .into_any_element()
    }

    fn button(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Button {
            key,
            content,
            label,
            checked,
            on_press,
            width,
            height,
            padding,
            style,
            ..
        } = node
        else {
            unreachable!()
        };
        let mut button = button_style(Button::new(key.clone()), style.preset)
            .disabled(on_press.is_none())
            .selected(checked.unwrap_or(false));
        button = match content {
            wire::ButtonContent::Label(text) => button.label(text.clone()),
            wire::ButtonContent::Child(child) => {
                let fit_content = matches!(height, None | Some(wire::Length::Shrink));
                if fit_content {
                    button = button.h_auto();
                }
                button.child(self.node(child, window, cx))
            }
        };
        if let Some(label) = label {
            button = button.accessibility_label(label.clone());
        }
        if let Some(message) = on_press {
            let message = *message;
            button = button.on_click(cx.listener(move |this, _, _, cx| {
                this.user_activation.set(Some(message));
                cx.emit(wire::Event::Message(message));
                cx.stop_propagation();
            }));
        }
        dimensions(pad(button, *padding), *width, *height).into_any_element()
    }

    fn toggle(&mut self, node: &wire::Node, cx: &mut Context<Self>) -> AnyElement {
        let wire::Node::Toggle {
            key,
            kind,
            label,
            checked,
            on_toggle,
            ..
        } = node
        else {
            unreachable!()
        };
        if *kind == wire::ToggleKind::Switch {
            let mut toggle = gpui_kit::component::switch::Switch::new(key.clone())
                .label(label.clone())
                .checked(*checked)
                .disabled(on_toggle.is_none());
            if let Some(handler) = on_toggle {
                let handler = *handler;
                toggle = toggle.on_click(cx.listener(move |_, on, _, cx| {
                    cx.emit(wire::Event::Toggle { handler, on: *on })
                }));
            }
            return toggle.into_any_element();
        }
        let mut checkbox = Checkbox::new(key.clone())
            .label(label.clone())
            .checked(*checked)
            .disabled(on_toggle.is_none());
        if let Some(handler) = on_toggle {
            let handler = *handler;
            checkbox =
                checkbox.on_click(cx.listener(move |_, on, _, cx| {
                    cx.emit(wire::Event::Toggle { handler, on: *on })
                }));
        }
        checkbox.into_any_element()
    }

    fn resize_handle(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::ResizeHandle {
            key,
            on_press,
            on_release,
            on_drag,
            content,
            cursor,
        } = node
        else {
            unreachable!()
        };
        let press_key = key.clone();
        let move_key = key.clone();
        let release_key = key.clone();
        let press = *on_press;
        let release = *on_release;
        let drag = *on_drag;
        let view = cx.entity().downgrade();
        let capture = canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                let moving = view.clone();
                let move_key = move_key.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase != gpui_kit::DispatchPhase::Capture {
                        return;
                    }
                    let _ = moving.update(cx, |this, cx| {
                        let Some(previous) = this.drags.get_mut(&move_key) else {
                            return;
                        };
                        if event.pressed_button != Some(MouseButton::Left) {
                            this.drags.remove(&move_key);
                            return;
                        }
                        let delta = event.position - *previous;
                        *previous = event.position;
                        if let Some(handler) = drag {
                            cx.emit(wire::Event::Drag {
                                handler,
                                dx: f32::from(delta.x) as f64,
                                dy: f32::from(delta.y) as f64,
                            });
                        }
                    });
                });
                let releasing = view.clone();
                let release_key = release_key.clone();
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if phase != gpui_kit::DispatchPhase::Capture
                        || event.button != MouseButton::Left
                    {
                        return;
                    }
                    let _ = releasing.update(cx, |this, cx| {
                        let was_dragging = this.drags.remove(&release_key).is_some();
                        if was_dragging && let Some(message) = release {
                            cx.emit(wire::Event::Message(message));
                        }
                    });
                });
            },
        )
        .absolute()
        .inset_0();
        let (width, height) = content_dimensions(content);
        let element = dimensions(div(), width, height)
            .id(key.clone())
            .relative()
            .cursor(native_cursor(*cursor))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.drags.insert(press_key.clone(), event.position);
                    if let Some(message) = press {
                        cx.emit(wire::Event::Message(message));
                    }
                }),
            )
            .child(self.node(content, window, cx))
            .child(capture);
        #[cfg(test)]
        let element = {
            use gpui_kit::test::TestSupportExt as _;
            element.test_support()
        };
        element.into_any_element()
    }

    fn responsive(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Responsive {
            key,
            content,
            width,
            height,
        } = node
        else {
            unreachable!()
        };
        let weak = cx.entity().downgrade();
        let key = key.clone();
        let measure = canvas(
            move |bounds, _, cx| {
                let size = [
                    f32::from(bounds.size.width) as f64,
                    f32::from(bounds.size.height) as f64,
                ];
                let _ = weak.update(cx, |this, cx| {
                    let changed = this.containers.get(&key) != Some(&size);
                    if changed {
                        this.containers.insert(key, size);
                        cx.notify();
                    }
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        dimensions(div().relative(), *width, *height)
            .child(self.node(content, window, cx))
            .child(measure)
            .into_any_element()
    }

    fn when(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::When {
            condition,
            children,
            ..
        } = node
        else {
            unreachable!()
        };
        let mut element = div().flex().flex_col();
        if condition.matches(&self.containers) {
            for child in children {
                element = element.child(self.node(child, window, cx));
            }
        }
        element.into_any_element()
    }

    fn sensor(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Sensor {
            key,
            reset,
            on_show,
            on_resize,
            on_hide,
            anticipate,
            delay,
            child,
            ..
        } = node
        else {
            unreachable!()
        };
        let sensor = self.sensors.entry(key.clone()).or_insert(SensorState {
            reset: reset.clone(),
            size: None,
            on_hide: *on_hide,
            on_show: *on_show,
            on_resize: *on_resize,
            pending: None,
        });
        if sensor.reset != *reset {
            sensor.reset = reset.clone();
            sensor.size = None;
            sensor.pending = None;
        }
        sensor.on_hide = *on_hide;
        sensor.on_show = *on_show;
        sensor.on_resize = *on_resize;
        let route = key.clone();
        let show = *on_show;
        let resize = *on_resize;
        let anticipate = px(anticipate.unwrap_or_default());
        let delay = std::time::Duration::from_secs_f32(delay.unwrap_or_default().max(0.0) / 1000.0);
        let weak = cx.entity().downgrade();
        let measure = canvas(
            move |bounds, window, cx| {
                let viewport = window.content_mask().bounds;
                let visible_bounds = Bounds::new(
                    bounds.origin - point(anticipate, anticipate),
                    bounds.size + size(anticipate * 2.0, anticipate * 2.0),
                );
                let visible = viewport.intersects(&visible_bounds);
                let _ = weak.update(cx, |this, cx| {
                    this.bounds.insert(route.clone(), bounds);
                    let Some(sensor) = this.sensors.get_mut(&route) else {
                        return;
                    };
                    if !visible {
                        sensor.pending = None;
                        if sensor.size.take().is_some()
                            && let Some(message) = sensor.on_hide
                        {
                            cx.emit(wire::Event::Message(message));
                        }
                        return;
                    }
                    let unchanged = sensor.size == Some(bounds.size);
                    if unchanged {
                        sensor.pending = None;
                        return;
                    }
                    if !delay.is_zero() {
                        let waiting = sensor
                            .pending
                            .as_ref()
                            .is_some_and(|(size, _)| *size == bounds.size);
                        if waiting {
                            return;
                        }
                        let route = route.clone();
                        let size = bounds.size;
                        let timer = cx.background_executor().timer(delay);
                        let pending = cx.spawn(async move |this, cx| {
                            timer.await;
                            let _ = this.update(cx, |this, cx| {
                                let Some(sensor) = this.sensors.get_mut(&route) else {
                                    return;
                                };
                                let current = sensor
                                    .pending
                                    .as_ref()
                                    .is_some_and(|(pending, _)| *pending == size);
                                if !current {
                                    return;
                                }
                                let handler = match sensor.size {
                                    None => sensor.on_show,
                                    Some(_) => sensor.on_resize,
                                };
                                sensor.size = Some(size);
                                sensor.pending = None;
                                if let Some(handler) = handler {
                                    cx.emit(wire::Event::Size {
                                        handler,
                                        width: f32::from(size.width),
                                        height: f32::from(size.height),
                                    });
                                }
                            });
                        });
                        sensor.pending = Some((size, pending));
                        return;
                    }
                    let handler = match sensor.size {
                        None => show,
                        Some(previous) if previous != bounds.size => resize,
                        Some(_) => None,
                    };
                    sensor.size = Some(bounds.size);
                    if let Some(handler) = handler {
                        cx.emit(wire::Event::Size {
                            handler,
                            width: f32::from(bounds.size.width),
                            height: f32::from(bounds.size.height),
                        });
                    }
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        // A sensor is layout-transparent. In particular, a fill spacer
        // must not collapse inside an auto-sized measurement wrapper.
        let (width, height) = content_dimensions(child);
        dimensions(div().relative(), width, height)
            .child(self.node(child, window, cx))
            .child(measure)
            .into_any_element()
    }

    fn grid(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Grid {
            key,
            children,
            width,
            height,
            padding,
            spacing,
            columns,
            fluid,
            aspect,
            background,
            border,
        } = node
        else {
            unreachable!()
        };
        let available = self
            .bounds
            .get(key)
            .map_or(f32::from(window.viewport_size().width), |bounds| {
                f32::from(bounds.size.width)
            });
        let columns = fluid
            .filter(|value| *value > 0.0)
            .map_or(columns.unwrap_or(1), |value| {
                (available / value).ceil().max(1.0) as u32
            })
            .max(1);
        let mut grid = decoration(
            pad(
                dimensions(div().flex().flex_wrap(), *width, *height),
                *padding,
            ),
            *background,
            *border,
        );
        let gap = spacing.unwrap_or_default();
        grid = grid.gap(px(gap));
        let cell_width =
            ((available - gap * columns.saturating_sub(1) as f32) / columns as f32).max(0.0);
        for child in children {
            let cell = div()
                .w(px(cell_width))
                .h(px(cell_width / aspect.unwrap_or(1.0).max(0.001)));
            grid = grid.child(cell.child(self.node(child, window, cx)));
        }
        grid.relative()
            .child(self.measure(key, cx))
            .into_any_element()
    }

    fn hover(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Hover {
            key,
            children,
            open,
            width,
            height,
            padding,
            background,
            border,
            tint,
            radius,
        } = node
        else {
            unreachable!()
        };
        let route = key.clone();
        let mut element = decoration(
            pad(dimensions(div().relative(), *width, *height), *padding),
            *background,
            *border,
        )
        .id(key.clone());
        /// How far above the row's top edge the float's box starts: half a
        /// message bar, so the bar straddles the edge.
        const HOVER_FLOAT_LIFT: f32 = 14.;
        let reveal = *open || self.hovered.contains(key);
        // The tint is the row's own ground while the pointer is on it — it
        // sits UNDER the content, never over the text.
        if reveal && let Some(color) = tint {
            element = element.bg(rgba(*color)).rounded(px(*radius));
        }
        if let Some(base) = children.first() {
            element = element.child(self.node(base, window, cx));
        }
        if reveal && let Some(child) = children.get(1) {
            // The float straddles the row's top edge, half above it, the way
            // a message bar does; nothing here clips, so it draws over the
            // row above.
            let layer = div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(-HOVER_FLOAT_LIFT))
                .bottom_0();
            element = element.child(layer.child(self.node(child, window, cx)));
        }
        let element = element.on_hover(cx.listener(move |this, hovered, _, cx| {
            match hovered {
                true => {
                    this.hovered.insert(route.clone());
                }
                false => {
                    this.hovered.remove(&route);
                }
            }
            cx.notify();
        }));
        #[cfg(test)]
        let element = {
            use gpui_kit::test::TestSupportExt as _;
            element.test_support()
        };
        element.into_any_element()
    }

    fn tooltip(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Tooltip {
            key,
            children,
            delay_ms,
            ..
        } = node
        else {
            unreachable!()
        };
        let Some(content) = children.first() else {
            return div().into_any_element();
        };
        let mut element = div()
            .id(key.clone())
            .tooltip_show_delay(std::time::Duration::from_millis(*delay_ms))
            .child(self.node(content, window, cx));
        if let Some(tip) = children.get(1) {
            let tip = tip.clone();
            element = element.tooltip(move |_, cx| cx.new(|_| ViewTree::new(tip.clone())).into());
        }
        element.into_any_element()
    }

    fn float(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Float {
            key,
            content,
            x,
            y,
            scale: _,
            shadow,
            radius,
        } = node
        else {
            unreachable!()
        };
        let bounds = self.bounds.get(key).copied().unwrap_or_default();
        let viewport = window.viewport_size();
        let geometry = [
            f32::from(bounds.origin.x) as f64,
            f32::from(bounds.origin.y) as f64,
            f32::from(bounds.size.width) as f64,
            f32::from(bounds.size.height) as f64,
            0.0,
            0.0,
            f32::from(viewport.width) as f64,
            f32::from(viewport.height) as f64,
        ];
        let mut element = shadows(
            div()
                .relative()
                .bg(gpui_kit::component::Theme::global(cx)
                    .color_tokens()
                    .surface)
                .text_color(
                    gpui_kit::component::Theme::global(cx)
                        .color_tokens()
                        .surface_foreground,
                )
                .left(px(x.evaluate(geometry)))
                .top(px(y.evaluate(geometry))),
            *shadow,
        );
        if let Some(radius) = radius {
            element = decoration(
                element,
                None,
                Some(wire::Border {
                    color: None,
                    width: None,
                    radius: Some(*radius),
                }),
            );
        }
        // A press inside a floated card is the card's: it never
        // reaches what the card floats over (a dismissing backdrop,
        // the document under a comment card).
        element = element
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation());
        // Authored floating rails use unit scale; their measurement is
        // outside the translated child to avoid positional feedback.
        div()
            .relative()
            .child(element.child(self.node(content, window, cx)))
            .child(self.measure(key, cx))
            .into_any_element()
    }

    fn vector(&mut self, node: &wire::Node, window: &mut Window) -> AnyElement {
        let wire::Node::Svg {
            hash,
            bytes,
            color,
            inherit_button_ink,
            fit,
            width,
            height,
            opacity,
            ..
        } = node
        else {
            unreachable!()
        };
        if let Some(bytes) = bytes {
            self.remember_vector(*hash, bytes);
        }
        let mut element = dimensions(div(), *width, *height).opacity(opacity.unwrap_or(1.0));
        if let Some(bytes) = self.vectors.get(hash) {
            let monochrome = color.is_some() || *inherit_button_ink;
            if monochrome {
                let ink = color.map(rgba).unwrap_or_else(|| window.text_style().color);
                element = element.child(svg().data(bytes).size_full().text_color(ink));
            } else {
                // GPUI's SVG icon renderer is an alpha mask. Untinted
                // artwork instead uses its native full-color image decoder.
                element = element.child(
                    img(Arc::new(Image::from_bytes(
                        ImageFormat::Svg,
                        bytes.to_vec(),
                    )))
                    .size_full()
                    .object_fit(object_fit(*fit)),
                );
            }
        }
        element.into_any_element()
    }

    fn drawing(&mut self, node: &wire::Node, cx: &mut Context<Self>) -> AnyElement {
        let wire::Node::Canvas {
            key,
            width,
            height,
            commands,
            ..
        } = node
        else {
            unreachable!()
        };
        // Primitive canvases paint in the current native frame. Recreating an
        // asynchronous SVG image on every pointer move leaves blank drag frames.
        if native_canvas_commands(commands) {
            let commands = commands.clone();
            return dimensions(div().relative().overflow_hidden(), *width, *height)
                .child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, _| {
                            paint_canvas_commands(&commands, bounds.origin, window);
                        },
                    )
                    .size_full(),
                )
                .child(self.measure(key, cx))
                .into_any_element();
        }
        let bounds = self.bounds.get(key).copied().unwrap_or_default();
        let known = |length: Option<wire::Length>, measured: Pixels| match length {
            Some(wire::Length::Fixed(value)) => value,
            _ => f32::from(measured).max(1.0),
        };
        dimensions(div().relative(), *width, *height)
            .child(
                img(Arc::new(Image::from_bytes(
                    ImageFormat::Svg,
                    canvas_svg(
                        commands,
                        known(*width, bounds.size.width),
                        known(*height, bounds.size.height),
                    ),
                )))
                .size_full()
                .object_fit(ObjectFit::Fill),
            )
            .child(self.measure(key, cx))
            .into_any_element()
    }

    fn stack(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Stack {
            children,
            width,
            height,
            padding,
            background,
            border,
            clip,
            under,
            ..
        } = node
        else {
            unreachable!()
        };
        let mut element = decoration(
            pad(dimensions(div().relative(), *width, *height), *padding),
            *background,
            *border,
        );
        if *clip {
            element = element.overflow_hidden();
        }
        if *under == 0 {
            element = element.grid().grid_cols(1).grid_rows(1);
        }
        for (index, child) in children.iter().enumerate() {
            let content = self.node(child, window, cx);
            element = match (*under, index) {
                (0, _) => element.child(div().col_start(1).row_start(1).child(content)),
                (base, index) if index == base as usize => element.child(content),
                _ => element.child(div().absolute().inset_0().child(content)),
            };
        }
        element.into_any_element()
    }

    fn overlay(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Overlay {
            key,
            children,
            backdrop,
            padding,
            align_x,
            align_y,
            on_dismiss,
        } = node
        else {
            unreachable!()
        };
        let mut element = div().relative().size_full();
        if let Some(base) = children.first() {
            element = element.child(self.node(base, window, cx));
        }
        if let Some(modal) = children.get(1) {
            let shade = div()
                .id(format!("{key}/backdrop"))
                .absolute()
                .inset_0()
                .bg(rgba(*backdrop));
            let mut layer = div()
                .id(format!("{key}/layer"))
                .absolute()
                .inset_0()
                .flex()
                .p(px(*padding));
            if let Some(message) = on_dismiss {
                let message = *message;
                layer = layer.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |_, _, _, cx| cx.emit(wire::Event::Message(message))),
                );
            }
            layer = match align_x {
                wire::AlignX::Left => layer.justify_start(),
                wire::AlignX::Center => layer.justify_center(),
                wire::AlignX::Right => layer.justify_end(),
            };
            layer = match align_y {
                wire::AlignY::Top => layer.items_start(),
                wire::AlignY::Center => layer.items_center(),
                wire::AlignY::Bottom => layer.items_end(),
            };
            element = element.child(shade).child(
                layer.child(
                    div()
                        .bg(gpui_kit::component::Theme::global(cx)
                            .color_tokens()
                            .surface)
                        .text_color(
                            gpui_kit::component::Theme::global(cx)
                                .color_tokens()
                                .surface_foreground,
                        )
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(self.node(modal, window, cx)),
                ),
            );
        }
        element.into_any_element()
    }

    fn progress(&mut self, node: &wire::Node, cx: &mut Context<Self>) -> AnyElement {
        let wire::Node::Progress {
            value,
            min,
            max,
            axis,
            length,
            girth,
            background,
            bar,
            border,
            ..
        } = node
        else {
            unreachable!()
        };
        let span = max - min;
        let valid = span.is_finite() && span > 0.0;
        let fraction = match valid {
            true => ((value - min) / span).clamp(0.0, 1.0),
            false => 0.0,
        };
        let fill = div().bg(bar.map(rgba).unwrap_or_else(|| {
            gpui_kit::component::Theme::global(cx)
                .color_tokens()
                .primary
        }));
        match axis {
            wire::Axis::Row => decoration(dimensions(div(), *length, *girth), *background, *border)
                .child(fill.w(relative(fraction)).h_full())
                .into_any_element(),
            wire::Axis::Column => decoration(
                dimensions(div().flex().flex_col().justify_end(), *girth, *length),
                *background,
                *border,
            )
            .child(fill.h(relative(fraction)).w_full())
            .into_any_element(),
        }
    }

    fn pin(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Pin {
            content,
            x,
            y,
            width,
            height,
            ..
        } = node
        else {
            unreachable!()
        };
        dimensions(div().absolute().left(px(*x)).top(px(*y)), *width, *height)
            .child(self.node(content, window, cx))
            .into_any_element()
    }

    fn editor(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Editor {
            key,
            document,
            width,
            height,
            min_height,
            max_height,
            ..
        } = node
        else {
            unreachable!()
        };
        let Some(store) = self.editor_store.clone() else {
            return div().child("Editor host is unavailable").into_any_element();
        };
        let rich = matches!(node, wire::Node::Editor { options, .. } if options.rich.is_some());
        let changed_renderer = self
            .editors
            .get(key)
            .is_some_and(|mount| matches!(mount.view, EditorView::Rich(_)) != rich);
        let replaced_focus = self
            .editors
            .get(key)
            .filter(|_| changed_renderer)
            .and_then(|mount| {
                mount
                    .view
                    .is_focused(window, cx)
                    .then(|| wire::WidgetCommand::Focus {
                        target: key.clone(),
                    })
            });
        if changed_renderer {
            self.editors.remove(key);
        }
        if !self.editors.contains_key(key) {
            let events = store.clone();
            let (view, subscription) = match rich {
                true => {
                    let view = cx.new(|cx| {
                        crate::editor::wire::RichWireEditor::new(key.clone(), store, window, cx)
                    });
                    let subscription =
                        cx.subscribe(&view, move |_, _, _: &(), cx| drain_editor(&events, cx));
                    (EditorView::Rich(view), subscription)
                }
                false => {
                    let view = cx.new(|cx| {
                        crate::editor::wire::TextEditor::new(key.clone(), store, window, cx)
                    });
                    let subscription =
                        cx.subscribe(&view, move |_, _, _: &(), cx| drain_editor(&events, cx));
                    (EditorView::Text(view), subscription)
                }
            };
            self.editors.insert(
                key.clone(),
                EditorMount {
                    view,
                    _subscription: subscription,
                },
            );
        }
        let editor = self.editors.get(key).expect("editor inserted");
        editor.view.sync(window, cx);
        if let Some(command) = replaced_focus {
            editor.view.widget_command(&command, window, cx);
        }
        if self.presentation.editors.remove(key).as_ref() == Some(document) {
            editor.view.restore_focus(key, window, cx);
        }
        let view = editor.view.element();
        let mut element = dimensions(
            div().relative(),
            Some(width.map_or(wire::Length::Fill, wire::Length::Fixed)),
            Some(height.unwrap_or(wire::Length::Fill)),
        )
        .min_h(px(min_height.unwrap_or(0.)));
        if let Some(maximum) = max_height {
            element = element.max_h(px(*maximum));
        }
        element
            .child(view)
            .child(self.measure(key, cx))
            .into_any_element()
    }

    fn measure(&self, key: &str, cx: &Context<Self>) -> impl IntoElement + use<> {
        let route = key.to_owned();
        let weak = cx.entity().downgrade();
        canvas(
            move |bounds, _, cx| {
                let _ = weak.update(cx, |this, cx| {
                    let changed = this.bounds.get(&route) != Some(&bounds);
                    if changed {
                        this.bounds.insert(route, bounds);
                        cx.notify();
                    }
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0()
    }

    fn mouse_area(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::MouseArea {
            key,
            content,
            on_press,
            on_release,
            on_double_click,
            on_right_press,
            on_right_release,
            on_middle_press,
            on_middle_release,
            on_enter,
            on_exit,
            on_move,
            on_press_at,
            on_scroll,
        } = node
        else {
            unreachable!()
        };
        // A mouse area is layout-transparent, like a sensor: a fill-sized
        // child must not collapse inside an auto-sized wrapper.
        let (width, height) = content_dimensions(content);
        let mut element = dimensions(div().id(key.clone()).relative(), width, height);
        for (button, down, up) in [
            (MouseButton::Left, *on_press, *on_release),
            (MouseButton::Right, *on_right_press, *on_right_release),
            (MouseButton::Middle, *on_middle_press, *on_middle_release),
        ] {
            if let Some(message) = down {
                element = element.on_mouse_down(
                    button,
                    cx.listener(move |_, _, _, cx| cx.emit(wire::Event::Message(message))),
                );
            }
            if let Some(message) = up {
                element = element.on_mouse_up(
                    button,
                    cx.listener(move |_, _, _, cx| cx.emit(wire::Event::Message(message))),
                );
            }
        }
        if let Some(message) = on_double_click {
            let message = *message;
            element = element.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, event: &MouseDownEvent, _, cx| {
                    if event.click_count == 2 {
                        cx.emit(wire::Event::Message(message));
                    }
                }),
            );
        }
        let enter = *on_enter;
        let exit = *on_exit;
        element = element.on_hover(cx.listener(move |_, hovered, _, cx| {
            let message = match hovered {
                true => enter,
                false => exit,
            };
            if let Some(message) = message {
                cx.emit(wire::Event::Message(message));
            }
        }));
        if let Some(handler) = on_move {
            let handler = *handler;
            let route = key.clone();
            element =
                element.on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                    let origin = this
                        .bounds
                        .get(&route)
                        .map_or(Point::default(), |bounds| bounds.origin);
                    let local = event.position - origin;
                    cx.emit(wire::Event::Pointer {
                        handler,
                        x: f32::from(local.x),
                        y: f32::from(local.y),
                    });
                }));
        }
        if let Some(handler) = on_press_at {
            let handler = *handler;
            let route = key.clone();
            element = element.capture_any_mouse_down(cx.listener(
                move |this, event: &MouseDownEvent, _, cx| {
                    // A right press reports its position too: a context menu
                    // opens where the pointer is.
                    let reported = matches!(event.button, MouseButton::Left | MouseButton::Right);
                    if !reported {
                        return;
                    }
                    let Some(bounds) = this.bounds.get(&route) else {
                        return;
                    };
                    if !bounds.contains(&event.position) {
                        return;
                    }
                    let local = event.position - bounds.origin;
                    cx.emit(wire::Event::Pointer {
                        handler,
                        x: f32::from(local.x),
                        y: f32::from(local.y),
                    });
                },
            ));
        }
        if let Some(handler) = on_scroll {
            let handler = *handler;
            element =
                element.on_scroll_wheel(cx.listener(move |_, event: &ScrollWheelEvent, _, cx| {
                    let (delta, pixels) = match event.delta {
                        ScrollDelta::Pixels(delta) => {
                            (point(f32::from(delta.x), f32::from(delta.y)), true)
                        }
                        ScrollDelta::Lines(delta) => (delta, false),
                    };
                    cx.emit(wire::Event::Scroll {
                        handler,
                        dx: delta.x,
                        dy: delta.y,
                        pixels,
                    });
                }));
        }
        element
            .child(self.node(content, window, cx))
            .child(self.measure(key, cx))
            .into_any_element()
    }

    fn slider(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Slider {
            key,
            value,
            min,
            max,
            step,
            on_change,
            on_release,
            axis,
            width,
            height,
            ..
        } = node
        else {
            unreachable!()
        };
        let bounds = [*min, *max, *step];
        let rebuild = self
            .ranges
            .get(key)
            .is_none_or(|control| control.bounds != bounds);
        if rebuild {
            let state = cx.new(|_| {
                SliderState::new()
                    .min(*min)
                    .max(*max)
                    .step(*step)
                    .default_value(*value)
            });
            let route = key.clone();
            let subscription = cx.subscribe_in(&state, window, move |this, _, event, _, cx| {
                let Some(control) = this.ranges.get_mut(&route) else {
                    return;
                };
                match event {
                    SliderEvent::Change(value) => {
                        control.value = value.start();
                        cx.emit(wire::Event::Slide {
                            handler: control.on_change,
                            value: control.value,
                        });
                    }
                    SliderEvent::Release(_) => {
                        if let Some(message) = control.on_release {
                            cx.emit(wire::Event::Message(message));
                        }
                    }
                }
            });
            self.ranges.insert(
                key.clone(),
                RangeControl {
                    state,
                    bounds,
                    value: *value,
                    on_change: *on_change,
                    on_release: *on_release,
                    _subscription: subscription,
                },
            );
        }
        let control = self.ranges.get_mut(key).expect("range inserted");
        control.on_change = *on_change;
        control.on_release = *on_release;
        if control.value != *value {
            control.value = *value;
            control
                .state
                .update(cx, |state, cx| state.set_value(*value, window, cx));
        }
        let slider = match axis {
            wire::Axis::Row => Slider::new(&control.state).horizontal(),
            wire::Axis::Column => Slider::new(&control.state).vertical(),
        };
        dimensions(div(), *width, *height)
            .child(slider)
            .into_any_element()
    }

    fn rich_text(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::RichText {
            key,
            spans,
            size,
            color,
            font,
            width,
            align_x,
            options,
            on_link,
        } = node
        else {
            unreachable!()
        };
        let canonical = spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();
        let selection = self.rich_selections.entry(key.clone()).or_insert_with(|| {
            let handle = gpui_kit::base::TextSelectionHandle::new(canonical.clone(), cx);
            let refresh = handle.refresh_window_on_change(window, cx);
            RichSelection {
                handle,
                _refresh: refresh,
            }
        });
        selection.handle.set_fallback_copy_text(canonical, cx);
        let handle = selection.handle.clone();
        let mut content = text_options(
            dimensions(div().flex().flex_col(), *width, options.height),
            *font,
            *align_x,
            options,
        );
        if let Some(size) = size {
            content = content.text_size(px(*size));
        }
        if let Some(color) = color {
            content = content.text_color(rgba(*color));
        }
        let base_size = size
            .map(px)
            .unwrap_or_else(|| window.text_style().font_size.to_pixels(window.rem_size()));
        let line_height = match options.line_height {
            Some(wire::LineHeight::Absolute(height)) => px(height),
            Some(wire::LineHeight::Relative(height)) => base_size * height,
            None => window
                .text_style()
                .line_height
                .to_pixels(base_size.into(), window.rem_size()),
        };
        let native_row = || {
            let mut row = div().flex().flex_row().items_baseline().min_h(line_height);
            if options.wrapping != Some(wire::Wrapping::None) {
                row = row.flex_wrap();
            }
            horizontal_align(row, *align_x)
        };
        let mut row = native_row();
        let mut layouts = Vec::new();
        let selections = std::rc::Rc::new(std::cell::RefCell::new(Vec::<
            Option<std::ops::Range<usize>>,
        >::new()));
        // ONE SPAN IS ONE PARAGRAPH, laid out by the text system: it wraps
        // where a plain label would, with the same spacing, and the whole
        // run is one selection layout. Only a paragraph of SEVERAL spans is
        // cut into word fragments, so native flex can wrap across the style
        // changes — the cut is what made a long plain message read
        // differently from every other text on screen.
        let whole = spans.len() == 1;
        for (span_index, span) in spans.iter().enumerate() {
            // Native flex wraps at Unicode word boundaries; padding is paint
            // geometry only, while every copied fragment retains source bytes.
            let fragments = if whole {
                vec![span.content.as_str()]
            } else {
                span.content.split_word_bounds().collect::<Vec<_>>()
            };
            for (index, fragment) in fragments.iter().enumerate() {
                let mut run_options = options.clone();
                run_options.font = span.font.clone().or_else(|| options.font.clone());
                run_options.line_height = span.line_height.or(options.line_height);
                let plate = if whole {
                    div().min_w_0().w_full()
                } else {
                    div().flex_shrink_0().max_w_full()
                };
                let mut paint = text_options(plate, *font, None, &run_options);
                if let Some(size) = span.size {
                    paint = paint.text_size(px(size));
                }
                if let Some(color) = span.color {
                    paint = paint.text_color(rgba(color));
                }
                let mut padding = span.padding.unwrap_or_default();
                if index > 0 {
                    padding.left = 0.;
                }
                if index + 1 < fragments.len() {
                    padding.right = 0.;
                }
                paint = decoration(pad(paint, Some(padding)), span.background, span.border);
                let mut style = HighlightStyle::default();
                if span.underline {
                    style.underline = Some(UnderlineStyle {
                        thickness: px(1.),
                        color: span.color.map(rgba),
                        wavy: false,
                    });
                }
                if span.strikethrough {
                    style.strikethrough = Some(StrikethroughStyle {
                        thickness: px(1.),
                        color: span.color.map(rgba),
                    });
                }
                let text: SharedString = (*fragment).to_owned().into();
                let styled =
                    StyledText::new(text.clone()).with_highlights([(0..text.len(), style)]);
                let layout = styled.layout().clone();
                let run_index = layouts.len();
                layouts.push((text, layout.clone()));
                let ranges = selections.clone();
                let selection = canvas(
                    |_, _, _| (),
                    move |_, (), window, cx| {
                        if let Some(Some(range)) = ranges.borrow().get(run_index) {
                            paint_rich_selection(&layout, range, window, cx);
                        }
                    },
                )
                .absolute()
                .size_full();
                let id = format!("{key}-span-{span_index}-{index}");
                let mut painted = paint.id(id).child(selection).child(styled);
                if let (Some(handler), Some(link)) = (on_link, &span.link) {
                    let handler = *handler;
                    let link = link.clone();
                    painted =
                        painted
                            .cursor_pointer()
                            .on_click(cx.listener(move |_, _, window, cx| {
                                if gpui_kit::base::TextSelection::has_selection(window, cx) {
                                    return;
                                }
                                cx.emit(wire::Event::Input {
                                    handler,
                                    text: link.clone(),
                                });
                            }));
                }
                // a whole paragraph keeps its newlines: the text system
                // breaks the lines, and the selection layout spans them
                let newline = !whole && fragment.contains('\n');
                if newline {
                    // Explicit source line breaks remain real measured text, not
                    // injected spaces in the selection/copy representation.
                    // One native row per explicit source line preserves empty
                    // lines too. The newline participates in copy, not sizing.
                    painted = painted.w(px(0.)).h(px(0.));
                    row = row.child(painted);
                    content = content.child(row);
                    row = native_row();
                    continue;
                }
                row = row.child(painted);
            }
        }
        content = content.child(row);
        RichParagraph {
            id: key.clone().into(),
            content: content.into_any_element(),
            layouts,
            handle,
            selections,
        }
        .into_any_element()
    }

    fn flex(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::Flex {
            layout,
            children,
            items,
            background,
            border,
            ..
        } = node
        else {
            unreachable!()
        };
        let mut element = decoration(
            pad(
                dimensions(div().flex(), layout.width, layout.height),
                layout.padding,
            ),
            *background,
            *border,
        );
        element = match layout.direction {
            wire::FlexDirection::Row => element.flex_row(),
            wire::FlexDirection::RowReverse => element.flex_row_reverse(),
            wire::FlexDirection::Column => element.flex_col(),
            wire::FlexDirection::ColumnReverse => element.flex_col_reverse(),
        };
        element = match layout.wrap {
            wire::FlexWrap::NoWrap => element,
            wire::FlexWrap::Wrap => element.flex_wrap(),
            wire::FlexWrap::WrapReverse => element.flex_wrap_reverse(),
        };
        if let Some(width) = layout.max_width {
            element = element.max_w(px(width));
        }
        if let Some(height) = layout.max_height {
            element = element.max_h(px(height));
        }
        if let Some(gap) = layout.row_gap {
            element = element.gap_y(px(gap));
        }
        if let Some(gap) = layout.column_gap {
            element = element.gap_x(px(gap));
        }
        if layout.clip {
            element = element.overflow_hidden();
        }
        if let Some(alignment) = layout.justify {
            element = justify(element, alignment);
        }
        if let Some(alignment) = layout.items {
            element = align_items(element, alignment);
        }
        if let Some(alignment) = layout.content {
            element = match alignment {
                wire::FlexContentAlignment::Start | wire::FlexContentAlignment::FlexStart => {
                    element.content_start()
                }
                wire::FlexContentAlignment::End | wire::FlexContentAlignment::FlexEnd => {
                    element.content_end()
                }
                wire::FlexContentAlignment::Center => element.content_center(),
                wire::FlexContentAlignment::SpaceBetween => element.content_between(),
                wire::FlexContentAlignment::SpaceAround => element.content_around(),
                wire::FlexContentAlignment::SpaceEvenly => element.content_evenly(),
                wire::FlexContentAlignment::Stretch => element.content_stretch(),
            };
        }
        let mut order: Vec<_> = children.iter().enumerate().collect();
        order.sort_by_key(|(index, _)| items.get(*index).map_or(0, |item| item.order));
        for (index, child) in order {
            let mut item = div();
            if let Some(rules) = items.get(index) {
                item.style().flex_grow = rules.grow;
                item.style().flex_shrink = Some(rules.shrink);
                item.style().flex_basis = match rules.basis {
                    wire::FlexBasis::Auto | wire::FlexBasis::Content => Some(auto()),
                    wire::FlexBasis::Fixed(value) => Some(px(value).into()),
                    wire::FlexBasis::Percent(value) => Some(relative(value / 100.0).into()),
                };
                if let Some(alignment) = rules.align {
                    item = match alignment {
                        wire::FlexItemAlignment::Start => item.self_start(),
                        wire::FlexItemAlignment::FlexStart => item.self_flex_start(),
                        wire::FlexItemAlignment::End => item.self_end(),
                        wire::FlexItemAlignment::FlexEnd => item.self_flex_end(),
                        wire::FlexItemAlignment::Center => item.self_center(),
                        wire::FlexItemAlignment::Baseline => item.self_baseline(),
                        wire::FlexItemAlignment::Stretch => item.self_stretch(),
                    };
                }
                let margin = |value| match value {
                    wire::FlexMargin::Zero => px(0.0).into(),
                    wire::FlexMargin::Auto => auto(),
                    wire::FlexMargin::Fixed(value) => px(value).into(),
                    wire::FlexMargin::Percent(value) => relative(value / 100.0).into(),
                };
                item = item
                    .mt(margin(rules.margins.top))
                    .mr(margin(rules.margins.right))
                    .mb(margin(rules.margins.bottom))
                    .ml(margin(rules.margins.left));
            }
            element = element.child(item.child(self.node(child, window, cx)));
        }
        let mut outer = dimensions(div(), layout.surface_width, layout.surface_height);
        if let Some(width) = layout.surface_max_width {
            outer = outer.max_w(px(width));
        }
        outer.child(element).into_any_element()
    }

    fn refresh_resource(&self, data: Option<&wire::ImageData>, window: &mut Window) {
        if let Some(wire::ImageData::Resource(_)) = data {
            window.request_animation_frame();
        }
    }

    fn remember_image(&mut self, hash: u64, data: &wire::ImageData) {
        if matches!(data, wire::ImageData::Resource(_)) {
            return;
        }
        if self.images.contains_key(&hash) {
            return;
        }
        let Some(image) = decode_image(data) else {
            return;
        };
        self.images.insert(hash, Arc::new(image));
    }

    fn image_frame(&self, hash: u64, data: Option<&wire::ImageData>) -> Option<Arc<RenderImage>> {
        match data {
            Some(wire::ImageData::Resource(key)) => {
                crate::video::stage_frame(key).map(|(_, _, image)| image)
            }
            _ => self.images.get(&hash).cloned(),
        }
    }

    fn remember_vector(&mut self, hash: u64, bytes: &[u8]) {
        if self.vectors.contains_key(&hash) {
            return;
        }
        self.vectors.insert(hash, Arc::from(bytes));
    }

    fn image_viewer(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wire::Node::ImageViewer {
            key,
            hash,
            data,
            width,
            height,
            options,
            ..
        } = node
        else {
            unreachable!()
        };
        self.refresh_resource(data.as_ref(), window);
        if let Some(data) = data {
            self.remember_image(*hash, data);
        }
        let frame = self.image_frame(*hash, data.as_ref());
        let viewer = self.viewers.entry(key.clone()).or_default();
        if viewer.scale == 0.0 {
            viewer.scale = 1.0;
        }
        let mut element =
            dimensions(div().relative().overflow_hidden(), *width, *height).id(key.clone());
        if let Some(image) = frame {
            let original = image.size(0);
            let viewport = self
                .bounds
                .get(key)
                .map_or(window.viewport_size(), |bounds| bounds.size);
            let inset = options.padding.unwrap_or_default() * 2.0;
            let ratio = ((f32::from(viewport.width) - inset) / u32::from(original.width) as f32)
                .min((f32::from(viewport.height) - inset) / u32::from(original.height) as f32)
                .max(0.0);
            let width = u32::from(original.width) as f32 * ratio * viewer.scale;
            let height = u32::from(original.height) as f32 * ratio * viewer.scale;
            let x = (f32::from(viewport.width) - width) / 2.0 + f32::from(viewer.offset.x);
            let y = (f32::from(viewport.height) - height) / 2.0 + f32::from(viewer.offset.y);
            element = element.child(
                img(image.clone())
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(width))
                    .h(px(height)),
            );
        }
        let (minimum, maximum) = options.scale_bounds.unwrap_or((0.25, 10.0));
        let step = options.scale_step.unwrap_or(0.1);
        let wheel_key = key.clone();
        element =
            element.on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
                let delta = match event.delta {
                    ScrollDelta::Pixels(delta) => f32::from(delta.y),
                    ScrollDelta::Lines(delta) => delta.y,
                };
                let Some(viewer) = this.viewers.get_mut(&wheel_key) else {
                    return;
                };
                viewer.scale =
                    (viewer.scale * (1.0 + step).powf(delta.signum())).clamp(minimum, maximum);
                cx.stop_propagation();
                cx.notify();
            }));
        let down_key = key.clone();
        element = element.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                if let Some(viewer) = this.viewers.get_mut(&down_key) {
                    viewer.drag = Some(event.position);
                }
                cx.stop_propagation();
            }),
        );
        let move_key = key.clone();
        element = element.on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
            let Some(viewer) = this.viewers.get_mut(&move_key) else {
                return;
            };
            let Some(previous) = viewer.drag else {
                return;
            };
            if event.pressed_button != Some(MouseButton::Left) {
                viewer.drag = None;
                return;
            }
            viewer.offset += event.position - previous;
            viewer.drag = Some(event.position);
            cx.notify();
        }));
        let up_key = key.clone();
        element = element.on_mouse_up(
            MouseButton::Left,
            cx.listener(move |this, _, _, _| {
                if let Some(viewer) = this.viewers.get_mut(&up_key) {
                    viewer.drag = None;
                }
            }),
        );
        element.child(self.measure(key, cx)).into_any_element()
    }

    fn picture(&mut self, node: &wire::Node, window: &mut Window) -> AnyElement {
        let wire::Node::Image {
            hash,
            data,
            width,
            height,
            fit,
            opacity,
            ..
        } = node
        else {
            unreachable!()
        };
        self.refresh_resource(data.as_ref(), window);
        if let Some(data) = data {
            self.remember_image(*hash, data);
        }
        let mut element = dimensions(div(), *width, *height).opacity(opacity.unwrap_or(1.0));
        if let Some(image) = self.image_frame(*hash, data.as_ref()) {
            element = element.child(img(image.clone()).size_full().object_fit(object_fit(*fit)));
        }
        element.into_any_element()
    }

    fn picker(
        &mut self,
        node: &wire::Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (key, options, selected, handler, placeholder, width, reset, input, menu_height) =
            match node {
                wire::Node::PickList {
                    key,
                    options,
                    selected,
                    on_select,
                    placeholder,
                    width,
                    settings,
                    ..
                } => (
                    key,
                    options,
                    *selected,
                    *on_select,
                    placeholder.as_deref().unwrap_or_default(),
                    *width,
                    None,
                    None,
                    settings.menu_height,
                ),
                wire::Node::ComboBox {
                    key,
                    state_key,
                    options,
                    selected,
                    on_select,
                    placeholder,
                    width,
                    reset,
                    settings,
                    ..
                } => (
                    key,
                    options,
                    *selected,
                    *on_select,
                    placeholder.as_str(),
                    *width,
                    Some((state_key.clone(), *reset)),
                    settings.input,
                    settings.menu_height,
                ),
                _ => unreachable!(),
            };
        if self
            .pickers
            .get(key)
            .is_some_and(|picker| picker.reset != reset)
        {
            self.pickers.remove(key);
        }
        let weak = cx.entity().downgrade();
        let choices = || {
            let items = options
                .iter()
                .enumerate()
                .map(|(index, label)| Choice {
                    index: index as u32,
                    label: label.clone(),
                })
                .collect::<Vec<_>>();
            let weak = weak.clone();
            let key = key.clone();
            PickerChoices {
                items: SearchableVec::new(items),
                query: Box::new(move |query, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        if let Some(handler) =
                            this.pickers.get(&key).and_then(|picker| picker.input)
                        {
                            cx.emit(wire::Event::Input {
                                handler,
                                text: query.to_owned(),
                            });
                        }
                    });
                }),
            }
        };
        let index = selected.map(|index| IndexPath::new(index as usize));
        if !self.pickers.contains_key(key) {
            let state = cx.new(|cx| {
                SelectState::new(choices(), index, window, cx).searchable(reset.is_some())
            });
            let route = key.to_owned();
            let subscription = cx.subscribe_in(&state, window, move |this, _, event, _, cx| {
                let SelectEvent::Confirm(Some(index)) = event else {
                    return;
                };
                let Some(picker) = this.pickers.get_mut(&route) else {
                    return;
                };
                picker.selected = Some(*index);
                cx.emit(wire::Event::Select {
                    handler: picker.handler,
                    index: *index,
                });
            });
            self.pickers.insert(
                key.into(),
                Picker {
                    state,
                    options: options.to_vec(),
                    selected,
                    handler,
                    input,
                    reset,
                    _subscription: subscription,
                },
            );
        }
        let picker = self.pickers.get_mut(key).expect("picker inserted");
        picker.handler = handler;
        picker.input = input;
        if picker.options != *options {
            picker.options = options.to_vec();
            picker
                .state
                .update(cx, |state, cx| state.set_items(choices(), window, cx));
        }
        if picker.selected != selected {
            picker.selected = selected;
            let state = picker.state.downgrade();
            let route = key.clone();
            // A wire index identifies a value in the full option set, not a
            // row in the filtered menu. Native value projection clears search
            // synchronously, so run it after releasing this render borrow.
            window.defer(cx, move |window, cx| {
                let Some(state) = state.upgrade() else {
                    return;
                };
                let current = weak
                    .read_with(cx, |tree, _| {
                        tree.pickers.get(&route).is_some_and(|picker| {
                            picker.state.entity_id() == state.entity_id()
                                && picker.selected == selected
                        })
                    })
                    .unwrap_or(false);
                if !current {
                    return;
                }
                state.update(cx, |state, cx| match selected {
                    Some(value) => state.set_selected_value(&value, window, cx),
                    None => state.set_selected_index(None, window, cx),
                });
            });
        }
        let mut select = Select::new(&picker.state)
            .id(key.clone())
            .placeholder(placeholder.to_owned());
        if let Some(wire::Length::Fixed(height)) = menu_height {
            select = select.menu_max_h(px(height));
        }
        dimensions(
            div().relative().child(select).child(self.measure(key, cx)),
            width,
            None,
        )
        .into_any_element()
    }
}

impl Render for ViewTree {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for image in crate::video::take_retired() {
            let _ = window.drop_image(image);
        }
        self.mounted.clear();
        let node = self.node(&self.root.clone(), window, cx);
        // Only controls mounted by this replacement frame may recover focus.
        self.presentation = NativePresentation::default();
        node
    }
}

fn native_cursor(cursor: Option<wire::mouse::Cursor>) -> CursorStyle {
    use wire::mouse::Cursor as C;
    match cursor {
        Some(C::ResizingHorizontally) => CursorStyle::ResizeLeftRight,
        Some(C::ResizingVertically) => CursorStyle::ResizeUpDown,
        Some(C::ResizingDiagonallyUp) => CursorStyle::ResizeUpRightDownLeft,
        Some(C::ResizingDiagonallyDown) => CursorStyle::ResizeUpLeftDownRight,
        Some(C::ResizingColumn) => CursorStyle::ResizeColumn,
        Some(C::ResizingRow) => CursorStyle::ResizeRow,
        Some(C::Pointer) => CursorStyle::PointingHand,
        Some(C::Grab) => CursorStyle::OpenHand,
        Some(C::Grabbing | C::Move | C::AllScroll) => CursorStyle::ClosedHand,
        Some(C::Text) => CursorStyle::IBeam,
        Some(C::Cell | C::Crosshair) => CursorStyle::Crosshair,
        Some(C::NoDrop | C::NotAllowed) => CursorStyle::OperationNotAllowed,
        Some(C::Alias) => CursorStyle::DragLink,
        Some(C::Copy) => CursorStyle::DragCopy,
        Some(C::ContextMenu) => CursorStyle::ContextualMenu,
        Some(
            C::None
            | C::Hidden
            | C::Idle
            | C::Help
            | C::Progress
            | C::Wait
            | C::ZoomIn
            | C::ZoomOut,
        )
        | None => CursorStyle::Arrow,
    }
}

fn rgba(color: wire::Rgba) -> Hsla {
    let [r, g, b, a] = color.0;
    gpui_kit::Rgba { r, g, b, a }.into()
}

fn content_dimensions(node: &wire::Node) -> (Option<wire::Length>, Option<wire::Length>) {
    match node {
        wire::Node::Space { width, height }
        | wire::Node::Linear { width, height, .. }
        | wire::Node::KeyedColumn { width, height, .. }
        | wire::Node::Grid { width, height, .. }
        | wire::Node::Container { width, height, .. }
        | wire::Node::Hover { width, height, .. }
        | wire::Node::Scroll { width, height, .. }
        | wire::Node::Stack { width, height, .. }
        | wire::Node::Responsive { width, height, .. } => (*width, *height),
        // an overlay always renders full-size (see its arm)
        wire::Node::Overlay { .. } => (Some(wire::Length::Fill), Some(wire::Length::Fill)),
        _ => (None, None),
    }
}

fn dimensions<T: Styled>(
    mut element: T,
    width: Option<wire::Length>,
    height: Option<wire::Length>,
) -> T {
    element = match width {
        Some(wire::Length::Fixed(value)) => element.w(px(value)).min_w(px(value)),
        Some(wire::Length::Fill) => element.w_full().min_w_0(),
        Some(wire::Length::FillPortion(_)) => element.flex_1(),
        Some(wire::Length::Shrink) | None => element,
    };
    match height {
        Some(wire::Length::Fixed(value)) => element.h(px(value)).min_h(px(value)),
        Some(wire::Length::Fill) => element.h_full().min_h_0(),
        Some(wire::Length::FillPortion(_)) => element.flex_1(),
        Some(wire::Length::Shrink) | None => element,
    }
}

/// A virtual column and its surrounding vertical chrome share one native
/// viewport. Keep the wire wrappers on each item, splitting only their outer
/// padding, so prefix controls never become part of the message-key sequence.
fn virtual_rows(node: &wire::Node) -> Option<Vec<VirtualRow>> {
    use wire::Node;
    match node {
        Node::KeyedColumn {
            key,
            keys,
            children,
            virtual_row: Some(estimated_height),
            spacing,
            ..
        } => {
            let rows = children
                .iter()
                .enumerate()
                .map(|(index, content)| VirtualRow {
                    key: keys
                        .as_ref()
                        .and_then(|keys| keys.get(index))
                        .map(|identity| format!("{key}/@row:{}", identity.virtual_key()))
                        .unwrap_or_else(|| format!("{key}/@index:{index}")),
                    content: content.clone(),
                    estimated_height: *estimated_height,
                    gap: if index + 1 < children.len() {
                        spacing.unwrap_or_default()
                    } else {
                        0.
                    },
                })
                .collect();
            Some(wrap_virtual_rows(node, rows))
        }
        Node::Linear {
            axis: wire::Axis::Column,
            children,
            spacing,
            ..
        } => {
            if !children.iter().any(has_virtual_column) {
                return None;
            }
            let mut found = false;
            let mut rows = Vec::new();
            for (index, child) in children.iter().enumerate() {
                let mut part = match virtual_rows(child) {
                    Some(rows) => {
                        found = true;
                        rows
                    }
                    None => vec![VirtualRow {
                        key: child.key().map(str::to_owned).unwrap_or_else(|| {
                            format!("{}/@static:{index}", node.key().unwrap_or("column"))
                        }),
                        content: child.clone(),
                        gap: 0.,
                        estimated_height: 44.,
                    }],
                };
                if index + 1 < children.len()
                    && let Some(last) = part.last_mut()
                {
                    last.gap += spacing.unwrap_or_default();
                }
                rows.extend(part);
            }
            found.then(|| wrap_virtual_rows(node, rows))
        }
        Node::Container { content, .. } => {
            virtual_rows(content).map(|rows| wrap_virtual_rows(node, rows))
        }
        _ => None,
    }
}

fn has_virtual_column(node: &wire::Node) -> bool {
    match node {
        wire::Node::KeyedColumn {
            virtual_row: Some(_),
            ..
        } => true,
        wire::Node::Linear {
            axis: wire::Axis::Column,
            children,
            ..
        } => children.iter().any(has_virtual_column),
        wire::Node::Container { content, .. } => has_virtual_column(content),
        _ => false,
    }
}

fn wrap_virtual_rows(node: &wire::Node, rows: Vec<VirtualRow>) -> Vec<VirtualRow> {
    let mut shell = node.clone();
    match &mut shell {
        wire::Node::Linear { children, .. } | wire::Node::KeyedColumn { children, .. } => {
            children.clear()
        }
        wire::Node::Container { content, .. } => {
            **content = wire::Node::Space {
                width: None,
                height: None,
            }
        }
        _ => unreachable!("only vertical layout wrappers surround virtual rows"),
    }
    let count = rows.len();
    rows.into_iter()
        .enumerate()
        .map(|(index, mut row)| {
            let mut wrapped = shell.clone();
            let padding = match &mut wrapped {
                wire::Node::Linear {
                    children,
                    padding,
                    height,
                    ..
                } => {
                    children.push(row.content);
                    *height = None;
                    padding
                }
                wire::Node::KeyedColumn {
                    children,
                    keys,
                    virtual_row,
                    padding,
                    height,
                    ..
                } => {
                    children.push(row.content);
                    *keys = None;
                    *virtual_row = None;
                    *height = None;
                    padding
                }
                wire::Node::Container {
                    content,
                    padding,
                    height,
                    ..
                } => {
                    **content = row.content;
                    *height = None;
                    padding
                }
                _ => unreachable!("vertical layout wrapper"),
            };
            if let Some(padding) = padding {
                if index > 0 {
                    padding.top = 0.;
                }
                if index + 1 < count {
                    padding.bottom = 0.;
                }
            }
            row.content = wrapped;
            row
        })
        .collect()
}

fn pad<T: Styled>(element: T, padding: Option<wire::Edges>) -> T {
    match padding {
        Some(edges) => element
            .pt(px(edges.top))
            .pr(px(edges.right))
            .pb(px(edges.bottom))
            .pl(px(edges.left)),
        None => element,
    }
}

fn decoration<T: Styled>(
    mut element: T,
    background: Option<wire::Rgba>,
    border: Option<wire::Border>,
) -> T {
    if let Some(color) = background {
        element = element.bg(rgba(color));
    }
    if let Some(border) = border {
        if let Some(color) = border.color {
            element = element.border_color(rgba(color));
        }
        if let Some(width) = border.width {
            element = element.border(px(width));
        }
        if let Some([tl, tr, br, bl]) = border.radius {
            element = element
                .rounded_tl(px(tl))
                .rounded_tr(px(tr))
                .rounded_br(px(br))
                .rounded_bl(px(bl));
        }
    }
    element
}

fn button_style(button: Button, preset: wire::ButtonPreset) -> Button {
    match preset {
        wire::ButtonPreset::Primary => button.primary(),
        // A secondary action is an outlined button: the filled grey block
        // is reserved for `Background` (a tab, a chip).
        wire::ButtonPreset::Secondary => button.outline(),
        wire::ButtonPreset::Success => button.success(),
        wire::ButtonPreset::Warning => button.warning(),
        wire::ButtonPreset::Danger => button.danger(),
        wire::ButtonPreset::Text => button.link(),
        wire::ButtonPreset::Background => button.secondary(),
        wire::ButtonPreset::Subtle => button.ghost(),
    }
}

fn font_weight(weight: wire::Weight) -> FontWeight {
    match weight {
        wire::Weight::Thin => FontWeight::THIN,
        wire::Weight::ExtraLight => FontWeight::EXTRA_LIGHT,
        wire::Weight::Light => FontWeight::LIGHT,
        wire::Weight::Normal => FontWeight::NORMAL,
        wire::Weight::Medium => FontWeight::MEDIUM,
        wire::Weight::Semibold => FontWeight::SEMIBOLD,
        wire::Weight::Bold => FontWeight::BOLD,
        wire::Weight::ExtraBold => FontWeight::EXTRA_BOLD,
        wire::Weight::Black => FontWeight::BLACK,
    }
}

fn text_options(
    mut element: Div,
    font: wire::Font,
    align: Option<wire::AlignX>,
    options: &wire::TextOptions,
) -> Div {
    element = element.font_weight(font_weight(font.weight));
    if font.monospace {
        element = element.font_family("monospace");
    }
    if let Some(font) = &options.font {
        let family = match &font.family {
            wire::FontFamily::Named(name) => name.clone(),
            wire::FontFamily::Serif => "serif".into(),
            wire::FontFamily::SansSerif => "sans-serif".into(),
            wire::FontFamily::Monospace => "monospace".into(),
            wire::FontFamily::Cursive => "cursive".into(),
            wire::FontFamily::Fantasy => "fantasy".into(),
        };
        element = element
            .font_family(family)
            .font_weight(font_weight(font.weight));
        if font.style != wire::FontStyle::Normal {
            element = element.italic();
        }
    }
    element = match align {
        Some(wire::AlignX::Center) => element.text_center(),
        Some(wire::AlignX::Right) => element.text_right(),
        _ => element,
    };
    if let Some(height) = options.line_height {
        element = match height {
            wire::LineHeight::Relative(value) => element.line_height(relative(value)),
            wire::LineHeight::Absolute(value) => element.line_height(px(value)),
        };
    }
    if options.wrapping == Some(wire::Wrapping::None) {
        // A non-wrapping label still owns only its allocated box. In a row,
        // painting the full intrinsic line would cover the following fields.
        element = element.truncate();
    } else {
        // Native buttons set inherited nowrap. A wire label's wrapping is
        // independent of its interactive ancestor, including the default.
        element = element.whitespace_normal();
    }
    element
}

fn horizontal_align(element: Div, alignment: Option<wire::AlignX>) -> Div {
    match alignment {
        Some(wire::AlignX::Left) => element.justify_start(),
        Some(wire::AlignX::Center) => element.justify_center(),
        Some(wire::AlignX::Right) => element.justify_end(),
        None => element,
    }
}

fn vertical_align(element: Div, alignment: Option<wire::AlignY>) -> Div {
    match alignment {
        Some(wire::AlignY::Top) => element.items_start(),
        Some(wire::AlignY::Center) => element.items_center(),
        Some(wire::AlignY::Bottom) => element.items_end(),
        None => element,
    }
}

fn cross_align(element: Div, alignment: Option<wire::AlignX>) -> Div {
    match alignment {
        Some(wire::AlignX::Left) => element.items_start(),
        Some(wire::AlignX::Center) => element.items_center(),
        Some(wire::AlignX::Right) => element.items_end(),
        None => element,
    }
}

fn justify(element: Div, alignment: wire::FlexContentAlignment) -> Div {
    match alignment {
        wire::FlexContentAlignment::Start | wire::FlexContentAlignment::FlexStart => {
            element.justify_start()
        }
        wire::FlexContentAlignment::End | wire::FlexContentAlignment::FlexEnd => {
            element.justify_end()
        }
        wire::FlexContentAlignment::Center => element.justify_center(),
        wire::FlexContentAlignment::SpaceBetween => element.justify_between(),
        wire::FlexContentAlignment::SpaceAround => element.justify_around(),
        wire::FlexContentAlignment::SpaceEvenly => element.justify_evenly(),
        wire::FlexContentAlignment::Stretch => element,
    }
}

fn align_items(element: Div, alignment: wire::FlexItemAlignment) -> Div {
    match alignment {
        wire::FlexItemAlignment::Start | wire::FlexItemAlignment::FlexStart => {
            element.items_start()
        }
        wire::FlexItemAlignment::End | wire::FlexItemAlignment::FlexEnd => element.items_end(),
        wire::FlexItemAlignment::Center => element.items_center(),
        wire::FlexItemAlignment::Baseline => element.items_baseline(),
        wire::FlexItemAlignment::Stretch => element.items_stretch(),
    }
}

fn shadows(element: Div, shadow: wire::Shadow) -> Div {
    let Some(color) = shadow.color else {
        return element;
    };
    element.shadow(vec![BoxShadow {
        color: rgba(color),
        offset: point(
            px(shadow.x.unwrap_or_default()),
            px(shadow.y.unwrap_or_default()),
        ),
        blur_radius: px(shadow.blur.unwrap_or_default()),
        spread_radius: px(0.0),
        inset: false,
    }])
}

fn object_fit(fit: Option<wire::ContentFit>) -> ObjectFit {
    match fit {
        Some(wire::ContentFit::Cover) => ObjectFit::Cover,
        Some(wire::ContentFit::Fill) => ObjectFit::Fill,
        Some(wire::ContentFit::None) => ObjectFit::None,
        Some(wire::ContentFit::ScaleDown) => ObjectFit::ScaleDown,
        Some(wire::ContentFit::Contain) | None => ObjectFit::Contain,
    }
}

fn decode_image(data: &wire::ImageData) -> Option<RenderImage> {
    let mut pixels = match data {
        wire::ImageData::Resource(_) => return None,
        wire::ImageData::Rgba {
            width,
            height,
            pixels,
        } => image::RgbaImage::from_raw(*width, *height, pixels.clone())?,
        wire::ImageData::Encoded(bytes) => {
            let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
                .with_guessed_format()
                .ok()?;
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(8192);
            limits.max_image_height = Some(8192);
            limits.max_alloc = Some(64 << 20);
            reader.limits(limits);
            reader.decode().ok()?.into_rgba8()
        }
    };
    for pixel in pixels.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Some(RenderImage::new(vec![image::Frame::new(pixels)]))
}

fn qr(code: &wire::Qr) -> AnyElement {
    let Some(payload) = &code.payload else {
        return div().into_any_element();
    };
    let correction = match code.correction {
        Some(wire::QrCorrection::Low) => qrcode::EcLevel::L,
        Some(wire::QrCorrection::Quartile) => qrcode::EcLevel::Q,
        Some(wire::QrCorrection::High) => qrcode::EcLevel::H,
        Some(wire::QrCorrection::Medium) | None => qrcode::EcLevel::M,
    };
    let result = match code.version {
        Some(wire::QrVersion::Normal(value)) => qrcode::QrCode::with_version(
            payload,
            qrcode::Version::Normal(i16::from(value)),
            correction,
        ),
        Some(wire::QrVersion::Micro(value)) => qrcode::QrCode::with_version(
            payload,
            qrcode::Version::Micro(i16::from(value)),
            correction,
        ),
        None => qrcode::QrCode::with_error_correction_level(payload, correction),
    };
    let Ok(matrix) = result else {
        return div()
            .child("QR payload exceeds the selected code capacity")
            .into_any_element();
    };
    let modules = matrix.width() + 8;
    let total = match code.size {
        Some(wire::QrSize::Total(size)) => size,
        Some(wire::QrSize::Cell(size)) => size * modules as f32,
        None => 4.0 * modules as f32,
    };
    let cell = code.cell.map(rgba).unwrap_or_else(|| rgb(0).into());
    let background = code
        .background
        .map(rgba)
        .unwrap_or_else(|| rgb(0xffffff).into());
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            window.paint_quad(fill(bounds, background));
            let unit = f32::from(bounds.size.width.min(bounds.size.height)) / modules as f32;
            for y in 0..matrix.width() {
                for x in 0..matrix.width() {
                    if matrix[(x, y)] == qrcode::Color::Dark {
                        let origin = bounds.origin
                            + point(px((x + 4) as f32 * unit), px((y + 4) as f32 * unit));
                        window
                            .paint_quad(fill(Bounds::new(origin, size(px(unit), px(unit))), cell));
                    }
                }
            }
        },
    )
    .w(px(total))
    .h(px(total))
    .into_any_element()
}

fn native_canvas_commands(commands: &[wire::CanvasCommand]) -> bool {
    commands.iter().all(|command| {
        let wire::CanvasCommand::Draw { shape, stroke, .. } = command else {
            return false;
        };
        let solid = stroke.as_ref().is_none_or(|s| s.dash.is_empty());
        let primitive = match shape {
            wire::CanvasShape::Rectangle { radius, .. } => radius.iter().all(|r| *r == radius[0]),
            wire::CanvasShape::Circle { .. } => true,
            wire::CanvasShape::Line { .. } => stroke
                .as_ref()
                .is_none_or(|s| s.cap != wire::CanvasLineCap::Square),
            wire::CanvasShape::Path(_) => false,
        };
        solid && primitive
    })
}

fn paint_canvas_commands(
    commands: &[wire::CanvasCommand],
    origin: Point<Pixels>,
    window: &mut Window,
) {
    for command in commands {
        let wire::CanvasCommand::Draw {
            shape,
            fill: background,
            stroke,
            ..
        } = command
        else {
            continue;
        };
        let position = |p: [f32; 2]| origin + point(px(p[0]), px(p[1]));
        let (bounds, radius) = match shape {
            wire::CanvasShape::Rectangle {
                position: p,
                size: s,
                radius,
            } => (
                Bounds::new(position(*p), size(px(s[0]), px(s[1]))),
                radius[0],
            ),
            wire::CanvasShape::Circle { center, radius } => (
                Bounds::new(
                    position([center[0] - radius, center[1] - radius]),
                    size(px(2. * radius), px(2. * radius)),
                ),
                *radius,
            ),
            wire::CanvasShape::Line { from, to } => {
                if let Some(stroke) = stroke {
                    let mut path = gpui_kit::PathBuilder::stroke(px(stroke.width));
                    path.move_to(position(*from));
                    path.line_to(position(*to));
                    if let Ok(path) = path.build() {
                        window.paint_path(path, rgba(stroke.color));
                    }
                    if stroke.cap == wire::CanvasLineCap::Round {
                        let r = stroke.width / 2.;
                        for p in [from, to] {
                            window.paint_quad(
                                fill(
                                    Bounds::new(
                                        position([p[0] - r, p[1] - r]),
                                        size(px(2. * r), px(2. * r)),
                                    ),
                                    rgba(stroke.color),
                                )
                                .corner_radii(px(r)),
                            );
                        }
                    }
                }
                continue;
            }
            wire::CanvasShape::Path(_) => continue,
        };
        // SVG strokes straddle the geometry; native quad borders are inset.
        let border = stroke.as_ref().map_or(0., |s| s.width);
        let expanded = Bounds::new(
            bounds.origin - point(px(border / 2.), px(border / 2.)),
            bounds.size + size(px(border), px(border)),
        );
        let color = background.map(rgba).unwrap_or_default();
        let border_color = stroke.as_ref().map(|s| rgba(s.color)).unwrap_or_default();
        window.paint_quad(
            fill(expanded, color)
                .corner_radii(px(radius + border / 2.))
                .border_widths(px(border))
                .border_color(border_color),
        );
    }
}

fn svg_color(color: wire::Rgba) -> String {
    let [r, g, b, a] = color.0;
    format!(
        "rgba({},{},{},{a})",
        (r * 255.0) as u8,
        (g * 255.0) as u8,
        (b * 255.0) as u8
    )
}

fn canvas_svg(commands: &[wire::CanvasCommand], width: f32, height: f32) -> Vec<u8> {
    use std::fmt::Write;
    let mut svg =
        format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\">");
    let mut depth = 0;
    for (index, command) in commands.iter().enumerate() {
        match command {
            wire::CanvasCommand::Push {
                translate,
                rotate,
                scale,
                clip,
            } => {
                let _ = write!(
                    svg,
                    "<g transform=\"translate({} {}) rotate({}) scale({} {})\">",
                    translate[0],
                    translate[1],
                    rotate.to_degrees(),
                    scale[0],
                    scale[1]
                );
                if let Some([x, y, w, h]) = clip {
                    let _ = write!(
                        svg,
                        "<defs><clipPath id=\"c{index}\"><rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\"/></clipPath></defs><g clip-path=\"url(#c{index})\">"
                    );
                } else {
                    svg.push_str("<g>");
                }
                depth += 1;
            }
            wire::CanvasCommand::Pop => {
                if depth > 0 {
                    svg.push_str("</g></g>");
                    depth -= 1;
                }
            }
            wire::CanvasCommand::Draw {
                shape,
                fill,
                even_odd,
                stroke,
            } => {
                let path = canvas_path(shape);
                let color = fill.map(svg_color).unwrap_or_else(|| "none".into());
                let rule = match even_odd {
                    true => "evenodd",
                    false => "nonzero",
                };
                let _ = write!(
                    svg,
                    "<path d=\"{path}\" fill=\"{color}\" fill-rule=\"{rule}\""
                );
                if let Some(stroke) = stroke {
                    let cap = match stroke.cap {
                        wire::CanvasLineCap::Butt => "butt",
                        wire::CanvasLineCap::Square => "square",
                        wire::CanvasLineCap::Round => "round",
                    };
                    let join = match stroke.join {
                        wire::CanvasLineJoin::Miter => "miter",
                        wire::CanvasLineJoin::Round => "round",
                        wire::CanvasLineJoin::Bevel => "bevel",
                    };
                    let dash = stroke
                        .dash
                        .iter()
                        .map(f32::to_string)
                        .collect::<Vec<_>>()
                        .join(" ");
                    let _ = write!(
                        svg,
                        " stroke=\"{}\" stroke-width=\"{}\" stroke-linecap=\"{cap}\" stroke-linejoin=\"{join}\" stroke-dasharray=\"{dash}\" stroke-dashoffset=\"{}\"",
                        svg_color(stroke.color),
                        stroke.width,
                        stroke.dash_offset
                    );
                }
                svg.push_str("/>");
            }
        }
    }
    for _ in 0..depth {
        svg.push_str("</g></g>");
    }
    svg.push_str("</svg>");
    svg.into_bytes()
}

fn canvas_path(shape: &wire::CanvasShape) -> String {
    let segments = match shape {
        wire::CanvasShape::Rectangle {
            position,
            size,
            radius,
        } => vec![wire::CanvasSegment::Rectangle {
            position: *position,
            size: *size,
            radius: *radius,
        }],
        wire::CanvasShape::Circle { center, radius } => vec![wire::CanvasSegment::Circle {
            center: *center,
            radius: *radius,
        }],
        wire::CanvasShape::Line { from, to } => vec![
            wire::CanvasSegment::Move(*from),
            wire::CanvasSegment::Line(*to),
        ],
        wire::CanvasShape::Path(segments) => segments.clone(),
    };
    let mut path = String::new();
    let mut cursor = [0.0, 0.0];
    let mut origin = cursor;
    use std::fmt::Write;
    for segment in segments {
        match segment {
            wire::CanvasSegment::Move([x, y]) => {
                let _ = write!(path, "M{x} {y} ");
                cursor = [x, y];
                origin = cursor;
            }
            wire::CanvasSegment::Line([x, y]) => {
                let _ = write!(path, "L{x} {y} ");
                cursor = [x, y];
            }
            wire::CanvasSegment::Close => {
                path.push_str("Z ");
                cursor = origin;
            }
            wire::CanvasSegment::Rectangle {
                position: [x, y],
                size: [w, h],
                radius: [tl, tr, br, bl],
            } => {
                let _ = write!(
                    path,
                    "M{} {y} H{} Q{} {y} {} {} V{} Q{} {} {} {} H{} Q{x} {} {x} {} V{} Q{x} {y} {} {y} Z ",
                    x + tl,
                    x + w - tr,
                    x + w,
                    x + w,
                    y + tr,
                    y + h - br,
                    x + w,
                    y + h,
                    x + w - br,
                    y + h,
                    x + bl,
                    y + h,
                    y + h - bl,
                    y + tl,
                    x + tl
                );
            }
            wire::CanvasSegment::Circle {
                center: [x, y],
                radius,
            } => {
                let _ = write!(
                    path,
                    "M{} {y} a{radius} {radius} 0 1 0 {} 0 a{radius} {radius} 0 1 0 {} 0 Z ",
                    x - radius,
                    radius * 2.0,
                    -radius * 2.0
                );
            }
            wire::CanvasSegment::Bezier { a, b, end } => {
                let _ = write!(
                    path,
                    "C{} {} {} {} {} {} ",
                    a[0], a[1], b[0], b[1], end[0], end[1]
                );
                cursor = end;
            }
            wire::CanvasSegment::Quadratic { control, end } => {
                let _ = write!(
                    path,
                    "Q{} {} {} {} ",
                    control[0], control[1], end[0], end[1]
                );
                cursor = end;
            }
            wire::CanvasSegment::Arc {
                center,
                radius,
                start,
                end,
            } => {
                append_arc(&mut path, center, [radius, radius], 0.0, start, end);
                cursor = [
                    center[0] + radius * end.cos(),
                    center[1] + radius * end.sin(),
                ];
            }
            wire::CanvasSegment::Ellipse {
                center,
                radius,
                rotation,
                start,
                end,
            } => {
                append_arc(&mut path, center, radius, rotation, start, end);
                cursor = [
                    center[0] + radius[0] * end.cos() * rotation.cos()
                        - radius[1] * end.sin() * rotation.sin(),
                    center[1]
                        + radius[0] * end.cos() * rotation.sin()
                        + radius[1] * end.sin() * rotation.cos(),
                ];
            }
            wire::CanvasSegment::ArcTo { a, b, radius } => {
                cursor = append_arc_to(&mut path, cursor, a, b, radius);
            }
        }
    }
    path
}

fn append_arc(
    path: &mut String,
    center: [f32; 2],
    radius: [f32; 2],
    rotation: f32,
    start: f32,
    end: f32,
) {
    use std::fmt::Write;
    let point = |angle: f32| {
        let x = radius[0] * angle.cos();
        let y = radius[1] * angle.sin();
        [
            center[0] + x * rotation.cos() - y * rotation.sin(),
            center[1] + x * rotation.sin() + y * rotation.cos(),
        ]
    };
    let a = point(start);
    let sweep = i32::from(end >= start);
    let _ = write!(path, "M{} {} ", a[0], a[1]);
    let steps = ((end - start).abs() / std::f32::consts::PI)
        .ceil()
        .clamp(1.0, 128.0) as usize;
    for index in 1..=steps {
        let b = point(start + (end - start) * index as f32 / steps as f32);
        let _ = write!(
            path,
            "A{} {} {} 0 {sweep} {} {} ",
            radius[0],
            radius[1],
            rotation.to_degrees(),
            b[0],
            b[1]
        );
    }
}

fn append_arc_to(
    path: &mut String,
    current: [f32; 2],
    a: [f32; 2],
    b: [f32; 2],
    radius: f32,
) -> [f32; 2] {
    use std::fmt::Write;
    let from = [current[0] - a[0], current[1] - a[1]];
    let to = [b[0] - a[0], b[1] - a[1]];
    let first = from[0].hypot(from[1]);
    let second = to[0].hypot(to[1]);
    let cross = from[0] * to[1] - from[1] * to[0];
    let degenerate = radius <= 0.0
        || first < f32::EPSILON
        || second < f32::EPSILON
        || cross.abs() < f32::EPSILON;
    if degenerate {
        let _ = write!(path, "L{} {} ", a[0], a[1]);
        return a;
    }
    let from = [from[0] / first, from[1] / first];
    let to = [to[0] / second, to[1] / second];
    let angle = (from[0] * to[0] + from[1] * to[1]).clamp(-1.0, 1.0).acos();
    let distance = radius / (angle / 2.0).tan();
    let enter = [a[0] + from[0] * distance, a[1] + from[1] * distance];
    let exit = [a[0] + to[0] * distance, a[1] + to[1] * distance];
    let sweep = i32::from(cross < 0.0);
    let _ = write!(
        path,
        "L{} {} A{radius} {radius} 0 0 {sweep} {} {} ",
        enter[0], enter[1], exit[0], exit[1]
    );
    exit
}

#[cfg(test)]
pub(crate) fn assert_released_image_is_not_cached(key: &str) {
    let mut tree = ViewTree::new(wire::Node::empty());
    let resource = wire::ImageData::Resource(key.into());
    tree.remember_image(7, &resource);
    let image = tree.image_frame(7, Some(&resource)).expect("live resource");
    let released = Arc::downgrade(&image);
    drop(image);
    crate::video::forget_peer(key);
    drop(crate::video::take_retired());
    assert!(tree.image_frame(7, Some(&resource)).is_none());
    assert!(
        released.upgrade().is_none(),
        "the renderer must not retain a released resource"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::test::TestWindowExt as _;

    #[gpui_kit::test]
    fn rich_editor_selection_uses_the_contract_and_replaces_a_changed_primitive(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        cx.update(crate::editor::wire::init_notion);
        for key in ["another-app/body", "/pages/document"] {
            let mut root = wire::Node::Editor {
                key: key.into(),
                document: wire::editor_document::EditorDocumentRef {
                    document: key.into(),
                    reset: 1,
                    text_revision: 0,
                    revision: 0,
                    cursor: Default::default(),
                    byte_len: 0,
                },
                on_document: 0,
                editable: true,
                placeholder: String::new(),
                width: None,
                height: None,
                min_height: None,
                max_height: None,
                options: Box::new(wire::EditorOptions {
                    rich: Some(Box::new(wire::editor_rich::RichPresentation {
                        document: wire::editor_rich::RichDocument {
                            blocks: vec![wire::editor_rich::RichBlock {
                                kind: "paragraph".into(),
                                ..Default::default()
                            }],
                            ..Default::default()
                        },
                        ..Default::default()
                    })),
                    ..Default::default()
                }),
            };
            let store = crate::editor::wire::EditorStore::new(99);
            store.replace(&root).unwrap();
            let window = cx.open_window(size(px(400.), px(300.)), |_, cx| {
                let mut tree = ViewTree::new(root.clone());
                tree.set_editor_store(store, cx);
                tree
            });
            let tree = window.root(cx).unwrap();
            let mut native = gpui_kit::VisualTestContext::from_window(window.into(), cx);
            native.update(|window, cx| {
                window.render_frame(cx);
                assert!(matches!(
                    tree.read(cx).editors.get(key).unwrap().view,
                    EditorView::Rich(_)
                ));
                let wire::Node::Editor { options, .. } = &mut root else {
                    unreachable!()
                };
                options.rich = None;
                tree.update(cx, |tree, cx| tree.replace(root.clone(), cx));
                window.render_frame(cx);
                assert!(matches!(
                    tree.read(cx).editors.get(key).unwrap().view,
                    EditorView::Text(_)
                ));
            });
        }
    }

    #[gpui_kit::test]
    fn primitive_canvas_paints_in_the_first_frame_and_after_a_move(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let command = wire::CanvasCommand::Draw {
            shape: wire::CanvasShape::Rectangle {
                position: [10., 10.],
                size: [40., 30.],
                radius: [4.; 4],
            },
            fill: Some(wire::Rgba([1., 0., 0., 1.])),
            stroke: None,
            even_odd: false,
        };
        let node = wire::Node::Canvas {
            key: "canvas".into(),
            width: Some(wire::Length::Fixed(100.)),
            height: Some(wire::Length::Fixed(80.)),
            commands: vec![command.clone()],
        };
        let window = cx.open_window(size(px(100.), px(80.)), |_, _| ViewTree::new(node));
        let tree = window.root(cx).unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                !window.painted_quads().is_empty(),
                "no asynchronous image load needed"
            );
            tree.update(cx, |tree, cx| {
                if let wire::Node::Canvas { commands, .. } = &mut tree.root
                    && let wire::CanvasCommand::Draw {
                        shape: wire::CanvasShape::Rectangle { position, .. },
                        ..
                    } = &mut commands[0]
                {
                    *position = [40., 25.];
                }
                cx.notify();
            });
            window.draw(cx).clear(cx);
            assert!(
                !window.painted_quads().is_empty(),
                "moving keeps visible native geometry"
            );
        })
        .unwrap();
        let mut complex = vec![command];
        complex.push(wire::CanvasCommand::Pop);
        assert!(
            !native_canvas_commands(&complex),
            "transform stacks keep the complete SVG renderer"
        );
    }

    #[gpui_kit::test]
    fn untinted_svg_uses_native_color_decoder_and_retains_pixels_without_resent_bytes(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let bytes = br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24"><path fill="#ff0000" d="M0 0h12v24H0z"/><path fill="#0000ff" d="M12 0h12v24H12z"/></svg>"##.to_vec();
        let image = Arc::new(Image::from_bytes(ImageFormat::Svg, bytes.clone()));
        let node = wire::Node::Svg {
            key: "artwork".into(),
            hash: 42,
            bytes: Some(bytes),
            inherit_button_ink: false,
            label: None,
            color: None,
            hover: None,
            fit: Some(wire::ContentFit::Contain),
            rotation: None,
            opacity: None,
            width: Some(wire::Length::Fixed(24.)),
            height: Some(wire::Length::Fixed(24.)),
        };
        let window = cx.open_window(size(px(80.), px(80.)), |_, _| ViewTree::new(node));
        let tree = window.root(cx).unwrap();
        let mut native = gpui_kit::VisualTestContext::from_window(window.into(), cx);
        native.update(|window, cx| window.render_frame(cx));
        native.run_until_parked();
        native.update(|window, cx| {
            assert!(
                gpui_kit::ImageSource::Image(image.clone()).is_asset_cached(cx),
                "the rendered SVG must request the native full-color image path"
            );
            let rendered = image
                .clone()
                .get_render_image(window, cx)
                .expect("native SVG decoded");
            let pixels = rendered.as_bytes(0).expect("decoded native pixels");
            assert!(
                pixels.chunks_exact(4).any(|p| p == [0, 0, 255, 255]),
                "red survives"
            );
            assert!(
                pixels.chunks_exact(4).any(|p| p == [255, 0, 0, 255]),
                "blue survives"
            );
            tree.update(cx, |tree, cx| {
                let mut next = tree.root.clone();
                if let wire::Node::Svg { bytes, .. } = &mut next {
                    *bytes = None;
                }
                tree.replace(next, cx);
            });
            window.render_frame(cx);
            assert!(
                image.clone().get_render_image(window, cx).is_some(),
                "unchanged artwork remains available when a wire patch omits bytes"
            );
        });
    }

    #[gpui_kit::test]
    fn container_focus_is_native_and_handoff_never_reuses_retired_handles(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        struct Host {
            tree: Entity<ViewTree>,
            keys: std::rc::Rc<std::cell::Cell<usize>>,
        }
        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let keys = self.keys.clone();
                div()
                    .capture_key_down(move |_, _, cx| {
                        keys.set(keys.get() + 1);
                        cx.stop_propagation();
                    })
                    .child(self.tree.clone())
            }
        }
        cx.update(gpui_kit::init);
        let menu = || {
            ducktape_view_guest::kit::column(
                "menu",
                [ducktape_view_guest::kit::text(
                    "label",
                    "A real menu, without an input",
                )],
            )
        };
        let keys = std::rc::Rc::new(std::cell::Cell::new(0));
        let window = cx.open_window(size(px(500.), px(300.)), |_, cx| Host {
            tree: cx.new(|_| ViewTree::new(menu())),
            keys: keys.clone(),
        });
        let host = window.root(cx).unwrap();
        let mut native = gpui_kit::VisualTestContext::from_window(window.into(), cx);
        native.update(|window, cx| window.render_frame(cx));
        let tree = host.read_with(&native, |host, _| host.tree.clone());
        native.update(|window, cx| {
            tree.update(cx, |tree, cx| {
                assert!(tree.fields.is_empty());
                tree.execute_widget_command(
                    wire::WidgetCommand::Focus {
                        target: "menu".into(),
                    },
                    window,
                    cx,
                )
                .unwrap();
            })
        });
        native.update(|window, cx| window.render_frame(cx));
        let retired = native.update(|window, cx| {
            tree.read_with(cx, |tree, cx| {
                assert!(tree.target_focused("menu", window, cx));
                tree.focus_targets["menu"].1.clone()
            })
        });
        native.update(|window, cx| {
            window.dispatch_keystroke(gpui_kit::Keystroke::parse("escape").unwrap(), cx)
        });
        assert_eq!(
            keys.get(),
            1,
            "focused menu participates in native key capture"
        );
        native.update(|window, cx| {
            let saved = tree.read_with(cx, |tree, cx| tree.presentation(window, cx));
            host.update(cx, |host, cx| {
                host.tree = cx.new(|_| ViewTree::new(menu()).with_presentation(saved));
                cx.notify();
            });
        });
        native.update(|window, cx| window.render_frame(cx));
        let replacement = host.read_with(&native, |host, _| host.tree.clone());
        native.update(|window, cx| {
            replacement.read_with(cx, |tree, cx| {
                assert!(tree.target_focused("menu", window, cx));
                assert!(
                    !retired.is_focused(window),
                    "handoff uses a fresh native handle"
                );
            })
        });
        native.update(|window, cx| {
            window.dispatch_keystroke(gpui_kit::Keystroke::parse("escape").unwrap(), cx)
        });
        assert_eq!(keys.get(), 2);
        native.update(|window, cx| {
            replacement.update(cx, |tree, cx| {
                tree.replace(ducktape_view_guest::kit::text("closed", "menu closed"), cx);
                assert!(tree.focus_targets.is_empty());
                assert!(!tree.target_focused("menu", window, cx));
            })
        });
        native.update(|window, cx| window.render_frame(cx));
        native.update(|window, cx| {
            retired.focus(window, cx);
            window.dispatch_keystroke(gpui_kit::Keystroke::parse("escape").unwrap(), cx);
        });
        assert_eq!(
            keys.get(),
            2,
            "retired menu has no current native dispatch path"
        );
    }

    #[gpui_kit::test]
    fn text_respects_parent_width_and_keeps_nowrap_inside_its_box(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let text = |key: &str, content: String, width, wrapping| wire::Node::Text {
            key: key.into(),
            content,
            width,
            size: Some(14.),
            color: None,
            font: Default::default(),
            align_x: None,
            options: wire::TextOptions {
                wrapping,
                ..Default::default()
            },
        };
        let paragraph = text(
            "paragraph",
            "A long description with words that must wrap within the available parent width. "
                .repeat(8),
            None,
            None,
        );
        let row = wire::Node::Linear {
            key: "row".into(),
            axis: wire::Axis::Row,
            spacing: Some(8.),
            padding: None,
            width: Some(wire::Length::Fill),
            height: Some(wire::Length::Fill),
            background: None,
            border: None,
            align: None,
            max_width: None,
            clip: false,
            wrap: None,
            children: vec![
                text("height", "17968".into(), None, Some(wire::Wrapping::None)),
                text(
                    "hash",
                    "0123456789abcdef".repeat(16),
                    Some(wire::Length::Fill),
                    Some(wire::Wrapping::WordOrGlyph),
                ),
                text("count", "12 ops".into(), None, Some(wire::Wrapping::None)),
            ],
        };
        let mut header = row.clone();
        if let wire::Node::Linear { children, .. } = &mut header {
            *children = vec![
                text("label", "Pages".into(), None, Some(wire::Wrapping::None)),
                wire::Node::Space {
                    width: Some(wire::Length::Fill),
                    height: None,
                },
                text(
                    "actions",
                    "New page".into(),
                    Some(wire::Length::Fixed(100.)),
                    Some(wire::Wrapping::None),
                ),
            ];
        }
        let mut reference = row.clone();
        if let wire::Node::Linear {
            children, height, ..
        } = &mut reference
        {
            *height = None;
            *children = vec![text(
                "reference",
                "Pages".into(),
                None,
                Some(wire::Wrapping::None),
            )];
        }
        let root = wire::Node::Linear {
            key: "column".into(),
            axis: wire::Axis::Column,
            spacing: Some(8.),
            padding: None,
            width: Some(wire::Length::Fill),
            height: None,
            background: None,
            border: None,
            align: None,
            max_width: Some(620.),
            clip: false,
            wrap: None,
            children: vec![paragraph, row, reference, header],
        };
        let root = wire::Node::Button {
            key: "wrapping-parent".into(),
            content: wire::ButtonContent::Child(Box::new(root)),
            label: None,
            checked: None,
            expanded: None,
            description: None,
            on_press: Some(1),
            width: Some(wire::Length::Fill),
            height: None,
            padding: None,
            style: Default::default(),
        };
        let window = cx.open_window(size(px(800.), px(500.)), |_, _| ViewTree::new(root));
        let tree = window.root(cx).unwrap();
        let mut native = gpui_kit::VisualTestContext::from_window(window.into(), cx);
        native.update(|window, cx| window.render_frame(cx));
        let button_bounds = native.update(|window, _| window.find("wrapping-parent").bounds());
        tree.read_with(&native, |tree, _| {
            let paragraph = tree.measured_bounds("paragraph").unwrap();
            let hash = tree.measured_bounds("hash").unwrap();
            assert!(
                button_bounds.bottom() >= hash.bottom(),
                "auto-height button must show every wrapped line"
            );
            let count = tree.measured_bounds("count").unwrap();
            assert!(tree.measured_bounds("height").unwrap().right() <= hash.left());
            assert_eq!(
                tree.measured_bounds("label").unwrap().size.width,
                tree.measured_bounds("reference").unwrap().size.width,
                "intrinsic labels cannot lose letters to a Fill spacer"
            );
            assert!(paragraph.size.width <= px(620.));
            assert!(
                paragraph.size.height > hash.size.height,
                "default wrapping creates multiple lines"
            );
            assert!(hash.right() <= count.left());
            assert!(
                hash.size.height > tree.measured_bounds("reference").unwrap().size.height,
                "WordOrGlyph must override a native Button's inherited nowrap"
            );
            assert!(count.right() <= paragraph.left() + px(620.));
        });
        let mut nowrap = text_options(
            div(),
            Default::default(),
            None,
            &wire::TextOptions {
                wrapping: Some(wire::Wrapping::None),
                ..Default::default()
            },
        );
        assert_eq!(nowrap.style().overflow.x, Some(gpui_kit::Overflow::Hidden));
        assert!(
            nowrap.text_style().text_overflow.is_some(),
            "the native text shaper must truncate glyphs, not only the containing box"
        );
    }

    #[gpui_kit::test]
    fn horizontal_overflow_scrollbar_reveals_offscreen_columns(cx: &mut gpui_kit::TestAppContext) {
        use ducktape_view_guest::kit;
        use gpui_kit::InputEvent as _;
        cx.update(gpui_kit::init);
        let columns = kit::sized(
            kit::row(
                "columns",
                (0..4).map(|index| {
                    kit::sized(
                        kit::container(
                            format!("column-{index}"),
                            kit::text(format!("name-{index}"), "Folder"),
                        ),
                        Some(wire::Length::Fixed(230.)),
                        Some(wire::Length::Fill),
                    )
                }),
            ),
            Some(wire::Length::Fixed(920.)),
            Some(wire::Length::Fill),
        );
        let mut root = kit::scroll("folders", kit::spaced(columns, 0.));
        if let wire::Node::Scroll { direction, .. } = &mut root {
            *direction = wire::ScrollDirection::Horizontal;
        }
        let root = kit::sized(
            kit::column(
                "main",
                [
                    kit::sized(
                        kit::container("header", kit::text("header-text", "Header")),
                        None,
                        Some(wire::Length::Fixed(24.)),
                    ),
                    root,
                    kit::sized(
                        kit::container("footer", kit::text("footer-text", "Footer")),
                        None,
                        Some(wire::Length::Fixed(24.)),
                    ),
                ],
            ),
            Some(wire::Length::Fill),
            Some(wire::Length::Fill),
        );
        let root = kit::spaced(root, 0.);
        let window = cx.open_window(size(px(400.), px(200.)), |_, _| ViewTree::new(root));
        let tree = window.root(cx).unwrap();
        let mut native = gpui_kit::VisualTestContext::from_window(window.into(), cx);
        native.update(|window, cx| window.render_frame(cx));
        tree.read_with(&native, |tree, _| {
            assert_eq!(tree.scrolls["folders"].max_offset().x, px(520.));
            assert_eq!(tree.scrolls["folders"].bounds().size.height, px(152.));
            assert_eq!(
                tree.measured_bounds("column-0").unwrap().size.width,
                px(230.)
            );
        });
        native.update(|window, cx| window.render_frame(cx));
        let position = point(px(350.), px(168.));
        native.update(|window, cx| {
            window.dispatch_event(
                MouseDownEvent {
                    position,
                    button: MouseButton::Left,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                MouseUpEvent {
                    position,
                    button: MouseButton::Left,
                    modifiers: Default::default(),
                    click_count: 1,
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
        });
        tree.read_with(&native, |tree, _| {
            assert!(
                tree.scrolls["folders"].offset().x < px(-200.),
                "scrollbar track click reveals offscreen columns: bounds={:?}, offset={:?}",
                tree.scrolls["folders"].bounds(),
                tree.scrolls["folders"].offset()
            );
            assert_eq!(tree.scrolls["folders"].offset().y, px(0.));
        });
    }

    #[gpui_kit::test]
    fn sensor_preserves_linear_fill_bounds(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let root = wire::Node::Sensor {
            key: "viewport".into(),
            reset: None,
            on_show: Some(1),
            on_resize: None,
            on_hide: None,
            anticipate: None,
            delay: None,
            child: Box::new(wire::Node::Linear {
                key: "content".into(),
                axis: wire::Axis::Column,
                width: Some(wire::Length::Fill),
                height: Some(wire::Length::Fill),
                max_width: None,
                clip: false,
                wrap: None,
                spacing: None,
                padding: None,
                align: None,
                background: None,
                border: None,
                children: vec![wire::Node::Space {
                    width: Some(wire::Length::Fixed(20.)),
                    height: Some(wire::Length::Fixed(5.)),
                }],
            }),
        };
        let window = cx.open_window(size(px(400.), px(300.)), |_, _| ViewTree::new(root));
        let tree = window.root(cx).unwrap();
        let mut native = gpui_kit::VisualTestContext::from_window(window.into(), cx);
        native.update(|window, cx| window.render_frame(cx));
        tree.read_with(&native, |tree, _| {
            assert_eq!(
                tree.sensors["viewport"].size,
                Some(size(px(400.), px(300.)))
            );
        });
    }

    #[gpui_kit::test]
    fn combo_search_reset_and_routes_use_fresh_native_state(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let combo = |reset, handler| wire::Node::ComboBox {
            key: "combo".into(),
            state_key: "choices".into(),
            reset,
            options: vec!["Alpha".into(), "Beta".into()],
            selected: None,
            placeholder: "Choose".into(),
            on_select: handler,
            width: Some(wire::Length::Fixed(200.)),
            settings: Box::new(wire::ComboOptions {
                input: Some(44),
                ..Default::default()
            }),
        };
        let window = cx.open_window(size(px(400.), px(200.)), |_, _| ViewTree::new(combo(0, 7)));
        let tree = window.root(cx).unwrap();
        let mut native = gpui_kit::VisualTestContext::from_window(window.into(), cx);
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let observed = events.clone();
        let _subscription = native.update(|_, cx| {
            cx.subscribe(&tree, move |_, event: &wire::Event, _| {
                observed.borrow_mut().push(event.clone())
            })
        });
        native.update(|window, cx| window.render_frame(cx));
        let old = tree.read_with(&native, |tree, _| tree.pickers["combo"].state.clone());
        native.update(|window, cx| window.within("combo").click("input", cx));
        native.update(|window, cx| window.render_frame(cx));
        native.simulate_input("Be");
        native.run_until_parked();
        assert!(
            events.borrow().iter().any(
                |event| matches!(event, wire::Event::Input { handler: 44, text } if text == "Be")
            ),
            "native search routes its query: {:?}",
            events.borrow()
        );
        let mut authoritative = combo(0, 7);
        if let wire::Node::ComboBox { selected, .. } = &mut authoritative {
            *selected = Some(0);
        }
        tree.update(&mut native, |tree, cx| tree.replace(authoritative, cx));
        native.update(|window, cx| window.render_frame(cx));
        assert_eq!(
            old.read_with(&native, |state, _| state.selected_value().copied()),
            Some(0),
            "authoritative Alpha is not filtered menu row zero (Beta)"
        );
        assert!(
            events.borrow().iter().any(|event| matches!(event,
            wire::Event::Input { handler: 44, text } if text.is_empty())),
            "native authoritative projection clears its search query"
        );
        native.update(|window, cx| {
            let mut pending = combo(0, 7);
            if let wire::Node::ComboBox { selected, .. } = &mut pending {
                *selected = Some(1);
            }
            tree.update(cx, |tree, cx| tree.replace(pending, cx));
            window.render_frame(cx);
            tree.update(cx, |tree, cx| tree.replace(combo(1, 99), cx));
            window.render_frame(cx);
        });
        assert_eq!(
            old.read_with(&native, |state, _| state.selected_value().copied()),
            Some(0),
            "a deferred projection cannot mutate a retired native picker"
        );
        let fresh = tree.read_with(&native, |tree, _| {
            assert_eq!(tree.measured_bounds("combo").unwrap().size.width, px(200.));
            tree.pickers["combo"].state.clone()
        });
        assert_ne!(old.entity_id(), fresh.entity_id());
        events.borrow_mut().clear();
        old.update(&mut native, |_, cx| cx.emit(SelectEvent::Confirm(Some(0))));
        assert!(
            events.borrow().is_empty(),
            "retired picker cannot emit a fresh-frame route"
        );
        fresh.update(&mut native, |_, cx| cx.emit(SelectEvent::Confirm(Some(1))));
        assert!(events.borrow().iter().any(|event| matches!(
            event,
            wire::Event::Select {
                handler: 99,
                index: 1
            }
        )));
    }

    #[gpui_kit::test]
    fn sensor_visibility_uses_current_routes_and_removal_does_not_replay_old_ids(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::test::TestWindowExt as _;
        let sensor = |y, hide| wire::Node::Flex {
            key: "host".into(),
            layout: Default::default(),
            background: None,
            border: None,
            items: Vec::new(),
            children: vec![wire::Node::Pin {
                key: "position".into(),
                x: 0.,
                y,
                width: Some(wire::Length::Fill),
                height: Some(wire::Length::Fill),
                content: Box::new(wire::Node::Sensor {
                    key: "watched".into(),
                    reset: None,
                    on_show: None,
                    on_resize: None,
                    on_hide: Some(hide),
                    anticipate: None,
                    delay: None,
                    child: Box::new(wire::Node::Space {
                        width: Some(wire::Length::Fixed(20.)),
                        height: Some(wire::Length::Fixed(20.)),
                    }),
                }),
            }],
        };
        cx.update(gpui_kit::init);
        let window = cx.open_window(size(px(100.), px(100.)), |_, _| {
            ViewTree::new(sensor(0., 11))
        });
        let tree = window.root(cx).unwrap();
        let mut native = gpui_kit::VisualTestContext::from_window(window.into(), cx);
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let observed = events.clone();
        let _subscription = native.update(|_, cx| {
            cx.subscribe(&tree, move |_, event: &wire::Event, _| {
                observed.borrow_mut().push(event.clone())
            })
        });
        native.update(|window, cx| window.render_frame(cx));
        tree.read_with(&native, |tree, _| {
            assert!(tree.sensors["watched"].size.is_some())
        });
        tree.update(&mut native, |tree, cx| {
            tree.replace(sensor(200., 23), cx);
            assert_eq!(
                tree.sensors["watched"].on_hide,
                Some(23),
                "routes refresh before native draw or delayed measurement"
            );
        });
        native.update(|window, cx| window.render_frame(cx));
        assert_eq!(
            &*events.borrow(),
            &[wire::Event::Message(23)],
            "mounted viewport exit uses the new frame route"
        );
        tree.update(&mut native, |tree, cx| tree.replace(sensor(0., 11), cx));
        native.update(|window, cx| window.render_frame(cx));
        events.borrow_mut().clear();
        tree.update(&mut native, |tree, cx| {
            tree.replace(
                wire::Node::Button {
                    key: "watched".into(),
                    content: wire::ButtonContent::Label("New action".into()),
                    label: None,
                    checked: None,
                    expanded: None,
                    description: None,
                    on_press: Some(11),
                    width: None,
                    height: None,
                    padding: None,
                    style: Default::default(),
                },
                cx,
            );
            assert!(
                tree.sensors.is_empty(),
                "reusing a key for another widget does not retain its sensor"
            );
        });
        native.update(|window, cx| window.render_frame(cx));
        assert!(
            events.borrow().is_empty(),
            "old hide ID 11 must not invoke the replacement action 11"
        );
        tree.update(&mut native, |tree, _| {
            assert!(tree.take_user_activation(&wire::Event::Message(11)).is_none(),
                "a generated event cannot grant device authority");
        });
        native.update(|window, cx| window.click("watched", cx));
        tree.update(&mut native, |tree, _| {
            assert!(tree.take_user_activation(&wire::Event::Message(11)).is_some());
            assert!(tree.take_user_activation(&wire::Event::Message(11)).is_none(),
                "one native click authorizes at most one session");
        });
        assert_eq!(
            &*events.borrow(),
            &[wire::Event::Message(11)],
            "only a real new-frame click activates the replacement action"
        );
    }

    #[gpui_kit::test]
    fn editor_obeys_authored_size_and_height_limits(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        for (height, minimum, maximum, expected) in [
            (Some(wire::Length::Fixed(60.)), None, None, 60.),
            (Some(wire::Length::Fixed(60.)), Some(100.), None, 100.),
            (Some(wire::Length::Fixed(180.)), None, Some(100.), 100.),
            (None, None, None, 300.),
        ] {
            let root = wire::Node::Editor {
                key: "document".into(),
                options: Box::default(),
                placeholder: String::new(),
                document: wire::editor_document::EditorDocumentRef {
                    document: "sizing".into(),
                    reset: 1,
                    text_revision: 0,
                    revision: 0,
                    cursor: Default::default(),
                    byte_len: 0,
                },
                on_document: 0,
                editable: false,
                width: Some(240.),
                height,
                min_height: minimum,
                max_height: maximum,
            };
            let store = crate::editor::wire::EditorStore::new(91);
            store.replace(&root).unwrap();
            let window = cx.open_window(size(px(400.), px(300.)), |_, cx| {
                let mut tree = ViewTree::new(root);
                tree.set_editor_store(store, cx);
                tree
            });
            let tree = window.root(cx).unwrap();
            let mut native = gpui_kit::VisualTestContext::from_window(window.into(), cx);
            native.update(|window, cx| window.render_frame(cx));
            let bounds = tree
                .read_with(&native, |tree, _| tree.measured_bounds("document"))
                .unwrap();
            assert_eq!(bounds.size, size(px(240.), px(expected)));
        }
    }

    #[test]
    fn geometry_and_pixels_keep_the_wire_meaning() {
        let mut path = String::new();
        let end = append_arc_to(&mut path, [0.0, 0.0], [10.0, 0.0], [10.0, 10.0], 2.0);
        assert!((end[0] - 10.0).abs() < 0.001 && (end[1] - 2.0).abs() < 0.001);
        assert!(path.contains("A2 2"));
        path.clear();
        append_arc(
            &mut path,
            [10.0, 10.0],
            [5.0, 5.0],
            0.0,
            0.0,
            std::f32::consts::TAU,
        );
        assert_eq!(path.as_str().matches('A').count(), 2);
        let pixels = wire::ImageData::Rgba {
            width: 1,
            height: 1,
            pixels: vec![255, 10, 20, 255],
        };
        let image = decode_image(&pixels).expect("one valid pixel");
        assert_eq!(image.as_bytes(0), Some([20, 10, 255, 255].as_slice()));
        assert!(decode_image(&wire::ImageData::Encoded(vec![1, 2, 3])).is_none());
    }
}
