//! The built-in node types.
//!
//! Each is a [`BlockSpec`]; the registry holds them in the order registered,
//! which is the order the slash menu lists them. An application adds a node
//! type the same way: implement [`BlockSpec`], call
//! [`BlockRegistry::register`].

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Editor;
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::{ActiveTheme, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, img, px, relative,
};

use gpui_kit::TestSupportExt as _;

use super::block::{
    BlockAttrs, BlockCaps, BlockContext, BlockInputRule, BlockLayout, BlockRegistry, BlockSpec,
    BlockType, SlashItem, leading_slot, types,
};
use super::theme::EditorTheme;

/// Register the node types the Notion-like editor ships with.
pub fn init(cx: &mut App) {
    cx.set_global(BlockRegistry::new(std::sync::Arc::new(Paragraph)));
    BlockRegistry::register(cx, Heading);
    BlockRegistry::register(cx, BulletList);
    BlockRegistry::register(cx, OrderedList);
    BlockRegistry::register(cx, TaskList);
    BlockRegistry::register(cx, Blockquote);
    BlockRegistry::register(cx, CodeBlock);
    BlockRegistry::register(cx, HorizontalRule);
    BlockRegistry::register(cx, Image);
    BlockRegistry::register(cx, Callout);
    BlockRegistry::register(cx, Toggle);
    BlockRegistry::register(cx, Table);
}

// ------------------------------------------------------------------ paragraph

pub struct Paragraph;

impl BlockSpec for Paragraph {
    fn type_name(&self) -> &'static str {
        types::PARAGRAPH
    }

    fn label(&self, _: &BlockAttrs) -> SharedString {
        "Text".into()
    }

    fn placeholder(&self, _: &BlockAttrs) -> SharedString {
        "Write, type '/' for commands…".into()
    }

    fn slash_items(&self) -> Vec<SlashItem> {
        vec![SlashItem {
            title: "Text",
            subtext: "Regular text paragraph",
            keywords: &["p", "paragraph", "text"],
            group: "Style",
            icon: "type",
            run: |editor, window, cx| editor.set_paragraph(window, cx),
        }]
    }
}

// -------------------------------------------------------------------- heading

pub struct Heading;

impl BlockSpec for Heading {
    fn type_name(&self) -> &'static str {
        types::HEADING
    }

    fn label(&self, attrs: &BlockAttrs) -> SharedString {
        format!("Heading {}", attrs.level.max(1)).into()
    }

    fn layout(&self, attrs: &BlockAttrs, theme: &EditorTheme) -> BlockLayout {
        // The space above a heading is relative to the heading's own size, as
        // it is in the template, so the hierarchy survives a change of base.
        let heading = theme.heading(attrs.level);
        BlockLayout {
            text_size: heading.size,
            font_weight: heading.weight,
            line_height: heading.line_height,
            margin_top: heading.size * heading.margin_above,
            margin_bottom: theme.rems(0.125),
            ..BlockLayout::new(theme)
        }
    }

    fn placeholder(&self, attrs: &BlockAttrs) -> SharedString {
        format!("Heading {}", attrs.level.max(1)).into()
    }

    fn placeholder_always(&self) -> bool {
        true
    }

    fn split_into(&self, attrs: &BlockAttrs, at_end: bool) -> (BlockType, BlockAttrs) {
        if at_end {
            (types::PARAGRAPH.into(), BlockAttrs::default())
        } else {
            (types::HEADING.into(), attrs.clone())
        }
    }

    fn input_rules(&self) -> Vec<BlockInputRule> {
        vec![BlockInputRule {
            pattern: r"^(#{1,6})\s$",
            build: |caps| {
                let level = caps.get(1)?.as_str().len() as u8;
                Some((types::HEADING.into(), BlockAttrs::level(level)))
            },
        }]
    }

    fn slash_items(&self) -> Vec<SlashItem> {
        vec![
            SlashItem {
                title: "Heading 1",
                subtext: "Top-level heading",
                keywords: &["h", "heading1", "h1"],
                group: "Style",
                icon: "heading-1",
                run: |editor, window, cx| editor.toggle_heading(1, window, cx),
            },
            SlashItem {
                title: "Heading 2",
                subtext: "Key section heading",
                keywords: &["h2", "heading2", "subheading"],
                group: "Style",
                icon: "heading-2",
                run: |editor, window, cx| editor.toggle_heading(2, window, cx),
            },
            SlashItem {
                title: "Heading 3",
                subtext: "Subsection and group heading",
                keywords: &["h3", "heading3", "subheading"],
                group: "Style",
                icon: "heading-3",
                run: |editor, window, cx| editor.toggle_heading(3, window, cx),
            },
        ]
    }
}

