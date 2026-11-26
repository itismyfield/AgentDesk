"""Kernel lease status and recovery without a production runner."""

import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from scripts.e2e.continuous import __main__ as cli, channel_lock


RESOURCE = {"guild": "1", "parents": ["20", "30"], "bot_set": ["4"]}


class ChannelLockStatus(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.state = self.root / "state"
        self.config = self.root / "config.json"
        self.config.write_text(json.dumps({"state_dir": str(self.state), "resource": RESOURCE}))
        self.path = self.state / "locks" / f"{channel_lock.resource_key(RESOURCE)}.lock"

    def inspect(self, expected, exit_code=0):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            try:
                code = cli.main(["--config", str(self.config), "lease-status"])
            except SystemExit as error:
                self.fail(f"lease-status rejected by parser: {error}")
        self.assertEqual(code, exit_code)
        self.assertEqual(json.loads(out.getvalue())["status"], expected)

    def test_dead_process_metadata_does_not_hide_reacquirable_lock(self):
        subprocess.run([sys.executable, "-c", """
import json, os, sys
from scripts.e2e.continuous import channel_lock
held = channel_lock.acquire(sys.argv[1], json.loads(sys.argv[2]))
held.publish_owner({'pid': os.getpid(), 'kind': 'fixture'})
os._exit(0)
""", str(self.state), json.dumps(RESOURCE)], check=True, timeout=10)
        raw, inode = self.path.read_bytes(), self.path.stat().st_ino
        self.assertGreater(json.loads(raw)["pid"], 0)
        self.inspect("FREE")
        with channel_lock.acquire(self.state, RESOURCE) as recovered:
            self.assertEqual(recovered.identity.st_ino, inode)
            self.assertEqual(self.path.read_bytes(), raw)
            self.inspect("HELD")

    def test_actual_holder_blocks_with_live_remote_or_missing_pid_evidence(self):
        with channel_lock.acquire(self.state, RESOURCE) as held:
            for owner in ({"pid": os.getpid()}, {"pid": 99049, "host": "different-host"}, {}):
                with self.subTest(owner=owner):
                    held.publish_owner(owner)
                    raw = self.path.read_bytes()
                    self.inspect("HELD")
                    with self.assertRaises(channel_lock.LeaseBusy):
                        channel_lock.acquire(self.state, RESOURCE)
                    self.assertEqual(self.path.read_bytes(), raw)

    def test_unreadable_flock_is_rejected_and_absent_state_is_not_created(self):
        self.inspect("FREE")
        self.assertFalse(self.state.exists())
        with channel_lock.acquire(self.state, RESOURCE):
            raw = self.path.read_bytes()
        with mock.patch.object(channel_lock.fcntl, "flock", side_effect=PermissionError("fixture denied")):
            self.inspect("REJECTED", exit_code=2)
            with self.assertRaises(PermissionError):
                channel_lock.acquire(self.state, RESOURCE)
        self.assertEqual(self.path.read_bytes(), raw)
