"""Contracts for the read-only giant deadline warning reporter and its workflow.

Test names carry the acceptance id (``test_c1_NN_*``); the Script checks shard
contract requires every id to run unskipped.
"""

from __future__ import annotations

import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest import mock

import yaml

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import report_giant_deadlines as reporter  # noqa: E402

WORKFLOW = REPO_ROOT / ".github/workflows/giant-deadline-warning.yml"
WORKFLOW_NAME = "Giant deadline warning"
LEDGERS = ("scripts/audit_maintainability_giant_baseline.toml", "scripts/giant_file_closed_issue_transition_list.txt")
STAMP = "2026-10-06T23:02:50Z"
REFRESHED = datetime(2026, 10, 6, 23, 2, 50, tzinfo=timezone.utc)
NOW = datetime(2026, 10, 13, 12, 0, tzinfo=timezone.utc)
EXPR = re.compile(r"\$\{\{\s*(.+?)\s*\}\}")


def shrink(path: str, deadline: str, **extra: str) -> dict[str, str]:
    return {"file": path, "decision": "shrink", "owner": "discord-relay", "deadline": deadline,
            "decompose_issue": "#4712", **extra}


def registry_text(entries: list[dict], grandfathered: list[str] = ()) -> str:
    lines = ["grandfathered_baseline_paths = []", f"grandfathered = {json.dumps(list(grandfathered))}", ""]
    for item in entries:
        lines += ["[[entry]]", *(f"{key} = {json.dumps(value)}" for key, value in item.items()), ""]
    return "\n".join(lines)


def metadata_text(issues: list[dict] | None = None, refreshed_at: str | None = STAMP) -> str:
    payload = {"schema_version": 2, "refreshed_at": refreshed_at,
               "ratchets": {"closed_deadline_entries": 0, "transition_list_entries": 0},
               "issues": issues if issues is not None else [
                   {"number": 4712, "state": "closed", "title": "relay", "owners": ["discord-relay"],
                    "files": ["src/a.rs"]}]}
    if refreshed_at is None:
        del payload["refreshed_at"]
    return json.dumps(payload)


def run_report(registry: str, metadata: str | None = None, now: datetime = NOW) -> tuple[int, dict]:
    """Run the script entry point on a fixture checkout and return (exit code, report.json)."""
    with tempfile.TemporaryDirectory(prefix="giant-deadlines-main-") as tmp:
        root = write_root(Path(tmp) / "root", registry, metadata_text() if metadata is None else metadata)
        rc = run_main("--root", str(root), "--out", str(Path(tmp) / "out"), "--now", now.isoformat())
        return rc, read_report(Path(tmp) / "out")


def main_report(registry: str, metadata: str | None = None, now: datetime = NOW) -> dict:
    return run_report(registry, metadata, now)[1]


def by_path(report: dict) -> dict[str, dict]:
    return {row["path"]: row for row in report["deadlines"]["rows"]}


def problems(report: dict) -> str:
    return "\n".join(f"{d['input']} {d['path']} {d['problem']}" for d in report["completeness"]["diagnostics"])


def clean_env(**extra: str) -> dict[str, str]:
    env = {k: v for k, v in os.environ.items() if not k.startswith(("GIT_", "SCRIPT_CHECK_", "GITHUB_"))}
    return {**env, **extra}


def write_root(root: Path, registry: str, metadata: str | None, extra: dict[str, bytes] = {}) -> Path:
    (root / "scripts").mkdir(parents=True, exist_ok=True)
    (root / reporter.REGISTRY_PATH).write_text(registry, encoding="utf-8")
    if metadata is not None:
        (root / reporter.METADATA_PATH).write_text(metadata, encoding="utf-8")
    for rel, data in extra.items():
        (root / rel).parent.mkdir(parents=True, exist_ok=True)
        (root / rel).write_bytes(data)
    return root


def git(root: Path, *args: str) -> str:
    return subprocess.run(["git", "-C", str(root), *args], check=True, capture_output=True, text=True,
                          env=clean_env()).stdout.strip()


def commit_all(root: Path) -> str:
    git(root, "init", "-q")
    git(root, "add", "-A")
    git(root, "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture")
    return git(root, "rev-parse", "HEAD")


def run_main(*argv: str, clock=None) -> int:
    with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
        return reporter.main(list(argv), **({"clock": clock} if clock else {}))


def read_report(out: Path) -> dict:
    return json.loads((out / "report.json").read_text(encoding="utf-8"))


class FixtureCase(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory(prefix="giant-deadlines-")
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name).resolve()


