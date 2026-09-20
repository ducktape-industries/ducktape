#!/usr/bin/env bash
set -euo pipefail

die() {
	printf 'app install: %s\n' "$*" >&2
	exit 1
}

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
pin_file=${APP_REV_FILE:-$repo_root/ops/app/APP_REV}
checkout=${APP_CHECKOUT_DIR:-$repo_root/target/unified-install/ducktape-app}
app_repo=${APP_REPO:-https://github.com/ducktape-industries/ducktape-app.git}
app_make=${APP_MAKE:-make}
export CARGO_TARGET_DIR=${APP_CARGO_TARGET_DIR:-$repo_root/target/unified-install/app-target}
export CARGO_BUILD_JOBS

[ -r "$pin_file" ] || die "missing_pin_file: $pin_file"
app_rev=$(<"$pin_file")
[[ "$app_rev" =~ ^[0-9a-f]{40}$ ]] || die "invalid_pin: APP_REV must be one lowercase 40-character commit SHA"

mkdir -p "$(dirname -- "$checkout")"

# The checkout is build output this script owns, under target/ — never a tree
# anyone edits, and never the place to test a local App change (the App repo
# has its own `make install` for that). So it is REPAIRED to the pin rather
# than inspected and refused: a leftover cargo target from an older layout, a
# clone interrupted mid-fetch, or a half-applied checkout would otherwise wedge
# every later `make install` with nothing to do but delete the tree by hand.
# `--force` discards edits to tracked files and `clean -ffd` removes untracked
# ones: together exactly what the pin check below can see. Ignored paths are
# deliberately left alone — the App's bundle step stages under its own
# (ignored) target/, and clearing that would throw away work no check objects
# to.
pin_checkout() {
	local current
	current=$(git -C "$checkout" rev-parse --verify HEAD 2>/dev/null || true)
	if [ "$current" != "$app_rev" ]; then
		git -C "$checkout" fetch --no-tags origin "$app_rev" >/dev/null 2>&1 \
			|| die "fetch_failed: could not fetch the pinned App commit"
	fi
	git -C "$checkout" checkout --detach --force --quiet "$app_rev" \
		|| die "checkout_failed: could not select the pinned App commit"
	git -C "$checkout" clean -ffdq \
		|| die "clean_failed: could not clear leftovers from $checkout"
}

if [ -e "$checkout" ]; then
	# A non-git path here is the one thing repair cannot cover: deleting a
	# directory this script never created is not its call.
	[ -d "$checkout/.git" ] || die "checkout_not_git: refusing to replace $checkout"
else
	git clone --no-checkout "$app_repo" "$checkout" >/dev/null 2>&1 \
		|| die "fetch_failed: could not clone the pinned App repository"
fi
pin_checkout

actual=$(git -C "$checkout" rev-parse --verify HEAD 2>/dev/null || true)
[ "$actual" = "$app_rev" ] || die "pin_mismatch: App checkout is not $app_rev"
dirty=$(git -C "$checkout" status --porcelain --untracked-files=all 2>/dev/null) \
	|| die "checkout_invalid: could not inspect the App checkout"
[ -z "$dirty" ] || die "repair_failed: App checkout is not clean after pinning"
[ -f "$checkout/Makefile" ] || die "missing_makefile: pinned App does not provide Makefile"

if ! "$app_make" -C "$checkout" --no-print-directory -n install >/dev/null 2>&1; then
	die "missing_install_target: pinned App does not provide make install"
fi

printf 'app install: pinned %s\n' "$app_rev"
printf 'app install: checkout %s\n' "$checkout"
printf 'app install: running make install; its output reports destinations\n'
if "$app_make" -C "$checkout" --no-print-directory install; then
	:
else
	status=$?
	printf 'app install: install_failed: pinned App make install exited %s\n' "$status" >&2
	exit "$status"
fi
printf 'app install: complete; see the App install output above for destinations\n'
