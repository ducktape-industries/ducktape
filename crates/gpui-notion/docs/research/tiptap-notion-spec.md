# Tiptap "Notion-like Editor" — feature + style specification

Target: rebuild the Tiptap Notion-like editor template in Rust/GPUI with behavioural and
visual parity.

## Source basis and confidence

Three classes of evidence are used; every nontrivial claim is tagged.

| Tag | Meaning |
| --- | --- |
| **[DOC]** | Stated in Tiptap's public docs (URL cited inline). |
| **[BUNDLE]** | Read directly out of the shipped production bundle of the official live preview <https://template.tiptap.dev/preview/templates/notion-like> (CSS chunks + minified JS chunks under `/_next/static/…`). These are exact values from the running template, not guesses. |
| **[ESTIMATED]** | Not verifiable from either; a reasoned default is given. |

Primary docs:

- Template page — <https://tiptap.dev/docs/ui-components/templates/notion-like-editor>
- Component index — <https://tiptap.dev/docs/ui-components/components/overview>
- Slash dropdown menu — <https://tiptap.dev/docs/ui-components/components/slash-dropdown-menu>
- Drag context menu — <https://tiptap.dev/docs/ui-components/components/drag-context-menu>
- Link popover — <https://tiptap.dev/docs/ui-components/components/link-popover>
- Image upload node — <https://tiptap.dev/docs/ui-components/node-components/image-upload-node>
- Table node — <https://tiptap.dev/docs/ui-components/node-components/table-node>
- List dropdown — <https://tiptap.dev/docs/ui-components/components/list-dropdown-menu>
- Heading dropdown — <https://tiptap.dev/docs/ui-components/components/heading-dropdown-menu>
- Text align button — <https://tiptap.dev/docs/ui-components/components/text-align-button>
- Color highlight popover — <https://tiptap.dev/docs/ui-components/components/color-highlight-popover>
- Undo/redo button — <https://tiptap.dev/docs/ui-components/components/undo-redo-button>
- Emoji dropdown — <https://tiptap.dev/docs/ui-components/components/emoji-dropdown-menu>
- Mention dropdown — <https://tiptap.dev/docs/ui-components/components/mention-dropdown-menu>
- Suggestion menu util — <https://tiptap.dev/docs/ui-components/utils-components/suggestion-menu>
- Style setup — <https://tiptap.dev/docs/ui-components/getting-started/style>
- Node/mark extension docs (shortcuts + input rules), e.g.
  <https://tiptap.dev/docs/editor/extensions/nodes/heading>,
  `…/nodes/blockquote`, `…/nodes/code-block`, `…/nodes/bullet-list`,
  `…/nodes/ordered-list`, `…/nodes/task-list`, `…/nodes/horizontal-rule`,
  `…/marks/bold`, `…/marks/italic`, `…/marks/strike`, `…/marks/code`,
  `…/marks/underline`, `…/marks/highlight`, `…/marks/link`,
  `…/marks/subscript`, `…/marks/superscript`,
  <https://tiptap.dev/docs/editor/extensions/functionality/undo-redo>.

### Exact extension configuration of the template **[BUNDLE]**

```js
extensions: [
  StarterKit.configure({ undoRedo: false, horizontalRule: false,
                         dropcursor: { width: 2 }, link: { openOnClick: false } }),
  HorizontalRule,
  TextAlign.configure({ types: ["heading", "paragraph"] }),
  Collaboration.configure({ document: ydoc }),
  CollaborationCaret.configure({ provider, user }),
  Placeholder.configure({ placeholder, emptyNodeClass: "is-empty with-slash" }),
  Mention,
  Emoji.configure({ emojis: gitHubEmojis.filter(e => !e.name.includes("regional")),
                    forceFallbackImages: true }),
  TableKit.configure({ table: { resizable: true, cellMinWidth: 120 } }),
  NodeBackground.configure({ types: ["paragraph","heading","blockquote","taskList",
      "bulletList","orderedList","tableCell","tableHeader","tocNode"] }),
  NodeAlignment, TextStyle, Mathematics, Superscript, Subscript, Color,
  TaskList, TaskItem.configure({ nested: true }),
  Highlight.configure({ multicolor: true }),
  Selection, Image,
  TableOfContents.configure({ getIndex: getHierarchicalIndexes }),
  TableHandleExtension,
  ImageUploadNode.configure({ accept: "image/*", maxSize: MAX_FILE_SIZE, limit: 3, upload }),
  UniqueID.configure({ types: ["table","paragraph","bulletList","orderedList",
      "taskList","heading","blockquote","codeBlock","tocNode"] }),
  Typography, UiState, TocNode.configure({ topOffset: 48 }), Ai.configure({ … }),
]
// editorProps.attributes.class = "notion-like-editor"
// default placeholder prop value = "Start writing..."
```

Note: `undoRedo:false` because history is delegated to the collaboration/Yjs undo manager;
a non-collaborative Rust port must supply its own history with the same shortcuts (§4).
Collaboration/AI/mentions are out of scope for a local GPUI clone but the surfaces they
occupy (AI menu, presence carets) should be left as extension points.

---

## 1. Node types

Slash-command labels/subtexts/keywords below are verbatim from the bundle's
`SlashMenuItemType` table **[BUNDLE]**; input rules are the literal regexes from the
shipped extension code **[BUNDLE]**, matching the extension docs.

