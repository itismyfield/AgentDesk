#!/usr/bin/env python3
"""Cap the comment ratio of *newly added* production Rust files.

The existing ratchets do not cover this. ``hotfile_ratchet`` only stops
already-huge files from re-expanding, and both giant-file gates start at 1,000
lines (``giant_file_progress.py``, ``check_agent_maintenance_docs.py``), so a
brand-new 999-line file passes every gate in the tree. PR #5953 shipped a
711-line new file that was 398 lines of comment and registered nowhere.

A file fails when ``comment_lines / (comment_lines + code_lines) > 25%``. The
denominator excludes blank lines so padding cannot dilute the ratio, and lines
inside ``#[cfg(test)] mod`` blocks are excluded from both sides.

Out of scope by design: files that already exist (retroactive enforcement would
red 200+ files), test files, and files under the measurable-size floor.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from generate_inventory_docs import test_line_numbers  # noqa: E402
from rust_lex import CODE, COMMENT, LITERAL, StripState, lex_segments  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parent.parent
WORKING_TREE = "<working tree>"
MAX_COMMENT_RATIO = 0.25
# Below this many measurable lines the ratio is noise: a four-line `mod.rs`
# with a one-line header is already at the cap.
MIN_MEASURED_LINES = 20
BASE_REF_ENV = "AGENTDESK_NEW_FILE_COMMENT_RATIO_BASE_REF"

TEST_FILE_NAMES = frozenset({"tests.rs", "integration_tests.rs"})
TEST_FILE_SUFFIXES = ("_test.rs", "_tests.rs")


class GitError(RuntimeError):
    pass


@dataclass(frozen=True)
class Measurement:
    path: str
    comment_lines: int
    code_lines: int

    @property
    def measured(self) -> int:
        return self.comment_lines + self.code_lines

    @property
    def ratio(self) -> float:
        return self.comment_lines / self.measured if self.measured else 0.0


def git(*args: str, binary: bool = False) -> str | bytes:
    result = subprocess.run(
        ["git", "-C", str(REPO_ROOT), *args], check=False, capture_output=True
    )
    if result.returncode != 0:
        raise GitError(
            f"git {' '.join(args)} failed: "
            f"{result.stderr.decode('utf-8', 'replace').strip()}"
        )
    return result.stdout if binary else result.stdout.decode("utf-8")


def is_test_path(path: str) -> bool:
    parts = path.split("/")
    name = parts[-1]
    if name in TEST_FILE_NAMES or name.endswith(TEST_FILE_SUFFIXES):
        return True
    return any(part == "tests" or part.endswith("_tests") for part in parts[:-1])


def measure(path: str, text: str) -> Measurement:
    skipped = test_line_numbers(text)
    state = StripState()
    comment_lines = code_lines = 0
    for lineno, line in enumerate(text.splitlines(), start=1):
        inside_literal = state.in_string or state.raw_hashes is not None
        kinds = {kind for kind, _ in lex_segments(line, state)}
        if lineno in skipped:
            continue
        if not inside_literal and COMMENT in kinds and not kinds & {CODE, LITERAL}:
            comment_lines += 1
        elif line.strip():
            code_lines += 1
    return Measurement(path, comment_lines, code_lines)


def default_base_ref() -> str:
    configured = os.environ.get(BASE_REF_ENV)
    if configured:
        return configured
    github_base = os.environ.get("GITHUB_BASE_REF")
    if github_base:
        return f"origin/{github_base}"
    try:
        git("rev-parse", "--verify", "origin/main")
    except GitError:
        return "main"
    return "origin/main"


def added_rust_files(base_ref: str, head_ref: str) -> tuple[str, list[str]]:
    head = "HEAD" if head_ref == WORKING_TREE else head_ref
    base = git("merge-base", base_ref, head).strip()
    args = ["diff", "--diff-filter=A", "--find-renames", "--name-only", "-z", base]
    if head_ref != WORKING_TREE:
        args.append(head_ref)
    paths = [path for path in git(*args).split("\0") if path.endswith(".rs")]
    return base, paths


def read_file(head_ref: str, path: str) -> str | None:
    if head_ref == WORKING_TREE:
        candidate = REPO_ROOT / path
        raw = candidate.read_bytes() if candidate.is_file() else None
    else:
        try:
            raw = git("show", f"{head_ref}:{path}", binary=True)
        except GitError:
            raw = None
    if raw is None:
        return None
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError:
        return None


def check(base_ref: str, head_ref: str, max_ratio: float, min_lines: int) -> int:
    base, paths = added_rust_files(base_ref, head_ref)
    print(f"base: {base_ref} -> {base}")
    print(f"head: {head_ref}")

    measurements: list[Measurement] = []
    for path in paths:
        if is_test_path(path):
            continue
        text = read_file(head_ref, path)
        if text is None:
            print(f"::warning::skipping {path}: not readable as UTF-8 text")
            continue
        measurements.append(measure(path, text))

    print(f"added production Rust files: {len(measurements)}")
    over = [
        item
        for item in measurements
        if item.measured >= min_lines and item.ratio > max_ratio
    ]
    for item in sorted(measurements, key=lambda item: -item.ratio):
        flag = " <- over cap" if item in over else ""
        print(
            f"  {item.ratio:6.1%}  {item.comment_lines:5d}/{item.measured:<5d}  "
            f"{item.path}{flag}"
        )
    if not over:
        print(f"OK: no new production file exceeds the {max_ratio:.0%} comment-line cap")
        return 0

    print("")
    print(f"FAIL: {len(over)} new production file(s) over the {max_ratio:.0%} cap")
    for item in over:
        allowed = int(max_ratio * item.measured)
        print(
            f"  {item.path}: {item.comment_lines} comment lines of "
            f"{item.measured} measured ({item.ratio:.1%}); "
            f"at most {allowed} are allowed"
        )
    print("")
    print("Cut the comments, do not raise the cap. Comments that restate the code,")
    print("or record how the code used to be, belong in the PR body instead.")
    return 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base-ref", default=None, help=f"base revision (default: ${BASE_REF_ENV}, $GITHUB_BASE_REF, origin/main)")
    parser.add_argument("--head-ref", default=WORKING_TREE, help="head revision (default: working tree)")
    args = parser.parse_args(argv)
    # The cap is policy, not a knob: no CLI flag can raise it.
    try:
        return check(
            args.base_ref or default_base_ref(),
            args.head_ref,
            MAX_COMMENT_RATIO,
            MIN_MEASURED_LINES,
        )
    except GitError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
