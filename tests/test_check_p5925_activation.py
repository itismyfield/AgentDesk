"""Refusal tests for the #5925 legacy restart replay activation validator.

Each negative starts from one complete, approvable survey and applies one realistic gap, then
asserts that the gap is named in the refusal, so a green run means the validator still sees it.
"""

from __future__ import annotations

import copy
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check_p5925_activation.py"
SPEC = importlib.util.spec_from_file_location("check_p5925_activation", SCRIPT)
activation = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(activation)

SHA = "a" * 40
BIN = "b" * 64


def complete_survey():
    return {
        "approved_source_sha": SHA,
        "artifacts": [{"path": "target/release/agentdesk", "sha256": BIN, "source_sha": SHA}],
        "declared_hosts": ["mac-mini"],
        "declared_roots": ["~/.adk/release"],
        "hosts": [{"host": "mac-mini", "surveyed": True}],
        "roots": [
            {
                "declared": "~/.adk/release",
                "realpath": "/Users/op/.adk/release",
                "fs_id": "disk3s5",
                "accessible": True,
            }
        ],
        "writers_stopped": [
            {
                "host": "mac-mini",
                "role": "primary",
                "pid": 101,
                "supervisor_label": "com.agentdesk.release",
                "alive": False,
                "restartable": False,
            }
        ],
        "writers_restarted": [
            {
                "host": "mac-mini",
                "role": "primary",
                "pid": 202,
                "started_at": "2026-10-13T16:00:00+09:00",
                "executable_realpath": "/Users/op/.adk/release/bin/agentdesk",
                "executable_sha256": BIN,
                "root_realpath": "/Users/op/.adk/release",
            }
        ],
        "sources": [
            {
                "channel": "c1",
                "tmux": "AgentDesk-codex-c1",
                "policy": "Legacy",
                "policy_evidence": "source_authority",
                "incarnation": "nonce-1",
                "session_id": "s1",
                "raw_path": "/Users/op/.codex/sessions/rollout.jsonl",
                "raw_file_identity": [1, 2],
                "recovery_target": True,
                "native_turn_id": "turn-1",
                "nonce": "turn-nonce-1",
                "body_msg_id": "123",
                "proof": "planned",
            }
        ],
    }


def refusals(mutate):
    survey = complete_survey()
    mutate(survey)
    return activation.check(survey)


class ActivationValidatorTests(unittest.TestCase):
    def assert_refused(self, mutate, needle):
        reasons = refusals(mutate)
        self.assertTrue(
            any(needle in reason for reason in reasons),
            f"expected a refusal naming {needle!r}, got {reasons}",
        )

    def test_a_complete_same_sha_survey_is_approved(self):
        self.assertEqual(activation.check(complete_survey()), [])

    def test_one_surviving_old_writer_is_refused(self):
        def mutate(s):
            s["writers_stopped"].append(dict(s["writers_stopped"][0], pid=102, alive=True))

        self.assert_refused(mutate, "stopped writer mac-mini:102: still alive or restartable")

    def test_one_unresolved_root_is_refused(self):
        def mutate(s):
            s["declared_roots"].append("~/.adk/standby")

        self.assert_refused(mutate, "root ~/.adk/standby: declared but not resolved")

    def test_two_declared_roots_resolving_to_one_root_are_refused(self):
        def mutate(s):
            s["declared_roots"].append("/Users/op/.adk/release")
            s["roots"].append(dict(s["roots"][0], declared="/Users/op/.adk/release"))

        self.assert_refused(mutate, "resolve to one root")

    def test_a_verified_unavailable_source_is_refused(self):
        def mutate(s):
            s["sources"][0]["policy"] = "VerifiedUnavailable"

        self.assert_refused(mutate, "VerifiedUnavailable is never downgraded")

    def test_an_env_only_policy_verdict_is_refused(self):
        def mutate(s):
            s["sources"][0]["policy_evidence"] = "env"

        self.assert_refused(mutate, "policy not taken from the tmux source authority")

    def test_empty_survey_sections_are_refused(self):
        for key in ("hosts", "roots", "writers_stopped", "writers_restarted", "sources", "artifacts"):
            with self.subTest(key=key):
                self.assert_refused(lambda s, key=key: s.__setitem__(key, []), f"{key}: survey section")

    def test_an_unsurveyed_declared_host_is_refused(self):
        def mutate(s):
            s["declared_hosts"].append("mac-book")

        self.assert_refused(mutate, "host mac-book: declared but not surveyed")

    def test_a_mixed_sha_artifact_is_refused(self):
        def mutate(s):
            s["artifacts"].append({"path": "worker", "sha256": "c" * 64, "source_sha": "d" * 40})

        self.assert_refused(mutate, "not the approved SHA")

    def test_a_running_executable_outside_the_approved_artifacts_is_refused(self):
        def mutate(s):
            s["writers_restarted"][0]["executable_sha256"] = "e" * 64

        self.assert_refused(mutate, "not an approved artifact")

    def test_a_symlink_only_executable_record_is_refused(self):
        def mutate(s):
            del s["writers_restarted"][0]["executable_realpath"]
            del s["writers_restarted"][0]["executable_sha256"]

        self.assert_refused(mutate, "executable realpath or start time not recorded")

    def test_an_unknown_role_or_unconfirmed_supervisor_is_refused(self):
        self.assert_refused(lambda s: s["writers_stopped"][0].__setitem__("role", "?"), "role unknown")
        self.assert_refused(
            lambda s: s["writers_stopped"][0].pop("supervisor_label"),
            "auto-restart supervisor not confirmed",
        )

    def test_a_recovery_target_without_identity_or_a_planned_proof_is_refused(self):
        self.assert_refused(lambda s: s["sources"][0].pop("native_turn_id"), "without Legacy policy")
        self.assert_refused(lambda s: s["sources"][0].__setitem__("proof", "held"), "without Legacy policy")

    def test_cli_exit_codes(self):
        with tempfile.TemporaryDirectory() as tmp:
            good = Path(tmp) / "good.json"
            good.write_text(json.dumps(complete_survey()))
            bad = Path(tmp) / "bad.json"
            survey = copy.deepcopy(complete_survey())
            survey["sources"][0]["policy"] = "VerifiedUnavailable"
            bad.write_text(json.dumps(survey))
            run = lambda path: subprocess.run(
                [sys.executable, str(SCRIPT), str(path)], capture_output=True, text=True
            )
            ok, refused, unreadable = run(good), run(bad), run(Path(tmp) / "absent.json")
        self.assertEqual((ok.returncode, ok.stdout.strip()), (0, "P5925_ACTIVATION_OK"))
        self.assertEqual(refused.returncode, 1)
        self.assertTrue(refused.stdout.startswith("P5925_ACTIVATION_REFUSED\nREFUSE "))
        self.assertEqual(unreadable.returncode, 2)


if __name__ == "__main__":
    unittest.main()
