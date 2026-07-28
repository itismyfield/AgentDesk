#!/usr/bin/env python3
"""Aggregate the last day of dcserver logs and emit one human-review digest."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import BinaryIO, Iterable

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
OPEN_ISSUE_LIMIT = 1000
UNDATED_CHECKPOINT_VERSION = 3
UNDATED_HEAD_FINGERPRINT_CAP = 65_536
_LINE_TIMESTAMP_RE = re.compile(
    r"(?<!\d)(\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?)"
)


def runtime_root() -> Path:
    """Resolve the runtime root used by the release launcher."""

    # src/config.rs owns AGENTDESK_ROOT_DIR as the canonical runtime override.
    # ADK_REL remains a compatibility fallback because deploy-release.sh derives
    # it from that override and the routine launcher may pass only ADK_REL.
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


def _load_undated_offsets(
    checkpoint_path: Path | None,
) -> tuple[dict[str, dict[str, int | str]], list[str]]:
    if checkpoint_path is None or not checkpoint_path.is_file():
        return {}, []
    try:
        payload = json.loads(checkpoint_path.read_text(encoding="utf-8"))
        if not isinstance(payload, dict):
            raise ValueError("unsupported checkpoint shape")
        files = payload.get("files")
        if payload.get("version") != UNDATED_CHECKPOINT_VERSION or not isinstance(files, dict):
            raise ValueError("unsupported checkpoint shape")
        offsets: dict[str, dict[str, int | str]] = {}
        for path, entry in files.items():
            if not isinstance(path, str) or not isinstance(entry, dict):
                raise ValueError("invalid checkpoint entry")
            offsets[path] = {
                "device": int(entry["device"]),
                "inode": int(entry["inode"]),
                "offset": int(entry["offset"]),
                "head_hash": str(entry["head_hash"]),
                "head_length": int(entry["head_length"]),
            }
        return offsets, []
    except (OSError, KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        return {}, [f"could not load undated-line checkpoint {checkpoint_path}: {error}"]


def _save_undated_offsets(
    checkpoint_path: Path | None, offsets: dict[str, dict[str, int | str]]
) -> list[str]:
    if checkpoint_path is None:
        return []
    try:
        checkpoint_path.parent.mkdir(parents=True, exist_ok=True)
        temporary = checkpoint_path.with_suffix(checkpoint_path.suffix + ".tmp")
        temporary.write_text(
            json.dumps(
                {"version": UNDATED_CHECKPOINT_VERSION, "files": offsets},
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )
        temporary.replace(checkpoint_path)
    except OSError as error:
        return [f"could not save undated-line checkpoint {checkpoint_path}: {error}"]
    return []


def _head_fingerprint(stream: BinaryIO, offset: int) -> tuple[int, str]:
    head_length = min(offset, UNDATED_HEAD_FINGERPRINT_CAP)
    stream.seek(0)
    digest = hashlib.sha256(stream.read(head_length)).hexdigest()
    return head_length, digest


def _watermark_matches(stream: BinaryIO, previous: dict[str, int | str]) -> bool:
    offset = int(previous["offset"])
    head_length = int(previous["head_length"])
    if head_length != min(offset, UNDATED_HEAD_FINGERPRINT_CAP):
        return False
    stream.seek(0)
    current = hashlib.sha256(stream.read(head_length)).hexdigest()
    return current == previous["head_hash"]


def recent_log_lines(
    paths: Iterable[Path],
    since: datetime,
    now: datetime,
    *,
    undated_checkpoint: Path | None = None,
) -> tuple[list[str], list[str]]:
    """Read the 24h window, baselining unseen undated launchd files at EOF."""

    lines: list[str] = []
    undated_offsets, warnings = _load_undated_offsets(undated_checkpoint)
    found_any = False
    for path in paths:
        if not path.is_file():
            continue
        found_any = True
        try:
            stat = path.stat()
            include_undated = (
                path.name == "dcserver.launchd.stderr.log"
                and datetime.fromtimestamp(stat.st_mtime, timezone.utc) >= since
            )
            checkpoint_key = str(path.resolve())
            previous = undated_offsets.get(checkpoint_key)
            with path.open("rb") as stream:
                previous_offset = 0
                if previous is None and undated_checkpoint is not None:
                    # The first persisted observation establishes a watermark;
                    # pre-existing undated history has no reliable 24h timestamp.
                    previous_offset = stat.st_size
                elif (
                    previous is not None
                    and previous["device"] == stat.st_dev
                    and previous["inode"] == stat.st_ino
                    and 0 <= int(previous["offset"]) <= stat.st_size
                    and _watermark_matches(stream, previous)
                ):
                    previous_offset = int(previous["offset"])
                stream.seek(0)
                while raw_bytes := stream.readline():
                    line_end = stream.tell()
                    raw_line = raw_bytes.decode("utf-8", errors="replace")
                    if extract_severity(raw_line) is None:
                        continue
                    line = raw_line.rstrip("\n")
                    timestamp = _parse_line_timestamp(line)
                    if timestamp is not None:
                        if since <= timestamp <= now + timedelta(minutes=5):
                            lines.append(line)
                    elif include_undated and line_end > previous_offset:
                        # Identity, offset, and head fingerprint make appended
                        # ranges eligible once. Rotation or detected rewrite
                        # restarts at byte zero; first observation baselines EOF.
                        lines.append(line)
                if path.name == "dcserver.launchd.stderr.log":
                    final_offset = stream.tell()
                    # A rewrite beyond the cap that reproduces the first 64 KiB
                    # cannot be distinguished from an append. For append-only
                    # launchd logs, reproducing that prefix (or a SHA collision)
                    # after truncate/regrow is considered operationally negligible.
                    head_length, head_hash = _head_fingerprint(stream, final_offset)
                    undated_offsets[checkpoint_key] = {
                        "device": stat.st_dev,
                        "inode": stat.st_ino,
                        "offset": final_offset,
                        "head_length": head_length,
                        "head_hash": head_hash,
                    }
        except OSError as error:
            warnings.append(f"could not read {path}: {error}")
    if not found_any:
        warnings.append("no dcserver stdout or launchd stderr log files were found")
    warnings.extend(_save_undated_offsets(undated_checkpoint, undated_offsets))
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
        str(OPEN_ISSUE_LIMIT),
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
    if len(issues) == OPEN_ISSUE_LIMIT:
        return (
            issues,
            f"open-issue dedup may be truncated at {OPEN_ISSUE_LIMIT} results; drafts suppressed",
        )
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


def threshold_from_env(value: str | None) -> tuple[int, str | None]:
    configured = value if value is not None else str(DEFAULT_DAILY_THRESHOLD)
    try:
        return positive_int(configured), None
    except (ValueError, argparse.ArgumentTypeError):
        return (
            DEFAULT_DAILY_THRESHOLD,
            "invalid AGENTDESK_LOG_DIGEST_THRESHOLD "
            f"value {configured!r}; using default {DEFAULT_DAILY_THRESHOLD}",
        )


def parse_args() -> argparse.Namespace:
    env_threshold, threshold_warning = threshold_from_env(
        os.environ.get("AGENTDESK_LOG_DIGEST_THRESHOLD")
    )
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=runtime_root())
    parser.add_argument("--repo", default=os.environ.get("AGENTDESK_LOG_DIGEST_REPO", REPOSITORY))
    parser.add_argument(
        "--threshold",
        type=positive_int,
        default=None,
    )
    parser.add_argument("--now", help="RFC3339 test/diagnostic override")
    args = parser.parse_args()
    if args.threshold is None:
        args.threshold = env_threshold
        args.threshold_warning = threshold_warning
    else:
        args.threshold_warning = None
    return args


def main() -> int:
    args = parse_args()
    now = datetime.fromisoformat(args.now.replace("Z", "+00:00")) if args.now else datetime.now(timezone.utc)
    if now.tzinfo is None:
        now = now.replace(tzinfo=timezone.utc)
    now = now.astimezone(timezone.utc)
    since = now - timedelta(days=1)
    window_label = f"{since:%Y-%m-%d %H:%M}–{now:%Y-%m-%d %H:%M} UTC"

    lines, warnings = recent_log_lines(
        dcserver_log_paths(args.root),
        since,
        now,
        undated_checkpoint=args.root
        / "runtime"
        / "daily-log-digest"
        / "undated-line-offsets.json",
    )
    if args.threshold_warning:
        warnings.append(args.threshold_warning)
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
