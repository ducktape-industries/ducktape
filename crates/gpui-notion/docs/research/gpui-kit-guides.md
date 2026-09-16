# GPUI Kit guides — actionable checklist (for gpui-notion)

Condensed from the normative guides. Citation keys (all under `/tmp/gpui-kit-probe/skills/`):
`CG` = `gpui-kit/references/coding-guides.md` · `DG` = `gpui-kit-design-guides/references/design-guides.md` ·
`SK` = `gpui-kit/SKILL.md` · `CV` = `gpui-kit/references/conventions.md` · `US` = `gpui-kit/references/usage.md` ·
`RC` = `gpui-kit/references/recipes.md` · `T` = `gpui-kit/references/gpui/test.md` ·
`EN` = `gpui-kit/references/gpui/entity.md` · `EV` = `gpui-kit/references/gpui/event.md` ·
`FH` = `gpui-kit/references/gpui/focus-handle.md` · `EID` = `gpui-kit/references/gpui/element-id.md` ·
`EBP` = `gpui-kit/references/gpui/element-best-practices.md`

## 1. Hard rules

### Architecture & units
- Never invent an API; grep the real signature in `gpui-kit` source before writing a call — invented look-alike methods are the top failure mode. `SK:82`
- Depend only on the `gpui-kit` crate; GPUI is `use gpui_kit::*;`, layers are `gpui_kit::{component,base,assets,platform}`. `SK:85`
- Call `gpui_kit::init(cx)` first and wrap every window's first-level view in `Root::new(view, window, cx)` — Root owns overlays, focus restoration, focus traps, `set_rem_size`. `CG:108`, `US:34`
- Render the overlay layers yourself: `.children(Root::render_dialog_layer(...))`, `render_sheet_layer`, `render_notification_layer`; constructing `Root` does not mount them. `RC:8`, `T:55`
- Use `RenderOnce`/`IntoElement` when all inputs come from the caller and nothing is retained between frames. `CG:146`
- Use `Entity<T>` + `Render` only when behavior spans frames (subscriptions, focus, async, history, measurement); do not entity-ify every visual fragment. `CG:171`, `CG:191`
- Never retain `&mut Window`, `&mut App`, `&mut Context<_>` past the call; retain `Entity`, `WeakEntity`, `FocusHandle`, scroll handles, domain IDs. `CG:140`
- Split by capability (feature module/crate owning model+view+commands+dialogs), not by `views/`, `models/`, `modals/` role folders. `CG:40`, `CG:94`

### State ownership
- Put state in the narrowest owner that keeps it correct; one entity holding the whole app's unrelated state is a listed failure mode. `CG:211`, `CG:843`
- Prefer controlled values: pass current value in, receive a requested change, mutate the owner, re-render — a callback must not create a second source of truth. `CG:219`, `CV:26`
- Call `cx.notify()` after a mutation that changes rendering; update fields forming one invariant together and notify once. `CG:234`
- Never mutate state or notify unconditionally from `render` — it schedules another render and loops forever. `CG:239`, `CG:829`
- Do not feed an owner-supplied value back through the user callback during sync; track origin or compare snapshots so one logical change reports once. `CG:249`
- Render callbacks supplied by the app (item renderers, menu builders) must be side-effect-free: no business ops, no new subscriptions. `CG:316`
- Never call `entity.read(cx)`/`entity.update(cx, …)` on an outer entity from inside a render hook or a `defer_in` callback of the same entity — GPUI re-enters the lock and panics; keep a plain snapshot field. `EN:159`, `EN:196`
- Use `WeakEntity` in closures/async so a closed view is not kept alive. `EN:97`, `CG:497`

### Events, subscriptions, focus
- `cx.emit` for a semantic event an owner should handle; `cx.subscribe`/`cx.subscribe_in` (window access) / `cx.observe` when lifetime follows an entity. `CG:234`, `EV:150`
- Store every `Subscription` on the owning view (e.g. `_subscriptions: Vec<Subscription>`) or `.detach()`; a constructor-local binding dies at construction. `US:7`, `EV:196`
- Never build mutual subscriptions between two entities — infinite event loop. `EV:224`
- Retain a `FocusHandle` in the entity that owns keyboard interaction and `.track_focus(&handle)` it; without it Tab/keyboard does nothing. `CG:475`, `FH:200`
- Attach `key_context` and its `on_action` handlers to the *same* focused region; a registered Action off the focus path is not a working keybinding. `CG:481`
- Never request focus unconditionally from `render`. `CG:479`
- Transfer focus when opening an overlay, trap focus in modal surfaces, restore the previous valid target on dismissal, dismiss nested overlays top-down. `CG:477`, `CG:488`
- Stop propagation only when a nested interaction must block its parent; blanket `stop_propagation` breaks menus, selection, drag, window commands. `CG:469`
- Render a visible `focus_visible` state; an ancestor `overflow_hidden()` clips the focus ring, so leave room. `CG:478`, `CG:404`
- Start async work from an event or named method, never as a render side effect; reject stale results by revision/identity. `CG:495`, `CG:508`

