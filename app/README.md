# Ducktape desktop

Native GPUI desktop shell with dynamically loaded, Rust-authored WASM views.

```bash
cargo build -p node-bin
make views                  # the tabs: wasm views staged under target/views
cargo run -p ducktape-app
```

The launch window lists the workspaces under the ducktape home (one
directory per network, `$DUCKTAPE_HOME` when set, else `~/.ducktape`) and the
remote endpoints it has saved; picking one opens that workspace's keystore.
An RPC endpoint typed in wins whenever it is set; left empty it resolves
`DUCKTAPE_NODE`, else the one workspace under the home, else
`http://127.0.0.1:8844`. Chat + Pages hydrate over HTTP after the resumable
`module:chat` and `module:pages` WebSocket topics are active, then rehydrate on
committed changes. Writes sign with the encrypted key at `DUCKTAPE_USER_KEY`,
else the picked workspace's active wallet (`<workspace>/keys/<name>.key`); no
active wallet is a refusal, not a guess — unlock one in the launch window.
The app's own state — its preferences, its log, its forge mirrors — lives in
the platform's application directories (`~/.config`, `~/.local/state` and
`~/.cache` on Linux, `~/Library/Application Support`, `~/Library/Logs` and
`~/Library/Caches` on macOS; an `XDG_*` variable wins on either), never under
the home.

Agents keeps a compact run list on the left and opens the selected run's
Conversation, Trace, Journal and Raw tabs in the main pane, with session
controls available across tabs.
The process disclosure shows provider thinking as Markdown and groups tool inputs
with their results. The executor's elapsed time labels the closed session
(`Worked for 2m 5s`); the answer remains visible when the process is collapsed.
Raw lists compact event rows; expanding a row shows its formatted JSON.
Conversation renders reply blocks as Markdown and leaves execution metadata
in Raw.
Journal message references use compact channel and message labels, with inline
previews and links to the original chat context. Repeated trigger references
appear once in Relevant.
The external requester, or the current controller of a program requester, can
read its output, add instructions, stop the run, and answer pending tool
approvals. Codex steers its active turn; Claude interrupts the current response
and accepts the instruction in the same process and session. Controls require a
compute worker attached to the connected node; a peer's mirrored output alone
does not provide a control connection. Trace is a bounded live buffer and can
expire. Settled runs use the same requester/controller access check for buffered
output.
Connection failures remain visible above the tabs and offer a
reconnect action; an empty buffer is distinct from a refused connection.

## Module-owned views

The Approvals, Members, Agents, Node, Explorer, Settings, Chat, Files,
Pages and Forge tabs keep their state and behavior in WASM views under `crates/views`
(`governance`, `members`, `agents`, `node`, `explorer`, `settings`, `chat`, `files`,
`pages`, `forge`). Rust cdylibs compile to `wasm32-unknown-unknown`; wasm-tools wraps their embedded WIT exports as components that the app
loads from a file at runtime (`src/module_view.rs`).
`make views` builds every view under `crates/views` and stages it as
`target/views/<module>_view.wasm`, where a built binary looks for it
(`DUCKTAPE_VIEWS_DIR` overrides; the native packaging script carries the
directory into `Ducktape.app` as resources linked beside the executable, and the Linux
`make install-app` stages it beside the binary in the release it seeds); `make dev`
and `make app` run it first. A tab whose view is not
staged says so in its place.

Deployed views follow the module registry's active deployment hash on block
events. The host fetches and verifies a candidate, compiles it away from the
window thread, then snapshots the seated guest and restores that state into
the candidate. Installation rechecks the seated instance and snapshot tick.
The chain and existing view keep running during preparation; a rejected
candidate leaves the seated view in place. Input routes belong to the seated
instance, not to an in-progress load attempt. Matching native input values,
selections and focus can survive installation without retaining old callbacks.

`ducktape module update` accepts `--view` and `--assets` alongside the module
component and optional index. A deployment is a complete set: omitting its
view or index removes that part on activation, rather than keeping an older
copy. See [`../docs/records/architecture/wasm-module-authoring.md`](../docs/records/architecture/wasm-module-authoring.md)
for module packaging and registry operations.

