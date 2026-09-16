//! Native rich block rendering over the guest-owned editor transaction lane.
//! Block snapshots and toolbar tags cross the wire without interpreting the
//! application's canonical document format.
use super::EditorStore;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Bounds, Context, Entity, EventEmitter, Focusable as _,
    InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _, Pixels, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription,
    Window, canvas, div, img, px,
};
use gpui_notion::NotionEditor;
use gpui_notion::editor::input_rules::InputRuleMode;
use gpui_notion::editor::comments::{AnnotationMode, AnnotationRequested};
use gpui_notion::editor::block::{
    BlockAttrs, BlockCaps, BlockContent, BlockContext, BlockLayout, BlockRegistry, BlockSpec, types,
};
use gpui_notion::editor::mark::{HighlightColor, Mark, MarkKind, MarkList, TextColor};
use gpui_notion::editor::theme::{ActiveEditorTheme as _, EditorTheme};
use gpui_notion::editor::toolbar::{ToolbarAction, ToolbarItem};
use gpui_notion::editor::slash::{ApplicationMenu, ApplicationMenuAnchor, MenuAction};
use gpui_notion::editor::view::{Caret, DocumentChanged, LinkPressed, SelectionChanged};
use view_wire as wire;
use wire::editor_presentation::EditorMargin;

/// Register gpui-notion after `gpui_kit::init`. The guest already sizes and
/// pads the document column, so the editor's own page column is flush with
/// it: no width cap, a short tail under the last block, and exactly the
/// gutter's width of side padding — the mount pulls the editor out by that
/// much on both sides (see `Render`), so the text lands on the guest column
/// and the hover "+ ⠿" controls hang in the guest's left padding.
pub fn init(cx: &mut App) {
    gpui_notion::editor::init(cx);
    gpui_notion::editor::EditorTheme::customize(cx, |theme, _| {
        theme.page_width = px(f32::MAX);
        theme.page_padding = theme.gutter_controls_width;
        theme.page_bottom = theme.rem * 4.;
    });
    // Over the library's own image block, which draws its `src` with `img` —
    // and no image loader can fetch a `duck://` address.
    BlockRegistry::register(cx, DocumentImage);
    // A block the library has no notion of: a page inside this one.
    BlockRegistry::register(cx, DocumentPage);
}

/// A subpage, on the line the writer made it on. Its text is the page's own
/// title — editing it renames the page — and the guest marks the whole line
/// as a link into that page, so pressing it opens the page through the same
/// plane every other document link uses.
struct DocumentPage;

impl BlockSpec for DocumentPage {
    fn type_name(&self) -> &'static str {
        "page"
    }

    fn label(&self, _: &BlockAttrs) -> SharedString {
        "Page".into()
    }

    fn caps(&self) -> BlockCaps {
        BlockCaps {
            // A page title is a name, not prose: no bold, no input rules
            // turning "1. " into a list inside it. The link the guest puts
            // over the whole line survives regardless — it is drawn, never
            // typed.
            marks: false,
            input_rules: false,
            ..BlockCaps::default()
        }
    }

    /// A marker slot of its own, or `render_leading` draws into nothing.
    fn layout(&self, _: &BlockAttrs, theme: &EditorTheme) -> BlockLayout {
        BlockLayout {
            leading_width: theme.marker_width,
            ..BlockLayout::new(theme)
        }
    }

    fn placeholder(&self, _: &BlockAttrs) -> SharedString {
        "Untitled".into()
    }

    /// A page with no name still reads as a page, focused or not.
    fn placeholder_always(&self) -> bool {
        true
    }

    fn render_leading(
        &self,
        ctx: &BlockContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        // Drawn, not a glyph: the bundled font carries no page character, and
        // one that falls back renders as an empty box or nothing at all. Two
        // ruled lines inside, or a bare outline reads as an unticked to-do.
        let ink = cx.theme().muted_foreground;
        let rule = || div().w(px(5.)).h(px(1.)).bg(ink);
        let sheet = div()
            .w(px(10.))
            .h(px(13.))
            .rounded(px(2.))
            .border_1()
            .border_color(ink)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(2.))
            .child(rule())
            .child(rule());
        Some(
            div()
                .w(ctx.leading_width)
                .h(ctx.line_height)
                .flex()
                .items_center()
                .child(sheet)
                .into_any_element(),
        )
    }
}

/// How tall a picture may draw. A page is read top to bottom, so a portrait
/// photo that filled the window would push the next paragraph off the screen.
const PICTURE_HEIGHT: Pixels = px(480.);

/// The document's image block: gpui-notion's, with the one change that a
/// `duck://files/…` address draws from the host picture store. A picture in a
/// page is a file on the network, not on the writer's disk — the guest puts
/// it there and then asks `picture.load` for every address its page names, so
/// what this draws is already decoded.
struct DocumentImage;

