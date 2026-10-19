#!/bin/sh
# One run of the CronJob registry/registry-gc: once rlm-home is as full as the kubelet's image-GC line and the store holds blobs no manifest references, it sets Deployment registry read-only, collects them and sets it back.
# Built into the image by ops/k8s/registry-gc/deploy; reads GC_USED_PERCENT and the registry's claim at GC_STORE, which it locks so that one run acts at a time; ops/k8s/registry-gc/gc-test.py covers it.
set -eu

store=${GC_STORE:?GC_STORE is unset}
config=/etc/distribution/config.yml
deployment=deployment/registry
read_only_env=REGISTRY_STORAGE_MAINTENANCE_READONLY
line=${GC_USED_PERCENT:?GC_USED_PERCENT is unset}
work=$(mktemp -d)

say() { echo "registry-gc: event=$*"; }

whole_number() {
  case $1 in
    '' | *[!0-9]*) return 1 ;;
  esac
}

used_percent() {
  set -- $(stat -f -c '%a %b' "$store")
  echo $((100 - $1 * 100 / $2))
}

store_bytes() {
  set -- $(du -sk "$store")
  echo $(($1 * 1024))
}

blob_bytes() {
  total=0
  for digest in $(sed -n 's/^blob eligible for deletion: sha256://p' "$1"); do
    prefix=$(echo "$digest" | cut -c1-2)
    total=$((total + $(stat -c %s "$store/docker/registry/v2/blobs/sha256/$prefix/$digest/data")))
  done
  echo "$total"
}

eligible_blobs() {
  sed -n 's/^[0-9]* blobs marked, \([0-9]*\) blobs and [0-9]* manifests eligible for deletion$/\1/p' "$1"
}

set_registry_env() {
  kubectl patch "$deployment" --patch "$(printf '{"spec":{"template":{"spec":{"containers":[{"name":"registry","env":[%s]}]}}}}' "$1")"
  kubectl rollout status "$deployment"
}

read_only_on() {
  set_registry_env "{\"name\":\"$read_only_env\",\"value\":\"{\\\"enabled\\\":true}\"}"
  say "read_only_on"
}

read_only_off() {
  set_registry_env "{\"name\":\"$read_only_env\",\"\$patch\":\"delete\"}"
  say "read_only_off"
}

if ! whole_number "$line"; then
  say "line_unread line_percent=$line"
  exit 1
fi

exec 9< "$store"
if ! flock -n 9; then
  say "another_run"
  exit 0
fi

left_on=$(kubectl get "$deployment" -o jsonpath="{.spec.template.spec.containers[?(@.name==\"registry\")].env[?(@.name==\"$read_only_env\")].name}")
if [ -n "$left_on" ]; then
  say "read_only_left_on"
  read_only_off
fi

used=$(used_percent)
if [ "$used" -lt "$line" ]; then
  say "under_line used_percent=$used line_percent=$line"
  exit 0
fi

registry garbage-collect --dry-run "$config" > "$work/dry-run"
blobs=$(eligible_blobs "$work/dry-run")
if ! whole_number "$blobs"; then
  say "dry_run_unread"
  exit 1
fi
if [ "$blobs" -eq 0 ]; then
  say "nothing_to_collect used_percent=$used line_percent=$line"
  exit 0
fi
bytes=$(blob_bytes "$work/dry-run")
say "collecting used_percent=$used line_percent=$line blobs=$blobs blob_bytes=$bytes"

trap read_only_off EXIT
trap 'exit 143' TERM
read_only_on
store_before=$(store_bytes)
registry garbage-collect "$config" > "$work/collect"
store_after=$(store_bytes)
collected=$(eligible_blobs "$work/collect")
trap - EXIT TERM
read_only_off
say "collected blobs=$collected freed_bytes=$((store_before - store_after)) used_percent=$(used_percent)"
