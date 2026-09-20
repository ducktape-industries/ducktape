# Fresh-install CI verification

The workflow and script are 92 lines total (`wc -l .github/workflows/fresh-install.yml ops/ci/fresh-install.sh`: 0).

Commands and exit codes:

- `bash -n ops/ci/fresh-install.sh` — 0.
- `shellcheck ops/ci/fresh-install.sh` — 0.
- `command -v actionlint` — 1; not installed, so actionlint was skipped.
- `git diff --check` — 0.
- `bash -o pipefail -c 'tar --exclude=./.git --exclude=./target -cf - . | podman run --rm -i docker.io/library/debian:trixie bash -lc "mkdir -p /workspace && tar -xf - -C /workspace && cd /workspace && bash ops/ci/fresh-install.sh"'` — 2.

The container installed the README Linux prerequisites, then stopped at the
expected current-`dev` dependency: `make: *** No rule to make target 'install'.
Stop.` No host build ran, so no `CARGO_TARGET_DIR` or target directory needed
cleanup. The container had no host mounts.