### ElementId
- Repeated elements need domain-derived IDs: `Button::new(("delete-block", block.id))`, never a list index or translated label when items can move. `CG:261`, `CG:263`, `CG:845`
- Namespace child IDs with their owning object; IDs need only be unique inside the nearest stateful parent scope. `CG:262`, `EID:34`
- Never generate a fresh/random ID during `render`; a changed ID resets that UI identity (state, scroll, animation). `CG:265`, `CG:267`
- The same stability rule applies to overlay tokens, scroll handles, transition channels and persistence IDs. `CG:269`

### Theme, styling, layout
- Application code must not contain raw hex, `rgb`/`rgba`/`hsla`; read a semantic role from `cx.theme()` or add the role to the product theme. `CG:357`, `DG:153`
- Application layout must not call `px(...)`: use rem helpers (`p_2()`, `gap_3()`, `w_64()`, `text_sm()`, `h_8()`, `size_4()`) or component sizes; every direct `px()`/raw color is a review finding, allowed only for documented physical/raster/measured boundaries. `CG:359`, `CG:428`, `DG:382`
- Derive radii from the theme (`cx.theme().radius`, `radius_full()` for pills); never a literal. `DG:161`
- Anything cached from resolved layout must include `window.rem_size()` in its invalidation key (row heights, text shaping, popup geometry). `CG:433`
- `h_flex()` centers on the cross axis, `v_flex()` stretches: a full-height column in a row needs `h_full()` or the row needs `items_stretch()`/`items_start()`, else its header is clipped off the top. `CG:519`, `CG:553`
- Every scrollable region has exactly one owner; give the shrinking flex child `min_w_0()`/`min_h_0()`, and attach the scroll owner to the element owning the viewport with the inset *inside* it. `CG:562`, `CG:574`
- Compose from the standard semantic component (menu, select, popover, command) before hand-building one from `div`s; custom clickable `div`s lose focus/keyboard/disabled/a11y behavior. `CG:308`, `CG:847`
- Virtualize any collection that can grow unbounded; keep row identity separate from visible position. `CG:579`

### Public API, naming, docs
- No `pub` fields across a seam: private fields + builders + reader methods; any public struct with public fields carries `#[non_exhaustive]` and a constructor/`Default`. `SK:89`, `CG:616`
- Builders take and return `Self` and omit `set_`; `&mut self` mutation uses `set_<field>`; non-boolean replacement builders use `with_<field>`. `CG:606`, `CG:685`
- Boolean readers are `is_<adjective>`/`has_<noun>` (`is_open`, `has_selection`); never add new `can_` readers. `CG:611`, `CG:692`
- Naming families: value-like control = noun, retained model = `<Control>State`, shared ref = `<Control>Handle`, event = `<Control>Event`, delegate = `<Role>Delegate`, app-supplied presentation = `render_<part>`. `CG:668`–`CG:676`
- Use `Id` for identity, `ix` for zero-based index (never `idx`); never key or persist reorderable data by index. `CG:759`, `CG:733`
- Keep domain words exact: **selected** ≠ **focused** ≠ **hovered** ≠ **confirmed**; **open/close** vs **show/hide** vs **expand/collapse**; **disabled** vs **read-only** vs **loading**; **placement** vs **position**; **size** vs **width/height**. `CG:724`–`CG:740`
- `on_click` only for a genuine click contract; a controlled semantic primitive uses `on_change(next_value, …)`; name lifecycle hooks precisely (`on_will_change`, `on_confirm`, `on_dismiss`). `CG:768`, `CG:774`
- Spell `Context` out — `cx` is GPUI's; name any other context after what it holds. `SK:91`
- Name views/entities after product concepts (`BlockList`, `SlashMenuState`), handlers after intent (`confirm_delete`, `on_query_changed`). `CG:644`, `CG:646`
- Public doc comments start with what the type does and who owns its state; document defaults, focus behavior, callback ordering, and any required `notify`/`emit`/sync call. Do not narrate obvious builder calls. `CG:782`, `CG:653`

