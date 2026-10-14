"""Fixtures for the dormant flaky-retry judge: which single re-runs turn a red lane FLAKY.

Output follows Rust 1.94.1 libtest; REAL_LOGFILE and the nested-run shape were captured from a real binary.
"""

from __future__ import annotations

import contextlib
import io
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import lane_retry_verdict as judge  # noqa: E402
from run_test_lane import LedgerEntry, Outcome, TranscriptScanner  # noqa: E402

ENTRY = "alpha::child_entry"
FAILS = "alpha::fails"
FLAPS = "alpha::flaps"
PASSES = "alpha::passes"
SPAWNS = "alpha::spawns_child"
SELECTED = frozenset({ENTRY, FAILS, FLAPS, PASSES, SPAWNS})
ENTRY_IGNORED = LedgerEntry("ignored", ENTRY, "demo", "#1",
                            "child-process entry point run only by a nested test", 1)
BINARY = judge.BinaryIdentity("/work/target/debug/deps/agentdesk-0123", "a" * 64)
LOG_WORD = {"ok": "ok", "FAILED": "failed", "ignored": "ignored"}

ONE_FAILED = [(ENTRY, "ignored"), (FAILS, "FAILED"), (FLAPS, "ok"), (PASSES, "ok"), (SPAWNS, "ok")]
TWO_FAILED = [(ENTRY, "ignored"), (FAILS, "FAILED"), (FLAPS, "FAILED"), (PASSES, "ok"),
              (SPAWNS, "ok")]
SPAWNS_FAILED = [(ENTRY, "ignored"), (FAILS, "ok"), (FLAPS, "ok"), (PASSES, "ok"),
                 (SPAWNS, "FAILED")]
ALL_PASSED = [(ENTRY, "ignored"), (FAILS, "ok"), (FLAPS, "ok"), (PASSES, "ok"), (SPAWNS, "ok")]
NO_TEST = ("\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; "
           "5 filtered out; finished in 0.00s\n")
# rustc 1.94.1 --test, run with --logfile: should_panic mismatches span three lines.
REAL_LOGFILE = (
    "ignored alpha::child_entry\n"
    "failed alpha::fails\n"
    "ignored: needs a real host alpha::ignored_reason\n"
    "ok alpha::passes\n"
    "failed: panic did not contain expected string\n"
    '      panic message: "other \\"quoted\\"\\nline"\n'
    ' expected substring: "wanted" alpha::sp_mismatch\n'
    "failed: test did not panic as expected at fx.rs:14:8 alpha::sp_no_panic\n"
    "failed: expected panic with string value,\n"
    " found non-string value: `TypeId(0x79f19461b401588885a89e43144b08f1)`\n"
    '     expected substring: "x" alpha::sp_nonstring\n'
    "ok alpha::spawns_child\n"
).encode()


def scan(text: str) -> Outcome:
    outcome = Outcome()
    scanner = TranscriptScanner(outcome)
    for line in text.split("\n"):
        scanner.feed(line)
    return outcome


def summary(passed: int, failed: int, ignored: int, filtered_out: int) -> str:
    status = "FAILED" if failed else "ok"
    return (f"test result: {status}. {passed} passed; {failed} failed; {ignored} ignored; "
            f"0 measured; {filtered_out} filtered out; finished in 0.00s")


def libtest_stdout(results: list[tuple[str, str]], filtered_out: int = 0,
                   child_of: str | None = None) -> str:
    """Pretty stdout for (id, verdict) pairs; child_of nests a child run of ENTRY inside that test."""
    lines = ["", f"running {len(results)} test" + ("" if len(results) == 1 else "s")]
    for test_id, verdict in results:
        if test_id == child_of:
            lines += [f"test {test_id} ... ", "running 1 test", "child says hi",
                      f"test {ENTRY} ... ok", "", summary(1, 0, 0, len(SELECTED) - 1), "", verdict]
        else:
            lines.append(f"test {test_id} ... {verdict}")
    failed = [test_id for test_id, verdict in results if verdict == "FAILED"]
    if failed:
        lines += ["", "failures:", ""]
        for test_id in failed:
            lines += [f"---- {test_id} stdout ----", "",
                      f"thread '{test_id}' (7) panicked at src/lib.rs:5:18:", "boom", ""]
        lines += ["", "failures:", *(f"    {test_id}" for test_id in failed)]
    passed = sum(verdict == "ok" for _, verdict in results)
    ignored = sum(verdict == "ignored" for _, verdict in results)
    lines += ["", summary(passed, len(failed), ignored, filtered_out), ""]
    return "\n".join(lines)


def logfile(results: list[tuple[str, str]]) -> bytes:
    return "".join(f"{LOG_WORD[verdict]} {test_id}\n" for test_id, verdict in results).encode()