class Classification(FixtureCase):
    def test_c1_01_boundaries_are_inclusive_and_old_overdue_stays_visible(self) -> None:
        offsets = {"src/ancient.rs": -400, "src/m1.rs": -1, "src/d0.rs": 0, "src/d14.rs": 14,
                   "src/d15.rs": 15, "src/d60.rs": 60, "src/d61.rs": 61}
        rc, report = run_report(registry_text([shrink(p, (NOW.date() + timedelta(days=d)).isoformat())
                                               for p, d in offsets.items()]))
        self.assertEqual(rc, 0)
        rows = by_path(report)
        self.assertEqual({p: r["severity"] for p, r in rows.items()}, {
            "src/ancient.rs": "OVERDUE", "src/m1.rs": "OVERDUE", "src/d0.rs": "URGENT", "src/d14.rs": "URGENT",
            "src/d15.rs": "HORIZON", "src/d60.rs": "HORIZON"})
        self.assertEqual(rows["src/d0.rs"], {"path": "src/d0.rs", "owner": "discord-relay", "issue": "#4712",
                                             "deadline": "2026-10-13", "days_remaining": 0, "severity": "URGENT",
                                             "snapshot_issue_state": "CLOSED"})
        self.assertEqual(report["deadlines"]["counts"], {"overdue": 2, "urgent": 2, "horizon": 2, "beyond_horizon": 1,
                                                         "keep": 0, "invalid": 0, "untracked": 0})
        self.assertEqual(report["completeness"]["status"], "complete")

    def test_c1_02_days_follow_the_utc_date_across_kst_and_utc_midnight(self) -> None:
        registry = registry_text([shrink("src/a.rs", "2026-10-31")])
        for now, days in (("2026-10-31T14:59:59+00:00", 0), ("2026-10-31T15:00:00+00:00", 0),
                          ("2026-11-01T08:59:59+09:00", 0), ("2026-10-31T23:59:59.999999+00:00", 0),
                          ("2026-11-01T00:00:00+00:00", -1), ("2026-11-01T09:00:00+09:00", -1)):
            with self.subTest(now=now):
                report = main_report(registry, now=datetime.fromisoformat(now))
                self.assertEqual(by_path(report)["src/a.rs"]["days_remaining"], days)
                self.assertEqual(report["run"]["observed_utc_date"], "2026-10-31" if days == 0 else "2026-11-01")

    def test_c1_02_process_time_zone_does_not_change_the_report(self) -> None:
        root = write_root(self.tmp / "root", registry_text([shrink("src/a.rs", "2026-10-31")]), metadata_text())
        reports = []
        for zone in ("Asia/Seoul", "America/Los_Angeles", "UTC"):
            out = self.tmp / zone.replace("/", "-")
            done = subprocess.run([sys.executable, str(REPO_ROOT / "scripts/report_giant_deadlines.py"),
                                   "--root", str(root), "--out", str(out), "--now", "2026-10-31T15:30:00Z"],
                                  env=clean_env(TZ=zone), capture_output=True, text=True, timeout=60)
            self.assertEqual(done.returncode, 0, done.stderr)
            reports.append(read_report(out))
        self.assertEqual(by_path(reports[0])["src/a.rs"]["days_remaining"], 0)
        self.assertTrue(all(report == reports[0] for report in reports))

    def test_c1_02_main_reads_the_clock_exactly_once(self) -> None:
        root = write_root(self.tmp / "root", registry_text([shrink("src/a.rs", "2026-10-31")]), metadata_text())
        ticks = iter([datetime(2026, 10, 31, 23, 59, 59, 999999, tzinfo=timezone.utc)])
        self.assertEqual(run_main("--root", str(root), "--out", str(self.tmp / "out"), clock=lambda: next(ticks)), 0)
        report = read_report(self.tmp / "out")
        self.assertEqual(report["run"]["observed_at"], "2026-10-31T23:59:59.999999Z")
        self.assertEqual(by_path(report)["src/a.rs"]["days_remaining"], 0)


class Freshness(FixtureCase):
    def run_at(self, now: datetime, metadata: str) -> tuple[int, dict]:
        root = write_root(self.tmp / "root", registry_text([shrink("src/a.rs", "2026-10-31")]), metadata)
        out = self.tmp / f"out-{len(list(self.tmp.iterdir()))}"
        rc = run_main("--root", str(root), "--out", str(out), "--now", now.isoformat())
        return rc, read_report(out)

    def test_c1_03_thirty_days_is_fresh_and_one_microsecond_later_is_stale(self) -> None:
        for now, freshness in ((REFRESHED + timedelta(days=30), "FRESH"),
                               (REFRESHED + timedelta(days=30, microseconds=1), "STALE")):
            with self.subTest(freshness=freshness):
                rc, report = self.run_at(now, metadata_text())
                self.assertEqual(rc, 0)
                self.assertEqual(report["metadata"]["freshness"], freshness)
                self.assertEqual(report["metadata"]["fresh_through"], "2026-11-05T23:02:50Z")
                self.assertEqual(report["completeness"]["status"], "complete")
                self.assertEqual(len(report["deadlines"]["rows"]), 1)

    def test_c1_04_bad_stamps_keep_deadline_rows_and_warn(self) -> None:
        for label, stamp in (("missing", None), ("format", "2026-10-06 23:02:50"),
                             ("future", "2026-10-13T12:00:01Z")):
            with self.subTest(stamp=label):
                rc, report = self.run_at(NOW, metadata_text(refreshed_at=stamp))
                self.assertEqual(rc, 0)
                self.assertEqual(report["metadata"]["freshness"], "INVALID")
                self.assertEqual(report["completeness"]["status"], "partial")
                self.assertIn("refreshed_at", problems(report))
                self.assertEqual(by_path(report)["src/a.rs"]["snapshot_issue_state"], "CLOSED")