// ----------------------------------------------------------------- list items

fn list_layout(theme: &EditorTheme) -> BlockLayout {
    BlockLayout {
        // A list gets air above it; items inside it sit tight together,
        // which `collapse_with_siblings` takes care of.
        margin_top: theme.section_gap,
        leading_width: theme.marker_width,
        collapse_with_siblings: true,
        ..BlockLayout::new(theme)
    }
}

pub struct BulletList;

impl BlockSpec for BulletList {
    fn type_name(&self) -> &'static str {
        types::BULLET_LIST
    }

    fn label(&self, _: &BlockAttrs) -> SharedString {
        "Bullet List".into()
    }

    fn caps(&self) -> BlockCaps {
        BlockCaps::list()
    }

    fn layout(&self, _: &BlockAttrs, theme: &EditorTheme) -> BlockLayout {
        list_layout(theme)
    }

    fn placeholder(&self, _: &BlockAttrs) -> SharedString {
        "List".into()
    }

    fn split_into(&self, _: &BlockAttrs, _: bool) -> (BlockType, BlockAttrs) {
        (types::BULLET_LIST.into(), BlockAttrs::default())
    }

    fn render_leading(&self, ctx: &BlockContext, _: &mut Window, cx: &mut App) -> Option<AnyElement> {
        // disc → circle → square, cycling every three levels.
        let glyph = match ctx.indent % 3 {
            0 => "•",
            1 => "◦",
            _ => "▪",
        };
        Some(leading_slot(
            ctx,
            div()
                .w_full()
                .text_size(ctx.theme.text_size)
                .text_color(cx.theme().foreground)
                .child(glyph),
        ))
    }

    fn input_rules(&self) -> Vec<BlockInputRule> {
        vec![BlockInputRule {
            pattern: r"^\s*([-+*])\s$",
            build: |_| Some((types::BULLET_LIST.into(), BlockAttrs::default())),
        }]
    }

    fn slash_items(&self) -> Vec<SlashItem> {
        vec![SlashItem {
            title: "Bullet List",
            subtext: "List with unordered items",
            keywords: &["ul", "li", "list", "bulletlist", "bullet list"],
            group: "Style",
            icon: "list",
            run: |editor, window, cx| editor.toggle_bullet_list(window, cx),
        }]
    }
}

pub struct OrderedList;

impl BlockSpec for OrderedList {
    fn type_name(&self) -> &'static str {
        types::ORDERED_LIST
    }

    fn label(&self, _: &BlockAttrs) -> SharedString {
        "Numbered List".into()
    }

    fn caps(&self) -> BlockCaps {
        BlockCaps::list()
    }

    fn layout(&self, _: &BlockAttrs, theme: &EditorTheme) -> BlockLayout {
        list_layout(theme)
    }

    fn placeholder(&self, _: &BlockAttrs) -> SharedString {
        "List".into()
    }

    fn split_into(&self, _: &BlockAttrs, _: bool) -> (BlockType, BlockAttrs) {
        (types::ORDERED_LIST.into(), BlockAttrs::default())
    }

    fn render_leading(&self, ctx: &BlockContext, _: &mut Window, cx: &mut App) -> Option<AnyElement> {
        // decimal → lower-alpha → lower-roman, cycling every three levels.
        let n = ctx.ordinal.max(1);
        let label = match ctx.indent % 3 {
            0 => format!("{n}."),
            1 => format!("{}.", alpha_ordinal(n)),
            _ => format!("{}.", roman_ordinal(n)),
        };
        Some(leading_slot(
            ctx,
            div()
                .w_full()
                .text_size(ctx.theme.table_text_size)
                .text_color(cx.theme().foreground)
                .child(label),
        ))
    }

    fn input_rules(&self) -> Vec<BlockInputRule> {
        vec![BlockInputRule {
            pattern: r"^(\d+)\.\s$",
            build: |caps| {
                let start = caps.get(1)?.as_str().parse().ok();
                Some((
                    types::ORDERED_LIST.into(),
                    BlockAttrs {
                        start,
                        ..Default::default()
                    },
                ))
            },
        }]
    }

    fn slash_items(&self) -> Vec<SlashItem> {
        vec![SlashItem {
            title: "Numbered List",
            subtext: "List with ordered items",
            keywords: &["ol", "li", "list", "numberedlist", "numbered list"],
            group: "Style",
            icon: "list-ordered",
            run: |editor, window, cx| editor.toggle_ordered_list(window, cx),
        }]
    }
}

