# ducktape build + install entry points.
#
# `make node` / `make coordinator` build the runnable product surfaces (the
# networked node daemon and the untrusted UDP coordinator). `make install`
# installs the `ducktape` operator CLI and the desktop app — Ducktape.app on
# macOS, the binary plus its `x-scheme-handler/duck` desktop entry on Linux.
# `make test`
# is the full local verification gate — run it before every push.

CARGO ?= cargo
# every build/test recipe resolves against the COMMITTED lock: a guest's wasm
# bytes reach the descriptor's module table, so a silent re-resolution between
# two operators founds a DIFFERENT network from the same source — the genesis
# fingerprint covers every `id=code_hash` line, and a member built against the
# other resolution cannot handshake.
LOCKED ?= --locked
BIN_DEST ?= $(HOME)/.cargo/bin
UNAME_S := $(shell uname -s)

.PHONY: all app app-release release-app publish-app airlock-gateway-image rcodesign views views-repro-check dev dev-clear demo-seed demo-app demo-clear dogfood-forge node coordinator coordinator-smoke install install-app install-node install-coordinator test clean wasm-modules wasm-modules-check wasm-embed-check wasm-repro-check wasm-rebuild-check wasm-rebuild-refresh labs-gate audit

## the system packages a build needs and cargo cannot install: rustup (the
## pinned toolchain and its wasm32 target install themselves through it), a C
## compiler, and on Linux pkg-config, libclang (bindgen, for the app's camera
## bindings) and ALSA's headers (the app's audio). macOS builds with the
## command line tools alone. Checked up front, so a fresh machine hears the one install line
## instead of a linker error twenty minutes into the build.
.PHONY: prereqs
prereqs:
	@missing=""; \
	command -v rustup >/dev/null || missing="$$missing rustup"; \
	command -v cc >/dev/null || missing="$$missing cc"; \
	if [ "$(UNAME_S)" = Linux ]; then \
	  command -v pkg-config >/dev/null || missing="$$missing pkg-config"; \
	  { [ -n "$$LIBCLANG_PATH" ] || $$(command -v ldconfig || echo /sbin/ldconfig) -p 2>/dev/null | grep -q libclang; } || missing="$$missing libclang"; \
	  pkg-config --exists alsa 2>/dev/null || missing="$$missing alsa"; \
	  for library in x11-xcb xkbcommon xkbcommon-x11 fontconfig freetype2; do \
	    pkg-config --exists "$$library" 2>/dev/null || missing="$$missing $$library"; \
	  done; \
	fi; \
	[ -z "$$missing" ] || { \
	  echo "missing build prerequisites:$$missing" >&2; \
	  if [ "$(UNAME_S)" = Darwin ]; then \
	    echo "  xcode-select --install" >&2; \
	  else \
	    echo "  sudo apt install build-essential pkg-config libclang-dev libasound2-dev libx11-xcb-dev libxkbcommon-dev libxkbcommon-x11-dev libfontconfig1-dev libfreetype6-dev   # Debian/Ubuntu" >&2; \
	    echo "  On other distributions, install development packages for the missing libraries above." >&2; \
	  fi; \
	  echo "  rustup: https://rustup.rs" >&2; \
	  exit 1; }

## build every workspace crate (the default target)
all: prereqs
	$(CARGO) build $(LOCKED) --workspace

## the app dev loop: found the "demo" localnet anew (stopping and clearing
## what a previous lap left) from the modules, index guests and views this
## build staged, start its node and the local compute/agent/airlock services,
## sync ducktape's own repo into that node's forge (dogfood-forge — non-fatal
## when origin is unreachable), then run the desktop app against it in the
## foreground. Ctrl-C quits the app and leaves the node and services up for
## `cargo run -p ducktape-app`; the next `make dev` replaces them.
## `make dev-clear` stops that background runtime without deleting its state,
## while `make demo-clear` removes the workspace entirely. `make dev YES=1`
## installs every agent CLI the checklist would offer without asking.
dev: views
	@bash ops/dev.sh

## stop the demo node and compute/agent/airlock services left by `make dev`.
## Preserves the workspace: its module state, wallets, guest, executors and
## credentials. The foreground app and `make demo-app` are not killed.
dev-clear:
	@bash ops/dev-clear.sh

## seed a local "demo" network preloaded with sample data — chat channels +
## messages, a tasks board, pages, the registered ChiefDuck agent (the
## network's maintainer on the `claude` capability: the full action grant,
## forge read+push on `ducktape` and on a seeded `playground` repo with an
## issue mentioning it, its persona as an always-loaded skill — once `make
## dev` installs the claude CLI and starts the compute service it replies in
## chat and opens a pull request from a microVM), jobs, an automation rule —
## plus TWO gateway web-app routes: a
## NETWORK-hosted static site (DuckFS) and a USER-hosted loopback app. Stops and
## replaces any previous "demo" workspace under ~/.ducktape (demo-clear), and
## builds the workspace's own guest images and shell executor. Builds ducktape
## if needed (or set DUCKTAPE_NODE_BIN). See ops/demo-seed.sh.
demo-seed:
	@bash ops/demo-seed.sh

## serve the user-hosted web app behind the demo's app.<id>.duck gateway route
## (demo-seed publishes the route; this runs the loopback server it proxies to).
## Foreground — Ctrl-C to stop. See ops/demo-app.sh.
demo-app:
	@bash ops/demo-app.sh

## remove the seeded "demo" workspace: stop its node (cmdline-verified pid
## sweep, graceful /v1/shutdown first) and delete ~/.ducktape/demo — the whole
## network; other workspaces untouched. See ops/demo-clear.sh.
demo-clear:
	@bash ops/demo-clear.sh

## dogfood: host ducktape's own source in the local dev node's forge module.
## registers a static `ducktape-dev` git remote at the node's forge endpoint and
## synchronizes canonical `origin/dev` into Forge `dev` without moving
## release-only `main`, then verifies the exact ref (needs a running dev node).
## see ops/dogfood-forge.sh.
dogfood-forge:
	@bash ops/dogfood-forge.sh