### Error handling & performance
- Degrade gracefully in element code (fallback content) instead of panicking; clamp indices and ranges defensively before slicing text/selections. `EBP:333`, `EBP:359`
- Surface recoverable errors in the UI with explicit idle/loading/loaded/failed states; logs are not user feedback; prevent duplicate destructive submissions. `CG:501`
- Do not rebuild entities, subscriptions, focus handles or expensive structures per frame; do not clone large strings/collections just to satisfy a closure. `CG:830`, `CG:834`
- In custom elements: no allocation or mutation in `paint`, create hitboxes in `prepaint`, keep layout state in the associated type, always check `phase.bubble()`. `EBP:446`–`EBP:535`

### Testing
- Test at the lowest layer that proves the behavior: pure tests → context tests → `VisualTestContext`/UI integration → smoke tests. `CG:806`
- Cover the semantic contract (pointer + keyboard activation, controlled value change, disabled behavior, focus movement, event order, stable identity, empty/failure states), not internals. `CG:813`
- Tests that call internal methods but never exercise keyboard or pointer behavior are a listed failure mode. `CG:854`
- Add a regression test before fixing any deterministically reproducible bug. `CG:817`
- Add `test-support` to the dev-dependency on `gpui-kit` with exactly the same source and version as the normal dependency. `T:23`
- Custom (non-Kit) elements must opt into queries with `.id("x").test_support()` from `TestSupportExt`, placed **before** `.track_focus(&handle)`; wrappers must forward the method. `T:58`
- Exercise the real handler path via `TestWindowExt`; invoking callbacks or setting control state directly bypasses the behavior under test. `T:61`
- "Compiles" is not a quality bar; an uncompiled example or a test that never checks the outcome is not completion evidence. `CG:873`, `T:68`

## 2. Design rules that apply to a text-editor page

- **Spacing scale** — 2/4/8/12/16/24/32 px semantics: `xxs` optical, `xs` parts of one control, `sm` closely related controls, `md` one content group, `lg` panel padding & groups, `xl` separate sections, `xxl` region boundary. Read them via `cx.theme().spacing_tokens()` / rem helpers, never literals. `DG:171`, `DG:186`
- Spacing expresses relationship: block↔block gap < section gap; equal gaps imply equal relationships. Padding belongs to the component, gaps to the parent; do not double padding (a padded page band + padded block). `DG:199`–`DG:215`
- Keep repeated rows on stable columns (gutter handle, block content, trailing actions) so the eye can scan. `DG:207`
- **Typography** — platform UI font for interface text, monospace only for code blocks, identifiers, shortcuts, aligned numerals; type steps ≈12/14/16/18/20; avoid excessive uppercase/letter-spacing. `DG:412`, `DG:172`
- Hierarchy = page title → section title → body → muted metadata → labels, built from size/weight/spacing/separators *before* color or containers; avoid cards inside cards. `DG:106`, `DG:115`
- **Color tokens** — `background`/`foreground` main surface, `popover`+foreground for the slash menu, `muted`/`muted_foreground` for placeholders and block hints, `primary` for the principal action/selection, `danger|warning|success|info` only for meaning, `border`/`input`/focus-ring for structure. `DG:132`
- Never encode meaning by color alone; verify light + dark + custom themes; keep Badge/Alert variants scarce and mostly neutral. `DG:145`, `DG:149`
- **Overlays/popovers** — pick the smallest surface: tooltip (shortcut), popover (contextual controls), menu (actions), notification (async status), dialog (decision/short form), alert dialog (consequential confirm), sheet (persistent side work). `DG:627`
- Slash menu / block menu must reuse the themed popover surface and the Kit menu system rather than approximating border+shadow literals; menus already own directional keys, confirm/cancel, disabled items, submenus, focus transfer and restoration. `DG:404`, `DG:575`
- Avoid stacking overlays; Escape dismisses the topmost dismissible layer and focus returns to the trigger. An overlay action must refer to something the overlay actually shows. `DG:643`, `DG:646`
- Prefer visible command surfaces over hover toolbars: primary action stays visible, secondary behind a visible `DropdownMenu`, object-scoped commands in a `ContextMenu`, plus a key binding; a hover-revealed icon is only a shortcut to a command reachable elsewhere. `DG:558`–`DG:568`
- Never make hover the only path to a destructive/essential block action (delete, duplicate, turn into). `DG:551`
- **Motion** — short transitions for appear/dismiss/expand only; never animate large layout changes when opacity/transform says the same; honor reduced motion; never require animation to understand state. `DG:661`, `DG:664`
- **Interaction states** — design rest, hover, pressed, open-while-popup-shown, focus-visible, selected/checked, disabled, loading, error for every control; a block handle owning the slash/plus popup stays visibly pressed until dismissal. `DG:519`, `DG:532`
- Selection (current block, multi-block) is part of the information model and needs a persistent visual state distinct from hover, plus distinct focus/hover/active-row treatments in dense views. `DG:532`, `DG:679`
- Reversible edits get undo or a quiet notification; use `AlertDialog` only for serious irreversible loss, naming the object and the consequence. `DG:539`, `US:225`
- **Density** — medium is the default for the page body; use `small`/compact for the toolbar, gutter and menus, changed for the whole local context via the component `Size` API (never a bare height override). `DG:338`, `DG:346`
- Zoom is the theme base font through `Root`; verify hierarchy, wrapping, focus-ring clearance, popup placement and virtual row measurement at several base font sizes. `DG:358`, `DG:392`
- Icon-only controls (block handle, drag grip) require a tooltip and accessible name; one icon family only. `DG:417`
- **Copy** — sentence case (`Turn into`, not `Turn Into`); no final period on labels/menu items/placeholders; single `…` character on every command that opens a dialog/sheet or needs more input; no exclamation marks; errors say what happened plus recovery. `DG:789`, `DG:803`, `DG:809`, `DG:818`
- Let context carry context: nouns for objects, verbs for commands, adjectives for states; drop `Management`/`Operation` wrappers; keep one canonical term per block type everywhere (menu, tooltip, shortcut search, docs). `DG:715`, `DG:740`
- Provide a useful empty state that explains the next action (empty page → placeholder describing the slash command). `DG:687`

