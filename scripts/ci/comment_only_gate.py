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
keeps the full run). Anything else -- including a missing input or an
exception -- writes ``comment_only=false``; the workflow reads only ``'true'``.
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


def git(*args: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(REPO_ROOT), *args], check=False, capture_output=True, text=True
    )
    if result.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout


def evaluate(base: str, head: str, raw_filters: str | None) -> tuple[bool, list[str], str]:
    if not SHA.match(base) or not SHA.match(head):
        return False, [f"base/head are not full SHAs: {base!r} {head!r}"], ""
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
    return verdict, reasons, judge_log


def main() -> int:
    try:
        verdict, reasons, judge_log = evaluate(
            os.environ.get("BASE_SHA", ""),
            os.environ.get("HEAD_SHA", ""),
            os.environ.get("FILTER_OUTPUTS"),
        )
    except Exception as error:  # every failure falls back to the full run
        verdict, reasons, judge_log = False, [f"gate error: {error!r}"], ""

    value = "true" if verdict else "false"
    judge_lines = judge_log.splitlines()
    if len(judge_lines) > SUMMARY_LINE_LIMIT:
        judge_lines = judge_lines[:SUMMARY_LINE_LIMIT] + ["... (truncated)"]
    report = [f"comment_only={value}"]
    report += [f"- {reason}" for reason in reasons]
    report += ["", "check_comment_only_change.py:", *(judge_lines or ["<not run>"])]
    print("\n".join(report))
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary_path:
        with open(summary_path, "a", encoding="utf-8") as summary:
            summary.write("### Comment-only gate\n\n```\n" + "\n".join(report) + "\n```\n")
    output_path = os.environ.get("GITHUB_OUTPUT")
    if output_path:
        with open(output_path, "a", encoding="utf-8") as output:
            output.write(f"comment_only={value}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