class RegistrySemantics(FixtureCase):
    def test_missing_deadline_and_issue_warn_and_succeed(self) -> None:
        entry = shrink("src/a.rs", "2026-10-20")
        del entry["deadline"], entry["decompose_issue"]
        root = write_root(self.tmp / "missing-plan", registry_text([entry]), metadata_text())
        out, err = self.tmp / "missing-out", io.StringIO()
        with redirect_stdout(io.StringIO()), redirect_stderr(err):
            rc = reporter.main(["--root", str(root), "--out", str(out), "--now", NOW.isoformat()])
        self.assertEqual(rc, 0, err.getvalue())
        self.assertEqual(read_report(out)["completeness"]["status"], "partial")
        self.assertIn("WARNING:", err.getvalue())
        for value in ("src/a.rs", "deadline", "decompose_issue"):
            self.assertIn(value, err.getvalue())

    def test_c1_05_omitted_decision_is_shrink_and_valid_keep_is_excluded(self) -> None:
        legacy = {key: value for key, value in shrink("src/legacy.rs", "2026-10-20").items() if key != "decision"}
        keep = {"file": "src/kept.rs", "decision": "keep", "owner": "relay", "keep_reason": "cohesive durable contract"}
        report = main_report(registry_text([legacy, keep]))
        self.assertEqual(list(by_path(report)), ["src/legacy.rs"])
        self.assertEqual(report["deadlines"]["counts"]["keep"], 1)
        self.assertEqual(report["completeness"]["status"], "complete")

    def test_c1_05_unknown_decision_and_keep_with_deadline_warn(self) -> None:
        rc, report = run_report(registry_text([
            shrink("src/near.rs", "2026-10-20"),
            {**shrink("src/odd.rs", "2026-10-20"), "decision": "defer"},
            {"file": "src/kd.rs", "decision": "keep", "owner": "relay", "keep_reason": "cohesive", "deadline": "2026-10-20"},
        ]))
        self.assertEqual((rc, list(by_path(report))), (0, ["src/near.rs"]))
        drifted = main_report(registry_text([shrink("src/near.rs", "2026-10-20", extension="2027-01-01")]) + "[policy]\nmode = 1\n")
        self.assertEqual(list(by_path(drifted)), ["src/near.rs"])
        self.assertIn("unknown field(s) not interpreted: extension", problems(drifted))
        self.assertIn("unknown top-level key(s) not interpreted: policy", problems(drifted))
        self.assertEqual(drifted["completeness"]["status"], "partial")
        self.assertEqual((report["deadlines"]["counts"]["invalid"], report["deadlines"]["counts"]["keep"]), (2, 0))
        self.assertIn("src/odd.rs unknown decision 'defer'", problems(report))
        self.assertIn("src/kd.rs keep forbids deadline", problems(report))
        self.assertEqual(report["completeness"]["status"], "partial")

    def test_c1_05_restored_grandfathered_registry_is_partial_not_zero(self) -> None:
        for label, entries in (("grandfathered only", []), ("with near shrink", [shrink("src/near.rs", "2026-10-20")])):
            with self.subTest(label):
                root = write_root(self.tmp / label.replace(" ", "-"),
                                  registry_text(entries, grandfathered=["src/old.rs"]), metadata_text())
                out = self.tmp / f"out-{label.replace(' ', '-')}"
                self.assertEqual(run_main("--root", str(root), "--out", str(out), "--now", NOW.isoformat()), 0)
                report = read_report(out)
                self.assertEqual(report["completeness"]["deadline_coverage"], "partial")
                self.assertEqual(report["deadlines"]["untracked_paths"], ["src/old.rs"])
                self.assertEqual((report["deadlines"]["counts"]["untracked"], report["deadlines"]["counts"]["keep"]), (1, 0))
                self.assertEqual(list(by_path(report)), [e["file"] for e in entries])

    def test_c1_05_registry_fixture_matches_the_existing_parser(self) -> None:
        sys.path.insert(0, str(REPO_ROOT / "scripts"))
        import generate_inventory_docs as inventory

        entries = [shrink("src/a.rs", "2026-10-01"),
                   {"file": "src/keep.rs", "owner": "discord-relay", "decision": "keep",
                    "keep_reason": "Shared cursor and relay identity form one lifecycle."}]
        registry, metadata = registry_text(entries), metadata_text()
        root = write_root(self.tmp / "fixture", registry, metadata)
        stamp = json.loads(metadata)["refreshed_at"]
        # Far future: every valid shrink registration is overdue and therefore listed.
        self.assertEqual(run_main("--root", str(root), "--out", str(self.tmp / "far"), "--now", "2200-01-01T00:00:00Z"), 0)
        report = read_report(self.tmp / "far")
        self.assertEqual(report["completeness"]["diagnostics"], [])
        with mock.patch.object(inventory, "GIANT_FILE_REGISTRY", root / reporter.REGISTRY_PATH):
            _grandfathered, entries, _baseline = inventory.load_giant_file_registry()
        expected = {(e["file"], e["owner"], e["deadline"], e["decompose_issue"])
                    for e in entries if e.get("decision", "shrink") == "shrink"}
        self.assertEqual({(r["path"], r["owner"], r["deadline"], r["issue"]) for r in report["deadlines"]["rows"]},
                         expected)
        self.assertEqual(report["deadlines"]["counts"]["keep"], sum(e.get("decision") == "keep" for e in entries))
        self.assertEqual(run_main("--root", str(root), "--out", str(self.tmp / "at"), "--now", stamp), 0)
        at_stamp = read_report(self.tmp / "at")
        self.assertEqual((at_stamp["metadata"]["freshness"], at_stamp["completeness"]["status"]), ("FRESH", "complete"))

    def test_c1_06_closed_rows_stay_and_metadata_scope_never_creates_rows(self) -> None:
        registry = registry_text([shrink("src/a.rs", "2026-10-31"), shrink("src/b.rs", "2026-10-31", decompose_issue="#9999")])
        metadata = metadata_text([
            {"number": 4712, "state": "closed", "title": "relay", "owners": ["discord-relay"],
             "files": ["src/a.rs", "src/retired.rs"]},
            {"number": 5000, "state": "open", "title": "x", "owners": ["x"], "files": ["src/only_in_metadata.rs"]}])
        rc, report = run_report(registry, metadata)
        self.assertEqual(rc, 0)
        rows = by_path(report)
        self.assertEqual({p: r["snapshot_issue_state"] for p, r in rows.items()}, {"src/a.rs": "CLOSED", "src/b.rs": "UNKNOWN"})
        unavailable = main_report(registry, "{not json")
        self.assertEqual({p: r["snapshot_issue_state"] for p, r in by_path(unavailable).items()},
                         {"src/a.rs": "UNKNOWN", "src/b.rs": "UNKNOWN"})
        self.assertEqual(unavailable["completeness"]["metadata_coverage"], "unavailable")

    def test_c1_06_malformed_issue_state_types_leave_rows_unknown(self) -> None:
        registry = registry_text([shrink("src/a.rs", "2026-10-31")])
        for label, record in (("array", {"state": []}), ("object", {"state": {}}), ("null", {"state": None}),
                              ("number", {"state": 7}), ("missing", {})):
            with self.subTest(state=label):
                rc, report = run_report(registry, metadata_text([{"number": 4712, **record}]))
                self.assertEqual(rc, 0)
                self.assertEqual(by_path(report)["src/a.rs"]["snapshot_issue_state"], "UNKNOWN")
                self.assertEqual(report["completeness"]["status"], "partial")
                self.assertIn("issue #4712 record", problems(report))

    def test_c1_07_malformed_rows_are_isolated_and_diagnosed_beyond_the_horizon(self) -> None:
        root = write_root(self.tmp / "root", registry_text([
            shrink("src/good.rs", "2026-10-16"),
            shrink("src/tbd_owner.rs", "2026-10-16", owner="TBD"),
            shrink("src/far.rs", "2099-01-01", decompose_issue="#4519"),
            shrink("src/bad_date.rs", "2026-02-30"),
        ]), metadata_text())
        self.assertEqual(run_main("--root", str(root), "--out", str(self.tmp / "out"), "--now", NOW.isoformat()), 0)
        report = read_report(self.tmp / "out")
        self.assertEqual(list(by_path(report)), ["src/good.rs"])
        self.assertEqual(report["deadlines"]["counts"]["invalid"], 3)
        for path in ("src/tbd_owner.rs", "src/far.rs", "src/bad_date.rs"):
            self.assertIn(path, problems(report))
        self.assertEqual(report["completeness"]["status"], "partial")

    def test_c1_08_duplicate_paths_and_issue_records_are_conflicts_not_overrides(self) -> None:
        rc, report = run_report(registry_text([shrink("src/dup.rs", "2026-10-20"), shrink("src/dup.rs", "2026-12-20"),
                                      shrink("src/b.rs", "2026-10-20", decompose_issue="#5000")],
                                     grandfathered=["src/b.rs"]),
                       metadata_text([{"number": 4712, "state": "closed"}, {"number": 4712, "state": "open"},
                                      {"number": 5000, "state": "open"}]))
        self.assertEqual((rc, by_path(report)), (0, {}))
        self.assertEqual(report["deadlines"]["counts"]["invalid"], 4)
        self.assertIn("issue #4712 is recorded more than once", problems(report))
        repeated_key = main_report(registry_text([shrink("src/a.rs", "2026-10-20")]),
                             metadata_text().replace('"state": "closed"', '"state": "closed", "state": "open"'))
        self.assertEqual(by_path(repeated_key)["src/a.rs"]["snapshot_issue_state"], "UNKNOWN")
        top_level = main_report(registry_text([shrink("src/a.rs", "2026-10-20")]),
                          metadata_text().replace('"schema_version": 2', '"schema_version": 2, "refreshed_at": "2026-10-13T00:00:00Z"'))
        self.assertEqual(top_level["metadata"]["freshness"], "INVALID")
        self.assertIn("repeats top-level key(s): refreshed_at", problems(top_level))

    def test_c1_08_unparsable_registry_is_unavailable_not_zero_warnings(self) -> None:
        broken = registry_text([shrink("src/a.rs", "2026-10-20")]) + 'owner = "again"\n'
        root = write_root(self.tmp / "root", broken, metadata_text())
        self.assertEqual(run_main("--root", str(root), "--out", str(self.tmp / "out"), "--now", NOW.isoformat()), 2)
        report = read_report(self.tmp / "out")
        self.assertEqual((report["completeness"]["status"], report["deadlines"]["counts"]), ("unavailable", None))
        self.assertEqual(report["metadata"]["freshness"], "FRESH")
        self.assertIn("not produced", (self.tmp / "out/report.md").read_text(encoding="utf-8"))
        for label, text in (("empty file", ""), ("comment only", "# registry\n"),
                            ("baseline only", 'grandfathered_baseline_paths = ["src/x.rs"]\n')):
            with self.subTest(registry=label):
                rc, report = run_report(text)
                self.assertEqual((rc, report["completeness"]["status"], report["deadlines"]["counts"]),
                                 (2, "unavailable", None))
        for label, text in (("explicit grandfathered", "grandfathered = []\n"), ("explicit entry", "entry = []\n")):
            with self.subTest(registry=label):
                rc, report = run_report(text)
                self.assertEqual((rc, report["completeness"]["status"]), (0, "complete"))
                self.assertEqual(set(report["deadlines"]["counts"].values()), {0})