## build-check the quarantined labs crate. It is EXCLUDED from the workspace
## (its own Cargo.lock) so its revm/alloy dep tree never taxes workspace gates;
## this target is how CI/devs still keep it compiling.
labs-gate:
	$(CARGO) check $(LOCKED) --manifest-path crates/labs/Cargo.toml

## release build of the networked node (the app-facing daemon surface)
node: prereqs
	$(CARGO) build $(LOCKED) --release -p node-bin

## stage the airlock enclave image root under target/airlock-gateway-image:
## the release `airlock-gateway`, the pinned `rcodesign` it signs release
## bundles with, and the entitlements it applies (ops/airlock-gateway/).
airlock-gateway-image:
	ops/airlock-gateway/stage-image.sh

## the pinned `rcodesign` into $(BIN_DEST): what `cargo test -p airlock` signs
## a fixture bundle with (the gateway's own `POST /sign/macos-bundle` path),
## installed from the same pinned release the image carries.
rcodesign:
	ops/airlock-gateway/install-rcodesign.sh --prefix "$(patsubst %/,%,$(dir $(BIN_DEST)))"

## release build of the untrusted UDP coordinator
coordinator:
	$(CARGO) build $(LOCKED) --release -p coordinator-bin

## coordinator-only verification gate: CLI/policy tests + live UDP smoke
coordinator-smoke:
	$(CARGO) test $(LOCKED) -p coordinator-bin

# Pin wasm-tools to the component encoder used by guest-builder.
WASM_TOOLS_VERSION = 1.$(shell sed -n 's/^wit-component = "=0\.\([0-9.]*\)".*/\1/p' bin/guest-builder/Cargo.toml | head -n1)
WASM_TOOLS_ROOT = $(CURDIR)/target/wasm-tools/$(WASM_TOOLS_VERSION)
WASM_TOOLS_BIN = $(WASM_TOOLS_ROOT)/bin/wasm-tools

$(WASM_TOOLS_BIN):
	CARGO_TARGET_DIR="$(WASM_TOOLS_ROOT)/build" $(CARGO) install wasm-tools \
		--version "$(WASM_TOOLS_VERSION)" --locked --root "$(WASM_TOOLS_ROOT)"

