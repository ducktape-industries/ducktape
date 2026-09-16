# gpui-kit 0.6.1 — code-level API reference for building a block editor

Source of truth: full checkout of `longbridge/gpui-kit` at `/tmp/gpui-kit-probe`
(workspace version `0.6.1`, `edition = 2024`). All `file:line` refs below are
relative to that checkout. Everything here was read from source, not docs.

GPUI itself is the `gpui-pre*` family (`gpui = { package = "gpui-pre", version = "0.3.1" }`,
`/tmp/gpui-kit-probe/Cargo.toml:54`). GPUI signatures below were read from the
locally vendored `gpui-pre-0.3.4` in `~/.cargo/registry` — same minor line,
signature-compatible, but the pin in the workspace is `0.3.1`.

---

## 0. THE HEADLINE BREAKING FACT

**`InputState` is no longer a one-size-fits-all state. `.multi_line()`,
`.auto_grow()`, `.code_editor()`, `.soft_wrap()` DO NOT EXIST on `InputState`.**

In 0.6.1 the editing engine is one generic type parameterised by a compile-time
mode marker, with three public aliases:

```rust
// crates/base/src/input/base/kind.rs:6-9 (module doc), input/input/mod.rs:10,
// textarea/mod.rs:10, editor/mod.rs:11
pub type InputState    = InputBaseState<InputMode>;     // single-line text field
pub type TextareaState = InputBaseState<TextareaMode>;  // ordinary multi-line
pub type EditorState   = InputBaseState<EditorMode>;    // source-code editor
```

> "A method that only makes sense for one mode lives in that mode's `impl`
> block, so it does not exist on the others: `InputState` has no `auto_grow`
> or `soft_wrap`, `TextareaState` has no `masked` or `line_number`, and only
> `EditorState` performs code actions."
> — `crates/base/src/input/base/kind.rs:12-17`

Renderers, in `gpui_kit::component::input`:

| State | Unstyled element (`gpui-base`) | Styled element (`gpui-component`) |
|---|---|---|
| `InputState` | `Input::new(&Entity<InputState>)` `base/src/input/input/mod.rs:22` | `Input::new(&Entity<InputState>)` `component/src/input/input.rs:174` |
| `TextareaState` | `Textarea::new(&Entity<TextareaState>)` `base/src/input/textarea/mod.rs:19` | `Textarea::new(&Entity<TextareaState>)` `component/src/input/textarea.rs:32` |
| `EditorState` | `Editor::new(&Entity<EditorState>)` `base/src/input/editor/mod.rs:202` | `Editor::new(&Entity<EditorState>)` `component/src/input/editor.rs:37` |

`gpui-component`'s `Textarea` and `Editor` both delegate to the same
`Input::from_state(...)` frame internally (`component/src/input/textarea.rs:110`).

---

## 1. `InputState` / `InputBaseState<M>` — full public API

Definition: `crates/base/src/input/base/state.rs:337`.
`impl<M: InputModeKind> EventEmitter<InputEvent> for InputBaseState<M> {}` — `state.rs:526`.
`impl<M> Focusable`, `impl<M> Render`, `impl<M> EntityInputHandler` — `state.rs:4112 / 4118 / 3638`.

### 1.1 Constructors

```rust
// state.rs:8828   — single line
impl InputBaseState<InputMode> { pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self }
// state.rs:9027   — multi line
impl InputBaseState<TextareaMode> { pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self }
// state.rs:9094   — code editor
impl InputBaseState<EditorMode> { pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self }
```

Always built inside an entity:

```rust
let input = cx.new(|cx| InputState::new(window, cx).placeholder("Enter your name"));
// examples/input/src/main.rs:22
```

### 1.2 Builders / setters available on ALL three modes
(`impl<M: InputModeKind> InputBaseState<M>`, `state.rs:528`…)

```rust
pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self          // :763
pub fn set_placeholder(&mut self, p: impl Into<SharedString>, _: &mut Window, cx: &mut Context<Self>) // :847
pub fn default_value(mut self, value: impl Into<SharedString>) -> Self              // :1178
pub fn clean_on_escape(mut self) -> Self                                           // :1083
pub fn set_clean_on_escape(&mut self, clean: bool)                                 // :1088
pub fn submit_on_enter(mut self, submit: bool) -> Self                             // :1097
pub fn set_submit_on_enter(&mut self, submit: bool, cx: &mut Context<Self>)        // :1102
pub fn show_whitespaces(mut self, show: bool) -> Self                              // :1109
pub fn scroll_beyond_last_line(mut self, rows: Option<usize>) -> Self              // :1129
pub fn cursor_surrounding_lines(mut self, lines: Option<usize>) -> Self            // :1158
pub fn mask_pattern(mut self, pattern: impl Into<MaskPattern>) -> Self             // :3315
pub fn context_menu(mut self, enable: bool) -> Self                                // :746
pub fn replaceable(mut self, allow: bool) -> Self                                  // :757  (#[doc(hidden)])
pub fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>)             // :1040
pub fn set_readonly(&mut self, readonly: bool, cx: &mut Context<Self>)             // :1062
pub fn set_text_align(&mut self, text_align: TextAlign, cx: &mut Context<Self>)    // :613 (single-line only, no-op otherwise)
pub fn toggle_masked(&mut self, _: &mut Window, cx: &mut Context<Self>)            // :627
pub fn refresh(&mut self, cx: &mut Context<Self>)                                  // :1272
```

Predicates / presentation:

```rust
pub fn is_multi_line(&self) -> bool        // :578  (const from the mode marker)
pub fn is_single_line(&self) -> bool       // :584
pub fn is_code_editor(&self) -> bool       // :590
pub fn is_editable(&self) -> bool          // :1078   (= !disabled && !readonly)
pub fn is_copyable(&self) -> bool          // :597
pub fn presentation(&self) -> InputPresentation           // :547
pub fn context_menu_capabilities(&self) -> InputContextMenuCapabilities // :601
```

`InputPresentation` (`state.rs:463-524`) is a read-only snapshot with
`focus_handle()`, `is_disabled/readonly/editable/loading/masked/multi_line/code_editor()`,
`text_align()`, `placeholder()`, `mask_placeholder()`.

### 1.3 Single-line-only (`impl InputBaseState<InputMode>`, `state.rs:8826`)

```rust
pub fn masked(mut self, masked: bool) -> Self                                  // :8852
pub fn set_masked(&mut self, masked: bool, _: &mut Window, cx: &mut Context<Self>) // :8858
pub fn pattern(mut self, pattern: regex::Regex) -> Self                        // :8864
pub fn validate(mut self, f: impl Fn(&str, &mut App) -> bool + 'static) -> Self// :8880
pub fn step(mut self, step: impl Into<NumberStep>) -> Self                     // :8902
pub fn min(mut self, min: f64) -> Self / pub fn max(...)                       // :8912 / :8922
pub fn step_by(mut self, f: impl Fn(f64, StepAction, &mut App) -> f64 + 'static) -> Self // :8846
pub fn set_loading(&mut self, loading: bool, _: &mut Window, cx: &mut Context<Self>)     // :8951
```

### 1.4 Multi-line-only (`impl<M: MultiLineMode>`, `state.rs:8959` — Textarea **and** Editor)

```rust
pub fn searchable(mut self, searchable: bool) -> Self                              // :8962
pub fn soft_wrap(mut self, wrap: bool) -> Self                                     // :8974
pub fn set_soft_wrap(&mut self, wrap: bool, _: &mut Window, cx: &mut Context<Self>)// :8980
pub fn wrapping_indent(mut self, wrapping_indent: WrappingIndent) -> Self          // :9003
```
Soft wrap defaults to `true` (`state.rs:687`, test `test_soft_wrap_is_enabled_by_default` at `:6608`).

### 1.5 Textarea-only (`state.rs:9022`)