A view owns its state, receives session and domain props (`<module>.props`,
one JSON item per change), and emits a wire tree rendered by native gpui-kit
controls. It can emit intents (`governance.vote`, `members.propose`, …) to
the app or request operations through the host kernel. Signing secrets stay
in the app; network access and timers run through the kernel rather than
direct guest OS access. A view that traps shows why in its place instead of
taking the window with it. A
view can also request `host.widget` operations on its own mounted tree and nested overlays:
focus traversal, targeted focus and focus queries; native text-input cursor
and selection operations; and scroll offsets, relative movement, end snapping
and keyed-row reveals. Requests are validated and bounded, run only after the
matching native frame is laid out and editor work has drained, and are refused
if their frame was replaced. They cannot address another view's widgets.
A view may leave a slot for native rendering, such as the Files and Forge
code and Markdown readers (`src/module_view/surfaces.rs`). Node's Activity
tab keeps its log rows in the guest and composes native controls through the
wire tree. A view that needs the network uses the kernel's bounded request/reply interface:
Explorer queries through `rpc.query` and `rpc.view`. A
view keeps its own drafts and hands the app only what the reader submitted:
Settings' rename, key and ticket fields cross as intents, the signing seat
crosses in as a flag (the password never leaves the app), and a committed op
tells the view which drafts it consumed. Files keeps an unsaved text edit with its
original file and network when navigation changes. Returning to that file
resumes the same edit; only an explicit discard or that save's successful
reply clears it. Saving uses the snapshot that supplied the original text,
so an intervening edit is refused without losing the draft. Pages owns Markdown
conversion, toolbar actions, comment anchors and undo history in the guest. Its
rich presentation supplies native blocks explicitly. The host validates those
primitives and retains GPUI focus, IME and caret state. Bounded document transfers
and ordered acknowledgements keep old-instance notifications from overwriting
a replacement's canonical document.
Views can use native surfaces for decoded pictures, Markdown display and
highlighted code. Surface arguments describe the content, and link events return
to the guest's own handler. Chat and Forge own their composer drafts, mentions,
formatting, undo, attachments and message submission in WASM.
Forge and Files project large read-only text and lists before sending props.
The display counts omitted rows and labels shortened text; routing identifiers
are kept intact. If the fixed metadata itself is too large, the view shows a
bounded explanation instead of a partial, ambiguous browser. Files keeps the
complete read separately as its editor seed: shortening the preview neither
marks the read truncated nor disables Edit. These projections budget incoming
facts, not arbitrary editor or input drafts.

The shared `Editor` contract keeps canonical documents and editing decisions in
the guest. GPUI retains focus, IME, caret and native rendering; ordered editor
actions preserve input arriving while a guest decision is pending. Native file
pickers, drops and clipboard reads issue revocable file tokens scoped to one
guest. Guests read bounded chunks and choose the module operations that upload
or submit them. Replacing a guest revokes its device grants.
The views workspace and desktop crate pin the same `ducktape-ui` revision for
their shared wire vocabulary.

## Release build (macOS: signed and notarized)

`make app` builds `Ducktape.app` and `Ducktape-<version>-<arch>.dmg` under
`target/app-bundle/` and signs both **ad-hoc**, which runs on the machine that
built it and nowhere else — Gatekeeper refuses an ad-hoc bundle that arrived
over the network. A bundle that leaves this Mac is signed with a Developer ID
identity and notarized by Apple. `ops/bundle-app-macos.sh` does both itself, off four
environment variables; `make app` inherits the environment, so exporting them
is the whole configuration.

The bundle carries two executables in `Contents/MacOS`: `ducktape-launcher`,
its `CFBundleExecutable` (`app/packaging/Info.plist`), reads the update state
and `exec`s `ducktape-app` beside it — same PID, same bundle, so notifications,
TCC grants and `duck://` events all belong to `dev.ducktape.app`. The views
sit under `Contents/Resources/views` with a `MacOS/views` link, where
`views_dir()` finds them beside the executable. The helper is nested code and
is signed first, then the bundle, so `codesign --verify --deep --strict` — what
`ducktape-launcher --qualify` runs on a staged release — passes on the bundle
as built. `make install-app` hands that bundle to `ducktape-launcher install
--from target/app-bundle/Ducktape.app` unchanged: nothing is copied in, nothing
is re-sealed.

1. **The signing identity.** A "Developer ID Application" certificate from the
   Apple Developer Program, in the login keychain. The exact string is what
   `DUCKTAPE_CODESIGN_IDENTITY` takes:

   ```sh
   security find-identity -v -p codesigning   # "Developer ID Application: … (TEAMID)"
   ```

   With it set, the `.app` and the `.dmg` are signed with `--timestamp
   --options runtime` — the hardened runtime and a trusted timestamp are both
   preconditions for notarization.

2. **The notary key.** An App Store Connect API key: **App Store Connect →
   Users and Access → Integrations → Team Keys**, created with the *Developer*
   role. The `.p8` downloads **once**. Store it outside this repo (a repo copy
   is a leaked signing credential), and keep the Key ID and the Issuer UUID the
   same page shows.