impl BlockSpec for DocumentImage {
    fn type_name(&self) -> &'static str {
        types::IMAGE
    }

    fn label(&self, _: &BlockAttrs) -> SharedString {
        "Image".into()
    }

    fn caps(&self) -> BlockCaps {
        BlockCaps::atom()
    }

    fn layout(&self, _: &BlockAttrs, theme: &EditorTheme) -> BlockLayout {
        BlockLayout {
            margin_top: theme.section_gap,
            margin_bottom: theme.section_gap,
            ..BlockLayout::new(theme)
        }
    }

    fn render_body(&self, ctx: &BlockContext, _: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let framed = |picture: AnyElement| {
            let frame = div().w_full();
            let frame = match ctx.selected {
                true => frame.border_2().border_color(cx.theme().primary),
                false => frame,
            };
            frame.child(picture).into_any_element()
        };
        let Some(src) = ctx.attrs.src.clone() else {
            return Some(plate(ctx, cx, "Add a picture with /image").into_any_element());
        };
        let Some(path) = duckfs_path(&src) else {
            // A picture off the web: the loader fetches it and it is the one
            // that knows the dimensions, so the two bounds are all this can
            // say about the size.
            return Some(framed(
                img(src.to_string())
                    .max_w_full()
                    .max_h(PICTURE_HEIGHT)
                    .object_fit(ObjectFit::Contain)
                    .into_any_element(),
            ));
        };
        // Until the guest's `picture.load` lands, the block holds its place
        // rather than collapsing the text around it.
        let Some(picture) = crate::backend::stored_picture(crate::backend::PAGES_SURFACE, path)
        else {
            return Some(plate(ctx, cx, "Loading the picture…").into_any_element());
        };
        Some(framed(drawn(&picture)))
    }
}

/// The picture at its own size, so a small one is not blown up to the column:
/// BOTH dimensions are stated, because an image given only one takes its size
/// from the aspect ratio gpui hands it and overflows whatever box it is in.
/// The bounds then keep it inside the column and off the next paragraph, and
/// `Contain` keeps a bounded one in proportion.
fn drawn(picture: &crate::backend::Picture) -> AnyElement {
    img(picture.source())
        .w(px(picture.width as f32))
        .h(px(picture.height as f32))
        .max_w_full()
        .max_h(PICTURE_HEIGHT)
        .object_fit(ObjectFit::Contain)
        .into_any_element()
}

/// The dashed box an image block draws while it has no picture to draw.
fn plate(ctx: &BlockContext, cx: &App, say: &'static str) -> impl IntoElement {
    div()
        .w_full()
        .h(ctx.theme.rems(7.5))
        .rounded(ctx.theme.radius)
        .border_1()
        .border_dashed()
        .border_color(match ctx.selected {
            true => cx.theme().primary,
            false => cx.theme().border,
        })
        .flex()
        .items_center()
        .justify_center()
        .text_color(cx.theme().muted_foreground)
        .child(say)
}

/// The duckfs path behind a picture's address, or `None` when the address is
/// not one of ours — a picture off the web, or a path on somebody's disk.
fn duckfs_path(src: &str) -> Option<&str> {
    let path = src.strip_prefix("duck://files")?;
    let plain = !path.is_empty() && !path.contains(['?', '#']);
    plain.then_some(path)
}

/// The badge's height: one marker slot.
const BADGE_HEIGHT: f32 = 22.;

pub struct RichWireEditor {
    key: String,
    store: EditorStore,
    editor: Entity<NotionEditor>,
    installed: wire::editor_rich::RichDocument,
    reset: Option<u64>,
    fault: Option<String>,
    /// Where the mount painted last frame, so block bounds (window space)
    /// can be turned into overlay offsets.
    bounds: Option<Bounds<Pixels>>,
    /// Whether the mount takes the box it was given or the room its blocks
    /// need; see [`RichWireEditor::set_fills`].
    fills: bool,
    /// Guest-authored margin badges indexed by rich block.
    margins: Vec<EditorMargin>,
    menu: Option<wire::editor_presentation::EditorMenu>,
    _menu_actions: Subscription,
    _changes: Subscription,
    _selection: Subscription,
    _actions: Subscription,
    _annotations: Subscription,
    _links: Subscription,
}

impl EventEmitter<()> for RichWireEditor {}

