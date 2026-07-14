#!/usr/bin/env python3
"""Aggregate the last day of dcserver logs and emit one human-review digest."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Iterable

from log_digest_issue_drafts import (
    CONFIRMED_APPROVAL,
    DEFAULT_DAILY_THRESHOLD,
    IssueDraft,
    OpenIssue,
    aggregate_normalized_signatures,
    decide_issue_drafts,
    extract_severity,
    format_daily_summary,
    maybe_post_approved_drafts,
    write_pending_drafts,
)


REPOSITORY = "itismyfield/AgentDesk"
_LINE_TIMESTAMP_RE = re.compile(
    r"(?<!\d)(\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?)"
)


def runtime_root() -> Path:
    """Mirror AgentDesk's AGENTDESK_ROOT_DIR then release-root fallback."""

    configured = os.environ.get("AGENTDESK_ROOT_DIR") or os.environ.get("ADK_REL")
    if configured:
        return Path(configured).expanduser()
    return Path.home() / ".adk" / "release"


def dcserver_log_paths(root: Path) -> list[Path]:
    """Return internal stdout rotations plus the actual launchd stderr path."""

    logs = root / "logs"
    stdout = logs / "dcserver.stdout.log"
    paths = [stdout]
    paths.extend(logs / f"dcserver.stdout.log.{index}" for index in range(1, 11))
    paths.append(logs / "dcserver.launchd.stderr.log")
    return paths


def _parse_line_timestamp(line: str) -> datetime | None:
    match = _LINE_TIMESTAMP_RE.search(line)
    if not match:
        return None
    value = match.group(1).replace(",", ".")
    if value.endswith("Z"):
        value = value[:-1] + "+00:00"
    try:
        parsed = datetime.fromisoformat(value)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def recent_log_lines(paths: Iterable[Path], since: datetime, now: datetime) -> tuple[list[str], list[str]]:
    """Read the 24h window; recent undated launchd lines use file mtime."""

    lines: list[str] = []
    warnings: list[str] = []
    found_any = False
    for path in paths:
        if not path.is_file():
            continue
        found_any = True
        try:
            include_undated = (
                path.name == "dcserver.launchd.stderr.log"
                and datetime.fromtimestamp(path.stat().st_mtime, timezone.utc) >= since
            )
            with path.open(encoding="utf-8", errors="replace") as stream:
                for raw_line in stream:
                    if extract_severity(raw_line) is None:
                        continue
                    line = raw_line.rstrip("\n")
                    timestamp = _parse_line_timestamp(line)
                    if timestamp is not None:
                        if since <= timestamp <= now + timedelta(minutes=5):
                            lines.append(line)
                    elif include_undated:
                        # launchd bootstrap stderr is not guaranteed to carry a
                        # timestamp. Its bounded current file is included only when
                        # the file itself changed during the daily window.
                        lines.append(line)
        except OSError as error:
            warnings.append(f"could not read {path}: {error}")
    if not found_any:
        warnings.append("no dcserver stdout or launchd stderr log files were found")
    return lines, warnings


def load_open_issues(repo: str) -> tuple[list[OpenIssue], str | None]:
    command = [
        "gh",
        "issue",
        "list",
        "--repo",
        repo,
        "--state",
        "open",
        "--limit",
        "1000",
        "--json",
        "number,title,body,url",
    ]
    try:
        completed = subprocess.run(command, check=False, capture_output=True, text=True, timeout=30)
    except (OSError, subprocess.TimeoutExpired) as error:
        return [], f"open-issue dedup unavailable ({error}); drafts suppressed"
    if completed.returncode != 0:
        detail = completed.stderr.strip() or f"gh exited {completed.returncode}"
        return [], f"open-issue dedup unavailable ({detail}); drafts suppressed"
    try:
        payload = json.loads(completed.stdout)
        issues = [
            OpenIssue(
                number=int(item["number"]),
                title=str(item.get("title") or ""),
                body=str(item.get("body") or ""),
                url=str(item.get("url") or ""),
            )
            for item in payload
        ]
    except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        return [], f"open-issue dedup response invalid ({error}); drafts suppressed"
    return issues, None


def create_github_issue(repo: str, draft: IssueDraft) -> str:
    if draft.path is None:
        raise ValueError("approved issue draft must be written before posting")
    completed = subprocess.run(
        [
            "gh",
            "issue",
            "create",
            "--repo",
            repo,
            "--title",
            draft.title,
            "--body-file",
            str(draft.path),
        ],
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    )
    return completed.stdout.strip()


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return parsed


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=runtime_root())
    parser.add_argument("--repo", default=os.environ.get("AGENTDESK_LOG_DIGEST_REPO", REPOSITORY))
    parser.add_argument(
        "--threshold",
        type=positive_int,
        default=positive_int(os.environ.get("AGENTDESK_LOG_DIGEST_THRESHOLD", str(DEFAULT_DAILY_THRESHOLD))),
    )
    parser.add_argument("--now", help="RFC3339 test/diagnostic override")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    now = datetime.fromisoformat(args.now.replace("Z", "+00:00")) if args.now else datetime.now(timezone.utc)
    if now.tzinfo is None:
        now = now.replace(tzinfo=timezone.utc)
    now = now.astimezone(timezone.utc)
    since = now - timedelta(days=1)
    window_label = f"{since:%Y-%m-%d %H:%M}–{now:%Y-%m-%d %H:%M} UTC"

    lines, warnings = recent_log_lines(dcserver_log_paths(args.root), since, now)
    patterns = aggregate_normalized_signatures(lines)
    open_issues, dedup_warning = load_open_issues(args.repo)
    if dedup_warning:
        warnings.append(dedup_warning)
    decisions = decide_issue_drafts(
        patterns,
        open_issues,
        threshold=args.threshold,
        window_label=window_label,
        dedup_available=dedup_warning is None,
    )
    pending_dir = args.root / "runtime" / "pending-issue-drafts" / "daily-log-digest"
    drafts = write_pending_drafts(
        [decision.draft for decision in decisions if decision.draft is not None],
        pending_dir,
    )

    approval_mode = os.environ.get("AGENTDESK_LOG_DIGEST_CREATE_ISSUE", "off")
    post = maybe_post_approved_drafts(
        drafts,
        approval_mode,
        lambda draft: create_github_issue(args.repo, draft),
    )
    if approval_mode not in {"off", CONFIRMED_APPROVAL}:
        warnings.append(
            "invalid AGENTDESK_LOG_DIGEST_CREATE_ISSUE value ignored; use literal 'confirmed' or 'off'"
        )
    elif approval_mode == CONFIRMED_APPROVAL and not post.attempted:
        warnings.append(post.reason)
    if post.created_urls:
        warnings.append("human-confirmed issues created: " + ", ".join(post.created_urls))

    print(
        format_daily_summary(
            patterns,
            decisions,
            drafts,
            threshold=args.threshold,
            window_label=window_label,
            warnings=warnings,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
