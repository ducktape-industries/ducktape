# Parity grading against the Tiptap Notion-like editor template

> **Since this was graded**, `style.rs` has become
> [`theme.rs`](../src/editor/theme.rs): every metric is a multiple of the theme's base
> font size and every colour is a token, so the page, gutter and typography numbers
> quoted below are now defaults rather than constants. Item 39's palettes are the
> template's own (see `assets/themes/notion.json` and `apply_template_palette`).

Graded against `docs/research/tiptap-notion-spec.md` §7 (42 items), §1–§6 for detail.

**Basis.** Re-graded against `git` HEAD `6a08ddcd32d070f088b2bd717fb67414783c7aa4`
(clean working tree). `cargo test` on that tree: **54/54** UI tests in
`tests/editing.rs` and **15/15** unit tests in `src/editor/mark.rs` pass. Line
numbers are valid for that commit.

Statuses are strict: `Done` means the code does what §7 asks, not that something
adjacent exists.

---

## 1. What is actually implemented

The document is a **flat `Vec<Block>` owned by one `NotionEditor` entity**
(`view.rs:34-70`); each `Block` (`block.rs:404-422`) is a node-type name +
`BlockAttrs` + a `String` mirror of its text + a `MarkList`, and owns its own
`Entity<EditorState>` — i.e. every block is a separate gpui-kit multi-line input,
not one document buffer. Inline formatting is **not** in the text: marks are byte
ranges in `MarkList` (`mark.rs:140-193`) that are flattened into non-overlapping
runs and pushed into the input's Monaco-style decoration layer as
`TextDecoration`/`HighlightStyle` (`view.rs:533-562`, `view.rs:886-938`), with the
text mirror re-diffed after every `InputEvent::Change` (`view.rs:435-488`,
`mark.rs:443-473`) so marks travel with edits. Node types are **data**: a
`BlockSpec` trait (`block.rs:240-325`) supplies label, caps, layout, leading
marker, wrapper chrome, split-into type (caret-aware, `block.rs:305`), markdown
input rules and slash-menu entries, and a `BlockRegistry` global (`block.rs:327-400`, registrations in
`blocks.rs:27-41`) is the single source for the slash menu, the input-rule engine
and "turn into" — adding a node type is one `impl` plus one `register` call. A spec
that sets `BlockCaps::grid()` (`block.rs:135`) also gets a `CellGrid` of child
`EditorState`s kept beside the block (`grid.rs:45-95`, `grid.rs:100-215`, `view.rs:50`), which is how
the table node holds its cells. Document-level undo is a snapshot stack over
`Vec<BlockContent>` with 500 ms typing coalescing (`history.rs:17-134`), because the
per-input undo stacks cannot see structural edits.

---

## 2. The 42 checklist items

