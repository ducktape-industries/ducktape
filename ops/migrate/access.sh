#!/usr/bin/env bash
# Moves a running network founded before the access role onto it in place, by the validators' vote, while its node still runs the binary it was founded with.
# Usage: ops/migrate/access.sh FOUNDING_DIR LEAD_BLOCKS [DEPLOY_COMMAND...]
set -euo pipefail

usage="usage: ops/migrate/access.sh FOUNDING_DIR LEAD_BLOCKS [DEPLOY_COMMAND...]"
founding=$(cd -- "${1:?$usage}" && pwd)
lead=${2:?$usage}
shift 2
deploy=("$@")
read -r -a dt <<<"${DUCKTAPE:-ducktape}"
read -r -a putter <<<"${DUCKTAPE_PUT:-${DUCKTAPE:-ducktape}}"
remote=${FOUNDING_REMOTE:-$founding}
work=${MIGRATE_DIR:-$PWD/access-migration}
mkdir -p "$work"

temporary=(forge files harness)
migrating=(forge files harness identity)

read -r -d '' codec <<'PY' || true
import hashlib, struct, sys, tomllib
from pathlib import Path

out = sys.stdout.buffer

def u8(n): return struct.pack("<B", n)
def u64(n): return struct.pack("<Q", n)
def s(text): b = text.encode(); return struct.pack("<I", len(b)) + b
def vbytes(b): return struct.pack("<I", len(b)) + b
def blob(text):
    kind, digest = text.split(":")
    return u8({"sha256": 0, "sha1": 1}[kind]) + bytes.fromhex(digest)

class Reader:
    def __init__(self, b): self.b, self.p = b, 0
    def take(self, n): v = self.b[self.p:self.p + n]; self.p += n; return v
    def u8(self): return self.take(1)[0]
    def u32(self): return struct.unpack("<I", self.take(4))[0]
    def u64(self): return struct.unpack("<Q", self.take(8))[0]
    def s(self): return self.take(self.u32()).decode()
    def bytes(self): return self.take(self.u32())
    def blob(self):
        tag = self.u8()
        return ("sha256:" + self.take(32).hex()) if tag == 0 else ("sha1:" + self.take(20).hex())
    def party(self):
        return f"account:{self.u64()}" if self.u8() == 0 else f"program:{self.s()}"
    def ownership(self):
        tag = self.u8()
        if tag == 0: return "owner"
        if tag == 2: return "anyone"
        return "listed:" + ",".join(self.party() for _ in range(self.u32()))

def reply(): return Reader(sys.stdin.buffer.read())

def program_id(path):
    body = Path(path).read_bytes()
    return "sha256:" + hashlib.sha256(f"program {len(body)}\0".encode() + body).hexdigest()

cmd, args = sys.argv[1], sys.argv[2:]
if cmd == "founding":
    root = Path(args[0])
    doc = tomllib.loads((root / "founding.toml").read_text())
    for p in doc.get("programs", []):
        params = p.get("params", "-")
        print("\t".join(["program", p["id"], p["code"], program_id(root / p["code"]), params,
                         ",".join(p.get("roles", [])) or "-"]))
    for v in doc.get("views", []):
        print("\t".join(["view", v["name"], v["code"], program_id(root / v["code"])]))
elif cmd == "params-hex":
    print(Path(args[0]).read_bytes().hex() if args else "")
elif cmd == "set":
    height, program, code, params = args
    out.write(u8(0) + u64(int(height)) + u8(0) + s(program) + blob(code) + vbytes(bytes.fromhex(params)))
elif cmd == "view":
    height, name, code = args
    out.write(u8(0) + u64(int(height)) + u8(2) + s(name) + blob(code))
elif cmd == "tag":
    out.write(u8(int(args[0])))
elif cmd == "ask-program":
    out.write(u8(2) + s(args[0]))
elif cmd == "params":
    r = reply()
    assert r.u8() == 2
    r.u64()
    if r.u8() == 0:
        print("-")
    else:
        r.s(); r.blob(); print(r.bytes().hex())
elif cmd == "ask-views":
    out.write(u8(3) + u64(int(args[0])))
elif cmd == "views":
    r = reply()
    assert r.u8() == 3
    for _ in range(r.u32()):
        print(f"{r.s()}\t{r.blob()}")
elif cmd == "ask-scheduled":
    out.write(u8(1) + u8(0) + u8(0))