## 3. Violations to check for in this architecture

Findings from `src/editor/**` measured against the rules above.

1. **110 raw `px(...)`/`rgb(...)` occurrences** (`blocks.rs` 44, `gutter.rs` 17, `style.rs` 14, `view.rs` 10, `slash.rs` 9, `ui.rs` 9, `block.rs` 5). Rule: application layout must not call `px()` and must not construct raw colors — use rem helpers/component sizes and `cx.theme()` roles, or define the missing role in a product token layer. `CG:357`, `CG:428`, `DG:382`
2. **`Entity<NotionEditor>` holding `Vec<Block>` + all editing, selection, slash-menu and overlay state** risks "one entity containing the entire application's unrelated state". Rule: narrowest correct owner; split document model / selection / overlay state so updates and notifies stay local. `CG:211`, `CG:843`
3. **Each block owning `Entity<EditorState>` while the parent also caches block text** is a classic duplicated-state drift and a feedback loop: parent writes a value into the state which re-emits `InputEvent::Change` back. Rule: controlled value with one source of truth + origin tracking so one logical change is reported once. `CG:225`, `CG:249`
4. **`.id(("block", id.0 as usize))`** (`view.rs:556`) is domain-derived — keep it that way; audit every other repeated child (gutter buttons, slash items, marks) for index-derived or per-frame IDs and namespace them under the block ID. `CG:261`, `CG:262`, `CG:265`
5. **Block-level subscriptions vector** (`view.rs:174`–`206`, `block.rs:386,:392`): confirm subscriptions are dropped when a block is deleted and recreated on re-insert, and that no block subscribes back to the editor (mutual subscription loop). `EV:196`, `EV:224`
6. **Overlays via `deferred()` + `Positioner`**: inside a `defer_in` callback the current entity is locked — calling `editor.update(cx, …)` from a block's deferred callback (or `block_state.read(cx)` from the editor's render closure) panics. Use the `&mut` reference the callback provides and snapshot fields for render. `EN:159`, `EN:196`
7. **Hand-built slash menu** (`slash.rs`) instead of `PopupMenu`/`Command`: rebuilds arrow navigation, confirm/cancel, disabled items, focus transfer and restoration, dismissal. Rule: compose the standard semantic component, or extend its API. `CG:308`, `DG:575`
8. **Custom `BlockSpec` registry as a public seam** (`pub struct BlockId(pub u64)`, `pub` fields in `BlockAttrs`, `pub type BlockType = SharedString`): public fields across a seam need `#[non_exhaustive]` + builders/readers; type-alias-as-enum loses exhaustiveness. `SK:89`, `CG:616`
9. **Global registry via `Global`** (`block.rs:14`): a registry is an app service — fine as a global, but document ownership and keep mutation out of `render`; prefer a named type (`BlockSpecRegistry`) over `Manager`/`Config` names. `CG:215`, `CG:742`
10. **Single editor `FocusHandle` for a multi-block page** (`view.rs:36`): each block's `EditorState` carries its own focus; assert who owns `key_context(actions::CONTEXT)` and that block-level actions are bound on the focused block's region, not only the page container. `CG:475`, `CG:481`
11. **`focus_handle.focus()` calls from `gutter.rs:124` / `keymap.rs:315`**: ensure none of these run from `render`, and that opening a slash popover transfers focus and restores it on dismiss. `CG:479`, `CG:477`
12. **Page scrolling**: give the scroll owner the content inset and `min_h_0()` on the shrinking flex child; a bare `h_flex()` around gutter+content needs `items_stretch()` or the gutter column is centred and clipped. `CG:562`, `CG:519`
13. **Long documents**: `Vec<Block>` rendered in full will not hold; plan `v_virtual_list` with model-coordinate keyboard selection and rem-aware measurement invalidation. `CG:579`, `CG:589`, `CG:433`
14. **Naming**: check `_idx` vs `_ix`, `selected` vs `focused` vs `active` for blocks, and `set_*` on fluent builders in `BlockSpec`. `CG:759`, `CG:724`, `CG:685`

