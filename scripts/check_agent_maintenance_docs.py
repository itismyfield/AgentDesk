#!/usr/bin/env python3
"""Freshness gate for docs/agent-maintenance.

The agent-maintenance pages are an authority for migration-sensitive code
surfaces. This script keeps that authority explicit:

* every guarded page carries a ``Last refreshed`` header tied to ``main``;
* the referenced commit is an ancestor of the current checkout;
* PRs that touch guarded code paths also touch the matching maintenance page;
* line counts copied into ``change-surfaces.md`` are compared with the
  generated module inventory.

Warnings do not fail the script. Errors fail unless ``--warning-only`` is
passed, which is the initial CI rollout mode for #1432.
"""

from __future__ import annotations

import argparse
import datetime as dt
import fnmatch
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_FRESHNESS_DAYS = 90

LAST_REFRESHED_RE = re.compile(
    r"^>\s*Last refreshed:\s*"
    r"(?P<date>\d{4}-\d{2}-\d{2})\s*"
    r"\(against\s+`main`\s+@\s+`(?P<commit>[0-9a-f]{7,40})`\)\.?\s*$"
)
MODULE_INVENTORY_ROW_RE = re.compile(
    r"^\|\s*`[^`]+`\s*\|\s*`(?P<path>[^`]+\.rs)`\s*\|\s*(?P<lines>\d+)\s*\|"
)
CHANGE_SURFACE_LINE_RE = re.compile(
    r"`(?P<path>src/[^`]+\.rs)`\s*\((?P<lines>\d+)\s+lines\b"
)


@dataclass(frozen=True)
class Finding:
    severity: str
    path: str
    message: str
    line: int | None = None


@dataclass(frozen=True)
class TouchRule:
    patterns: tuple[str, ...]
    required_doc: str
    reason: str


MIGRATION_SENSITIVE_DOCS: tuple[str, ...] = (
    "docs/agent-maintenance/change-surfaces.md",
    "docs/agent-maintenance/known-legacy.md",
    "docs/agent-maintenance/discord-outbound-migration.md",
    "docs/agent-maintenance/opencode-usability-spec.md",
)

DOC_TOUCH_RULES: tuple[TouchRule, ...] = (
    TouchRule(
        patterns=("src/services/discord/outbound/**",),
        required_doc="docs/agent-maintenance/discord-outbound-migration.md",
        reason="Discord outbound API changes must refresh the callsite coverage map.",
    ),
    TouchRule(
        patterns=("src/services/discord/tmux.rs",),
        required_doc="docs/agent-maintenance/change-surfaces.md",
        reason="tmux watcher changes must refresh the migration-sensitive surface map.",
    ),
    TouchRule(
        patterns=(
            "src/services/git/**",
            "src/services/git.rs",
            "src/services/git_runner.rs",
            "services/git/**",
        ),
        required_doc="docs/agent-maintenance/change-surfaces.md",
        reason="central git-helper changes must keep the maintenance map in sync.",
    ),
)