elif cmd == "scheduled":
    r = reply()
    assert r.u8() == 1
    r.u64()
    for _ in range(r.u32()):
        height, tag = r.u64(), r.u8()
        if tag in (0, 6):
            name = r.s(); r.blob(); r.bytes()
            if tag == 6:
                [r.s() for _ in range(r.u32())]
        elif tag == 2:
            name = "view:" + r.s(); r.blob()
        else:
            name = r.s()
        if r.u8() == 1:
            r.u64()
        print(f"{height} {name}")
elif cmd == "ask-roles":
    out.write(u8(4))
elif cmd == "roles":
    r = reply()
    assert r.u8() == 4
    for _ in range(r.u32()):
        print(f"{r.s()} {r.s()}")
elif cmd == "ask-lists":
    after = None if args[1] == "-" else bytes.fromhex(args[1])
    page = (u8(0) if after is None else u8(1) + vbytes(after)) + u8(0)
    out.write(u8(3) + s(args[0]) + page)
elif cmd == "lists":
    r = reply()
    assert r.u8() == 3
    r.u64()
    for _ in range(r.u32()):
        print(f"{r.s()}\t{r.ownership()}")
    if r.u8() == 1:
        print(f"next\t{r.bytes().hex()}")
else:
    sys.exit(f"codec: no command {cmd}")
PY

codec() { python3 -c "$codec" "$@"; }
step() { printf '\n== %s\n' "$*"; }
status() { "${dt[@]}" status; }
height() { status | awk '$1 == "height" { print $2 }'; }
roles() { status | sed -n 's/^roles *//p' | tr ',' '\n' | sed 's/^ *//' | sed '/^$/d' | sort; }
running() { "${dt[@]}" programs | awk -F'\t' -v p="$1" '$1 == p { print $2 }'; }
ask() { local program=$1; shift; codec "$@" | "${dt[@]}" query "$program"; }

wait_for() {
  local at=$1
  echo "waiting for block $at"
  until (($(height) >= at)); do sleep 1; done
}

ballot() {
  local outcome=$1 again=$2
  echo "  $outcome"
  case $outcome in
    enacted*) ;;
    *)
      echo "the vote is $outcome: every other validator casts the same ballot ($again), then run this script again" >&2
      exit 3
      ;;
  esac
}

vote_role() {
  local role=$1 file=$2
  ballot "$("${dt[@]}" vote role "$role" <"$file")" "ducktape vote role $role $file"
}

vote_bind() {
  ballot "$("${dt[@]}" vote bind "$@")" "ducktape vote bind $*"
}

declare -A code_of path_of params_of roles_of view_code view_path
programs=()
views=()
while IFS=$'\t' read -r kind name path id params declared; do
  case $kind in
    program)
      programs+=("$name")
      code_of[$name]=$id
      path_of[$name]=$path
      params_of[$name]=${params#-}
      roles_of[$name]=$declared
      ;;
    view)
      views+=("$name")
      view_code[$name]=$id
      view_path[$name]=$path
      ;;
  esac
done < <(codec founding "$founding")

expected_roles() {
  local program role
  for program in "${programs[@]}"; do
    [[ ${roles_of[$program]} == - ]] && continue
    IFS=, read -r -a declared <<<"${roles_of[$program]}"
    for role in "${declared[@]}"; do echo "$role $program"; done
  done | sort
}

table_roles() { ask programs ask-roles | codec roles | sort; }

view_running() {
  ask programs ask-views "$(height)" | codec views | awk -F'\t' -v v="$1" '$1 == v { print $2 }'
}

pending() { ask programs ask-scheduled | codec scheduled; }

change_op() {
  local name=$1 at=$2 params
  case $name in
    view:*)
      codec view "$at" "${name#view:}" "${view_code[${name#view:}]}"
      ;;
    *)
      if [[ -n $(running "$name") ]]; then
        params=$(ask programs ask-program "$name" | codec params)
      else
        params=$(codec params-hex ${params_of[$name]:+"$founding/${params_of[$name]}"})
      fi
      codec set "$at" "$name" "${code_of[$name]}" "$params"
      ;;
  esac
}

land() {
  local at name file
  at=$(($(height) + lead + $#))
  for name in "$@"; do
    file="$work/${name/:/-}-set.op"
    change_op "$name" "$at" >"$file"
    echo "$name -> block $at"
    vote_role programs "$file"
    pending | grep -qx "$at $name" || { echo "the programs program refused the change to $name: see ducktape logs" >&2; exit 1; }
  done
  wait_for "$at"
  for name in "$@"; do
    case $name in
      view:*) [[ $(view_running "${name#view:}") == "${view_code[${name#view:}]}" ]] ;;
      *) [[ $(running "$name") == "${code_of[$name]}" ]] ;;
    esac || { echo "$name does not run its new code" >&2; exit 1; }
  done
}

