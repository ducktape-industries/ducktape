#!/usr/bin/env bash
# Seeds a freshly founded network's first agents: founder creates chief and gives it its own key; chief's key does the rest and creates tech.
# Usage: DUCKTAPE=<ducktape> seed.sh NODE_URL NODE_KEY FOUNDER_WORKSPACE FOUNDER_PASSWORD_FILE CHIEF_WORKSPACE CHIEF_PASSWORD_FILE STEP_DIR
set -euo pipefail
[ $# -eq 7 ] || { sed -n 3p "$0" >&2; exit 2; }
node=$1 node_key=$2
founder_workspace=$3 founder_password_file=$4
chief_workspace=$5 chief_password_file=$6
out=$7
B=${DUCKTAPE:?the ducktape binary}
model=${MODEL:-claude-opus-5-5}
service=${SERVICE:-anthropic}
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
land() {
  local who=$1 name=$2 program=$3 op=$4
  [ -e "$name.landed" ] && { cat "$name.landed"; return; }
  as "$who" op "$program" "$op" --land > "$name.json"
  mv "$name.json" "$name.landed"
  cat "$name.landed"
}
returned() {
  grep -oE '"returned":[0-9]+' "$1.landed" | cut -d: -f2
}
active_key() {
  as "$1" wallet list | awk '$1 == "*" {print $3}'
}
put_config() {
  local who=$1 name=$2 path=$3
  [ -e "$name.md" ] || {
    printf '+++\nmodel = "%s"\nprovider = "anthropic"\nmax_tokens = 65535\nthinking = "high"\nserver_tools = ["web_search", "web_fetch"]\nsettle = "5s"\ncompact_at = 650000\n+++\n' "$model"
    cat "$name.prompt"
  } > "$name.md"
  [ -e "$name.blob" ] || as "$who" blob put "$name.md" --kind chunk --wait > "$name.blob"
  local id; id=$(grep -oE '^sha256:[0-9a-f]{64}' "$name.blob" | head -1 | cut -d: -f2)
  local size; size=$(stat -c %s "$name.md")
  land "$who" "$name-commit" files "{\"Commit\": {\"base\": null, \"message\": \"seed $path\", \"changes\": [{\"Put\": {\"path\": \"$path\", \"exec\": false, \"meta\": [], \"content\": {\"Chunks\": {\"size\": $size, \"chunks\": [{\"Sha256\": \"$id\"}]}}}}]}}"
}

for who in founder chief; do
  [ -n "$(active_key "$who")" ] || { echo "seed: $who's workspace holds no active wallet key; run ducktape wallet new $who there first" >&2; exit 1; }
done

land founder create-founder identity '{"Create": {"name": "founder", "scheme": "Ed25519"}}'
founder=$(returned create-founder)
land founder create-chief identity '{"CreateAgent": {"name": "chief"}}'
chief=$(returned create-chief)

if [ ! -e add-chief-key.landed ]; then
  consent=$(as chief wallet consent --account "$chief")
  land founder add-chief-key identity "{\"AddKey\": {\"scheme\": \"Ed25519\", \"label\": \"chief\", \"consent\": $consent}}"
fi
land founder chief-answers access "{\"Set\": {\"program\": \"harness\", \"key\": \"agents/$chief\", \"list\": \"Anyone\"}}"

land chief register-service egress "{\"Register\": {\"service\": \"$service\"}}"
land chief assign-service egress "{\"Assign\": {\"service\": \"$service\", \"nodes\": [\"$node_key\"]}}"
land chief set-route harness "{\"SetRoute\": {\"provider\": \"anthropic\", \"services\": [{\"service\": \"$service\", \"wire\": \"AnthropicMessages\"}]}}"

network=$(as chief status | awk '$1 == "network" {print $2}')
[ -e chief.prompt ] || printf 'You are chief (account %s), the operator agent of the %s network. Your full prompt is still to be seeded.\n' "$chief" "$network" > chief.prompt
put_config chief chief /shared/agents/chief/md
land chief script-chief identity "{\"SetScript\": {\"account\": $chief, \"script\": {\"calls\": \"harness\", \"config\": \"/shared/agents/chief/md\"}}}"

land chief create-tech identity '{"CreateAgent": {"name": "tech"}}'
tech=$(returned create-tech)
[ -e tech.prompt ] || printf 'You are tech (account %s), the engineer agent of the %s network, managed by chief (account %s). Your full prompt is still to be seeded.\n' "$tech" "$network" "$chief" > tech.prompt
put_config chief tech /shared/agents/tech/md
land chief script-tech identity "{\"SetScript\": {\"account\": $tech, \"script\": {\"calls\": \"harness\", \"config\": \"/shared/agents/tech/md\"}}}"
land chief tech-answers access "{\"Set\": {\"program\": \"harness\", \"key\": \"agents/$tech\", \"list\": \"Anyone\"}}"
land chief tech-conversations access "{\"Set\": {\"program\": \"harness\", \"key\": \"agents/$tech/conversation\", \"list\": {\"Listed\": [{\"Account\": $chief}]}}}"

echo "founder $founder chief $chief tech $tech"
