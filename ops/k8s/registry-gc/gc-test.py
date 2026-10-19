"""Exercise gc.sh against stand-in kubectl, registry and stat commands: the line, the dry-run gate, the read-only round trip, failures and a stop after read-only is on, and a registry left read-only; run by `make test`."""
import fcntl
import os
import pathlib
import signal
import subprocess
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
SCRIPT = HERE / "gc.sh"
BLOCKS = 1000
DRY_RUN_EMPTY = "registry\nregistry: marking manifest sha256:aa \n\n3 blobs marked, 0 blobs and 0 manifests eligible for deletion\n"
DRY_RUN_GARBAGE = (
    "registry\n\n3 blobs marked, 2 blobs and 0 manifests eligible for deletion\n"
    "blob eligible for deletion: sha256:" + "1" * 64 + "\n"
    "blob eligible for deletion: sha256:" + "2" * 64 + "\n"
)
READ_ONLY_ON = '{"spec":{"template":{"spec":{"containers":[{"name":"registry","env":[{"name":"REGISTRY_STORAGE_MAINTENANCE_READONLY","value":"{\\"enabled\\":true}"}]}]}}}}'
READ_ONLY_OFF = '{"spec":{"template":{"spec":{"containers":[{"name":"registry","env":[{"name":"REGISTRY_STORAGE_MAINTENANCE_READONLY","$patch":"delete"}]}]}}}}'

KUBECTL = """#!/bin/sh
echo "kubectl $*" >> "$GC_TEST/calls"
case $1 in
  get) cat "$GC_TEST/env-set"; exit "$(cat "$GC_TEST/get-exit")" ;;
  patch) printf '%s\\n' "$4" >> "$GC_TEST/patches"; exit "$(cat "$GC_TEST/patch-exit")" ;;
  rollout) exit "$(cat "$GC_TEST/rollout-exit")" ;;
esac
"""

REGISTRY = """#!/bin/sh
echo "registry $*" >> "$GC_TEST/calls"
case $2 in
  --dry-run) cat "$GC_TEST/dry-run" ;;
  *)
    [ ! -p "$GC_TEST/started" ] || { echo started > "$GC_TEST/started"; cat "$GC_TEST/release" > /dev/null; }
    cat "$GC_TEST/dry-run"
    exit "$(cat "$GC_TEST/collect-exit")"
    ;;
esac
"""

STAT = """#!/bin/sh
case "$1 $2 $3" in
  "-f -c %a %b") echo "$(cat "$GC_TEST/avail") $GC_TEST_BLOCKS" ;;
  "-c %s "*) echo 1000 ;;
esac
"""


DU = """#!/bin/sh
count=$(cat "$GC_TEST/du-count")
echo $((count + 1)) > "$GC_TEST/du-count"
sed -n "$((count + 1))p" "$GC_TEST/du"
"""


