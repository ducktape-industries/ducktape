#!/usr/bin/env bash
# The QA walk runner (ducktape-industries/ducktape-qa) at exactly the rev in RUNNER_REV.
#   ops/qa/run.sh <scenario>… <walk.py args>
#       e.g. ops/qa/run.sh setup.json update-path.json --out ~/qa/<sha> --params ~/qa/params.json
#   ops/qa/run.sh --check   every acceptance scenario through the pinned runner's walk.load; non-zero on a refusal
# This repo has no suite of its own, so a leading bare name is one of the pinned checkout's
# scenarios/acceptance/*.json; a name with a slash (an absolute path included) is passed through.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
rev=$(tr -d '[:space:]' < "$here/RUNNER_REV")
[[ $rev =~ ^[0-9a-f]{40}$ ]] || { echo "run.sh: RUNNER_REV is not a full sha: $rev" >&2; exit 3; }
dir=${XDG_CACHE_HOME:-$HOME/.cache}/ducktape-qa/$rev
if [[ ! -e $dir ]]; then
    git clone -q --no-checkout https://github.com/ducktape-industries/ducktape-qa.git "$dir"
    git -C "$dir" checkout -q --detach "$rev"
fi
# never pulled, never repaired: a checkout that is not exactly $rev is refused
[[ $(git -C "$dir" rev-parse HEAD 2>/dev/null) == "$rev" && -z $(git -C "$dir" status --porcelain) ]] \
    || { echo "run.sh: $dir is not a clean checkout of $rev; move it aside and run again" >&2; exit 3; }
export PYTHONDONTWRITEBYTECODE=1
if [[ ${1-} == --check ]]; then
    exec python3 - "$dir" "$dir"/scenarios/acceptance/*.json <<'EOF'
import sys
sys.path.insert(0, sys.argv[1])
import walk
bad = 0
for path in sys.argv[2:]:
    try:
        walk.load(path)
    except Exception as error:  # any failure to load is a refusal
        print(f'{path}: {error!r}', file=sys.stderr)
        bad += 1
print(f'{len(sys.argv) - 2 - bad}/{len(sys.argv) - 2} acceptance scenarios load through {walk.__file__}')
sys.exit(1 if bad else 0)
EOF
fi
scenarios=()
while (($#)); do
    case $1 in
        -*) break ;;  # the scenarios end at the first flag
        */*) scenarios+=("$1") ;;
        *) scenarios+=("$dir/scenarios/acceptance/$1") ;;
    esac
    shift
done
exec python3 "$dir/walk.py" "${scenarios[@]}" "$@"