fn alpha_ordinal(n: usize) -> String {
    let mut n = n;
    let mut out = String::new();
    while n > 0 {
        let rem = (n - 1) % 26;
        out.insert(0, (b'a' + rem as u8) as char);
        n = (n - 1) / 26;
    }
    out
}

fn roman_ordinal(n: usize) -> String {
    const TABLE: [(usize, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut n = n;
    let mut out = String::new();
    for (value, glyph) in TABLE {
        while n >= value {
            out.push_str(glyph);
            n -= value;
        }
    }
    out
}

pub struct TaskList;

impl BlockSpec for TaskList {
    fn type_name(&self) -> &'static str {
        types::TASK_LIST
    }

    fn label(&self, _: &BlockAttrs) -> SharedString {
        "To-do list".into()
    }

    fn caps(&self) -> BlockCaps {
        BlockCaps::list()
    }

    fn layout(&self, attrs: &BlockAttrs, theme: &EditorTheme) -> BlockLayout {
        BlockLayout {
            // A checked item is dimmed and struck through.
            text_opacity: if attrs.checked { 0.5 } else { 1.0 },
            strikethrough: attrs.checked,
            ..list_layout(theme)
        }
    }

    fn placeholder(&self, _: &BlockAttrs) -> SharedString {
        "To-do".into()
    }

    fn split_into(&self, _: &BlockAttrs, _: bool) -> (BlockType, BlockAttrs) {
        (types::TASK_LIST.into(), BlockAttrs::default())
    }

    fn render_leading(&self, ctx: &BlockContext, _: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let checked = ctx.attrs.checked;
        let id = ctx.id;
        let editor = ctx.editor.clone();
        let theme = cx.theme();
        let (bg, border_color) = if checked {
            (theme.primary, theme.primary)
        } else {
            (gpui_kit::transparent_black(), theme.border)
        };

        Some(leading_slot(
            ctx,
            div()
                .id(("check", id.0 as usize))
                .size(ctx.theme.text_size)
                .rounded(ctx.theme.radius_sm)
                .border_1()
                .border_color(border_color)
                .bg(bg)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .when(checked, |this| {
                    this.child(
                        div()
                            .text_size(ctx.theme.rems(0.6875))
                            .text_color(cx.theme().primary_foreground)
                            .child("✓"),
                    )
                })
                .on_click(move |_, _window, cx| {
                    let _ = editor.update(cx, |editor, cx| editor.toggle_check(id, cx));
                }),
        ))
    }

    fn input_rules(&self) -> Vec<BlockInputRule> {
        vec![BlockInputRule {
            pattern: r"^\s*\[([ xX])?\]\s$",
            build: |caps| {
                let checked = caps
                    .get(1)
                    .map(|m| m.as_str().eq_ignore_ascii_case("x"))
                    .unwrap_or(false);
                Some((
                    types::TASK_LIST.into(),
                    BlockAttrs {
                        checked,
                        ..Default::default()
                    },
                ))
            },
        }]
    }

    fn slash_items(&self) -> Vec<SlashItem> {
        vec![SlashItem {
            title: "To-do list",
            subtext: "List with tasks",
            keywords: &["tasklist", "task list", "todo", "checklist"],
            group: "Style",
            icon: "list-todo",
            run: |editor, window, cx| editor.toggle_task_list(window, cx),
        }]
    }
}

// ----------------------------------------------------------------- blockquote

pub struct Blockquote;

impl BlockSpec for Blockquote {
    fn type_name(&self) -> &'static str {
        types::BLOCKQUOTE
    }

    fn label(&self, _: &BlockAttrs) -> SharedString {
        "Blockquote".into()
    }

    fn caps(&self) -> BlockCaps {
        BlockCaps {
            lifts_when_empty: true,
            ..Default::default()
        }
    }

    fn layout(&self, _: &BlockAttrs, theme: &EditorTheme) -> BlockLayout {
        BlockLayout {
            margin_top: theme.section_gap,
            margin_bottom: px(0.),
            inner_padding: theme.block_padding,
            collapse_with_siblings: true,
            ..BlockLayout::new(theme)
        }
    }

    fn placeholder(&self, _: &BlockAttrs) -> SharedString {
        "Empty quote".into()
    }

    fn wrap(
        &self,
        ctx: &BlockContext,
        content: AnyElement,
        _: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        let theme = &ctx.theme;
        h_flex()
            .w_full()
            .items_stretch()
            .py(theme.rems(0.375))
            .child(
                div()
                    .w(theme.rems(0.25))
                    .flex_none()
                    .rounded(theme.radius_sm)
                    .bg(theme.quote_bar),
            )
            .child(div().flex_1().pl(theme.block_padding).child(content))
            .into_any_element()
    }

    fn input_rules(&self) -> Vec<BlockInputRule> {
        vec![BlockInputRule {
            pattern: r"^\s*>\s$",
            build: |_| Some((types::BLOCKQUOTE.into(), BlockAttrs::default())),
        }]
    }

    fn slash_items(&self) -> Vec<SlashItem> {
        vec![SlashItem {
            title: "Blockquote",
            subtext: "Blockquote block",
            keywords: &["quote", "blockquote"],
            group: "Style",
            icon: "quote",
            run: |editor, window, cx| editor.toggle_blockquote(window, cx),
        }]
    }
}