```rust
pub fn rows(mut self, rows: usize) -> Self                                        // :9042  (#[doc(hidden)], default 2)
pub fn set_rows(&mut self, rows: usize, cx: &mut Context<Self>)                   // :9059
pub fn auto_grow(mut self, min_rows: usize, max_rows: usize) -> Self              // :9076
pub fn set_auto_grow(&mut self, min_rows: usize, max_rows: usize, cx: &mut Context<Self>) // :9031
```

### 1.6 Editor-only (`state.rs:9083`, plus `editor/indent.rs`, `editor/search.rs`, `editor/mod.rs`)

```rust
pub fn language(mut self, language: impl Into<SharedString>) -> Self   // state.rs:9104
pub fn language_name(&self) -> SharedString                            // :9118
pub fn folding(mut self, folding: bool) -> Self                        // :9129
pub fn unfold_at(&mut self, position: impl Into<Position>, cx) -> bool // :9161
pub fn line_number(mut self, line_number: bool) -> Self                // :9186
pub fn auto_close(mut self, auto_close: bool) -> Self                  // :9207
pub fn smart_indent(mut self, smart_indent: bool) -> Self              // :9224
pub fn indent_guides(mut self, indent_guides: bool) -> Self            // editor/indent.rs:170
pub fn tab_size(mut self, tab: TabSize) -> Self                        // editor/indent.rs:495
pub fn lsp(&self) -> &Lsp  /  pub fn lsp_mut(&mut self) -> &mut Lsp    // editor/mod.rs:180 / :185
pub fn open_search(&mut self, replace_mode: bool, cx)                  // editor/search.rs:78
pub fn next_search_match(&mut self, cx) -> Option<Range<usize>>        // editor/search.rs:154
pub fn create_decorations_collection(&mut self, Vec<TextDecoration>, cx) -> TextDecorationCollection // editor/decorations.rs:238
```

### 1.7 Value get/set

```rust
pub fn value(&self) -> SharedString              // :1194  materialises a String each call
pub fn text(&self) -> &Rope                      // :1224  zero-copy, the state owns a `ropey::Rope`
pub fn selected_value(&self) -> SharedString     // :1203
pub fn selected_text(&self) -> RopeSlice<'_>     // :3376
pub fn unmask_value(&self) -> SharedString       // :1208

pub fn set_value(&mut self, value: impl Into<SharedString>, window, cx)   // :894  CLEARS undo history
pub fn replace_all(&mut self, text: impl Into<SharedString>, window, cx)  // :923  KEEPS undo history
pub fn insert(&mut self, text: impl Into<SharedString>, window, cx)       // :951  at cursor
pub fn replace(&mut self, text: impl Into<SharedString>, window, cx)      // :970  replaces selection
pub fn clean(&mut self, window, cx)                                       // :2001
```

`set_value` explicitly does `self.undo_manager.clear()` (`:919`); use `replace_all`
if the user should be able to undo the programmatic change.

### 1.8 Cursor / selection — **UTF-8 BYTE offsets**

```rust
/// Get byte offset of the cursor. The offset is the UTF-8 offset.
pub fn cursor(&self) -> usize                                   // :2739
/// Returns the active selection as a byte range into the text.
pub fn selected_range(&self) -> std::ops::Range<usize>          // :2804
/// Set the selected range using UTF-8 byte offsets, removing additional cursors.
pub fn set_selected_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) // :2819
pub fn select_all(&mut self, _: &mut Window, cx: &mut Context<Self>)             // :2808
pub fn unselect(&mut self, _: &mut Window, cx: &mut Context<Self>)               // :3051

/// Return the (0-based) `Position` (= lsp_types::Position { line, character }) of the cursor.
pub fn cursor_position(&self) -> Position                                        // :1229
pub fn set_cursor_position(&mut self, position: impl Into<Position>, window, cx)  // :1237 (also focuses)
```

So: **byte offsets everywhere on the public API**, except `cursor_position()` /
`set_cursor_position()` which use LSP-style `Position { line: u32, character: u32 }`
(`pub use lsp_types::Position` — `base/src/input/mod.rs:97`). The `EntityInputHandler`
impl is the only place UTF-16 shows up (IME/platform contract).

Internally `Selections` supports multiple cursors, but `selections` is
`pub(super)` — **multi-cursor manipulation from an app is NOT AVAILABLE**;
only the built-in `AddCursorAbove`/`AddCursorBelow` actions.

### 1.9 Focus

```rust
pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>)  // :1252 — focuses + starts blink
impl<M> Focusable for InputBaseState<M> { fn focus_handle(&self, _: &App) -> FocusHandle }  // :4112
```

### 1.10 Events + subscription

```rust
// crates/base/src/input/base/state.rs:121
#[derive(Clone)]
pub enum InputEvent {
    Change,
    PressEnter { secondary: bool, shift: bool },
    Focus,
    Blur,
}
```
That is the **entire** event surface. There is no `SelectionChanged`, no
`CursorMoved`, no `Paste` event — **NOT AVAILABLE**. Track selection changes by
`cx.observe(&state, ...)` (notify-based) or by re-reading `selected_range()` in render.

Subscribe:

```rust
// examples/input/src/main.rs:24-33
let _subscriptions = vec![cx.subscribe_in(&input_state, window, {
    let input_state = input_state.clone();
    move |this, _, ev: &InputEvent, _window, cx| match ev {
        InputEvent::Change => { let value = input_state.read(cx).value(); /* … */ cx.notify() }
        _ => {}
    }
})];
// or without window: cx.subscribe(&editor, |this, _editor, _: &InputEvent, cx| { … })
// examples/editor/src/main.rs:733
```
Keep the `Vec<Subscription>` alive on your view or the subscription drops.

### 1.11 Intercepting keystrokes before the input handles them

The input registers **actions**, not raw key handlers
(`Render for InputBaseState`, `state.rs:4144-4200`):

```rust
div().id("input-state")
     .key_context(CONTEXT)              // CONTEXT = "Input"   (state.rs:129, used at :4146)
     .track_focus(&self.focus_handle)
     .on_action(window.listener_for(&entity, InputBaseState::enter))
     .on_action(… backspace, delete, paste, cut, undo, redo, left, right, up, down …)
```

GPUI dispatches **key bindings → actions before** any `on_key_down` listener
(`gpui-pre/src/window.rs:5741-5755`, then `finish_dispatch_key_event`). Action
dispatch itself runs Capture (root→leaf) then Bubble (leaf→root)
(`window.rs:6083`, `:6117`). Therefore:

* **To pre-empt the input**, use `capture_action::<A>` on an **ancestor** element
  and call `cx.stop_propagation()`:
  ```rust
  use gpui_kit::component::input::{Enter, Backspace};
  div()
    .capture_action(cx.listener(|this, ev: &Enter, window, cx| {
        this.split_block(window, cx);
        cx.stop_propagation();          // the InputState never sees it
    }))
    .capture_action(cx.listener(|this, _: &Backspace, window, cx| {
        if this.caret_at_start(cx) { this.merge_with_previous(window, cx); cx.stop_propagation(); }
    }))
    .child(Input::new(&self.state))
  ```
  `fn capture_action<A: Action>(self, listener: impl Fn(&A, &mut Window, &mut App) + 'static) -> Self`
  — `gpui-pre/src/elements/div.rs:1052`.
* `on_key_down` / `capture_key_down` also exist
  (`div.rs:1092` / `div.rs:1104`, signature
  `impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static`) but only fire for
  keystrokes that did **not** match a binding, so they are the wrong tool for
  Enter/Backspace/arrows and the right tool for e.g. detecting `/`.
  (`/` is not bound, so it reaches `replace_text_in_range` via IME — see §5.)
* Exported action types you can capture: `Enter { secondary, shift }`,
  `Backspace`, `Delete`, `MoveUp/Down/Left/Right`, `MoveHome/End`,
  `SelectAll`, `Copy/Cut/Paste`, `Undo/Redo`, `Escape`, `Indent/Outdent`,
  `Tab`-driven `IndentInline/OutdentInline`, etc.
  (`crates/component/src/input/mod.rs:19-34` re-exports them.)
  `Enter::is_primary(&dyn Action) -> bool` at `state.rs:60`.

