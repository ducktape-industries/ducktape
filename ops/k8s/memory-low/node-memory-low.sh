#!/usr/bin/env bash
# Holds memory.low on a network's node pod and its parent slices on the cluster host, again as each node pod comes up and each time the kubelet starts.
# Usage: node-memory-low.sh install|check NETWORK BYTES KUBECONFIG | node-memory-low.sh watch NETWORK BYTES
set -euo pipefail

slices() {
  local uid=${1//-/_} qos=$2 class
  case $qos in
    Guaranteed)
      echo "kubepods.slice kubepods-pod$uid.slice"
      ;;
    Burstable | BestEffort)
      class=$(echo "$qos" | tr '[:upper:]' '[:lower:]')
      echo "kubepods.slice kubepods-$class.slice kubepods-$class-pod$uid.slice"
      ;;
    *)
      echo "node-memory-low: pod $1 names no QoS class ($qos)" >&2
      return 1
      ;;
  esac
}

whole_pages() {
  local page
  page=$(getconf PAGESIZE)
  echo $(($1 / page * page))
}

holds() {
  local want=$1 slice held
  shift
  for slice in "$@"; do
    held=$(cat "/sys/fs/cgroup$(systemctl show -p ControlGroup --value "$slice")/memory.low")
    if [ "$held" != "$want" ]; then
      echo "node-memory-low: $slice holds memory.low $held, not $want" >&2
      return 1
    fi
    echo "node-memory-low: $slice holds memory.low $held"
  done
}

hold() {
  local low=$1 slice
  shift
  for slice in "$@"; do
    if ! systemctl set-property --runtime "$slice" "MemoryLow=$low"; then
      echo "node-memory-low: $slice took no MemoryLow" >&2
      return 1
    fi
  done
  holds "$(whole_pages "$low")" "$@"
}

hold_pod() {
  local low=$1 line
  local -a held
  line=$(slices "$2" "$3") || return 1
  read -r -a held <<< "$line"
  hold "$low" "${held[@]}"
}

running_pod() {
  kubectl -n "$1" get pods -l app=node --field-selector=status.phase=Running \
    -o jsonpath='{.items[0].metadata.uid} {.items[0].status.qosClass}'
}

hold_running_pod() {
  local network=$1 low=$2 uid qos
  read -r uid qos <<< "$(running_pod "$network")"
  if [ -z "$uid" ]; then return 0; fi
  hold_pod "$low" "$uid" "$qos"
}

running_pod_slices() {
  local network=$1 uid qos
  read -r uid qos <<< "$(running_pod "$network")"
  if [ -z "$uid" ]; then
    echo "node-memory-low: no node pod of $network runs" >&2
    return 1
  fi
  slices "$uid" "$qos"
}

install_here() {
  local network=$1 low=$2 kubeconfig=$3 here parent line
  local -a held
  here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
  export KUBECONFIG=$kubeconfig
  line=$(running_pod_slices "$network") || return 1
  read -r -a held <<< "$line"
  for parent in "${held[@]:0:${#held[@]}-1}"; do
    mkdir -p "/etc/systemd/system/$parent.d"
    printf '[Slice]\nMemoryLow=%s\n' "$low" > "/etc/systemd/system/$parent.d/50-memory-low.conf"
  done
  install -D -m 755 "$here/node-memory-low.sh" /usr/local/libexec/node-memory-low.sh
  sed -e "s#NETWORK#$network#g" -e "s#BYTES#$low#g" -e "s#KUBECONFIG_PATH#$kubeconfig#g" \
    "$here/node-memory-low.service" > /etc/systemd/system/node-memory-low.service
  systemctl daemon-reload
  hold "$low" "${held[@]}"
  systemctl enable node-memory-low.service
  systemctl restart node-memory-low.service
}

check_here() {
  local network=$1 low=$2 line
  local -a held
  export KUBECONFIG=$3
  line=$(running_pod_slices "$network") || return 1
  read -r -a held <<< "$line"
  holds "$(whole_pages "$low")" "${held[@]}"
}

watch_here() {
  local network=$1 low=$2
  kubectl -n default get events --watch \
    --field-selector involvedObject.kind=Node,reason=NodeAllocatableEnforced -o jsonpath='{.metadata.name}{"\n"}' |
    while read -r _; do hold_running_pod "$network" "$low" || continue; done &
  kubectl -n "$network" get pods -l app=node --watch \
    -o jsonpath='{.metadata.uid} {.status.qosClass} {.status.phase}{"\n"}' |
    while read -r uid qos phase; do
      if [ "$phase" != Running ]; then continue; fi
      hold_pod "$low" "$uid" "$qos" || continue
    done &
  wait -n
}

case ${1:-} in
  install)
    shift
    install_here "$@"
    ;;
  check)
    shift
    check_here "$@"
    ;;
  watch)
    shift
    watch_here "$@"
    ;;
  *)
    echo "usage: node-memory-low.sh install|check NETWORK BYTES KUBECONFIG | node-memory-low.sh watch NETWORK BYTES" >&2
    exit 1
    ;;
esac
