#!/usr/bin/env python3
"""Run a cargo test lane and adjudicate, from its own output, exactly what ran.

`cargo test --lib <filter>` exits 0 when the filter matches zero tests, so a
required CI check can stay green while nothing executed; this repository has
recorded five such false greens (#5003, #5008, #5041, #5046, #5185). Filter
validation alone cannot close that hole: it inspects the command, not the run.

An execution *threshold* cannot close it either, and #5185's first round shipped
one. A floor is a scalar summary of a set, and every scalar summary admits the
same evasion: two measured experiments against a floor of 6500 -- narrowing one
module by 402 tests (executed 6539) and disabling a 213-test module
(executed 6708) -- both stayed above the floor and reported a green lane. #5144
already moved this repository's other identity pin off a scalar and onto a
sorted manifest set comparison that names both sides of the diff; this wrapper
follows the same convention.

The wrapper therefore derives the id set the lane is *supposed* to run from the
checked-in inventory manifest and compares it against the id set the run
actually reported, naming every id on either side of the difference:

1. **Selection identity** -- ``expected = (manifest - static-only + cargo-only)
   - skipped``, compared as a set against ``executed | ignored``. A module that
   stops compiling, a filter that quietly narrows, and a test that is deleted
   all surface as named missing ids instead of a smaller number.
2. **Ignore identity** -- ``#[ignore]`` moves a test out of *executed* while
   leaving it in *selected*, so marking 213 tests ignored would satisfy (1)
   alone. The set of ids reported ignored must equal the set the ledger
   declares with mode ``ignored``.
3. **Summary cross-validation** -- libtest's own ``test result:`` line must
   account for exactly the expected selection. Nested libtest runs (a test that
   re-executes the test binary) write their own result lines to the inherited
   stdout and would otherwise inflate the observed counts; more result lines
   than ``--max-summaries`` declares is an error, and the primary summary is
   cross-checked against the derived expectation.
3b. **Failure attribution, as a set** -- the counts in (3) are scalars, and two
   transcript mis-reads that cancel satisfy all of them at once (measured; see
   the parser comment below). libtest also prints a ``failures:`` block naming
   every failing id, so the set of ids parsed as failing is compared against
   that block, naming both sides of any difference. A summary that reports
   failures with no recoverable block is itself an error.
4. **Known-failure ledger** -- failures are compared against
   ``scripts/known_test_failures.txt``. A failure that is not listed fails the
   lane. A listed ``always`` entry whose tests all passed also fails the lane,
   so isolated debt cannot outlive the defect it documents. ``flaky`` entries
   must name an exact test id and carry an ``expires=YYYY-MM-DD`` date, and the
   lane fails once that date passes, so isolation carries expiry pressure
   instead of decaying into a permanent allowlist.

The wrapper never widens or narrows the command it is given. It observes the
run, and it verifies that the skip filters it was told about are exactly the
ones the command carries -- declaring a narrower lane to the adjudicator than
the one actually executed is the evasion this check exists to make impossible.
"""

from __future__ import annotations

import argparse
import datetime as _datetime
import re
import subprocess
import sys
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from check_test_target_integrity import (  # noqa: E402
    LIB_INVENTORY_KNOWN_CARGO_ONLY,
    expected_lib_static_only,
    load_lib_inventory_manifest,
)