| # | Item | Status | Evidence | Gap |
| --- | --- | --- | --- | --- |
| 1 | Node set (para, h1–6, lists, task, quote, code, hr, image(+caption), imageUpload, table, hardBreak, mention/emoji atoms) | Partial | `blocks.rs:27-41` registers paragraph/heading/bullet/ordered/task/quote/code/hr/image/callout/toggle/**table**; `block.rs:29-42`; hard break = `shift-enter` newline inside the block, test `shift_enter_breaks_the_line_inside_a_block`; mention/emoji reachable from the slash menu and from `Mod-Shift-2`/`Mod-Shift-E` (`slash.rs:433-450`, `actions.rs:81-82`) | No separate `imageUpload` node, no image caption child, no `tocNode`. Mention/emoji are not inline atoms (mention = a mark over plain text `slash.rs:299-309`; emoji = a literal character). Headings 4–6 reachable only via the `####` input rule (`blocks.rs:121-129`), no menu/shortcut. Extra non-spec nodes: `callout`, `details` toggle. |
| 2 | Mark set incl. exclusive `code`, multicolor highlight, textStyle color, super/subscript | Partial | `mark.rs:15-31` (all 10 kinds + `Mention` + `Comment`); exclusivity of super/sub in `commands.rs:176-184`; multicolor `mark.rs:68-100`; color `mark.rs:103-144` | `code` is not exclusive: toggling it neither strips nor blocks other marks (`commands.rs:172-174` is a plain `toggle_mark`), it is only non-inclusive for continued typing (`mark.rs:59-64`). Superscript/subscript apply **no rendering at all** — `view.rs:933` is an empty match arm (see §4: impossible on this stack). |
| 3 | Stable node ids on every `UniqueID` type | Partial | `block.rs:24` `BlockId(u64)`, minted in `view.rs:272-276`, used by focus/drag/selection/grids | Ids are runtime-only: `BlockContent` (`block.rs:461-468`) has no id, so ids are not serialized and undo/redo mints new ones (`history.rs:107-134`). No anchor-link feature depends on them. |
| 4 | Node input rules `#`, `-/+/*`, `1.`, `[] / [x] `, `> `, ```` ```lang ````/`~~~`, `---` | Done | `blocks.rs:121-129` (h1–6), `:218-223`, `:282-296` (start attr), `:418-435`, `:499-504`, `:597-626` (language captured, and offered again by the code block's language menu `blocks.rs:552-595`, `:920-925`), `:675-680`; engine `input_rules.rs:154-197`; tests `markdown_rules_create_nodes`, `enter_inside_a_code_block_adds_a_line` | (Tests cover `#`, `-`, `1.`, `>` and ``` only; task/hr rules are implemented but untested.) |
| 5 | Mark input rules `**`,`__`,`*`,`_`,`~~`,`` ` ``,`==` on typing **and** on paste | Partial | `input_rules.rs:38-67`, applied at `:199-238`; tests `inline_rules_apply_marks`, `typing_after_an_inline_rule_is_plain` | Paste runs **node** rules only: `split_pasted_lines` (`view.rs:490-531`) calls `apply_markdown_prefix` (`input_rules.rs:91-127`), which never touches `INLINE_RULES`. Pasting `**x**` leaves the asterisks. |
| 6 | Smart typography `--`, `...`, quotes, `(c)` | Partial | `input_rules.rs:70-85` (`(c) (r) (tm) ... <- -> -- != <= >= +/-`), applied `:240-259` | No curly-quote substitution — the one rule of the four named in §7 that is missing. |
| 7 | All 30+ §4 bindings, `Mod` = platform modifier, badges in tooltips/menus | Partial | `actions.rs:68-112` (31 bindings, `secondary-` = platform Mod), incl. `secondary-shift-e` emoji, `secondary-shift-2` mention, `secondary-shift-i` insert image (`:81-83`) and the non-template `secondary-shift-m` comment (`:80`); handlers `keymap.rs:56-229` | Still missing: `Mod-Alt-4/5/6`, `Mod-Shift-L/E/R/J` align, `Mod-Shift-D` download, `Mod-Ctrl-L` copy anchor, `Mod-Enter`. **No shortcut badges anywhere** — menus are built with plain `.menu(label, action)` (`gutter.rs:222-284`, `toolbar.rs:150-171`) and tooltips are bare strings. |
| 8 | Enter: split; heading→paragraph at end; empty list item lifts; triple-Enter exits code; Shift/Mod-Enter hard break | Partial | `keymap.rs:231-274`; split `commands.rs:484-531` with `at_end` computed at `:515-516`; `BlockSpec::split_into(attrs, at_end)` (`block.rs:305`) and `Heading::split_into` (`blocks.rs:113-119`) keep a mid-text split a heading and start a paragraph at the end; lift `commands.rs:495-504`; triple-Enter `keymap.rs:249-262`; ArrowDown exit `keymap.rs:367-373`; tests `enter_in_the_middle_of_a_heading_keeps_both_halves_headings`, `enter_splits_a_block_at_the_caret`, `enter_continues_a_list_and_an_empty_item_leaves_it`, `enter_inside_a_code_block_adds_a_line`, `arrow_down_leaves_a_code_block_at_the_end_of_the_document`, `shift_enter_breaks_the_line_inside_a_block` | Only `Mod-Enter` (the second hard-break spelling) is still unbound. |
| 9 | Backspace at block start: join/lift/unwrap/heading-downgrade; whole-atom delete; node-selected delete as its own undo step | Partial | `keymap.rs:276-297`; `commands.rs:532-571` (lift → downgrade → join, deletes a preceding atom node); block-selection delete `selection.rs:103-128` with `Step::Structural` (`history.rs:66-85`); tests `backspace_at_the_start_joins_with_the_previous_block`, `shift_down_selects_whole_blocks_and_backspace_removes_them` | No whole-atom deletion: a mention is ordinary text under a mark, so Backspace removes one character of the name. |
| 10 | Tab/Shift-Tab: list sink/lift, table cells (Tab at end appends a row), menu nav with wrap | Done | `keymap.rs:414-462`; list sink/lift `commands.rs:600-683` (now over every selected block); **cell traversal** `keymap.rs:424-429`/`:449-454` → `grid.rs:402-433`, where running past the last cell inserts a row and lands in it; menu wrap `slash.rs:256-266` (`rem_euclid`); tests `tab_nests_a_list_item_and_shift_tab_lifts_it`, `the_slash_menu_inserts_a_table_and_tab_walks_its_cells`, `tab_in_the_last_cell_adds_a_row` | (Sink is capped at "one deeper than the item above", `commands.rs:623-650`, which matches ProseMirror.) |
| 11 | Undo/redo with typing coalescing; `Mod-Y` also redoes | Done | `history.rs:17-134` (500 ms `COALESCE`, `Step::Typing` vs `Structural`); `actions.rs:110-111` binds `secondary-shift-z` and `secondary-y`; `keymap.rs:79-86` captures `Undo`/`Redo` from the inputs; tests `undo_reverts_typing_and_redo_restores_it`, `undo_reverts_a_split`, `undo_reverts_a_node_change`, `redo_through_the_api` | — |
| 12 | Slash opens on `/` at start or after whitespace; also `Mod-/` and gutter `+` | Done | `slash.rs:128-190` (word-boundary check at `:172-175`); `Mod-/` → `actions.rs:79` → `slash.rs:85-119` (`open_suggestion` is the generic opener for `/`, `@` and `:`); `+` button `gutter.rs:96-101` → `gutter.rs:127-132`; tests `the_slash_menu_filters_and_runs_an_item`, `the_gutter_plus_adds_a_block_below`, `a_colon_in_prose_does_not_hold_a_menu_open` | — |
| 13 | Ghost `Filter...` decoration; filter on title + keywords; hide unavailable items | Partial | filter `slash.rs:193-254` + `suggestion.rs:118-126` (case-insensitive title **and** keywords) | The `Filter...` string exists (`suggestion.rs:41-46`) but `Trigger::hint()` **has no caller** — no ghost decoration is ever rendered. There is no per-item `check(editor)`, so nothing can be conditionally hidden. |
| 14 | Group order AI → Style → Insert → Upload, labels, separators, exact §3.1 titles | Partial | `GROUP_ORDER` + `group_rank` (`slash.rs:63-70`) and the stable sort at `slash.rs:222` put groups in the template's order while items keep registry order; one header per group with a 1 px separator between groups (`slash.rs:345-368`); `TRIGGER_ITEMS` adds the verbatim **Mention** and **Emoji** entries in group Insert (`slash.rs:433-450`); titles/keywords/groups per spec in `blocks.rs:59-68, 131-158, 225-234, 298-307, 437-446, 506-515, 628-637, 682-691, 772-781` and the Table entry `blocks.rs:1013-1022`; test `the_slash_menu_groups_items_in_template_order` asserts Style → Insert → Upload | Still missing: the AI group (Continue Writing, Ask AI) and **Table of contents**. Extra items the template does not have: Callout, Toggle list (both sorted into Insert/Style). |
| 15 | ↑/↓/Tab/Shift-Tab/Home/End/Enter/Escape; first item preselected; reset on query change; scroll into view | Partial | ↑/↓ `keymap.rs:322-373`, Tab/Shift-Tab `:414-462`, Enter `:237-241`, Escape `:502-542`; preselect `slash.rs:111-117`; reset on query change `slash.rs:148-152`; tests `escape_closes_the_slash_menu_and_keeps_the_text`, `the_emoji_entry_of_the_slash_menu_opens_the_emoji_menu` | `Home`/`End` are not intercepted — they move the caret and re-scope the query instead of jumping to first/last. No scroll-into-view for the selected row (the list is `overflow_y_scroll` with `max_h(360px)`, `slash.rs:418-420`, but nothing scrolls it). |
| 16 | Committing deletes the `/query` text before running the command | Done | `slash.rs:268-312` — `edit_block_text(menu.start..end, "")` then `run(self, …)`; test `the_slash_menu_filters_and_runs_an_item` asserts the block text is `""` after commit | — |
| 17 | Toolbar only for non-empty valid selections; hidden while dragging / during AI / on small viewports | Partial | `toolbar.rs:32-54`: non-empty range **and** the block's spec allows marks, or a whole-block selection; hidden while a suggestion menu is open, while a drag is tracked, and — new — while the mouse button is still down (`press_in_progress`, `selection.rs:175-177`), so it appears on release; anchored over the first selected block for block selections (`toolbar.rs:118-128`); tests `the_toolbar_appears_over_a_selection`, `the_toolbar_waits_for_the_button_to_come_up` | "Dragging" is still approximated by `drop_target` (`gutter.rs:147-167`); there is no `isDragging`/`UiState`. The toolbar is **not** hidden while the comment input is open, which §3.2 requires. No viewport rule (no mobile layout at all), no AI state. |
| 18 | Order: AI improve \| Turn into \| B I U S Code \| image actions \| link, color \| more(super/sub, 4 aligns) | Partial | `toolbar.rs:70-102` — Turn into ¦ B I U S Code ¦ **Comment**, link, color ¦ more; `more` = super/sub + Reset formatting (`toolbar.rs:183-197`); the `more` group is suppressed while `code` is active, as in the template | No AI improve, no image-specific group. The four alignment buttons are absent — see §4, alignment is not available on this stack. `Reset formatting` sits in `more` (the template puts it in the drag menu), and the Comment button (`toolbar.rs:89-96`, `comments.rs:124-152`) is an addition the template has no equivalent for — see §5. |
| 19 | Turn-into shows exactly the 9 §3.2 labels and reflects the active type | Done | `toolbar.rs:160-168` — Text, Heading 1/2/3, Bulleted list, Numbered list, To-do list, Blockquote, Code block, verbatim and in order; trigger label = active node's display name (`toolbar.rs:151`, `:173-181`); with several blocks selected the conversion applies to all of them (`commands.rs:285-322`, test `turning_selected_blocks_into_a_list_changes_all_of_them`) | (The active type is shown on the trigger, not as a checked row.) |
| 20 | Buttons show active state with brand-colored icons | Partial | `toolbar.rs:137-148` `.selected(is_mark_active)`, link `:199-209` | The active look is gpui-kit's `Button` selected styling; no brand token is used anywhere in the editor (`rg 6229ff|7a52ff` over `src/` returns nothing). |
| 21 | `+` and grip on hover in the left margin, 16 px off the block, top-aligned >40 px else centred | Partial | `gutter.rs:60-118`: `.invisible().group_hover(group_name(id), …)`; the block row reaches into the margin (`view.rs:810-811`, `ml(-52px)/pl(52px)`) so the hover area bridges handle→text; the controls now track the block's indentation (`gutter.rs:80-82`) | Placed at `left(4px + 24px × indent)` inside that 52 px reach, not a 16 px main-axis offset from the text. Vertical position is always "centred on the first line" (`gutter.rs:76`), which approximates but does not implement the >40 px rule. No `transition: top .2s`. |
| 22 | Grip tooltip is two lines "Click for options" / "Hold for drag" | Partial | `gutter.rs:111` `.tooltip("Click for options, hold for drag")` | One line, comma-joined; gpui-kit's tooltip takes a string. |
| 23 | Drag reorders; editor cursor `grabbing`; handle hides during text selection | Partial | drag payload + preview `gutter.rs:286-294`, `:37-58`; tracking `:147-167`; drop `:169-188`; indicator `:190-209`; reorder `commands.rs:723-736` (and `reorder_blocks` for a multi-block drag, `:737-767`); tests `dragging_the_handle_reorders_blocks`, `a_block_can_be_dragged_below_another` | No `grabbing` cursor on the editor, no `grab` cursor on the handle. The gutter does not hide while a text selection is active. |
| 24 | Drag menu: opens left, ≥15 rem, node name label, §3.3 groups incl. Color submenu, shortcut badges | Partial | `gutter.rs:222-284`: label = node display name, **Color submenu** (`:253-256` → `color_menu` `:296-303`, labelled "Text color" / separator / "Highlight color"), a **Table submenu** on grid blocks (`:236-250`: Insert row above/below, Insert column left/right, Delete row/column — the §3.6 labels verbatim), Turn into submenu (`:257-274`, 9 items), Reset formatting, ¦ Duplicate, Copy to clipboard, ¦ Move up/down, ¦ Delete | Default popover placement, no `min-width: 15rem`. The Color submenu has the 10 text colors but only 7 highlights (`mark.rs:73`) and no "Recent colors" group; swatch rows are plain labels here (the toolbar's `swatch_row` is not reused). Still missing: Copy anchor link, Ask AI, Download image, and every shortcut badge. "Duplicate" not "Duplicate node". |
| 25 | Link popover: `Paste a link...`, Enter applies, apply/open/remove, apply disabled when empty and no active link | Partial | `toolbar.rs:242-282` (seeded from the existing href, autofocus + select-all), Enter via `InputEvent::PressEnter` `:261-269`, render `:324-385`, apply `:346-353`, remove `:355-373`, `normalize_href` `:405-411`; test `the_link_editor_sets_a_link_on_the_selection` | No open-in-new-tab button. Apply is disabled whenever the field is empty, without the "…and no link is active" exception. |
| 26 | Image upload node: dashed drop zone, exact strings, per-file progress, drag states, accent border when selected | Partial | `blocks.rs:719-770`: dashed 120 px zone with the `image-up` icon and the literal `Click to upload or drag and drop`, hover fill, accent border while the node is selected, `on_click` → `NotionEditor::pick_image` (`commands.rs:846-865`, `cx.prompt_for_paths`) and `on_drop::<ExternalPaths>` → `set_image_source` (`commands.rs:867-884`, records `Step::Structural`); a placed image gets a 2 px primary outline when selected (`blocks.rs:761-769`); test `an_image_block_takes_a_dropped_file` | No per-file progress rows, no `limit`/`maxSize` subtext or enforcement, no MIME filtering, no separate `imageUpload` node, no caption child, and no distinct `drag-active`/`drag-over` washes. The "Click to upload" half is not italicised. |
| 27 | Table: resize handles, extend buttons, insert labels, cell overlay, clear contents | Partial | `Table` spec `blocks.rs:943-1023` renders a bordered grid with a header row (`render_row` `:1025-1071`, header styling `:1033`/`:1064`), per-row delete controls outside the frame (`row_control` `:1073-1097`), an add-row bar (`:1099-1125`) and an add-column button (`:1127-1153`); cells are child `EditorState`s in a `CellGrid` (`grid.rs:45-95`, bound to the block at `grid.rs:119-215`) persisted through undo/redo in a `cells` block attribute (`grid.rs:90-94`, `:164-172`; test `a_table_survives_undo_and_redo_with_its_text`); row/column edits `grid.rs:267-360` and `:437-483`; insert labels verbatim in the block menu (`gutter.rs:236-250`); `commands.rs:356-387` `insert_table`; copies as a markdown table (`grid.rs:74-88`, `selection.rs:150-155`); tests `a_table_grows_and_shrinks_by_row_and_column`, `a_table_copies_as_a_markdown_table` | No column **resize handles** (`cellMinWidth: 120` is not modelled), no cell selection or `#c8c8ff66` selected-cell overlay, no "Clear all contents", no fit-to-width, no header-row toggle and no cell merge. Cell content is plain text: marks do not apply inside a cell (`grid.rs:15-31` holds text only). |
| 28 | Emoji (`:`) and mention (`@`) share the slash-menu keyboard contract | Done | one `SuggestionMenu` + one `Trigger` for all three, with one generic opener `open_suggestion` (`slash.rs:21-29`, `:92-119`, `suggestion.rs:13-62`); the emoji menu waits for a shortcode (`suggestion.rs:50-52`, `slash.rs:225-234`); tests `the_emoji_menu_replaces_the_shortcode`, `the_mention_menu_inserts_a_marked_name`, `a_colon_in_prose_does_not_hold_a_menu_open` | (They share it exactly — including the gaps listed under #15.) |
| 29 | Placeholder only in the focused empty block, italic `Write, type '/' for commands…` | Partial | exact string `blocks.rs:55-57`; focus rule `view.rs:172-196` + `view.rs:152-170`; the input shows it only when empty | Not italic (`view.rs:723-752` sets no per-placeholder font style). `Heading` sets `placeholder_always` (`blocks.rs:109-111`), so every heading shows `Heading N` whether focused or not — a deliberate Notion-ism, not the template's rule. Other specs define their own strings (`blocks.rs:193, 256, 372, 479, 805, 875`), shown under the focus rule. |
| 30 | A trailing paragraph always exists after a non-paragraph last node; clicking below focuses it | Partial | `commands.rs:462-483` `ensure_paragraph_after`, called after HR (`:423`), image (`:457`), code-block exit (`keymap.rs:257`), ArrowDown off the last block (`keymap.rs:367-373`) and non-textual input rules (`input_rules.rs:189`); click-below `view.rs:996-1012` + `:1022-1040`; tests `clicking_below_the_document_appends_a_paragraph`, `arrow_down_leaves_a_code_block_at_the_end_of_the_document` | It is a set of call sites, not an invariant: deleting the paragraph after a rule/image leaves a non-textual node last until you arrow down or click below. Bottom space is a fixed `280 px` (`style.rs:18`), not `30vh`. |
| 31 | Node selection highlight = translucent brand fill, 8 px radius; images get a 2 px outline | Partial | `view.rs:811-814` `.rounded(px(4.)).bg(theme.selection.opacity(0.4))`, now reached by a real multi-block selection as well: dragging out of the block a press started in switches to whole-block selection (`selection.rs:214-303`, block bounds captured per frame by a `canvas` probe `view.rs:828-848`), Shift+Click selects the range (`selection.rs:232-246`), and typing over it replaces it (`keymap.rs:466-500`); tests `dragging_across_blocks_selects_them`, `shift_clicking_another_block_selects_the_range`, `typing_over_selected_blocks_replaces_them`, `bold_over_selected_blocks_marks_all_of_them`; images get their own 2 px outline in `theme.primary` (`blocks.rs:761-769`), the drop zone an accent border (`blocks.rs:734-738`), a selected table an accent frame (`blocks.rs:1001`) | 4 px radius, not 8; the fill and the outlines use the theme's selection/primary colors, not brand-500 at 20 %. |
| 32 | Selection stays visible when focus moves to a toolbar/popover | Done | The platform does it: the input element paints its selection paths whenever the **window** is active, with no focus condition (`gpui-kit/crates/base/src/input/base/element.rs:2280-2296`), and `on_blur` deliberately keeps the selection (`state.rs:3156-3157`, "NOTE: Do not cancel select, when blur."). The range itself lives in the block's `EditorState`, which is why toolbar and menu commands still act on the right text after focus moves (`toolbar.rs:32-54`, `view.rs:260-264`); the link editor takes focus into its own input (`toolbar.rs:270-273`) and the selection stays painted. | (Comes free from gpui-kit, not from editor code; the fill is the theme's selection color — see #39.) |
| 33 | Content column 708 px, side gutters ≥96 px, padding 48/48/30vh | Partial | `style.rs:10` `PAGE_WIDTH = 708`, `:12` `PAGE_PADDING = 48`; applied `view.rs:991-994`; wrap width 612 px (`view.rs:81`) | `GUTTER_WIDTH = 96` (`style.rs:14`) is declared and **never used** — the column is simply centred, so side margins are whatever the window leaves and can fall below 96 px. Bottom space is a fixed 280 px, not 30vh. No ≤768 px/≤480 px behaviour. |
| 34 | Paragraph 16/1.6/400 + 20 px top; H1 1.5em/700/3em-top, H2 1.25em/700/2.5em, H3 1.125em/600/2em; first heading no top margin | Done | `style.rs:21-25` (16 px, 1.6, 20 px paragraph top margin); `blocks.rs:84-103` — 24 px/700, 20 px/700, 18 px/600 with `margin_em` factors `3.0/2.5/2.0` of each heading's own size (`:89-94`), i.e. 72/50/36 px; H4–H6 fall back to the spec's H4 values (16 px/600/2em); first block margin 0 at `view.rs:786-792` | — |
| 35 | Lists: 1.5em vertical margin, 1.5em left padding, marker cycles, 24 px indent | Partial | marker cycles `blocks.rs:201-216` (disc/circle/square) and `:264-280` (decimal/alpha/roman, `alpha_ordinal`/`roman_ordinal` `:310-346`); `style.rs:28` `INDENT_WIDTH = 24`; `list_layout()` `margin_top: 24px` (= 1.5em) with `leading_width: 24px` (`blocks.rs:163-172`) | Top margin and left gutter match; there is still no explicit 1.5em bottom margin (spacing below a list comes from the next block's own top margin), and consecutive items get a 2 px gap (`view.rs:786-792`) rather than CSS-collapsed zero. |
| 36 | Blockquote: 0.25em bar in gray-900, 1em left padding, 1.5rem vertical margin | Partial | `blocks.rs:483-497` — 4 px bar in `theme.foreground`, `pl(16px)`, `py(6px)`; `margin_top: 24px` (`:469-477`) | `margin_bottom: 0` instead of 1.5rem, and the bar is rounded (`rounded(px(2.))`) where the template's is square. |
| 37 | Code block 1em padding, 1 px border, 6 px radius, mono 16 px; inline code 0.875em, tinted bg + border | Partial | block: `blocks.rs:552-595` (`p(16)`, `border_1`, `rounded(6)`, `mono: true`, hover language menu); inline: `view.rs:905-908` (background + red foreground, `style.rs:55-69`) | Code-block text is 14 px, not 16 px. Inline code has no border and cannot be 0.875em — see §4. |
| 38 | Task checkbox 1em square, 0.25rem radius, checked = gray-a-900 fill + white check, checked text 50 % + strike | Partial | `blocks.rs:380-416` (16 px box, `rounded(4)`, click toggles) and `:363-370` (`text_opacity 0.5` + `strikethrough`), painted through the decoration layer (`view.rs:537-539`) | Fill/border use `theme.primary`, not gray-a-900; the check is the text glyph `✓` at 11 px rather than a masked SVG; no 80 ms transition. |
| 39 | Light/dark palettes match the §5.4 token table incl. selection and brand | Partial | mark palettes `style.rs:96-117` (highlight) and `:119-143` (text color); chrome colors come from the gpui-kit theme (`ui.rs:35-79`) | Values do not match §5.4: e.g. yellow highlight light `#fef3c0` vs spec `#fef9c3`; orange text is **swapped** (code: light `#d9730d`, dark `#c77d48`; spec: light `#c77d48`, dark `#d9730d`). No brand ramp, no `#7a52ff33` selection, no Tiptap gray/alpha ramps — page, border, popover, caret and link colors are whatever the active kit theme says. |
| 40 | Popovers: 1.125 rem radius, 0.375 rem padding, elevated-md 4-layer shadow, 2 rem rows, ghost hover, 0.75 rem/600 labels, 1 px separators | Partial | `ui.rs:35-44` (radius 12 px, padding 4 px, `shadow_lg`), `ui.rs:47-62` (32 px rows, hover fill, 6 px radius) | Radius 12 px vs 18 px, padding 4 px vs 6 px, gpui-kit's single-layer `shadow_lg` vs the 4-layer elevated-md, group labels 11 px at default weight (`slash.rs:358-367`) vs 12 px/600. Group separators are now drawn (`slash.rs:347-356`), 1 px in `theme.border`. |
| 41 | Floating toolbar: 0.188 rem padding, 0.875 rem radius, 2 rem buttons, 0.125 rem gap, 1 px × 1.5 rem separators | Partial | `toolbar.rs:70-75` (`p(4px)`, `gap(2px)`, surface from `ui.rs:35`), separators `toolbar.rs:388-394` (1 px × 20 px) | Padding 4 px vs 3 px, radius 12 px vs 14 px, separators 20 px vs 24 px tall; buttons are gpui-kit `Button::small()`, not a 2 rem square (the local 32 px `toolbar_button` helper, `ui.rs:65-79`, is unused). |
| 42 | Tooltips: dark pill, 0.375 rem radius, 12 px/500, shortcut as a `kbd` badge | Partial | gpui-kit tooltips used throughout (`toolbar.rs:94, 142, 189, 205, 217, 350, 360`; `gutter.rs:100, 111`) | Appearance is entirely the kit's default; no geometry is set and **no shortcut badge is rendered anywhere**, in tooltips or menus. |

**Counts — Done 9 · Partial 33 · Missing 0 · Not possible on this stack 0.**

Done: #4, **#10**, #11, #12, #16, #19, #28, #32, #34. Nothing is wholly missing any
more. Since the previous grading, **#10 Partial → Done** (table cell traversal, Tab
in the last cell appends a row) and **#27 Missing → Partial** (a real table node
exists; resize handles, cell selection overlay and "Clear all contents" do not);
#1/#17/#18/#19/#21/#23/#24/#31 were re-graded in place and stay Partial/Done as
before.

No whole checklist item is blocked by the platform; three *sub*-requirements are
(superscript/subscript rendering in #2, the four alignment buttons in #18, inline
code's 0.875em in #37). They are itemised in §4.

---

## 3. Known gaps, ranked by value

1. **Table depth (#27).** Cell marks (cells are plain text today, `grid.rs:15-31`),
   column resize with `cellMinWidth: 120`, a cell selection with the `#c8c8ff66`
   overlay, and "Clear all contents" / fit-to-width in the Table submenu
   (`gutter.rs:236-250`). Marks inside cells means giving each `Cell` its own
   `MarkList` and decoration collection, the way `Block` has one.
2. **Shortcut badges everywhere (#7, #24, #42).** Add `shortcut: Option<&str>` to
   `SlashItem` and use gpui-kit's label+badge menu row for `gutter.rs:222-284` and
   `toolbar.rs:150-171`; render the same string as a `kbd` pill in tooltips.
3. **Image upload completeness (#26).** On top of the picker and drop handler,
   add a `Vec<Upload>` progress model on the editor with per-file rows, the
   `Maximum {n} file(s), {m}MB each.` subtext, size/type rejection, and an image
   caption child; `commands.rs:846-884` is the place they hook in.
4. **Palette and chrome tokens (#39, #40, #41, #31).** Replace the ad-hoc values in
   `style.rs:96-143` with the §5.4 table (they are currently off, and orange is
   swapped light/dark), and add a brand ramp so selection, caret, link and active
   icons stop inheriting the kit theme. This is now the largest visual divergence.
5. **Mark input rules on paste (#5).** In `apply_markdown_prefix`
   (`input_rules.rs:91-127`), after the node rule fires, scan the finished line with
   the `INLINE_RULES` regexes globally (not anchored at `$`) and apply marks.
6. **Ghost `Filter...` decoration and item availability (#13).** Render
   `Trigger::hint()` — today it has no caller — as an absolutely-positioned muted
   label at `cursor_layout()`; add `check: fn(&NotionEditor,&App)->bool` to
   `SlashItem` so unavailable rows can be hidden.
7. **Home/End and scroll-into-view in menus (#15).** Capture `MoveToStartOfLine`/
   `MoveToEndOfLine` in `keymap.rs` while `suggestion_is_open()`, and give the row
   list a `ScrollHandle` so `menu.selected` can be scrolled into view.
8. **Recent colors and 10 highlights (#24).** Extend `HighlightColor::ALL`
   (`mark.rs:79`) to the template's 10 names and keep a `recent: Vec<ApplyColor>` on
   the editor, rendered as the first group of `color_menu` (`gutter.rs:296-303`);
   reuse `toolbar.rs:397-402`'s `swatch_row` so both menus show swatches.
9. **Hide the toolbar while the comment input is open (#17).** §3.2 lists
   `commentInputVisible` among the conditions; `selection_toolbar_visible`
   (`toolbar.rs:32-54`) does not consult `comment_draft_is_open()`
   (`comments.rs:118-120`) yet. One line, and it removes a real overlap.
10. **Drag affordances (#21, #23).** `cursor_grab()` on the handle, a `dragging: bool`
    on the editor set by `on_drag`/`on_drop` that sets `cursor_grabbing()` on the root
    and hides the gutter, and a `crossAxis` rule using the already-measured
    `block.rows` (`block.rs:419`) for the >40 px case.
11. **Trailing-node invariant (#30).** Make "last block is textual" an invariant
    enforced in `render`/`insert_block` rather than the six `ensure_paragraph_after`
    call sites, and swap `PAGE_BOTTOM` (`style.rs:18`) for a viewport-relative 30vh.
12. **Link popover completeness (#25), two-line grip tooltip (#22), remaining
    bindings (#7, #8).** Open-in-new-tab and the "empty field but active link" enable
    case in `toolbar.rs:324-385`; a two-line tooltip on the grip; `Mod-Enter`,
    `Mod-Alt-4/5/6`, and `Mod-Shift-D`/`Mod-Ctrl-L` once download/anchor-link exist.

---

## 4. Deliberate deviations (platform-forced)

* **Marks are decorations, not styled spans; per-range font size/family is
  impossible.** `HighlightStyle` carries only
  `color, font_weight, font_style, background_color, underline, strikethrough, fade_out`
  (`docs/research/gpui-kit-api.md:414-417`, `:1085-1086`). Consequences:
  superscript/subscript have a mark kind and mutually-exclusive toggles but paint
  nothing (`view.rs:933`); inline `code` cannot be 0.875em or use a mono family
  inside prose, so it is shown as a tinted background plus Notion's red foreground
  (`view.rs:905-908`, `style.rs:55-69`); a completed to-do is struck through by
  synthesising a full-width `Strike` run (`view.rs:537-539`) rather than styling the
  text element.
* **One `EditorState` per block instead of one document buffer.** The same limit
  means "H1 inside the same input is impossible" (`gpui-kit-api.md:1085-1086`), so
  each node type gets its own input sized by its own `BlockLayout`
  (`view.rs:723-752`, created in `view.rs:278-309`). Table cells are the same thing one
  level down: a `CellGrid` of child `EditorState`s beside the block (`grid.rs:45-95`).
  This is why the editor re-implements cross-block caret
  movement (`keymap.rs:322-412`), cross-block selection (`selection.rs:214-303`) and a
  document-level undo stack (`history.rs`) instead of getting them from the input.
* **No text alignment.** `InputState::set_text_align` is documented
  "single-line only, no-op otherwise" (`gpui-kit-api.md:92`), and every block is a
  soft-wrapping multi-line editor (`view.rs:302`), so the template's four align
  buttons and `Mod-Shift-L/E/R/J` are omitted rather than shipped dead
  (`toolbar.rs:155-169`).
* **Mentions and emoji are not inline atoms.** There is no inline-embed/atomic-widget
  API inside an input — `inline_object`/`InlineElement` belong to `TextView`, not to
  the input family (`gpui-kit-api.md:1087-1089`). A mention is therefore the person's
  name as plain text under a `Mention(id)` mark (`slash.rs:299-309`,
  `view.rs:921-925`) and an emoji is the literal character (`slash.rs:288-298`);
  neither deletes as one unit (#9).
* **Block text is a mirror plus a diff, not an edit stream.** The input reports only
  "something changed" (`gpui-kit-api.md` decoration section; `InputEvent::Change`),
  so `on_block_changed` recovers the edit from a common-prefix/suffix diff
  (`view.rs:435-488`, `mark.rs:438-473`) to remap marks — and infers a paste from
  "an insertion longer than one character containing a newline" (`view.rs:447-451`)
  because there is no paste event to hook.
* **Heights are estimated before layout.** The input lays out inside the height it is
  given, so each block's height is pre-computed by wrapping its text with
  `line_wrapper` and reconciled with the last measured bounds
  (`view.rs:652-704`; `remeasure` at `view.rs:652-657` runs only when a block's text,
  type or nesting changes), with the row count cached on the block (`block.rs:417-419`).
  Block bounds, needed for drag-selection hit-testing, are likewise captured once per
  frame by a `canvas` probe rather than measured on demand (`view.rs:828-848`).
* **Document undo is a snapshot stack.** Per-input undo cannot see splits, joins or
  type changes across blocks, so `history.rs:17-134` snapshots
  `Vec<BlockContent>` and the editor captures the inputs' `Undo`/`Redo` actions on
  the way down (`keymap.rs:79-86`); table cells ride along by serialising into a
  `cells` block attribute (`grid.rs:90-94`, `:164-172`). Focus is restored on the next frame via
  `window.defer` (`history.rs:127-131`) because the replayed inputs are not yet in
  the focus tree.
* **Colors come from the gpui-kit theme, not the Tiptap token set.** Only the mark
  palettes are hard-coded (`style.rs:96-143`); page, border, popover, selection and
  link colors are read from `cx.theme()` so light/dark follow the host application
  (`style.rs:42-93`, `ui.rs:35-79`). This is the root cause of #39/#40/#41/#31.

---

## 5. Beyond parity

Things the port has that the Tiptap template does not, so they are graded nowhere
above:

* **Comment threads** (`comments.rs`, 484 lines). Selecting text and pressing the
  toolbar's Comment button (`toolbar.rs:89-96`) or `Mod-Shift-M` (`actions.rs:80`)
  anchors a `MarkKind::Comment(ThreadId)` over the range (`comments.rs:124-152`,
  `mark.rs:30`), painted as an amber wash plus underline (`view.rs:925-932`,
  `style.rs:77-94`); a popover shows the quoted text, the thread's messages, a reply
  input and Resolve/Cancel/Comment; clicking commented text opens its thread
  (`comments.rs:163-196`) and abandoning an empty one removes it
  (`comments.rs:223-262`). Tests `commenting_a_selection_opens_a_thread`,
  `an_abandoned_comment_leaves_no_thread_behind`,
  `resolving_a_thread_takes_the_highlight_off`. The spec mentions comments only as a
  `commentInputVisible` flag the template's toolbar reads (§3.2, §6.8) — there is no
  comment UI in the template to match, so this is an addition, and the one place it
  touches parity is #17 (the toolbar should hide while the comment input is open).
* **Callout and Toggle-list node types** (`blocks.rs:788-915`), Notion nodes the
  Tiptap template has no equivalent of; they appear as extra slash-menu rows (#14).
* **Whole-block selection by drag and Shift+Click** (`selection.rs:214-303`) goes
  beyond the template's ProseMirror text selection, matching Notion instead (#31).