## 4. Testing recipe (UI integration)

```rust
// tests/ui.rs — no `use gpui_kit::*;` here (it shadows Rust's #[test]). T:22
use gpui_kit::test::{TestSupportExt, TestWindowExt};
use gpui_kit::{AppContext, Context, Entity, TestAppContext, Window, component::Root, px, size};

#[gpui_kit::test]
fn typing_splits_a_block(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);          // 1. init first
    cx.update(gpui_notion::editor::init);
    let mut editor = None;
    let handle = cx.open_window(size(px(900.), px(700.)), |window, cx| {
        let view = cx.new(|cx| NotionEditor::new(window, cx)); // production view + ctor
        editor = Some(view.clone());
        Root::new(view, window, cx)      // 2. wrap in Root; mount overlay layers in render
    });
    let editor = editor.unwrap();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);                       // 3. frame before querying
        window.click("block-1", cx);                   // 4. real pointer/keys only
        window.input("Hello", cx);
        assert_eq!(window.find("block-1").focused(), Some(true));
        assert_eq!(window.find("block-1").value(), Some("Hello"));
        window.press("enter", cx);                     // named keys: backspace, escape, secondary-a
        assert!(window.find("block-2").bounds().top() >= window.find("block-1").bounds().bottom());
    }).unwrap();

    cx.update(|cx| assert_eq!(editor.read(cx).blocks().len(), 2)); // 5. app result too
}
```

- `Cargo.toml`: `[dev-dependencies] gpui-kit = { version = "0.6.1", features = ["test-support"] }` — same source/version as the normal dep. `T:23`
- Queries: `window.find(id)` (panics if absent/ambiguous), `window.try_find(id)`, `window.within("slash-menu").find("item-heading")` for repeated local IDs. `T:176`
- Interactions: `click`, `right_click`, `double_click`, `hover`, `click_at(id, offset, cx)`, `scroll(id, delta, cx)`, `drag_to(from, to, cx)`, `window.drag(from_pt, to_pt, cx)`, `press(key, cx)`, `input(text, cx)`. `T:180`–`T:188`
- Custom block elements are invisible to `find` until they call `.id("block-1").test_support()` (from `TestSupportExt`), placed before `.track_focus(&handle)`; wrapper elements must forward `test_support()`. `T:58`
- Focus assertions: `find(id).focused()` measures the observed native focus scope; `None` can mean the element does not advertise `Action::Focus` — register the real binding rather than asserting on test-only state. `T:216`
- `disabled()` returns only `Some(true)`/`None`: to prove a control is enabled, perform the interaction and assert the outcome. `T:213`
- Snapshots are frame facts, not live handles: re-`find` after every action; call `window.render_frame(cx)` after an external entity update, focus change or resize. `T:199`
- Deferred work (overlays, `defer_in`, subscriptions) needs to leave `update_window`: use `cx.run_until_parked()` or `cx.wait_for(handle, Duration::from_secs(1), |window, _| window.try_find("slash-menu").is_none()).await`. `T:204`
- Process a queued action *before* changing an input that the action validates, or it reads the later value. `T:212`
- `Animation` uses wall-clock time; advancing test time does not finish it — use reduced motion or a bounded wait before asserting final geometry. `T:220`
- Always include a negative case (disabled control rejects the interaction, invalid input blocks save) and assert the visible outcome after each meaningful action. `T:65`
- Not covered in-process: pixels, packaged-app behavior, full OS IME composition. `label()` is the accessibility name, not rendered text. `T:228`, `T:222`

