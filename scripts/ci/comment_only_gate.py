#!/usr/bin/env python3
"""Decide whether ci-pr.yml may skip its heavy test jobs for a comment-only PR.

Environment: ``BASE_SHA``/``HEAD_SHA`` (the pull request's base and head),
``FILTER_OUTPUTS`` (``toJSON`` of the dorny/paths-filter step run with
``list-files: json``), and the usual ``GITHUB_OUTPUT``/``GITHUB_STEP_SUMMARY``.

``comment_only=true`` needs all of: scripts/check_comment_only_change.py exits
0 for base..head, at least one ``.rs`` file changed and every changed ``.rs``
file was edited in place, every other changed file is ``*.md`` on both sides,
and every file any path filter selected is one of those verified ``.rs`` files
(so a ``.md`` that selects a filter, or a file the filter saw but git did not,
keeps the full run), and no changed ``.rs`` file holds a line break other than
LF/CRLF on either side, since the judge's ``splitlines()`` erases those inside
literals. Anything else -- including a missing input or an exception --
writes ``comment_only=false``; the workflow reads only ``'true'``.

``rust_tests_skip=true`` also lets the library sweep skip. It needs
``comment_only=true`` and no ``include!``/``include_str!``/``include_bytes!``
call in the base or head tree that reads, or may read, a changed file
(scripts/ci/rust_include_reads.py); tests see those files' comments. A scan
that cannot finish writes ``comment_only=false`` too.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
JUDGE = REPO_ROOT / "scripts" / "check_comment_only_change.py"
SHA = re.compile(r"^[0-9a-f]{40}$")
MARKDOWN_STATUSES = {"A", "M", "D", "R", "C"}
SUMMARY_LINE_LIMIT = 200
# str.splitlines() boundaries other than LF and CRLF; the judge rejoins split lines with LF.
OTHER_LINE_BREAK = re.compile("[\r\x0b\x0c\x1c\x1d\x1e\x85\u2028\u2029]")


def decide(
    judge_rc: int,
    entries: list[tuple[str, str, str]],
    filter_outputs: object,
) -> tuple[bool, list[str]]:
    """Pure verdict: (comment_only, reasons it is not) from the three inputs."""

    reasons: list[str] = []
    if judge_rc != 0:
        reasons.append(f"check_comment_only_change.py exited {judge_rc}")

    rust: set[str] = set()
    for status, old_path, new_path in entries:
        if old_path.endswith(".rs") or new_path.endswith(".rs"):
            if status == "M" and old_path == new_path:
                rust.add(new_path)
            else:
                reasons.append(f"{old_path} -> {new_path}: Rust change with status {status} is not an in-place edit")
        elif not (old_path.endswith(".md") and new_path.endswith(".md")):
            reasons.append(f"{new_path}: non-Rust, non-Markdown change ({status})")
        elif status not in MARKDOWN_STATUSES:
            reasons.append(f"{new_path}: Markdown change with unexpected status {status}")
    if not rust:
        reasons.append("no changed .rs file")

    if not isinstance(filter_outputs, dict):
        reasons.append("paths-filter outputs are missing or not a JSON object")
        return False, reasons
    names = sorted(key for key, value in filter_outputs.items() if value in ("true", "false"))
    if not names:
        reasons.append("paths-filter outputs name no filter")
    for name in names:
        raw = filter_outputs.get(f"{name}_files")
        try:
            files = json.loads(raw) if isinstance(raw, str) else None
        except json.JSONDecodeError:
            files = None
        if not isinstance(files, list) or not all(isinstance(item, str) for item in files):
            reasons.append(f"filter {name}: file list missing or unreadable")
            continue
        if filter_outputs[name] == "true" and not files:
            reasons.append(f"filter {name}: true with an empty file list")
        for path in files:
            if path not in rust:
                reasons.append(f"filter {name}: selected by {path}, which is not a verified comment-only .rs edit")
    return not reasons, reasons


def include_readers(changed: set[str], sites) -> list[str]:
    """Why the library sweep must run: each include call that may read a changed file."""
    return sorted(
        f"{path} is read by {site.describe()}"
        for site in sites
        for path in changed
        if site.reads(path)
    )


def other_line_breaks(sides: dict[str, bytes]) -> list[str]:
    """Refusals for each side whose bytes are not UTF-8 or hold a non-LF/CRLF line break."""
    reasons = []
    for label, raw in sides.items():
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError:
            reasons.append(f"{label}: not UTF-8")
            continue
        found = OTHER_LINE_BREAK.search(text.replace("\r\n", "\n"))
        if found:
            reasons.append(f"{label}: line break {found.group()!r} other than LF/CRLF, which the judge cannot compare")
    return reasons


def blob(rev: str, path: str) -> bytes:
    result = subprocess.run(
        ["git", "-C", str(REPO_ROOT), "show", f"{rev}:{path}"], check=False, capture_output=True
    )
    if result.returncode != 0:
        raise RuntimeError(f"git show {rev}:{path} failed: {result.stderr.decode('utf-8', 'replace').strip()}")
    return result.stdout


def git(*args: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(REPO_ROOT), *args], check=False, capture_output=True, text=True
    )
    if result.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout


def evaluate(base: str, head: str, raw_filters: str | None) -> tuple[bool, list[str], str, bool, list[str]]:
    """(comment_only, its refusals, judge log, library sweep may skip, sweep notes)."""
    if not SHA.match(base) or not SHA.match(head):
        return False, [f"base/head are not full SHAs: {base!r} {head!r}"], "", False, []
    judge = subprocess.run(
        [sys.executable, str(JUDGE), base, head, "--allow-non-rust"],
        check=False, capture_output=True, text=True,
    )
    judge_log = (judge.stdout + judge.stderr).strip()
    # Imported here so a broken judge module is caught by main() like any other failure.
    sys.path.insert(0, str(REPO_ROOT / "scripts"))
    from check_comment_only_change import parse_name_status

    merge_base = git("merge-base", base, head).strip()
    entries = parse_name_status(git("diff", "--name-status", "--find-renames", "-z", merge_base, head))
    try:
        filter_outputs = json.loads(raw_filters) if raw_filters else None
    except json.JSONDecodeError:
        filter_outputs = None
    verdict, reasons = decide(judge.returncode, entries, filter_outputs)
    if verdict:
        rust = sorted({new_path for _status, _old, new_path in entries if new_path.endswith(".rs")})
        reasons = other_line_breaks(
            {f"{path} ({side})": blob(rev, path) for path in rust for side, rev in (("base", merge_base), ("head", head))}
        )
    if not verdict or reasons:
        return False, reasons, judge_log, False, []
    try:
        from rust_include_reads import tree_sites

        changed = {path for _status, old_path, new_path in entries for path in (old_path, new_path)}
        sites = tree_sites(str(REPO_ROOT), base) | tree_sites(str(REPO_ROOT), head)
        readers = include_readers(changed, sites)
        notes = [*readers, f"scanned {len(sites)} include calls in the base and head trees"]
    except Exception as error:  # a scan that cannot finish falls back to the full run
        return False, [f"include scan error: {error!r}"], judge_log, False, []
    return True, [], judge_log, not readers, notes


def main() -> int:
    try:
        verdict, reasons, judge_log, skip_sweep, sweep_notes = evaluate(
            os.environ.get("BASE_SHA", ""),
            os.environ.get("HEAD_SHA", ""),
            os.environ.get("FILTER_OUTPUTS"),
        )
    except Exception as error:  # every failure falls back to the full run
        verdict, reasons, judge_log, skip_sweep, sweep_notes = False, [f"gate error: {error!r}"], "", False, []

    value = "true" if verdict else "false"
    skip = "true" if verdict and skip_sweep else "false"
    judge_lines = judge_log.splitlines()
    if len(judge_lines) > SUMMARY_LINE_LIMIT:
        judge_lines = judge_lines[:SUMMARY_LINE_LIMIT] + ["... (truncated)"]
    report = [f"comment_only={value}", *(f"- {reason}" for reason in reasons)]
    if verdict:
        report += [f"rust_tests_skip={skip}", *(f"- {note}" for note in sweep_notes)]
    report += ["", "check_comment_only_change.py:", *(judge_lines or ["<not run>"])]
    print("\n".join(report))
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary_path:
        with open(summary_path, "a", encoding="utf-8") as summary:
            summary.write("### Comment-only gate\n\n```\n" + "\n".join(report) + "\n```\n")
    output_path = os.environ.get("GITHUB_OUTPUT")
    if output_path:
        with open(output_path, "a", encoding="utf-8") as output:
            output.write(f"comment_only={value}\nrust_tests_skip={skip}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
