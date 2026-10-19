#!/usr/bin/env bash
# Seeds a freshly founded network's first agents: founder creates chief and gives it its own key; chief's key does the rest and creates tech.
# Usage: DUCKTAPE=<ducktape> TOOL=<found> seed.sh NODE_URL NETWORK NODE_KEY FOUNDER_WORKSPACE FOUNDER_PASSWORD_FILE CHIEF_WORKSPACE CHIEF_PASSWORD_FILE CHIEF_KEY_FILE STEP_DIR
set -euo pipefail
[ $# -eq 9 ] || { sed -n 3p "$0" >&2; exit 2; }
node=$1 network=$2 node_key=$3
founder_workspace=$4 founder_password_file=$5
chief_workspace=$6 chief_password_file=$7 chief_key_file=$8
out=$9
B=${DUCKTAPE:?the ducktape binary}
T=${TOOL:?the found binary}
model=${MODEL:-claude-opus-5-5}
service=${SERVICE:-anthropic}
consent_ttl_ms=900000
mkdir -p "$out"
cd "$out"

as() {
  local who=$1; shift
  local workspace password_file
  case $who in
    founder) workspace=$founder_workspace password_file=$founder_password_file ;;
    chief) workspace=$chief_workspace password_file=$chief_password_file ;;
  esac
  DUCKTAPE_PASSWORD="$(cat "$password_file")" "$B" --workspace "$workspace" --node "$node" "$@"
}
ask() {
  as founder query "$@"
}
land() {
  local who=$1 name=$2 program=$3; shift 3
  [ -e "$name.landed" ] && { cat "$name.landed"; return; }
  "$T" "$@" > "$name.bin"
  as "$who" submit "$program" "$name.bin" --land | tee "$name.landed"
  grep -q '^landed [0-9]* applied' "$name.landed" || { echo "seed: $name did not apply" >&2; exit 1; }
}
returned() {
  "$T" d-number "$(awk '/^landed/ {print $4}' "$1.landed")"
}
active_key() {
  as "$1" wallet list | awk '$1 == "*" {print $3}'
}
put_config() {
  local who=$1 name=$2 path=$3
  "$T" config "$model" 65535 high web 5000 650000 "$name.prompt" > "$name.md"
  [ -e "$name.blob" ] || as "$who" blob put "$name.md" --kind chunk --wait | tee "$name.blob"
  local id; id=$(grep -oE '(sha256:)?[0-9a-f]{64}' "$name.blob" | head -1)
  land "$who" "$name-commit" files commit-file "seed $path" "$path" "$(stat -c %s "$name.md")" "$id"
}

for who in founder chief; do
  [ -n "$(active_key "$who")" ] || { echo "seed: $who's workspace holds no active wallet key; run ducktape wallet new $who there first" >&2; exit 1; }
done
chief_key=$(active_key chief)

land founder create-founder identity create founder
founder=$(returned create-founder)
land founder create-chief identity create-agent chief
chief=$(returned create-chief)

if [ ! -e add-chief-key.landed ]; then
  "$T" q-generation "$chief_key" > generation.q
  generation=$(ask identity generation.q | "$T" d-identity | grep -oE '[0-9]+' | head -1)
  expires=$(( $(date +%s000) + consent_ttl_ms ))
  land founder add-chief-key identity add-key "$chief_key_file" "$chief_password_file" "$chief" "$network" "$generation" "$expires" chief
fi
land founder chief-answers access set-list harness "$("$T" agent-key "$chief")" anyone

land chief register-service egress register "$service"
land chief assign-service egress assign "$service" "$node_key"
land chief set-model harness set-model "$model" "$service" "$model"

[ -e chief.prompt ] || printf 'You are chief (account %s), the operator agent of the %s network. Your full prompt is still to be seeded.\n' "$chief" "$network" > chief.prompt
put_config chief chief /shared/agents/chief/md
land chief script-chief identity set-script "$chief" /shared/agents/chief/md

land chief create-tech identity create-agent tech
tech=$(returned create-tech)
[ -e tech.prompt ] || printf 'You are tech (account %s), the engineer agent of the %s network, managed by chief (account %s). Your full prompt is still to be seeded.\n' "$tech" "$network" "$chief" > tech.prompt
put_config chief tech /shared/agents/tech/md
land chief script-tech identity set-script "$tech" /shared/agents/tech/md
land chief tech-answers access set-list harness "$("$T" agent-key "$tech")" anyone
land chief tech-conversations access set-list harness "$("$T" conversation-key "$tech")" "listed:$chief"

echo "founder $founder chief $chief tech $tech"
