//! Actions and key bindings, named after the Tiptap commands they run.

use gpui_kit::component::input;
use gpui_kit::{Action, App, KeyBinding, actions};
use serde::Deserialize;

use super::mark::{HighlightColor, TextColor};

/// Setting a code block's language, dispatched by its language menu.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = notion, no_json)]
pub struct SetCodeLanguage(pub &'static str);

/// Applying one of the palette colors, dispatched by the color menus.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = notion, no_json)]
pub enum ApplyColor {
    Text(TextColor),
    Highlight(HighlightColor),
}

actions!(
    notion,
    [
        ToggleBold,
        ToggleItalic,
        ToggleUnderline,
        ToggleStrike,
        ToggleCode,
        ToggleHighlight,
        ToggleSuperscript,
        ToggleSubscript,
        SetLink,
        ClearMarks,
        SetParagraph,
        SetHeading1,
        SetHeading2,
        SetHeading3,
        ToggleBulletList,
        ToggleOrderedList,
        ToggleTaskList,
        ToggleBlockquote,
        ToggleCodeBlock,
        SetHorizontalRule,
        OpenSlashMenu,
        AddComment,
        InsertRowAbove,
        InsertRowBelow,
        InsertColumnLeft,
        InsertColumnRight,
        DeleteRow,
        DeleteColumn,
        OpenEmojiMenu,
        OpenMentionMenu,
        InsertImage,
        DuplicateBlock,
        DeleteBlock,
        MoveBlockUp,
        MoveBlockDown,
        SelectBlock,
        CopyBlock,
    ]
);

/// Key context of the editor surface; bindings below resolve inside it.
pub const CONTEXT: &str = "NotionEditor";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-b", ToggleBold, Some(CONTEXT)),
        KeyBinding::new("secondary-i", ToggleItalic, Some(CONTEXT)),
        KeyBinding::new("secondary-u", ToggleUnderline, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-s", ToggleStrike, Some(CONTEXT)),
        KeyBinding::new("secondary-e", ToggleCode, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-h", ToggleHighlight, Some(CONTEXT)),
        KeyBinding::new("secondary-.", ToggleSuperscript, Some(CONTEXT)),
        KeyBinding::new("secondary-,", ToggleSubscript, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-k", SetLink, Some(CONTEXT)),
        KeyBinding::new("secondary-/", OpenSlashMenu, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-m", AddComment, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-e", OpenEmojiMenu, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-2", OpenMentionMenu, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-i", InsertImage, Some(CONTEXT)),
        KeyBinding::new("secondary-r", ClearMarks, Some(CONTEXT)),
        KeyBinding::new("secondary-alt-0", SetParagraph, Some(CONTEXT)),
        KeyBinding::new("secondary-alt-1", SetHeading1, Some(CONTEXT)),
        KeyBinding::new("secondary-alt-2", SetHeading2, Some(CONTEXT)),
        KeyBinding::new("secondary-alt-3", SetHeading3, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-8", ToggleBulletList, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-7", ToggleOrderedList, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-9", ToggleTaskList, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-b", ToggleBlockquote, Some(CONTEXT)),
        KeyBinding::new("secondary-alt-c", ToggleCodeBlock, Some(CONTEXT)),
        KeyBinding::new("secondary-d", DuplicateBlock, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-backspace", DeleteBlock, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-up", MoveBlockUp, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-down", MoveBlockDown, Some(CONTEXT)),
        // With whole blocks selected the caret is out of the inputs, so the
        // editing keys are bound here as well as in the `Input` context.
        KeyBinding::new("escape", input::Escape, Some(CONTEXT)),
        KeyBinding::new("backspace", input::Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", input::Delete, Some(CONTEXT)),
        KeyBinding::new("secondary-c", input::Copy, Some(CONTEXT)),
        KeyBinding::new("secondary-x", input::Cut, Some(CONTEXT)),
        KeyBinding::new("up", input::MoveUp, Some(CONTEXT)),
        KeyBinding::new("down", input::MoveDown, Some(CONTEXT)),
        KeyBinding::new("shift-up", gpui_kit::base::actions::SelectUp, Some(CONTEXT)),
        KeyBinding::new("shift-down", gpui_kit::base::actions::SelectDown, Some(CONTEXT)),
        // The inputs bind redo per platform; the editor accepts both spellings.
        KeyBinding::new("secondary-shift-z", input::Redo, Some(CONTEXT)),
        KeyBinding::new("secondary-y", input::Redo, Some(CONTEXT)),
    ]);
}