impl RichWireEditor {
    pub fn new(
        key: String,
        store: EditorStore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = NotionEditor::new(window, cx);
            editor.set_annotation_mode(AnnotationMode::External);
            editor.set_application_menu(None, cx);
            editor.set_input_rule_mode(InputRuleMode::Application);
            editor
        });
        let changes = cx.subscribe_in(
            &editor,
            window,
            |this, _, _: &DocumentChanged, window, cx| this.changed(String::new(), window, cx),
        );
        let selection = cx.subscribe_in(
            &editor,
            window,
            |this, _, _: &SelectionChanged, window, cx| this.changed(String::new(), window, cx),
        );
        let actions = cx.subscribe_in(
            &editor,
            window,
            |this, _, action: &ToolbarAction, window, cx| {
                this.changed(action.tag.to_string(), window, cx)
            },
        );
        let annotations = cx.subscribe_in(
            &editor,
            window,
            |this, _, event: &AnnotationRequested, _, cx| this.annotation(event, cx),
        );
        let menu_actions =
            cx.subscribe_in(&editor, window, |this, _, action: &MenuAction, _, cx| {
                this.menu_action(action, cx);
            });
        // A link in a page is as often `duck://page/…` as it is the web, and
        // the app already knows what every `duck://` address names — so a
        // press goes to the one place that routes them all.
        let links = cx.subscribe(&editor, |_, _, pressed: &LinkPressed, _| {
            crate::shell::open_link(pressed.0.to_string())
        });
        let mut this = Self {
            key,
            store,
            editor,
            installed: Default::default(),
            reset: None,
            fault: None,
            bounds: None,
            fills: true,
            margins: Vec::new(),
            menu: None,
            _menu_actions: menu_actions,
            _changes: changes,
            _selection: selection,
            _actions: actions,
            _annotations: annotations,
            _links: links,
        };
        this.sync(window, cx);
        this
    }

    /// Whether the mount takes the box it was given or the room its blocks
    /// need. Set from the node's height: a mount that always asked for all of
    /// its parent's height gave a shrinking box nothing to shrink to.
    pub fn set_fills(&mut self, fills: bool, cx: &mut Context<Self>) {
        if self.fills == fills {
            return;
        }
        self.fills = fills;
        cx.notify();
    }

    /// Install the projection when it settled on text this editor did not
    /// produce (another writer, a guest normalization, a page switch).
    pub fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(projection) = self.store.projection(&self.key) else {
            return;
        };
        if let Some(fault) = projection.fault.as_deref() {
            self.note_fault(Some(fault));
            return;
        }
        let margins = projection
            .options
            .presentation
            .as_ref()
            .map(|paint| paint.affordances.margins.clone())
            .unwrap_or_default();
        if margins != self.margins {
            self.margins = margins;
            cx.notify();
        }
        let Some(rich) = projection.options.rich.as_ref() else {
            return;
        };
        let menu = projection
            .options
            .presentation
            .as_ref()
            .and_then(|paint| paint.affordances.menu.clone());
        if self.menu != menu {
            let native = menu.as_ref().map(|menu| ApplicationMenu {
                anchor: match menu.anchor {
                    wire::editor_presentation::EditorMenuAnchor::Caret => {
                        ApplicationMenuAnchor::Caret
                    }
                    wire::editor_presentation::EditorMenuAnchor::Line(line) => {
                        ApplicationMenuAnchor::Block(line as usize)
                    }
                },
                items: menu
                    .items
                    .iter()
                    .map(|item| ToolbarItem {
                        tag: item.tag.clone().into(),
                        label: item.label.clone().into(),
                    })
                    .collect(),
                selected: menu.selected as usize,
            });
            self.editor
                .update(cx, |editor, cx| editor.set_application_menu(native, cx));
            self.menu = menu;
        }
        self.editor.update(cx, |editor, cx| {
            editor.set_toolbar(
                Some(
                    rich.toolbar
                        .iter()
                        .map(|item| ToolbarItem {
                            tag: item.tag.clone().into(),
                            label: item.label.clone().into(),
                        })
                        .collect(),
                ),
                cx,
            )
        });
        // The guest's projection is installed only after its canonical text is
        // available and the existing transaction queue has settled.
        if projection.text.is_none() {
            return;
        }
        let reset = self.reset != Some(projection.reference.reset);
        let install = reset || (!projection.pending && rich.document != self.installed);
        if !install {
            return;
        }
        if let Err(error) = validate_rich(&rich.document) {
            self.note_fault(Some(error));
            return;
        }
        self.note_fault(None);
        self.installed = rich.document.clone();
        self.reset = Some(projection.reference.reset);
        let content = self
            .installed
            .blocks
            .iter()
            .map(|block| native_block(block).expect("validated native rich block"))
            .collect::<Vec<_>>();
        let restore = self.is_focused(window, cx).then_some(self.installed.cursor);
        self.editor.update(cx, |editor, cx| {
            let changed = editor.content() != content;
            if changed {
                while let Some(id) = editor.block_id_at(0) {
                    editor.remove_block(id, cx);
                }
                for (ix, block) in content.into_iter().enumerate() {
                    editor.insert_block(ix, block, window, cx);
                }
            }
            if let Some(cursor) = restore {
                restore_cursor(editor, cursor, window, cx);
            }
        });
        cx.notify();
    }

    /// A store fault stops every editor on the document; say so once.
    fn note_fault(&mut self, fault: Option<&str>) {
        if fault == self.fault.as_deref() {
            return;
        }
        if let Some(fault) = fault {
            tracing::warn!(target: "ducktape::app", fault, "the notion editor store faulted");
        }
        self.fault = fault.map(str::to_owned);
    }

    fn snapshot(&self, cx: &App) -> Result<wire::editor_rich::RichDocument, &'static str> {
        let editor = self.editor.read(cx);
        // A newly installed, unfocused input has a local caret at zero.
        // Only a focused input can replace the guest's supplied cursor.
        let cursor = editor
            .focused_id()
            .and_then(|_| editor.selection(cx))
            .and_then(|(id, range)| {
                let index = editor.index_of(id)?;
                let caret = editor.caret_offset(id, cx).unwrap_or(range.end);
                let anchor = if caret == range.start {
                    range.end
                } else {
                    range.start
                };
                Some(wire::EditorCursor {
                    position: wire::EditorPosition {
                        line: index as u32,
                        column: caret as u32,
                    },
                    selection: (!range.is_empty()).then_some(wire::EditorPosition {
                        line: index as u32,
                        column: anchor as u32,
                    }),
                })
            })
            .unwrap_or(self.installed.cursor);
        Ok(wire::editor_rich::RichDocument {
            blocks: editor
                .content()
                .iter()
                .map(wire_block)
                .collect::<Result<Vec<_>, _>>()?,
            cursor,
        })
    }

    fn menu_action(&mut self, action: &MenuAction, cx: &mut Context<Self>) {
        use wire::editor_presentation::EditorInteraction;
        let interaction = match action {
            MenuAction::Select(index) => EditorInteraction::MenuSelect {
                index: *index as u32,
            },
            MenuAction::Pick(tag) => EditorInteraction::MenuPick {
                tag: tag.to_string(),
            },
            MenuAction::Dismiss => EditorInteraction::MenuDismiss,
            MenuAction::Open(trigger) => EditorInteraction::Action {
                tag: trigger.to_string(),
            },
        };
        let document = match self.snapshot(cx) {
            Ok(document) => document,
            Err(error) => {
                self.note_fault(Some(error));
                return;
            }
        };
        self.store.request(
            &self.key,
            wire::EditorRequestInput::RichEdit {
                edit: Box::new(wire::editor_rich::RichEdit {
                    before: Some(self.installed.clone()),
                    document,
                    action: String::new(),
                    interaction: Some(interaction),
                }),
            },
        );
        cx.emit(());
        cx.notify();
    }

    fn changed(&mut self, action: String, _window: &mut Window, cx: &mut Context<Self>) {
        let document = match self.snapshot(cx) {
            Ok(document) => document,
            Err(error) => {
                self.note_fault(Some(error));
                return;
            }
        };
        let unchanged = document == self.installed && action.is_empty();
        if unchanged {
            return;
        }
        if let Err(error) = document.validate() {
            self.note_fault(Some(error));
            return;
        }
        self.store.request(
            &self.key,
            wire::EditorRequestInput::RichEdit {
                edit: Box::new(wire::editor_rich::RichEdit {
                    before: Some(self.installed.clone()),
                    document: document.clone(),
                    action,
                    interaction: None,
                }),
            },
        );
        self.installed = document;
        cx.emit(());
        cx.notify();
    }


    fn annotation(&mut self, event: &AnnotationRequested, cx: &mut Context<Self>) {
        let Some(line) = self.editor.read(cx).index_of(event.block) else {
            return;
        };
        let mut document = match self.snapshot(cx) {
            Ok(document) => document,
            Err(error) => {
                self.note_fault(Some(error));
                return;
            }
        };
        document.cursor = wire::EditorCursor {
            position: wire::EditorPosition {
                line: line as u32,
                column: event.range.end as u32,
            },
            selection: Some(wire::EditorPosition {
                line: line as u32,
                column: event.range.start as u32,
            }),
        };
        self.store.request(
            &self.key,
            wire::EditorRequestInput::RichEdit {
                edit: Box::new(wire::editor_rich::RichEdit {
                    before: Some(self.installed.clone()),
                    document,
                    action: String::new(),
                    interaction: Some(wire::editor_presentation::EditorInteraction::Margin {
                        line: line as u32,
                    }),
                }),
            },
        );
        cx.emit(());
        cx.notify();
    }

    fn margin(&mut self, line: u32, cx: &mut Context<Self>) {
        let mut document = match self.snapshot(cx) {
            Ok(document) => document,
            Err(error) => {
                self.note_fault(Some(error));
                return;
            }
        };
        document.cursor = wire::EditorCursor {
            position: wire::EditorPosition { line, column: 0 },
            selection: None,
        };
        self.store.request(
            &self.key,
            wire::EditorRequestInput::RichEdit {
                edit: Box::new(wire::editor_rich::RichEdit {
                    before: Some(self.installed.clone()),
                    document,
                    action: String::new(),
                    interaction: Some(wire::editor_presentation::EditorInteraction::Margin {
                        line,
                    }),
                }),
            },
        );
        cx.emit(());
        cx.notify();
    }

    /// One badge per commented block, on the block's last line at the text
    /// column's right edge; pressing it opens the guest's card for the block.
    fn badges(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(origin) = self.bounds.map(|bounds| bounds.origin) else {
            return Vec::new();
        };
        let editor = self.editor.read(cx);
        let theme = cx.editor_theme().clone();
        self.margins
            .iter()
            .filter_map(|margin| {
                let line = margin.line as usize;
                let ix = line;
                let bounds = editor.block_bounds(editor.block_id_at(ix)?)?;
                let top = bounds.bottom() - px(BADGE_HEIGHT) - origin.y;
                let line = margin.line;
                Some(
                    div()
                        .id(("margin", line as usize))
                        .absolute()
                        .right(px(0.))
                        .top(top)
                        .h(px(BADGE_HEIGHT))
                        .px(theme.rems(0.375))
                        .flex()
                        .items_center()
                        .gap(theme.rems(0.25))
                        .rounded(theme.radius_sm)
                        .bg(theme.comment_fill)
                        .text_color(theme.comment_accent)
                        .text_size(theme.ui_small_text_size)
                        .cursor_pointer()
                        .child(gpui_notion::editor::ui::icon(
                            "message-square",
                            theme.ui_small_text_size,
                            theme.comment_accent,
                        ))
                        .child(margin.count.to_string())
                        .on_click(cx.listener(move |this, _, _, cx| this.margin(line, cx)))
                        .into_any_element(),
                )
            })
            .collect()
    }

    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.editor
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx)
    }

    pub fn widget_command(
        &mut self,
        command: &wire::WidgetCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !matches!(command, wire::WidgetCommand::Focus { .. }) {
            return false;
        }
        if self.is_focused(window, cx) {
            return true;
        }
        // The caret goes to the block the document cursor names, never to a
        // trailing paragraph the editor would have to insert: focusing a page
        // must not write to it.
        let line = self.installed.cursor.position.line as usize;
        let column = self.installed.cursor.position.column as usize;
        self.editor.update(cx, |editor, cx| {
            let last = editor.block_count().saturating_sub(1);
            let Some(id) = editor.block_id_at(line.min(last)) else {
                return;
            };
            editor.focus_block(id, Caret::At(column), window, cx);
        });
        true
    }
}