## Compile Rust-authored views and stage dynamically loaded WASM components.
VIEW_PACKAGES = $(shell awk '/^\[/{ in_package = ($$0 == "[package]") } in_package && /^name *= *"/ { split($$0, part, "\""); printf "-p %s ", part[2] }' crates/views/*/Cargo.toml)

views: $(WASM_TOOLS_BIN)
	PATH="$(WASM_TOOLS_ROOT)/bin:$$PATH" CARGO="$(CARGO)" bash ops/build-views.sh $(VIEW_PACKAGES)

## Rebuild committed view sources in two isolated roots and compare bytes.
views-repro-check: $(WASM_TOOLS_BIN)
	bash ops/views-repro-check.sh "$(WASM_TOOLS_ROOT)"

ifeq ($(UNAME_S),Darwin)
## Build native Ducktape.app and DMG using Apple's packaging tools.
## DUCKTAPE_CODESIGN_IDENTITY selects Developer ID; default is ad-hoc.
## DUCKTAPE_NOTARY_KEY, DUCKTAPE_NOTARY_KEY_ID, DUCKTAPE_NOTARY_ISSUER
## together submit and staple the signed image. See app/README.md.
app: prereqs views
	CARGO="$(CARGO)" bash ops/bundle-app-macos.sh

app-release:
	@test -n "$$DUCKTAPE_CODESIGN_IDENTITY" && test "$$DUCKTAPE_CODESIGN_IDENTITY" != "-" || \
	  { echo "app-release requires DUCKTAPE_CODESIGN_IDENTITY (Developer ID Application)" >&2; exit 1; }
	@$(MAKE) app

## the archive a release offers: a Developer ID-signed, notarized bundle
## (every offered release is), the ticket stapled to it, packed by
## ops/release/archive.sh into
## target/release-archive/Ducktape-<sha7>-macos-<arch>.tar.zst — the script
## refuses an ad-hoc or unnotarized bundle by name, so it is the host-side
## verifier on both paths below. Publish it with `make publish-app`.
##
## ONE signing path per environment, chosen by DUCKTAPE_SIGN_VIA:
##   unset    the local Developer ID: `app-release` with the three
##            DUCKTAPE_NOTARY_* required (not optional as for `app`).
##   airlock  the airlock gateway signs: `app` stages Ducktape.app UNSIGNED,
##            `ducktape release sign-bundle` sends it to the enclave holding
##            the apple-codesign credential DUCKTAPE_SIGN_CREDENTIAL through
##            NODE (the local node's http base) and unpacks the signed,
##            notarized, stapled bundle back in place. Set together with
##            DUCKTAPE_CODESIGN_IDENTITY or any DUCKTAPE_NOTARY_* it refuses
##            (`sign_path_conflict`): there is no fallback from one to the other.
##     DUCKTAPE_SIGN_VIA=airlock DUCKTAPE_SIGN_CREDENTIAL=release-sign NODE=http://127.0.0.1:8844 make release-app
ifeq ($(DUCKTAPE_SIGN_VIA),airlock)
release-app:
	@test -z "$$DUCKTAPE_CODESIGN_IDENTITY" -a -z "$$DUCKTAPE_NOTARY_KEY" -a -z "$$DUCKTAPE_NOTARY_KEY_ID" -a -z "$$DUCKTAPE_NOTARY_ISSUER" || \
	  { echo "release-app: sign_path_conflict: DUCKTAPE_SIGN_VIA=airlock with DUCKTAPE_CODESIGN_IDENTITY or DUCKTAPE_NOTARY_* set; one signing path per environment" >&2; exit 2; }
	@test -n "$(DUCKTAPE_SIGN_CREDENTIAL)" -a -n "$(NODE)" || \
	  { echo "release-app: DUCKTAPE_SIGN_VIA=airlock needs DUCKTAPE_SIGN_CREDENTIAL=<apple-codesign credential name> and NODE=<the local node's http base>" >&2; exit 2; }
	@DUCKTAPE_SIGN_VIA=airlock $(MAKE) app
	"$${DUCKTAPE_BIN:-$(CARGO_BIN)/ducktape}" release sign-bundle target/app-bundle/Ducktape.app \
	  --credential "$(DUCKTAPE_SIGN_CREDENTIAL)" --node "$(NODE)" \
	  --out target/app-bundle/Ducktape-signed.tar.zst --unpack-into target/app-bundle
	bash ops/release/archive.sh --from target/app-bundle/Ducktape.app --out-dir "$(RELEASE_ARCHIVE_DIR)"
else ifeq ($(DUCKTAPE_SIGN_VIA),)
release-app:
	@test -n "$$DUCKTAPE_NOTARY_KEY" -a -n "$$DUCKTAPE_NOTARY_KEY_ID" -a -n "$$DUCKTAPE_NOTARY_ISSUER" || \
	  { echo "release-app requires DUCKTAPE_NOTARY_KEY, DUCKTAPE_NOTARY_KEY_ID and DUCKTAPE_NOTARY_ISSUER: a release is notarized" >&2; exit 1; }
	@$(MAKE) app-release
	bash ops/release/archive.sh --from target/app-bundle/Ducktape.app --out-dir "$(RELEASE_ARCHIVE_DIR)"
else
release-app:
	@echo "release-app: DUCKTAPE_SIGN_VIA=$(DUCKTAPE_SIGN_VIA) is not a signing path (unset = local Developer ID, airlock = the airlock gateway)" >&2; exit 2
endif

## install the operator CLI and desktop app without requiring root
install: install-node install-app

## put the built bundle under the launcher: `ducktape-launcher install` seeds
## it as a release, swaps it into /Applications (DUCKTAPE_INSTALL_DIR
## overrides; the directory must be writable by this user) and writes the
## update state the app and the launcher share. The bundle ships its own
## launcher (Contents/MacOS/ducktape-launcher is its CFBundleExecutable),
## signed with the rest of it, so nothing is added or re-sealed here.
install-app: app
	target/release/ducktape-launcher install --from target/app-bundle/Ducktape.app
else
## where the Linux desktop entry and its icon land — the XDG per-user roots,
## so `make install-app` needs no root.
DESKTOP_DEST ?= $(if $(XDG_DATA_HOME),$(XDG_DATA_HOME),$(HOME)/.local/share)/applications
ICON_DEST ?= $(if $(XDG_DATA_HOME),$(XDG_DATA_HOME),$(HOME)/.local/share)/icons/hicolor/scalable/apps

## install the ducktape operator CLI and the desktop app without requiring root
install: install-node install-app

## build the desktop app binary, its launcher, and the views it loads, and
## stage them whole as one release under target/app-release:
## `{ducktape-launcher, ducktape-app, views/*.wasm}` — what `ducktape-launcher
## install --from` takes and what ops/release/archive.sh packs.
app: prereqs views
	$(CARGO) build $(LOCKED) --release -p ducktape-app -p app-launcher
	rm -rf target/app-release
	mkdir -p target/app-release/views
	install -m 0755 target/release/ducktape-app target/release/ducktape-launcher target/app-release/
	install -m 0644 target/views/*.wasm target/app-release/views/

## the archive a release offers: the staged release packed by
## ops/release/archive.sh into
## target/release-archive/Ducktape-<sha7>-linux-<arch>.tar.zst. Publish it
## with `make publish-app`.
release-app: app
	bash ops/release/archive.sh --from target/app-release --out-dir "$(RELEASE_ARCHIVE_DIR)"

## install the desktop app under its launcher and REGISTER THE duck:// SCHEME
## with the desktop. The release is staged whole (`ducktape-launcher`,
## `ducktape-app`, `views/`) and `ducktape-launcher install` seeds it as
## `$XDG_DATA_HOME/ducktape/releases/<sha>`, points `current` at it, writes
## the update state, and installs the `.desktop` entry with `Exec=` at
## `current/ducktape-launcher %u` — an absolute path, since a desktop
## session inherits no shell PATH, and the launcher, since that is what
## flips to a staged update and passes the URL on to the app. The entry's
## `MimeType=x-scheme-handler/duck` is what makes
## `xdg-open 'duck://forge/ducktape/1?net=<digest>'` reach the app; its
## `%u` puts the URL in argv where the app reads it; its name is the app id
## the window reports, so the running window associates with it (icon,
## pinned-app identity).
install-app: app
	mkdir -p "$(ICON_DEST)"
	install -m 0644 app/assets/icon.svg "$(ICON_DEST)/ducktape.svg"
	target/release/ducktape-launcher install --from target/app-release
	-update-desktop-database "$(DESKTOP_DEST)"
endif

## where `release-app` leaves its archives and `archives.txt` (one
## `<os>-<arch>=<path>` line per platform; a rerun replaces its own line).
RELEASE_ARCHIVE_DIR ?= target/release-archive

## publish a built desktop-app release to a network's duckfs: compose +
## seal the manifest, sign it with the release wallet, `fs put` the archives,
## the manifest and its signature under /shared/releases. Runs
## ops/release/publish.sh; every flag is a variable. ARCHIVES defaults to
## what `make release-app` wrote to $(RELEASE_ARCHIVE_DIR)/archives.txt, so
## after a `release-app` on this machine the archive list may be left out;
## a release for several platforms names every archive explicitly:
##   make publish-app NODE=http://127.0.0.1:8844 RELEASE_KEY=~/.ducktape/release/keys/release.key \
##        SEQUENCE=18 DISPLAY="2026.09.2+9d71b254a" \
##        ARCHIVES="macos-aarch64=target/Ducktape-macos-aarch64.tar.zst linux-x86_64=target/Ducktape-linux-x86_64.tar.zst"
ARCHIVES ?= $(shell cat "$(RELEASE_ARCHIVE_DIR)/archives.txt" 2>/dev/null)
publish-app:
	@test -n "$(NODE)" -a -n "$(RELEASE_KEY)" -a -n "$(SEQUENCE)" -a -n "$(DISPLAY)" -a -n "$(ARCHIVES)" || \
	  { echo "publish-app needs NODE, RELEASE_KEY, SEQUENCE, DISPLAY and ARCHIVES (see the comment above)" >&2; exit 2; }
	DUCKTAPE_BIN="$${DUCKTAPE_BIN:-$(CARGO_BIN)/ducktape}" bash ops/release/publish.sh \
	  --node "$(NODE)" --key "$(RELEASE_KEY)" --sequence "$(SEQUENCE)" --display "$(DISPLAY)" \
	  $(if $(NOTES_URL),--notes-url "$(NOTES_URL)") \
	  $(foreach archive,$(ARCHIVES),--archive "$(archive)")

# where `cargo install` puts the binary, and so where the installed binary
# looks for its founding set: workspace_config::modules_dir() reads
# $DUCKTAPE_MODULES_DIR, else `modules/` beside the executable.
CARGO_BIN = $${CARGO_HOME:-$$HOME/.cargo}/bin

## the binary embeds no wasm: `node init` composes a network's genesis out of
## the founding set (`<id>.component.wasm`, `<id>.index.wasm`, the netstack
## guest) that noded's build script stages beside every build's binary, in the
## directory named for THIS checkout (target/<profile>/modules%<path>, see
## crates/workspace-config/src/staged_key.rs — the name is the checkout's path
## with `/` written `%`, which is why make can spell it with one `subst` and
## needs no second implementation). Installing the node copies that set beside
## the installed binary under the plain, unkeyed name an installed layout
## reads. `--target-dir target` keeps the install build in the checkout's
## target dir, which is where the staged set lands.
STAGED_MODULES = modules$(subst /,%,$(CURDIR))
STAGED_SIM_MODULES = sim-modules$(subst /,%,$(CURDIR))
install-node: prereqs
	$(CARGO) install --path bin/node --locked --target-dir target
	rm -rf "$(CARGO_BIN)/modules"
	cp -r "target/release/$(STAGED_MODULES)" "$(CARGO_BIN)/modules"
	rm -rf "$(CARGO_BIN)/sim-modules"
	cp -r "target/release/$(STAGED_SIM_MODULES)" "$(CARGO_BIN)/sim-modules"
	@echo "installed the founding set into $(CARGO_BIN)/modules"

## coordinator -> ~/.cargo/bin/ducktape-coordinator
install-coordinator:
	$(CARGO) build $(LOCKED) --release -p coordinator-bin
	mkdir -p "$(BIN_DEST)"
	install -m 755 target/release/coordinator "$(BIN_DEST)/ducktape-coordinator"

## the full LOCAL verification gate (no hosted CI by design — run this before
## every push): the wasm-artifact drift gate, the rust workspace including the
## process-level e2e suites (bin/node spawns a real 4-node cluster over localhost
## TCP, bin/noded drives a real spawned daemon over http/ws), the consensus
## sim-feature suite, and a build of the noded + simnode binaries the test
## harnesses stage.
# WHERE THE E2E SUITES PUT NODE STORAGE — pinned to disk, on purpose.
#
# Every spawned cluster writes `storage=$TMPDIR/.tmpXXXX/storage-N`, and a test
# that panics (or a node killed with it) leaves the whole tree behind. On a host
# where /tmp is tmpfs — the default on this dev box — those leaks are RAM. One
# session left 11 dirs totalling 22 GB, two of them 7.2 GB each, and since
# tmpfs has no swap the box lost that memory for good: each gate run started
# with less than the last, until rustc and ld began dying mid-compile. The runs
# measuring the box were degrading it.
#
# Pinning TMPDIR under target/ makes a leak cost disk instead of memory, and the
# rm -rf reclaims the previous run's leftovers before each pass. The leak itself
# is still a bug worth fixing in the harnesses — see #887.
TEST_TMPDIR := $(CURDIR)/target/test-tmp

test: wasm-modules-check wasm-embed-check
# The reclaim may not fail the gate: a first run has nothing to clean.
	-rm -rf "$(TEST_TMPDIR)" 2>/dev/null
	mkdir -p "$(TEST_TMPDIR)"
	TMPDIR="$(TEST_TMPDIR)" $(CARGO) test $(LOCKED) --workspace
# the auth page's pure helpers (fragment parsing, DER→raw, SPKI→SEC1) — the
# browser half of `crates/authpage`'s contract, dependency-free under node.
# Skips with a notice where there is no node, like the bun line below.
	@if command -v node >/dev/null; then node ops/auth-page/test.mjs; \
	else echo "[test] skipped ops/auth-page/test.mjs — node (nodejs) is not installed" >&2; fi
# the ops/ scripts' own tests. Every one is offline — a real temporary git
# repository, a mocked systemd socket, a recorded cluster inventory — and none
# reaches a node, a network or a Proxmox host. They run here because a test no
# target runs is a false guarantee, not a spare one: `worktree-clean.sh`'s
# refusal to remove a worktree that is dirty, unmerged or in use is precisely
# what its test covers, and a regression there destroys unmerged work.
	@if command -v node >/dev/null; then node ops/proxmox-view-observe-test.mjs; \
	else echo "[test] skipped ops/proxmox-view-observe-test.mjs — node (nodejs) is not installed" >&2; fi
# One guard for all three: `tomllib` is 3.11, which the lane test reads its
# fixtures with, so a box that fails this check cannot run any of them.
# PYTHONDONTWRITEBYTECODE because `__pycache__` beside a tracked script is
# untracked litter the gate would leave in everyone's `git status`.
	@if python3 -c 'import tomllib' >/dev/null 2>&1; then \
	  PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s ops/application-service && \
	  PYTHONDONTWRITEBYTECODE=1 python3 ops/proxmox-view-lane-test.py && \
	  PYTHONDONTWRITEBYTECODE=1 python3 ops/worktree-clean-test.py && \
	  PYTHONDONTWRITEBYTECODE=1 python3 ops/dogfood-forge-test.py; \
	else echo "[test] skipped the ops/ script tests — they need python 3.11 (tomllib)" >&2; fi
# demo-clear's refusal line against a stub admin surface (the reason token it
# prints has to be the node's own, not one invented in the script) and its
# process sweep (only the workspace's ducktape node and services, never a
# bystander naming the path). Needs `bun` (so does demo-clear itself); the
# script skips with a notice where there is none, like the podman lines above.
	bash ops/demo-clear-test.sh
# the #[ignore]d tests are ignored ONLY because they must not share a process
# with the parallel suite — they still have to run. `absolute_configs_resolve_
# after_launch_cwd_is_deleted` re-execs the test binary, and doing that under 32
# live libtest threads made unrelated tests fail ~4 runs in 11 with integrity
# errors. Serial + its own invocation is the isolation. See #887.
	TMPDIR="$(TEST_TMPDIR)" $(CARGO) test $(LOCKED) -p node-bin --bin ducktape -- --ignored --test-threads=1
	TMPDIR="$(TEST_TMPDIR)" $(CARGO) test $(LOCKED) -p consensus --features sim
# ducktape-app rides this line for a reason: `cargo test` builds the TEST
# target, which links dev-dependencies, so a product path reaching for a
# dev-only crate compiles under every test lane and breaks only the BINARY.
# That is not hypothetical — it shipped and sat on dev for 81 commits.
	$(CARGO) build $(LOCKED) -p noded-bin -p simnode -p ducktape-app

## rebuild every wasm guest into its artifact and refresh EVERY committed copy
## in one sweep (the canonical artifact in the module's own directory, which
## the build stages into the founding set, + the kernel test fixtures), so the
## copies can never drift apart. the componentizer is the `wit-component` crate
## guest-builder links (pinned in bin/guest-builder/Cargo.toml); the wasm32
## target uses rust-toolchain.toml. every guest is built ALONE, out of the
## platform repository at this checkout's HEAD (bin/guest-builder), so HEAD must
## be pushed first; each module's guest.lock records the revision and the
## registry versions its artifact came from and seeds its next build, so a
## crates.io publish never moves the bytes, and a revision that changes none of
## what a module compiles leaves its bytes as they are. component bytes ARE
## toolchain-dependent: a rebuild on a different rustc may legitimately differ
## from the committed bytes — move the channel and the whole set together.
## `wasm-modules-check` guards the copies' mutual consistency,
## `wasm-rebuild-check` every artifact against a rebuild of its source, and
## `wasm-repro-check` that nothing builder-local reaches the bytes.
#
# Every product/example module carries its own guest port (src/guest.rs behind
# the `guest` feature); guest-builder builds it out of the repository and
# writes the canonical component.wasm and guest.lock into the module directory
# itself. The five kernel-fixture test guests (hello, hello-replacement, noop,
# sibling, object) keep their standalone crates/guests workspaces below.
BUILDER_MODULES := \
  crates/examples/directory \
  crates/modules/apps/inbox crates/modules/apps/pages crates/modules/apps/agent \
  crates/modules/apps/automations crates/modules/apps/collaboration \
  crates/modules/apps/runs \
  crates/modules/apps/tasks crates/modules/apps/chat crates/modules/apps/files \
  crates/modules/apps/forge crates/modules/apps/boards \
  crates/modules/system/attribution crates/modules/system/dispatch \
  crates/modules/system/capability crates/modules/system/identity \
  crates/modules/system/gateway crates/modules/system/governance \
  crates/modules/system/saga crates/modules/system/acl crates/modules/system/kv \
  crates/modules/system/modules crates/modules/system/valset

# Modules that additionally ship an INDEX guest (src/index_guest.rs behind the
# `index-guest` feature): guest-builder --index writes the canonical
# index.wasm (core wasm, no componentize) into the module directory; the
# build stages it into the founding set as `<id>.index.wasm` and a genesis
# carries it. The shell file IS the declaration: noded's build script stages
# an index guest for exactly the module crates that carry src/index_guest.rs,
# so this list must name exactly those. The reference testmap mapper is the
# indexer crate's test fixture and rides the same sweep.
INDEX_MODULES := \
  crates/modules/apps/chat crates/modules/apps/tasks crates/modules/apps/pages \
  crates/modules/apps/inbox crates/modules/apps/runs crates/modules/system/saga \
  crates/kernel/index-guest/testmap

# The netstack guest: the reachability machine as a `ducktape:netstack`
# component (crates/networking/netstack-machine/src/guest.rs behind the same
# `guest` feature convention). Not a consensus module, so no kernel fixture
# copy and no place in a genesis: the build stages it into the founding set
# as `netstack.component.wasm`, bin/node reads it from there at boot, and
# the netstack-wasm scenario lane reads it from the crate directory.
NETSTACK_GUEST := crates/networking/netstack-machine

# guest-builder is built and run out of THIS CHECKOUT'S OWN target directory,
# never the one a host config shares between worktrees.
#
# The binary bakes its platform root in at compile time, so the one sitting in
# a shared target belongs to whichever worktree built it last. Run that one and
# it refuses every module you own — "<module> is outside the platform checkout
# <someone else's worktree>" — and it does so MID-SWEEP, because a sibling's
# build can land between two guests of yours. `touch bin/guest-builder/src/main.rs`
# only wins the race until the next session builds; a target directory of our
# own ends it. It is a sibling of `target/guest-builder/<id>/`, where the
# builder puts each module's ephemeral build tree, and never the same path.
#
# `ops/build-guest-rootfs.sh` and `ops/wasm-repro-check.sh` take the same
# precaution for the same reason.
GUEST_BUILDER_DIR := $(CURDIR)/target/guest-builder-bin
GUEST_BUILDER := $(CARGO) run -q $(LOCKED) --target-dir $(GUEST_BUILDER_DIR) -p guest-builder --
# the same command as a person would type it, for the advice a failing gate prints.
GUEST_BUILDER_SHOWN := $(CARGO) run --target-dir $(GUEST_BUILDER_DIR) -p guest-builder --

wasm-modules:
	@for m in $(BUILDER_MODULES); do \
	  id=$$(basename $$m) && \
	  $(GUEST_BUILDER) $$m && \
	  cp $$m/component.wasm \
	    crates/kernel/host/tests/fixtures/$$id.component.wasm || exit 1; \
	done
	@for m in $(INDEX_MODULES); do \
	  $(GUEST_BUILDER) --index $$m || exit 1; \
	done
	$(GUEST_BUILDER) $(NETSTACK_GUEST)
	# hello mirrors its component into BOTH fixture homes; sibling/object write
	# straight to the wasm-host fixture with no guest copy; hello-replacement
	# builds the replacement crate directly into the host fixture. Each shape is
	# unique — kept explicit. These five ARE STANDALONE workspaces (each owns a
	# committed lock, so $(LOCKED) applies same as everywhere else) — their
	# components are kernel test fixtures, nothing the genesis hash pins.
	cd crates/guests/hello-wasm && $(CARGO) build $(LOCKED) --target wasm32-unknown-unknown --release
	$(GUEST_BUILDER) componentize \
	  crates/guests/hello-wasm/target/wasm32-unknown-unknown/release/hello_wasm.wasm \
	  --out crates/guests/hello-wasm/component.wasm
	cp crates/guests/hello-wasm/component.wasm \
	  crates/kernel/wasm-host/tests/fixtures/hello.component.wasm
	cp crates/guests/hello-wasm/component.wasm \
	  crates/kernel/host/tests/fixtures/hello.component.wasm
	# noop: the smallest compliant module, the admission fixture that touches
	# nothing. Its component is committed beside the crate and pinned in the
	# host fixtures, the hello shape.
	cd crates/guests/noop-wasm && $(CARGO) build $(LOCKED) --target wasm32-unknown-unknown --release
	$(GUEST_BUILDER) componentize \
	  crates/guests/noop-wasm/target/wasm32-unknown-unknown/release/noop_wasm.wasm \
	  --out crates/guests/noop-wasm/component.wasm
	cp crates/guests/noop-wasm/component.wasm \
	  crates/kernel/host/tests/fixtures/noop.component.wasm
	cd crates/guests/hello-wasm-replacement && $(CARGO) build $(LOCKED) --target wasm32-unknown-unknown --release
	$(GUEST_BUILDER) componentize \
	  crates/guests/hello-wasm-replacement/target/wasm32-unknown-unknown/release/hello_wasm_replacement.wasm \
	  --out crates/kernel/host/tests/fixtures/hello-replacement.component.wasm
	cd crates/guests/sibling-wasm && $(CARGO) build $(LOCKED) --target wasm32-unknown-unknown --release
	$(GUEST_BUILDER) componentize \
	  crates/guests/sibling-wasm/target/wasm32-unknown-unknown/release/sibling_wasm.wasm \
	  --out crates/kernel/wasm-host/tests/fixtures/sibling.component.wasm
	cd crates/guests/object-wasm && $(CARGO) build $(LOCKED) --target wasm32-unknown-unknown --release
	$(GUEST_BUILDER) componentize \
	  crates/guests/object-wasm/target/wasm32-unknown-unknown/release/object_wasm.wasm \
	  --out crates/kernel/wasm-host/tests/fixtures/object.component.wasm
	cd crates/guests/object-wasm && $(CARGO) build $(LOCKED) --target wasm32-unknown-unknown --release --features replacement
	$(GUEST_BUILDER) componentize \
	  crates/guests/object-wasm/target/wasm32-unknown-unknown/release/object_wasm.wasm \
	  --out crates/kernel/wasm-host/tests/fixtures/object-replacement.component.wasm

## the drift gate for the committed component artifacts: every copy of the SAME
## module must be byte-identical (`node init` hashes the bundle into the
## descriptor; the kernel test fixtures pin the same bytes).
## toolchain-independent, so it rides the pre-push `test` gate; run
## `make wasm-modules` to refresh the set.
wasm-modules-check:
	cmp crates/guests/hello-wasm/component.wasm \
	  crates/kernel/wasm-host/tests/fixtures/hello.component.wasm
	cmp crates/guests/hello-wasm/component.wasm \
	  crates/kernel/host/tests/fixtures/hello.component.wasm
	cmp crates/guests/noop-wasm/component.wasm \
	  crates/kernel/host/tests/fixtures/noop.component.wasm
	@for m in $(BUILDER_MODULES); do \
	  id=$$(basename $$m) && \
	  cmp $$m/component.wasm \
	    crates/kernel/host/tests/fixtures/$$id.component.wasm || exit 1; \
	done
	@for m in $(INDEX_MODULES); do \
	  test -f $$m/index.wasm || { echo "missing $$m/index.wasm (make wasm-modules)"; exit 1; }; \
	done
	@test -f $(NETSTACK_GUEST)/component.wasm \
	  || { echo "missing $(NETSTACK_GUEST)/component.wasm (make wasm-modules)"; exit 1; }
# every built guest carries its lock: the record of the revision and the
# registry versions the artifact came from, without which a rebuild cannot
# reproduce it.
	@for m in $(BUILDER_MODULES) $(INDEX_MODULES) $(NETSTACK_GUEST); do \
	  test -f $$m/guest.lock || { echo "missing $$m/guest.lock (make wasm-modules)"; exit 1; }; \
	done
# and no committed artifact may carry a builder-local absolute path. guest-builder
# remaps the checkout, CARGO_HOME and RUSTUP_HOME prefixes to stable tokens (see
# `remap_flags`), so a `/home/...` or `/Users/...` in the bytes means an artifact
# built without that remap — the state where every box disagrees on every module.
	@leaks=$$(git ls-files -z '*.wasm' | xargs -0 grep -laE '/home/|/Users/' || true); \
	  test -z "$$leaks" || { \
	    echo "builder host paths embedded in: $$leaks"; \
	    echo "rebuild with make wasm-modules (guest-builder --remap-path-prefix)"; exit 1; }
	@echo "wasm module artifacts are mutually consistent"

## the binary embeds no wasm (AGENTS.md, "No Embedded Wasm"): an
## include_bytes!/include_str! of a `.wasm` is allowed only in a test — a file
## under a `tests/` directory or named `tests.rs`, or an item a `#[cfg(test)]`
## governs. A source-parsing lint like `sdk_shaped` and `tracing_plane_lint`:
## it parses every `.rs` in the tree with `syn`, so which items `#[cfg(test)]`
## governs and whether a `.wasm` is an argument or text inside a literal are
## answered by the parser rather than guessed. Its own fixtures run beside it.
wasm-embed-check:
	$(CARGO) test $(LOCKED) -p topology --test wasm_embed

## the reproducibility gate: one guest built twice, in two scratch directories,
## must be byte-identical and carry no host path. Needs the wasm32 target
## and a pushed HEAD (which `wasm-modules-check` deliberately does
## not), so it stands apart from the pre-push `test` gate. See
## ops/wasm-repro-check.sh.
wasm-repro-check:
	@bash ops/wasm-repro-check.sh

REBUILD_CHECK_DIR := $(CURDIR)/target/wasm-rebuild-check

## the drift gate between an artifact and its source: rebuild every guest out
## of the repository at HEAD, seeded from its committed guest.lock, and compare
## it with the committed bytes. `wasm-modules-check` only cmps committed copies
## against each other, so an artifact could drift arbitrarily far from the
## source beside it and stay green. A `--out` build leaves the module
## directory (lock included) untouched, so the tree stays clean under the
## check. Needs the wasm32 target and a pushed HEAD, so like
## `wasm-repro-check` it stands apart from the pre-push `test` gate.
# Every guest is rebuilt and compared before this reports, so ONE run names
# every stale artifact. Stopping at the first turned a sweep into a queue: each
# refresh had to be built, committed and re-run to learn whether another guest
# was hiding behind it, and a guest whose dependency moved is rarely alone.
# A build that cannot run at all still stops the target, because after it
# nothing downstream would be comparing anything.
#
# SCOPE IT TO WHAT YOU CHANGED. The full sweep is 31 guest builds; on a loaded
# box that is most of an hour, and a PR that moved one crate does not owe the
# other twenty-eight. `CRATES` names the crates the change touched and the run
# covers exactly the guests that compile them:
#
#     make wasm-rebuild-check CRATES="files duckfs-core"
#
# The guest list comes from each module's OWN `guest.lock`, which records what
# it actually compiled, so the scope cannot drift from the graph the way a
# hand-kept table would. One lock covers both members of a module that ships an
# index guest — the builder's shell workspace holds every guest the module
# declares, so the lock is their union. A crate no lock names is refused rather than quietly
# checking nothing — a typo that reports success is worse than no gate. Omit
# `CRATES` and every guest is swept, which is what a module-SDK or toolchain
# move owes.
wasm-rebuild-check:
	@mkdir -p "$(REBUILD_CHECK_DIR)"
	@stale=""; \
	compiles() { \
	  [ -z "$(CRATES)" ] && return 0; \
	  for c in $(CRATES); do \
	    grep -sqx "name = \"$$c\"" "$$1/guest.lock" && return 0; \
	  done; \
	  return 1; \
	}; \
	for c in $(CRATES); do \
	  grep -sqx "name = \"$$c\"" \
	    $(addsuffix /guest.lock,$(BUILDER_MODULES) $(NETSTACK_GUEST) $(INDEX_MODULES)) || { \
	      echo "wasm-rebuild-check: no guest.lock names the crate \"$$c\" — check the spelling."; \
	      echo "  A guest compiles what its lock records; nothing here compiles that."; \
	      exit 1; }; \
	done; \
	for m in $(BUILDER_MODULES) $(NETSTACK_GUEST); do \
	  compiles $$m || continue; \
	  id=$$(basename $$m) && \
	  $(GUEST_BUILDER) $$m \
	    --out "$(REBUILD_CHECK_DIR)/$$id.component.wasm" >/dev/null || exit 1; \
	  cmp -s $$m/component.wasm "$(REBUILD_CHECK_DIR)/$$id.component.wasm" || \
	    stale="$$stale $$m"; \
	done; \
	for m in $(INDEX_MODULES); do \
	  compiles $$m || continue; \
	  id=$$(basename $$m) && \
	  $(GUEST_BUILDER) --index $$m \
	    --out "$(REBUILD_CHECK_DIR)/$$id.index.wasm" >/dev/null || exit 1; \
	  cmp -s $$m/index.wasm "$(REBUILD_CHECK_DIR)/$$id.index.wasm" || \
	    stale="$$stale --index $$m"; \
	done; \
	if [ -z "$$stale" ]; then \
	  echo "committed guests match a rebuild of their source at HEAD"; exit 0; \
	fi; \
	echo "these committed guests do not match a rebuild of their source:"; \
	set -- $$stale; \
	while [ $$# -gt 0 ]; do \
	  if [ "$$1" = "--index" ]; then \
	    echo "    $(GUEST_BUILDER_SHOWN) --index $$2"; shift 2; \
	  else \
	    echo "    $(GUEST_BUILDER_SHOWN) $$1"; shift; \
	  fi; \
	done; \
	echo "  make wasm-rebuild-refresh CRATES=\"$(CRATES)\" promotes what this run already built."; \
	echo "  Then re-run this check, and commit the result with its kernel fixture copy."; \
	exit 1

## promote what `wasm-rebuild-check` just built, instead of building it again.
##
## The check rebuilds every guest in scope into `$(REBUILD_CHECK_DIR)` and then
## tells you to rebuild the stale ones — so shipping three refreshed guests cost
## six full builds, and a 4 MB component is minutes each. The builder now writes
## the shell lock beside every artifact it emits (`<out>.lock`), so the check's
## output directory already holds everything a refresh needs: the bytes, and the
## record of what produced them. This target copies both into place.
#
# It REFUSES a pair whose lock names a revision other than HEAD. The check
# directory is a build output, not a fact: it survives a rebase, a pull and a
# branch switch, and promoting it afterwards would commit bytes built from
# source that is no longer here — the one failure a guest artifact must never
# have, because nothing downstream would notice. The lock records the exact
# platform revision every guest compiled (`source = "git+…#<sha>"`), so the
# mismatch is a refusal by name rather than a silent wrong answer.
#
# `CRATES` scopes this exactly as it scopes the check, through the same
# `guest.lock` reading. That is not tidiness: the directory accumulates across
# runs and branches, so without scoping a `CRATES="chat"` refresh reaches every
# leftover in it — one with no lock aborts a run that had nothing to do with it,
# and one whose lock happens to name HEAD is PROMOTED, overwriting a committed
# artifact the run was never asked to touch and reporting success. A crate no
# lock names is refused here for the same reason it is in the check.
#
# Validation runs over the whole scope BEFORE anything is copied. A refusal
# midway through promotion would leave half the guests on new bytes and half on
# old — a tree state nothing downstream would notice either.
wasm-rebuild-refresh:
	@head=$$(git rev-parse HEAD); \
	ls "$(REBUILD_CHECK_DIR)"/*.wasm >/dev/null 2>&1 || { \
	  echo "wasm-rebuild-refresh: $(REBUILD_CHECK_DIR) holds no build to promote."; \
	  echo "  Run wasm-rebuild-check first; it builds what this promotes."; \
	  exit 1; }; \
	compiles() { \
	  [ -z "$(CRATES)" ] && return 0; \
	  for c in $(CRATES); do \
	    grep -sqx "name = \"$$c\"" "$$1/guest.lock" && return 0; \
	  done; \
	  return 1; \
	}; \
	for c in $(CRATES); do \
	  grep -sqx "name = \"$$c\"" \
	    $(addsuffix /guest.lock,$(BUILDER_MODULES) $(NETSTACK_GUEST) $(INDEX_MODULES)) || { \
	      echo "wasm-rebuild-refresh: no guest.lock names the crate \"$$c\" — check the spelling."; \
	      echo "  A guest compiles what its lock records; nothing here compiles that."; \
	      exit 1; }; \
	done; \
	scope=""; \
	for m in $(BUILDER_MODULES) $(NETSTACK_GUEST); do \
	  compiles $$m && scope="$$scope $$m:component"; \
	done; \
	for m in $(INDEX_MODULES); do \
	  compiles $$m && scope="$$scope $$m:index"; \
	done; \
	built_in_scope=""; \
	for entry in $$scope; do \
	  m=$${entry%:*}; kind=$${entry##*:}; id=$$(basename $$m); \
	  built="$(REBUILD_CHECK_DIR)/$$id.$$kind.wasm"; \
	  [ -f "$$built" ] || continue; \
	  built_in_scope="yes"; \
	  lock="$(REBUILD_CHECK_DIR)/$$id.$$kind.lock"; \
	  [ -f "$$lock" ] || { \
	    echo "wasm-rebuild-refresh: $$built has no $$id.$$kind.lock beside it."; \
	    echo "  The builder writes the lock with the bytes, so this predates that."; \
	    echo "  Re-run wasm-rebuild-check; promoting bytes with no provenance is the"; \
	    echo "  one thing this target must not do."; \
	    exit 1; }; \
	  grep -sq "#$$head\"" "$$lock" || { \
	    echo "wasm-rebuild-refresh: $$lock was built from another revision, not $$head."; \
	    echo "  That build predates your HEAD. Re-run wasm-rebuild-check."; \
	    exit 1; }; \
	done; \
	if [ -z "$$built_in_scope" ]; then \
	  echo "wasm-rebuild-refresh: $(REBUILD_CHECK_DIR) holds no build for CRATES=\"$(CRATES)\"."; \
	  echo "  It holds builds for other guests; this run promotes none of them."; \
	  echo "  Run wasm-rebuild-check with the same CRATES first."; \
	  exit 1; \
	fi; \
	promoted=""; \
	for entry in $$scope; do \
	  m=$${entry%:*}; kind=$${entry##*:}; id=$$(basename $$m); \
	  built="$(REBUILD_CHECK_DIR)/$$id.$$kind.wasm"; \
	  [ -f "$$built" ] || continue; \
	  artifact="$$m/$$kind.wasm"; \
	  cmp -s "$$built" "$$artifact" && continue; \
	  cp "$$built" "$$artifact"; \
	  cp "$(REBUILD_CHECK_DIR)/$$id.$$kind.lock" "$$m/guest.lock"; \
	  promoted="$$promoted $$artifact"; \
	done; \
	if [ -z "$$promoted" ]; then \
	  echo "nothing to promote: every built guest already matches its committed bytes"; exit 0; \
	fi; \
	echo "promoted:"; \
	for a in $$promoted; do echo "    $$a"; done; \
	echo "  Copy each component into crates/kernel/host/tests/fixtures/, then re-run wasm-rebuild-check."

## the supply-chain tripwire: RustSec advisories and yanked crates against the
## committed Cargo.lock, under `deny.toml` — where every carried advisory is
## listed WITH the reason it is carried and what would clear it. Needs
## `cargo deny` and network, so it is not part of the offline `test` gate; run
## it when the lock moves. `cargo audit` is not also run: same database, and a
## second ignore list in `.cargo/audit.toml` is a second place to go stale.
audit:
	$(CARGO) deny check advisories

clean:
	$(CARGO) clean
