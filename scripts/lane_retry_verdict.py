"""Dormant judge for re-running a lane's failed tests once: FLAKY, REAL or unretryable.

It reuses run_test_lane's structure checks and cross-checks them against the parent libtest --logfile.
"""

from __future__ import annotations

import hashlib
import re
import sys
from collections import Counter
from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from run_test_lane import LedgerEntry, Outcome, verify_structure  # noqa: E402

# libtest exits 101 when tests ran and some failed; signals and aborts never yield it directly.
LIBTEST_FAILURE_RC = 101
CLEAN, RETRYABLE, UNRETRYABLE = "clean", "retryable", "unretryable"
FLAKY, REAL = "flaky", "real"
OK, FAILED, IGNORED, TIMED_OUT = "ok", "failed", "ignored", "timed-out"
MAX_NAMED = 20

TEST_ID = r"(?P<id>[^\s:]+::\S*)"
DEBUG_STR = r'"(?:[^"\\]|\\.)*"'
# Rust 1.94.1 console.rs write_log_result: "<verdict> <name>\n" per finished test.
ONE_LINE_RECORD = re.compile(
    r"^(?:(?P<ok>ok)|(?P<ignored>ignored(?:: .*)?)|(?P<timed>failed \(time limit exceeded\))"
    r"|(?P<failed>failed(?:: test did not panic as expected(?: at .+:\d+:\d+)?"
    r"|: panicked after reporting success)?)) " + TEST_ID + r"$"
)
# Rust 1.94.1 test_result.rs calc_result: the two should_panic mismatch messages span three lines.
MULTI_LINE_RECORDS = (
    (re.compile(r"^failed: panic did not contain expected string$"),
     re.compile(rf"^      panic message: {DEBUG_STR}$"),
     re.compile(rf"^ expected substring: {DEBUG_STR} {TEST_ID}$")),
    (re.compile(r"^failed: expected panic with string value,$"),
     re.compile(r"^ found non-string value: `TypeId\(0x[0-9a-f]{32}\)`$"),
     re.compile(rf"^     expected substring: {DEBUG_STR} {TEST_ID}$")),
)


@dataclass(frozen=True)
class LogRecord:
    verdict: str
    test_id: str


@dataclass(frozen=True)
class BinaryIdentity:
    path: str
    sha256: str


@dataclass(frozen=True)
class Assessment:
    status: str
    retry_ids: tuple[str, ...] = ()
    errors: tuple[str, ...] = ()
    binary: BinaryIdentity | None = None
    declared_ignored: frozenset[str] = frozenset()
    max_summaries: int = 1


@dataclass(frozen=True)
class RetryRun:
    test_id: str
    binary: BinaryIdentity | None
    rc: int
    timed_out: bool
    outcome: Outcome
    logfile: bytes | None


@dataclass(frozen=True)
class Verdict:
    status: str
    flaky: tuple[str, ...] = ()
    real: tuple[str, ...] = ()
    errors: tuple[str, ...] = ()

    @property
    def green(self) -> bool:
        return self.status == FLAKY


def _names(ids: Iterable[str]) -> str:
    ordered = sorted(ids)
    more = f" (+{len(ordered) - MAX_NAMED} more)" if len(ordered) > MAX_NAMED else ""
    return ", ".join(ordered[:MAX_NAMED]) + more


def _name(errors: list[str], label: str, ids: set[str] | frozenset[str]) -> None:
    if ids:
        errors.append(f"{label}: {_names(ids)}")


def binary_identity(path: Path) -> BinaryIdentity:
    """Resolved path and content digest of a test binary, taken right before each spawn."""
    resolved = Path(path).resolve(strict=True)
    with resolved.open("rb") as handle:
        return BinaryIdentity(str(resolved), hashlib.file_digest(handle, "sha256").hexdigest())


