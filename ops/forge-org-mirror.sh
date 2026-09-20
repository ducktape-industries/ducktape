#!/usr/bin/env bash
# Mirror every GitHub organization repository's default branch and tags into Forge.
# Plan mode is the default and never contacts a node. --execute is the only
# mode that verifies a node or writes Forge. Existing tags must match exactly;
# branches may only fast-forward. No force refspec, deletion, or tag rewrite.
set -euo pipefail

readonly SCRIPT_NAME="${0##*/}"
die() { printf '%s: %s\n' "$SCRIPT_NAME" "$*" >&2; exit 1; }

usage() {
    cat <<'EOF'
Usage: ops/forge-org-mirror.sh [options]

Plan mode is the default. It uses `gh api --paginate` and local source fetches,
but never contacts a Forge node. --execute verifies the node/account and then
pushes with signed Git HTTP. The wallet password is read by ducktape's masked
stdin prompt; this script never accepts, stores, or prints it.

  --execute                  verify the node/account and push to Forge
  --org ORG                  GitHub organization (required)
  --source-base URL|PATH     source base or local fixture root (default: https://github.com)
  --node URL                 Forge node HTTP base (required with --execute)
  --network CHAIN-ID         node chain id, e.g. example#01234567 (required with --execute)
  --owner-account NUMBER     writing account number (required with --execute)
  --owner-handle HANDLE      Forge owner handle (required with --execute)
  --key PATH                 encrypted ducktape wallet key (required with --execute)
  --git-signing-key PATH     SSH key for `git push --signed` (required with --execute)
  --ensure-owner             set the handle and publish its Git route if absent
  --work-dir PATH            retain local source staging
  -h, --help                 show this help
EOF
}

require_value() {
    [ "$#" -ge 2 ] || die "$1 requires a value"
    [ -n "$2" ] || die "$1 requires a non-empty value"
}

EXECUTE=0; ENSURE_OWNER=0; ORG=""; SOURCE_BASE="https://github.com"
NODE=""; NETWORK=""; OWNER_ACCOUNT=""; OWNER_HANDLE=""; KEY_PATH=""
GIT_SIGNING_KEY=""; WORK_DIR=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --execute) EXECUTE=1; shift ;;
        --ensure-owner) ENSURE_OWNER=1; shift ;;
        --org) require_value "$1" "${2-}"; ORG="$2"; shift 2 ;;
        --source-base) require_value "$1" "${2-}"; SOURCE_BASE="$2"; shift 2 ;;
        --node) require_value "$1" "${2-}"; NODE="$2"; shift 2 ;;
        --network) require_value "$1" "${2-}"; NETWORK="$2"; shift 2 ;;
        --owner-account) require_value "$1" "${2-}"; OWNER_ACCOUNT="$2"; shift 2 ;;
        --owner-handle) require_value "$1" "${2-}"; OWNER_HANDLE="$2"; shift 2 ;;
        --key) require_value "$1" "${2-}"; KEY_PATH="$2"; shift 2 ;;
        --git-signing-key) require_value "$1" "${2-}"; GIT_SIGNING_KEY="$2"; shift 2 ;;
        --work-dir) require_value "$1" "${2-}"; WORK_DIR="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown argument: $1 (use --help)" ;;
    esac
