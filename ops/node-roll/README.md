# Rolling the node to a new contract

How duckpond moves from node contract N to N+1. Agreed in #engineering 1651 (10-21) after that day's roll left phones on the wrong contract twice.

## The goal

The app people run never speaks a different contract from the node, in either direction, not even for minutes. There are no compat versions, so the order has to do the work. The only gap left is a phone installing the new build, and the app it already has tells it to update. If an order can't avoid a mismatch, the roll waits.

## 1. One hour before

- Pick the roll height R. Post R and the expected time in #engineering, in a thread of its own (the roll thread). Mention eddy (18), orthory (15), the operator (22) and every resident operator.
- Check that the app people have installed already shows "this node speaks contract X; update this app" (app #144 and later). A roll never ships that screen.
- Stage every client's contract N+1 build and say where each one is. Staged means proven, not only built: each build has run once against an N+1 node (a local network, or a rehearsal on a copy) before R. On 10-21 chief-relay's first contract-10 image crash-looped, because it signed queries as writes. The clients:
  - the node and every resident (each runs `ducktape roll R` at R);
  - pods, the mac runner, forge-mirror, pushd, chief-relay, pi-chief;
  - the reconcile, watch and warm pod images, and the warm pod's shared `/out` binaries (its pause check reads `/out/ducktape` and fails closed);
  - the desktop build runner's toolchain and out images;
  - every repo's CI toolchain pin under `.forge/workflows` (tools check.toml, app gates and release tomls, programs gates.toml);
  - the operator's own CLI scripts (post, readback, alerts), in their N+1 form;
  - each agent's CLI, its git helper (git-remote-duck as a release, not only a blob under ops/) and change_op;
  - ts-client and the chat bridge;
  - the operator's client list from the last roll, which is kept as the template.
- The contract N+1 app change is reviewed and ready, but not merged.
- If TestFlight allows it, build the N+1 app from that reviewed branch now and hold it undistributed.
- Acks. Every client owner acks R in the roll thread before R. A missing ack either holds the roll, or goes on the checklist as a named gap before R.

## 2. At R: one sitting, no gaps

- pi-chief is scaled to 0 before the node stops at R, and comes back only as its proven N+1 image after "node proven", **with its `node` record reset** (empty, so it takes the running N+1 image as proven). With its old record it would treat N+1 as unproven and, on a stall in the next epoch, set the node back to the N image, which refuses state stamped with the new contract.
- The node stops at R and rolls. Residents run `ducktape roll R` with it.
- The operator posts "node proven" from node-only checks (apply_slow, state root, history, egress, decodes) before rolling any image. PAUSED and the agents' after-roll steps wait on that word, and the image rolls come after it.
- As soon as `/v1/status` answers contract N+1, merge the app change, then build TestFlight and desktop, or release the held build.
- Post in the roll thread at R, and again once the node is proven.

## 3. The gate

**Planned, not built yet (J-134):** App CI's TestFlight and desktop release jobs build only when app dev's `NODE_CONTRACT` equals the live node's contract (`/v1/status`). With it, an app on the wrong contract can't reach a phone: before R it is held back, and after R it is the only build that goes out.

**Until that gate lands,** pause TestFlight and desktop releases from dev for the window by hand: from the moment the N+1 app change is ready until it merges at R.

tools' `build.sh pack` (tools change 102) checks one side only: it refuses a program built for a contract past the node's. It doesn't refuse an older-contract build after R, so it is not the same gate.

## 4. After

Post a checklist in the roll thread with one line per client from step 1. Each line is either confirmed, with its height or build, or not yet, with who owns it.