def parse_logfile(data: bytes | None) -> tuple[list[LogRecord], list[str]]:
    """Parse a libtest --logfile; a line outside the 1.94.1 grammar ends the parse as an error."""
    if data is None:
        return [], ["no libtest --logfile was written"]
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        return [], ["the logfile is not UTF-8"]
    if text and not text.endswith("\n"):
        return [], ["the logfile ends mid-record, so it was cut off"]
    lines = text.split("\n")[:-1]
    records: list[LogRecord] = []
    index = 0
    while index < len(lines):
        shape = next((s for s in MULTI_LINE_RECORDS if s[0].match(lines[index])), None)
        if shape:
            body = lines[index + 1:index + 3]
            last = shape[2].match(body[1]) if len(body) == 2 and shape[1].match(body[0]) else None
            if not last:
                return records, [f"logfile line {index + 1}: a three-line should_panic "
                                 "failure record is incomplete"]
            records.append(LogRecord(FAILED, last.group("id")))
            index += 3
            continue
        match = ONE_LINE_RECORD.match(lines[index])
        if not match:
            return records, [f"logfile line {index + 1} is not a libtest result record: "
                             f"{lines[index][:120]!r}"]
        kind = next(key for key in ("ok", "ignored", "timed", "failed") if match.group(key))
        verdict = {"ok": OK, "ignored": IGNORED, "timed": TIMED_OUT, "failed": FAILED}[kind]
        records.append(LogRecord(verdict, match.group("id")))
        index += 1
    repeated = {test_id for test_id, count in Counter(r.test_id for r in records).items()
                if count > 1}
    if repeated:
        return records, [f"the logfile records a test more than once: {_names(repeated)}"]
    return records, []


def _cross_check_logfile(records: list[LogRecord], outcome: Outcome,
                         expected_selected: frozenset[str]) -> list[str]:
    """The parent logfile names each selected test once and agrees with stdout on every verdict."""
    errors: list[str] = []
    by_verdict = {verdict: {r.test_id for r in records if r.verdict == verdict}
                  for verdict in (OK, FAILED, IGNORED, TIMED_OUT)}
    logged = {r.test_id for r in records}
    _name(errors, "selected but missing from the logfile", expected_selected - logged)
    _name(errors, "in the logfile but not selected", logged - expected_selected)
    _name(errors, "hit libtest's time limit, which is never retried", by_verdict[TIMED_OUT])
    printed = {
        FAILED: outcome.failed,
        IGNORED: outcome.ignored,
        OK: outcome.executed - outcome.failed - outcome.ignored,
    }
    for verdict, stdout_ids in printed.items():
        _name(errors, f"{verdict} in the logfile only", by_verdict[verdict] - stdout_ids)
        _name(errors, f"{verdict} on stdout only", stdout_ids - by_verdict[verdict])
    return errors


def assess_original_run(rc: int, timed_out: bool, outcome: Outcome, logfile: bytes | None,
                        entries: list[LedgerEntry], expected_selected: frozenset[str],
                        max_summaries: int, binary: BinaryIdentity | None) -> Assessment:
    """Decide whether a finished lane run may re-run its failed tests once, and which ones."""
    declared_ignored = frozenset(e.pattern for e in entries if e.mode == "ignored")

    def decide(errors: list[str], failed: tuple[str, ...] = ()) -> Assessment:
        status = UNRETRYABLE if errors else (RETRYABLE if failed else CLEAN)
        return Assessment(status, () if errors else failed, tuple(errors), binary,
                          declared_ignored, max_summaries)

    if timed_out:
        return decide(["the run hit its deadline; partial or late results are never retried"])
    if not expected_selected:
        return decide(["the lane expects no test, so a run cannot prove it ran"])
    errors = verify_structure(outcome, declared_ignored, expected_selected, max_summaries)
    if not outcome.failed:
        if rc != 0:
            errors.append(f"exited {rc} without a failing test: a harness or compile "
                          "failure is not retried")
        return decide(errors)
    if rc != LIBTEST_FAILURE_RC:
        errors.append(f"exited {rc}, not libtest's test-failure code {LIBTEST_FAILURE_RC}: "
                      "an abnormal exit is not retried")
    if binary is None:
        errors.append("the test binary identity was not recorded")
    records, log_errors = parse_logfile(logfile)
    errors += log_errors or _cross_check_logfile(records, outcome, expected_selected)
    return decide(errors, tuple(sorted(r.test_id for r in records if r.verdict == FAILED)))