class ReadOnlyBoundary(FixtureCase):
    def setUp(self) -> None:
        super().setUp()
        ledgers = {rel: (REPO_ROOT / rel).read_bytes() for rel in LEDGERS}
        self.root = write_root(self.tmp / "checkout", registry_text([shrink("src/a.rs", "2026-10-20")]),
                               metadata_text(), ledgers)
        self.sha = commit_all(self.root)
        self.before = {rel: (self.root / rel).read_bytes() for rel in git(self.root, "ls-files").splitlines()}

    def assert_untouched(self) -> None:
        self.assertEqual({rel: (self.root / rel).read_bytes() for rel in self.before}, self.before)
        self.assertEqual(git(self.root, "status", "--porcelain"), "")

    def report_args(self, out: Path) -> list[str]:
        return ["--root", str(self.root), "--out", str(out), "--expect-sha", self.sha, "--now", NOW.isoformat()]

    def test_c1_09_run_leaves_registry_metadata_and_ledgers_byte_identical(self) -> None:
        self.assertEqual(set(self.before), {reporter.REGISTRY_PATH, reporter.METADATA_PATH, *LEDGERS})
        self.assertTrue(all(self.before.values()), "an empty ledger cannot show that it stayed unchanged")
        out = self.tmp / "out"
        self.assertEqual(run_main(*self.report_args(out)), 0)
        self.assertEqual(list(by_path(read_report(out))), ["src/a.rs"])
        self.assert_untouched()

    def test_c1_09_reporter_never_loads_writer_gate_or_network_modules(self) -> None:
        done = subprocess.run([sys.executable, "-X", "importtime", str(REPO_ROOT / "scripts/report_giant_deadlines.py"),
                               *self.report_args(self.tmp / "out")], env=clean_env(), capture_output=True, text=True,
                              timeout=60)
        self.assertEqual(done.returncode, 0, done.stderr[-2000:])
        loaded = {line.rsplit("|", 1)[-1].strip() for line in done.stderr.splitlines() if line.startswith("import time:")}
        self.assertIn("tomllib", loaded)
        forbidden = {"generate_inventory_docs", "giant_file_progress", "refresh_giant_file_issue_metadata",
                     "socket", "ssl", "http.client", "urllib.request"}
        self.assertEqual(forbidden & loaded, set())

    def test_c1_09_output_inside_checkout_symlinked_or_reused_is_refused(self) -> None:
        previous = self.tmp / "previous"
        previous.mkdir()
        (previous / "report.json").write_text("earlier run", encoding="utf-8")
        (self.tmp / "link").symlink_to(self.tmp / "elsewhere", target_is_directory=True)
        for out in (self.root / "scripts", self.root / "reports", self.tmp / "link", previous):
            with self.subTest(out=out.name):
                self.assertEqual(run_main(*self.report_args(out)), 1)
        self.assertEqual((previous / "report.json").read_text(encoding="utf-8"), "earlier run")
        self.assertFalse((self.tmp / "elsewhere").exists())
        self.assert_untouched()

    def test_c1_09_failed_write_leaves_no_report(self) -> None:
        real = reporter._write_atomic

        def fail_json(path: Path, text: str) -> None:
            if path.name == "report.json":
                raise OSError("disk full")
            real(path, text)

        with mock.patch.object(reporter, "_write_atomic", fail_json):
            self.assertEqual(run_main(*self.report_args(self.tmp / "out")), 1)
        self.assertEqual(sorted(p.name for p in (self.tmp / "out").iterdir()), [])

    def test_c1_12_checkout_must_match_the_expected_sha_and_be_clean(self) -> None:
        out = self.tmp / "out"
        self.assertEqual(run_main(*self.report_args(out)[:-4], "--expect-sha", "0" * 40), 1)
        (self.root / reporter.REGISTRY_PATH).write_text(registry_text([]), encoding="utf-8")
        self.assertEqual(run_main(*self.report_args(out)), 1)
        self.assertFalse((out / "report.json").exists())

    def test_c1_12_verify_recomputes_input_hashes(self) -> None:
        out = self.tmp / "out"
        self.assertEqual(run_main(*self.report_args(out)), 0)
        verify = ["--verify", "--root", str(self.root), "--out", str(out), "--expect-sha", self.sha]
        self.assertEqual(run_main(*verify), 0)
        report = read_report(out)
        report["inputs"]["registry"]["sha256"] = "0" * 64
        (out / "report.json").write_text(json.dumps(report), encoding="utf-8")
        self.assertEqual(run_main(*verify), 1)