## 5. Audit of gpui-notion

Format: `path:line — rule broken (citation) — fix`.

### State ownership / entity vs element

- `src/editor/block.rs:386,:392` — `pub state: Entity<EditorState>` and `pub subscriptions: Vec<Subscription>` publish behavioral state across the seam; private fields are the default for state that must evolve (`CG:616`) — make both private, expose `state(&self) -> &Entity<EditorState>`, and keep subscription ownership an implementation detail.
- `src/editor/block.rs:382` — `pub text: String` mirrors `EditorState`'s value, a second source of truth that must be resynced on every `InputEvent::Change` (`CG:225`, `CG:249`) — keep the mirror but make it private with a `text()` reader, and document in one place that `EditorState` is authoritative and `text` is the diff snapshot.
- `src/editor/view.rs:33-48` — one `NotionEditor` entity owns document, focus, hover, node selection, slash menu, drag target and wrap width; "one entity containing the entire application's unrelated state" is a named failure mode (`CG:843`, `CG:211`) — split at least the transient interaction state (`hovered`, `selected`, `slash`, `drop_target`) into a `SelectionState`/`SlashMenuState` so an edit does not notify the drag machinery.
- `src/editor/slash.rs:21-28` — `SlashMenu` is retained interaction state with `pub` fields mutated from several modules (`slash.rs:141`, `keymap.rs:301`) (`CG:616`, `CG:211`) — private fields + `query()`, `selected_ix()`, `select(delta)`.
- `src/editor/block.rs:452-466` — `BlockContent` builders are correct (`with_attrs`/`with_marks`/`with_indent`, `CG:685`); keep that shape when the type gains fields.
- `src/editor/view.rs:181` — per-block `cx.subscribe_in` stored on the block is right (`EV:196`, `US:7`); verify `remove_block` (`view.rs:230`) drops them — it does, via `Vec::remove`. No change.
- `src/editor/block.rs:200`, `src/editor/slash.rs:35` — `run: fn(&mut NotionEditor, …)` hard-wires the registry to the concrete view, so `BlockSpec` cannot be reused by another host (`CG:28`, dependency direction) — take a command enum or a `&mut dyn BlockCommands` seam instead.

### ElementId stability

- `src/editor/view.rs:567` — `.id(("block", id.0 as usize))` is a domain-derived, namespaced ID and is **correct** (`CG:261-262`, `EID:34`); the same applies to `gutter.rs:81` `("insert", id.0 as usize)`. Keep it.
- `src/editor/slash.rs:219` — `.id(("slash-item", index))` keys a row by its position in a *filtered* list, so the same ID means a different command after every keystroke (`CG:263`, `CG:845`) — key by the item's title/command identity: `("slash-item", item.title)`.
- `src/editor/view.rs:625` — `group_name` builds a `SharedString` with `format!` for every block on every frame (`CG:834`) — store the group name on the `Block` once, or derive the group from the already-stable `ElementId`.

### `pub` fields on public types — what the guide actually requires

The rule is: private fields are the default for behavioral state; `pub` fields are acceptable only for deliberately record-like configuration/geometry/serialized schemas, and then **every public struct with public fields must carry `#[non_exhaustive]`** plus a constructor, `Default`, or builders so callers never write an exhaustive struct literal (`SK:89`, `CG:616-622`).

