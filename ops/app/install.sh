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
export CARGO_TARGET_DIR=${APP_CARGO_TARGET_DIR:-$checkout/target-core-install}
export CARGO_BUILD_JOBS

[ -r "$pin_file" ] || die "missing_pin_file: $pin_file"
app_rev=$(<"$pin_file")
[[ "$app_rev" =~ ^[0-9a-f]{40}$ ]] || die "invalid_pin: APP_REV must be one lowercase 40-character commit SHA"

mkdir -p "$(dirname -- "$checkout")"

select_pin() {
	git -C "$checkout" fetch --no-tags origin "$app_rev" >/dev/null 2>&1 \
		|| die "fetch_failed: could not fetch the pinned App commit"
	git -C "$checkout" checkout --detach --quiet "$app_rev" \
		|| die "checkout_failed: could not select the pinned App commit"
}

if [ -e "$checkout" ]; then
	[ -d "$checkout/.git" ] || die "checkout_not_git: refusing to replace $checkout"
	dirty=$(git -C "$checkout" status --porcelain --untracked-files=all 2>/dev/null) \
		|| die "checkout_invalid: could not inspect the App checkout"
	[ -z "$dirty" ] || die "checkout_dirty: refusing to change $checkout"
	current=$(git -C "$checkout" rev-parse --verify HEAD 2>/dev/null || true)
	[ "$current" = "$app_rev" ] || select_pin
else
	git clone --no-checkout "$app_repo" "$checkout" >/dev/null 2>&1 \
		|| die "fetch_failed: could not clone the pinned App repository"
	select_pin
fi

actual=$(git -C "$checkout" rev-parse --verify HEAD 2>/dev/null || true)
[ "$actual" = "$app_rev" ] || die "pin_mismatch: App checkout is not $app_rev"
dirty=$(git -C "$checkout" status --porcelain --untracked-files=all 2>/dev/null) \
	|| die "checkout_invalid: could not inspect the App checkout"
[ -z "$dirty" ] || die "checkout_dirty: App checkout is not clean after pinning"
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