// ------------------------------------------------------------------ code block

pub struct CodeBlock;

impl BlockSpec for CodeBlock {
    fn type_name(&self) -> &'static str {
        types::CODE_BLOCK
    }

    fn label(&self, _: &BlockAttrs) -> SharedString {
        "Code Block".into()
    }

    fn caps(&self) -> BlockCaps {
        BlockCaps {
            marks: false,
            input_rules: false,
            multiline: true,
            ..Default::default()
        }
    }

    fn layout(&self, _: &BlockAttrs, theme: &EditorTheme) -> BlockLayout {
        BlockLayout {
            text_size: theme.code_text_size,
            line_height: theme.code_line_height,
            margin_top: theme.section_gap,
            margin_bottom: px(0.),
            mono: true,
            inner_padding: theme.block_padding,
            ..BlockLayout::new(theme)
        }
    }

    fn wrap(&self, ctx: &BlockContext, content: AnyElement, _: &mut Window, cx: &mut App) -> AnyElement {
        let language = ctx
            .attrs
            .language
            .clone()
            .unwrap_or(SharedString::new_static("plain text"));

        v_flex()
            .w_full()
            .relative()
            .p(ctx.theme.block_padding)
            .rounded(ctx.theme.radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(ctx.theme.code_block_background)
            .child(content)
            .child(
                // The language sits in the corner and only shows on hover,
                // the way a code block names itself without shouting.
                div()
                    .absolute()
                    .top(ctx.theme.rems(0.25))
                    .right(ctx.theme.rems(0.25))
                    .invisible()
                    .group_hover(super::view::group_name(ctx.id), |this| this.visible())
                    .child(
                        Button::new(("code-language", ctx.id.0 as usize))
                            .ghost()
                            .xsmall()
                            .label(language)
                            .dropdown_menu(move |menu, _window, _cx| {
                                let mut menu = menu.label("Language");
                                for language in CODE_LANGUAGES {
                                    menu = menu.menu(
                                        *language,
                                        Box::new(super::actions::SetCodeLanguage(language)),
                                    );
                                }
                                menu
                            }),
                    ),
            )
            .into_any_element()
    }

    fn input_rules(&self) -> Vec<BlockInputRule> {
        vec![
            BlockInputRule {
                pattern: r"^```([a-zA-Z0-9+#-]+)?[\s]$",
                build: |caps| {
                    let language = caps.get(1).map(|m| SharedString::from(m.as_str().to_string()));
                    Some((
                        types::CODE_BLOCK.into(),
                        BlockAttrs {
                            language,
                            ..Default::default()
                        },
                    ))
                },
            },
            BlockInputRule {
                pattern: r"^~~~([a-zA-Z0-9+#-]+)?[\s]$",
                build: |caps| {
                    let language = caps.get(1).map(|m| SharedString::from(m.as_str().to_string()));
                    Some((
                        types::CODE_BLOCK.into(),
                        BlockAttrs {
                            language,
                            ..Default::default()
                        },
                    ))
                },
            },
        ]
    }

    fn slash_items(&self) -> Vec<SlashItem> {
        vec![SlashItem {
            title: "Code Block",
            subtext: "Code block with syntax highlighting",
            keywords: &["code", "pre"],
            group: "Style",
            icon: "code",
            run: |editor, window, cx| editor.toggle_code_block(window, cx),
        }]
    }
}