def run_git(repo_root: Path, args: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", *args],
        cwd=repo_root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


def rel_posix(path: Path, repo_root: Path) -> str:
    try:
        return path.relative_to(repo_root).as_posix()
    except ValueError:
        return path.as_posix()


def parse_last_refreshed(text: str) -> tuple[dt.date, str, int] | None:
    for line_no, line in enumerate(text.splitlines()[:24], start=1):
        match = LAST_REFRESHED_RE.match(line.strip())
        if match is None:
            continue
        try:
            refreshed_on = dt.date.fromisoformat(match.group("date"))
        except ValueError:
            return None
        return refreshed_on, match.group("commit"), line_no
    return None


def check_doc_headers(
    repo_root: Path, today: dt.date, freshness_days: int
) -> list[Finding]:
    findings: list[Finding] = []
    for rel_path in MIGRATION_SENSITIVE_DOCS:
        path = repo_root / rel_path
        if not path.is_file():
            findings.append(
                Finding("error", rel_path, "migration-sensitive maintenance doc is missing")
            )
            continue

        parsed = parse_last_refreshed(path.read_text(encoding="utf-8"))
        if parsed is None:
            findings.append(
                Finding(
                    "error",
                    rel_path,
                    "missing header: Last refreshed: YYYY-MM-DD (against `main` @ `<sha>`).",
                )
            )
            continue

        refreshed_on, commit, line_no = parsed
        if refreshed_on > today:
            findings.append(
                Finding(
                    "error",
                    rel_path,
                    f"Last refreshed date {refreshed_on.isoformat()} is in the future.",
                    line_no,
                )
            )

        age_days = (today - refreshed_on).days
        if age_days > freshness_days:
            findings.append(
                Finding(
                    "warning",
                    rel_path,
                    (
                        f"Last refreshed is {age_days} days old; re-audit within "
                        f"{freshness_days} days."
                    ),
                    line_no,
                )
            )

        resolved = run_git(repo_root, ["rev-parse", "--verify", f"{commit}^{{commit}}"])
        if resolved.returncode != 0:
            findings.append(
                Finding(
                    "error",
                    rel_path,
                    f"Last refreshed commit {commit} does not resolve in this checkout.",
                    line_no,
                )
            )
            continue

        ancestor = run_git(
            repo_root,
            ["merge-base", "--is-ancestor", resolved.stdout.strip(), "HEAD"],
        )
        if ancestor.returncode != 0:
            findings.append(
                Finding(
                    "error",
                    rel_path,
                    f"Last refreshed commit {commit} is not an ancestor of HEAD.",
                    line_no,
                )
            )
    return findings


def default_base_ref(repo_root: Path) -> str:
    github_base = os.environ.get("GITHUB_BASE_REF")
    if github_base:
        return f"origin/{github_base}"
    if run_git(repo_root, ["rev-parse", "--verify", "origin/main"]).returncode == 0:
        return "origin/main"
    return "main"


def changed_files_from_git(repo_root: Path, base_ref: str) -> tuple[set[str], Finding | None]:
    diff = run_git(repo_root, ["diff", "--name-only", f"{base_ref}...HEAD"])
    if diff.returncode == 0:
        return {line.strip() for line in diff.stdout.splitlines() if line.strip()}, None

    message = diff.stderr.strip() or diff.stdout.strip() or "git diff failed"
    return (
        set(),
        Finding(
            "warning",
            ".",
            f"could not compute changed files against {base_ref}: {message}",
        ),
    )


def _matches_any(path: str, patterns: Iterable[str]) -> bool:
    return any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns)


def check_doc_touch_rules(changed_files: set[str]) -> list[Finding]:
    findings: list[Finding] = []
    for rule in DOC_TOUCH_RULES:
        touched_sources = sorted(
            path for path in changed_files if _matches_any(path, rule.patterns)
        )
        if not touched_sources or rule.required_doc in changed_files:
            continue
        sample = ", ".join(touched_sources[:3])
        if len(touched_sources) > 3:
            sample += f", +{len(touched_sources) - 3} more"
        findings.append(
            Finding(
                "error",
                rule.required_doc,
                (
                    f"{rule.required_doc} must be touched because {sample} changed. "
                    f"{rule.reason}"
                ),
            )
        )
    return findings


def parse_module_inventory(path: Path) -> dict[str, int]:
    inventory: dict[str, int] = {}
    if not path.is_file():
        return inventory
    for line in path.read_text(encoding="utf-8").splitlines():
        match = MODULE_INVENTORY_ROW_RE.match(line)
        if match is None:
            continue
        inventory[match.group("path")] = int(match.group("lines"))
    return inventory