---

## 2. Rendering, text style, and per-range styled runs

### 2.1 How the element gets its typography

`TextElement` (`crates/base/src/input/base/element.rs`) takes **everything** from
the inherited GPUI text style:

```rust
let style      = window.text_style();                          // element.rs:1757
let text_size  = style.font_size.to_pixels(window.rem_size()); // element.rs:1759
let line_height = window.line_height();                        // element.rs:1726, :1828
let text_style = window.text_style();                          // element.rs:1778
let fg = dim(text_style.color);                                // element.rs:1789
```

So font family, size, weight, colour and line-height are set by **styling the
ancestor element**, exactly like any GPUI text:

```rust
// examples/editor/src/main.rs:1167-1174
Editor::new(&self.editor)
    .bordered(false)
    .p_0()
    .h(relative(1.))
    .font_family(cx.theme().mono_font_family.clone())
    .text_size(cx.theme().mono_font_size)
```
`gpui-component`'s `Input` additionally hard-codes `const LINE_HEIGHT: Rems = Rems(1.25)`
and applies `.line_height(LINE_HEIGHT)` on the frame (`component/src/input/input.rs:377`, `:578`).
`Input`, `Textarea` and `Editor` all implement `Styled`, so `.text_size()`,
`.font_weight()`, `.text_color()`, `.line_height()`, padding etc. all work.

### 2.2 Per-range styled runs (bold/italic/colour spans inside ONE input)

**Available — but only on `EditorState`.** Two mechanisms, both producing
`Vec<(Range<usize>, HighlightStyle)>` with **UTF-8 byte ranges**.

There is **no** `set_highlights`, **no** `TextStyleRun`, **no** marked-text-based
styling API. `set_highlighter(name)` (`state.rs:769`) only switches the
*language name* for code-editor mode; it does not take styles.

#### (a) `InputHighlighter` — you own the whole document's styling

```rust
// crates/base/src/input/editor/highlighting.rs:30-56
pub trait InputHighlighter {
    fn language(&self) -> SharedString;
    fn update(&mut self, edit: Option<InputEdit>, text: &Rope, folding: bool,
              window: &mut Window, cx: &mut Context<EditorState>);
    /// Return ordered, non-overlapping style runs that fully cover `range`.
    /// Use `HighlightStyle::default()` for text without a semantic style.
    fn styles(&self, range: &Range<usize>, resolver: &dyn HighlightStyleResolver)
        -> Vec<(Range<usize>, HighlightStyle)>;
    fn fold_ranges(&self, text: &Rope) -> Vec<FoldRange>;
    fn fold_ranges_for_edit(&self, range: Range<usize>, text: &Rope) -> Vec<FoldRange> { … }
}

pub type InputHighlighterFactory = Rc<dyn Fn(&str) -> Option<Box<dyn InputHighlighter>>>;
pub trait HighlightStyleResolver: Send + Sync { fn style(&self, name: &str) -> Option<HighlightStyle>; }
pub type SharedHighlightStyleResolver = Arc<dyn HighlightStyleResolver>;
```

> "Base never imports a parser; implementations live in UI crates or apps."
> — `highlighting.rs:76`

Install:
```rust
pub fn set_highlighter_factory(&mut self, factory: InputHighlighterFactory, cx: &mut Context<Self>) // state.rs:799
pub fn ensure_highlighter_factory(&mut self, factory: InputHighlighterFactory)                      // state.rs:810
```
`gpui-component`'s `Input::render` calls `ensure_highlighter_factory(tree_sitter factory)`
(`component/src/input/input.rs:383`) — *ensure*, so **an app-installed factory wins**.
The factory is called with the language name, once, lazily (`base/mode.rs:288-295`);
returning `None` means "no highlighting".
Reference implementation: `crates/component/src/highlighter/input_adapter.rs:22-176`
(tree-sitter, with debounced background parse).

#### (b) `TextDecoration` collections — Monaco-style, edit-tracking

```rust
// crates/base/src/input/editor/decorations.rs:15-25
pub struct TextDecoration { pub range: Range<usize>, pub style: HighlightStyle }
impl TextDecoration { pub fn new(range: Range<usize>, style: HighlightStyle) -> Self }

// decorations.rs:238  (only on InputBaseState<EditorMode>)
pub fn create_decorations_collection(&mut self, decorations: Vec<TextDecoration>,
                                     cx: &mut Context<Self>) -> TextDecorationCollection

// decorations.rs:45/58/71/79
impl TextDecorationCollection {
    pub fn set(&self, decorations: Vec<TextDecoration>, cx: &mut App);
    pub fn append(&self, decorations: Vec<TextDecoration>, cx: &mut App);
    pub fn clear(&self, cx: &mut App);
    pub fn get_ranges(&self, cx: &App) -> Vec<Range<usize>>;
}
```
> "Decoration ranges follow text edits and do not need to be set again after
> each change. Insertions at a range boundary do not expand the range …
> Collections are layered in insertion order; the first collection wins when
> overlapping decorations set the same `HighlightStyle` property."
> — `decorations.rs:225-236`

This is the closest thing to rich-text marks: create one collection per mark kind
(bold, italic, code, link colour), and it tracks the user's edits for you.

#### (c) Composition order in the renderer

`TextElement::highlight_lines` (`element.rs:1460-1602`) composes, in order:
LSP semantic tokens → highlighter styles → decoration layers → diagnostics,
via `gpui::combine_highlights`, then clips to the visible line groups. Runs are
turned into `TextRun`s at `element.rs:1912-1936`:

```rust
let mut run = text_style.clone().highlight(*style).to_run(range.len());
```

#### (d) Hard limits (critical)

* `HighlightStyle` (`gpui-pre/src/style.rs:580-601`) has ONLY:
  `color, font_weight, font_style, background_color, underline, strikethrough, fade_out`.
  **No `font_size`, no `font_family`.** So per-range heading sizes / mixed
  monospace inside one input are **NOT AVAILABLE**.
* `InputState` and `TextareaState` declare `type Extras = ();`
  (`kind.rs`, `impl InputModeKind for InputMode` / `for TextareaMode`), and
  `InputExtras::decoration_layers()` defaults to `Vec::new()` (`kind.rs:78-80`).
  The highlighter only lives in `LayoutMode::CodeEditor`
  (`base/mode.rs:336-346` — `set_highlighter_factory` is a silent no-op for
  other modes). **Per-range styling on `InputState`/`TextareaState`: NOT AVAILABLE.**
* Lines longer than `MAX_HIGHLIGHT_LINE_LENGTH` skip highlighting (`element.rs:1536`).
* Decorations are not rendered while `masked`.

---

## 3. Multi-line behaviour

* **Soft wrap**: `soft_wrap(bool)` / `set_soft_wrap(...)` on `TextareaState` and
  `EditorState` (`state.rs:8974` / `:8980`); default `true` (`state.rs:687`).
  Also `wrapping_indent(WrappingIndent)` (`state.rs:9003`). Wrap width is the
  input's laid-out width (`state.rs:3356`).
* **Line height**: from `window.line_height()`; readable back after layout via
  `pub fn line_height(&self) -> Option<gpui::Pixels>` (`state.rs:2793`).
* **Rows / auto-grow**: `rows(n)` (default 2), `auto_grow(min_rows, max_rows)`
  (`state.rs:9042` / `:9076`); `LayoutMode::{PlainText, AutoGrow, CodeEditor}`
  (`base/mode.rs:29-50`).

### 3.1 Enter handling — and how to stop it inserting a newline