// -------------------------------------------------------------- horizontal rule

pub struct HorizontalRule;

impl BlockSpec for HorizontalRule {
    fn type_name(&self) -> &'static str {
        types::HORIZONTAL_RULE
    }

    fn label(&self, _: &BlockAttrs) -> SharedString {
        "Separator".into()
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

    fn render_body(&self, _: &BlockContext, _: &mut Window, cx: &mut App) -> Option<AnyElement> {
        Some(
            div()
                .w_full()
                .h(px(1.))
                .bg(cx.theme().border)
                .into_any_element(),
        )
    }

    fn input_rules(&self) -> Vec<BlockInputRule> {
        vec![BlockInputRule {
            pattern: r"^(---|___|\*\*\*)$",
            build: |_| Some((types::HORIZONTAL_RULE.into(), BlockAttrs::default())),
        }]
    }

    fn slash_items(&self) -> Vec<SlashItem> {
        vec![SlashItem {
            title: "Separator",
            subtext: "Horizontal line to separate content",
            keywords: &["hr", "horizontalRule", "line", "separator"],
            group: "Insert",
            icon: "minus",
            run: |editor, window, cx| editor.set_horizontal_rule(window, cx),
        }]
    }
}

// ----------------------------------------------------------------------- image

pub struct Image;

impl BlockSpec for Image {
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
        let id = ctx.id;
        let editor = ctx.editor.clone();
        let selected = ctx.selected;

        let Some(src) = ctx.attrs.src.clone() else {
            let dropped = editor.clone();
            return Some(
                div()
                    .id(("image-drop", id.0 as usize))
                    .w_full()
                    .h(ctx.theme.rems(7.5))
                    .rounded(ctx.theme.radius)
                    .border_1()
                    .border_dashed()
                    .border_color(if selected {
                        cx.theme().primary
                    } else {
                        cx.theme().border
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(ctx.theme.rems(0.375))
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().muted.opacity(0.4)))
                    .text_color(cx.theme().muted_foreground)
                    .child(super::ui::icon(
                        "image-up",
                        ctx.theme.rems(1.125),
                        cx.theme().muted_foreground,
                    ))
                    .child("Click to upload or drag and drop")
                    .on_click(move |_, window, cx| {
                        let _ = editor.update(cx, |editor, cx| editor.pick_image(id, window, cx));
                    })
                    .on_drop(move |paths: &gpui_kit::ExternalPaths, _window, cx| {
                        let Some(path) = paths.paths().first().cloned() else {
                            return;
                        };
                        let _ =
                            dropped.update(cx, |editor, cx| editor.set_image_source(id, path, cx));
                    })
                    .into_any_element(),
            );
        };
        Some(
            div()
                .w_full()
                .when(selected, |this| {
                    this.border_2().border_color(cx.theme().primary)
                })
                .child(img(src.to_string()).w_full().rounded(ctx.theme.radius))
                .into_any_element(),
        )
    }

    fn slash_items(&self) -> Vec<SlashItem> {
        vec![SlashItem {
            title: "Image",
            subtext: "Resizable image with caption",
            keywords: &["image", "imageUpload", "upload", "img", "picture", "media"],
            group: "Upload",
            icon: "image",
            run: |editor, window, cx| editor.set_image("", "", window, cx),
        }]
    }
}

