#!/usr/bin/env python3
"""Reject production Rust diffs adding more comment rows than code rows.

Scope: src/**/*.rs and build.rs, excluding test paths and cfg(test) modules.
Generator outputs outside this scope are excluded; source header claims do
not exempt a file. Blank rows count for neither side; mixed code/comment rows
count as code. Exact comment rows moved across files cancel as a multiset.
Comment corrections/deletions pass only when every changed source has the
same comment-free normal form and no net new comment rows. New files above
25% comment density emit warnings (minimum 20 measured rows).
"""

from __future__ import annotations

import argparse
import difflib
import os
import subprocess
import sys
from dataclasses import dataclass
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from generate_inventory_docs import test_line_numbers  # noqa: E402
from check_comment_only_change import normalize  # noqa: E402
from rust_lex import CODE, COMMENT, LITERAL, StripState, lex_segments, strip_line  # noqa: E402

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


def line_kinds(text: str) -> list[str | None]:
    state = StripState()
    skipped = test_line_numbers("\n".join(strip_line(row, state) for row in text.splitlines()))
    state = StripState()
    result: list[str | None] = []
    for lineno, line in enumerate(text.splitlines(), start=1):
        inside_literal = state.in_string or state.raw_hashes is not None
        kinds = {kind for kind, _ in lex_segments(line, state)}
        if lineno in skipped:
            result.append(None)
        elif not inside_literal and COMMENT in kinds and not kinds & {CODE, LITERAL}:
            result.append(COMMENT)
        elif line.strip():
            result.append(CODE)
        else:
            result.append(None)
    return result


def measure(path: str, text: str) -> Measurement:
    kinds = line_kinds(text)
    return Measurement(path, kinds.count(COMMENT), kinds.count(CODE))


def is_production_path(path: str) -> bool:
    return (path.startswith("src/") or path == "build.rs") and path.endswith(".rs") and not is_test_path(path)


def diff_measure(base: str, head_ref: str) -> tuple[int, int]:
    args = ["diff", "--no-renames", "--name-status", "-z", base]
    if head_ref != WORKING_TREE:
        args.append(head_ref)
    fields = git(*args).split("\0")[:-1]
    added: Counter[str] = Counter()
    deleted: Counter[str] = Counter()
    code = 0
    unchanged_code = True
    for status, path in zip(fields[::2], fields[1::2]):
        if not is_production_path(path):
            continue
        old = "" if status == "A" else read_file(base, path)
        new = "" if status == "D" else read_file(head_ref, path)
        unchanged_code &= status == "M" and normalize(old)[0] == normalize(new)[0]
        old_rows, new_rows = old.splitlines(), new.splitlines()
        old_kinds, new_kinds = line_kinds(old), line_kinds(new)
        for tag, start, end, new_start, new_end in difflib.SequenceMatcher(None, old_rows, new_rows, autojunk=False).get_opcodes():
            if tag == "equal":
                continue
            deleted.update(old_rows[i].strip() for i in range(start, end) if old_kinds[i] == COMMENT)
            added.update(new_rows[i].strip() for i in range(new_start, new_end) if new_kinds[i] == COMMENT)
            code += sum(new_kinds[i] == CODE for i in range(new_start, new_end))
    # Only proven corrections/deletions may bypass a zero-code diff budget.
    if unchanged_code and added.total() <= deleted.total():
        return 0, code
    return (added - deleted).total(), code


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


def read_file(head_ref: str, path: str) -> str:
    if head_ref == WORKING_TREE:
        candidate = REPO_ROOT / path
        if not candidate.is_file():
            raise GitError(f"{path}: missing or unreadable source")
        raw = candidate.read_bytes()
    else:
        raw = git("show", f"{head_ref}:{path}", binary=True)
    return raw.decode("utf-8")


def check(base_ref: str, head_ref: str, max_ratio: float, min_lines: int) -> int:
    base, paths = added_rust_files(base_ref, head_ref)
    print(f"base: {base_ref} -> {base}")
    print(f"head: {head_ref}")

    measurements: list[Measurement] = []
    for path in paths:
        if not is_production_path(path):
            continue
        text = read_file(head_ref, path)
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
    for item in over:
        print(
            f"::warning file={item.path}::{item.comment_lines} comment lines of "
            f"{item.measured} measured ({item.ratio:.1%}); "
            f"new-file density exceeds {max_ratio:.0%}"
        )
    comments, code = diff_measure(base, head_ref)
    print(f"added production rows after comment moves/corrections: comments={comments} code={code}")
    if comments > code:
        print(f"FAIL: added comments ({comments}) exceed added code ({code})")
        return 1
    print("OK: added comments do not exceed added code")
    return 0


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
    except (GitError, OSError, UnicodeError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
