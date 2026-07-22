#!/usr/bin/env python3
"""Freshness gate for docs/agent-maintenance.

The agent-maintenance pages are an authority for migration-sensitive code
surfaces. This script keeps that authority explicit:

* every guarded page carries a ``Last refreshed`` header tied to ``main`` or
  an explicit manual refresh anchor;
* referenced commits are ancestors of the current checkout when the header is
  commit-anchored;
* PRs that touch guarded code paths also touch the matching maintenance page;
* frozen ``change-surfaces.md`` paths are checked against the generated module
  inventory for current giant-surface integrity.

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
# Keep the scan bounded so examples later in a doc cannot satisfy the header.
HEADER_SCAN_LINE_LIMIT = 80

LAST_REFRESHED_RE = re.compile(
    r"^>\s*Last refreshed:\s*"
    r"(?P<date>\d{4}-\d{2}-\d{2})\s*"
    r"\("
    r"(?:"
    r"against\s+`main`\s+@\s+`(?P<commit>[0-9a-f]{7,40})`"
    r"|manual:\s+(?P<manual_anchor>[^)]+)"
    r"|against\s+(?:PR\s+)?(?P<issue_anchor>#\d+[^)]*)"
    r")"
    r"\)\.?\s*$"
)
# Module inventory row: | `module` | `path` | <total> | <prod> | <test> | flags |
MODULE_INVENTORY_ROW_RE = re.compile(
    r"^\|\s*`[^`]+`\s*\|\s*`(?P<path>[^`]+\.rs)`\s*\|"
    r"\s*(?P<lines>\d+)\s*\|\s*(?P<prod>\d+)\s*\|\s*(?P<test>\d+)\s*\|"
)
# `` `path` (1234 lines …) `` or `` `path` (~1234 lines …) `` — explicit form.
CHANGE_SURFACE_LINE_RE = re.compile(
    r"`(?P<path>src/[^`]+\.rs)`\s*\(~?(?P<lines>\d+)\s+lines\b"
)
# `` `path` (1234) `` or `` `path` (~1234) `` — bare line-count shorthand used in
# the services_misc_giants list. The parenthetical must be a number only (no
# trailing prose), so `(line 1971, …)` or `(directory, refactored)` are ignored.
CHANGE_SURFACE_SHORTHAND_RE = re.compile(
    r"`(?P<path>src/[^`]+\.rs)`\s*\(~?(?P<lines>\d+)\)"
)
GIANT_FILE_THRESHOLD = 1000
MIGRATION_0093_PATH = (
    "migrations/postgres/0093_intake_outbox_preserve_on_cancel.sql"
)
MIGRATION_0093_ROLLOUT_MARKERS: tuple[str, ...] = (
    "migration 0093 is an irreversible binary-floor boundary",
    "pre-stage and upgrade the entire fleet before applying migration 0093",
    (
        "after migration 0093, binaries embedding only migrations 0092 or earlier "
        "must not restart or roll back"
    ),
    "forward-fix",
    "restore a pre-0093 database backup and roll back the entire fleet together",
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


@dataclass(frozen=True)
class LastRefreshed:
    refreshed_on: dt.date
    commit: str | None
    anchor: str
    line: int


MIGRATION_SENSITIVE_DOCS: tuple[str, ...] = (
    "docs/agent-maintenance/change-surfaces.md",
    "docs/agent-maintenance/known-legacy.md",
    "docs/agent-maintenance/discord-outbound-migration.md",
    "docs/agent-maintenance/opencode-usability-spec.md",
    "docs/agent-maintenance/multinode-transition.md",
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
    TouchRule(
        patterns=(
            "src/server/worker_registry.rs",
            "src/services/cluster/intake_router_hook.rs",
            "src/services/cluster/intake_router_hook/**",
            "src/services/cluster/intake_worker_capabilities.rs",
            "src/services/discord/runtime_bootstrap.rs",
            "migrations/postgres/0093_intake_outbox_preserve_on_cancel.sql",
            "policies/merge-automation.js",
            "src/server/routes/dispatches/outbox.rs",
        ),
        required_doc="docs/agent-maintenance/multinode-transition.md",
        reason="multinode-sensitive ownership, singleton, and lease assumptions must stay audited.",
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


def is_shallow_checkout(repo_root: Path) -> bool:
    result = run_git(repo_root, ["rev-parse", "--is-shallow-repository"])
    return result.returncode == 0 and result.stdout.strip() == "true"


def rel_posix(path: Path, repo_root: Path) -> str:
    try:
        return path.relative_to(repo_root).as_posix()
    except ValueError:
        return path.as_posix()


def parse_last_refreshed(text: str) -> LastRefreshed | None:
    lines = text.splitlines()[:HEADER_SCAN_LINE_LIMIT]
    for line_no, line in enumerate(lines, start=1):
        match = LAST_REFRESHED_RE.match(line.strip())
        if match is None:
            continue
        try:
            refreshed_on = dt.date.fromisoformat(match.group("date"))
        except ValueError:
            return None
        commit = match.group("commit")
        anchor = commit or match.group("manual_anchor") or match.group("issue_anchor") or ""
        return LastRefreshed(refreshed_on, commit, anchor.strip(), line_no)
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
                    (
                        "missing header: Last refreshed: YYYY-MM-DD "
                        "(against `main` @ `<sha>`), (against #<issue> <reason>), "
                        "or (manual: <reason>)."
                    ),
                )
            )
            continue

        if parsed.refreshed_on > today:
            findings.append(
                Finding(
                    "error",
                    rel_path,
                    f"Last refreshed date {parsed.refreshed_on.isoformat()} is in the future.",
                    parsed.line,
                )
            )

        age_days = (today - parsed.refreshed_on).days
        if age_days > freshness_days:
            findings.append(
                Finding(
                    "warning",
                    rel_path,
                    (
                        f"Last refreshed is {age_days} days old; re-audit within "
                        f"{freshness_days} days."
                    ),
                    parsed.line,
                )
            )

        if parsed.commit is None:
            continue

        resolved = run_git(
            repo_root, ["rev-parse", "--verify", f"{parsed.commit}^{{commit}}"]
        )
        if resolved.returncode != 0:
            if is_shallow_checkout(repo_root):
                findings.append(
                    Finding(
                        "warning",
                        rel_path,
                        (
                            f"Last refreshed commit {parsed.commit} is not present in this "
                            "shallow checkout; fetch full history to verify ancestry."
                        ),
                        parsed.line,
                    )
                )
                continue
            findings.append(
                Finding(
                    "error",
                    rel_path,
                    f"Last refreshed commit {parsed.commit} does not resolve in this checkout.",
                    parsed.line,
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
                    f"Last refreshed commit {parsed.commit} is not an ancestor of HEAD.",
                    parsed.line,
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
    """Map each production module path to its *production* LoC.

    change-surfaces.md freezes modules by their review surface, which is the
    production line count (excluding `#[cfg(test)] mod` blocks). The inventory's
    ``Prod`` column is the authority for frozen giant-surface integrity (#3036).
    """

    inventory: dict[str, int] = {}
    if not path.is_file():
        return inventory
    for line in path.read_text(encoding="utf-8").splitlines():
        match = MODULE_INVENTORY_ROW_RE.match(line)
        if match is None:
            continue
        inventory[match.group("path")] = int(match.group("prod"))
    return inventory


def check_change_surface_line_counts(repo_root: Path) -> list[Finding]:
    change_surfaces = repo_root / "docs/agent-maintenance/change-surfaces.md"
    inventory = parse_module_inventory(repo_root / "docs/generated/module-inventory.md")
    if not inventory:
        return [
            Finding(
                "warning",
                "docs/generated/module-inventory.md",
                "module inventory is missing or empty; cannot verify frozen giant surfaces.",
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
    rel_doc = rel_posix(change_surfaces, repo_root)
    for line_no, line in enumerate(change_surfaces.read_text(encoding="utf-8").splitlines(), start=1):
        # Collect (path, documented_lines) pairs from both the explicit
        # "(N lines)" form and the bare "(N)"/"(~N)" shorthand, de-duplicating
        # overlaps where the shorthand regex also matches inside a longer span.
        matched: dict[tuple[int, str], int] = {}
        for match in CHANGE_SURFACE_LINE_RE.finditer(line):
            matched[(match.start(), match.group("path"))] = int(match.group("lines"))
        explicit_spans = [
            match.span() for match in CHANGE_SURFACE_LINE_RE.finditer(line)
        ]
        for match in CHANGE_SURFACE_SHORTHAND_RE.finditer(line):
            if any(start <= match.start() < end for start, end in explicit_spans):
                continue
            matched[(match.start(), match.group("path"))] = int(match.group("lines"))

        for (_pos, path), documented in sorted(matched.items()):
            actual = inventory.get(path)
            if actual is None:
                # The module-inventory only lists *production* modules, so a
                # dedicated `*_tests.rs` file is legitimately absent — warn so the
                # freeze entry can be reworded, but do not gate. A path that is
                # also missing from disk, however, is a deleted/renamed frozen
                # entry (a ghost the gate must catch, #3036).
                on_disk = (repo_root / path).is_file()
                is_test_path = path.endswith("_tests.rs") or path.rsplit("/", 1)[-1] in {
                    "tests.rs",
                    "integration_tests.rs",
                }
                if on_disk and is_test_path:
                    findings.append(
                        Finding(
                            "warning",
                            rel_doc,
                            (
                                f"{path} is a test file (excluded from the "
                                "production module inventory); reword the freeze "
                                "entry to drop its line count."
                            ),
                            line_no,
                        )
                    )
                else:
                    findings.append(
                        Finding(
                            "error",
                            rel_doc,
                            (
                                f"{path} is frozen with a line count but is "
                                + (
                                    "not a production module in "
                                    "docs/generated/module-inventory.md"
                                    if on_disk
                                    else "missing from disk (deleted or renamed)"
                                )
                                + "; remove the ghost freeze entry."
                            ),
                            line_no,
                        )
                    )
                continue
            # Ghost registration: a freeze entry whose production surface has
            # fallen below the giant threshold (decomposition complete). Stale
            # ghosts kept the page frozen against modules that no longer need
            # it (#3036), so this is a hard error — remove the entry.
            if actual < GIANT_FILE_THRESHOLD:
                findings.append(
                    Finding(
                        "error",
                        rel_doc,
                        (
                            f"{path} now has {actual} production lines "
                            f"(< {GIANT_FILE_THRESHOLD}); it is no longer a giant "
                            "file. Remove the ghost freeze entry from "
                            "change-surfaces.md."
                        ),
                        line_no,
                    )
                )
                continue
    return findings


def check_migration_0093_rollout_contract(
    repo_root: Path, changed_files: set[str]
) -> list[Finding]:
    if MIGRATION_0093_PATH not in changed_files:
        return []

    rel_path = "docs/agent-maintenance/multinode-transition.md"
    path = repo_root / rel_path
    if not path.is_file():
        return [
            Finding("error", rel_path, "migration 0093 rollout contract is missing")
        ]
    normalized = " ".join(path.read_text(encoding="utf-8").lower().split())
    missing = [
        marker
        for marker in MIGRATION_0093_ROLLOUT_MARKERS
        if marker not in normalized
    ]
    if not missing:
        return []
    return [
        Finding(
            "error",
            rel_path,
            "migration 0093 rollout contract is missing required marker(s): "
            + "; ".join(missing),
        )
    ]


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
    parser.add_argument(
        "--line-count-gate",
        action="store_true",
        help=(
            "hard-fail on frozen giant-surface integrity errors, including "
            "missing, non-production, and below-threshold ghost entries even "
            "under --warning-only (#3036)"
        ),
    )
    parser.add_argument(
        "--migration-0093-rollout-gate",
        action="store_true",
        help=(
            "hard-fail when the migration 0093 binary-floor rollout contract is "
            "missing, even under --warning-only"
        ),
    )
    return parser.parse_args(argv)


# Path/severity of findings produced by check_change_surface_line_counts.
CHANGE_SURFACE_DOC = "docs/agent-maintenance/change-surfaces.md"
MODULE_INVENTORY_DOC = "docs/generated/module-inventory.md"


def ensure_module_inventory(repo_root: Path) -> Finding | None:
    """Regenerate the inventory before parsing its line counts."""

    # The generator currently writes all inventory docs, including tracked
    # snapshots. This standalone gate favors fresh line-count data over a clean
    # worktree; CI separately rejects any resulting tracked-doc drift.
    generator = repo_root / "scripts/generate_inventory_docs.py"
    if not generator.is_file():
        return None
    result = subprocess.run(
        [sys.executable, str(generator)],
        cwd=repo_root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode == 0 and (repo_root / MODULE_INVENTORY_DOC).is_file():
        return None
    detail = (result.stderr or result.stdout).strip()
    return Finding(
        "error",
        MODULE_INVENTORY_DOC,
        f"failed to generate untracked module inventory: {detail or 'unknown error'}",
    )


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    repo_root = args.repo_root.resolve()

    findings: list[Finding] = []
    inventory_finding = ensure_module_inventory(repo_root)
    if inventory_finding is not None:
        findings.append(inventory_finding)
    findings.extend(check_doc_headers(repo_root, args.today, args.freshness_days))
    line_count_findings = check_change_surface_line_counts(repo_root)
    findings.extend(line_count_findings)

    if args.changed_file:
        changed_files = {path.strip() for path in args.changed_file if path.strip()}
    else:
        base_ref = args.base_ref or default_base_ref(repo_root)
        changed_files, warning = changed_files_from_git(repo_root, base_ref)
        if warning is not None:
            findings.append(warning)
    rollout_findings = check_migration_0093_rollout_contract(repo_root, changed_files)
    findings.extend(rollout_findings)
    findings.extend(check_doc_touch_rules(changed_files))

    emit_findings(findings, args.warning_only)
    has_errors = any(finding.severity == "error" for finding in findings)
    if has_errors and not args.warning_only:
        return 1
    # Even in the #1432 warning-only rollout, frozen giant-surface integrity is
    # a hard gate when requested: missing, non-production, and ghost entries
    # must not slip through (#3036).
    if args.line_count_gate and any(
        finding.severity == "error" for finding in line_count_findings
    ):
        return 1
    if args.migration_0093_rollout_gate and any(
        finding.severity == "error" for finding in rollout_findings
    ):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