```rust
// crates/base/src/input/base/state.rs:1884-1999 (abridged)
pub(super) fn enter(&mut self, action: &Enter, window: &mut Window, cx: &mut Context<Self>) {
    …
    // In multi-line mode with `submit_on_enter` enabled, a plain `Enter`
    // (without Shift) is treated as submit: propagate the action and emit
    // PressEnter without inserting a newline. `Shift+Enter` still inserts a newline.
    let insert_newline = self.is_multi_line() && (!self.submit_on_enter || action.shift);

    if insert_newline {
        …  // multi-cursor newline, bracket split, auto-indent
    } else {
        // Single line input or submit-on-enter: just emit the event
        self.undo_manager.break_transaction_coalescing();
        cx.propagate();                          // <-- action keeps bubbling
    }
    cx.emit(InputEvent::PressEnter { secondary: action.secondary, shift: action.shift });
}
```

**Yes — the input can be made to NOT handle Enter.** Two supported ways:

1. `TextareaState::new(window, cx).submit_on_enter(true)` → plain Enter inserts
   nothing, emits `InputEvent::PressEnter { shift: false, .. }` and `cx.propagate()`s
   the action to your ancestor's `on_action::<Enter>`. `Shift+Enter` still
   inserts a real newline (a good "soft break inside a block" affordance).
2. A single-line `InputState` never inserts a newline at all; it always emits
   `PressEnter` and propagates. For a Notion-style editor where each block is
   one paragraph, `InputState` per block gives correct Enter semantics for free.
3. To also pre-empt case (1)'s Shift+Enter, or to intercept before any of it,
   use `capture_action::<Enter>` + `cx.stop_propagation()` (see §1.11).

Key bindings (registered in `input::init`, `state.rs:131`+): `enter`,
`shift-enter`, `secondary-enter` all map to `Enter { secondary, shift }` in
`key_context("Input")`.

---

## 4. Cursor geometry (for slash menus / popovers)

```rust
// crates/base/src/input/base/state.rs:530
#[doc(hidden)]
pub fn cursor_layout(&self) -> Option<(Bounds<Pixels>, Pixels)>   // (caret bounds, line height)

pub fn input_bounds(&self) -> Bounds<Pixels>                      // :535  container bounds
pub fn text_bounds(&self) -> Option<Bounds<Pixels>>               // :539  text area bounds
pub fn scroll_offset(&self) -> gpui::Point<gpui::Pixels>          // :2780
pub fn set_scroll_offset(&mut self, offset: Point<Pixels>, cx)    // :2787
pub fn line_height(&self) -> Option<gpui::Pixels>                 // :2793
pub fn visible_row_range(&self) -> Option<Range<usize>>           // :2775  (display rows)

/// Return the rendered bounds for a UTF-8 byte range in the current input contents.
/// Returns `None` when the requested range is not currently laid out or visible.
pub fn range_to_bounds(&self, range: &Range<usize>) -> Option<Bounds<Pixels>>  // :3385
```

`cursor_layout()` is marked `#[doc(hidden)]` but is `pub` and is exactly what the
built-in completion menu uses. Bounds are **window-absolute** (the callers
subtract `input_bounds().origin` to get input-relative):

```rust
// crates/component/src/input/popovers/completion_menu.rs:341-355
fn origin(&self, cx: &App) -> Option<Point<Pixels>> {
    let editor = self.editor.upgrade()?;
    let editor = editor.read(cx);
    let Some((cursor_bounds, line_height)) = editor.cursor_layout() else { return None };
    let cursor_origin = cursor_bounds.origin;
    let scroll_origin = editor.scroll_offset();
    Some(scroll_origin + cursor_origin - editor.input_bounds().origin
         + Point::new(-px(4.), line_height + px(4.)))   // just under the caret
}
```
(Same pattern in `code_action_menu.rs:285-296`; `hover_popover.rs:106` uses
`range_to_bounds`.) There is a regression test
`test_cursor_layout_consumer_updates_after_selection` (`state.rs:4417`) asserting
it updates after selection changes.

**Line/row info**: `cursor_position() -> Position { line, character }` (buffer
coordinates), `visible_row_range()`, and the `DisplayMap`/`DisplayPoint`/
`BufferPoint` types are re-exported (`component/src/input/mod.rs:22`) — but
`state.display_map` itself is `pub(super)`, so display-row conversion helpers on
the state are **not** public. `RopeExt` gives `offset_to_position` /
`position_to_offset` / `line_start_offset` / `line_end_offset`
(`base/src/input/base/rope_ext.rs`, re-exported as `RopeExt`).

---

## 5. IME / marked text