done
[ -n "$ORG" ] || die "--org is required"
case "$ORG" in */*|*" "*) die "--org must be one organization name" ;; esac

if [ "$EXECUTE" -eq 1 ]; then
    [ -n "$NODE" ] || die "--execute requires --node"
    [ -n "$NETWORK" ] || die "--execute requires --network"
    [ -n "$OWNER_ACCOUNT" ] || die "--execute requires --owner-account"
    [ -n "$OWNER_HANDLE" ] || die "--execute requires --owner-handle"
    [ -n "$KEY_PATH" ] || die "--execute requires --key"
    [ -n "$GIT_SIGNING_KEY" ] || die "--execute requires --git-signing-key"
    [ -r "$KEY_PATH" ] || die "wallet key is not readable"
    [ -r "$GIT_SIGNING_KEY" ] || die "Git signing key is not readable"
    case "$NETWORK" in *#*) ;; *) die "--network must contain #" ;; esac
    case "$OWNER_ACCOUNT" in ''|*[!0-9]*) die "--owner-account must be decimal" ;; esac
    case "$OWNER_HANDLE" in ''|*[!a-z0-9._-]*) die "--owner-handle has unsupported characters" ;; esac
fi

GH_BIN="${GH_BIN:-gh}"; GIT_BIN="${GIT_BIN:-git}"; DUCKTAPE_BIN="${DUCKTAPE_BIN:-ducktape}"
if [ -z "$WORK_DIR" ]; then
    WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/forge-org-mirror.XXXXXX")"; REMOVE_WORK=1
else
    mkdir -p "$WORK_DIR"; REMOVE_WORK=0
fi
[ "$REMOVE_WORK" -eq 0 ] || trap 'rm -rf -- "$WORK_DIR"' EXIT
SOURCE_BASE="${SOURCE_BASE%/}"
mkdir -p "$WORK_DIR/repos"
command -v "$GH_BIN" >/dev/null 2>&1 || die "gh is required"
command -v "$GIT_BIN" >/dev/null 2>&1 || die "git is required"

inventory_json="$WORK_DIR/github-pages.json"
if ! "$GH_BIN" api --paginate --slurp --method GET \
    --header 'Accept: application/vnd.github+json' \
    "orgs/$ORG/repos?per_page=100&type=all" >"$inventory_json" 2>/dev/null; then
    die "GitHub organization inventory failed"
fi
inventory="$WORK_DIR/repos.tsv"
if ! python3 - "$inventory_json" >"$inventory" <<'PY'
import json, re, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    pages = json.load(stream)
if pages and isinstance(pages[0], dict):
    pages = [pages]
if not isinstance(pages, list):
    raise SystemExit("invalid GitHub inventory")
rows = {}
for page in pages:
    if not isinstance(page, list):
        raise SystemExit("invalid GitHub inventory page")
    for repo in page:
        if not isinstance(repo, dict):
            raise SystemExit("invalid GitHub repository record")
        name, branch = repo.get("name"), repo.get("default_branch")
        good_name = isinstance(name, str) and len(name) <= 64 and re.fullmatch(r"[a-z0-9][a-z0-9._-]*", name)
        good_branch = isinstance(branch, str) and bool(branch) and "\n" not in branch and "\t" not in branch
        if not good_name or not good_branch:
            raise SystemExit("unusable repository name or default branch")
        if name in rows and rows[name] != branch:
            raise SystemExit(f"repository {name} repeated with different default branches")
        rows[name] = branch
for name in sorted(rows):
    print(f"{name}\t{rows[name]}")
PY
then
    die "GitHub organization inventory had an invalid shape"
fi

source_url() { printf '%s/%s/%s.git' "$SOURCE_BASE" "$ORG" "$1"; }

fetch_source_repo() {
    local repo="$1" branch="$2" source="$3" repo_dir="$WORK_DIR/repos/$1.git"
    local error_file="$WORK_DIR/$1.fetch.error"
    mkdir -p "$repo_dir"
    if [ ! -f "$repo_dir/HEAD" ]; then
        "$GIT_BIN" init --bare -q "$repo_dir" 2>"$error_file" || die "$repo: cannot initialize staging"
    fi
    if ! "$GIT_BIN" -C "$repo_dir" fetch -q --no-tags "$source" "refs/heads/$branch" 2>"$error_file"; then
        die "$repo: cannot fetch its default branch"
    fi
    local branch_oid
    branch_oid="$($GIT_BIN -C "$repo_dir" rev-parse 'FETCH_HEAD^{commit}')" || die "$repo: default branch is not a commit"
    "$GIT_BIN" -C "$repo_dir" update-ref "refs/heads/$branch" "$branch_oid"
    local tags_file="$WORK_DIR/$1.tags.tsv"
    if ! "$GIT_BIN" ls-remote --refs "$source" 'refs/tags/*' >"$tags_file" 2>"$error_file"; then
        die "$repo: cannot list source tags"
    fi
    : >"$WORK_DIR/$1.source-refs.tsv"
    printf '%s\trefs/heads/%s\n' "$branch_oid" "$branch" >>"$WORK_DIR/$1.source-refs.tsv"
    local oid ref tag
    while read -r oid ref; do
        [ -n "${oid:-}" ] || continue
        tag="${ref#refs/tags/}"
        if ! "$GIT_BIN" -C "$repo_dir" fetch -q --no-tags "$source" "$ref" 2>"$error_file"; then
            die "$repo: cannot fetch tag $tag"
        fi
        "$GIT_BIN" -C "$repo_dir" cat-file -e "$oid^{object}" 2>"$error_file" || die "$repo: source tag unavailable"
        "$GIT_BIN" -C "$repo_dir" update-ref "$ref" "$oid"
        printf '%s\t%s\n' "$oid" "$ref" >>"$WORK_DIR/$1.source-refs.tsv"
    done <"$tags_file"
}

print_plan() {
    local repo="$1" oid ref
    while IFS=$'\t' read -r oid ref; do
        [ -n "${ref:-}" ] || continue
        printf 'plan\t%s\t%s\t%s\n' "$repo" "$ref" "$oid"
    done <"$WORK_DIR/$repo.source-refs.tsv"
}

while IFS=$'\t' read -r repo branch; do
    [ -n "${repo:-}" ] || continue
    fetch_source_repo "$repo" "$branch" "$(source_url "$repo")"
    [ "$EXECUTE" -eq 1 ] || print_plan "$repo"
done <"$inventory"
[ "$EXECUTE" -eq 1 ] || exit 0

command -v "$DUCKTAPE_BIN" >/dev/null 2>&1 || die "ducktape is required for --execute"
wallet_status="$WORK_DIR/wallet.status"
if ! "$DUCKTAPE_BIN" user key status --key "$KEY_PATH" >"$wallet_status" 2>/dev/null; then
    die "wallet key status failed"
fi
wallet_pubkey="$(awk '$1 == "encrypted" {print $2; exit}' "$wallet_status")"
[ -n "$wallet_pubkey" ] || die "wallet key is not encrypted"
account_output="$WORK_DIR/account.txt"
if ! "$DUCKTAPE_BIN" account show --number "$OWNER_ACCOUNT" --node "$NODE" --key "$KEY_PATH" >"$account_output" 2>/dev/null; then
    die "owner account lookup failed"
fi
grep -Eq "^number=${OWNER_ACCOUNT} name=" "$account_output" || die "endpoint did not resolve the requested account"
grep -F "key=ed25519 $wallet_pubkey" "$account_output" >/dev/null 2>&1 || die "wallet key is not in the requested account"

# forge setup is the source CLI's real endpoint/status/network/Git-door
# resolver. Its isolated registry is local process state, not an account switch.
DUCKTAPE_HOME="$WORK_DIR/ducktape-home"; export DUCKTAPE_HOME
setup_output="$WORK_DIR/forge-setup.txt"; setup_ok=0
if "$DUCKTAPE_BIN" forge setup --node "$NODE" >"$setup_output" 2>/dev/null; then setup_ok=1; fi
grep -F "$NETWORK" "$setup_output" >/dev/null 2>&1 || die "endpoint did not verify the requested network"
owner_door() { grep -F "for owner $OWNER_HANDLE" "$setup_output" >/dev/null 2>&1; }
if [ "$setup_ok" -eq 0 ] || ! owner_door; then
    [ "$ENSURE_OWNER" -eq 1 ] || die "owner has no Git door; use --ensure-owner to create it"
    "$DUCKTAPE_BIN" account set-handle --handle "$OWNER_HANDLE" --node "$NODE" --key "$KEY_PATH" || die "owner handle write failed"
    "$DUCKTAPE_BIN" forge publish --node "$NODE" --key "$KEY_PATH" || die "owner Git route write failed"
    "$DUCKTAPE_BIN" forge setup --node "$NODE" >"$setup_output" 2>/dev/null || die "owner resolution failed after owner writes"
    grep -F "$NETWORK" "$setup_output" >/dev/null 2>&1 || die "endpoint network changed during owner setup"
    owner_door || die "owner has no Git door after owner setup"
else
    # This idempotent owner write uses the existing masked wallet prompt and
    # catches a wrong signer before any repository ref is read or pushed.
    "$DUCKTAPE_BIN" forge publish --node "$NODE" --key "$KEY_PATH" || die "owner Git route verification failed"
fi

network_authority="${NETWORK/\#/-}"
FORGE_BASE="duck://${network_authority}/forge/${OWNER_HANDLE}"
ducktape_path="$(command -v "$DUCKTAPE_BIN")"
ducktape_dir="$(dirname "$ducktape_path")"
export PATH="$ducktape_dir:$PATH"

for_repo() {
    local repo="$1" branch="$2" source_file="$WORK_DIR/$1.source-refs.tsv"
    local remote_url="$FORGE_BASE/$repo" remote_file="$WORK_DIR/$1.remote-refs.tsv"
    local push_file="$WORK_DIR/$1.push-refs" error_file="$WORK_DIR/$1.remote.error"
    local oid ref old forge_oid
    : >"$push_file"
    if ! "$GIT_BIN" ls-remote --refs "$remote_url" "refs/heads/$branch" 'refs/tags/*' >"$remote_file" 2>"$error_file"; then
        die "$repo: cannot read Forge refs"
    fi
    while IFS=$'\t' read -r oid ref; do
        [ -n "${ref:-}" ] || continue
        old="$(awk -v want="$ref" '$2 == want {print $1; exit}' "$remote_file")"
        if [ -z "$old" ]; then
            printf '%s:%s\n' "$ref" "$ref" >>"$push_file"
            printf 'plan\t%s\t%s\t%s\n' "$repo" "$ref" "$oid"
            continue
        fi
        if [ "$old" = "$oid" ]; then
            printf 'unchanged\t%s\t%s\t%s\n' "$repo" "$ref" "$oid"
            continue
        fi
        case "$ref" in
            refs/tags/*) die "$repo: existing tag $ref differs; refusing to overwrite" ;;
            refs/heads/*)
                if ! "$GIT_BIN" -C "$WORK_DIR/repos/$repo.git" fetch -q --no-tags "$remote_url" "$ref" 2>"$error_file"; then
                    die "$repo: cannot fetch Forge branch for ancestry"
                fi
                forge_oid="$($GIT_BIN -C "$WORK_DIR/repos/$repo.git" rev-parse 'FETCH_HEAD^{commit}')" || die "$repo: Forge branch is not a commit"
                [ "$forge_oid" = "$old" ] || die "$repo: Forge branch changed during preflight"
                "$GIT_BIN" -C "$WORK_DIR/repos/$repo.git" merge-base --is-ancestor "$old" "$oid" || die "$repo: diverging Forge branch $ref; refusing to overwrite"
                printf '%s:%s\n' "$ref" "$ref" >>"$push_file"
                printf 'plan\t%s\t%s\t%s\n' "$repo" "$ref" "$oid"
                ;;
            *) die "$repo: unexpected source ref $ref" ;;
        esac
    done <"$source_file"
}

# Validate every repository before the first push. A divergence or tag
# collision therefore stops the run without a ref overwrite.
while IFS=$'\t' read -r repo branch; do
    [ -n "${repo:-}" ] || continue
    for_repo "$repo" "$branch"
done <"$inventory"

while IFS=$'\t' read -r repo branch; do
    [ -n "${repo:-}" ] || continue
    push_file="$WORK_DIR/$repo.push-refs"
    [ -s "$push_file" ] || continue
    mapfile -t refspecs <"$push_file"
    printf 'push\t%s\t%s\n' "$repo" "${#refspecs[@]}"
    GIT_TERMINAL_PROMPT=0 GIT_CONFIG_COUNT=3 \
    GIT_CONFIG_KEY_0=gpg.format GIT_CONFIG_VALUE_0=ssh \
    GIT_CONFIG_KEY_1=user.signingkey GIT_CONFIG_VALUE_1="$GIT_SIGNING_KEY" \
    GIT_CONFIG_KEY_2=push.gpgSign GIT_CONFIG_VALUE_2=true \
        "$GIT_BIN" -C "$WORK_DIR/repos/$repo.git" push --porcelain --signed "$FORGE_BASE/$repo" "${refspecs[@]}" || die "$repo: signed Forge push failed"
done <"$inventory"
printf 'complete\t%s\n' "$ORG"