def assess(results: list[tuple[str, str]], *, rc: int | None = None, stdout: str | None = None,
           log: object = ..., timed_out: bool = False, entries: tuple = (ENTRY_IGNORED,),
           selected: frozenset[str] = SELECTED, max_summaries: int = 2,
           binary: judge.BinaryIdentity | None = BINARY, nested: str | None = None
           ) -> judge.Assessment:
    text = libtest_stdout(results, child_of=nested) if stdout is None else stdout
    data = logfile(results) if log is ... else log
    code = (101 if any(v == "FAILED" for _, v in results) else 0) if rc is None else rc
    # verify_structure names every missing id on stderr; keep the test output readable.
    with contextlib.redirect_stderr(io.StringIO()):
        return judge.assess_original_run(code, timed_out, scan(text), data, list(entries),
                                         selected, max_summaries, binary)


def rerun(test_id: str, verdict: str = "ok", *, rc: int | None = None, stdout: str | None = None,
          log: object = ..., binary: judge.BinaryIdentity | None = BINARY,
          timed_out: bool = False, nested: bool = False) -> judge.RetryRun:
    results = [(test_id, verdict)]
    if stdout is None:
        stdout = libtest_stdout(results, len(SELECTED) - 1, test_id if nested else None)
    data = logfile(results) if log is ... else log
    code = (0 if verdict == "ok" else 101) if rc is None else rc
    return judge.RetryRun(test_id, binary, code, timed_out, scan(stdout), data)