class WorkflowLoader(yaml.SafeLoader):
    """Keeps YAML 1.1 words such as ``on`` as strings, as Actions does."""


WorkflowLoader.yaml_implicit_resolvers = {
    key: [(tag, rx) for tag, rx in resolvers if tag != "tag:yaml.org,2002:bool"]
    for key, resolvers in yaml.SafeLoader.yaml_implicit_resolvers.items()}
WorkflowLoader.add_implicit_resolver("tag:yaml.org,2002:bool", re.compile(r"^(?:true|false)$"), list("tf"))


def load_workflow(path: Path = WORKFLOW) -> dict:
    return yaml.load(path.read_text(encoding="utf-8"), Loader=WorkflowLoader)


class WorkflowStructure(unittest.TestCase):
    def setUp(self) -> None:
        self.workflow = load_workflow()
        self.job = self.workflow["jobs"]["report"]

    def test_c1_10_runs_only_on_schedule_or_dispatch_as_one_independent_job(self) -> None:
        self.assertEqual(self.workflow["name"], WORKFLOW_NAME)
        self.assertEqual(set(self.workflow["on"]), {"schedule", "workflow_dispatch"})
        self.assertEqual(self.workflow["on"]["schedule"], [{"cron": "17 0 * * *"}])
        self.assertFalse(self.workflow["on"]["workflow_dispatch"])
        self.assertEqual(list(self.workflow["jobs"]), ["report"])
        self.assertEqual(self.workflow["permissions"], {"contents": "read"})
        for key in ("needs", "permissions", "concurrency", "continue-on-error"):
            self.assertNotIn(key, self.job)
        self.assertNotIn("concurrency", self.workflow)
        self.assertEqual(self.job["if"], "github.repository == 'itismyfield/AgentDesk' && github.ref == 'refs/heads/main'")
        self.assertEqual(self.job["timeout-minutes"], 5)
        self.assertEqual({s["uses"] for s in self.job["steps"] if "uses" in s},
                         {"actions/checkout@v4", "actions/setup-python@v5", "actions/upload-artifact@v4"})
        self.assertNotIn("secrets.", WORKFLOW.read_text(encoding="utf-8"))
        for step in self.job["steps"]:
            self.assertNotIn("continue-on-error", step)
            run = step.get("run", "")
            self.assertNotIn("${{", run)
            for forbidden in ("cargo", "ci-script-checks", "generate_inventory_docs", "giant_file_progress", "GFP_",
                              "refresh_giant_file_issue_metadata", "gh ", "git push", "git commit"):
                self.assertNotIn(forbidden, run)

    def test_c1_10_no_workflow_run_subscriber_listens_to_this_workflow(self) -> None:
        for path in sorted((REPO_ROOT / ".github/workflows").glob("*.y*ml")):
            trigger = load_workflow(path).get("on")
            if isinstance(trigger, dict) and isinstance(trigger.get("workflow_run"), dict):
                with self.subTest(workflow=path.name):
                    self.assertNotIn(WORKFLOW_NAME, trigger["workflow_run"].get("workflows", []))

    def test_c1_11_steps_follow_the_fixed_order(self) -> None:
        self.assertEqual([step["id"] for step in self.job["steps"]],
                         ["bootstrap", "checkout", "setup", "provenance", "report", "postcheck", "execution",
                          "summary", "upload", "retention"])

    def test_c1_12_checkout_pins_the_event_sha_and_attempt_scoped_artifact(self) -> None:
        steps = {step["id"]: step for step in self.job["steps"]}
        self.assertEqual(steps["checkout"]["with"], {"ref": "${{ github.sha }}", "fetch-depth": 1,
                                                     "persist-credentials": False})
        self.assertEqual(steps["setup"]["with"], {"python-version": "3.11"})
        self.assertEqual(steps["upload"]["with"], {
            "name": "giant-deadlines-${{ github.sha }}-${{ github.run_id }}-${{ github.run_attempt }}",
            "path": "${{ steps.execution.outputs.dir }}",
            "retention-days": 90, "if-no-files-found": "error"})