| Node | Slash label (subtext) | Markdown input rule | Shortcut | Enter | Backspace at start | Tab | Nesting |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `paragraph` | **Text** — "Regular text paragraph" (keywords `p, paragraph, text`) | — | `Mod-Alt-0` (badge shows `mod+alt+0`) | split into new paragraph | join with previous block; if first child of a wrapper (list item, blockquote) lift out one level | — (no-op unless in list) | top level and inside list item / blockquote / table cell |
| `heading` 1–6 (menu exposes 1–3) | **Heading 1** "Top-level heading" (`h, heading1, h1`); **Heading 2** "Key section heading" (`h2, heading2, subheading`); **Heading 3** "Subsection and group heading" (`h3, heading3, subheading`) | `/^(#{1,6})\s$/` → level = count of `#` | `Mod-Alt-1` … `Mod-Alt-6`; UI badge renders `ctrl+alt+N` | Enter at end creates a **paragraph** (not another heading); Enter mid-text splits into two headings | converts heading → paragraph, keeps text | — | block-level only |
| `bulletList`/`listItem` | **Bullet List** — "List with unordered items" (`ul, li, list, bulletlist, bullet list`) | `/^\s*([-+*])\s$/` | `Mod-Shift-8` | new list item; Enter on an empty item lifts/ends the list | empty item → lift out of list; non-empty first item → unwrap to paragraph | `Tab` sink item (indent), `Shift-Tab` lift item (outdent) | items nest arbitrarily; markers cycle `disc → circle → square → …` |
| `orderedList` | **Numbered List** — "List with ordered items" (`ol, li, list, numberedlist, numbered list`) | `/^(\d+)\.\s$/` (start attr = matched number) | `Mod-Shift-7` | as bullet list | as bullet list | as bullet list | numbering cycles `decimal → lower-alpha → lower-roman → …` |
| `taskList`/`taskItem` | **To-do list** — "List with tasks" (`tasklist, task list, todo, checklist`) | `/^\s*(\[([( \|x])?\])\s$/` — `[] ` unchecked, `[x] ` checked | `Mod-Shift-9` | new unchecked item; empty item exits list | empty item lifts out | `Tab`/`Shift-Tab` nest/unnest (`nested: true`) | nested allowed |
| `blockquote` | **Blockquote** — "Blockquote block" (`quote, blockquote`) | `/^\s*>\s$/` | `Mod-Shift-B` | new paragraph inside the quote; Enter on empty trailing paragraph exits | lifts the paragraph out of the quote | — | contains `block+`, so lists/headings can live inside |
| `codeBlock` | **Code Block** — "Code block with syntax highlighting" (`code, pre`) | `` /^```([a-z]+)?[\s\n]$/ `` (language captured) and `/^~~~([a-z]+)?[\s\n]$/` | `Mod-Alt-C` | newline inside the block; **triple Enter** exits (`exitOnTripleEnter: true`); `ArrowDown` at end exits (`exitOnArrowDown: true`) | deletes the code block if empty, else converts to paragraph preserving text | inserts a literal tab/indent inside block | leaf text block; no marks apply inside |
| `horizontalRule` | **Separator** — "Horizontal line to separate content" (`hr, horizontalRule, line, separator`) | `/^(?:---\|—-\|___\s\|\*\*\*)$/` | — | selects/creates a paragraph after the rule | deletes the rule (node selection then delete) | — | top-level only |
| `image` / `imageUpload` | **Image** — "Resizable image with caption" (`image, imageUpload, upload, img, picture, media, url`) | — (paste an image URL / drag-drop a file) | `Mod-Shift-I` inserts an `imageUpload` node | Enter with the node selected inserts a paragraph after | deletes the node | — | atomic block node with an editable caption child |
| `table` | **Table** — "Insert a table" (`table, insertTable`) | — | — | Enter inside a cell splits the paragraph in that cell | at cell start does nothing across the cell boundary | `Tab` next cell (creates a row at the end), `Shift-Tab` previous cell | cells hold `block+` |
| `mention` | **Mention** — "Mention a user or item" (`mention, user, item, tag`) | type `@` | `Mod-Shift-2` inserts the trigger | — | deletes the whole atom in one press | — | inline atom |
| `emoji` | **Emoji** — "Insert an emoji" (`emoji, emoticon, smiley`) | type `:` + shortcode | `Mod-Shift-E` | — | deletes whole atom | — | inline atom |
| `tocNode` | **Table of contents** — "Insert a table of contents" (`toc, tableofcontents, table of contents`) | — | — | — | deletes node | — | top-level block |
| AI items | **Continue Writing** "Continue writing from the current position"; **Ask AI** "Ask AI to generate content" | — | — | — | — | — | inserted only when the AI extension is available |

Slash-menu group assignment **[BUNDLE]**: `AI` → *Continue Writing, Ask AI*;
`Style` → *Text, Heading 1, Heading 2, Heading 3, Bullet List, Numbered List, To-do list,
Blockquote, Code Block*; `Insert` → *Mention, Emoji, Table, Separator, Table of contents*;
`Upload` → *Image*.

`Typography` (smart punctuation) is enabled, so `--`→en-dash, `...`→ellipsis,
`(c)`→©, quotes → curly quotes, etc. **[BUNDLE]**

---

## 2. Marks

Shortcut constants are the exact values used by the shipped `MarkButton`
(`{bold:"mod+b", italic:"mod+i", underline:"mod+u", strike:"mod+shift+s",
code:"mod+e", superscript:"mod+.", subscript:"mod+,"}`) **[BUNDLE]**.

| Mark | Shortcut | Input rule(s) **[BUNDLE]** | Paste rule | Toggle semantics |
| --- | --- | --- | --- | --- |
| bold | `Mod-B` | `(?:^\|\s)(\*\*(?!\s+\*\*)([^*]+)\*\*(?!\s+\*\*))$` and `__…__` | same regexes, global | toggle over selection; with empty selection sets stored mark for next typed char |
| italic | `Mod-I` | `(?:^\|\s)(\*(?!\s+\*)([^*]+)\*(?!\s+\*))$` and `_…_` | same | as bold |
| underline | `Mod-U` | — | — | as bold |
| strike | `Mod-Shift-S` | `(?:^\|\s)(~~(?!\s+~~)([^~]+)~~(?!\s+~~))$` | same | as bold |
| code (inline) | `Mod-E` | `` (?:^|\s)(`([^`]+)`)$ `` | same | excludes all other marks (`code` is exclusive); toggling off restores plain text |
| link | `Mod-Shift-K` **[ESTIMATED]** — the template exposes link only through the popover; `openOnClick:false` **[BUNDLE]** | autolink on typing a URL + space (StarterKit `Link` with autolink) | pasting a URL over a text selection wraps it in a link | mark with `href`, `target`, `rel`; `keepOnSplit:false`, `exitable:true` **[BUNDLE]** |
| highlight | no default shortcut (applied via popover/menu) | `(?:^\|\s)(==(?!\s+==)([^=]+)==(?!\s+==))$` | same | `multicolor:true` → mark carries a `color` attr; re-applying the same color removes it |
| text color | none (menu only) | — | — | `TextStyle` + `Color` — sets `style="color:…"` on a `textStyle` mark |
| superscript | `Mod-.` | — | — | mutually exclusive with subscript |
| subscript | `Mod-,` | — | — | mutually exclusive with superscript |

"Reset formatting" (`ResetAllFormattingButton`) clears every mark except `inlineThread`,
shortcut `mod+r` **[BUNDLE]**.

Color palettes are the Notion palette **[BUNDLE]** (`TEXT_COLORS` / `HIGHLIGHT_COLORS`,
labels verbatim: *Default text, Gray text, Brown text, Orange text, Yellow text, Green
text, Blue text, Purple text, Pink text, Red text*; highlights use the same 10 names with
"…background"):

| Name | Text (light) | Text (dark) | Highlight (light) | Highlight (dark) |
| --- | --- | --- | --- | --- |
| gray | `#787673` | `#9c9c9c` | `#f8f8f7` | `#2f2f2f` |
| brown | `#9d6a53` | `#b9856e` | `#f4eeee` | `#4a3228` |
| orange | `#c77d48` | `#d9730d` | `#fbecdd` | `#5c3b23` |
| yellow | `#ca922f` | `#ca994e` | `#fef9c3` | `#6b6524` |
| green | `#448361` | `#519e71` | `#dcfce7` | `#509568` |
| blue | `#327da9` | `#3699d3` | `#e0f2fe` | `#6e92aa` |
| purple | `#8f64af` | `#9e69d3` | `#f3e8ff` | `#583e74` |
| pink | `#c24c8b` | `#d15796` | `#fcf1f6` | `#4e2c3c` |
| red | `#d34a45` | `#df5553` | `#ffe4e6` | `#743e42` |

---

## 3. UI surfaces

### 3.1 Slash menu (`SlashDropdownMenu`)

- **Trigger**: character `/`, ProseMirror suggestion plugin key `slashDropdownMenu`,
  decoration class `tiptap-slash-decoration`, decoration content `"Filter..."` (shown as
  ghost text after the `/` while the query is empty) **[BUNDLE]**. Also openable from the
  `SlashCommandTriggerButton` (`+` button, `mod+/`, aria-label **"Insert block"**), which
  inserts `/` at the caret or in a fresh paragraph after the hovered block **[BUNDLE]**.
- **Filtering**: `filterSuggestionItems(items, query)` — case-insensitive match against
  `title` plus the `keywords` array; groups preserved in declaration order **[BUNDLE]**.
- **Item order** (default `enabledItems` = declaration order) **[BUNDLE]**:
  1. group **AI**: Continue Writing, Ask AI
  2. group **Style**: Text, Heading 1, Heading 2, Heading 3, Bullet List, Numbered List,
     To-do list, Blockquote, Code Block
  3. group **Insert**: Mention, Emoji, Table, Separator, Table of contents
  4. group **Upload**: Image
- Group labels are rendered as `CardGroupLabel` and groups separated by a horizontal
  `Separator`; `showGroups:false` flattens the list **[DOC]**
  (<https://tiptap.dev/docs/ui-components/components/slash-dropdown-menu>).
- Each row = ghost `Button` with leading icon (`tiptap-button-icon`) + `title` text.
  (Subtexts exist in the data model but the shipped row renders title only **[BUNDLE]**.)
- **Keyboard** (`useMenuNavigation`, vertical orientation, `autoSelectFirstItem: true`,
  `loopOnTab: true`) **[BUNDLE]**:
  `ArrowDown`/`ArrowUp` move with wraparound; `ArrowLeft/Right` ignored in vertical mode;
  `Tab`/`Shift-Tab` move forward/back with wrap; `Home`/`End` jump to first/last;
  `Enter` commits the selected item (ignored while IME composing); `Escape` closes.
  Selection resets to index 0 whenever the query changes. The selected row is scrolled
  into view using overflow detection against `[data-selector="tiptap-slash-dropdown-menu"]`.
- Items whose `check(editor)` fails are omitted (e.g. AI items without the AI extension,
  nodes not in the schema).
- Committing an item deletes the `/query` range then runs the action
  (`toggleHeading`, `toggleBulletList`, `insertContent({type:"imageUpload"})`, …).

Emoji (`:`) and mention (`@`) menus reuse the same suggestion/navigation machinery;
the emoji menu additionally has a search `Input` with placeholder
`"Add a emoji reaction..."` **[BUNDLE]** (sic, verbatim).

### 3.2 Selection (bubble/floating) toolbar

Rendered inside `FloatingElement`, `Toolbar variant="floating"`, shown when the selection
is non-empty and valid, hidden while dragging, while the AI panel is open, while a comment
input is open, and on viewports ≤ 480 px (mobile uses a fixed bottom toolbar instead)
**[BUNDLE]**.

Exact desktop order **[BUNDLE]**:

1. `ImproveDropdown` (AI) — `|` separator
2. `TurnIntoDropdown` — `|` separator
3. `MarkButton` **bold**, **italic**, **underline**, **strike**, **code** — `|` separator
4. `ImageNodeFloating` group (image-specific actions, only for image selections)
5. `LinkPopover` (`autoOpenOnLinkActive:false`), `ColorTextPopover`
6. conditional `|` + "more" popover containing **superscript**, **subscript** and the four
   text-align buttons (rendered only when any of those commands is available and `code`
   is not active)

**Turn into** menu items, verbatim and in order **[BUNDLE]**:
Text, Heading 1, Heading 2, Heading 3, Bulleted list, Numbered list, To-do list,
Blockquote, Code block. Convertible source types:
`paragraph, heading, bulletList, orderedList, taskList, blockquote, codeBlock`.

Mobile toolbar (≤ 480 px) is a fixed bar pinned above the keyboard; order **[BUNDLE]**:
slash trigger + node "more" menu | AI improve | bold/italic/strike/code | highlighter &
link sub-views (drill-in with a back arrow, titles "Text Highlighter" / "Link Editor") |
image floating | super/subscript | align (left, center, right, justify) | outdent/indent |
`ImageUploadButton` labelled **"Add"** | move node down / move node up.

### 3.3 Drag handle, "+" button and its menu (`DragContextMenu`)

- The gutter widget is a horizontal flex containing, left→right:
  the **`+` (SlashCommandTriggerButton)** with `data-weight="small"`, then the
  **grip handle** (`GripVerticalIcon`, `cursor: grab`) **[BUNDLE]**.
- Handle positioning: floating-ui with `mainAxis: 16` px offset from the block and
  `crossAxis: height > 40 ? 0 : height/2 - handleHeight/2` — i.e. top-aligned for tall
  blocks, vertically centred for short ones; CSS var `--drag-handle-main-axis-offset: 16px`
  and a `::before` hit-area strip of that width bridging handle→text so the hover doesn't
  flicker; `transition: top .2s ease-out` **[BUNDLE]**.
- Handle tooltip is two lines: **"Click for options"** / **"Hold for drag"** **[BUNDLE]**.
- Mouse-down on the handle selects the node and hides other floating UI; drag start sets
  `isDragging` (editor cursor becomes `grabbing`, toolbar hidden); drag end re-focuses the
  view on the next tick **[BUNDLE]**.
- The whole gutter is hidden (opacity 0, pointer-events none) while AI generation is
  active, on mobile breakpoints, or while a text selection is active **[BUNDLE]**.
- Menu opens to the **left** (`placement:"left"`), `minWidth: 15rem`, portalled, body
  scroll locked **[BUNDLE]**. Contents in order:
  1. `MenuGroupLabel` = current node display name (e.g. "paragraph", "Table")
  2. group: *TOC show-title toggle* (if applicable) · **Color** submenu · *Table align*
     submenu (tables) · *Fit to width* + **"Clear all contents"** (tables) ·
     **"Turn Into"** submenu (label "Turn into", same 9 items as §3.2) ·
     **Reset formatting** · *Download image* (images)
  3. separator, then: **Duplicate node** (`mod+d`), **Copy to clipboard** (`mod+c`),
     **Copy anchor link** (`mod+ctrl+l`) — each with a shortcut badge
  4. separator, **Ask AI**
  5. separator, **Delete** (badge: `backspace`)
  The **Color** submenu shows *Recent colors* (if any), then **"Text color"** (10 swatches)
  then **"Highlight color"** (10 swatches), each as its own labelled group separated by
  horizontal separators **[BUNDLE]**.

### 3.4 Link popover

- Trigger: link button in the floating toolbar; content is a `Card` with a horizontal
  item-group: URL `Input` `type="url"` placeholder **`"Paste a link..."`**, autofocused,
  `autoComplete/autoCorrect/autoCapitalize = off`; `Enter` applies **[BUNDLE]**.
- Buttons after the input: apply (`CornerDownLeftIcon`, title **"Apply link"**, disabled
  when the field is empty and no link is active), separator, then open-in-new-tab and
  remove-link buttons **[BUNDLE]**.
- On mobile the same content renders inline in the toolbar with `box-shadow:none; border:0`
  **[BUNDLE]**.
- `canSetLink` gates visibility: hidden for image node selections **[BUNDLE]**.

### 3.5 Image upload node

- Inserted as an `imageUpload` node; config `accept:"image/*"`, `limit: 3`,
  `maxSize: MAX_FILE_SIZE`, `onError` logs `"Upload failed:"` **[BUNDLE]**.
- Empty state: dashed drop zone, centred icon, text
  **"*Click to upload* or drag and drop"** (the "Click to upload" part italic `<em>`),
  subtext **"Maximum {limit} file(s), {maxSize/1024/1024}MB each."** **[BUNDLE]**.
- Per-file row while uploading: file icon, file name, human size (`Bytes/KB/MB/GB`,
  2-decimals), right side shows `{progress}%` and a close button; a progress bar element
  `.tiptap-image-upload-progress` is width-driven by the percentage **[BUNDLE]**.
- Drop zone states: `:hover` border tint, `.drag-active` (5 % accent wash),
  `.drag-over` (10 % accent wash); when the node is selected and the editor focused the
  border switches to the accent color **[BUNDLE]**.
- Uploading requires a server; Tiptap ships only the client node **[DOC]**.
- On success the node is replaced by an `image` node with an editable caption; the caption
  placeholder is rendered from `data-placeholder` in `#9ca3af` **[BUNDLE]**.
- Selected image: `outline: .125rem solid var(--tt-brand-color-500)`, radius `.25rem` **[BUNDLE]**.

### 3.6 Table controls

- `TableKit.configure({ table: { resizable: true, cellMinWidth: 120 } })` **[BUNDLE]**.
- Surfaces: `TableExtendRowColumnButtons` (the `+` bars to append a row/column),
  `TableHandle` (row/column grab handles; `.is-dragging` turns the handle brand-colored
  with a white icon and `cursor: grabbing`), and `TableSelectionOverlay` with
  `showResizeHandles: true` and a per-cell menu **[BUNDLE]**.
- Insert labels **[BUNDLE]**: rows — **"Insert row above"**, **"Insert row below"**;
  columns — **"Insert column left"**, **"Insert column right"**. Drag-handle menu adds
  *fit to width* and **"Clear all contents"**.
- `table { border-collapse: collapse; table-layout: fixed; width: 100% }`; cells without an
  explicit `colwidth` get `min-width: var(--default-cell-min-width)`; selected cells get an
  overlay `background: #c8c8ff66` **[BUNDLE]**.
- Cell navigation: `Tab`/`Shift-Tab` next/previous cell (Tab in the last cell appends a
  row); `Mod-Backspace`/`Delete`/`Mod-Delete` clear selected cells **[BUNDLE]**.

---

## 4. Keyboard map

`Mod` = ⌘ on macOS, Ctrl elsewhere.

| Shortcut | Command | Source |
| --- | --- | --- |
| `Mod-B` | toggle bold | [BUNDLE] |
| `Mod-I` | toggle italic | [BUNDLE] |
| `Mod-U` | toggle underline | [BUNDLE] |
| `Mod-Shift-S` | toggle strike | [BUNDLE] |
| `Mod-E` | toggle inline code | [BUNDLE] |
| `Mod-.` / `Mod-,` | superscript / subscript | [BUNDLE] |
| `Mod-Alt-0` | set paragraph ("Text") | [BUNDLE] |
| `Mod-Alt-1` … `Mod-Alt-6` | heading level 1–6 (UI badge `ctrl+alt+N`) | [BUNDLE] |
| `Mod-Shift-7` | toggle ordered list | [BUNDLE] |
| `Mod-Shift-8` | toggle bullet list | [BUNDLE] |
| `Mod-Shift-9` | toggle task list | [BUNDLE] |
| `Mod-Shift-B` | toggle blockquote | [BUNDLE] |
| `Mod-Alt-C` | toggle code block | [BUNDLE] |
| `Mod-Shift-L / E / R / J` | align left / center / right / justify | [BUNDLE] |
| `Mod-/` | open slash menu ("Insert block") | [BUNDLE] |
| `Mod-Shift-2` | insert mention trigger `@` | [BUNDLE] |
| `Mod-Shift-E` | insert emoji trigger `:` | [BUNDLE] |
| `Mod-Shift-I` | insert image-upload node | [BUNDLE] |
| `Mod-Shift-D` | download selected image | [BUNDLE] |
| `Mod-D` | duplicate node | [BUNDLE] |
| `Mod-C` | copy node to clipboard | [BUNDLE] |
| `Mod-Ctrl-L` | copy anchor link | [BUNDLE] |
| `Mod-R` | reset all formatting | [BUNDLE] |
| `Mod-Shift-ArrowUp` / `Mod-Shift-ArrowDown` | move node up / down | [BUNDLE] |
| `Backspace` (node selected) | delete node | [BUNDLE] |
| `Mod-Z` | undo | [BUNDLE] |
| `Mod-Shift-Z`, `Mod-Y` | redo | [BUNDLE] |
| `Enter` | split block; in lists new item; on empty list item lift; triple-Enter exits code block | [BUNDLE]/[DOC] |
| `Shift-Enter`, `Mod-Enter` | hard break (`setHardBreak`) | [BUNDLE] |
| `Mod-Enter` (ProseMirror base) | exit/split node near an atom when hard break isn't applicable | [BUNDLE] |
| `Tab` / `Shift-Tab` | list: sink / lift item; table: next / previous cell; menus: next / previous item | [BUNDLE] |
| `Backspace` at block start | join with previous block; lift out of list/quote; convert heading→paragraph; delete empty code block | [BUNDLE]/[DOC] |
| `Mod-Backspace`, `Shift-Backspace`, `Delete`, `Mod-Delete` | list-item / table-cell aware delete variants | [BUNDLE] |
| `ArrowUp`/`ArrowDown` | caret across blocks; `ArrowDown` at the end of a code block exits it; vertical gap cursor around atoms | [BUNDLE] |
| `ArrowLeft`/`ArrowRight` | caret; crosses node boundaries and selects atoms (mention/emoji deleted whole) | [BUNDLE] |
| `Mod-A` | select all (ProseMirror base keymap `selectAll`) | [ESTIMATED, standard] |
| `Escape` | close open menu/popover; return focus to the editor | [BUNDLE] |
| menu nav | `↑ ↓` move, `Tab`/`Shift-Tab` move with wrap, `Home`/`End` first/last, `Enter` select, `Escape` close | [BUNDLE] |

---

## 5. Visual style values

All values below are the literal shipped CSS custom properties and rules **[BUNDLE]**
unless tagged otherwise. `1rem` = 16 px.

### 5.1 Layout

```
.notion-like-editor-layout  grid
  --content-width : minmax(auto, 708px)
  --margin-width  : minmax(96px, 1fr)     /* ≤1024px: minmax(1rem, 1fr) */
  columns: [full-start] margin [content-start] content [content-end] margin [full-end]
  ≤768px: display:block (single column, content max-width 768px, centred)
.tiptap.ProseMirror.notion-like-editor  padding: 3rem 3rem 30vh     /* ≤480px: 1.5rem 1.5rem 30vh */
.notion-like-editor-header  sticky top:0, height 3rem, padding .5rem .75rem,
                            border-bottom 1px, z-index 50
.notion-like-editor-wrapper ≤480px: width 100vw; height 100vh; overflow auto
```

So: **content column 708 px max, ≥96 px side gutters (that gutter is where the drag handle
lives), 48 px top/side padding, 30vh bottom padding for click-below space.**

### 5.2 Typography

| Element | Values |
| --- | --- |
| editor font | `DM Sans, sans-serif` |
| paragraph | `font-size 1rem; font-weight 400; line-height 1.6`; `margin-top: 20px` for every `p` that is not first-child and not in a table cell |
| H1 | `font-size 1.5em; font-weight 700; margin-top 3em` |
| H2 | `font-size 1.25em; font-weight 700; margin-top 2.5em` |
| H3 | `font-size 1.125em; font-weight 600; margin-top 2em` |
| H4 | `font-size 1em; font-weight 600; margin-top 2em` |
| first block | any `h1–h4` that is first child (or follows a widget) gets `margin-top: 0` |
| inline code | `font-family "JetBrains Mono NL", monospace; font-size .875em; line-height 1.4; padding .1em .2em; border-radius .375rem; 1px border` |
| code block | `padding 1em; margin 1.5em 0; font-size 1rem; border 1px; border-radius .375rem` |
| list item paragraphs | `line-height 1.6; margin-top 0` for non-first paragraphs |
| AI prompt input | `Inter, sans-serif` |

Recommended GPUI substitutes when DM Sans/JetBrains Mono NL are unavailable
**[ESTIMATED]**: any geometric humanist sans (Inter) and any mono (JetBrains Mono).

### 5.3 Block spacing, lists, quotes, rules

```
ul, ol           margin: 1.5em 0; padding-left: 1.5em   (nested lists: margin 0)
                 first-child → margin-top 0; last-child → margin-bottom 0
ol markers       decimal → lower-alpha → lower-roman (cycles every 3 levels)
ul markers       disc(outside) → circle → square (cycles every 3 levels)
indent unit      --tt-indent-unit: 24px   (Indent extension / outdent-indent buttons)
blockquote       margin 1.5rem 0; padding .375em 0 .375em 1em;
                 ::before bar width .25em, full height, color --blockquote-bg-color
                 (light: --tt-gray-light-900 #222325; dark: --tt-gray-dark-900)
hr               height 1px; no border; color --horizontal-rule-color
                 (light --tt-gray-light-a-200, dark --tt-gray-dark-a-200)
pre              margin 1.5em 0
task checkbox    label padding-top .375rem; padding-right .5rem
                 box 1em × 1em, 1px border, radius .25rem, 80ms transition
                 checked: bg/border --tt-checklist-bg-active-color (gray-a-900),
                 check glyph .75em masked SVG in --tt-checklist-check-icon-color (white/black)
                 checked item text: opacity .5 + line-through
```

### 5.4 Color tokens

Grayscale (light theme opaque): `50 #fafafa, 100 #f4f4f5, 200 #eaeaeb, 300 #d5d6d7,
400 #a6a7ab, 500 #7d7f82, 600 #53565a, 700 #404145, 800 #2c2d30, 900 #222325`.

Light alpha ramp (`--tt-gray-light-a-*`): `50 #3838380a, 100 #0f16240d, 200 #25272d1a,
300 #2f323733, 400 #282c336b, 500 #34373ca3, 600 #24272ec7, 700 #23252ade,
800 #1e2024f2, 900 #1d1e20fa`.

Dark alpha ramp (`--tt-gray-dark-a-*`): `50 #e8e8fd0d, 100 #e7e7f312, 200 #eeeef61c,
300 #efeff538, 400 #f4f4ff5e, 500 #eceefd80, 600 #f7f7fda3, 700 #fbfbfebf,
800 #fdfdfde0, 900 #fffffff5`.

Brand: `50 #efeeff, 100 #dedbff, 200 #c3bdff, 300 #9d8aff, 400 #7a52ff, 500 #6229ff,
600 #5400e5, 700 #4b00cc, 800 #380099, 900 #2b1966, 950 #0d002e`.
`--white: #fff`, `--black: #0e0e11`.

| Role | Light | Dark |
| --- | --- | --- |
| page / card background | `#fff` | `#0e0e11` |
| body text | `--tt-gray-light-900 #222325` | `--tt-gray-dark-900` |
| muted / placeholder | `--tt-gray-light-a-400 #282c336b` | `--tt-gray-dark-a-400 #f4f4ff5e` |
| border | `--tt-gray-light-a-200 #25272d1a` | `--tt-gray-dark-a-200 #eeeef61c` |
| selection | `#7a52ff33` | `#9d8aff33` |
| caret | brand-500 `#6229ff` | brand-400 `#7a52ff` |
| link | brand-500 | brand-400 |
| inline-code bg / text / border | gray-a-100 / gray-a-700 / gray-a-200 | dark-a-100 / dark-a-700 / dark-a-200 |
| code-block bg / text / border | gray-a-50 / gray-a-800 / gray-a-200 | dark-a-50 / dark-a-800 / dark-a-200 |
| accent (active icon) | brand-500 | brand-400 |
| toolbar bg / border | `#fff` / gray-a-100 | `#0e0e11` / dark-a-50 |

Dark mode is a `.dark` class on an ancestor that re-points the same variables.

### 5.5 Radii, shadows, popovers, menus, buttons

```
--tt-radius-xxs .125rem  xs .25rem  sm .375rem  md .5rem  lg .75rem  xl 1rem
--tt-shadow-elevated-md (light):
   0 16px 48px #1118270a, 0 12px 24px #1118270a, 0 6px 8px #11182705, 0 2px 3px #11182705
--tt-shadow-elevated-md (dark):
   0 16px 48px #00000080, 0 12px 24px #0000003d, 0 6px 8px #00000038, 0 2px 3px #0000001f

.tiptap-card              padding var(--padding)=.375rem; border 1px;
                          border-radius calc(.375rem + .75rem) = 1.125rem;
                          box-shadow elevated-md; column flex
.tiptap-card-header/body  padding .375rem  (header has bottom border)
.tiptap-card-group-label  font-size .75rem; weight 600; padding .75rem .5rem .25rem;
                          text-transform capitalize
.tiptap-card-item-group   vertical: column; horizontal: row + gap .25rem
.tiptap-menu-content      same radius formula; max-width 16rem; min-width = anchor width;
                          padding .375rem; margin-block .375rem; overflow-y auto;
                          max-height var(--popover-available-height); z-index 50
drag-menu list            min-width 15rem
.tiptap-toolbar           row flex, gap .25rem
.tiptap-toolbar-group     row flex, gap .125rem  (empty groups + their separators hidden)
.tiptap-toolbar[floating] padding .188rem; border 1px; radius calc(.125rem+.75rem+1px);
                          box-shadow elevated-md
.tiptap-toolbar[fixed]    min-height var(--tt-toolbar-height)=2.75rem; bottom-border
.tiptap-button            height 2rem; min-width 2rem; padding .5rem; gap .25rem;
                          radius .75rem; font-size .875rem; weight 500; line-height 1.15
.tiptap-button[data-weight=small]  width/min-width 1.5rem; no horizontal padding
.tiptap-separator         horizontal: height 1px, margin .5rem 0; vertical: 1px × 1.5rem
.tiptap-tooltip           radius .375rem; padding .375rem .5rem; font-size .75rem; weight 500;
                          bg gray-light-900 / text white (inverted in dark);
                          shadow 0 4px 6px -1px #0000001a
```

Button state colors (ghost variant, light) **[BUNDLE]**: default text/icon
`gray-a-600`; hover text/icon `gray-a-900` with hover bg `gray-light-200`;
active icon = `brand-500`, active bg `gray-a-200`; disabled text/icon `gray-a-400`,
disabled bg `gray-a-50`. Menu/dropdown row height therefore equals the button height,
**2 rem (32 px)**, with `.375rem` container padding — total popover row pitch ≈ 32 px.

### 5.6 Drag handle geometry

```
gutter widget = [ + button (1.5rem wide, small weight) ][ grip button (1.5rem) ]
placement     : left of the block, mainAxis offset 16px
vertical      : block height > 40px → top aligned; else centred on the block
hit bridge    : .drag-handle::before  width 16px (var --drag-handle-main-axis-offset),
                height 100%, left:100%  (keeps hover alive between handle and text)
motion        : transition top .2s ease-out
cursor        : grab (grabbing while dragging, applied to the whole editor)
```

### 5.7 Exact strings

| String | Where |
| --- | --- |
| `"Write, type '/' for commands…"` (italic) | empty paragraph placeholder when the node also has class `with-slash` **[BUNDLE]** |
| `"Start writing..."` | default `placeholder` prop of `<NotionEditor>` **[DOC]/[BUNDLE]** |
| `"Write something …"` | Placeholder extension's own default (overridden here) **[BUNDLE]** |
| `"Filter..."` | ghost decoration after a freshly typed `/` **[BUNDLE]** |
| `"Insert block"` | aria-label/tooltip of the `+` button **[BUNDLE]** |
| `"Click for options"` / `"Hold for drag"` | grip tooltip (two lines) **[BUNDLE]** |
| `"Paste a link..."` / `"Apply link"` | link popover **[BUNDLE]** |
| `"Click to upload"` + `" or drag and drop"` | image upload empty state **[BUNDLE]** |
| `"Maximum {n} file(s), {m}MB each."` | image upload subtext **[BUNDLE]** |
| `"Add a emoji reaction..."` | emoji menu search input **[BUNDLE]** |
| `"Turn into"`, `"Turn Into"` | toolbar dropdown label / drag-menu submenu trigger **[BUNDLE]** |
| `"Text color"`, `"Highlight color"`, `"Recent colors"` | color menu group labels **[BUNDLE]** |
| `"Duplicate node"`, `"Copy to clipboard"`, `"Copy anchor link"`, `"Reset formatting"`, `"Ask AI"`, `"Delete"` | drag context menu items **[BUNDLE]** |
| `"Align left" / "Align center" / "Align right" / "Align justify"` | text-align button labels **[BUNDLE]** |
| `"Undo" / "Redo"`, `"Move Up" / "Move Down"` | button labels **[BUNDLE]** |
| `"Insert row above/below"`, `"Insert column left/right"`, `"Clear all contents"` | table controls **[BUNDLE]** |

---

## 6. Subtle behaviors

1. **Placeholder rules.** The Placeholder extension marks empty nodes with
   `is-empty with-slash`; CSS renders `content: attr(data-placeholder)` only when the node's
   sole child is a `ProseMirror-trailingBreak`. With the `with-slash` class the content is
   forcibly the literal italic string `Write, type '/' for commands…`; without it, the
   node's own `data-placeholder`. Placeholder text is absolutely positioned with
   `height: 0; width: 100%; pointer-events: none; text-align: inherit`, colored
   `--placeholder-color`. `showOnlyCurrent` default means only the block containing the
   caret shows a placeholder. **[BUNDLE]**
2. **Trailing node / click below.** A `trailingNode` plugin appends a paragraph whenever the
   last node in the doc is not already a paragraph (configurable `notAfter`), so there is
   always a place to type below an image/table/code block. Combined with the `30vh` bottom
   padding, clicking in empty space below the document lands the caret in that trailing
   paragraph. **[BUNDLE]**
3. **Node selection highlight.** `.ProseMirror-selectednode` (excluding `img`, `pre`, and
   React node views) gets `background: var(--tt-selection-color)` and `border-radius: .5rem`.
   Images instead get a 2 px brand outline. **[BUNDLE]**
4. **Selection persistence when the editor blurs.** The `Selection` extension paints an
   inline decoration with class `selection` (same translucent brand color) whenever the doc
   has a non-empty selection but the view is not focused and is not dragging; it also clears
   the native DOM selection on blur and restores focus on refocus. This is what makes the
   selection stay visible while you interact with a popover. **[BUNDLE]**
5. **Multi-block selection.** Standard ProseMirror text selection across blocks; commands in
   the toolbar operate over all ranges. `CellSelection` is a distinct selection class for
   tables (node display name resolves to `"Table"`). Drag handle hides while a text
   selection is active. **[BUNDLE]**
6. **Paste / markdown handling.** Mark and node input rules (§1, §2) fire on typing; the
   equivalent *paste* rules (global-flag variants of the same regexes) fire on paste, so
   pasting `**bold**` yields bold text. Pasting a URL over a selection creates a link.
   Autolink converts typed URLs. Images pasted/dropped as files route into the
   `imageUpload` node. Smart typography (`Typography`) rewrites quotes, dashes, ellipses
   and `(c)/(r)/(tm)` as you type. **[BUNDLE]**
7. **Undo grouping.** The template disables StarterKit history (`undoRedo:false`) and relies
   on the Yjs undo manager; a standalone port should use ProseMirror-style history with
   `newGroupDelay ≈ 500 ms`, grouping adjacent typing into one step and breaking the group
   on selection jumps or non-typing commands **[ESTIMATED]**. Some commands explicitly
   detach from history — e.g. delete-node dispatches with `closeHistory(tr)` so the deletion
   is its own undo step, and initial demo content is inserted with
   `setMeta("addToHistory", false)` **[BUNDLE]**.
8. **UI state coordination.** A `UiState` extension carries `isDragging`,
   `lockDragHandle`, `aiGenerationActive`, `commentInputVisible`; the floating toolbar and
   drag handle read it to hide themselves, and opening the drag menu locks the handle in
   place. On drag end the view is blurred and refocused on the next tick to reset the
   browser's drag state. **[BUNDLE]**
9. **Stable node IDs.** `UniqueID` stamps `data-id` on tables, paragraphs, lists, headings,
   quotes, code blocks and TOC nodes — this backs "Copy anchor link" and the TOC/scroll-to-hash
   behavior. **[BUNDLE]**
10. **Mobile adaptation.** ≤ 480 px: floating toolbar is replaced by a fixed bottom toolbar
    positioned above the virtual keyboard via a cursor-visibility hook, sub-views drill in
    with a back arrow, and popovers drop their shadow/border. Drag handle is disabled
    ≤ 768 px. **[BUNDLE]**
11. **Suggestion plugins never open inside images** — `allow()` walks the resolved position
    and rejects when any ancestor is an `image` node. **[BUNDLE]**

---

## 7. Parity checklist

Graded items; each should be independently verifiable in the GPUI port.

**Document model**

1. Nodes: paragraph, heading 1–6, bulletList/listItem, orderedList, taskList/taskItem
   (nested), blockquote, codeBlock, horizontalRule, image (+caption), imageUpload, table
   (resizable, cellMinWidth 120), hardBreak, and inline atoms mention/emoji.
2. Marks: bold, italic, underline, strike, code (exclusive), link, highlight (multicolor),
   textStyle color, superscript, subscript (mutually exclusive with superscript).
3. Every node type in the `UniqueID` list carries a stable id.

**Input rules**

4. `#`×1–6 + space → heading; `-`/`+`/`*` + space → bullet; `1.` + space → ordered;
   `[] `/`[x] ` → task item; `> ` → blockquote; ``` ```lang ``` / `~~~lang` → code block;
   `---` → horizontal rule.
5. `**x**`, `__x__`, `*x*`, `_x_`, `~~x~~`, `` `x` ``, `==x==` produce the corresponding
   marks on typing **and** on paste.
6. Smart typography: `--`, `...`, quotes, `(c)`.

**Keyboard**

7. All 30+ bindings in §4 mapped, with `Mod` = platform modifier and the same badges shown
   in tooltips/menus.
8. Enter semantics: split; heading→paragraph at end; empty list item lifts; triple-Enter
   exits code block; Shift-Enter/Mod-Enter hard break.
9. Backspace at block start: join/lift/unwrap/heading-downgrade; whole-atom deletion of
   mention/emoji; node-selected Backspace deletes the node as its own undo step.
10. Tab/Shift-Tab: list sink/lift, table cell traversal (Tab at end appends a row), menu
    navigation with wrap.
11. Undo/redo with typing coalescing; `Mod-Y` also redoes.

**Slash menu**

12. Opens on `/` at the start of an empty textblock or after whitespace; also via `Mod-/`
    and the gutter `+` button.
13. Shows the ghost `Filter...` decoration; filters on title + keywords; hides unavailable
    items.
14. Exact group order AI → Style → Insert → Upload, with group labels and separators, and
    exact item titles listed in §3.1.
15. ↑/↓/Tab/Shift-Tab/Home/End/Enter/Escape behave exactly as §3.1, first item preselected,
    selection resets on query change, selected row auto-scrolled into view.
16. Committing deletes the `/query` text before running the command.

**Selection toolbar**

17. Appears only for non-empty valid selections; hidden while dragging, during AI, and on
    small viewports.
18. Exact order: AI improve | Turn into | B I U S Code | image actions | link, text color |
    (more: super/subscript, 4 alignments).
19. Turn-into list shows exactly the 9 labels in §3.2 and reflects the active type.
20. Buttons show active state with brand-colored icons.

**Gutter**

21. `+` and grip appear on hover in the left margin, 16 px from the block, top-aligned for
    blocks taller than 40 px, centred otherwise.
22. Grip tooltip has the two lines "Click for options" / "Hold for drag".
23. Dragging reorders blocks; editor cursor becomes `grabbing`; handle hides during text
    selection.
24. Context menu opens to the left, ≥15 rem wide, with the node display name as its label
    and the groups/items/shortcut badges of §3.3, including the Color submenu with
    Recent/Text/Highlight groups.

**Popovers / nodes**

25. Link popover: URL input with `Paste a link...`, Enter applies, apply/open/remove
    buttons, disabled apply when empty and no active link.
26. Image upload node: dashed drop zone, `Click to upload or drag and drop`, limit/size
    subtext, per-file progress rows with percentage and remove button, drag-active and
    drag-over states, accent border when selected.
27. Table: resize handles, row/column extend buttons, insert-above/below/left/right labels,
    selected-cell overlay `#c8c8ff66`, clear contents.
28. Emoji (`:`) and mention (`@`) menus share the slash-menu keyboard contract.

**Behavior**

29. Placeholder shows only in the focused empty block, italic `Write, type '/' for commands…`.
30. A trailing paragraph always exists after a non-paragraph last node; clicking below the
    content focuses it.
31. Node selection highlight = translucent brand fill, 8 px radius (images get a 2 px brand
    outline instead).
32. Selection stays visible when focus moves to a toolbar/popover.

**Visual**

33. Content column max 708 px, side gutters ≥ 96 px, padding 48/48/30vh (24 px at ≤480 px).
34. Paragraph 16 px / 1.6 / 400 with 20 px top margin; H1 1.5em/700/3em-top,
    H2 1.25em/700/2.5em-top, H3 1.125em/600/2em-top; first heading has no top margin.
35. Lists: 1.5em vertical margin, 1.5em left padding, marker cycles, 24 px indent unit.
36. Blockquote: 0.25em left bar in gray-900, 1em left padding, 1.5rem vertical margin.
37. Code block: 1em padding, 1px border, 6 px radius, mono 16 px; inline code 0.875em with
    tinted bg + border.
38. Task checkbox 1em square, 0.25rem radius, checked fill gray-a-900 with white check,
    checked text 50 % opacity + strike.
39. Light and dark palettes match the token table in §5.4, including selection `#7a52ff33` /
    `#9d8aff33` and the brand accent `#6229ff` / `#7a52ff`.
40. Popovers/menus: 1.125 rem radius, 0.375 rem padding, elevated-md 4-layer shadow,
    2 rem-high rows, ghost hover background, 0.75 rem/600 group labels, 1 px separators with
    0.5 rem vertical margin.
41. Floating toolbar: 0.188 rem padding, 0.875 rem radius, 2 rem buttons, 0.125 rem intra-group
    gap, 1 px × 1.5 rem vertical separators.
42. Tooltips: dark pill, 0.375 rem radius, 12 px/500 text, shortcut rendered as a `kbd` badge.
