# ducktape build + install entry points.
#
# `make node` builds the release `ducktape` binary and `make install-node`
# installs it. `make test` is the full local verification gate — run it before
# every push. The desktop app lives in ducktape-industries/ducktape-app;
# `make install` delegates to its pinned install target.

CARGO ?= cargo
# every build/test recipe resolves against the COMMITTED lock: two operators
# building from the same source must run the same bytes.
LOCKED ?= --locked
BIN_DEST ?= $(HOME)/.cargo/bin
UNAME_S := $(shell uname -s)

.PHONY: all airlock-gateway-image rcodesign node coordinator coordinator-smoke install install-node install-coordinator test clean audit kernel-fixtures system-programs

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

## release build of the node
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

## the `ducktape` binary into $(BIN_DEST). The binary embeds no wasm: a
## network's programs are files the founding file names, and a joiner receives
## them over state sync.
install-node: prereqs
	$(CARGO) build $(LOCKED) --release -p node-bin
	mkdir -p "$(BIN_DEST)"
	install -m 0755 target/release/ducktape "$(BIN_DEST)/"

## install the node and the desktop app at the immutable revision in
## ops/app/APP_REV. The app checkout and its destination are owned by the app's
## own make install contract; ops/app/install.sh reports that output.
install: install-node
	@$(MAKE) --no-print-directory install-app

.PHONY: install-app
install-app:
	@bash ops/app/install.sh

## coordinator -> ~/.cargo/bin/ducktape-coordinator
install-coordinator:
	$(CARGO) build $(LOCKED) --release -p coordinator-bin
	mkdir -p "$(BIN_DEST)"
	install -m 755 target/release/coordinator "$(BIN_DEST)/ducktape-coordinator"

## the full LOCAL verification gate: the rust workspace (the daemon suites in
## crates/noded spawn real nodes over localhost TCP; the consensus suites run
## a simulated network), the ops/ script tests, and a build of the `ducktape`
## binary.
# WHERE THE SUITES PUT NODE STORAGE — pinned to disk, on purpose. On a host
# where /tmp is tmpfs a leaked storage directory is RAM; under target/ it is
# disk, and the rm -rf reclaims the previous run's leftovers.
TEST_TMPDIR := $(CURDIR)/target/test-tmp

test:
# The reclaim may not fail the gate: a first run has nothing to clean.
	-rm -rf "$(TEST_TMPDIR)" 2>/dev/null
	mkdir -p "$(TEST_TMPDIR)"
	TMPDIR="$(TEST_TMPDIR)" $(CARGO) test $(LOCKED) --workspace
# the ops/ scripts' own tests: offline, against a real temporary git
# repository. `worktree-clean.sh`'s refusal to remove a worktree that is
# dirty, unmerged or in use is precisely what its test covers.
# PYTHONDONTWRITEBYTECODE because `__pycache__` beside a tracked script is
# untracked litter the gate would leave in everyone's `git status`.
	@if python3 -c 'import tomllib' >/dev/null 2>&1; then \
	  PYTHONDONTWRITEBYTECODE=1 python3 ops/worktree-clean-test.py; \
	else echo "[test] skipped the ops/ script tests — they need python 3.11 (tomllib)" >&2; fi
# the unified install contract: `make install` delegates to the App pinned in
# ops/app/APP_REV, and the checkout it pins lives under target/ where this
# script owns it.
	bash ops/app/install-test.sh
# `cargo test` builds the TEST target, which links dev-dependencies, so a
# product path reaching for a dev-only crate compiles under every test lane
# and breaks only the BINARY.
	$(CARGO) build $(LOCKED) -p node-bin

## the supply-chain tripwire: RustSec advisories and yanked crates against the
## committed Cargo.lock, under `deny.toml` — where every carried advisory is
## listed WITH the reason it is carried and what would clear it. Needs
## `cargo deny` and network, so it is not part of the offline `test` gate; run
## it when the lock moves.
audit:
	$(CARGO) deny check advisories

clean:
	$(CARGO) clean

## rebuild the kernel fixture programs (wasm32 guests the runtime and host
## tests load) and refresh their committed bytes.
kernel-fixtures:
	$(CARGO) build --manifest-path crates/kernel/fixtures/Cargo.toml \
	  --target wasm32-unknown-unknown --release
	cp crates/kernel/fixtures/target/wasm32-unknown-unknown/release/fixture_*.wasm \
	  crates/kernel/fixtures/wasm/

## rebuild the system programs (the wasm32 programs a network is founded
## with: kv, acl, modules, valset, identity, governance, capability, saga,
## dispatch, attribution, gateway) and refresh their committed bytes, which
## `ducktape init` reads from a founding file and the wire tests load.
system-programs:
	$(CARGO) build --manifest-path crates/modules/system/Cargo.toml \
	  --target wasm32-unknown-unknown --release
	cp crates/modules/system/target/wasm32-unknown-unknown/release/*.wasm \
	  crates/modules/system/wasm/