def _stdout_disagreements(outcome: Outcome, test_id: str, verdict: str,
                          assessment: Assessment) -> list[str]:
    """Stdout shows this test with the logfile's verdict; nested runs add only declared-ignored passes."""
    found: list[str] = []
    want = (1, 0, 0) if verdict == OK else (0, 1, 0)
    if not outcome.summaries:
        found.append("printed no libtest summary")
    elif len(outcome.summaries) > assessment.max_summaries:
        found.append(f"printed {len(outcome.summaries)} summaries but the lane declares at "
                     f"most {assessment.max_summaries}")
    else:
        last = outcome.summaries[-1]
        if (last.passed, last.failed, last.ignored) != want:
            found.append(f"its own summary says {last.passed} passed, {last.failed} failed, "
                         f"{last.ignored} ignored")
    failed = {test_id} if verdict == FAILED else set()
    if test_id not in outcome.executed:
        found.append("stdout never reported its result")
    if outcome.failed != failed or outcome.declared_failures != failed:
        found.append(f"stdout failures [{_names(outcome.failed | outcome.declared_failures)}] "
                     f"disagree with the logfile verdict {verdict}")
    foreign = outcome.ignored | (outcome.executed - {test_id} - assessment.declared_ignored)
    if foreign:
        found.append(f"stdout reported ignored or other tests: {_names(foreign)}")
    return found


def _judge_run(run: RetryRun, assessment: Assessment, errors: list[str]) -> str | None:
    """Return the re-run's verdict when its evidence is complete and consistent, else None."""
    found: list[str] = []
    if run.timed_out:
        found.append("hit its deadline")
    if run.binary != assessment.binary:
        found.append("ran a different test binary than the original run")
    records, log_errors = parse_logfile(run.logfile)
    found += log_errors
    verdict = None
    if not log_errors:
        if not records:
            found.append("ran no test")
        elif [r.test_id for r in records] != [run.test_id]:
            found.append(f"logged [{_names(r.test_id for r in records)}] instead of exactly "
                         "this test")
        else:
            verdict = records[0].verdict
    if verdict in (IGNORED, TIMED_OUT):
        found.append(f"was reported {verdict}, which neither passes nor fails it")
    elif verdict is not None:
        expected_rc = 0 if verdict == OK else LIBTEST_FAILURE_RC
        if run.rc != expected_rc:
            found.append(f"exited {run.rc} although its logfile says {verdict}")
        found += _stdout_disagreements(run.outcome, run.test_id, verdict, assessment)
    errors += [f"retry of {run.test_id}: {item}" for item in found]
    return None if found else verdict


def judge_retries(assessment: Assessment, runs: list[RetryRun]) -> Verdict:
    """FLAKY only when every original failure was re-run alone exactly once and passed."""
    if assessment.status != RETRYABLE:
        errors = list(assessment.errors) or ["the original run has no failure to retry"]
        if runs:
            errors.append(f"{len(runs)} retry run(s) were made although the original run "
                          "may not be retried")
        return Verdict(UNRETRYABLE, errors=tuple(errors))
    errors: list[str] = []
    wanted = set(assessment.retry_ids)
    requested = Counter(run.test_id for run in runs)
    if len(runs) != len(wanted):
        errors.append(f"{len(wanted)} failed test(s) but {len(runs)} retry run(s); each "
                      "failure is re-run exactly once")
    _name(errors, "never retried", wanted - set(requested))
    _name(errors, "retried but not an original failure", set(requested) - wanted)
    _name(errors, "retried more than once", {i for i, count in requested.items() if count > 1})
    passed: set[str] = set()
    failed: set[str] = set()
    for run in runs:
        verdict = _judge_run(run, assessment, errors)
        if verdict == OK:
            passed.add(run.test_id)
        elif verdict == FAILED:
            failed.add(run.test_id)
    if errors:
        return Verdict(UNRETRYABLE, real=tuple(sorted(failed)), errors=tuple(errors))
    if failed:
        return Verdict(REAL, real=tuple(sorted(failed)),
                       errors=tuple(f"failed again when re-run alone: {i}" for i in sorted(failed)))
    return Verdict(FLAKY, flaky=tuple(sorted(passed)))
