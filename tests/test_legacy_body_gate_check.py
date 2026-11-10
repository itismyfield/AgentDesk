"""Verdict rules of the legacy body deletion gate, driven through file inputs like a real run."""
import contextlib
import copy
import io
import json
import tempfile
import unittest
from pathlib import Path

from scripts import legacy_body_gate_check as gate

SINCE = "2026-11-08T12:00:00+09:00"
SINCE_MS = 1794106800000  # 2026-11-08T03:00:00Z
TUI_CHANNEL, OTHER_CHANNEL = 111, 222
HOST, OTHER_HOST = "MacBook-Pro-14", "itismyfieldui-Macmini"
OLD_BOOT = "00000000-0000-4000-8000-000000000001"
NEW_BOOT = "00000000-0000-4000-8000-000000000002"


def target(at_ms, channel, output_class="LegacyBody", binding="TUI", site="W10", reason="DEFERRED"):
    return {"kind": "target", "at_ms": at_ms, "channel": channel, "op": "EDIT", "phase": "result",
            "census": {"output_class": output_class, "binding_mode": binding, "site": site,
                       "reason": reason, "actual_runtime": "unknown", "source_incarnation": None,
                       "body_owner": "Legacy"}}


def boot(boot_id, started, predecessor, targets):
    return {"header": {"boot_id": boot_id, "started_at_ms": started, "predecessor_boot_id": predecessor},
            "window": {"cut": len(targets), "drained": len(targets), "invalid": False},
            "selection": {"tui_bindings": [TUI_CHANNEL]}, "targets": targets}


def census():
    old = boot(OLD_BOOT, SINCE_MS - 3_600_000, None, [
        # A TUI legacy send before `since` belongs to the previous era and must not count.
        target(SINCE_MS - 60_000, TUI_CHANNEL)])
    new = boot(NEW_BOOT, SINCE_MS + 3_600_000, OLD_BOOT, [
        target(SINCE_MS + 7_200_000, OTHER_CHANNEL, binding="UNKNOWN", site="W17", reason="NON_MEMBER"),
        target(SINCE_MS + 7_300_000, TUI_CHANNEL, output_class="NonBody")])
    return {"host": "mac-book-release", "current_boot_id": NEW_BOOT, "evidence_truncated": False,
            "stale_snapshot": False, "errors": ["START_INVENTORY_UNAVAILABLE"],
            "coverage_start": {"boot_id": OLD_BOOT, "started_at_ms": SINCE_MS - 3_600_000},
            "cut_ms": SINCE_MS + 86_400_000, "live_journal": {"enqueued_cut": 2, "invalid": False},
            "boot_files": [old, new]}


def turns(agent, count, channel=TUI_CHANNEL, host=HOST):
    # created_at is UTC text; the first row sits exactly on `since`.
    key = f"claude/x/{host}:s" if host else None
    rows = [{"turn_id": f"{agent}{n}", "channel_id": str(channel), "session_key": key,
             "created_at": f"2026-11-08 03:00:{n:02d}", "duration_ms": 1000} for n in range(count)]
    return {"agent_id": agent, "transcripts": rows}


def bundle(*agents, limit=2000):
    return {"limit": limit, "agents": list(agents)}


class GateVerdictTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def run_gate(self, census_doc, turns_doc, *extra, wrap=False):
        census_path, turns_path = self.root / "census.json", self.root / "turns.json"
        census_path.write_text(json.dumps({"legacy_body_census": census_doc} if wrap else census_doc))
        turns_path.write_text(json.dumps(turns_doc))
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = gate.main(["check", "--census", str(census_path), "--turns", str(turns_path),
                              "--since", SINCE, "--min-turns", "50", "--turn-host", HOST, *extra])
        return code, out.getvalue()

    def test_complete_evidence_passes(self):
        code, out = self.run_gate(census(), bundle(turns("work", 50)), wrap=True)
        self.assertEqual(code, 0, out)
        self.assertTrue(out.startswith("VERDICT PASS\n"), out)
        self.assertIn("turns=50 ", out)
        self.assertIn("restarts=1\n", out)
        self.assertIn("tui_legacy_sends=0\n", out)

    def test_tui_legacy_send_fails_even_with_unknown_identity(self):
        for binding in ("TUI", "UNKNOWN"):
            with self.subTest(binding=binding):
                doc = census()
                doc["boot_files"][1]["targets"].append(target(SINCE_MS + 7_400_000, TUI_CHANNEL, binding=binding))
                code, out = self.run_gate(doc, bundle(turns("work", 50)))
                self.assertEqual(code, 1, out)
                self.assertTrue(out.startswith("VERDICT FAIL"), out)
                self.assertIn(f"ch={TUI_CHANNEL} site=W10 EDIT/result LegacyBody", out)
        doc = census()
        doc["boot_files"][1]["targets"].append(target(SINCE_MS + 7_400_000, TUI_CHANNEL, binding="NON_TUI"))
        code, out = self.run_gate(doc, bundle(turns("work", 50)))
        self.assertEqual(code, 0, out)
        self.assertIn("tui_legacy_sends=0\n", out)

    def test_evidence_gaps_never_pass(self):
        def capped(doc):
            doc["evidence_truncated"] = True

        def shrunk(doc):
            doc.update(evidence_truncated=True, boot_files=[])

        def late_coverage(doc):
            doc["coverage_start"]["started_at_ms"] = SINCE_MS + 1

        def missing_predecessor(doc):
            del doc["boot_files"][0]

        def journal_error(doc):
            doc["errors"].append("JOURNAL_READ_FAILED")

        def unknown_class(doc):
            doc["boot_files"][1]["targets"].append(target(SINCE_MS + 7_400_000, TUI_CHANNEL, output_class=None))

        def live_behind(doc):
            doc["live_journal"]["enqueued_cut"] = 3

        def stale(doc):
            doc["stale_snapshot"] = True

        def no_selection(doc):
            del doc["boot_files"][1]["selection"]

        def null_bindings(doc):
            doc["boot_files"][1]["selection"]["tui_bindings"] = None

        def no_targets(doc):
            doc["boot_files"][1]["targets"] = None

        cases = {capped: "EVIDENCE_TRUNCATED", shrunk: "BOOT_FILES_MISSING", late_coverage: "WINDOW_NOT_COVERING_SINCE",
                 missing_predecessor: f"BOOT_CHAIN_GAP:{OLD_BOOT}", journal_error: "CENSUS_ERROR:JOURNAL_READ_FAILED",
                 unknown_class: "BODY_CLASS_UNKNOWN", live_behind: "LIVE_JOURNAL_NOT_DRAINED",
                 stale: "STALE_SNAPSHOT", no_selection: f"BOOT_SELECTION_MISSING:{NEW_BOOT}",
                 null_bindings: f"BOOT_SELECTION_MISSING:{NEW_BOOT}", no_targets: f"BOOT_TARGETS_MISSING:{NEW_BOOT}"}
        for mutate, reason in cases.items():
            with self.subTest(reason=reason):
                doc = copy.deepcopy(census())
                mutate(doc)
                code, out = self.run_gate(doc, bundle(turns("work", 50)))
                self.assertEqual(code, 2, out)
                self.assertTrue(out.startswith("VERDICT INCOMPLETE"), out)
                self.assertIn(reason, out)

    def test_turn_and_restart_thresholds(self):
        cells = bundle(turns("work", 45), turns("cell", 10, channel=OTHER_CHANNEL))
        code, out = self.run_gate(census(), cells)
        self.assertEqual(code, 0, out)
        code, out = self.run_gate(census(), cells, "--exclude-agent", "cell")
        self.assertIn("TURNS_BELOW_THRESHOLD", out)
        code, out = self.run_gate(census(), cells, "--exclude-channel", str(OTHER_CHANNEL))
        self.assertIn("TURNS_BELOW_THRESHOLD", out)
        # A full page reaching back only to `since` may hide turns, even above the threshold.
        code, out = self.run_gate(census(), bundle(turns("work", 50), limit=50))
        self.assertEqual(code, 2, out)
        self.assertIn("TURNS_PAGE_FULL", out)
        older = turns("work", 50)
        older["transcripts"].append(dict(older["transcripts"][0], turn_id="old", created_at="2026-11-08 02:59:59"))
        code, out = self.run_gate(census(), bundle(older, limit=51))
        self.assertEqual(code, 0, out)
        no_restart = census()
        no_restart["boot_files"][1]["header"]["started_at_ms"] = SINCE_MS - 1
        code, out = self.run_gate(no_restart, bundle(turns("work", 50)))
        self.assertEqual(code, 2, out)
        self.assertIn("NO_RESTART_SINCE", out)

    def test_turns_count_only_the_named_host(self):
        code, out = self.run_gate(census(), bundle(turns("work", 40), turns("mini", 10, host=OTHER_HOST)))
        self.assertEqual(code, 2, out)
        self.assertIn("TURNS_BELOW_THRESHOLD", out)
        code, out = self.run_gate(census(), bundle(turns("work", 50), turns("mini", 10, host=OTHER_HOST)))
        self.assertEqual(code, 0, out)
        self.assertIn("turns=50 ", out)
        code, out = self.run_gate(census(), bundle(turns("work", 50), turns("anon", 1, host=None)))
        self.assertEqual(code, 2, out)
        self.assertIn("TURN_HOST_UNKNOWN:anon", out)

    def test_unknown_body_is_listed_without_deciding(self):
        doc = census()
        doc["boot_files"][1]["targets"].append(
            target(SINCE_MS + 7_500_000, TUI_CHANNEL, output_class="UnknownBody", binding="UNKNOWN", site=None))
        code, out = self.run_gate(doc, bundle(turns("work", 50)))
        self.assertEqual(code, 0, out)
        self.assertTrue(out.startswith("VERDICT PASS REVIEW_NEEDED\n"), out)
        self.assertIn(f"unknown_body=1 channels={{{TUI_CHANNEL}: 1}}", out)


if __name__ == "__main__":
    unittest.main()