impl Render for RichWireEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The editor is pulled out of the mount by its gutter width on both
        // sides and pads itself back in by the same amount (`init`): its text
        // column is the mount's box, and the gutter controls hang to the left
        // of it, in the guest's own padding.
        let gutter = cx.editor_theme().gutter_controls_width;
        let weak = cx.entity().downgrade();
        let probe = canvas(
            move |bounds, _, cx| {
                let _ = weak.update(cx, |this, cx| {
                    if this.bounds != Some(bounds) {
                        this.bounds = Some(bounds);
                        cx.notify();
                    }
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        let badges = self.badges(cx);
        let fills = self.fills;
        div()
            .w_full()
            .when(fills, |element| element.h_full())
            .relative()
            .flex()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .when(fills, |element| element.h_full())
                    .ml(-gutter)
                    .mr(-gutter)
                    .child(self.editor.clone()),
            )
            .child(probe)
            .children(badges)
    }
}

pub(super) fn validate_rich(
    document: &wire::editor_rich::RichDocument,
) -> Result<(), &'static str> {
    document.validate()?;
    for block in &document.blocks {
        native_block(block)?;
    }
    Ok(())
}

fn supported_block(kind: &str) -> Result<(), &'static str> {
    match kind {
        "paragraph" | "heading" | "bulletList" | "orderedList" | "taskList" | "blockquote"
        | "codeBlock" | "horizontalRule" | "callout" | "details" | "image" | "page" | "table" => {
            Ok(())
        }
        _ => Err("unsupported rich block kind"),
    }
}