def check_change_surface_line_counts(repo_root: Path) -> list[Finding]:
    change_surfaces = repo_root / "docs/agent-maintenance/change-surfaces.md"
    inventory = parse_module_inventory(repo_root / "docs/generated/module-inventory.md")
    if not inventory:
        return [
            Finding(
                "warning",
                "docs/generated/module-inventory.md",
                "module inventory is missing or empty; cannot verify copied line counts.",
            )
        ]
    if not change_surfaces.is_file():
        return [
            Finding(
                "error",
                "docs/agent-maintenance/change-surfaces.md",
                "change-surfaces.md is missing.",
            )
        ]

    findings: list[Finding] = []
    for line_no, line in enumerate(change_surfaces.read_text(encoding="utf-8").splitlines(), start=1):
        for match in CHANGE_SURFACE_LINE_RE.finditer(line):
            path = match.group("path")
            documented = int(match.group("lines"))
            actual = inventory.get(path)
            if actual is None:
                findings.append(
                    Finding(
                        "warning",
                        rel_posix(change_surfaces, repo_root),
                        f"{path} is not present in docs/generated/module-inventory.md.",
                        line_no,
                    )
                )
                continue
            if documented != actual:
                findings.append(
                    Finding(
                        "warning",
                        rel_posix(change_surfaces, repo_root),
                        (
                            f"{path} line count is {documented} in change-surfaces.md "
                            f"but {actual} in module-inventory.md."
                        ),
                        line_no,
                    )
                )
    return findings


def github_escape(value: str) -> str:
    return (
        value.replace("%", "%25")
        .replace("\r", "%0D")
        .replace("\n", "%0A")
        .replace(":", "%3A")
        .replace(",", "%2C")
    )


def emit_findings(findings: list[Finding], warning_only: bool) -> None:
    if not findings:
        print("agent-maintenance freshness check passed")
        return

    error_count = sum(1 for finding in findings if finding.severity == "error")
    warning_count = len(findings) - error_count
    print(
        "agent-maintenance freshness check found "
        f"{error_count} error(s), {warning_count} warning(s)"
    )
    github_actions = os.environ.get("GITHUB_ACTIONS") == "true"
    for finding in findings:
        severity = "warning" if warning_only or finding.severity == "warning" else "error"
        if github_actions:
            location = f"file={github_escape(finding.path)}"
            if finding.line is not None:
                location += f",line={finding.line}"
            print(f"::{severity} {location}::{github_escape(finding.message)}")
        else:
            line = f":{finding.line}" if finding.line is not None else ""
            print(f"{severity}: {finding.path}{line}: {finding.message}")


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Check docs/agent-maintenance freshness headers and PR touch rules."
    )
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=REPO_ROOT,
        help="repository root (default: parent of this script)",
    )
    parser.add_argument(
        "--base-ref",
        help="base ref for changed-file detection (default: GITHUB_BASE_REF or origin/main)",
    )
    parser.add_argument(
        "--changed-file",
        action="append",
        default=[],
        help="explicit changed file path; may be repeated, bypasses git diff detection",
    )
    parser.add_argument(
        "--freshness-days",
        type=int,
        default=DEFAULT_FRESHNESS_DAYS,
        help="warn when Last refreshed is older than this many days",
    )
    parser.add_argument(
        "--today",
        type=dt.date.fromisoformat,
        default=dt.date.today(),
        help=argparse.SUPPRESS,
    )
    parser.add_argument(
        "--warning-only",
        action="store_true",
        help="emit findings as warnings and exit 0; intended for the initial CI rollout",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    repo_root = args.repo_root.resolve()

    findings: list[Finding] = []
    findings.extend(check_doc_headers(repo_root, args.today, args.freshness_days))
    findings.extend(check_change_surface_line_counts(repo_root))

    if args.changed_file:
        changed_files = {path.strip() for path in args.changed_file if path.strip()}
    else:
        base_ref = args.base_ref or default_base_ref(repo_root)
        changed_files, warning = changed_files_from_git(repo_root, base_ref)
        if warning is not None:
            findings.append(warning)
    findings.extend(check_doc_touch_rules(changed_files))

    emit_findings(findings, args.warning_only)
    has_errors = any(finding.severity == "error" for finding in findings)
    if has_errors and not args.warning_only:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