- `src/editor/block.rs:46` `BlockAttrs`, `:86` `BlockCaps`, `:135` `BlockLayout`, `:184` `BlockInputRule`, `:192` `SlashItem`, `:203` `BlockContext`, `:413` `BlockContent`, `src/editor/gutter.rs:19` `DraggedBlock`, `:47` `DropTarget` — record-like, so `pub` fields are allowed, but none is `#[non_exhaustive]` (`CG:619`) — add `#[non_exhaustive]` to each and keep `Default`/semantic constructors so adding a field is not a breaking change.
- `src/editor/block.rs:24` — `pub struct BlockId(pub u64)` exposes the representation of an identity (`CG:616`) — make the field private with `BlockId::new(u64)`/`get()`, or accept it as a record-like newtype and mark it `#[non_exhaustive]`.
- `src/editor/ui.rs:12` — `pub struct Lucide(pub &'static str)` same issue, plus it duplicates `path()`/`path_string()` (`ui.rs:16`, `ui.rs:29`) — one inherent method, private field.
- `src/editor/block.rs:27` — `pub type BlockType = SharedString` gives a public API no type safety and no exhaustiveness (`CG:668` value-like types are nouns; `CG:733` id vs index precision) — keep the open registry, but wrap it: `pub struct BlockType(SharedString)` with `as_str()`.

### Colors, `px`, tokens

- `src/editor/style.rs:57,75-88,95-113` — raw `gpui_kit::rgb(0x…)` Notion palette in application code (`CG:357`, `DG:153`). The guides' handling for a palette that is *not* a theme token: raw colors belong only in the theme/token definition layer; if the semantic role does not exist, **define it in the product's theme/token layer** rather than at the call site (`DG:155-157`, `CG:428`). Fix: promote `style.rs` into an explicit product token type (e.g. `NotionTokens { highlight: [Hsla; 7], text_colors: […] }`) stored as a global/design-system state, resolved once per theme change (and re-derived on `Theme::change`), with call sites reading `NotionTokens::global(cx).highlight(color)`. The user-chosen highlight/text colors are then documented, audited data colors — the one allowed exception (`CG:430`).
- `src/editor/style.rs:9-32` — every page metric is a `px()` constant, and `block.rs:161-171`, `blocks.rs:82-85,149,194,258,377-389,453-477,524-538,608-618`, `ui.rs:40-77`, `gutter.rs:28-84`, `slash.rs:206-245` repeat literal `px()` in layout (110 occurrences total) — application layout must not call `px(...)`; use rem helpers and component sizes so type, spacing, icons and hit targets zoom together (`CG:359`, `CG:428`, `DG:382`). Fix: express page width/padding/indent/markers with `w_*`, `p_*`, `gap_*`, `text_sm/base/lg`, and keep `px()` only for the hairline in `blocks.rs:618` and documented raster/physical boundaries.
- `src/editor/view.rs:44,:60` — `wrap_width` is initialised from constants and never keyed on `window.rem_size()`, while `measure_rows` (`view.rs:450-472`) caches nothing but *depends* on rem-derived text size (`CG:433`) — include `window.rem_size()` (and the theme revision) in whatever invalidates measurement, and set `wrap_width` from real layout, not from `PAGE_WIDTH - PAGE_PADDING*2`.

### Render-time allocation and performance

- `src/editor/view.rs:450-472` (via `render_text` → `block_height` → `measure_rows`) — every frame re-wraps **every** block's text with a fresh `line_wrapper` to compute a fixed height; measurement per frame for the whole document breaks "avoid rebuilding expensive structures per frame" and "measure before adding caches; a cache must have a clear invalidation owner" (`CG:830`, `CG:437`) — cache row counts on the `Block`, invalidated by text change + `wrap_width` + `rem_size` + font revision.
- `src/editor/gutter.rs:65` — `block.text.clone()` allocates the full block text every frame just to build a drag preview (`CG:834`) — clone lazily inside the drag listener.
- `src/editor/view.rs:681-686` — `render` materialises `Vec<usize>` + `Vec<AnyElement>` for all blocks with no virtualization; long documents need `v_virtual_list` with model-coordinate keyboard selection (`CG:579`, `CG:589`).
- `src/editor/slash.rs:121-139, :199` — `slash_items(cx)` rebuilds and re-filters the whole registry list inside `render_slash_menu`, and again in `move_slash_selection`/`confirm_slash_item`; render callbacks should be cheap and side-effect-free (`CG:316`, `CG:829`) — resolve the filtered list once when the query changes and store it on the menu state.

### Naming and doc comments