# In multi-threaded mode libtest emits a result as two unsynchronised writes --
# `test <id> ... ` and then the verdict -- onto a stdout it shares with every
# test thread and with any child process that inherited the descriptor. A
# foreign write can therefore land before the name, between the name and the
# verdict, or between the verdict and its newline. Two of those were observed
# in real sweeps of this lane (`lane5185r2-f3.log:1216`, and
# `lane5185-sweep-3.log:1215`):
#
#   /var/.../agentdesk.plist: OKtest cli::…::required_or_unavailable… ...
#   ok
#   test cli::…::malformed_archived… ... /var/….plist: OK/var/….plist: OKok
#
# The first shape a `search` with a trailing `$` still parses, because the
# verdict is simply missing from the line and the next line supplies it. The
# second it drops entirely: `\s*(?:ok|FAILED|ignored)\b.*$` cannot span the
# foreign text, and there is no second `test ` token to re-anchor on. A test
# that PASSED is then reported as a missing id -- a false red, measured once in
# a five-run sweep, which is disqualifying for a required context.
#
# So parse a line as a *sequence* of libtest fragments rather than as one whole
# line: `finditer` every `test <id> ... ` name fragment, and attribute verdicts
# from the segments between them. A verdict is recognised only at the very
# start or the very end of a segment, which is where libtest's own write lands;
# an `ok` in the middle of a path or a message is never mistaken for one.
#
# When several names on one line are waiting for a verdict, attribution is
# last-in-first-out. That is a CHOICE, not a proof, and the earlier claim that
# libtest's write order proves it is wrong. `write_test_start` / `write_result`
# in rust 1.94's `library/test/src/formatters/pretty.rs` do show that in
# multi-threaded mode a single console thread writes a name and its verdict
# back to back -- but that is exactly why ONE libtest process can never leave
# two names pending. `test A ... test B ... ok` therefore requires two
# processes sharing the descriptor (this lane's nested
# `non_wait_relay_parent_subprocess_entry`, which is why it declares
# `--max-summaries 2`), and then the two possible orderings
#
#   W1-name, W2-name, W2-verdict, W1-verdict   (LIFO is right)
#   W1-name, W2-name, W1-verdict, W2-verdict   (LIFO is wrong)
#
# emit BYTE-IDENTICAL output. LIFO fixes one reading as the answer. No archived
# transcript of this lane contains a multi-name line at all (76 replayed, 0
# occurrences), and the failure-set check below turns a wrong choice into a
# named red rather than a silent green, which is why the ambiguity is resolved
# here instead of being rejected outright.
#
# WHAT THIS PARSER DOES AND DOES NOT CLOSE.
# The residual ambiguity is foreign text that ENDS in exactly `ok`, `FAILED` or
# `ignored`; no line-shape rule can tell that from a verdict. An earlier
# version of this comment claimed the cross-checks in `evaluate` made such a
# mis-read incapable of producing a false green. That was FALSE, and was
# measured false. With the ledger carrying `always | alpha::gamma`:
#
#   test alpha::beta  ... /var/.../.tmpQQ/agentdesk.plist: ok
#   FAILED
#   test alpha::gamma ... /var/.../.tmpZZ/lock: FAILED
#   ok
#
# gives `beta` the trailing `ok` and `gamma` the trailing `FAILED` -- the exact
# reverse of the truth. `executed`, `failed` and `selected` are COUNTS, and two
# mis-reads that cancel satisfy all three simultaneously: rc=0, unexpected=0,
# stale=0, with an unlisted failure silently green.
#
# So failure attribution is no longer adjudicated by a count. libtest prints a
# `failures:` block naming every failing id; `evaluate` compares that SET
# against the set parsed here, and a set difference cannot cancel. Precisely:
#
#   * a FAILED lost, invented, or attributed to the wrong id -- by any
#     mechanism, a wrong LIFO choice included -- fails the lane by name;
#   * a summary reporting failures with no recoverable `failures:` block fails
#     the lane, because which ids failed cannot then be adjudicated at all;
#   * an `ignored` mis-read fails the lane: the ignore set is pinned against
#     the ledger in both directions;
#   * an id invented by a test's own stdout fails as `lane-extra`; an id that
#     never reports a verdict fails as `lane-missing`.
#
# Still NOT closed. The list below is the whole of it; an earlier revision of
# this comment named the first two entries and stopped, which understated it.
#
#   1. A mis-read that swaps two PASSING verdicts. Harmless: both ids stay
#      executed and passing, and neither the ledger nor the selection set can
#      tell the difference.
#   2. A `failures:` block corrupted by an interleaved write. Yields a named
#      set difference, i.e. a false RED, not a false green.
#   3. Any test that writes a bare line `failures:` to the inherited stdout
#      puts `TranscriptScanner.feed` into block mode, and the four-space-
#      indented `a::b` lines that follow become `declared_failures`. A wholly
#      green run then fails on `declared-not-parsed`. Fail-closed, and no test
#      in this lane does it today -- but nothing forbids it.
#   4. A nested child libtest prints its own `failures:` block to the same
#      inherited stdout, and those ids are UNIONed into `declared_failures`
#      with the parent's. If a child fails on an id the parent never selected,
#      the lane fails naming an id absent from the manifest. This is the same
#      re-execution path that forces `--max-summaries 2`, so it is reachable by
#      construction rather than hypothetically. Fail-closed.
#   5. Foreign text at the START of a segment that begins with a verdict word
#      and continues with a NON-word character still steals a verdict:
#      the lookahead admits a `\W` continuation, so `ok: connect refused` reads
#      as `ok` and `ignored, using default` reads as `ignored` -- the latter
#      being indistinguishable from a real `ignored, <#[ignore] reason>`, which
#      is the shape the `,` case in `drain_verdicts` exists for. So the residual
#      is NOT only foreign text that *ends* in a verdict word, and it never was.
#
#      What that residual no longer covers is a verdict word followed by a WORD
#      character. `okhttp: connect` and `FAILED_upload_error` were read as
#      verdicts when `VERDICT_AT_START` carried no boundary at all; the
#      `(?=ok|FAILED|ignored|\W|$)` lookahead on `VERDICT_AT_START` refuses
#      both, and `tests/test_run_test_lane_5185.sh` §4g pins all three
#      directions -- 4g-1 rejects `okhttp:`, 4g-2 rejects `FAILED_upload_error`,
#      4g-3 requires that `okok` still parses as two verdicts.
#
#      That lookahead is what a `\b` boundary cannot be, and `\b` remains the
#      one form ruled out here: `\b` after `ok` requires a non-word character
#      next, and the merged-write case this parser exists to handle -- `okok`,
#      two verdicts written back to back with the newline lost -- has a word
#      character next, so the second verdict would be dropped and its id would
#      fail the lane as `lane-missing`. That is a false RED on a required
#      context, strictly worse than the false green it would prevent. A
#      lookahead admitting the next verdict word as its own boundary is not
#      answered by that argument, which is why it is the form applied.
#
#      `VERDICT_AT_END` deliberately keeps no boundary, because the loss
#      depends on WHERE one goes. A boundary placed BEFORE the verdict word
#      drops the measured `/var/….plist: OKok` shape: the `ok` in `OKok` is
#      preceded by `K`, a word character. A boundary placed AFTER it does not,
#      because the match already ends at end-of-segment. Only the leading
#      reading loses `OKok`, so the end anchor is left alone and §4h-6 pins
#      that `OKok` still yields its verdict.
#
#      For both anchors the false-green direction stays narrowed by the
#      `failures:` set comparison above, which is why this residual is carried
#      rather than closed.
#
# The closure argument in (2) rests on a premise worth stating, because it is
# not self-evident: block corruption is assumed to be INSERTION-only. An
# experiment that *substitutes* the block's contents so they match the parsed
# set does produce rc=0 -- but interleaving is a write landing between other
# writes, and it can only add bytes, never replace libtest's own. If some
# future mechanism could rewrite that block in place, the set comparison would
# stop being a closure and this comment would be wrong.
#
# Replaying all 76 archived transcripts of this lane -- including one
# 73-failure poison cascade -- the block set and the parsed set agree exactly,
# so the measured false-red cost of this check is 0.
NAME_FRAGMENT = re.compile(r"test (?P<id>[^\s:]+::\S*) \.\.\. ?")
VERDICT_AT_START = re.compile(r"^(?P<outcome>ok|FAILED|ignored)(?=ok|FAILED|ignored|\W|$)")
VERDICT_AT_END = re.compile(r"(?P<outcome>ok|FAILED|ignored)$")
# libtest's own list of failing ids, printed as `failures:` followed by one
# four-space-indented id per line. It is written by the console thread after
# every test thread has finished, which is why it survives the interleaving
# that corrupts the per-result lines.
FAILURES_HEADER = re.compile(r"^failures:$")
FAILURE_NAME = re.compile(r"^ {4}(?P<id>[^\s:]+::\S*)$")
SUMMARY_LINE = re.compile(
    r"test result: (?:ok|FAILED)\. (?P<passed>\d+) passed; (?P<failed>\d+) failed;"
    r" (?P<ignored>\d+) ignored;"
)
# Format only. There is no offline way to ask whether an issue exists, so this
# accepts `#1` as readily as `#5185`; the substantive admissibility checks are
# `reason_is_substantive` below and the stale/expiry pressure in `evaluate`.
ISSUE = re.compile(r"^#\d+$")
EXPIRES = re.compile(r"\bexpires=(\d{4}-\d{2}-\d{2})\b")
VALID_MODES = ("always", "flaky", "ignored")
MIN_REASON_CHARS = 12
# A reason has to say something. `aaaaaaaaaaaa` clears a length check, so
# require several distinct words of ordinary prose alongside the length.
MIN_REASON_WORDS = 4
MIN_REASON_DISTINCT_CHARS = 8


