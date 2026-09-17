//! The suggestion menus: `/` for blocks, `:` for emoji, `@` for mentions.
//!
//! One menu state serves all three. A trigger character typed at a word
//! boundary opens it, the characters after it filter, Enter commits and
//! Escape closes, leaving the typed text alone.

use gpui_kit::component::ActiveTheme;
use gpui_kit::TestSupportExt as _;
use gpui_kit::{
    Anchor, AnyElement, App, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    Point, SharedString, StatefulInteractiveElement as _, Styled as _, Window, deferred, div, px,
};

use super::block::{BlockId, BlockRegistry};
use super::mark::MarkKind;
use super::suggestion::{EMOJI, Emoji, Mention, Trigger, default_mentions, matches};
use super::toolbar::OVERLAY_PRIORITY;
use super::theme::ActiveEditorTheme;
use super::ui;
use super::view::NotionEditor;

/// Suggestions supplied by an embedding application. Labels and tags are
/// opaque; choosing a row never edits the document in this renderer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationMenu {
    pub anchor: ApplicationMenuAnchor,
    pub items: Vec<super::toolbar::ToolbarItem>,
    pub selected: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplicationMenuAnchor {
    Caret,
    Block(usize),
}

#[derive(Default)]
pub(crate) enum MenuSource {
    #[default]
    BuiltIn,
    Application(Option<ApplicationMenu>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuAction {
    Select(usize),
    Pick(SharedString),
    Dismiss,
    Open(char),
}

/// An open suggestion menu.
pub struct SuggestionMenu {
    pub trigger: Trigger,
    /// Block the query lives in.
    pub block: BlockId,
    /// Byte offset of the trigger character.
    pub start: usize,
    pub query: String,
    pub selected: usize,
}

/// One row of a menu, whatever opened it.
pub enum SuggestionItem {
    Block {
        title: &'static str,
        group: &'static str,
        icon: &'static str,
        run: fn(&mut NotionEditor, &mut Window, &mut Context<NotionEditor>),
    },
    Emoji(&'static Emoji),
    Mention(Mention),
}

impl SuggestionItem {
    pub fn title(&self) -> SharedString {
        match self {
            Self::Block { title, .. } => SharedString::from(*title),
            Self::Emoji(emoji) => SharedString::from(emoji.name),
            Self::Mention(mention) => mention.name.clone(),
        }
    }

    pub fn group(&self) -> &'static str {
        match self {
            Self::Block { group, .. } => group,
            Self::Emoji(_) => "Emoji",
            Self::Mention(_) => "People",
        }
    }
}

/// Group order of the menu, matching the template's AI → Style → Insert →
/// Upload. Groups a block spec invents are appended after these.
const GROUP_ORDER: &[&str] = &["AI", "Style", "Insert", "Upload", "Emoji", "People"];

fn group_rank(group: &str) -> usize {
    GROUP_ORDER
        .iter()
        .position(|known| *known == group)
        .unwrap_or(GROUP_ORDER.len())
}

impl NotionEditor {
    /// Supply the application's menu; None keeps application ownership with
    /// no open menu. Standalone editors use their built-in menus by default.
    pub fn set_application_menu(&mut self, menu: Option<ApplicationMenu>, cx: &mut Context<Self>) {
        let unchanged =
            matches!(&self.menu_source, MenuSource::Application(current) if current == &menu);
        if unchanged {
            return;
        }
        self.menu_source = MenuSource::Application(menu);
        self.suggestion = None;
        cx.notify();
    }

    pub fn suggestion_is_open(&self) -> bool {
        match &self.menu_source {
            MenuSource::BuiltIn => self.suggestion.is_some(),
            MenuSource::Application(menu) => menu.is_some(),
        }
    }

    /// Kept for the `/` affordances: the gutter `+` and `Mod+/`.
    pub fn slash_menu_is_open(&self) -> bool {
        self.suggestion
            .as_ref()
            .is_some_and(|menu| menu.trigger == Trigger::Slash)
    }

    /// Open the block menu at the caret, inserting the `/` if needed.
    pub fn open_slash_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_suggestion(Trigger::Slash, window, cx);
    }

    /// Open a menu at the caret, typing its trigger character if it is not
    /// already there — what the gutter `+`, `Mod+/` and the menu's own
    /// Mention and Emoji entries do.
    pub fn open_suggestion(
        &mut self,
        trigger: Trigger,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let MenuSource::Application(_) = &self.menu_source {
            cx.emit(MenuAction::Open(trigger.character()));
            return;
        }
        let Some(ix) = self.active_index() else {
            return;
        };
        if !self.spec_at(ix, cx).caps().input_rules {
            return;
        }
        let character = trigger.character();
        let caret = self.blocks[ix].state.read(cx).cursor();
        let present = self.blocks[ix].text[..caret].ends_with(character);
        if !present {
            let mut typed = String::new();
            // A trigger needs whitespace in front of it to count as one.
            if caret > 0 && !self.blocks[ix].text[..caret].ends_with(char::is_whitespace) {
                typed.push(' ');
            }
            typed.push(character);
            let caret_after = caret + typed.len();
            self.edit_block_text(ix, caret..caret, &typed, Some(caret_after), window, cx);
        }
        let caret = self.blocks[ix].state.read(cx).cursor();
        self.suggestion = Some(SuggestionMenu {
            trigger,
            block: self.blocks[ix].id,
            start: caret - character.len_utf8(),
            query: String::new(),
            selected: 0,
        });
        cx.notify();
    }

    pub fn close_suggestion_menu(&mut self, cx: &mut Context<Self>) {
        if let MenuSource::Application(menu) = &self.menu_source {
            if menu.is_some() {
                cx.emit(MenuAction::Dismiss);
            }
            return;
        }
        if self.suggestion.take().is_some() {
            cx.notify();
        }
    }

    /// Called after every edit: open on a fresh trigger, or re-read the query.
    pub(crate) fn sync_suggestion_menu(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let MenuSource::Application(_) = &self.menu_source {
            return;
        }
        let Some(ix) = self.active_index() else {
            self.suggestion = None;
            return;
        };
        let caret = self.blocks[ix].state.read(cx).cursor();
        let text = self.blocks[ix].text.clone();
        let id = self.blocks[ix].id;

        if let Some(menu) = &mut self.suggestion {
            let trigger = menu.trigger.character();
            let still_open = menu.block == id
                && caret > menu.start
                && text[menu.start..].starts_with(trigger)
                && menu.start < text.len();
            if !still_open {
                self.suggestion = None;
                cx.notify();
                return;
            }
            let query = text[menu.start + trigger.len_utf8()..caret].to_string();
            if query != menu.query {
                menu.query = query;
                menu.selected = 0;
            }
            // A query that matches nothing ends the menu, as the suggestion
            // plugin does, so ordinary prose is not held hostage by a `:`.
            if self.suggestion_items(cx).is_empty() {
                self.suggestion = None;
            }
            cx.notify();
            return;
        }

        if caret == 0 || !self.spec_at(ix, cx).caps().input_rules {
            return;
        }
        let Some(character) = text[..caret].chars().next_back() else {
            return;
        };
        let Some(trigger) = Trigger::from_character(character) else {
            return;
        };
        let start = caret - character.len_utf8();
        let preceding = text[..start].chars().next_back();
        if preceding.is_some_and(|previous| !previous.is_whitespace()) {
            return;
        }

        self.suggestion = Some(SuggestionMenu {
            trigger,
            block: id,
            start,
            query: String::new(),
            selected: 0,
        });
        if !trigger.opens_empty() {
            // The emoji menu waits for a shortcode before showing anything.
            cx.notify();
            return;
        }
        cx.notify();
    }

    /// Items matching the current query, in registry order.
    pub fn suggestion_items(&self, cx: &App) -> Vec<SuggestionItem> {
        let Some(menu) = self.suggestion.as_ref() else {
            return Vec::new();
        };
        let query = menu.query.clone();

        match menu.trigger {
            Trigger::Slash => {
                let mut items: Vec<SuggestionItem> = BlockRegistry::global(cx)
                    .slash_items()
                    .into_iter()
                    .chain(TRIGGER_ITEMS.iter().map(|item| super::block::SlashItem {
                        title: item.title,
                        subtext: item.subtext,
                        keywords: item.keywords,
                        group: item.group,
                        icon: item.icon,
                        run: item.run,
                    }))
                    .filter(|item| matches(&query, item.title, item.keywords))
                    .map(|item| SuggestionItem::Block {
                        title: item.title,
                        group: item.group,
                        icon: item.icon,
                        run: item.run,
                    })
                    .collect();
                // Groups appear in the template's order; items keep the order
                // their specs were registered in.
                items.sort_by_key(|item| group_rank(item.group()));
                items
            }
            Trigger::Emoji => {
                if query.is_empty() {
                    return Vec::new();
                }
                EMOJI
                    .iter()
                    .filter(|emoji| matches(&query, emoji.name, emoji.keywords))
                    .map(SuggestionItem::Emoji)
                    .collect()
            }
            Trigger::Mention => self
                .mentions
                .iter()
                .filter(|mention| matches(&query, &mention.name, &[]))
                .cloned()
                .map(SuggestionItem::Mention)
                .collect(),
        }
    }

    /// Kept for the tests and hosts that speak in terms of the slash menu.
    pub fn slash_items(&self, cx: &App) -> Vec<SuggestionItem> {
        self.suggestion_items(cx)
    }

    /// The people offered by `@`; an application sets its own list.
    pub fn set_mentions(&mut self, mentions: Vec<Mention>, cx: &mut Context<Self>) {
        self.mentions = mentions;
        cx.notify();
    }

    pub fn move_suggestion_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let MenuSource::Application(menu) = &mut self.menu_source {
            let Some(menu) = menu else {
                return;
            };
            if menu.items.is_empty() {
                return;
            }
            menu.selected = menu
                .selected
                .saturating_add_signed(delta)
                .min(menu.items.len() - 1);
            cx.emit(MenuAction::Select(menu.selected));
            cx.notify();
            return;
        }
        let count = self.suggestion_items(cx).len();
        let Some(menu) = &mut self.suggestion else {
            return;
        };
        if count == 0 {
            return;
        }
        menu.selected = (menu.selected as isize + delta).rem_euclid(count as isize) as usize;
        cx.notify();
    }