fn native_block(block: &wire::editor_rich::RichBlock) -> Result<BlockContent, &'static str> {
    supported_block(&block.kind)?;
    let marks = block
        .marks
        .iter()
        .map(|mark| {
            let value = mark.value.as_str();
            let kind = match mark.kind.as_str() {
                "bold" | "italic" | "underline" | "strike" | "code" | "superscript"
                | "subscript" => {
                    if !value.is_empty() {
                        return Err("unsupported rich mark value");
                    }
                    match mark.kind.as_str() {
                        "bold" => MarkKind::Bold,
                        "italic" => MarkKind::Italic,
                        "underline" => MarkKind::Underline,
                        "strike" => MarkKind::Strike,
                        "code" => MarkKind::Code,
                        "superscript" => MarkKind::Superscript,
                        "subscript" => MarkKind::Subscript,
                        _ => unreachable!(),
                    }
                }
                "highlight" => MarkKind::Highlight(if value.is_empty() {
                    None
                } else {
                    Some(
                        HighlightColor::ALL
                            .into_iter()
                            .find(|color| color.label() == value)
                            .ok_or("unsupported rich highlight color")?,
                    )
                }),
                "textStyle" => MarkKind::TextColor(
                    TextColor::ALL
                        .into_iter()
                        .find(|color| color.label() == value)
                        .ok_or("unsupported rich text color")?,
                ),
                "link" => MarkKind::Link(mark.value.clone().into()),
                "mention" => MarkKind::Mention(mark.value.clone().into()),
                _ => return Err("unsupported rich mark kind"),
            };
            Ok(Mark::new(kind, mark.start as usize..mark.end as usize))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut attrs = BlockAttrs {
        level: block.level,
        checked: block.checked,
        language: (!block.language.is_empty()).then(|| block.language.clone().into()),
        ..Default::default()
    };
    for attribute in &block.attributes {
        match attribute.name.as_str() {
            "collapsed" => {
                attrs.collapsed = match attribute.value.as_str() {
                    "true" => true,
                    _ => return Err("unsupported rich collapsed value"),
                }
            }
            "start" => {
                let start = attribute
                    .value
                    .parse::<u32>()
                    .map_err(|_| "unsupported rich list start")?;
                if start.to_string() != attribute.value {
                    return Err("noncanonical rich list start");
                }
                attrs.start = Some(start as usize);
            }
            "src" => attrs.src = Some(attribute.value.clone().into()),
            "alt" => attrs.alt = Some(attribute.value.clone().into()),
            "emoji" => attrs.emoji = Some(attribute.value.clone().into()),
            name => {
                let Some(name) = name.strip_prefix("extra:") else {
                    return Err("unsupported rich block attribute");
                };
                attrs
                    .extra
                    .insert(name.to_owned().into(), attribute.value.clone().into());
            }
        }
    }
    Ok(BlockContent::new(block.kind.clone(), block.text.clone())
        .with_indent(block.indent as usize)
        .with_attrs(attrs)
        .with_marks(MarkList::from_marks(marks)))
}

fn wire_block(block: &BlockContent) -> Result<wire::editor_rich::RichBlock, &'static str> {
    supported_block(&block.ty)?;
    let mut attributes = Vec::new();
    let mut add = |name: &str, value: String| {
        attributes.push(wire::editor_rich::RichAttribute {
            name: name.into(),
            value,
        })
    };
    if block.attrs.collapsed {
        add("collapsed", "true".into());
    }
    if let Some(start) = block.attrs.start {
        add("start", start.to_string());
    }
    if let Some(value) = &block.attrs.src {
        add("src", value.to_string());
    }
    if let Some(value) = &block.attrs.alt {
        add("alt", value.to_string());
    }
    if let Some(value) = &block.attrs.emoji {
        add("emoji", value.to_string());
    }
    for (name, value) in &block.attrs.extra {
        add(&format!("extra:{name}"), value.to_string());
    }
    attributes.sort_by(|a, b| a.name.cmp(&b.name));
    let marks = block
        .marks
        .iter()
        .map(|mark| {
            let value = match &mark.kind {
                MarkKind::Link(value) | MarkKind::Mention(value) => value.to_string(),
                MarkKind::Highlight(Some(color)) => color.label().into(),
                MarkKind::TextColor(color) => color.label().into(),
                MarkKind::Comment(_) => return Err("unsupported rich mark kind"),
                _ => String::new(),
            };
            Ok(wire::editor_rich::RichMark {
                start: mark.range.start as u32,
                end: mark.range.end as u32,
                kind: mark.kind.type_name().into(),
                value,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(wire::editor_rich::RichBlock {
        kind: block.ty.to_string(),
        text: block.text.clone(),
        indent: block.indent as u32,
        level: block.attrs.level,
        checked: block.attrs.checked,
        language: block
            .attrs
            .language
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default(),
        marks,
        attributes,
    })
}

fn restore_cursor(
    editor: &mut NotionEditor,
    cursor: wire::EditorCursor,
    window: &mut Window,
    cx: &mut Context<NotionEditor>,
) {
    let index = cursor.position.line as usize;
    let Some(id) = editor.block_id_at(index) else {
        return;
    };
    let Some(anchor) = cursor
        .selection
        .filter(|anchor| anchor.line == cursor.position.line)
    else {
        editor.focus_block(id, Caret::At(cursor.position.column as usize), window, cx);
        return;
    };
    let from = anchor.column.min(cursor.position.column) as usize;
    let to = anchor.column.max(cursor.position.column) as usize;
    editor.select_text_in_block(index, from..to, window, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui_kit::test]
    fn rich_application_menu_returns_opaque_choice_through_the_document_queue(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        application_menu_request(cx, MenuGesture::Pick);
    }

    #[gpui_kit::test]
    fn rich_application_menu_escape_uses_the_document_queue(cx: &mut gpui_kit::TestAppContext) {
        application_menu_request(cx, MenuGesture::Dismiss);
    }

    #[gpui_kit::test]
    fn rich_application_menu_receives_caret_movement(cx: &mut gpui_kit::TestAppContext) {
        application_menu_request(cx, MenuGesture::Move);
    }

    #[gpui_kit::test]
    fn rich_application_menu_uses_the_guest_line_anchor(cx: &mut gpui_kit::TestAppContext) {
        application_menu_request(cx, MenuGesture::LinePick);
    }

    #[gpui_kit::test]
    fn rich_application_menu_can_anchor_to_a_divider(cx: &mut gpui_kit::TestAppContext) {
        application_menu_request(cx, MenuGesture::DividerPick);
    }

    #[derive(Clone, Copy)]
    enum MenuGesture {
        DividerPick,
        LinePick,
        Pick,
        Dismiss,
        Move,
    }

    fn application_menu_request(cx: &mut gpui_kit::TestAppContext, gesture: MenuGesture) {
        use gpui_kit::test::TestWindowExt as _;
        use wire::editor_presentation::{
            EditorMenu, EditorMenuAnchor, EditorMenuItem, EditorPresentation, EditorInteraction,
        };
        cx.update(gpui_kit::init);
        cx.update(init);
        let store = EditorStore::new(93);
        let source = match gesture {
            MenuGesture::LinePick => "@\nsecond",
            MenuGesture::DividerPick => "@\n---",
            _ => "@",
        };
        let reference = wire::editor_document::EditorDocumentRef {
            document: "application-document".into(),
            reset: 1,
            revision: 0,
            text_revision: 0,
            byte_len: source.len() as u32,
            cursor: wire::EditorCursor {
                position: wire::EditorPosition { line: 0, column: 1 },
                selection: None,
            },
        };
        let mut paint = EditorPresentation::default();
        paint.affordances.menu = Some(EditorMenu {
            anchor: match gesture {
                MenuGesture::LinePick | MenuGesture::DividerPick => EditorMenuAnchor::Line(1),
                _ => EditorMenuAnchor::Caret,
            },
            items: vec![EditorMenuItem {
                tag: "opaque-choice".into(),
                label: "Application person".into(),
            }],
            selected: 0,
        });
        let rich = wire::editor_rich::RichPresentation {
            document: wire::editor_rich::RichDocument {
                blocks: source
                    .lines()
                    .map(|text| match text {
                        "---" => wire::editor_rich::RichBlock {
                            kind: "horizontalRule".into(),
                            ..Default::default()
                        },
                        _ => wire::editor_rich::RichBlock {
                            kind: "paragraph".into(),
                            text: text.into(),
                            ..Default::default()
                        },
                    })
                    .collect(),
                cursor: reference.cursor,
            },
            ..Default::default()
        };
        {
            let mut locked = store.lock();
            locked.fields.insert(
                "editor".into(),
                crate::editor::wire::Field {
                    reference: reference.clone(),
                    handler: 1,
                    editable: true,
                    placeholder: String::new(),
                    options: wire::EditorOptions {
                        presentation: Some(Box::new(paint)),
                        rich: Some(Box::new(rich)),
                        binding: Some(Box::new(wire::EditorBinding {
                            authored: true,
                            on_request: 2,
                            on_event: 3,
                            claims: Vec::new(),
                        })),
                        ..Default::default()
                    },
                },
            );
            locked.documents.insert(
                reference.document.clone(),
                crate::editor::wire::Document {
                    reference,
                    text: Some(std::sync::Arc::from(source)),
                    queue: Default::default(),
                    queued_bytes: 0,
                    phase: crate::editor::wire::Phase::Ready,
                },
            );
        }
        let window = cx.open_window(gpui_kit::size(px(600.), px(400.)), |window, cx| {
            RichWireEditor::new("editor".into(), store.clone(), window, cx)
        });
        let editor = window.root(cx).unwrap();
        let mut native = gpui_kit::VisualTestContext::from_window(window.into(), cx);
        native.update(|window, cx| {
            window.render_frame(cx);
            let child = editor.read(cx).editor.clone();
            child.update(cx, |child, cx| {
                let id = child.block_id_at(0).unwrap();
                child.focus_block(id, Caret::End, window, cx);
            });
            window.render_frame(cx);
            assert!(
                child.read(cx).suggestion_is_open(),
                "the application supplied the menu"
            );
        });
        native.run_until_parked();
        let initial = store.drain();
        assert!(
            initial.is_empty(),
            "initial projection must settle without an edit: {initial:?}"
        );
        native.update(|window, cx| {
            window.render_frame(cx);
            match gesture {
                MenuGesture::LinePick | MenuGesture::DividerPick => {
                    let menu = window.find(("application-suggestion", 0usize)).bounds();
                    let block = window.find(("block", 3usize)).bounds();
                    assert!(
                        menu.top() >= block.bottom(),
                        "menu {menu:?} must follow the supplied block {block:?}"
                    );
                    window.click(("application-suggestion", 0usize), cx);
                }
                MenuGesture::Pick => window.click(("application-suggestion", 0usize), cx),
                MenuGesture::Dismiss => window.press("escape", cx),
                MenuGesture::Move => window.press("left", cx),
            }
        });
        native.run_until_parked();
        let requests = store.drain();
        let expected = match gesture {
            MenuGesture::Pick | MenuGesture::LinePick | MenuGesture::DividerPick => {
                Some(EditorInteraction::MenuPick {
                    tag: "opaque-choice".into(),
                })
            }
            MenuGesture::Dismiss => Some(EditorInteraction::MenuDismiss),
            MenuGesture::Move => None,
        };
        assert!(
            requests.iter().any(
                |event| matches!(event, wire::Event::EditorRequest { request, .. }
            if matches!(&request.input, wire::EditorRequestInput::RichEdit { edit }
                if edit.interaction == expected && (expected.is_some() || edit.document.cursor.position.column == 0)))
            ),
            "menu choice must use the canonical document queue"
        );
        assert_eq!(
            store.lock().documents["application-document"]
                .text
                .as_deref(),
            Some(source)
        );
    }

    #[test]
    fn native_attributes_round_trip_through_the_rich_projection() {
        let mut block = BlockContent::new("details", "한글").with_attrs(BlockAttrs {
            collapsed: true,
            start: Some(42),
            src: Some("uri".into()),
            alt: Some("alt".into()),
            emoji: Some("🙂".into()),
            ..Default::default()
        });
        block
            .attrs
            .extra
            .insert("table-json".into(), "{cells:[]}".into());
        assert_eq!(native_block(&wire_block(&block).unwrap()).unwrap(), block);
        let mut wire = wire_block(&block).unwrap();
        for (name, value) in [("collapsed", "false"), ("start", "0002"), ("unknown", "")] {
            wire.attributes = vec![wire::editor_rich::RichAttribute {
                name: name.into(),
                value: value.into(),
            }];
            assert!(native_block(&wire).is_err());
        }
    }

    #[test]
    fn unsupported_rich_primitives_are_rejected() {
        let mut block = wire::editor_rich::RichBlock {
            kind: "unknown".into(),
            text: "abc".into(),
            ..Default::default()
        };
        assert!(native_block(&block).is_err());
        block.kind = "paragraph".into();
        for (kind, value) in [
            ("unknown", ""),
            ("bold", "payload"),
            ("highlight", "unknown"),
            ("textStyle", ""),
            ("comment", "42"),
        ] {
            block.marks = vec![wire::editor_rich::RichMark {
                start: 0,
                end: 3,
                kind: kind.into(),
                value: value.into(),
            }];
            assert!(native_block(&block).is_err(), "{kind}:{value}");
        }
        for kind in [
            "paragraph",
            "heading",
            "bulletList",
            "orderedList",
            "taskList",
            "blockquote",
            "codeBlock",
            "horizontalRule",
            "callout",
            "details",
        ] {
            block.kind = kind.into();
            block.marks.clear();
            assert_eq!(wire_block(&native_block(&block).unwrap()).unwrap(), block);
        }
    }

    #[test]
    fn rich_primitive_marks_round_trip_without_losing_payloads() {
        let mut marks = vec![
            ("bold", ""),
            ("italic", ""),
            ("underline", ""),
            ("strike", ""),
            ("code", ""),
            ("superscript", ""),
            ("subscript", ""),
            ("link", "https://example.test/a"),
            ("mention", "account:42"),
            ("highlight", ""),
        ];
        marks.extend(
            gpui_notion::editor::mark::HighlightColor::ALL
                .iter()
                .map(|color| ("highlight", color.label())),
        );
        marks.extend(
            gpui_notion::editor::mark::TextColor::ALL
                .iter()
                .map(|color| ("textStyle", color.label())),
        );
        for (kind, value) in marks {
            let block = wire::editor_rich::RichBlock {
                kind: "paragraph".into(),
                text: "한글".into(),
                marks: vec![wire::editor_rich::RichMark {
                    start: 0,
                    end: 6,
                    kind: kind.into(),
                    value: value.into(),
                }],
                ..Default::default()
            };
            assert_eq!(
                wire_block(&native_block(&block).unwrap()).unwrap(),
                block,
                "{kind}:{value}"
            );
        }
    }
}