class FlakyRetryVerdictTests(unittest.TestCase):
    def assertReason(self, reason: str, errors: tuple[str, ...]) -> None:
        self.assertTrue(any(reason in error for error in errors), (reason, errors))

    def test_failed_then_exactly_one_passing_rerun_is_flaky_green(self) -> None:
        original = assess(ONE_FAILED)
        self.assertEqual((original.status, original.retry_ids), (judge.RETRYABLE, (FAILS,)))
        verdict = judge.judge_retries(original, [rerun(FAILS)])
        self.assertEqual((verdict.status, verdict.green, verdict.flaky, verdict.errors),
                         (judge.FLAKY, True, (FAILS,), ()))

    def test_failed_again_is_real_red_even_for_a_ledgered_flaky_test(self) -> None:
        listed = LedgerEntry("flaky", FAILS, "demo", "#1",
                             "fails only under parallel load expires=2099-01-01", 2)
        original = assess(ONE_FAILED, entries=(ENTRY_IGNORED, listed))
        verdict = judge.judge_retries(original, [rerun(FAILS, "FAILED")])
        self.assertEqual((verdict.status, verdict.green, verdict.real),
                         (judge.REAL, False, (FAILS,)))

    def test_partial_output_then_hang_is_red_and_retries_nothing(self) -> None:
        partial = f"\nrunning 5 tests\ntest {ENTRY} ... ignored\ntest {FAILS} ... FAILED\n"
        partial_log = logfile(ONE_FAILED[:2])
        cases = {
            "hang killed at the deadline": dict(stdout=partial, log=partial_log, rc=-9,
                                                timed_out=True),
            "killed without a deadline flag": dict(stdout=partial, log=partial_log, rc=-9),
            "complete output but past the deadline": dict(timed_out=True),
        }
        for label, kwargs in cases.items():
            with self.subTest(label):
                original = assess(ONE_FAILED, **kwargs)
                self.assertEqual((original.status, original.retry_ids), (judge.UNRETRYABLE, ()))
                self.assertEqual(judge.judge_retries(original, []).status, judge.UNRETRYABLE)
        late = assess(ONE_FAILED, timed_out=True)
        self.assertReason("hit its deadline", late.errors)
        forced = judge.judge_retries(late, [rerun(FAILS)])
        self.assertEqual(forced.status, judge.UNRETRYABLE)
        self.assertReason("1 retry run(s) were made although", forced.errors)

    def test_zero_executed_is_red_on_either_run(self) -> None:
        nothing = assess([], stdout=NO_TEST, log=b"", rc=0)
        self.assertEqual((nothing.status, nothing.retry_ids), (judge.UNRETRYABLE, ()))
        self.assertReason("lane-missing", nothing.errors)
        unselected = assess([], stdout=NO_TEST, log=b"", rc=0, selected=frozenset())
        self.assertEqual(unselected.status, judge.UNRETRYABLE)
        self.assertReason("expects no test", unselected.errors)
        verdict = judge.judge_retries(assess(ONE_FAILED),
                                      [rerun(FAILS, stdout=NO_TEST, log=b"", rc=0)])
        self.assertEqual(verdict.status, judge.UNRETRYABLE)
        self.assertReason(f"retry of {FAILS}: ran no test", verdict.errors)

    def test_rerun_count_must_equal_the_failure_count(self) -> None:
        original = assess(TWO_FAILED)
        self.assertEqual(original.retry_ids, (FAILS, FLAPS))
        cases = {
            "one rerun for two failures": (
                [rerun(FAILS)], ["2 failed test(s) but 1 retry run(s)", f"never retried: {FLAPS}"]),
            "three reruns, one id twice": (
                [rerun(FAILS), rerun(FLAPS), rerun(FAILS)],
                ["2 failed test(s) but 3 retry run(s)", f"retried more than once: {FAILS}"]),
            "three reruns, one id not failed": (
                [rerun(FAILS), rerun(FLAPS), rerun(PASSES)],
                ["2 failed test(s) but 3 retry run(s)",
                 f"retried but not an original failure: {PASSES}"]),
        }
        for label, (runs, reasons) in cases.items():
            with self.subTest(label):
                verdict = judge.judge_retries(original, runs)
                self.assertEqual((verdict.status, verdict.green), (judge.UNRETRYABLE, False))
                for reason in reasons:
                    self.assertReason(reason, verdict.errors)
        both = judge.judge_retries(original, [rerun(FAILS), rerun(FLAPS)])
        self.assertEqual((both.status, both.flaky), (judge.FLAKY, (FAILS, FLAPS)))

    def test_another_name_on_either_run_is_red(self) -> None:
        original = assess(ONE_FAILED)
        cases = {
            "logs a different test": (
                rerun(FAILS, log=logfile([(FLAPS, "ok")]),
                      stdout=libtest_stdout([(FLAPS, "ok")], 4)),
                f"logged [{FLAPS}] instead of exactly this test"),
            "logs a second test beside it": (
                rerun(FAILS, log=logfile([(FAILS, "ok"), (FLAPS, "ok")])),
                f"logged [{FAILS}, {FLAPS}] instead of exactly this test"),
            "reports itself ignored": (
                rerun(FAILS, "ignored", rc=0), "was reported ignored"),
        }
        for label, (run, reason) in cases.items():
            with self.subTest(label):
                verdict = judge.judge_retries(original, [run])
                self.assertEqual(verdict.status, judge.UNRETRYABLE)
                self.assertReason(f"retry of {FAILS}: {reason}", verdict.errors)
        swapped = logfile([(ENTRY, "ignored"), (FAILS, "ok"), (FLAPS, "FAILED"), (PASSES, "ok"),
                           (SPAWNS, "ok")])
        disagreeing = assess(ONE_FAILED, log=swapped)
        self.assertEqual((disagreeing.status, disagreeing.retry_ids), (judge.UNRETRYABLE, ()))
        self.assertReason(f"failed in the logfile only: {FLAPS}", disagreeing.errors)
        self.assertReason(f"failed on stdout only: {FAILS}", disagreeing.errors)

    def test_nested_stdout_cannot_pass_a_rerun_its_parent_failed(self) -> None:
        original = assess(SPAWNS_FAILED)
        self.assertEqual(original.retry_ids, (SPAWNS,))
        child_claims_pass = "\n".join([
            "", "running 1 test", f"test {SPAWNS} ... ", "running 1 test", f"test {SPAWNS} ... ok",
            "", summary(1, 0, 0, 4), "", "FAILED", "", "failures:", "", f"---- {SPAWNS} stdout ----",
            "", "", "failures:", f"    {SPAWNS}", "", summary(0, 1, 0, 4), ""])
        run = judge.RetryRun(SPAWNS, BINARY, 101, False, scan(child_claims_pass),
                             logfile([(SPAWNS, "FAILED")]))
        verdict = judge.judge_retries(original, [run])
        self.assertEqual((verdict.status, verdict.green, verdict.real),
                         (judge.REAL, False, (SPAWNS,)))
        child_fails_unlisted = "\n".join([
            "", "running 1 test", f"test {SPAWNS} ... ", "running 1 test", "test alpha::rogue ... FAILED",
            "", "failures:", "    alpha::rogue", "", summary(0, 1, 0, 4), "", "ok", "",
            summary(1, 0, 0, 4), ""])
        rogue = judge.judge_retries(original, [rerun(SPAWNS, stdout=child_fails_unlisted)])
        self.assertEqual(rogue.status, judge.UNRETRYABLE)
        self.assertReason("stdout reported ignored or other tests: alpha::rogue", rogue.errors)
        capped = judge.judge_retries(assess(SPAWNS_FAILED, max_summaries=1),
                                     [rerun(SPAWNS, nested=True)])
        self.assertEqual(capped.status, judge.UNRETRYABLE)
        self.assertReason("printed 2 summaries but the lane declares at most 1", capped.errors)

    def test_cut_off_logfile_is_red_and_retries_nothing(self) -> None:
        full = logfile(ONE_FAILED)
        cases = {
            "cut mid-record": (full[:-5], "ends mid-record"),
            "cut at a record boundary": (full[:full.rindex(b"ok " + SPAWNS.encode())],
                                         f"selected but missing from the logfile: {SPAWNS}"),
            "never written": (None, "no libtest --logfile was written"),
        }
        for label, (data, reason) in cases.items():
            with self.subTest(label):
                original = assess(ONE_FAILED, log=data)
                self.assertEqual((original.status, original.retry_ids), (judge.UNRETRYABLE, ()))
                self.assertReason(reason, original.errors)
        verdict = judge.judge_retries(assess(ONE_FAILED), [rerun(FAILS, log=b"ok alpha::fai")])
        self.assertEqual(verdict.status, judge.UNRETRYABLE)
        self.assertReason(f"retry of {FAILS}: the logfile ends mid-record", verdict.errors)

    def test_a_replaced_test_binary_is_red(self) -> None:
        original = assess(ONE_FAILED)
        rebuilt = judge.BinaryIdentity(BINARY.path, "b" * 64)
        verdict = judge.judge_retries(original, [rerun(FAILS, binary=rebuilt)])
        self.assertEqual(verdict.status, judge.UNRETRYABLE)
        self.assertReason("ran a different test binary", verdict.errors)
        unrecorded = assess(ONE_FAILED, binary=None)
        self.assertEqual((unrecorded.status, unrecorded.retry_ids), (judge.UNRETRYABLE, ()))
        with tempfile.TemporaryDirectory() as tmp:
            exe = Path(tmp) / "agentdesk-0123"
            exe.write_bytes(b"first build")
            link = Path(tmp) / "current"
            link.symlink_to(exe)
            before = judge.binary_identity(link)
            exe.write_bytes(b"second build")
            after = judge.binary_identity(link)
        self.assertEqual(before.path, after.path)
        self.assertTrue(before.path.endswith("/agentdesk-0123"))
        self.assertNotEqual(before.sha256, after.sha256)

    def test_abnormal_exit_or_compile_failure_is_red_and_retries_nothing(self) -> None:
        complete = libtest_stdout(ONE_FAILED)
        cases = {
            "signal after complete output": dict(rc=-6),
            "shell-reported abort": dict(rc=134),
            "abort before the summary, cargo reports 101": dict(
                stdout=complete.split("\nfailures:")[0], rc=101),
            "compile failure": dict(stdout="", log=None, rc=101),
        }
        for label, kwargs in cases.items():
            with self.subTest(label):
                original = assess(ONE_FAILED, **kwargs)
                self.assertEqual((original.status, original.retry_ids), (judge.UNRETRYABLE, ()))
        self.assertReason("not libtest's test-failure code 101", assess(ONE_FAILED, rc=-6).errors)

    def test_normal_nested_output_parses_on_both_runs(self) -> None:
        clean = assess(ALL_PASSED, nested=SPAWNS)
        self.assertEqual((clean.status, clean.errors), (judge.CLEAN, ()))
        original = assess(ONE_FAILED, nested=SPAWNS)
        self.assertEqual((original.status, original.retry_ids), (judge.RETRYABLE, (FAILS,)))
        spawner = assess(SPAWNS_FAILED, nested=SPAWNS)
        verdict = judge.judge_retries(spawner, [rerun(SPAWNS, nested=True)])
        self.assertEqual((verdict.status, verdict.flaky, verdict.errors), (judge.FLAKY, (SPAWNS,), ()))

    def test_multi_line_logfile_records_parse_as_one_record(self) -> None:
        records, errors = judge.parse_logfile(REAL_LOGFILE)
        self.assertEqual(errors, [])
        self.assertEqual([(r.verdict, r.test_id) for r in records], [
            ("ignored", "alpha::child_entry"), ("failed", "alpha::fails"),
            ("ignored", "alpha::ignored_reason"), ("ok", "alpha::passes"),
            ("failed", "alpha::sp_mismatch"), ("failed", "alpha::sp_no_panic"),
            ("failed", "alpha::sp_nonstring"), ("ok", "alpha::spawns_child")])
        lines = REAL_LOGFILE.split(b"\n")
        for kept in (5, 6):
            with self.subTest(kept_lines=kept):
                _, cut_errors = judge.parse_logfile(b"\n".join(lines[:kept]) + b"\n")
                self.assertReason("three-line should_panic failure record is incomplete",
                                  tuple(cut_errors))
        _, repeated = judge.parse_logfile(b"ok alpha::passes\nfailed alpha::passes\n")
        self.assertReason("records a test more than once: alpha::passes", tuple(repeated))


if __name__ == "__main__":
    unittest.main()
