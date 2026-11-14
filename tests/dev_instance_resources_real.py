"""Explicitly selected installed PG/Herdr witnesses; no default discovery or skip."""

import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import time
import unittest
from unittest import mock

from scripts.dev_instance import host, resources
from scripts.dev_instance.files import InstanceError
from tests.test_dev_instance_lifecycle import ResourceFixture


class RealResourceTests(ResourceFixture):
    def setUp(self):
        super().setUp()
        self.spec = self.real_spec()
        self.spawned = []
        original = subprocess.Popen
        def capture(*args, **kwargs):
            child = original(*args, **kwargs)
            self.spawned.append((args[0], child))
            return child
        self.capture = mock.patch.object(subprocess, "Popen", side_effect=capture)
        self.capture.start()
        self.addCleanup(self.capture.stop)
        self.addCleanup(self.stop_actual_children)

    def stop_actual_children(self):
        cleanup_signals = []
        for command, child in reversed(self.spawned):
            if child.poll() is None:
                requested = signal.SIGINT if command[0] == self.spec["postgres_bin"] else signal.SIGTERM
                cleanup_signals.append({"pid": child.pid, "signal": int(requested)})
                child.send_signal(requested)
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    cleanup_signals.append({"pid": child.pid, "signal": int(signal.SIGKILL)})
                    child.kill()
                    child.wait(timeout=5)
        print("RESOURCE_TEST_TEARDOWN " + json.dumps({"fallback_signals": cleanup_signals}), flush=True)

    def report(self, owner, scenario):
        print("RESOURCE_ACTUAL " + json.dumps({"scenario": scenario, "witness": owner.witness()}, sort_keys=True), flush=True)

    def test_actual_private_postgres_and_herdr_close(self):
        owner = self.owner().start()
        self.assertEqual(len(owner.items), 2, "both foreground resources must actually start")
        self.assertTrue(all(child.poll() is None for child in owner.items))
        witness = owner.witness()
        pg = self.manifest["postgres"]
        self.assertEqual(witness["postgres"]["data_directory"], pg["data_dir"])
        self.assertEqual(witness["postgres"]["port"], pg["port"])
        self.assertEqual(witness["postgres"]["user"], pg["role"])
        self.assertEqual(witness["postgres"]["database"], pg["database"])
        self.assertEqual(witness["postgres"]["peer_pid"], owner.items[0].pid)
        self.assertEqual(witness["herdr"]["peer_pid"], owner.items[1].pid)
        alias = self.root / (".s.PGSQL." + str(pg["port"]))
        original = Path(pg["socket_dir"]) / alias.name
        self.assertEqual(alias.stat().st_ino, original.stat().st_ino)
        sql_commands = [command for command, _ in self.spawned if command[0] == self.spec["psql_bin"]]
        self.assertTrue(sql_commands)
        self.assertTrue(all(command[command.index("-h")+1] == str(self.root) for command in sql_commands),
                        "actual SQL must use the verified private Unix socket, never TCP")
        self.assertTrue(owner.close(time.monotonic() + 10))
        self.assertTrue(all(child.poll() is not None for child in owner.items))
        self.assertEqual(owner.witness()["close_order"], ["herdr", "postgres"])
        self.assertTrue(owner.close(time.monotonic() + 1))
        self.report(owner, "private-pg-herdr-up-down")

    def test_herdr_start_failure_rolls_back_actual_postgres(self):
        owner = self.owner()
        with mock.patch.object(host, "start_herdr", side_effect=InstanceError("synthetic Herdr start failure")):
            with self.assertRaises(InstanceError):
                owner.start()
        self.assertEqual(len(owner.items), 1)
        self.assertIsNotNone(owner.items[0].poll(), "rollback stack must retain and stop its PG handle")
        self.assertTrue(owner.witness()["known_resources_closed"])
        self.assertEqual(owner.witness()["close_order"], ["postgres"])
        self.report(owner, "partial-start-reverse-rollback")

    def test_herdr_publication_failure_preserves_spawn_before_publication(self):
        owner = self.owner()
        with mock.patch.object(host, "_publish_owner", side_effect=InstanceError("synthetic publication failure")):
            with self.assertRaises(InstanceError):
                owner.start()
        self.assertEqual(len(owner.items), 2)
        self.assertTrue(all(child.poll() is not None for child in owner.items), "all original handles must be reaped")
        self.assertEqual(owner.witness()["close_order"], ["herdr", "postgres"])
        self.report(owner, "spawn-before-publication-rollback")

    def test_foreign_herdr_receipt_holds_without_signalling(self):
        owner = self.owner().start()
        path = self.root / host.OWNER_FILE
        original = path.read_bytes()
        record = json.loads(original)
        record["daemon"]["pid"] += 1
        path.write_text(json.dumps(record))
        try:
            with self.assertRaises(resources.CleanupPending):
                owner.close(time.monotonic() + 1)
            self.assertTrue(all(child.poll() is None for child in owner.items))
            self.assertFalse(owner.witness()["known_resources_closed"])
        finally:
            path.write_bytes(original)
        self.assertTrue(owner.close(time.monotonic() + 10))
        self.report(owner, "foreign-receipt-hold")

    def test_replaced_socket_holds_before_any_daemon_signal(self):
        owner = self.owner().start()
        path = Path(self.manifest["hosts"]["herdr"]["socket"])
        saved = path.with_name("owned-saved.sock")
        path.rename(saved)
        foreign = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        foreign.bind(str(path))
        identity = path.stat().st_ino
        try:
            with self.assertRaises(resources.CleanupPending):
                owner.close(time.monotonic() + 1)
            self.assertTrue(all(child.poll() is None for child in owner.items))
            self.assertEqual(path.stat().st_ino, identity)
        finally:
            foreign.close()
            path.unlink(missing_ok=True)
            saved.rename(path)
        self.assertTrue(owner.close(time.monotonic() + 10))
        self.report(owner, "foreign-socket-hold")


if __name__ == "__main__":
    unittest.main()