// --------------------------------------------------------------------- callout

pub struct Callout;

impl BlockSpec for Callout {
    fn type_name(&self) -> &'static str {
        types::CALLOUT
    }

    fn label(&self, _: &BlockAttrs) -> SharedString {
        "Callout".into()
    }

    fn layout(&self, _: &BlockAttrs, theme: &EditorTheme) -> BlockLayout {
        BlockLayout {
            margin_top: theme.block_gap,
            inner_padding: theme.block_padding,
            ..BlockLayout::new(theme)
        }
    }

    fn placeholder(&self, _: &BlockAttrs) -> SharedString {
        "Callout".into()
    }

    fn wrap(&self, ctx: &BlockContext, content: AnyElement, _: &mut Window, cx: &mut App) -> AnyElement {
        // A document may carry its own emoji; without one the callout uses an
        // icon, which renders on every platform whether or not a color emoji
        // font is installed.
        let marker = match ctx.attrs.emoji.clone() {
            Some(emoji) => div().child(emoji).into_any_element(),
            None => super::ui::icon(
                "lightbulb",
                ctx.theme.rems(1.125),
                cx.theme().muted_foreground,
            )
            .into_any_element(),
        };

        h_flex()
            .w_full()
            .items_start()
            .gap(ctx.theme.rems(0.625))
            .p(ctx.theme.block_padding)
            .rounded(ctx.theme.radius)
            .bg(ctx.theme.callout_background)
            .child(div().flex_none().pt(ctx.theme.rems(0.125)).child(marker))
            .child(div().flex_1().child(content))
            .into_any_element()
    }

    fn slash_items(&self) -> Vec<SlashItem> {
        vec![SlashItem {
            title: "Callout",
            subtext: "Make writing stand out",
            keywords: &["callout", "note", "info"],
            group: "Insert",
            icon: "info",
            run: |editor, window, cx| editor.toggle_callout(window, cx),
        }]
    }
}

// ---------------------------------------------------------------------- toggle

pub struct Toggle;

impl BlockSpec for Toggle {
    fn type_name(&self) -> &'static str {
        types::TOGGLE
    }

    fn label(&self, _: &BlockAttrs) -> SharedString {
        "Toggle list".into()
    }

    fn caps(&self) -> BlockCaps {
        BlockCaps {
            lifts_when_empty: true,
            ..Default::default()
        }
    }

    fn layout(&self, _: &BlockAttrs, theme: &EditorTheme) -> BlockLayout {
        BlockLayout {
            leading_width: theme.marker_width,
            margin_top: theme.rems(0.25),
            ..BlockLayout::new(theme)
        }
    }

    fn placeholder(&self, _: &BlockAttrs) -> SharedString {
        "Toggle".into()
    }

    fn render_leading(&self, ctx: &BlockContext, _: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let id = ctx.id;
        let collapsed = ctx.attrs.collapsed;
        let editor = ctx.editor.clone();
        Some(leading_slot(
            ctx,
            div()
                .id(("toggle", id.0 as usize))
                .size(ctx.theme.rems(1.25))
                .flex()
                .items_center()
                .justify_center()
                .rounded(ctx.theme.radius_sm)
                .cursor_pointer()
                .text_size(ctx.theme.rems(0.625))
                .text_color(cx.theme().muted_foreground)
                .hover(|this| this.bg(cx.theme().accent))
                .child(if collapsed { "▶" } else { "▼" })
                .on_click(move |_, _window, cx| {
                    let _ = editor.update(cx, |editor, cx| editor.toggle_collapsed(id, cx));
                }),
        ))
    }

    fn slash_items(&self) -> Vec<SlashItem> {
        vec![SlashItem {
            title: "Toggle list",
            subtext: "Hide and show content",
            keywords: &["toggle", "details", "collapse"],
            group: "Style",
            icon: "chevron-right",
            run: |editor, window, cx| editor.toggle_node(types::TOGGLE, BlockAttrs::default(), window, cx),
        }]
    }
}

