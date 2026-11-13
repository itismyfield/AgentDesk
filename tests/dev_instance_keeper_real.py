"""Explicit real-resource caller acceptance; only synthetic Gateway credentials."""

import hashlib
import json
import os
from pathlib import Path
import socket
import shutil
import subprocess
import sys
import time
import unittest
import uuid

from scripts.dev_instance import keeper, lease
from scripts.dev_instance.files import InstanceError
from scripts.dev_instance.manifest import prepare
from tests.test_dev_instance import profile
from tests.test_dev_instance_lifecycle import ResourceFixture

REPO = Path(__file__).resolve().parents[1]


class RealKeeperTests(ResourceFixture):
    def setUp(self):
        super().setUp()
        for _ in range(64):
            self.home = Path("/private/tmp") / ("k" + uuid.uuid4().hex[:4])
            try:
                self.home.mkdir(mode=0o700)
                break
            except FileExistsError:
                continue
        else:
            self.fail("no fresh private HOME available")
        self.addCleanup(shutil.rmtree, self.home)
        self.base = self.home / ".adk/dev/instances"
        self.base.mkdir(mode=0o700, parents=True)
        self.manifest = prepare(self.base, self.instance_id, self.worktree, profile(), {})
        self.root = Path(self.manifest["root"])
        os.utime(self.root / "herdr-home/config.toml", (time.time()-3, time.time()-3))
        for field in ("postgres_bin", "initdb_bin", "psql_bin", "herdr_bin"):
            value = os.environ.get("ADK_TEST_" + field.upper())
            self.assertIsNotNone(value, "explicit private-resource binary required: " + field)
            self.spec[field] = str(Path(value))
        self.binary = self.worktree / "target/debug/agentdesk"
        self.binary.parent.mkdir(parents=True)
        (self.worktree / ".git/info/exclude").write_text("target/\n")
        self.binary.write_text(f'''#!{sys.executable}
import json,os,time
from pathlib import Path
root=Path(os.environ["AGENTDESK_ROOT_DIR"])
w=json.loads(os.environ["AGENTDESK_TEST_LEASE"])
m=os.fstat(w["fd"])
(root/"child-capture.json").write_text(json.dumps({{"pid":os.getpid(),"env":dict(os.environ),"lease_inode":m.st_ino}}))
end=time.monotonic()+60
while not (root/"cooperative-stop").exists() and time.monotonic()<end: time.sleep(.01)
(root/"child-exit.json").write_text(json.dumps({{"pid":os.getpid(),"exited_cooperatively":True}}))
''')
        self.binary.chmod(0o700)
        self.digest = hashlib.sha256(self.binary.read_bytes()).hexdigest()
        self.specpath = self.scratch / "resource-spec.json"
        self.wrapper = None
        self.addCleanup(self.finish_fixture)

    def wait_for(self, check, label, timeout=25):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            try:
                value = check()
                if value:
                    return value
            except (OSError, ValueError, KeyError):
                pass
            time.sleep(.02)
        self.fail("deadline waiting for " + label)

    def record(self):
        return json.loads((self.root / keeper.OWNER_FILE).read_text())

    def run_wrapper(self):
        self.specpath.write_text(json.dumps(self.spec))
        environment = {"PATH": "/usr/bin:/bin", "HOME": str(self.home),
                       "LC_ALL": "C", "DISCORD_TOKEN": "SYNTHETIC_NEVER_FORWARD"}
        self.wrapper = subprocess.Popen([sys.executable, "-E", "-s", "-m", "scripts.dev_instance",
            "up", "--instance", self.instance_id, "--head", self.manifest["head"],
            "--binary-sha256", self.digest, "--wall-timeout", "50", "--stop-grace", "5",
            "--resources", str(self.specpath)], cwd=str(REPO), env=environment,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        return self.wrapper

    def ready(self):
        record = self.wait_for(lambda: self.record() if self.record()["state"] == "RUNNING" else None,
                               "actual keeper RUNNING")
        capture = self.wait_for(lambda: json.loads((self.root / "child-capture.json").read_text()), "Gateway capture")
        self.assertFalse(record["live_ready"])
        self.assertFalse(record["known_resources_closed"])
        self.assertEqual(len(record["resources"]), 2)
        self.assertNotIn("SYNTHETIC_NEVER_FORWARD", json.dumps(capture))
        with self.assertRaises(lease.LeaseBusy):
            lease.acquire(self.root, self.instance_id)
        return record, capture

    def finish_fixture(self):
        (self.root / "cooperative-stop").write_text("cooperative fixture exit\n")
        if self.wrapper is not None:
            try:
                self.wrapper.communicate(timeout=15)
            except subprocess.TimeoutExpired:
                self.wrapper.kill()  # Only this fixture's original wrapper handle.
                self.wrapper.communicate(timeout=10)
        # A lost Gateway handle exits cooperatively; never signal its receipt PID.
        if (self.root / "child-capture.json").exists():
            self.wait_for(lambda: self._lease_free(), "last Gateway holder close", timeout=10)
        if (self.root / keeper.OWNER_FILE).exists():
            self.wait_for(lambda: self.record().get("known_resources_closed"), "owned resource exit", timeout=15)

    def _lease_free(self):
        try:
            with lease.acquire(self.root, self.instance_id):
                return True
        except lease.LeaseBusy:
            return False

    def report(self, scenario, result):
        print("KEEPER_ACTUAL " + json.dumps({"scenario": scenario, "result": result}, sort_keys=True), flush=True)

    def test_actual_caller_down_reaps_in_reverse_order_and_refuses_foreign_nonce(self):
        self.run_wrapper()
        record, _ = self.ready()
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(5)
            client.connect(str(self.root / keeper.SOCKET_FILE))
            keeper.send(client, {"root": str(self.root), "nonce": str(uuid.uuid4()), "action": "down"})
            self.assertEqual(keeper.receive(client)["state"], "HOLD")
        self.assertEqual(self.record()["state"], "RUNNING")
        frame = {"root": str(self.root), "nonce": record["nonce"], "action": "down"}
        stdout, stderr = self.wrapper.communicate(input=json.dumps(frame)+"\n", timeout=10)
        result = json.loads(stdout)
        self.assertEqual(self.wrapper.returncode, 0, stderr)
        self.assertEqual(result["state"], "STOPPED")
        self.assertTrue(result["gateway_exit_confirmed"])
        self.assertTrue(result["known_resources_closed"])
        self.assertEqual(result["close_order"], ["herdr", "postgres"])
        self.assertFalse(result["live_ready"])
        self.assertTrue(all(item["exitcode"] is not None for item in result["resources"]))
        self.assertFalse((self.base / keeper.MARKER_NAME).exists())
        self.assertTrue((self.base / keeper.LOCK_NAME).exists())
        self.assertEqual(json.loads(stdout)["status"], "STOPPED")
        self.report("normal-caller-down", result)

    def test_wrapper_sigkill_is_hold_without_gateway_guess_or_credential_delete(self):
        credential = self.root / "credential/claude_bot_token"
        credential.write_text("SYNTHETIC_PRIVATE_CREDENTIAL\n")
        credential.chmod(0o600)
        self.run_wrapper()
        record, capture = self.ready()
        self.wrapper.kill()
        self.wrapper.communicate(timeout=5)
        held = self.wait_for(lambda: self.record() if self.record()["state"] == "HOLD"
                             and self.record()["known_resources_closed"] else None, "keeper visible HOLD")
        self.assertFalse(held["gateway_exit_confirmed"])
        self.assertFalse(held["live_ready"])
        self.assertEqual(credential.read_text(), "SYNTHETIC_PRIVATE_CREDENTIAL\n")
        self.assertFalse((self.root / "child-exit.json").exists())
        with self.assertRaises(lease.LeaseBusy):
            lease.acquire(self.root, self.instance_id)
        (self.root / "cooperative-stop").write_text("fixture self exit")
        exited = self.wait_for(lambda: json.loads((self.root / "child-exit.json").read_text()), "cooperative Gateway exit")
        self.assertEqual(exited["pid"], capture["pid"])
        self.wait_for(self._lease_free, "independent reacquire")
        self.assertEqual(held["close_order"], ["herdr", "postgres"])
        self.report("wrapper-sigkill-hold", held)

    def test_foreign_tcp_listener_prevents_sql_and_resource_adoption(self):
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        listener.bind(("127.0.0.1", self.manifest["postgres"]["port"]))
        listener.listen(4)
        listener.settimeout(.1)
        owner = self.owner()
        try:
            with self.assertRaises(InstanceError):
                owner.start()
            self.assertTrue(all(child.poll() is not None for child in owner.items))
            self.assertEqual([entry["kind"] for entry in owner.witness()["commands"]], ["initdb"])
            with self.assertRaises(socket.timeout):
                listener.accept()
            self.assertFalse(owner.witness()["known_resources_closed"])
        finally:
            listener.close()
        self.report("foreign-tcp-no-sql", owner.witness())

    def test_actual_caller_partial_start_failure_rolls_back_before_gateway(self):
        self.spec["herdr_bin"] = str(self.scratch / "fake-resource")
        self.run_wrapper()
        stdout, stderr = self.wrapper.communicate(timeout=25)
        self.assertEqual(self.wrapper.returncode, 6, stderr)
        self.assertEqual(json.loads(stdout)["status"], "HOLD")
        record = self.record()
        self.assertFalse(record["live_ready"])
        self.assertTrue(record["known_resources_closed"])
        self.assertTrue(all(item["exitcode"] is not None for item in record["resources"]))
        self.assertEqual(record["close_order"], ["herdr", "postgres"])
        self.assertFalse((self.root / "child-capture.json").exists())
        self.assertFalse((self.base / keeper.MARKER_NAME).exists())
        self.report("caller-partial-start-rollback", record)

    def test_actual_caller_reap_uses_same_current_run_and_waits_for_exit(self):
        self.run_wrapper()
        record, _ = self.ready()
        frame = {"root": str(self.root), "nonce": record["nonce"], "action": "reap"}
        stdout, stderr = self.wrapper.communicate(input=json.dumps(frame)+"\n", timeout=10)
        self.assertEqual(self.wrapper.returncode, 0, stderr)
        result = json.loads(stdout)
        self.assertEqual(result["status"], "STOPPED")
        self.assertEqual(result["close_order"], ["herdr", "postgres"])
        self.assertTrue(result["known_resources_closed"])
        self.report("normal-caller-reap", result)

    def test_replaced_global_marker_never_reports_successful_down(self):
        self.run_wrapper()
        record, _ = self.ready()
        marker = self.base / keeper.MARKER_NAME
        marker.rename(self.base / "original-owned-marker")
        marker.write_text("FOREIGN_REPLACEMENT_PRESERVED")
        marker.chmod(0o600)
        frame = {"root": str(self.root), "nonce": record["nonce"], "action": "down"}
        stdout, stderr = self.wrapper.communicate(input=json.dumps(frame)+"\n", timeout=10)
        self.assertEqual(self.wrapper.returncode, 6, stderr)
        self.assertEqual(json.loads(stdout)["status"], "HOLD")
        self.assertEqual(marker.read_text(), "FOREIGN_REPLACEMENT_PRESERVED")
        self.assertEqual(self.record()["state"], "HOLD")
        self.assertTrue(self.record()["known_resources_closed"])
        self.report("finalization-refusal-hold", self.record())

    def test_actual_live_resources_and_lost_lock_marker_refuse_another_keeper(self):
        owner_lock = keeper.ResourceLock(self.root, str(uuid.uuid4()))
        owner = self.owner().start()
        try:
            with self.assertRaises(InstanceError):
                keeper.ResourceLock(self.root, str(uuid.uuid4()))
            owner_lock.close()  # Controlled kernel-FD loss; original resource handles retained by fixture.
            with self.assertRaises(InstanceError):
                keeper.ResourceLock(self.root, str(uuid.uuid4()))
            self.assertTrue(all(child.poll() is None for child in owner.items))
            self.assertEqual(len(owner.items), 2)
        finally:
            owner.close(time.monotonic() + 10)
            owner_lock.close()
        self.assertTrue(owner.witness()["known_resources_closed"])
        self.report("old-live-resources-marker-refusal", owner.witness())


if __name__ == "__main__":
    unittest.main()