`impl<M: InputModeKind> EntityInputHandler for InputBaseState<M>` —
`crates/base/src/input/base/state.rs:3638`. Methods implemented (all UTF-16
ranges, per GPUI's contract):

```rust
fn text_for_range(&mut self, range_utf16, adjusted_range: &mut Option<Range<usize>>, window, cx) -> Option<String>   // :3639
fn selected_text_range(&mut self, _ignore_disabled_input: bool, window, cx) -> Option<UTF16Selection>                // :3651
fn marked_text_range(&self, window, cx) -> Option<Range<usize>>                                                      // :3664
fn unmark_text(&mut self, window, cx)                                                                                // :3674
fn replace_text_in_range(&mut self, range_utf16: Option<Range<usize>>, new_text: &str, window, cx)                   // :3680
// + replace_and_mark_text_in_range, bounds_for_range, character_index_for_point (further down)
```

Wiring: `window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.state.clone()), cx)`
in the element's prepaint (`element.rs:2184-2187`). The composition range lives
in `pub(super) ime_marked_range: Option<CursorSelection>` (`state.rs:363`) and is
painted with a dedicated underlined `TextRun` (`element.rs:1893-1934`,
`split_run_for_ime_underline`).

**Hooks: NOT AVAILABLE.** There is no `on_ime`, no marked-text callback, no way
to observe composition start/end from an app. The only observable side effect is
`InputEvent::Change` after commit, and `cursor()` returning `ime_marked_range.end`
while composing (`state.rs:2740-2743`). Typing a plain `/` arrives through
`replace_text_in_range` — to detect it you must diff on `InputEvent::Change`
(read `text()` + `cursor()`), not via a key handler.

---

## 6. Undo/redo

### 6.1 On the input

`InputBaseState` owns `pub(super) undo_manager: UndoManager` (`state.rs:341`),
implemented in `crates/base/src/input/base/undo_manager.rs` (567 lines):
transaction coalescing, `EditIntent`, selection restore, LSP-aware grouping.

Public surface: **only the actions** `Undo` and `Redo` (bound to
`secondary-z` / `secondary-shift-z`), dispatched into the focused input.
`can_undo()`, `can_redo()`, `undo()`, `redo()` are **NOT public** on the state
(`InputBaseState::undo`/`redo` are `pub(super)`, registered via `on_action` at
`state.rs:4156-4157`). To drive undo programmatically you must
`window.dispatch_action(Box::new(Undo), cx)` with the input focused.

Related public bits:
```rust
pub fn set_value(...)   // clears undo history (state.rs:919)
pub fn replace_all(...) // preserves undo history (state.rs:923)
```

### 6.2 Reusable generic stacks — YES, two of them

`crates/component/src/history.rs` is a one-line re-export:
```rust
pub use gpui_base::{History, UndoHistory};
```

**`UndoHistory<T>`** — `crates/base/src/undo_history.rs:5`, "A history of grouped undo transactions."
```rust
pub fn new() -> Self                                     // :16
pub fn max_undos(mut self, max_undos: usize) -> Self     // :31  (default 1000)
pub fn group_interval(mut self, d: Duration) -> Self     // :41
pub fn start_grouping(&mut self) / end_grouping(&mut self)  // :47 / :52
pub fn is_ignoring(&self) -> bool / set_ignoring(&mut self, bool) // :57 / :62
pub fn push(&mut self, item: T)                          // :67
pub fn undo(&mut self) -> Option<Vec<T>>                 // :90
pub fn redo(&mut self) -> Option<Vec<T>>                 // :102
pub fn can_undo(&self) -> bool / can_redo(&self) -> bool // :118 / :123
pub fn clear(&mut self)                                  // :128
```
Fully generic in `T`; this is exactly what a block-level "document undo stack"
wants (`T` = your own `BlockEdit` enum).

**`History<T>`** — `crates/base/src/history.rs:8`, a browser-style
back/forward trail: `push`, `current`, `replace_current`, `remove_current`,
`can_back`, `can_forward`, `back`, `forward`, `entries`, `forward_entries`,
`retain`, `clear`, `max_entries` (default 1000). Not an undo stack — navigation.

---

## 7. Popovers / menus — anchoring a slash palette to the caret

### 7.1 Options

| API | Where | Anchoring | Fit for slash menu |
|---|---|---|---|
| `component::popover::Popover` | `component/src/popover.rs:106` | to a **trigger element** | ✗ trigger-based |
| `base::popover::{Popover, PopoverState}` | `base/src/popover.rs:32/169` | to a **trigger element** | ✗ |
| `base::Popup` | `base/src/popup.rs:29` | corner of a trigger's bounds | ✗ (trigger-based) |
| `base::Positioner` | `base/src/positioner.rs:62` | **arbitrary `Point<Pixels>`** | ✓ **best** |
| `component::menu::PopupMenu` | `component/src/menu/popup_menu.rs:284` | rendered inside `anchored()` | ✓ for a plain action menu |
| `component::command::{Command, CommandState}` | `component/src/command/command.rs:57`, `state.rs:117` | none of its own (you place it) | ✓ **best content** for a palette |

**Recommended:** `Command` (the filtered palette UI, with its own query input and
keyboard nav) placed inside `deferred(Positioner::corner(...))` at the caret point.

### 7.2 `Positioner` signatures (`crates/base/src/positioner.rs`)

```rust
pub enum Align { Start, Center /*default*/, End }                      // :17
pub struct ResolvedPosition { pub bounds: Bounds<Pixels>, pub placement: Option<Placement> } // :49

impl Positioner {
    /// Places the popup on a side of `trigger_bounds`, flipping when needed.
    pub fn side(trigger_bounds: Bounds<Pixels>) -> Self                // :76
    /// Places `anchor`'s corner of the popup at `position`.
    pub fn corner(anchor: Anchor, position: Point<Pixels>) -> Self     // :95
    pub fn placement(mut self, placement: Placement) -> Self           // :105
    pub fn align(mut self, align: Align) -> Self                       // :117
    pub fn offset(mut self, offset: Pixels) -> Self                    // :126
    pub fn occlude(mut self) -> Self                                   // :139
    pub fn margin(mut self, margin: Pixels) -> Self                    // :145
}
impl ParentElement for Positioner { … }   // :272
```
It owns measurement, viewport clamping and (for `side`) flipping.

### 7.3 The canonical deferred/anchored recipe

```rust
// crates/base/src/popup.rs:134-145
root.child(
    deferred(
        Positioner::corner(anchor, position.get())
            .margin(WINDOW_MARGIN)      // px(8.)
            .occlude()                  // popup eats the pointer
            .child(content),
    )
    .with_priority(POPUP_PRIORITY),     // pub const POPUP_PRIORITY: usize = 100;  popup.rs:15
)
```

Applied to a caret:

```rust
use gpui_kit::{Anchor, deferred, px, Point};
use gpui_kit::base::Positioner;
use gpui_kit::component::command::{Command, CommandState, CommandItem};

let Some((caret, line_h)) = self.block_state.read(cx).cursor_layout() else { return None };
let at = caret.origin + Point::new(px(0.), line_h + px(4.));   // window coords, just below caret
deferred(
    Positioner::corner(Anchor::TopLeft, at)
        .margin(px(8.)).occlude()
        .child(
            Command::new(&self.palette_state)
                .placeholder("Filter blocks…")
                .items(items)
                .on_confirm(cx.listener(|this, ix: &IndexPath, window, cx| { … })),
        ),
).with_priority(100)
```

The simpler `div().absolute().left(x).top(y)` inside `deferred(...)` is what the
built-in completion menu does (`completion_menu.rs:390-396`) — valid too, but it
does not clamp to the viewport.

### 7.4 `Command` palette API

```rust
// crates/component/src/command/command.rs
impl Command {
    pub fn new(state: &Entity<CommandState>) -> Self                 // :72
    pub fn item(mut self, item: CommandItem) -> Self                 // :87
    pub fn items(mut self, items: impl IntoIterator<Item = CommandItem>) -> Self // :93
    pub fn group(mut self, group: CommandGroup) -> Self              // :100
    pub fn separator(mut self) -> Self                               // :106
    pub fn searchable(mut self, searchable: bool) -> Self            // :112
    pub fn filterable(mut self, filterable: bool) -> Self            // :123
    pub fn on_query<F>(mut self, callback: F) -> Self                // :130
    pub fn on_select<F>(mut self, callback: F) -> Self               // :145
    pub fn on_confirm<F>(mut self, callback: F) -> Self              // :157
    pub fn on_cancel<F>(mut self, callback: F) -> Self               // :168
    pub fn placeholder(mut self, p: impl Into<SharedString>) -> Self // :177
    pub fn empty<F, E>(mut self, f: F) -> Self                       // :183
    pub fn max_h(mut self, height: impl Into<DefiniteLength>) -> Self// :195  (default rems(18.75))
    pub fn bordered(mut self, bordered: bool) -> Self                // :204
    pub fn header<F,E>(…) / pub fn footer<F,E>(…)                    // :210 / :222
}
// crates/component/src/command/state.rs
impl CommandState {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self  // :142
    pub fn query(&self, cx: &App) -> SharedString                    // :201
    pub fn set_query(…)                                              // :209
    pub fn selected_index(&self) -> Option<IndexPath>                // :231
    pub fn set_selected_index(…)                                     // :243
    pub fn matched_count(&self) -> usize                             // :276
    pub fn focus(&self, window: &mut Window, cx: &mut App)           // :281
    pub fn set_loading(…) / pub fn is_loading(&self) -> bool         // :293 / :301
}
// crates/component/src/command/item.rs
CommandItem::new().label(..).icon(..).action(Box<dyn Action>).checked(bool).keywords([..]).child(..)
CommandGroup::new().label(..).item(..).items(..)
```
Caveat: `CommandState` owns its **own** search `InputState` and takes focus, so
opening it steals focus from the block being edited — you must restore focus on
dismiss. Alternatively keep focus in the block and render a plain `List`
(`component::list::List`) yourself, filtering by the text the user typed after `/`.

### 7.5 `PopupMenu` (simple action menu)

```rust
// crates/component/src/menu/popup_menu.rs
pub fn init(cx: &mut App)                                                        // :21
PopupMenu::build(window, cx, |menu, window, cx| menu …) -> Entity<PopupMenu>      // :349
  .action_context(FocusHandle)   // :362   — where dispatched actions land
  .menu(label, Box<dyn Action>)  // :440
  .menu_with_icon(..) / .menu_with_check(..) / .menu_element(..) / .submenu(..) / .separator()
  .min_w / .max_w / .max_h / .scrollable / .check_side
```

### 7.6 `Root` requirements

`Root` is mandatory as the **first view in the window** — it hosts the dialog,
sheet, notification and context-menu layers:

```rust
// crates/component/src/root.rs:98
pub fn new(view: impl Into<AnyView>, window: &mut Window, cx: &mut Context<Self>) -> Self
pub fn bordered(mut self, bordered: bool) -> Self                 // :140
pub fn window_shadow_size(mut self, size: impl Into<Pixels>) -> Self // :148
pub fn update<F, R>(window: &mut Window, cx: &mut App, f: F) -> R  // :153
pub fn read<'a>(window: &'a Window, cx: &'a App) -> &'a Self       // :173
pub fn open_dialog<F>(&mut self, build: F, window, cx)             // :294
pub fn push_notification(…)                                        // :422
```
Plus `WindowExt` conveniences (`crates/component/src/window_ext.rs:12`):
`window.open_dialog(cx, build)` `:30`, `window.push_notification(note, cx)` `:65`,
`window.focused_input(cx) -> Option<AnyInputState>` `:86`.

`deferred(...)` popups do **not** need `Root` (they are a GPUI primitive), but
dialogs/sheets/notifications/context menus do.

---

## 8. Focus with many child inputs

* Each `InputBaseState` creates `cx.focus_handle().tab_stop(true)` in
  `new_in_mode` (`state.rs:643`).
* `impl Focusable` → `state.focus_handle(cx) -> FocusHandle`.
* Move focus programmatically, two equivalent forms:
  ```rust
  self.blocks[i].state.update(cx, |s, cx| s.focus(window, cx));  // state.rs:1252, also starts blink
  // or
  let h = self.blocks[i].state.focus_handle(cx);
  h.focus(window, cx);
  // deferred (e.g. right after creating a block), as the editor example does:
  window.defer(cx, move |window, cx| { h.focus(window, cx); });  // examples/editor/src/main.rs:721-723
  ```
* Which input is focused globally:
  `window.focused_input(cx) -> Option<AnyInputState>` (`window_ext.rs:86`, `:223`).
  `AnyInputState` (`component/src/input/state.rs`, re-exported at
  `component/src/input/mod.rs:51`) is the `Input|Textarea|Editor|Otp` enum with
  `as_input()`/`as_textarea()`/`as_editor()` accessors.
* Tab order: `Input::tab_index(isize)` (`component/src/input/input.rs:309`),
  `Textarea::tab_index` (`textarea.rs:77`).
* Containment queries (GPUI): `handle.is_focused(window)`,
  `handle.contains_focused(window, cx)`.

### `focus_trap`

```rust
// crates/base/src/focus_trap.rs:14-49
pub trait FocusTrapElement: InteractiveElement + Sized {
    fn focus_trap(self, id: impl Into<ElementId>, focus_handle: &FocusHandle)
        -> FocusTrapContainer<Self>
    where Self: ParentElement + Styled + Element + 'static;
}
impl<T: InteractiveElement + Sized> FocusTrapElement for T {}
pub fn active_focus_trap(window: &Window, cx: &App) -> Option<FocusHandle>   // :226
pub fn init(cx: &mut App)   // :9 — called by gpui_kit::init
```
Tab/Shift-Tab cycles within the container; `Root` intercepts and re-targets.
Example: `examples/focus_trap/src/main.rs`. **Do not** wrap a block list in a
focus trap unless you want Tab to cycle blocks rather than indent.

---

## 9. Drag & drop

All of it is plain GPUI (`gpui-pre/src/elements/div.rs`), re-exported by
`gpui_kit::*`:

```rust
fn on_drag<T, W>(self, value: T,
                 constructor: impl Fn(&T, Point<Pixels>, &mut Window, &mut App) -> Entity<W> + 'static) -> Self
    where T: 'static, W: 'static + Render;                                   // div.rs:1576
fn on_drag_move<T: 'static>(self,
                 listener: impl Fn(&DragMoveEvent<T>, &mut Window, &mut App) + 'static) -> Self; // div.rs:1008
fn on_drop<T: 'static>(self,
                 listener: impl Fn(&T, &mut Window, &mut App) + 'static) -> Self;                // div.rs:1187
fn can_drop(self,
                 predicate: impl Fn(&dyn Any, &mut Window, &mut App) -> bool + 'static) -> Self; // div.rs:1197
fn external_drag_payload<T>(self,
                 resolver: impl Fn(&T, &mut Window, &mut App) -> Option<ExternalDragPayload> + 'static) -> Self; // div.rs:1595
```
`DragMoveEvent<T>` gives you the dragged value and the pointer position;
`on_drag`'s constructor returns the drag **preview view**.

**Kit-level list-reorder helper: NOT AVAILABLE.** There is no generic sortable
list. What exists, as reference implementations to copy:
* Table column reorder: `component/src/table/state.rs:1619` (`on_drag(DragColumn…)`),
  `:1771` (`on_drag_move`), `:1787` (`on_drop`).
* Dock tab drag between panels: `component/src/dock/tab_panel.rs:401,525,544,562,584,598`.
* Resize handles / slider thumbs: `base/src/resizable/resize_handle.rs:81`,
  `base/src/slider.rs:614`.
* `component::list::ListItem` already sets a no-op `on_drag(DragPreview…)` /
  `on_drop` pair at `component/src/list/list_item.rs:284-286`.

Theme tokens exist for the visuals: `cx.theme().drag_border`, `cx.theme().drop_target`
(`component/src/theme/theme_color.rs:159, 161`).

---

## 10. Theme

```rust
// crates/component/src/theme/mod.rs:40-50
pub trait ActiveTheme { fn theme(&self) -> &Theme; }
impl ActiveTheme for App { fn theme(&self) -> &Theme { Theme::global(self) } }
```
`Theme` derefs to `ThemeColor` (`mod.rs:151-156`), so `cx.theme().foreground` works.

Non-colour fields (`mod.rs:88-145`): `colors: ThemeColor`, `tokens: ThemeTokens`,
`highlight_theme: Arc<HighlightTheme>`, `mode: ThemeMode`,
`font_family: SharedString`, `font_size: Pixels` (16px),
`mono_font_family: SharedString`, `mono_font_size: Pixels` (13px),
`radius: Pixels`, `radius_lg: Pixels`, `shadow: bool`, `focus_ring: bool`,
`scrollbar_mode`, `list`, `sheet`, `motion`.

Colour tokens relevant to an editor (`component/src/theme/theme_color.rs`):

| Token | Line | Use |
|---|---|---|
| `background` / `foreground` | :67 / :163 | page + body text |
| `muted` / `muted_foreground` | :193 / :195 | placeholder, secondary text |
| `border` | :69 | rules, block outlines |
| `input` | :173 | input border colour |
| `ring` | :211 | focus ring |
| `accent` / `accent_foreground` | :61 / :63 | hover/selected rows |
| `selection` | :227 | text selection fill |
| `caret` | :131 | caret |
| `popover` / `popover_foreground` | :197 / :199 | slash menu surface |
| `list`, `list_hover`, `list_active`, `list_active_border` | :181-:187 | menu items |
| `drag_border`, `drop_target` | :159 / :161 | drag & drop affordance |
| `primary`, `secondary`, `danger`, `warning`, `success`, `info` (+ `_hover/_active/_foreground`) | various | callouts |
| `overlay` | :308 region | modal scrim |
| `title_bar`, `status_bar`, `sidebar*`, `tab*`, `table*`, `scrollbar*` | various | chrome |

Helper methods: `Theme::input_background()` (`mod.rs:334`),
`Theme::editor_background()` (`mod.rs:344`, `pub(crate)` — not reachable from an app;
use `input_background()`), `Theme::global(cx)` (`mod.rs:174`),
`Theme::change(mode, window: Option<&mut Window>, cx)` (`mod.rs:237`).

Semantic/base tokens live in `crates/base/src/theme_tokens.rs:21-44`
(`background, foreground, surface, surface_foreground, primary, secondary, muted,
accent, destructive, border, input, ring, selection` + `_foreground` variants).
`InputEditorStyle::resolved(&SemanticThemeTokens)` fills unset colours from them
(`base/src/input/editor/highlighting.rs:116-139`).

`gpui_kit::init(cx)` → `gpui_component::init(cx)` → `theme::init(cx)` which calls
`Theme::change(ThemeMode::Light, None, cx)` (`component/src/theme/mod.rs:32-38`).

---

## 11. Scaffolding for a binary

### 11.1 Cargo.toml

`gpui-kit` **is published on crates.io at 0.6.1** (verified against
`https://crates.io/api/v1/crates/gpui-kit`: `newest_version = "0.6.1"`,
`max_stable_version = "0.6.1"`, published 2026-09-09, 3 versions total —
`0.1.0` (placeholder), `0.6.0`, `0.6.1`; not yanked). `gpui-component` 0.6.1 and
`gpui-base` 0.6.1 are published too. **There is no 0.2–0.5 line for `gpui-kit`**,
so `gpui-kit = "0.5"` will not resolve. No git dependency is required.

```toml
[package]
name = "gpui-notion"
version = "0.1.0"
edition = "2024"          # gpui-kit is edition 2024; your crate does not have to be,
                          # but the toolchain must support it (Rust >= 1.85)

[dependencies]
# default features = ["component", "assets"]  -> gpui_kit::component + gpui_kit::assets
gpui-kit = "0.6.1"
anyhow = "1"

# If you want tree-sitter syntax highlighting in code blocks, add the languages
# you need instead of the whole set:
# gpui-kit = { version = "0.6.1", features = ["tree-sitter-rust", "tree-sitter-markdown"] }
# or everything (what examples/editor uses):
# gpui-kit = { version = "0.6.1", features = ["tree-sitter-languages"] }
```

Repo's own examples (path deps, but same feature names):
* `examples/input/Cargo.toml`: `gpui-kit.workspace = true`, `anyhow.workspace = true`
* `examples/hello_world/Cargo.toml`: `gpui-kit.workspace = true`, `anyhow.workspace = true`
* `examples/editor/Cargo.toml`: `gpui-kit = { workspace = true, features = ["tree-sitter-languages"] }`

Useful `gpui-kit` features (`crates/kit/Cargo.toml:18-31`):
`component` (default), `assets` (default), `test-support`, `profiler`,
`inspector`, `decimal`, `tree-sitter`, `tree-sitter-languages`,
`tree-sitter-<lang>` for ~35 languages.

`gpui-kit` pulls in `gpui_platform` with `["font-kit", "x11", "wayland", "runtime_shaders"]`
on desktop, so on Linux you need the usual X11/Wayland/fontconfig dev packages.

### 11.2 `main.rs` boilerplate

Verbatim from `examples/input/src/main.rs:55-78` (the canonical shape):

```rust
use gpui_kit::assets::Assets;
use gpui_kit::component::{
    input::{Input, InputEvent, InputState},
    *,                                  // brings in Root, v_flex, h_flex, ActiveTheme, …
};
use gpui_kit::*;                        // = all of gpui: div(), px(), Window, Context, …

pub struct Example { /* … */ }

impl Render for Example {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex().p_5().gap_2().size_full().child("…")
    }
}

fn main() {
    let app = gpui_kit::application().with_assets(Assets);

    app.run(move |cx| {
        // This must be called before using any GPUI Component features.
        gpui_kit::init(cx);

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(800.), px(600.)), cx)),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            cx.open_window(window_options, |window, cx| {
                let view = cx.new(|cx| Example::new(window, cx));
                // This first level on the window, should be a Root.
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("Failed to open window");
        })
        .detach();
    });
}
```

Variant that also tints the root (`examples/hello_world/src/main.rs:24-40`):
`Root::new(view, window, cx).bg(cx.theme().background)`.

**Assets**: `gpui_kit::assets::Assets` is the default icon set; without
`.with_assets(Assets)` icon components render nothing. `gpui_kit::application()`
is re-exported from `gpui_platform` (`crates/kit/src/lib.rs:151`).

**Key bindings**: all input bindings are registered for you by
`gpui_kit::init` → `gpui_component::init` → `input::init` → `base::input::state::init`
(`crates/base/src/input/base/state.rs:131`+), in `key_context("Input")`.
Your own actions:

```rust
gpui_kit::actions!(notion, [Quit, ToggleBold, SlashMenu]);   // crates/kit/src/lib.rs:47 macro

// inside app.run:
cx.bind_keys([
    KeyBinding::new("cmd-q", Quit, None),
    KeyBinding::new("cmd-b", ToggleBold, Some("BlockEditor")),
]);
cx.on_action(|_: &Quit, cx| cx.quit());
```
Note: `gpui_kit::actions!` exists because GPUI's own `actions!` spells the derive
as `gpui::Action`, which does not resolve through the facade
(`crates/kit/src/lib.rs:44-47`).

---

## 12. Existing rich-text / markdown rendering

### 12.1 `TextView` — read-only markdown/HTML renderer. YES, it renders to styled GPUI elements.

`crates/component/src/text/mod.rs` is a thin re-export of `gpui_base::text`.
Core: `crates/base/src/text/text_view.rs` (2678 lines), `node.rs` (3658 lines),
`inline*.rs` (inline layout engine), `markdown_ext.rs` (plugin/extension API).

```rust
// crates/base/src/text/mod.rs:31-40
#[track_caller] pub fn markdown(source: impl Into<SharedString>) -> TextView
#[track_caller] pub fn html(source: impl Into<SharedString>) -> TextView

// crates/base/src/text/text_view.rs
impl TextView {
    pub fn new(state: &Entity<TextViewState>) -> Self                       // :159
    pub fn markdown(id: impl Into<ElementId>, markdown: impl Into<SharedString>) -> Self // :180
    pub fn html(id: impl Into<ElementId>, html: impl Into<SharedString>) -> Self         // :201
    pub fn style(mut self, style: TextViewStyle) -> Self                    // :222
    pub fn selectable(mut self, selectable: bool) -> Self                   // :228
    pub fn selection_format(mut self, f: SelectionFormat) -> Self           // :237
    pub fn scrollable(mut self, scrollable: bool) -> Self                   // :254
    pub fn max_lines(mut self, max_lines: usize) -> Self                    // :277
    pub fn code_block_actions<F, E>(mut self, f: F) -> Self                 // :286
    pub fn code_block_highlighter<F>(mut self, highlighter: F) -> Self      // :301
    pub fn table_actions<F, E>(mut self, f: F) -> Self                      // :313
    pub fn on_link_click<F>(mut self, handler: F) -> Self                   // :328
    pub fn markdown_extensions(mut self, extensions: MarkdownExtensions) -> Self // :337
    pub fn markdown_mdx(mut self) -> Self                                   // :346
    pub fn markdown_block_parser<F>(mut self, parser: F) -> Self            // :357
    pub fn markdown_block_renderer<F, E>(mut self, name, renderer) -> Self  // :372
    pub fn plugin<P: TextViewPlugin>(self, plugin: P) -> Self               // :386
}

// crates/base/src/text/state.rs
impl TextViewState {
    pub fn markdown(text: &str, cx: &mut Context<Self>) -> Self   // :135
    pub fn html(text: &str, cx: &mut Context<Self>) -> Self       // :140
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>)// :288
    pub fn push_str(&mut self, new_text: &str, cx: &mut Context<Self>) // :300  (streaming)
    pub fn selected_text(&self) -> String                         // :328
    pub fn select_all / clear_selection / focus_handle / bounds / list_state / is_clamped
}

// crates/base/src/text/markdown_ext.rs — extension points
pub type MarkdownBlockParserFn = …      // :28
pub type MarkdownBlockRenderFn = …      // :32
pub trait MarkdownPlugin: Send + Sync + 'static { … }            // :45
pub struct MarkdownNode { … new/name/as_text/as_markdown/source_range/text/markdown/data } // :108
pub struct MarkdownExtensions { … parser_revision/frontmatter/mdx/block_parser/block_renderer/plugin } // :246
```

Global defaults: `TextViewDefaults::new().with_style(..).with_code_block_highlighter(..).install(cx)`
(`text_view.rs:28-67`); `gpui-component` installs its theme-derived style +
tree-sitter code-block highlighter (`component/src/text/mod.rs:64-75`).

Also: `Text` enum (`base/src/text/mod.rs:43`) = `String | TextView`, so component
props that take `impl Into<Text>` accept either.

Examples: `examples/markdown`, `examples/stream-markdown`, `examples/html`,
`examples/markdown_table`, `examples/text_selection`, `examples/text_max_lines`.

### 12.2 Editable rich text

**NOT AVAILABLE.** There is no WYSIWYG / contenteditable-style component anywhere
in the workspace. Concretely:

* `TextView` is render-only: `TextViewState` has `set_text`/`push_str` and
  selection/copy, but no cursor, no `EntityInputHandler`, no editing actions.
* The input family edits **plain text in a `Rope`**; styling is a *presentation
  overlay* (`InputHighlighter` / `TextDecoration`) that never changes the
  underlying string and is only wired for `EditorState`.
* `HighlightStyle` cannot vary font size or family, so "H1 inside the same input"
  is impossible even on `EditorState`.
* There is no inline-embed/atomic-widget API inside an input (the `inline_object` /
  `InlineElement` machinery at `base/src/text/inline_object.rs`,
  `inline_element.rs` belongs to `TextView`, not to the input).
* `crates/component/src/text/window_selection.rs` (2417 lines) implements
  cross-element text selection for **read-only** text views.

---

## Implications for a block editor

### Fixed points you must design around

1. Per-block editing state is `InputState` (single-line) or `TextareaState`
   (multi-line) — neither can show **any** per-range styling.
2. Per-range styling (`HighlightStyle`: colour, weight, italic, background,
   underline, strikethrough) exists **only** on `EditorState`, and even there
   cannot change font size or family.
3. Caret geometry (`cursor_layout()`) and byte-range geometry (`range_to_bounds()`)
   are public on all three — slash menus and inline toolbars are feasible everywhere.
4. Enter can be fully taken over (`submit_on_enter(true)` on a textarea, or a
   single-line input, or `capture_action::<Enter>` + `stop_propagation`).
5. No document-level undo is given to you; `UndoHistory<T>` is a ready generic
   stack, and each input's own undo is action-only and per-block.
6. There is no reorder helper; drag & drop is raw GPUI.

### Architecture A — one `InputState` per block (plain text blocks)

*Each block is an entity holding an `InputState` (paragraph/heading/list item) or
`TextareaState` (code-ish, quote). The document is a `Vec<Block>` in a parent view.*

* **Pros**
  * Enter semantics are free: single-line `InputState` never inserts `\n`, always
    emits `PressEnter` and `cx.propagate()`s — split the block in your `on_action`.
  * Per-block font size/weight/colour is just styling the wrapper div, so
    H1/H2/quote/code blocks all look right (this is the *only* way to get
    per-block type scale, since `HighlightStyle` has no font size).
  * IME, selection, clipboard, undo-within-block, context menu, a11y: all free.
  * Backspace-at-start / Delete-at-end merges: `capture_action::<Backspace>` plus
    `state.cursor() == 0`.
  * Focus movement between blocks is a one-liner (`state.focus(window, cx)`),
    with byte-offset caret restoration via `set_selected_range(n..n, cx)`.
* **Cons**
  * **No inline marks at all** (no bold word inside a paragraph). This is the
    dealbreaker if you want Notion-grade inline formatting.
  * No cross-block selection: each `InputState` owns its own selection; a
    user-drag from block 3 to block 5 is **NOT AVAILABLE** and you would have to
    build it (the read-only `window_selection.rs` shows how much work that is).
  * Copying a multi-block range, drag-select, and "select all then delete" all
    need hand-rolled block-level selection.
  * N entities × N focus handles × N ropes; fine for hundreds of blocks, but each
    carries an `UndoManager` and a `DisplayMap`.

### Architecture B — one `EditorState` per block (rich inline text)

*Same block decomposition, but every block is an `EditorState` in
`soft_wrap(true).line_number(false).folding(false)` with a custom
`InputHighlighter` (or decoration collections) driving inline marks.*

* **Pros**
  * **Inline bold / italic / code / link colour / highlight inside a block works.**
    Two supported routes:
    * `create_decorations_collection(vec![TextDecoration::new(3..7, bold)], cx)` —
      ranges auto-track edits (Monaco `NeverGrowsWhenTypingAtEdges` semantics),
      so your mark model survives typing without re-derivation.
    * A custom `InputHighlighter` whose `styles()` reads your own mark table —
      good if marks are derived (e.g. you store markdown source and parse it).
  * Still get per-block type scale from the wrapper div, since each block is a
    separate element.
  * Everything from A (IME, clipboard, context menu, per-block undo) still applies.
* **Cons**
  * `EditorState` carries `EditorExtras` (LSP client, diagnostics, inline
    completion, hover, context-menu task) per block — heavier than `InputState`,
    and you pay a `DisplayMap` + fold machinery per block.
  * Enter inserts a newline by default in multi-line mode; you need
    `submit_on_enter(true)` (Shift+Enter then gives you a soft break, which is
    actually the right Notion behaviour) or `capture_action::<Enter>`.
  * `EditorState` also enables code-editor behaviours you must switch off or
    live with: auto-close brackets (`auto_close(false)`), smart indent
    (`smart_indent(false)`), indent guides, Tab = indent, search overlay
    (`searchable(false)`), line numbers (`line_number(false)`).
  * `gpui-component`'s `Input::render` installs the tree-sitter factory via
    `ensure_highlighter_factory`; you must call `set_highlighter_factory` yourself
    (it wins) or your `language()` name must not match a known parser.
  * Still **no per-range font size**, so "big bold" inside a paragraph is out.
  * Cross-block selection: same gap as A.

### Architecture C — one custom `Element` with your own text layout

*Own the rope, the layout, the caret, and paint with `window.text_system().shape_line/shape_text`
and `TextRun`s built from your own mark model. Implement `EntityInputHandler` yourself.*

* **Pros**
  * The only way to get **true rich text**: per-range font size/family (headings
    inline), inline embeds (mentions, equations, inline images), cross-block
    selection, a single document selection model, one document undo stack.
  * One `Rope` + one `UndoHistory<YourEdit>` for the whole document — clean
    collaborative/CRDT story later.
  * `TextRun { len, font, color, background_color, underline, strikethrough }`
    (`gpui-pre/src/text_system.rs:1003`) gives you font-per-run, which
    `HighlightStyle` does not.
* **Cons**
  * You re-implement everything `crates/base/src/input/base/element.rs` (3333
    lines) and `state.rs` (9234 lines) already do: IME marked text, UTF-16↔UTF-8
    conversion, soft wrap, line layout caching, hit-testing, scroll-to-cursor,
    blink cursor, autoscroll on drag, clipboard, a11y, context menu, word
    boundaries, bidi.
  * `DisplayMap`, `LineLayout`, `LastLayout`, `text_wrapper` are all `pub(super)`
    in gpui-kit — **you cannot reuse gpui-kit's layout engine**; you'd build on
    raw `gpui::TextSystem` / `ShapedLine` / `WrappedLine`.
  * `EntityInputHandler` is implementable by an app (it's a public GPUI trait,
    wired with `window.handle_input(&handle, ElementInputHandler::new(bounds, entity), cx)`
    — see `element.rs:2184`), so this is possible, just expensive.

### Recommended path

Start with **B for text blocks, A-style wrappers for everything else**:
`EditorState` per block with `soft_wrap(true)`, `submit_on_enter(true)`,
`line_number(false)`, `folding(false)`, `auto_close(false)`, `smart_indent(false)`,
`searchable(false)`, plus one `TextDecorationCollection` per mark kind. That buys
inline bold/italic/code/links today with edit-tracking, per-block type scale, and
full IME/clipboard/undo, at the cost of accepting (i) no per-range font size and
(ii) no cross-block selection.

Keep the block model (`Vec<Block>` with ids + a `UndoHistory<BlockEdit>` for
structural operations: insert, delete, move, change type) strictly outside the
input states, so that a later migration to **C** — where the same block model is
rendered by one custom element — does not touch your document layer. Treat
`cursor_layout()` + `Positioner::corner` as the stable seam for the slash menu
and any inline toolbar, since that API is identical under A, B and C.
