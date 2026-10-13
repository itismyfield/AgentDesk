"""Independent input contracts; fixture expectations never come from oracle output."""
import copy
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
ENTRY = ROOT / "scripts/n1_turn_canary_oracle.py"
PACKAGE = ROOT / "scripts/n1_canary"
CONTRACT = ROOT / "tests/fixtures/n1_canary_o1/contract.json"
BOOT = "11111111-1111-4111-8111-111111111111"
COUNTERS = {key: 0 for key in ("closed_events", "aborted_events", "unknown_outcome_events",
            "create_attempt", "create_committed", "attempt", "succeeded_post",
            "succeeded_patch", "failed_or_uncertain")}
EXPECTED_REASONS = {"unknown_outcome_unproduced", "confirmation_boundary_unknown", "stages_pending:o3",
                    "native_origin_unverified", "native_sample_under_50", "audit_capture_incomplete:legacy",
                    "audit_capture_incomplete:inflight", "o_init_unverified"}


def digest(data):
    return hashlib.sha256(data).hexdigest()


def bundle_digest():
    paths = [ENTRY, *sorted(PACKAGE.glob("*.py")), PACKAGE / "compatibility.json"]
    packed = b"".join(p.name.encode() + b"\0" + p.read_bytes() + b"\0" for p in paths)
    return digest(packed)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def snapshot(**changes):
    value = dict(status="available", schema_version=2, boot_id=BOOT, channel_id=123,
                 seq_high_water=1, counters=COUNTERS.copy(), dropped_events=0, sink_errors=0,
                 observer_unhealthy=False, open_attempts=0, open_attempts_complete=True,
                 first_confirmation={"seq": 0, "at_us": 10}, confirmation_observed=True)
    value.update(changes)
    return value