- `src/editor/block.rs:206` `BlockContext.index`, `src/editor/slash.rs:171` `run_slash_item(index)` — new zero-based indices use `ix`, never `index`/`idx` (`CG:759`, `CG:698`) — rename to `ix` (keep `selected_index`-style established Kit names untouched).
- `src/editor/block.rs:77,81` — `BlockAttrs::get`/`set` are vague public names (`CG:742`); a plain reader takes the field noun and in-place mutation is `set_<field>` (`CG:684`, `CG:686`) — `extra(key)` / `set_extra(key, value)`.
- `src/editor/block.rs:89` `BlockCaps` / `caps()` — abbreviation that is not an established ecosystem term and mixes abbreviation levels with `BlockLayout`/`BlockAttrs` (`CG:113`, `CG:653`) — `BlockCapabilities`/`capabilities()`, or document the short form.
- `src/editor/view.rs:31` `DocumentChanged` — semantic notifications are `<Control>Event` (`CG:672`) — `EditorEvent::DocumentChanged`.
- `src/editor/view.rs:751` `focus_handle_for_editor()` duplicates `Focusable::focus_handle` with a role-less name (`CG:742`) — delete it and call `self.focus_handle(cx)`.
- `src/editor/block.rs:194` `SlashItem.subtext` — the ecosystem term is `description` (`CG:661` vocabulary is part of the API).
- `src/editor/view.rs:41-43` — `focused`, `hovered`, `selected` are used precisely here (`CG:724`); keep that discipline in `BlockContext` too.
- `src/editor/block.rs:377`, `:413` — public docs must say **who owns the state** and note any required `notify`/`emit` (`CG:782`); `Block`'s doc does not say that mutating it requires `cx.notify()` + `cx.emit(DocumentChanged)` — add it.
- `src/editor/block.rs:29`, `:348`, `:76`, `src/editor/view.rs:82-99`, `src/editor/ui.rs:20` — `pub mod types`, `BlockRegistry::iter`, `BlockAttrs::get/set`, `block_count`/`index_of`/`block`/`block_mut`/`spec_at`, `ui::icon` are public with no doc comment (`CG:782`) — one line each: what it does, who owns the result.

### Overlays, Escape, focus return

- `src/editor/actions.rs:34,67` — `Cancel` is bound to `escape` but no handler exists anywhere (`on_escape` handles `input::Escape`, `keymap.rs:300`); a registered Action without a handler on the focused path is not a working keyboard interaction (`CG:481`) — either handle `Cancel` on the editor region or delete the action and binding.
- `src/editor/slash.rs:237-247` — the menu is a hand-built `deferred` + `Positioner` surface, so it re-implements what `PopupMenu`/`Command` already own (arrow navigation, confirm/cancel, disabled items, groups, focus transfer and restoration, outside-click dismissal) (`CG:308`, `CG:847`, `DG:575`) — compose the Kit menu, or extend its API.
- `src/editor/slash.rs:241` — `.occlude()` blocks clicks through the popup but nothing dismisses the menu on an outside click or on window blur; a menu's dismissal model is part of its contract (`CG:308`, `DG:643`) — add outside-press dismissal.
- `src/editor/slash.rs:44-60` — opening the menu never moves focus and there is no focus trap or restore point; overlays must transfer focus on open and restore it on dismissal (`CG:477`). Here focus deliberately stays in the block input, which is the right editor behavior — document that exception next to `open_slash_menu` (`CG:653`) so it is not read as an omission.
- `src/editor/keymap.rs:300-317` — Escape closes the menu, then clears selection, then selects the block: correct topmost-first ordering (`DG:643`). But the third branch moves focus to the editor handle without a visible `focus_visible` treatment on the page root (`CG:478`) — render a focus ring or rely on the selected-block highlight and say so.
- `src/editor/view.rs:680-725` — `render` never mounts `Root::render_dialog_layer`/`render_sheet_layer`/`render_notification_layer`, so any dialog (link editor, confirmations) or notification will simply not appear (`RC:8`, `T:55`, `US:389`) — add the three `.children(...)` calls to the root element.
- `src/editor/ui.rs:55`, `:73` — `cursor_pointer()` on menu rows and toolbar buttons; desktop menu items and buttons use the default arrow cursor, the pointing hand is for links (`DG:545`) — drop `cursor_pointer` (keep `cursor_text()` on `view.rs:718`, which is correct).
- `src/editor/gutter.rs:78-81` — the `+` and drag handle are `invisible()` until `group_hover`; a hover-revealed icon is acceptable only as a shortcut to a command reachable elsewhere (`DG:567`, `DG:551`) — `+` is covered by Enter, but drag-reorder needs its `MoveBlockUp`/`MoveBlockDown` bindings surfaced in a context menu or tooltip with the shortcut.
- `src/editor/slash.rs:205-215` — group headings are rendered as plain `div`s with literal `px(11.)` text; menus should reuse the themed popover/menu geometry (`DG:404`) — use the Kit menu's group/label part.