3. **Build.**

   ```sh
   export DUCKTAPE_CODESIGN_IDENTITY="Developer ID Application: Example (TEAMID)"
   export DUCKTAPE_NOTARY_KEY="$HOME/.appstoreconnect/AuthKey_XXXXXXXXXX.p8"
   export DUCKTAPE_NOTARY_KEY_ID=XXXXXXXXXX
   export DUCKTAPE_NOTARY_ISSUER=00000000-0000-0000-0000-000000000000
   make app-release          # refuses if DUCKTAPE_CODESIGN_IDENTITY is unset
   ```

   The three `DUCKTAPE_NOTARY_*` go together: all three set adds `xcrun notarytool
   submit --wait` on the DMG followed by `xcrun stapler staple`, so the ticket
   travels inside the image and a first launch with no network still passes.
   Set without `DUCKTAPE_CODESIGN_IDENTITY`, the packaging script refuses before the upload
   rather than after Apple's wait. Set none and the build prints the identity
   it used and says what to export to notarize.

4. **Verify** — on the built artifacts, before shipping them:

   ```sh
   spctl -a -vv target/app-bundle/Ducktape.app      # accepted, source=Notarized Developer ID
   xcrun stapler validate target/app-bundle/Ducktape-*.dmg
   codesign -dv --verbose=4 target/app-bundle/Ducktape.app   # Authority + TeamIdentifier
   ```

5. **Package and publish.** `make release-app` is `app-release` with all
   three `DUCKTAPE_NOTARY_*` required (every release a network offers is
   notarized), the ticket stapled to the bundle, and `ops/release/archive.sh`
   packing it into `target/release-archive/Ducktape-<sha7>-macos-<arch>.tar.zst`
   — the name `app_update::layout::archive_name` gives the archive's own
   sha256. The script refuses by name an ad-hoc bundle (`adhoc_bundle_refused`)
   or one Apple never notarized (`bundle_not_stapled`). On Linux the same
   target packs `target/app-release/{ducktape-launcher, ducktape-app, views/}`
   into `Ducktape-<sha7>-linux-<arch>.tar.zst`. Then `make publish-app` with
   `NODE`, `RELEASE_KEY`, `SEQUENCE` and `DISPLAY` composes and signs the
   manifest and lands everything under `/shared/releases` on the network's
   duckfs (`ops/release/publish.sh`); its `ARCHIVES` defaults to what
   `release-app` just wrote.

`ops/macos-preflight.sh` reports both halves — the Developer ID identities in
the keychain and whether the three notary variables are exported with the key
file present — as an informational section; it never fails on them, because a
local build needs neither.

### Signing through the airlock gateway

The other signing path keeps the Developer ID off this Mac entirely: an
airlock gateway (`bin/airlock-gateway`, in a confidential VM) holds the
certificate, the PKCS#12 password and the App Store Connect key as an
`apple-codesign` credential, and `POST /sign/macos-bundle` signs, notarizes
and staples a bundle inside the enclave. `make release-app` takes it with
`DUCKTAPE_SIGN_VIA=airlock`; there is exactly ONE path per environment, and
`DUCKTAPE_SIGN_VIA=airlock` set together with `DUCKTAPE_CODESIGN_IDENTITY` or
any `DUCKTAPE_NOTARY_*` is refused as `sign_path_conflict` before anything
builds (the Makefile and `ops/bundle-app-macos.sh` both check).