migrated() {
  [[ $(running programs) == "${code_of[programs]}" ]] && [[ -n $(table_roles) ]]
}

step "the network as it runs"
status

step "put every program and view of $founding as program blobs"
files=()
for program in "${programs[@]}"; do
  [[ $(running "$program") == "${code_of[$program]}" ]] || files+=("$remote/${path_of[$program]}")
done
for view in "${views[@]}"; do
  files+=("$remote/${view_path[$view]}")
done
if ((${#files[@]})); then
  "${putter[@]}" blob put --kind program --wait "${files[@]}"
fi

step "admit access, which fills the access role"
[[ $(running access) == "${code_of[access]}" ]] || land access

if ! migrated; then
  step "bind the access role, and a role each for the programs whose move the vote runs"
  for program in access "${temporary[@]}"; do
    roles | grep -qx "$program $program" || vote_bind "$program" "$program"
  done
fi

step "swap every program but programs and valset, and every view, to its new code at one block"
batch=()
for program in "${programs[@]}"; do
  case $program in programs | valset | access) continue ;; esac
  [[ $(running "$program") == "${code_of[$program]}" ]] || batch+=("$program")
done
for view in "${views[@]}"; do
  [[ $(view_running "$view") == "${view_code[$view]}" ]] || batch+=("view:$view")
done
((${#batch[@]} == 0)) || land "${batch[@]}"

if ! migrated; then
  step "run each program's move by vote: ${migrating[*]}"
  for program in "${migrating[@]}"; do
    case $program in
      forge) tag=11 ;;
      files) tag=4 ;;
      harness) tag=7 ;;
      identity) tag=12 ;;
    esac
    codec tag "$tag" >"$work/$program-migrate.op"
    echo "$program"
    vote_role "$program" "$work/$program-migrate.op"
  done

  step "unbind the roles bound for the move"
  for program in "${temporary[@]}"; do
    if roles | grep -q "^$program "; then vote_bind "$program"; fi
  done
fi

step "swap the programs program, whose new code keeps the role table"
[[ $(running programs) == "${code_of[programs]}" ]] || land programs

if ! migrated; then
  step "copy the kernel's role table into the programs program"
  codec tag 4 >"$work/programs-migrate.op"
  vote_role programs "$work/programs-migrate.op"
fi
migrated || { echo "the programs program holds no role table: see ducktape logs" >&2; exit 1; }

step "swap valset, whose new code has no Motion::Bind"
[[ $(running valset) == "${code_of[valset]}" ]] || land valset

step "verify"
failed=0
for program in "${programs[@]}"; do
  code=$(running "$program")
  mark=ok
  [[ $code == "${code_of[$program]}" ]] || { mark=WRONG; failed=1; }
  printf '%-12s %s %s\n' "$program" "$code" "$mark"
done
for view in "${views[@]}"; do
  code=$(view_running "$view")
  mark=ok
  [[ $code == "${view_code[$view]}" ]] || { mark=WRONG; failed=1; }
  printf 'view %-7s %s %s\n' "$view" "$code" "$mark"
done
echo "role table in the programs program:"
table_roles | sed 's/^/  /'
if ! diff <(table_roles) <(expected_roles) >/dev/null; then
  echo "the role table differs from the roles $founding/founding.toml declares:" >&2
  diff <(table_roles) <(expected_roles) >&2 || true
  failed=1
fi
if ! diff <(table_roles) <(roles) >/dev/null; then
  echo "the role table differs from the kernel's:" >&2
  diff <(table_roles) <(roles) >&2 || true
  failed=1
fi
for program in "${migrating[@]}" ; do
  echo "access lists of $program:"
  after=-
  while :; do
    page=$(ask access ask-lists "$program" "$after" | codec lists)
    grep -v '^next' <<<"$page" | sed '/^$/d; s/^/  /'
    after=$(awk -F'\t' '$1 == "next" { print $2 }' <<<"$page")
    [[ -n $after ]] || break
  done
done
((failed == 0)) || { echo "verification failed" >&2; exit 1; }

step "the network is ready for the node binary that runs the access role"
if ((${#deploy[@]})); then
  "${deploy[@]}"
else
  echo "next: deploy the node binary built from this ducktape commit, e.g. ops/k8s/deploy NETWORK $(git -C "$(dirname -- "${BASH_SOURCE[0]}")" rev-parse HEAD)"
fi
