# gpui-notion

[![CI](https://github.com/orthory/gpui-notion/actions/workflows/ci.yml/badge.svg)](https://github.com/orthory/gpui-notion/actions/workflows/ci.yml)

A Notion-like block editor written in Rust on [GPUI](https://www.gpui.rs) through
[`gpui-kit`](https://github.com/longbridge/gpui-kit), with the feature surface and command
names of Tiptap's [Notion-like editor template](https://tiptap.dev/docs/ui-components/templates/notion-like-editor).

```bash
cargo run            # the demo document
cargo test           # unit tests + UI integration tests
```

Requires a Rust toolchain with the 2024 edition and, on Linux, the libraries GPUI
draws and talks to the compositor with:

```bash
sudo apt install -y gcc g++ clang libfontconfig-dev libwayland-dev \
  libxkbcommon-x11-dev libx11-xcb-dev libssl-dev libzstd-dev \
  vulkan-validationlayers libvulkan1
```

## What it does

- **Blocks**: paragraph, heading 1–3, bullet / numbered / to-do lists (nested), blockquote,
  code block with syntax highlighting and a language menu, separator, image (click or drop
  a file), callout, toggle, table.
- **Inline marks**: bold, italic, underline, strike, code, link, highlight (10 colors),
  text color (10 colors), mention.
- **Markdown input rules**: `# `, `## `, `### `, `- `, `* `, `+ `, `1. `, `[] `, `[x] `,
  `> `, ` ``` `, `---`, plus `**bold**`, `_italic_`, `~~strike~~`, `` `code` ``, `==mark==`
  and smart typography (`--`, `...`, `(c)`).
- **Menus**: `/` block commands, `:` emoji, `@` mentions — one keyboard contract, filtered
  live, Enter commits, Escape closes and leaves the typed text alone.
- **Selection toolbar**: turn-into, B/I/U/S/code, link editor, color palettes, more menu.
- **Gutter**: hover `+` to insert a block, grip to select, drag to reorder, click for the
  block menu (turn into, reset formatting, duplicate, copy, move, delete).
- **Comments**: select text and comment on it — an amber anchor in the document, a thread
  popover with replies and Resolve, and clicking commented text opens its thread.
- **Block selection**: drag across blocks or Shift+Click to select whole ones, then type
  over them, format them, indent them or turn them all into something else.
- **Document undo/redo** with typing coalescing, clipboard as markdown (tables included),
  paste that becomes blocks.

Parity is graded item by item against the template in [`docs/PARITY.md`](docs/PARITY.md).
The spec it is graded against is [`docs/research/tiptap-notion-spec.md`](docs/research/tiptap-notion-spec.md),
extracted from Tiptap's docs and shipped bundle.

## Architecture

One entity owns the document; each block owns the input that renders it.

```
NotionEditor (Entity)            src/editor/view.rs
├── Vec<Block>                   src/editor/block.rs
│   ├── ty: BlockType + attrs    node type, heading level, checked, language…
│   ├── text: String             mirror of the input, used to diff edits
│   ├── marks: MarkList          inline formatting as byte ranges
│   └── state: Entity<EditorState>  one gpui-kit code-editor input per block
├── grids: BlockId -> CellGrid   child text areas for grid blocks (tables)
│   └── every text area is sized by measurement, never by assumed padding
├── comments: Vec<Thread>        discussions anchored by a mark
├── suggestion / link_editor / selection / history / drop_target
└── BlockRegistry (Global)       node types, their rendering and their rules
```

Every text area — a block, a table cell — is sized and placed by measurement through
[`InputFit`](src/editor/fit.rs). Two numbers are learned from each input every frame:
the **inset**, the height it keeps for itself, and the **lead**, how far from its own
corner it puts the first glyph. Neither is knowable up front — an input pads itself,
a code-editor input reserves a line-number gutter even with line numbers off, and the
text area is snapped to whole device pixels. A text area a fraction short of its text
scrolls inside itself as the caret moves between rows and the document rattles as it
is clicked around; an unmeasured lead puts every paragraph a few pixels off the
markers beside it. Both measurements are acted on only at the resolution of a physical
pixel: text is painted on that grid, so a correction smaller than one does not remove
the error, it trades it for another, and the page walks on the spot. The width text wraps at and the line height markers align to are
read back from the input for the same reason.

Inline formatting is painted by handing the input's decoration layer one
`HighlightStyle` run per mark run (`view.rs::apply_decorations`); the marks themselves live
in the editor and are remapped across every edit (`mark.rs`), so the text and its formatting
never drift apart.

The command surface mirrors Tiptap's: `toggle_bold`, `toggle_heading(2)`,
`toggle_bullet_list`, `split_block`, `join_backward`, `sink_list_item`, `lift_list_item`,
`set_link`, `unset_all_marks`, `set_horizontal_rule`, `insert_content` — see
`src/editor/commands.rs`.

## Adding a block type

Node types are data. Implement [`BlockSpec`](src/editor/block.rs) and register it; the slash
menu, the markdown rules, the turn-into menu and the layout all pick it up.

```rust
use gpui_notion::editor::block::*;

struct Warning;

impl BlockSpec for Warning {
    fn type_name(&self) -> &'static str { "warning" }
    fn label(&self, _: &BlockAttrs) -> SharedString { "Warning".into() }

    fn layout(&self, _: &BlockAttrs, theme: &EditorTheme) -> BlockLayout {
        // Sizes come from the tokens, so the block scales and re-themes with
        // the rest of the document.
        BlockLayout { inner_padding: theme.block_padding, ..BlockLayout::new(theme) }
    }

    fn input_rules(&self) -> Vec<BlockInputRule> {
        vec![BlockInputRule {
            pattern: r"^!\s$",
            build: |_| Some(("warning".into(), BlockAttrs::default())),
        }]
    }

    fn slash_items(&self) -> Vec<SlashItem> {
        vec![SlashItem {
            title: "Warning",
            subtext: "Something to watch out for",
            keywords: &["warning", "caution"],
            group: "Insert",
            icon: "triangle-alert",
            run: |editor, window, cx| editor.toggle_node("warning", BlockAttrs::default(), window, cx),
        }]
    }

    fn wrap(&self, _: &BlockContext, content: AnyElement, _: &mut Window, cx: &mut App) -> AnyElement {
        div().bg(cx.theme().warning).child(content).into_any_element()
    }
}

// in your app's init, after `gpui_notion::editor::init(cx)`
BlockRegistry::register(cx, Warning);
```

A spec can also supply `render_leading` (the bullet, number or checkbox column),
`render_body` (for nodes that are not text, like the separator or the table), `caps`
(whether marks, input rules, list nesting, multi-line Enter or a grid of child cells apply)
and `split_into` (what Enter creates). Asking for `BlockCaps::grid()` gets the block a
`CellGrid` of child text areas, which is all a table is.

## Theming

Nothing in the editor states a colour or a size at the point it is drawn. Both come
from [`EditorTheme`](src/editor/theme.rs), the editor's own token layer, which is
derived from the `gpui-kit` theme in force and derived again whenever that changes.

- **Colours** are semantic roles from `cx.theme()` — `foreground`, `muted`, `border`,
  `primary`, `selection`, `popover` — plus the few the framework has no name for: the
  wash behind commented text, the code and callout surfaces, and the palettes a
  `highlight` or `textStyle` mark picks from. Those live in `EditorTheme` as fields,
  not as literals at a call site. Floating surfaces use the framework's own popover
  treatment, so the slash menu and the toolbar cannot drift from the menus beside them.
- **Sizes** are multiples of the theme's base font size — the window's `rem`. The page
  column, its padding, the gutter, the indent per level, the marker column, block
  margins, the heading scale, the table's text and every popover in the editor all
  move together when that base changes: the document zooms as one system instead of
  growing text inside fixed boxes. Radii come from the theme's radius tiers, so a
  squared-off theme squares the editor off too.
- **Nothing assumes a physical number** except the two that are physical: a one-pixel
  hairline and the device-pixel slack an input measurement needs.

An application changes the whole editor from one place:

```rust
EditorTheme::customize(cx, |theme, kit| {
    theme.page_width = theme.rems(52.);          // a wider column
    theme.comment_fill = kit.warning.opacity(0.2);
    theme.headings[0].margin_above = 2.0;        // less air over an H1
});
```

The closure is kept and re-applied on top of freshly derived values whenever the
`gpui-kit` theme changes, so a customized editor stays customized across light, dark
and every base size.

The document ships in the template's own palette: `editor::theme::apply_template_palette`
installs [`assets/themes/notion.json`](assets/themes/notion.json) as the theme's light
and dark configurations — page, border, selection, caret and the brand link colour — and
sets the Notion text and highlight palettes and the gray-ramp code surfaces alongside it.
Because it becomes the theme, switching appearance keeps it. `NOTION_PALETTE=kit` starts
in the plain `gpui-kit` palette instead.

Appearance and zoom are the application's to own; `editor::theme::init_appearance_actions`
installs a ready-made set (`Mod+Shift+L` to switch palette, `Mod+=`, `Mod+-`, `Mod+0`
to change the base size), and the demo binary also takes `NOTION_THEME=dark` and
`NOTION_FONT_SIZE=<px>`.

## Keyboard

| Keys | Command |
| --- | --- |
| `Mod+B` / `I` / `U` / `Shift+S` / `E` | bold, italic, underline, strike, code |
| `Mod+.` / `Mod+,` | superscript / subscript |
| `Mod+Shift+K` | link editor · `Mod+R` reset formatting |
| `Mod+Alt+0…3` | paragraph, heading 1–3 |
| `Mod+Shift+7` / `8` / `9` | numbered, bullet, to-do list |
| `Mod+Shift+B` / `Mod+Alt+C` | blockquote / code block |
| `Enter` / `Shift+Enter` | split block / line break inside the block |
| `Backspace` at block start | join with the block above, or lift it to a paragraph |
| `Tab` / `Shift+Tab` | nest / lift a list item |
| `↑` `↓` `←` `→` at an edge | move to the neighbouring block |
| `Shift+↑` `Shift+↓` at an edge | select whole blocks · `Mod+A` twice selects the document |
| `Mod+D` / `Mod+Shift+Backspace` | duplicate / delete block |
| `Mod+Shift+↑` `Mod+Shift+↓` | move the block |
| `Mod+Z` / `Mod+Shift+Z` / `Mod+Y` | undo / redo |
| `Mod+Shift+M` | comment on the selection |
| `Mod+Shift+E` / `Mod+Shift+2` / `Mod+Shift+I` | emoji menu / mention menu / image |
| `Tab` / `Shift+Tab` in a table | next / previous cell; Tab in the last cell adds a row |
| `Mod+/` | open the block menu · `Escape` closes menus, then selects the block |
| `Mod+Shift+L` · `Mod+=` `Mod+-` `Mod+0` | switch palette · zoom the document (opt-in, see Theming) |

## Testing

`tests/editing.rs` drives the real UI headlessly with `#[gpui_kit::test]`: it clicks,
types, presses keys and drags, then asserts the document. Unit tests for the mark algebra
live beside it in `src/editor/mark.rs`.

```bash
cargo test --test editing        # UI integration tests
NOTION_DEMO=toolbar cargo run    # open the window with the selection toolbar shown
NOTION_DEMO=comment cargo run    # … with a comment thread open (also: slash, toolbar, caret)
NOTION_GUTTER=always cargo run   # … with every block's gutter controls drawn, for screenshots
```

## Notes and limits

Some template behaviour cannot be expressed on this stack today, and is recorded as such in
`docs/PARITY.md`: GPUI's `HighlightStyle` carries no font size or family, so per-range font
switching (real superscript baselines, monospace inline code) is unavailable, and
multi-line inputs do not support per-block text alignment. Table cells hold plain text (no
marks, no column resizing or merging), and image blocks take a picked or dropped file but
have no upload progress, size limits or captions.

## License

MIT — see [LICENSE](LICENSE).