class Fixture(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="n1-oracle-test-")
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        self.manifest = json.loads(CONTRACT.read_text())
        self.path = self.base / "manifest.json"
        source = self.manifest["scope"]["source"]
        identity = self.manifest["binding"]["identity"]
        self.values = {
            "executable": b"fixture executable, never run",
            "transcript": b'{"type":"session_meta","payload":{"id":"native-session"}}\n'
                          b'{"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}\n'
                          b'{"type":"response_item","payload":{"type":"message","role":"assistant","id":"b1","content":[{"type":"output_text","text":"fixture"}]}}\n'
                          b'{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1"}}\n',
            "baseline": snapshot(), "end_snapshot": snapshot(),
            "log": [{"event": "n1_mode_confirmed", "boot_id": BOOT, "channel_id": 123,
                     "provider": "codex", "seq": 1, "schema_version": 2, "observed_at_us": 20,
                     "confirmation_window_start": {"seq": 0, "at_us": 10},
                     "actual_confirmed_at_us": None,
                     "boundary_rule": "overlap_or_missing_boundary_is_unknown;never_shrink_first_window"}],
            "bindings": [dict(seq=n, boot_id=BOOT, channel_id=123, provider="codex",
                              status="resolved", source=source, identity=identity) for n in (1, 2, 3)],
            "o_init": {}, "o_ledger": [], "legacy": {}, "inflight": [], "pending": [],
            "abort": [], "debt": {"undelivered": [], "abandoned": []},
            "pg": {"intake_outbox": [], "turn_lifecycle_events": []},
            "discord": {"complete": True, "reasons": [], "channels": [123],
                        "lower_inclusive": "100", "upper_exclusive": "500", "pages": 1,
                        "page_boundaries": [{"channel_id": 123, "before": "500", "ids": []}],
                        "messages": [], "individual": {"200": {"id": "200", "channel_id": "123", "author": {"id": "456"}}}},
        }
        exe_hash = digest(self.values["executable"])
        self.manifest["runtime"]["executable_sha256"] = exe_hash
        self.values["process_proof"] = dict(self.manifest["runtime"], executable_path="/proc/42/exe")
        self.manifest["scope"].update(fiftieth_offset=len(self.values["transcript"]), closure_offset=len(self.values["transcript"]))
        self.manifest["source_digests"] = {"prefix_sha256": digest(b""), "window_sha256": digest(self.values["transcript"])}
        if ENTRY.exists():
            self.manifest["oracle"]["bundle_sha256"] = bundle_digest()
        self.save()

    def save(self):
        for name, value in self.values.items():
            path = self.base / self.manifest["files"][name]["path"]
            path.parent.mkdir(parents=True, exist_ok=True)
            raw = value if isinstance(value, bytes) else (
                b"".join(canonical(row) + b"\n" for row in value)
                if name in ("log", "bindings", "o_ledger") else canonical(value))
            path.write_bytes(raw)
            self.manifest["files"][name]["sha256"] = digest(raw)
        self.path.write_bytes(canonical(self.manifest))

    def cli(self, *args):
        self.assertTrue(ENTRY.is_file(), "required CLI entry is missing")
        result = subprocess.run([sys.executable, str(ENTRY), "--manifest", str(self.path), *args],
                                capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        output = json.loads(result.stdout)
        self.assertEqual(output["verdict"], "[U]")
        self.assertFalse(output["evidence_complete"])
        return output

    def test_valid_inputs_still_cannot_pass(self):
        output = self.cli()
        self.assertEqual(set(output["reasons"]), EXPECTED_REASONS)
        self.assertIsNone(output["observation"]["unknown_outcome_events"])
        self.assertEqual(output["observation"]["open_placeholder_attempts"], 0)
        self.assertEqual(output["confirmation"]["event_observed_at_us"], 20)
        self.assertIsNone(output["confirmation"]["actual_confirmed_at_us"])

    def test_unknown_missing_manifest_fields_and_versions(self):
        cases = [((), "manifest_version", 99), (("runtime",), "extra", 1),
                 (("policies",), "native_identity", 99), (("oracle",), "version", "later"),
                 (("policies",), "provider_format", "claude-jsonl-v1"),
                 (("oracle",), "bundle_sha256", "0" * 64)]
        original = copy.deepcopy(self.manifest)
        for parents, key, value in cases:
            with self.subTest(parents=parents, key=key):
                self.manifest = copy.deepcopy(original)
                node = self.manifest
                for parent in parents:
                    node = node[parent]
                node[key] = value
                self.save()
                self.assertIn("manifest_invalid", self.cli()["reasons"])
        self.manifest = original
        del self.manifest["scope"]["source"]["ino"]
        self.save()
        self.assertIn("manifest_invalid", self.cli()["reasons"])

    def test_unavailable_is_not_zero(self):
        self.values["end_snapshot"] = {"status": "unavailable"}
        self.save()
        output = self.cli()
        self.assertIn("snapshot_unavailable", output["reasons"])
        self.assertIsNone(output["observation"]["open_placeholder_attempts"])
        self.assertIsNone(output["observation"]["dropped_events"])

    def test_drop_marker_is_not_ignored(self):
        self.values["end_snapshot"] = snapshot(dropped_events=1)
        self.save()
        output = self.cli()
        self.assertIn("observation_unhealthy", output["reasons"])
        self.assertIsNone(output["observation"]["open_placeholder_attempts"])

    def test_incomplete_open_attempts_is_not_a_number(self):
        self.values["end_snapshot"] = snapshot(open_attempts=7, open_attempts_complete=False)
        self.save()
        output = self.cli()
        self.assertIn("open_attempts_incomplete", output["reasons"])
        self.assertIsNone(output["observation"]["open_placeholder_attempts"])

    def test_event_time_cannot_replace_actual_confirmation(self):
        self.manifest["observation"]["actual_confirmed_at_us"] = 20
        self.save()
        self.assertIn("confirmation_boundary_unknown", self.cli()["reasons"])

    def test_actual_confirmation_must_be_inside_event_window(self):
        self.values["log"][0]["actual_confirmed_at_us"] = 9
        self.manifest["observation"]["actual_confirmed_at_us"] = 9
        self.save()
        self.assertIn("confirmation_boundary_unknown", self.cli()["reasons"])

    def test_source_and_binding_a_b_a_cannot_be_hidden_by_endpoints(self):
        for field in ("source", "identity", "status", "boot_id"):
            with self.subTest(field=field):
                original = copy.deepcopy(self.values["bindings"])
                if field == "source":
                    self.values["bindings"][1][field] = {**original[1][field], "ino": 999}
                elif field == "identity":
                    self.values["bindings"][1][field] = {**original[1][field], "execution_nonce": "run-2"}
                else:
                    self.values["bindings"][1][field] = "pending" if field == "status" else "different-boot"
                self.save()
                self.assertIn("binding_discontinuous", self.cli()["reasons"])
                self.values["bindings"] = original

    def test_missing_corrupt_and_truncated_files(self):
        for name, raw in (("legacy", b"broken"), ("log", b"{}"), ("pg", b"null"), ("discord", b"null")):
            with self.subTest(name=name):
                original = self.values[name]
                self.values[name] = raw
                self.save()
                self.assertTrue(any("invalid" in x or "unavailable" in x for x in self.cli()["reasons"]))
                self.values[name] = original
        self.save()
        (self.base / self.manifest["files"]["debt"]["path"]).unlink()
        self.assertIn("file_unavailable:debt", self.cli()["reasons"])

    def test_digest_and_build_proof_cannot_fall_back_to_init(self):
        self.values["process_proof"]["build_commit"] = "0" * 40
        self.save()
        self.assertIn("build_identity_unverified", self.cli()["reasons"])
        self.values["process_proof"] = dict(self.manifest["runtime"], executable_path="/proc/42/exe")
        self.save()
        (self.base / "runtime/native.jsonl").write_bytes(b"changed")
        self.assertIn("file_digest_mismatch:transcript", self.cli()["reasons"])

    def test_help_parse_errors_and_missing_manifest_never_return_zero(self):
        self.assertTrue(ENTRY.is_file(), "required CLI entry is missing")
        for args in (["--help"], [], ["--unknown"], ["--manifest", "/nonexistent"]):
            result = subprocess.run([sys.executable, str(ENTRY), *args], capture_output=True, timeout=20)
            self.assertEqual(result.returncode, 2, result.stdout)

    def test_output_is_exclusive_and_outside_evidence_stores(self):
        output = self.base / "result.json"
        self.cli("--output", str(output))
        first = output.read_bytes()
        self.assertIn("output_unavailable", self.cli("--output", str(output))["reasons"])
        self.assertEqual(output.read_bytes(), first)
        target = self.base / "runtime/result.json"
        self.assertIn("output_unavailable", self.cli("--output", str(target))["reasons"])
        self.assertFalse(target.exists())

    def test_pg_failure_is_unknown_and_secret_is_not_output(self):
        bin_dir = self.base / "fixture-bin"
        bin_dir.mkdir()
        standin = bin_dir / "psql"
        standin.write_text("#!/bin/sh\nprintf 'secret-do-not-print' >&2\nexit 1\n")
        standin.chmod(0o755)
        with patch.dict(os.environ, {"N1_TEST_DSN": "secret-do-not-print", "PATH": str(bin_dir)}):
            output = self.cli("--pg-dsn-env", "N1_TEST_DSN")
        self.assertIn("pg_unavailable", output["reasons"])
        self.assertNotIn("secret-do-not-print", json.dumps(output))


if __name__ == "__main__":
    unittest.main()