class GcScript(unittest.TestCase):
    def setUp(self):
        self.dir = pathlib.Path(tempfile.mkdtemp(prefix="registry-gc-test."))
        bin_dir = self.dir / "bin"
        bin_dir.mkdir()
        self.store = self.dir / "store"
        self.store.mkdir()
        for name, body in (("kubectl", KUBECTL), ("registry", REGISTRY), ("stat", STAT), ("du", DU)):
            path = bin_dir / name
            path.write_text(body)
            path.chmod(0o755)
        self.files(**{"env-set": "", "get-exit": "0", "patch-exit": "0", "rollout-exit": "0", "collect-exit": "0", "dry-run": DRY_RUN_EMPTY, "calls": "", "patches": "", "du-count": "0", "du": "5000\t/var/lib/registry\n3000\t/var/lib/registry\n"})
        self.used(38)
        self.env = {
            **os.environ,
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "GC_TEST": str(self.dir),
            "GC_TEST_BLOCKS": str(BLOCKS),
            "GC_USED_PERCENT": "85",
            "GC_STORE": str(self.store),
            "TMPDIR": str(self.dir),
        }

    def tearDown(self):
        subprocess.run(["rm", "-rf", str(self.dir)], check=True)

    def files(self, **contents):
        for name, text in contents.items():
            (self.dir / name).write_text(text)

    def used(self, percent):
        self.files(avail=str(BLOCKS - percent * BLOCKS // 100))

    def run_gc(self, **env):
        return subprocess.run(["sh", str(SCRIPT)], env={**self.env, **env}, capture_output=True, text=True)

    def calls(self):
        return [line.split()[:3] for line in (self.dir / "calls").read_text().splitlines()]

    def patches(self):
        return (self.dir / "patches").read_text().splitlines()

    def events(self, done):
        return [line.split()[1] for line in done.stdout.splitlines() if line.startswith("registry-gc: event=")]

    def test_under_the_line_it_reads_the_deployment_and_touches_nothing(self):
        done = self.run_gc()
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.events(done), ["event=under_line"])
        self.assertEqual(self.calls(), [["kubectl", "get", "deployment/registry"]])

    def test_at_the_line_with_nothing_to_collect_it_leaves_the_registry_alone(self):
        self.used(85)
        done = self.run_gc()
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.events(done), ["event=nothing_to_collect"])
        self.assertEqual(self.calls(), [["kubectl", "get", "deployment/registry"], ["registry", "garbage-collect", "--dry-run"]])
        self.assertEqual(self.patches(), [])

    def test_with_garbage_it_collects_between_read_only_on_and_off(self):
        self.used(90)
        self.files(**{"dry-run": DRY_RUN_GARBAGE})
        done = self.run_gc()
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.events(done), ["event=collecting", "event=read_only_on", "event=read_only_off", "event=collected"])
        self.assertIn("blobs=2 blob_bytes=2000", done.stdout)
        self.assertIn("collected blobs=2 freed_bytes=2048000", done.stdout)
        self.assertEqual(
            self.calls(),
            [
                ["kubectl", "get", "deployment/registry"],
                ["registry", "garbage-collect", "--dry-run"],
                ["kubectl", "patch", "deployment/registry"],
                ["kubectl", "rollout", "status"],
                ["registry", "garbage-collect", "/etc/distribution/config.yml"],
                ["kubectl", "patch", "deployment/registry"],
                ["kubectl", "rollout", "status"],
            ],
        )
        self.assertEqual(self.patches(), [READ_ONLY_ON, READ_ONLY_OFF])

    def test_a_failed_collection_still_sets_the_registry_back(self):
        self.used(90)
        self.files(**{"dry-run": DRY_RUN_GARBAGE, "collect-exit": "3"})
        done = self.run_gc()
        self.assertNotEqual(done.returncode, 0)
        self.assertEqual(self.patches(), [READ_ONLY_ON, READ_ONLY_OFF])
        self.assertEqual(self.events(done)[-1], "event=read_only_off")

    def test_a_failed_rollout_into_read_only_still_sets_the_registry_back(self):
        self.used(90)
        self.files(**{"dry-run": DRY_RUN_GARBAGE, "rollout-exit": "1"})
        done = self.run_gc()
        self.assertNotEqual(done.returncode, 0)
        self.assertEqual(self.patches(), [READ_ONLY_ON, READ_ONLY_OFF])
        self.assertNotIn(["registry", "garbage-collect", "/etc/distribution/config.yml"], self.calls())

    def test_a_stop_during_collection_sets_the_registry_back_once_collection_ends(self):
        self.used(90)
        self.files(**{"dry-run": DRY_RUN_GARBAGE})
        os.mkfifo(self.dir / "started")
        os.mkfifo(self.dir / "release")
        gc = subprocess.Popen(["sh", str(SCRIPT)], env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        (self.dir / "started").read_text()
        gc.send_signal(signal.SIGTERM)
        (self.dir / "release").write_text("go\n")
        out, err = gc.communicate()
        self.assertEqual(gc.returncode, 143, err)
        self.assertEqual(self.patches(), [READ_ONLY_ON, READ_ONLY_OFF])
        self.assertEqual(self.calls()[-3:], [["registry", "garbage-collect", "/etc/distribution/config.yml"], ["kubectl", "patch", "deployment/registry"], ["kubectl", "rollout", "status"]])

    def test_a_registry_left_read_only_is_set_back_before_anything_else(self):
        self.files(**{"env-set": "REGISTRY_STORAGE_MAINTENANCE_READONLY"})
        done = self.run_gc()
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.events(done), ["event=read_only_left_on", "event=read_only_off", "event=under_line"])
        self.assertEqual(self.patches(), [READ_ONLY_OFF])

    def test_a_deployment_that_does_not_read_stops_it_before_anything_else(self):
        self.used(90)
        self.files(**{"dry-run": DRY_RUN_GARBAGE, "get-exit": "1"})
        done = self.run_gc()
        self.assertNotEqual(done.returncode, 0)
        self.assertEqual(self.calls(), [["kubectl", "get", "deployment/registry"]])

    def test_a_run_while_another_holds_the_store_does_nothing(self):
        self.used(90)
        self.files(**{"dry-run": DRY_RUN_GARBAGE, "env-set": "REGISTRY_STORAGE_MAINTENANCE_READONLY"})
        held = os.open(self.store, os.O_RDONLY)
        try:
            fcntl.flock(held, fcntl.LOCK_EX)
            done = self.run_gc()
        finally:
            os.close(held)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.events(done), ["event=another_run"])
        self.assertEqual(self.calls(), [])

    def test_a_line_that_is_not_a_whole_percent_stops_it(self):
        done = self.run_gc(GC_USED_PERCENT="85%")
        self.assertEqual(done.returncode, 1)
        self.assertEqual(self.events(done), ["event=line_unread"])
        self.assertEqual(self.calls(), [])

    def test_a_dry_run_without_its_summary_stops_it(self):
        self.used(90)
        self.files(**{"dry-run": "registry\n"})
        done = self.run_gc()
        self.assertEqual(done.returncode, 1)
        self.assertEqual(self.events(done), ["event=dry_run_unread"])
        self.assertEqual(self.patches(), [])


if __name__ == "__main__":
    unittest.main()