/// Languages offered by a code block's language menu. The set a build can
/// actually highlight comes from the `tree-sitter-*` features it enables.
pub const CODE_LANGUAGES: &[&str] = &[
    "bash", "c", "cpp", "css", "diff", "go", "html", "java", "javascript", "json", "kotlin", "lua",
    "markdown", "php", "plain text", "python", "ruby", "rust", "sql", "swift", "toml", "tsx",
    "typescript", "yaml",
];

/// Line height helper shared by specs that size their own children.
pub fn line_height(layout: &BlockLayout) -> gpui_kit::Pixels {
    layout.text_size * layout.line_height
}

/// Relative line height for a text element.
pub fn relative_line_height(layout: &BlockLayout) -> gpui_kit::DefiniteLength {
    relative(layout.line_height)
}


// ---------------------------------------------------------------------- table

/// A table of text cells. The block itself holds no text: its content lives
/// in the [`CellGrid`](super::grid::CellGrid) the editor keeps beside it.
pub struct Table;

impl BlockSpec for Table {
    fn type_name(&self) -> &'static str {
        types::TABLE
    }

    fn label(&self, _: &BlockAttrs) -> SharedString {
        "Table".into()
    }

    fn caps(&self) -> BlockCaps {
        BlockCaps::grid()
    }

    fn layout(&self, _: &BlockAttrs, theme: &EditorTheme) -> BlockLayout {
        BlockLayout {
            margin_top: theme.section_gap,
            margin_bottom: theme.rems(0.5),
            ..BlockLayout::new(theme)
        }
    }

    fn render_body(
        &self,
        ctx: &BlockContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let grid = ctx.grid?;
        let id = ctx.id;
        let columns = grid.columns();
        let rows: Vec<AnyElement> = (0..grid.rows())
            .map(|row| render_row(ctx, grid, row, columns, window, cx))
            .collect();

        let row_controls: Vec<AnyElement> = (0..grid.rows())
            .map(|row| row_control(ctx, row, grid.row_height(row), cx))
            .collect();

        Some(
            h_flex()
                .id(("table", id.0 as usize))
                .test_support()
                .items_start()
                .gap(ctx.theme.rems(0.125))
                .child(
                    v_flex()
                        .flex_1()
                        .rounded(ctx.theme.radius)
                        .overflow_hidden()
                        .border_1()
                        .border_color(cx.theme().border)
                        .when(ctx.selected, |this| this.border_color(cx.theme().primary))
                        .children(rows)
                        .child(add_row_button(ctx, cx)),
                )
                // Row and column controls sit outside the table's frame, the
                // way Notion keeps them out of the data.
                .child(
                    v_flex()
                        .flex_none()
                        .pt(ctx.theme.rems(0.0625))
                        .children(row_controls),
                )
                .child(add_column_button(ctx, cx))
                .into_any_element(),
        )
    }

    fn slash_items(&self) -> Vec<SlashItem> {
        vec![SlashItem {
            title: "Table",
            subtext: "Insert a table",
            keywords: &["table", "grid", "rows", "columns"],
            group: "Insert",
            icon: "table",
            run: |editor, window, cx| editor.insert_table(3, 3, window, cx),
        }]
    }
}

