# Boards

Shared native canvases: notes, boxes, text and arrows whose endpoints follow
cards. Every authenticated signer on the network can edit a board. Reads are
public; the recorded creator is attribution, not a privacy boundary.

`Operation::Edit` carries a field operation; `Operation::Batch` applies a whole
selection gesture atomically. The view applies changes immediately and submits
in the background. Consensus orders writes to the same field; move, resize,
text and color preserve other fields. Deleting a card also removes its arrows.
Late field edits to deleted cards are no-ops. Creation order determines stacking;
editing a card does not bring it to the front.

The view uses gpui-kit buttons, theme, native multiline Editor and Canvas through
the WASM wire contract. Camera, selection, history and unfinished gestures stay
local. Pending edits survive view snapshots, but are not persisted across app
restarts. Remote cursors, comments and shape clipboard are not provided.

## Controls

| Action | Input |
|---|---|
| Select / pan / note / box / text / connect | V / H / N / R / T / A (also 1–6) |
| Keep the creation tool active | Q or the lock button |
| Temporary pan | Space + drag; middle-button drag |
| Add/remove selection | Shift + click |
| Area selection | Drag empty space with Select |
| Select all / duplicate | Cmd/Ctrl+A / Cmd/Ctrl+D |
| Edit text | Enter or double-click a card |
| Text newline / finish | Enter / Cmd/Ctrl+Enter, Esc or Done |
| Adjacent note | Cmd/Ctrl+Enter outside text editing |
| Undo / redo | Cmd/Ctrl+Z / Cmd/Ctrl+Shift+Z (also Ctrl+Y) |
| Move selection | Arrow keys; Shift moves 10 units |
| Resize | Drag any corner; Shift preserves aspect ratio |
| Bypass alignment snapping | Alt while dragging |
| Pan / cursor-anchored zoom | Scroll / Cmd/Ctrl+scroll |
| Fit board / fit selection / reset zoom | F / Shift+F / 0 |
| Cancel gesture / clear selection | Esc |
| Keyboard help | ? |

Notes and text enter editing on creation. Native text input owns its keys; board
shortcuts do not run while typing. Multiple cards move, change color, align,
duplicate and delete together; their gesture is one history entry. Connections
follow their endpoint cards. Undo submits inverse field operations: a concurrent
edit to the same field can be overwritten by that inverse, following consensus
order. Camera and remote updates do not enter local undo history.

## Build and register

Build the consensus component from a committed, pushed revision:

```sh
cargo run -p guest-builder -- crates/modules/apps/boards
bash ops/build-views.sh -p boards-view
```

The desktop discovers view-only registry entries. Register the core as `boards`
and its UI as `canvas`; these are two ordinary deployments and do not change
the founding set:

```sh
ducktape module register boards crates/modules/apps/boards/component.wasm
ducktape module register canvas --view target/views/boards_view.wasm --assets crates/views/boards/assets
```

The UI subscribes to `canvas.props` and reads/submits against `boards`. Its tab
appears through the existing registry view discovery after activation. All WASM
bytes are loaded from files; no node or desktop binary embeds the board.

## Bounds and checks

The module allows 64 boards, 128 live shapes per board, 2048 UTF-8 bytes per
shape's text, and a 768 KiB encoded board. A board is one authenticated store
record. This deliberately bounded layout keeps atomic card/arrow deletion and
reopen simple; raising the board size requires revisiting that storage cost.

```sh
cargo test -p boards
cargo clippy -p boards --tests --no-deps
cargo test --manifest-path crates/views/Cargo.toml -p boards-view
cargo clippy --manifest-path crates/views/Cargo.toml -p boards-view --tests --no-deps
cargo test -p wasm-host --test boards
BOARDS_VIEW_WASM="$PWD/target/views/boards_view.wasm" cargo test \
  --manifest-path crates/views/Cargo.toml -p boards-view \
  --features host-verification --test wasm
```
