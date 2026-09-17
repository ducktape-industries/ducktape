# ducktape build + install entry points.
#
# `make node` / `make coordinator` build the runnable product surfaces (the
# networked node daemon and the untrusted UDP coordinator). `make install-node`
# installs the `ducktape` operator CLI beside its founding set. `make test`
# is the full local verification gate — run it before every push.
#
# The desktop app, its launcher and the views it mounts live in
# ducktape-industries/ducktape-app; their build and install targets went
# with them.

CARGO ?= cargo
# every build/test recipe resolves against the COMMITTED lock: a guest's wasm
# bytes reach the descriptor's module table, so a silent re-resolution between
# two operators founds a DIFFERENT network from the same source — the genesis
# fingerprint covers every `id=code_hash` line, and a member built against the
# other resolution cannot handshake.
LOCKED ?= --locked
BIN_DEST ?= $(HOME)/.cargo/bin
UNAME_S := $(shell uname -s)

.PHONY: all airlock-gateway-image rcodesign dev dev-clear demo-seed demo-app demo-clear dogfood-forge node coordinator coordinator-smoke install-node install-coordinator test clean wasm-embed-check labs-gate audit

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

## the dev loop: found the "demo" localnet anew (stopping and clearing what a
## previous lap left) from the modules and index guests this build staged,
## start its node and the local compute/agent/airlock services, and sync
## ducktape's own repo into that node's forge (dogfood-forge — non-fatal when
## origin is unreachable).
## `make dev-clear` stops that background runtime without deleting its state,
## while `make demo-clear` removes the workspace entirely. `make dev YES=1`
## installs every agent CLI the checklist would offer without asking.
dev:
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
## every push): the no-embedded-wasm lint, the rust workspace including the
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

test: wasm-embed-check
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
	  PYTHONDONTWRITEBYTECODE=1 python3 ops/dogfood-forge-test.py && \
	  PYTHONDONTWRITEBYTECODE=1 python3 ops/refound-smoke-test.py; \
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
# the bins ride this line for a reason: `cargo test` builds the TEST target,
# which links dev-dependencies, so a product path reaching for a dev-only
# crate compiles under every test lane and breaks only the BINARY.
# That is not hypothetical — it shipped and sat on dev for 81 commits.
	$(CARGO) build $(LOCKED) -p noded-bin -p simnode

# The wasm guest builder, the module SDK, and the wasm-modules /
# wasm-modules-check / wasm-repro-check / wasm-rebuild-check /
# wasm-rebuild-refresh targets that drove them all ship in ducktape-sdk now;
# no replacement machinery lives here.

## the binary embeds no wasm (AGENTS.md, "No Embedded Wasm"): an
## include_bytes!/include_str! of a `.wasm` is allowed only in a test — a file
## under a `tests/` directory or named `tests.rs`, or an item a `#[cfg(test)]`
## governs. A source-parsing lint like `sdk_shaped` and `tracing_plane_lint`:
## it parses every `.rs` in the tree with `syn`, so which items `#[cfg(test)]`
## governs and whether a `.wasm` is an argument or text inside a literal are
## answered by the parser rather than guessed. Its own fixtures run beside it.
wasm-embed-check:
	$(CARGO) test $(LOCKED) -p topology --test wasm_embed

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