fn render_row(
    ctx: &BlockContext,
    grid: &super::grid::CellGrid,
    row: usize,
    columns: usize,
    _window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let header = row == 0;
    // Every cell in a row is as tall as the tallest text in it, and each one
    // asks its own input how much height that takes.
    let text_height = grid.row_text_height(row);
    let row_height = grid.row_height(row);
    let cells: Vec<AnyElement> = (0..columns)
        .filter_map(|column| {
            let cell = grid.cell(super::grid::CellPosition::new(row, column))?;
            Some(
                div()
                    .flex_1()
                    .min_w(ctx.theme.rems(7.5))
                    .when(column + 1 < columns, |this| {
                        this.border_r_1().border_color(cx.theme().border)
                    })
                    .child(
                        Editor::new(cell.state())
                            .appearance(false)
                            .bordered(false)
                            .h(cell.height(text_height))
                            .text_size(ctx.theme.table_text_size)
                            .line_height(relative(ctx.theme.table_line_height))
                            .font_family(cx.theme().font_family.clone())
                            .text_color(cx.theme().foreground)
                            .when(header, |this| this.font_weight(FontWeight::SEMIBOLD)),
                    )
                    .into_any_element(),
            )
        })
        .collect();

    h_flex()
        .w_full()
        .h(row_height)
        .items_stretch()
        .when(header, |this| this.bg(ctx.theme.table_header_background))
        .when(row < grid.rows(), |this| {
            this.border_b_1().border_color(cx.theme().border)
        })
        .children(cells)
        .into_any_element()
}

/// The control beside a row that takes the row away.
fn row_control(
    ctx: &BlockContext,
    row: usize,
    row_height: gpui_kit::Pixels,
    _cx: &mut App,
) -> AnyElement {
    let id = ctx.id;
    let editor = ctx.editor.clone();
    div()
        .w(ctx.theme.rems(1.375))
        .h(row_height)
        .flex()
        .items_center()
        .justify_center()
        .when(!ctx.reveal_controls, |this| {
            this.invisible()
                .group_hover(super::view::group_name(id), |this| this.visible())
        })
        .child(
            Button::new(("drop-row", row))
                .ghost()
                .xsmall()
                .icon(super::ui::Lucide("x"))
                .tooltip("Delete row")
                .on_click(move |_, window, cx| {
                    let _ = editor.update(cx, |editor, cx| editor.remove_row(id, row, window, cx));
                }),
        )
        .into_any_element()
}

fn add_row_button(ctx: &BlockContext, cx: &mut App) -> AnyElement {
    let id = ctx.id;
    let editor = ctx.editor.clone();
    let rows = ctx.grid.map(super::grid::CellGrid::rows).unwrap_or(1);
    div()
        .w_full()
        .h(ctx.theme.rems(1.25))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_color(cx.theme().muted_foreground)
        .when(!ctx.reveal_controls, |this| {
            this.invisible()
                .group_hover(super::view::group_name(id), |this| this.visible())
        })
        .hover(|this| this.bg(cx.theme().muted.opacity(0.6)))
        .child(super::ui::icon(
            "plus",
            ctx.theme.rems(0.875),
            cx.theme().muted_foreground,
        ))
        .id(("add-row", id.0 as usize))
        .test_support()
        .on_click(move |_, window, cx| {
            let _ = editor.update(cx, |editor, cx| {
                editor.insert_row(id, rows.saturating_sub(1), window, cx)
            });
        })
        .into_any_element()
}

fn add_column_button(ctx: &BlockContext, cx: &mut App) -> AnyElement {
    let id = ctx.id;
    let editor = ctx.editor.clone();
    let columns = ctx.grid.map(super::grid::CellGrid::columns).unwrap_or(1);
    let height = ctx
        .grid
        .map(|grid| grid.row_height(0))
        .unwrap_or(ctx.theme.rems(2.));
    div()
        .flex_none()
        .w(ctx.theme.rems(1.25))
        .h(height)
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .rounded(ctx.theme.radius_sm)
        .text_color(cx.theme().muted_foreground)
        .when(!ctx.reveal_controls, |this| {
            this.invisible()
                .group_hover(super::view::group_name(id), |this| this.visible())
        })
        .hover(|this| this.bg(cx.theme().muted.opacity(0.6)))
        .child(super::ui::icon(
            "plus",
            ctx.theme.rems(0.875),
            cx.theme().muted_foreground,
        ))
        .id(("add-column", id.0 as usize))
        .test_support()
        .on_click(move |_, window, cx| {
            let _ = editor.update(cx, |editor, cx| {
                editor.insert_column(id, columns.saturating_sub(1), window, cx)
            });
        })
        .into_any_element()
}