    pub fn confirm_suggestion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let MenuSource::Application(menu) = &self.menu_source {
            let Some(item) = menu.as_ref().and_then(|menu| menu.items.get(menu.selected)) else {
                return;
            };
            cx.emit(MenuAction::Pick(item.tag.clone()));
            return;
        }
        let items = self.suggestion_items(cx);
        let Some(menu) = self.suggestion.take() else {
            return;
        };
        let Some(item) = items.into_iter().nth(menu.selected) else {
            cx.notify();
            return;
        };
        let Some(ix) = self.index_of(menu.block) else {
            return;
        };
        let caret = self.blocks[ix].state.read(cx).cursor();
        let end = caret.max(menu.start);

        match item {
            SuggestionItem::Block { run, .. } => {
                self.edit_block_text(ix, menu.start..end, "", Some(menu.start), window, cx);
                run(self, window, cx);
            }
            SuggestionItem::Emoji(emoji) => {
                let caret_after = menu.start + emoji.character.len();
                self.edit_block_text(
                    ix,
                    menu.start..end,
                    emoji.character,
                    Some(caret_after),
                    window,
                    cx,
                );
            }
            SuggestionItem::Mention(mention) => {
                let text = format!("@{} ", mention.name);
                let caret_after = menu.start + text.len();
                let marked = menu.start..menu.start + text.trim_end().len();
                self.edit_block_text(ix, menu.start..end, &text, Some(caret_after), window, cx);
                if let Some(block) = self.blocks.get_mut(ix) {
                    block.marks.add(mention.mark(), marked);
                    block.stored_marks = Some(Vec::new());
                }
                self.apply_decorations(menu.block, cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn run_suggestion_item(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &mut self.menu_source {
            MenuSource::Application(Some(menu)) => menu.selected = index,
            MenuSource::Application(None) => return,
            MenuSource::BuiltIn => {
                if let Some(menu) = &mut self.suggestion {
                    menu.selected = index;
                }
            }
        }
        self.confirm_suggestion(window, cx);
    }

    fn render_application_menu(
        &self,
        menu: &ApplicationMenu,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if menu.items.is_empty() {
            return None;
        }
        let theme = cx.editor_theme().clone();
        let gap = theme.rems(0.375);
        let position = match menu.anchor {
            ApplicationMenuAnchor::Caret => {
                let ix = self.active_index()?;
                let (caret, line_height) = self.blocks[ix].state.read(cx).cursor_layout()?;
                caret.origin + Point::new(px(0.), line_height + gap)
            }
            ApplicationMenuAnchor::Block(index) => {
                let bounds = self.block_bounds(self.blocks.get(index)?.id)?;
                Point::new(bounds.left(), bounds.bottom() + gap)
            }
        };
        let rows = menu
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                ui::menu_row(index == menu.selected, cx)
                    .id(("application-suggestion", index))
                    .test_support()
                    .child(item.label.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run_suggestion_item(index, window, cx)
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        Some(
            deferred(
                gpui_kit::base::Positioner::corner(Anchor::TopLeft, position)
                    .margin(theme.rems(0.5))
                    .occlude()
                    .child(
                        ui::popover_surface(cx)
                            .w(theme.rems(20.))
                            .max_h(theme.rems(22.5))
                            .id("application-suggestion-menu")
                            .overflow_y_scroll()
                            .children(rows),
                    ),
            )
            .with_priority(OVERLAY_PRIORITY)
            .into_any_element(),
        )
    }

    /// The menu, anchored under the caret.
    pub(crate) fn render_suggestion_menu(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if let MenuSource::Application(menu) = &self.menu_source {
            return self.render_application_menu(menu.as_ref()?, cx);
        }
        let menu = self.suggestion.as_ref()?;
        let ix = self.index_of(menu.block)?;
        let state = self.blocks[ix].state.read(cx);
        let (caret, line_height) = state.cursor_layout()?;
        let theme = cx.editor_theme().clone();
        let position = caret.origin + Point::new(px(0.), line_height + theme.rems(0.375));

        let items = self.suggestion_items(cx);
        if items.is_empty() {
            return None;
        }

        let mut rows: Vec<AnyElement> = Vec::new();
        let mut group = "";
        for (index, item) in items.iter().enumerate() {
            if item.group() != group {
                if !group.is_empty() {
                    rows.push(
                        div()
                            .h(px(1.))
                            .my(theme.rems(0.25))
                            .mx(theme.rems(0.25))
                            .bg(cx.theme().border)
                            .into_any_element(),
                    );
                }
                group = item.group();
                rows.push(
                    div()
                        .px(theme.rems(0.5))
                        .pt(theme.rems(0.5))
                        .pb(theme.rems(0.25))
                        .text_size(theme.ui_small_text_size)
                        .text_color(cx.theme().muted_foreground)
                        .child(group.to_string())
                        .into_any_element(),
                );
            }

            let selected = index == menu.selected;
            let leading = match item {
                SuggestionItem::Block { icon, .. } => ui::icon(
                    icon,
                    theme.text_size,
                    if selected {
                        cx.theme().accent_foreground
                    } else {
                        cx.theme().muted_foreground
                    },
                )
                .into_any_element(),
                SuggestionItem::Emoji(emoji) => div()
                    .w(theme.text_size)
                    .child(emoji.character.to_string())
                    .into_any_element(),
                SuggestionItem::Mention(mention) => div()
                    .w(theme.text_size)
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        mention
                            .name
                            .chars()
                            .next()
                            .map(|initial| initial.to_string())
                            .unwrap_or_default(),
                    )
                    .into_any_element(),
            };

            rows.push(
                ui::menu_row(selected, cx)
                    .id(("suggestion", index))
                    .child(leading)
                    .child(item.title())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run_suggestion_item(index, window, cx)
                    }))
                    .into_any_element(),
            );
        }

        Some(
            deferred(
                gpui_kit::base::Positioner::corner(Anchor::TopLeft, position)
                    .margin(theme.rems(0.5))
                    .occlude()
                    .child(
                        ui::popover_surface(cx)
                            .w(theme.rems(20.))
                            .max_h(theme.rems(22.5))
                            .id("suggestion-menu")
                            .overflow_y_scroll()
                            .children(rows),
                    ),
            )
            .with_priority(OVERLAY_PRIORITY)
            .into_any_element(),
        )
    }
}

/// Menu entries that open another menu, so `/` can reach emoji and mentions.
const TRIGGER_ITEMS: &[super::block::SlashItem] = &[
    super::block::SlashItem {
        title: "Mention",
        subtext: "Mention a user or item",
        keywords: &["mention", "user", "item", "tag"],
        group: "Insert",
        icon: "at-sign",
        run: |editor, window, cx| editor.open_suggestion(Trigger::Mention, window, cx),
    },
    super::block::SlashItem {
        title: "Emoji",
        subtext: "Insert an emoji",
        keywords: &["emoji", "emoticon", "smiley"],
        group: "Insert",
        icon: "smile",
        run: |editor, window, cx| editor.open_suggestion(Trigger::Emoji, window, cx),
    },
];

/// Mentions are styled like a soft chip; the mark carries the person's id.
pub fn mention_mark_id(kind: &MarkKind) -> Option<SharedString> {
    match kind {
        MarkKind::Mention(id) => Some(id.clone()),
        _ => None,
    }
}

/// Default people list, exposed so an application can start from it.
pub fn default_people() -> Vec<Mention> {
    default_mentions()
}