@dataclass(frozen=True)
class LedgerEntry:
    mode: str
    pattern: str
    lane: str
    issue: str
    reason: str
    lineno: int
    expires: _datetime.date | None = None

    def matches(self, test_id: str) -> bool:
        # `flaky` and `ignored` are exact-id only. Module-prefix `flaky`
        # isolation was measured to swallow unrelated regressions inside the
        # named module: an injected failure in a sibling test of an isolated
        # module produced rc=0 (#5185 review R-D).
        if self.mode in ("flaky", "ignored"):
            return test_id == self.pattern
        return test_id == self.pattern or test_id.startswith(self.pattern + "::")


@dataclass
class Summary:
    passed: int
    failed: int
    ignored: int

    @property
    def selected(self) -> int:
        return self.passed + self.failed + self.ignored


@dataclass
class Outcome:
    executed: set[str] = field(default_factory=set)
    failed: set[str] = field(default_factory=set)
    ignored: set[str] = field(default_factory=set)
    summaries: list[Summary] = field(default_factory=list)
    # Ids named by libtest's own `failures:` block(s), and how many blocks
    # contributed a name. Nested runs print their own, hence a union.
    declared_failures: set[str] = field(default_factory=set)


def reason_is_substantive(reason: str) -> bool:
    """Reject filler that clears a bare length check (`aaaaaaaaaaaa`)."""
    if len(reason) < MIN_REASON_CHARS:
        return False
    words = [word for word in re.split(r"\W+", reason) if word]
    if len(words) < MIN_REASON_WORDS:
        return False
    return len(set(reason.lower()) - {" "}) >= MIN_REASON_DISTINCT_CHARS