class WorkflowSimulation(FixtureCase):
    """Replays the job's real shell steps in order with stubbed actions."""

    def workspace(self, registry: str, extra: dict[str, bytes] = {}) -> tuple[Path, str]:
        script = {"scripts/report_giant_deadlines.py": (REPO_ROOT / "scripts/report_giant_deadlines.py").read_bytes()}
        root = write_root(self.tmp / "workspace", registry, metadata_text(refreshed_at="2026-01-01T00:00:00Z"),
                          {**script, **extra})
        return root, commit_all(root)

    def simulate(self, root: Path, sha: str, *, attempt: str = "1", actions: dict[str, str] = {},
                 artifact_id: str = "", summary: Path | None = None) -> dict:
        bin_dir = self.tmp / "bin"
        if not bin_dir.exists():
            bin_dir.mkdir()
            (bin_dir / "python3").symlink_to(sys.executable)
            (bin_dir / "cargo").write_text("#!/bin/sh\nexit 101\n", encoding="utf-8")
            (bin_dir / "cargo").chmod(0o755)
        summary = summary or self.tmp / f"summary-{attempt}.md"
        env = clean_env(PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", RUNNER_TEMP=str(self.tmp / "runner"),
                        GITHUB_RUN_ID="777", GITHUB_RUN_ATTEMPT=attempt, GITHUB_SHA=sha, GITHUB_WORKSPACE=str(root),
                        GITHUB_REPOSITORY="itismyfield/AgentDesk", GITHUB_EVENT_NAME="schedule",
                        GITHUB_STEP_SUMMARY=str(summary))
        outcomes: dict[str, str] = {}
        context = {"steps.upload.outputs.artifact-id": artifact_id}
        failed, artifact = False, None
        for step in load_workflow()["jobs"]["report"]["steps"]:
            def expand(value: object) -> str:
                return EXPR.sub(lambda m: context.get(m.group(1), ""), str(value))

            if not self.condition(step.get("if", "success()"), outcomes, failed):
                outcomes[step["id"]] = "skipped"
            elif "uses" in step:
                outcomes[step["id"]] = actions.get(step["id"], "success")
                if step["id"] == "upload":
                    artifact = Path(expand(step["with"]["path"]))
            else:
                output = self.tmp / f"output-{step['id']}"
                output.write_text("", encoding="utf-8")
                step_env = {key: expand(value) for key, value in step.get("env", {}).items()}
                done = subprocess.run(["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", step["run"]],
                                      cwd=root, env={**env, **step_env, "GITHUB_OUTPUT": str(output)},
                                      capture_output=True, text=True, timeout=60)
                outcomes[step["id"]] = "success" if done.returncode == 0 else "failure"
                for line in output.read_text(encoding="utf-8").splitlines():
                    key, _, value = line.partition("=")
                    context[f"steps.{step['id']}.outputs.{key}"] = value
            context[f"steps.{step['id']}.outcome"] = outcomes[step["id"]]
            if outcomes[step["id"]] == "failure" and not step.get("continue-on-error"):
                failed = True
        out = self.tmp / "runner/giant-deadlines/777" / attempt
        text = summary.read_text(encoding="utf-8") if summary.is_file() else ""
        published = artifact if artifact is not None else out
        execution = (published / "execution.txt").read_text(encoding="utf-8") \
            if (published / "execution.txt").is_file() else ""
        return {"outcomes": outcomes, "conclusion": "failure" if failed else "success", "out": out,
                "artifact": artifact, "summary": text, "execution": execution}

    @staticmethod
    def condition(expression: str, outcomes: dict[str, str], failed: bool) -> bool:
        holds = "always()" in expression or not failed
        for term in (part.strip() for part in expression.split("&&")):
            if term in ("always()", "success()"):
                continue
            match = re.fullmatch(r"steps\.([\w-]+)\.outcome == '(\w+)'", term)
            if match is None:
                raise AssertionError(f"unmodelled condition: {term}")
            holds = holds and outcomes.get(match.group(1)) == match.group(2)
        return holds

    def test_c1_10_report_runs_although_cargo_and_existing_gates_fail(self) -> None:
        root, sha = self.workspace(registry_text([shrink("src/a.rs", "2099-12-31")]),
                                   {"scripts/ci-script-checks.sh": b"#!/bin/sh\nexit 1\n"})
        run = self.simulate(root, sha)
        self.assertEqual(run["conclusion"], "success", run)
        self.assertEqual(read_report(run["out"])["run"]["source_sha"], sha)
        self.assertIn("Completeness: **complete**", run["summary"])

    def test_c1_11_reporter_exit_2_still_records_publishes_and_uploads(self) -> None:
        root, sha = self.workspace('grandfathered = [\n')
        run = self.simulate(root, sha)
        self.assertEqual(run["outcomes"]["report"], "failure")
        for step in ("postcheck", "execution", "summary", "upload", "retention"):
            self.assertNotEqual(run["outcomes"][step], "skipped", step)
        self.assertEqual(run["conclusion"], "failure")
        self.assertIn("Completeness: **unavailable**", run["summary"])
        self.assertIn("report=failure\npostcheck=success\nreport_generated=yes", run["execution"])

    def test_c1_11_planning_drift_warns_without_failing_workflow(self) -> None:
        root, sha = self.workspace(registry_text([shrink("src/a.rs", "2099-12-31"),
                                                  shrink("src/b.rs", "bad")]))
        run = self.simulate(root, sha)
        self.assertEqual(run["conclusion"], "success")
        self.assertEqual(run["outcomes"]["report"], "success")
        self.assertIn("Completeness: **partial**", run["summary"])
        self.assertIn("report=success\npostcheck=success\nreport_generated=yes", run["execution"])
        self.assertIn("WARNING: giant-file planning: src/b.rs",
                      (run["out"] / "reporter.log").read_text())

    def test_c1_11_setup_failure_publishes_report_not_generated(self) -> None:
        root, sha = self.workspace(registry_text([shrink("src/a.rs", "2099-12-31")]))
        run = self.simulate(root, sha, actions={"setup": "failure"})
        self.assertEqual((run["outcomes"]["provenance"], run["outcomes"]["report"]), ("skipped", "skipped"))
        self.assertEqual(run["outcomes"]["upload"], "success")
        self.assertIn("**Report not generated.**", run["summary"])
        self.assertIn("report=skipped", run["execution"])
        self.assertIn("report_generated=no", run["execution"])
        self.assertFalse((run["out"] / "report.json").exists())

    def test_c1_11_half_written_report_is_not_published_as_a_report(self) -> None:
        stub = b"import sys, pathlib\nif '--verify' in sys.argv: sys.exit(0)\n" \
               b"out = pathlib.Path(sys.argv[sys.argv.index('--out') + 1])\n" \
               b"(out / 'report.md').write_text('HALF WRITTEN')\nsys.exit(1)\n"
        root, sha = self.workspace(registry_text([]), {"scripts/report_giant_deadlines.py": stub})
        run = self.simulate(root, sha)
        self.assertIn("**Report not generated.**", run["summary"])
        self.assertNotIn("HALF WRITTEN", run["summary"])
        self.assertEqual(run["conclusion"], "failure")

    def test_c1_12_event_sha_mismatch_is_not_a_report(self) -> None:
        root, _sha = self.workspace(registry_text([shrink("src/a.rs", "2099-12-31")]))
        run = self.simulate(root, "0" * 40)
        self.assertEqual((run["outcomes"]["provenance"], run["outcomes"]["report"]), ("failure", "skipped"))
        self.assertIn("**Report not generated.**", run["summary"])
        self.assertFalse((run["out"] / "report.json").exists())

    def test_c1_12_each_attempt_reports_into_its_own_directory(self) -> None:
        root, sha = self.workspace(registry_text([shrink("src/a.rs", "2099-12-31")]))
        first, second = self.simulate(root, sha, attempt="1"), self.simulate(root, sha, attempt="2")
        self.assertEqual((first["conclusion"], second["conclusion"]), ("success", "success"), second)
        self.assertEqual([read_report(run["artifact"])["run"]["run_attempt"] for run in (first, second)], ["1", "2"])

    def test_c1_11_reused_run_directory_does_not_republish_the_earlier_report(self) -> None:
        root, sha = self.workspace(registry_text([shrink("src/a.rs", "2099-12-31")]))
        first = self.simulate(root, sha)
        again = self.simulate(root, sha, summary=self.tmp / "summary-again.md")
        self.assertEqual((first["conclusion"], again["outcomes"]["bootstrap"]), ("success", "failure"))
        self.assertIn("**Report not generated.**", again["summary"])
        self.assertNotIn("Completeness:", again["summary"])
        self.assertIn("report_generated=no", again["execution"])
        self.assertNotEqual(again["artifact"], first["artifact"])
        self.assertFalse((again["artifact"] / "report.json").exists())

    def test_c1_13_retention_and_receipt_are_reported_separately(self) -> None:
        root, sha = self.workspace(registry_text([shrink("src/a.rs", "2099-12-31")]))
        uploaded = self.simulate(root, sha, attempt="1", artifact_id="4242")
        lost = self.simulate(root, sha, attempt="2", actions={"upload": "failure"})
        self.assertIn("Artifact retention: uploaded (artifact id 4242)", uploaded["summary"])
        self.assertIn("Artifact retention: NOT confirmed (upload outcome failure)", lost["summary"])
        for run in (uploaded, lost):
            self.assertIn("Consumer receipt: UNVERIFIED", run["summary"])
            self.assertNotIn("RECEIVED", run["summary"])
            self.assertEqual(read_report(run["out"])["consumer_receipt"], {"status": "UNVERIFIED", "evidence": None})

    def test_c1_13_summary_failure_does_not_block_upload(self) -> None:
        root, sha = self.workspace(registry_text([shrink("src/a.rs", "2099-12-31")]))
        blocked = self.tmp / "summary-is-a-directory"
        blocked.mkdir()
        run = self.simulate(root, sha, summary=blocked)
        self.assertEqual((run["outcomes"]["summary"], run["outcomes"]["upload"]), ("failure", "success"))


if __name__ == "__main__":
    unittest.main()
