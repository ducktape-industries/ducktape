# Boards

Shared native canvases: notes, boxes, text and arrows whose endpoints follow
cards. Every authenticated signer on the network can edit a board. Reads are
public; the recorded creator is attribution, not a privacy boundary.

`Operation::Edit` carries one field operation. The view applies it immediately
and submits it in the background. Consensus orders writes to the same field;
move, resize, text and color operations preserve other fields. Deleting a card
also removes its arrows. Late field edits to deleted cards are no-ops.

The view uses the existing gpui-kit canvas, buttons, text input and theme through
the WASM wire contract. Camera, selection, undo history and unfinished gestures
are device-local. Text is applied with Enter or **Apply text**. Drag the lower
right corner to resize, use **Pan** or the scroll wheel to move the canvas, and
use **Fit** or the zoom buttons to frame it. **Connect** takes two card clicks.

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