def parse_ledger(path: Path, lane: str, today: _datetime.date
                 ) -> tuple[list[LedgerEntry], list[str]]:
    """Return (entries for `lane`, malformed-entry errors across all lanes)."""
    errors: list[str] = []
    entries: list[LedgerEntry] = []
    seen: set[tuple[str, str]] = set()
    if not path.exists():
        return entries, [f"{path}: ledger file is missing"]
    for lineno, raw in enumerate(path.read_text("utf-8").splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        fields = [part.strip() for part in line.split("|")]
        if len(fields) != 5:
            errors.append(f"{path}:{lineno}: expected 5 '|'-separated fields, "
                          f"got {len(fields)}")
            continue
        mode, pattern, lane_field, issue, reason = fields
        if mode not in VALID_MODES:
            errors.append(f"{path}:{lineno}: mode must be one of "
                          f"{'/'.join(VALID_MODES)}, got {mode!r}")
        if not pattern:
            errors.append(f"{path}:{lineno}: empty test pattern")
        lane_value = lane_field[len("lane="):] if lane_field.startswith("lane=") else ""
        if not lane_value:
            errors.append(f"{path}:{lineno}: missing 'lane=<name>' field")
        if not ISSUE.match(issue):
            errors.append(f"{path}:{lineno}: missing tracking issue "
                          f"(want '#1234', got {issue!r})")
        if not reason_is_substantive(reason):
            errors.append(
                f"{path}:{lineno}: reason must explain the isolation in prose "
                f"(>= {MIN_REASON_CHARS} chars, >= {MIN_REASON_WORDS} words, "
                f">= {MIN_REASON_DISTINCT_CHARS} distinct characters)"
            )
        expires: _datetime.date | None = None
        expiry_match = EXPIRES.search(reason)
        if mode == "flaky":
            # Expiry pressure: a `flaky` entry passes silently by construction,
            # so nothing else can ever make it expire.
            if not expiry_match:
                errors.append(
                    f"{path}:{lineno}: a flaky entry must carry "
                    "'expires=YYYY-MM-DD'; without one it never expires"
                )
            else:
                try:
                    expires = _datetime.date.fromisoformat(expiry_match.group(1))
                except ValueError:
                    errors.append(f"{path}:{lineno}: invalid expires= date "
                                  f"{expiry_match.group(1)!r}")
                else:
                    if expires < today:
                        errors.append(
                            f"{path}:{lineno}: flaky entry for {pattern} expired "
                            f"on {expires.isoformat()}. Fix the defect or "
                            "re-date the entry with fresh evidence."
                        )
        key = (lane_value, pattern)
        if key in seen:
            errors.append(f"{path}:{lineno}: duplicate entry for {pattern}")
        seen.add(key)
        if lane_value == lane and mode in VALID_MODES and pattern:
            entries.append(LedgerEntry(mode, pattern, lane_value, issue, reason,
                                       lineno, expires))
    return entries, errors


def derive_expected_selection(manifest_path: Path, platform_name: str,
                              skips: list[str]) -> tuple[frozenset[str], list[str]]:
    """Derive the id set this lane must select, from the checked-in manifest.

    The manifest is the *static* inventory. libtest lists what compiled for the
    host, so the two differ by the exact, reviewed sets #5144 pinned; reuse
    those constants rather than restating them.
    """
    try:
        manifest_ids = load_lib_inventory_manifest(manifest_path)
    except (OSError, ValueError) as error:
        return frozenset(), [f"inventory manifest: {error}"]
    static_only = expected_lib_static_only(platform_name)
    if static_only is None:
        return frozenset(), [
            f"inventory manifest: unsupported platform {platform_name!r}; the "
            "static-only set is reviewed data and cannot be guessed"
        ]
    runtime_ids = (manifest_ids - static_only) | LIB_INVENTORY_KNOWN_CARGO_ONLY
    selected = {test_id for test_id in runtime_ids
                if not any(skip in test_id for skip in skips)}
    return frozenset(selected), []


def declared_skips_in_command(command: list[str]) -> list[str]:
    """Extract `--skip <pat>` / `--skip=<pat>` from the libtest half."""
    found: list[str] = []
    index = 0
    while index < len(command):
        word = command[index]
        if word == "--skip" and index + 1 < len(command):
            found.append(command[index + 1])
            index += 2
            continue
        if word.startswith("--skip="):
            found.append(word[len("--skip="):])
        index += 1
    return found


def drain_verdicts(segment: str, pending: list[str],
                   record: Callable[[str, str], None]) -> None:
    """Attribute the verdict fragments in `segment` to the ids awaiting one.

    A segment is whatever sits between two libtest name fragments, so it holds
    zero or more verdicts with arbitrary foreign text around them. A verdict is
    accepted only at the very start of the segment (the uncorrupted case, and
    the second half of a merged `okok`) or at its very end (a foreign write
    landed between the name and the verdict). Refusing mid-segment matches is
    what keeps a path such as `/tmp/.tmpok9Z/x` from being read as a verdict.
    """
    while pending:
        match = VERDICT_AT_START.match(segment)
        if not match:
            break
        record(pending.pop(), match.group("outcome"))
        segment = segment[match.end():]
        if segment.startswith(","):
            # `ignored, <#[ignore] reason>` -- the rest is prose, not verdicts.
            return
    if pending:
        match = VERDICT_AT_END.search(segment)
        if match:
            record(pending.pop(), match.group("outcome"))


def scan_line(line: str, pending: list[str],
              record: Callable[[str, str], None]) -> None:
    """Feed one transcript line through the fragment parser."""
    fragments = list(NAME_FRAGMENT.finditer(line))
    if not fragments:
        drain_verdicts(line, pending, record)
        return
    drain_verdicts(line[:fragments[0].start()], pending, record)
    for index, fragment in enumerate(fragments):
        pending.append(fragment.group("id"))
        end = (fragments[index + 1].start() if index + 1 < len(fragments)
               else len(line))
        drain_verdicts(line[fragment.end():end], pending, record)


class TranscriptScanner:
    """Accumulate one libtest transcript into an `Outcome`, line by line.

    Kept separate from the subprocess plumbing so that replaying an archived
    transcript exercises exactly the code a live lane runs.
    """

    def __init__(self, outcome: Outcome) -> None:
        self.outcome = outcome
        self._pending: list[str] = []
        self._in_failures_block = False

    def _record(self, test_id: str, verdict: str) -> None:
        if verdict == "ignored":
            self.outcome.ignored.add(test_id)
        else:
            self.outcome.executed.add(test_id)
            if verdict == "FAILED":
                self.outcome.failed.add(test_id)

    def feed(self, line: str) -> None:
        """Consume one transcript line, newline already stripped."""
        # libtest prints `failures:` twice: once heading the captured-stdout
        # dumps, once heading the sorted id list. Only the second contributes
        # names, because the first is followed by `---- <id> stdout ----`
        # lines, which end the block on the first non-blank non-name line.
        if self._in_failures_block:
            named = FAILURE_NAME.match(line)
            if named:
                self.outcome.declared_failures.add(named.group("id"))
                return
            if not line.strip():
                return
            self._in_failures_block = False
        if FAILURES_HEADER.match(line):
            self._in_failures_block = True
            return
        scan_line(line, self._pending, self._record)
        summary = SUMMARY_LINE.search(line)
        if summary:
            self.outcome.summaries.append(Summary(
                int(summary.group("passed")),
                int(summary.group("failed")),
                int(summary.group("ignored")),
            ))


def run_and_capture(command: list[str], log_path: Path | None) -> tuple[int, Outcome]:
    outcome = Outcome()
    scanner = TranscriptScanner(outcome)
    sink = log_path.open("w", encoding="utf-8") if log_path else None
    try:
        process = subprocess.Popen(command, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, text=True,
                                   bufsize=1)
        assert process.stdout is not None

        for line in process.stdout:
            sys.stdout.write(line)
            if sink:
                sink.write(line)
            scanner.feed(line.rstrip("\n"))
        return process.wait(), outcome
    finally:
        if sink:
            sink.close()
        sys.stdout.flush()


def _report_set(errors: list[str], label: str, ids: set[str] | frozenset[str],
                explanation: str) -> None:
    if not ids:
        return
    for test_id in sorted(ids):
        print(f"{label}: {test_id}", file=sys.stderr)
    errors.append(f"{len(ids)} {label} id(s), named above. {explanation}")


def evaluate(lane: str, rc: int, outcome: Outcome, entries: list[LedgerEntry],
             expected_selected: frozenset[str], max_summaries: int
             ) -> tuple[list[str], list[str], dict[str, int]]:
    errors: list[str] = []
    warnings: list[str] = []

    declared_ignored = {entry.pattern for entry in entries
                        if entry.mode == "ignored"}
    observed_selected = outcome.executed | outcome.ignored
    expected_executed = expected_selected - declared_ignored

    # (1) Selection identity -- the set the lane must run, named on both sides.
    _report_set(
        errors, "lane-missing (expected but never reported)",
        expected_selected - observed_selected,
        "The lane did not run tests it is declared to run. Restore the "
        "selection, or regenerate the inventory manifest if the tests were "
        "deliberately deleted.",
    )
    _report_set(
        errors, "lane-extra (reported but not expected)",
        observed_selected - expected_selected,
        "The run reported ids the manifest does not contain. Regenerate the "
        "inventory manifest and review the diff.",
    )

    # (2) Ignore identity -- `#[ignore]` must not be a silent exit from (1).
    _report_set(
        errors, "undeclared-ignored", outcome.ignored - declared_ignored,
        "A test reported itself ignored without a ledger entry declaring it. "
        "Marking tests #[ignore] is how a lane keeps its selection while "
        "running less of it.",
    )
    _report_set(
        errors, "stale-ignored", declared_ignored - outcome.ignored,
        "The ledger declares these ignored but the run did not report them "
        "ignored. Remove the entry.",
    )
    _report_set(
        errors, "not-executed", expected_executed - outcome.executed,
        "These are expected to execute and did not report a result.",
    )

    # (3) Summary cross-validation. A nested libtest run writes its own result
    #     line to the inherited stdout; measured, that inflated `executed` by
    #     one against libtest's own count in the real sweep (#5185 review R-A).
    if not outcome.summaries:
        errors.append("no libtest 'test result:' line was emitted; the run "
                      "produced no summary to cross-check against")
    else:
        if len(outcome.summaries) > max_summaries:
            errors.append(
                f"{len(outcome.summaries)} libtest summary lines, but the lane "
                f"declares at most {max_summaries}. Extra summaries come from "
                "tests that re-execute the test binary and their output is "
                "counted as if it were this run's."
            )
        primary = max(outcome.summaries, key=lambda item: item.selected)
        if primary.selected != len(expected_selected):
            errors.append(
                f"libtest reported {primary.selected} selected tests "
                f"({primary.passed} passed + {primary.failed} failed + "
                f"{primary.ignored} ignored) but the manifest derives "
                f"{len(expected_selected)} for this lane."
            )
        # Cross-validate the parsed sets against libtest's own arithmetic.
        # `executed` is deduplicated against the declared ignore set first: a
        # test that re-executes the test binary makes its child report an
        # `#[ignore]`d entry point as `ok`, which is counted here but not by
        # this run's summary. Measured, that inflated the raw count by exactly
        # one against libtest's 6929.
        executed_attributed = outcome.executed - declared_ignored
        if len(executed_attributed) != primary.passed + primary.failed:
            errors.append(
                f"parsed {len(executed_attributed)} executed ids but libtest "
                f"reported {primary.passed + primary.failed} "
                f"({primary.passed} passed + {primary.failed} failed); the "
                "transcript and its own summary disagree."
            )
        if len(outcome.failed) != primary.failed:
            errors.append(
                f"parsed {len(outcome.failed)} failing ids but libtest reported "
                f"{primary.failed} failed."
            )

        # (3b) Failure attribution as a SET, not a count. Everything above is
        # a scalar, and two transcript mis-reads that CANCEL satisfy all of
        # them at once -- measured: a transcript that gave a failing test the
        # trailing `ok` of a foreign write and a passing test the trailing
        # `FAILED` of another reported rc=0 unexpected=0 stale=0 with an
        # unlisted failure green. libtest's own `failures:` block names the
        # failing ids, so compare the two sets and name both sides; a set
        # difference has no cancelling counterpart.
        if primary.failed and not outcome.declared_failures:
            errors.append(
                f"libtest reported {primary.failed} failing test(s) but no "
                "`failures:` block naming them could be recovered from the "
                "transcript. Which ids failed cannot be adjudicated, so the "
                "ledger cannot be applied and the run is not green."
            )
        else:
            _report_set(
                errors,
                "failure-unattributed (in libtest's failures: block, not parsed as failing)",
                outcome.declared_failures - outcome.failed,
                "libtest says these failed and the transcript parse does not. "
                "A verdict was lost or attributed to another id.",
            )
            _report_set(
                errors,
                "failure-misattributed (parsed as failing, absent from libtest's failures: block)",
                outcome.failed - outcome.declared_failures,
                "The transcript parse says these failed and libtest's own "
                "list does not. A foreign write was read as a verdict, or a "
                "verdict was attributed to the wrong id.",
            )

    known: set[str] = set()
    unexpected: list[str] = []
    for test_id in sorted(outcome.failed):
        entry = next((item for item in entries
                      if item.mode != "ignored" and item.matches(test_id)), None)
        if entry is None:
            unexpected.append(test_id)
        else:
            known.add(test_id)
            print(f"known-failure: {test_id} [{entry.mode}] {entry.issue} "
                  f"{entry.reason}")
    for test_id in unexpected:
        errors.append(
            f"unlisted test failure: {test_id}. Fix it, or add it to the "
            "known-failure ledger with a tracking issue and a reason."
        )

    stale = 0
    for entry in entries:
        if entry.mode == "ignored":
            continue
        covered = {test for test in outcome.executed if entry.matches(test)}
        if not covered:
            print(f"ledger-note: {entry.pattern} selected no test in lane {lane}")
            continue
        if covered & outcome.failed:
            continue
        if entry.mode == "always":
            stale += 1
            errors.append(
                f"stale ledger entry: {entry.pattern} ({entry.issue}) is "
                f"recorded as always failing but all {len(covered)} selected "
                "test(s) passed. Remove the entry so the debt does not outlive "
                "the defect."
            )
        else:
            warnings.append(
                f"known-flaky entry passed: {entry.pattern} ({entry.issue}); "
                f"expires {entry.expires.isoformat() if entry.expires else '?'}."
            )

    if rc != 0 and not outcome.failed:
        errors.append(
            f"command exited {rc} without reporting a failing test; treat this "
            "as a harness or compile failure, not a green lane."
        )
    if rc == 0 and outcome.failed:
        errors.append("command exited 0 while libtest reported failures")

    counters = {
        "executed": len(outcome.executed - declared_ignored),
        "passed": len(outcome.executed - declared_ignored) - len(outcome.failed),
        "failed": len(outcome.failed),
        "declared-failed": len(outcome.declared_failures),
        "known": len(known),
        "unexpected": len(unexpected),
        "stale": stale,
        "flaky-passed": len(warnings),
        "expected": len(expected_selected),
        "missing": len(expected_selected - observed_selected),
        "extra": len(observed_selected - expected_selected),
        "ignored": len(outcome.ignored),
        "summaries": len(outcome.summaries),
        "rc": rc,
    }
    return errors, warnings, counters


def _print_summary(lane: str, counters: dict[str, int]) -> None:
    rendered = " ".join(f"{key}={value}" for key, value in counters.items())
    print(f"test-lane summary: lane={lane} {rendered}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lane", required=True)
    parser.add_argument("--inventory-manifest", type=Path,
                        default=Path(__file__).resolve().parent
                        / "lib_test_inventory_manifest.txt")
    parser.add_argument("--skip", action="append", default=[],
                        help="libtest --skip pattern this lane declares; must "
                             "match the command exactly")
    parser.add_argument("--max-summaries", type=int, default=1,
                        help="libtest 'test result:' lines this lane declares; "
                             "nested test-binary re-execution emits extras")
    parser.add_argument("--platform", default=sys.platform)
    parser.add_argument("--ledger", type=Path,
                        default=Path(__file__).resolve().parent
                        / "known_test_failures.txt")
    parser.add_argument("--log", type=Path)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)

    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("a command to run is required after `--`")
    if args.max_summaries < 1:
        parser.error("--max-summaries must be at least 1")

    today = _datetime.date.today()
    entries, ledger_errors = parse_ledger(args.ledger, args.lane, today)
    expected_selected, manifest_errors = derive_expected_selection(
        args.inventory_manifest, args.platform, args.skip)

    # A lane that declares fewer skips than it executes would shrink its own
    # expectation to match whatever it chose to run. Refuse before running.
    command_skips = declared_skips_in_command(command)
    skip_errors: list[str] = []
    if sorted(command_skips) != sorted(args.skip):
        skip_errors.append(
            f"declared --skip {sorted(args.skip)} does not match the command's "
            f"--skip {sorted(command_skips)}; the adjudicator must be told the "
            "same selection the command executes"
        )
    if not expected_selected and not manifest_errors:
        skip_errors.append("the derived expectation is empty; a lane that may "
                           "run zero tests cannot prove it ran")

    startup_errors = ledger_errors + manifest_errors + skip_errors
    if startup_errors:
        for error in startup_errors:
            print(f"ERROR {error}", file=sys.stderr)
        _print_summary(args.lane, {
            "executed": 0, "passed": 0, "failed": 0, "declared-failed": 0,
            "known": 0, "unexpected": 0, "stale": 0, "flaky-passed": 0,
            "expected": len(expected_selected), "missing": len(expected_selected),
            "extra": 0, "ignored": 0, "summaries": 0, "rc": -1,
        })
        return 1

    rc, outcome = run_and_capture(command, args.log)
    errors, warnings, counters = evaluate(args.lane, rc, outcome, entries,
                                          expected_selected, args.max_summaries)
    for warning in warnings:
        print(f"::warning::{warning}")
    for error in errors:
        print(f"ERROR {error}", file=sys.stderr)
    _print_summary(args.lane, counters)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