1. **Enrol the identity** into the gateway, once, from any machine that holds
   the material. Only the TEE gateway mounts the signing route (the
   self-host lender daemon `ducktape service run airlock` holds an
   `apple-codesign` credential from `cred add apple-codesign` but serves no
   `/sign/macos-bundle`), so the verb is `ducktape user cred seal --vendor
   apple-codesign`: it verifies the enclave's quote against the pinned
   measurement (refusing `attestation_unverified` otherwise), seals the
   identity in, then submits the credential record under the attested seal
   key and publishes the account's `airlock` and `airlock-sign` routes, both
   served by the node it dials:

   ```sh
   # on the node fronting the enclave: both labels onto its loopback port
   ducktape gateway bind --workspace <node-workspace> --label airlock --port 9100 --account <account>
   ducktape gateway bind --workspace <node-workspace> --label airlock-sign --port 9100 --account <account>
   # enrol: quote verified, identity sealed, record + routes committed
   ducktape user cred seal --host http://127.0.0.1:9100 --attest snp --snp-product genoa \
     --measurement <audited image measurement> --vendor apple-codesign --name release-sign \
     --p12 DeveloperID.p12 --p12-password-file p12.password \
     --api-key AuthKey.json --team-id TEAMID -n <chain-id>
   # lend it to the account that releases (the release wallet's account)
   ducktape user cred grant release-sign <account> -n <chain-id>
   ```

   (`--remote airlock.<handle>.duck` in place of `--host` reaches an enclave
   already published, through this node's browser gateway.)

2. **Build, sign, pack.** The bundle script stages `Ducktape.app` unsigned
   (no `codesign`, no DMG), `ducktape release sign-bundle` sends it and
   unpacks the signed bundle back in place, and `ops/release/archive.sh`
   runs unchanged as the host-side verifier (`codesign --verify --deep
   --strict`, `stapler validate`, `spctl -a -t exec`) before it packs:

   ```sh
   DUCKTAPE_SIGN_VIA=airlock DUCKTAPE_SIGN_CREDENTIAL=release-sign NODE=http://127.0.0.1:8844 make release-app
   ```

   The verb resolves `release-sign` from committed state on `NODE` the way a
   provider run resolves a lent credential — the credential record names its
   owner, the owner's handle names the `airlock-sign.<handle>.duck` route
   (the enclave's signing lane, whose signed policy admits a 256 MiB bundle
   where the `airlock` model lane admits 16 MiB), the record's `seal_pk` is
   the pinned trust anchor — opens a sealed session on it through the node's
   browser gateway and streams the `.tar.zst` up and the signed one down.
   The enclave commits its reply head before it signs and keeps the stream
   live through Apple's notary wait, so no proxy deadline between the two
   is reached; the verb's own ceiling is 30 minutes. What comes back is
   checked to be the same bundle
   (identifier, version keys, executables, views) before it replaces the
   staged one; the archive the enclave returned is kept beside it as
   `target/app-bundle/Ducktape-signed.tar.zst`. The gateway's refusals come
   back by name (`bundle_shape_refused`, `bundle_too_large`,
   `codesign_failed`, `notary_rejected`, `staple_failed`, `tool_missing` —
   the last five ride the sealed stream's `Final` marker, since the head is
   already out); the verb's own are `credential_kind_mismatch` (the name is
   a model credential), `bundle_exceeds_route_cap` (the archive is over the
   `airlock-sign` route's published `max_request_bytes`) and
   `bundle_shape_changed`. Its progress is logged under
   `ducktape::gateway` (`release_sign_resolved`, `release_sign_submitted`,
   `release_sign_received`, `release_sign_refused`).

3. **Verify and publish** exactly as above (step 4, then `make publish-app`).

The microVM shim signs the same way: `bin/duck-vz-shim/build.sh` takes
`CODESIGN_IDENTITY` (ad-hoc `-` by default) and passes the
`com.apple.security.virtualization` entitlement whichever identity it is given
— Virtualization.framework refuses the binary without that entitlement.

## Visual language

One palette drives the whole app. `crates/views/support/design` holds it as
`design::LIGHT` / `design::DARK` — cool neutral greys, an ink sidebar in both
modes, one indigo accent: the native shell registers the two as gpui-kit
themes (`design::kit_theme_json`) and every WASM view reads the same values
through `ducktape_view_guest::kit::palette()`, switched by the `dark` fact the
kernel pushes with each view's session props. The accent is spent on the live
dot, the focus ring, the chosen row and the selection wash, so state colors
(success, warning, danger, agent) read as state and nothing else.

The console is a 200px ink rail carrying the network (name, live dot, block
height, the switch to another network) and the left-aligned navigation, a
40px header naming the screen with search and notifications, and the content,
which runs to the edges: a split view (chat, pages) butts its panes against
the rail, a reading (node, settings) insets itself. WASM views own their layout
and interaction routes and compose it from the guest kit's semantic pieces —
`page`, `card`, `pane`, `list_row`, `kv`, `badge`, `avatar`, `notice`,
`empty_state`, `tabs`, `field` — so a list, a record rail, a reading and a form
look the same on every screen. The native renderer maps the wire's button
presets onto gpui-kit variants (primary, outline, ghost, link, danger).

## Design system

- Faces: **Geist** (UI), **Geist Mono** (identifiers, hashes, paths, code).
  The files are embedded from `crates/views/support/design/assets/fonts/` at
  build time; the kit's component icons come from `gpui_kit::assets::Assets`.
- Type scale (`design::type_scale`): title 16, section 13.5, body 13,
  secondary 12, caption 11, mono 12.
- Radii (`design::radius`): 4px controls and badges, 6px cards, round avatars.
- Console frame: 1280×800 default (1040×540 minimum); launch window 480×680.
