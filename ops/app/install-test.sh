#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
SCRIPT=$ROOT/ops/app/install.sh
mkdir -p "$ROOT/target"
TEST_ROOT=$(mktemp -d "$ROOT/target/unified-install-test.XXXXXX")
trap 'rm -rf "$TEST_ROOT"' EXIT

fail() {
	printf 'FAIL: %s\n' "$*" >&2
	exit 1
}

assert_contains() {
	local needle=$1 file=$2
	grep -F -- "$needle" "$file" >/dev/null || fail "missing '$needle' in $file"
}

export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null

APP_REPO=$TEST_ROOT/app-repo
mkdir -p "$APP_REPO"
git -C "$APP_REPO" init -q
git -C "$APP_REPO" config user.email test@example.invalid
git -C "$APP_REPO" config user.name unified-install-test
mkdir -p "$APP_REPO/src"
printf 'first\n' > "$APP_REPO/src/version"
cat > "$APP_REPO/Makefile" <<'EOF'
.PHONY: install
install:
	@mkdir -p "$(INSTALL_DEST)"
	@printf '%s\n' "$$(git rev-parse HEAD)" >> "$(INSTALL_DEST)/installed-revs"
	@printf '%s\n' "$(CARGO_TARGET_DIR)" > "$(INSTALL_DEST)/target-dir"
	@printf '%s\n' "$(CARGO_BUILD_JOBS)" > "$(INSTALL_DEST)/jobs"
	@mkdir -p "$(CARGO_TARGET_DIR)" && touch "$(CARGO_TARGET_DIR)/build-artifact"
EOF
git -C "$APP_REPO" add .
git -C "$APP_REPO" commit -qm first
PIN=$(git -C "$APP_REPO" rev-parse HEAD)
printf 'second\n' > "$APP_REPO/src/version"
git -C "$APP_REPO" add src/version
git -C "$APP_REPO" commit -qm second

PIN_FILE=$TEST_ROOT/APP_REV
printf '%s\n' "$PIN" > "$PIN_FILE"
CHECKOUT=$TEST_ROOT/checkout
DEST=$TEST_ROOT/dest
LOG=$TEST_ROOT/install.log

[[ "$(<"$ROOT/ops/app/APP_REV")" =~ ^[0-9a-f]{40}$ ]] || fail 'invalid tracked App pin'

APP_REPO="$APP_REPO" APP_REV_FILE="$PIN_FILE" APP_CHECKOUT_DIR="$CHECKOUT" \
	CARGO_BUILD_JOBS=2 INSTALL_DEST="$DEST" "$SCRIPT" >"$LOG" 2>&1
[ "$(git -C "$CHECKOUT" rev-parse HEAD)" = "$PIN" ] || fail 'first install moved off the pinned commit'
[ "$(<"$DEST/installed-revs")" = "$PIN" ] || fail 'first install delegated the wrong revision'
assert_contains 'running make install' "$LOG"
# cargo writes into the target dir; inside the checkout that made the second run die with checkout_dirty.
target_dir=$(<"$DEST/target-dir")
[ -n "$target_dir" ] || fail 'App target not set'
case "$target_dir" in "$CHECKOUT"/*) fail 'App target inside the checkout dirties it' ;; esac
[ -e "$target_dir/build-artifact" ] || fail 'App build did not use the isolated target'
[ "$(<"$DEST/jobs")" = 2 ] || fail 'build jobs not forwarded'

# A pinned, clean checkout is reused without consulting the repository again.
APP_REPO="$TEST_ROOT/no-longer-available" APP_REV_FILE="$PIN_FILE" APP_CHECKOUT_DIR="$CHECKOUT" \
	INSTALL_DEST="$DEST" "$SCRIPT" >>"$LOG" 2>&1
[ "$(wc -l < "$DEST/installed-revs")" -eq 2 ] || fail 'repeated invocation did not run the App install'
[ "$(git -C "$CHECKOUT" rev-parse HEAD)" = "$PIN" ] || fail 'repeated invocation changed the pin'

# The Core target delegates to the helper without running install-node here.
TARGET_CHECKOUT=$TEST_ROOT/target-checkout
TARGET_DEST=$TEST_ROOT/target-dest
APP_REPO="$APP_REPO" APP_REV_FILE="$PIN_FILE" APP_CHECKOUT_DIR="$TARGET_CHECKOUT" \
	INSTALL_DEST="$TARGET_DEST" make -C "$ROOT" -o install-node -o prereqs install >>"$LOG" 2>&1
[ "$(<"$TARGET_DEST/installed-revs")" = "$PIN" ] || fail 'make install did not delegate to the App'
APP_REPO="$APP_REPO" APP_REV_FILE="$PIN_FILE" APP_CHECKOUT_DIR="$TARGET_CHECKOUT" \
	INSTALL_DEST="$TARGET_DEST" make -C "$ROOT" install-app >>"$LOG" 2>&1
[ "$(wc -l < "$TARGET_DEST/installed-revs")" -eq 2 ] || fail 'standalone install-app failed'

FAIL_REPO=$TEST_ROOT/fail-repo
mkdir -p "$FAIL_REPO"
git -C "$FAIL_REPO" init -q
git -C "$FAIL_REPO" config user.email test@example.invalid
git -C "$FAIL_REPO" config user.name unified-install-test
cat > "$FAIL_REPO/Makefile" <<'EOF'
.PHONY: install
install:
	@exit 23
EOF
git -C "$FAIL_REPO" add Makefile
git -C "$FAIL_REPO" commit -qm failing
FAIL_PIN=$(git -C "$FAIL_REPO" rev-parse HEAD)
printf '%s\n' "$FAIL_PIN" > "$TEST_ROOT/FAIL_REV"
if APP_REPO="$FAIL_REPO" APP_REV_FILE="$TEST_ROOT/FAIL_REV" APP_CHECKOUT_DIR="$TEST_ROOT/fail-checkout" \
	INSTALL_DEST="$TEST_ROOT/fail-dest" "$SCRIPT" >"$TEST_ROOT/fail.log" 2>&1; then
	fail 'App install failure was swallowed'
fi
assert_contains 'install_failed:' "$TEST_ROOT/fail.log"

NO_MAKE_REPO=$TEST_ROOT/no-make-repo
mkdir -p "$NO_MAKE_REPO"
git -C "$NO_MAKE_REPO" init -q
git -C "$NO_MAKE_REPO" config user.email test@example.invalid
git -C "$NO_MAKE_REPO" config user.name unified-install-test
printf 'no install contract\n' > "$NO_MAKE_REPO/README"
git -C "$NO_MAKE_REPO" add README
git -C "$NO_MAKE_REPO" commit -qm no-makefile
NO_MAKE_PIN=$(git -C "$NO_MAKE_REPO" rev-parse HEAD)
printf '%s\n' "$NO_MAKE_PIN" > "$TEST_ROOT/NO_MAKE_REV"
if APP_REPO="$NO_MAKE_REPO" APP_REV_FILE="$TEST_ROOT/NO_MAKE_REV" APP_CHECKOUT_DIR="$TEST_ROOT/no-make-checkout" \
	"$SCRIPT" >"$TEST_ROOT/no-make.log" 2>&1; then
	fail 'missing App Makefile was accepted'
fi
assert_contains 'missing_makefile:' "$TEST_ROOT/no-make.log"

printf 'unified install shell tests passed\n'
