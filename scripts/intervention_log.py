#!/usr/bin/env python3
"""Record manual-operation interventions and surface repeated patterns (#4264).

Future consolidation may share parsing with the deferred #4263 log-digest helper.
"""

from __future__ import annotations

import argparse
import datetime as dt
import fcntl
import json
import os
import socket
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Mapping, Sequence

VALID_TYPES = ("marker-clear", "re-baseline", "force-restart")
INTERVENTION_RECURRENCE_THRESHOLD = 3
HISTORY_REL_PATH = Path("scripts/intervention_history.toml")
RECENT_EVENT_LIMIT = 5


@dataclass(frozen=True)
class InterventionEvent:
    type: str
    timestamp: str
    node: str
    note: str
    issue: int | None
    count: int


@dataclass(frozen=True)
class RecordResult:
    event: InterventionEvent
    draft_path: Path | None
    issue_create_invoked: bool


def parse_history(text: str) -> list[InterventionEvent]:
    parsed = tomllib.loads(text)
    schema_version = parsed.get("schema_version")
    if (
        not isinstance(schema_version, int)
        or isinstance(schema_version, bool)
        or schema_version != 1
    ):
        raise ValueError("intervention history schema_version must be integer 1")
    rows = parsed.get("intervention", [])
    if not isinstance(rows, list):
        raise ValueError("intervention history [[intervention]] entries must be an array")

    events: list[InterventionEvent] = []
    last_count: dict[str, int] = {}
    for index, row in enumerate(rows, start=1):
        if not isinstance(row, dict):
            raise ValueError(f"intervention event {index} must be a TOML table")
        event_type = row.get("type")
        if event_type not in VALID_TYPES:
            raise ValueError(
                f"intervention event {index}: type must be one of {VALID_TYPES}, "
                f"got {event_type!r}"
            )
        for field in ("timestamp", "node", "note"):
            if not isinstance(row.get(field), str) or not row[field].strip():
                raise ValueError(
                    f"intervention event {index}: {field} must be a non-empty string"
                )
        issue = row.get("issue")
        if issue is not None and (
            isinstance(issue, bool) or not isinstance(issue, int) or issue <= 0
        ):
            raise ValueError(f"intervention event {index}: issue must be positive")
        count = row.get("count")
        expected = last_count.get(event_type, 0) + 1
        if isinstance(count, bool) or not isinstance(count, int) or count <= 0:
            raise ValueError(
                f"intervention event {index} ({event_type}): count must be a "
                f"positive integer, got {count!r}"
            )
        if count != expected:
            print(
                f"WARNING: intervention event {index} ({event_type}) has "
                f"out-of-sequence count {count}; using positional count {expected}",
                file=sys.stderr,
            )
        event = InterventionEvent(
            type=event_type,
            timestamp=row["timestamp"],
            node=row["node"],
            note=row["note"],
            issue=issue,
            count=expected,
        )
        events.append(event)
        last_count[event_type] = expected
    return events


def recurrence_count(events: Sequence[InterventionEvent], type: str) -> int:
    if type not in VALID_TYPES:
        raise ValueError(f"invalid intervention type: {type!r}")
    return sum(event.type == type for event in events)


def next_count(events: Sequence[InterventionEvent], type: str) -> int:
    return recurrence_count(events, type) + 1


def crosses_threshold(count: int) -> bool:
    return count > INTERVENTION_RECURRENCE_THRESHOLD


def build_draft_body(
    type: str, count: int, recent_events: Sequence[InterventionEvent]
) -> str:
    rows = ["| Timestamp | Node | Type | Count | Issue | Note |",
            "| --- | --- | --- | ---: | ---: | --- |"]
    for event in recent_events[-RECENT_EVENT_LIMIT:]:
        issue = f"#{event.issue}" if event.issue is not None else "—"
        note = event.note.replace("|", "\\|").replace("\n", " ")
        rows.append(
            f"| {event.timestamp} | {event.node} | {event.type} | "
            f"{event.count} | {issue} | {note} |"
        )
    return "\n".join(
        [
            f"# ops: repeated manual intervention — {type}",
            "",
            f"- Intervention type: `{type}`",
            f"- Per-type recurrence count: **{count}**",
            f"- Promotion threshold: more than {INTERVENTION_RECURRENCE_THRESHOLD}",
            "",
            "## Recent intervention rows",
            "",
            *rows,
            "",
            "## Recommendation",
            "",
            "**판정 모델 재설계 후보**로 검토한다. agentdesk-issue-pipeline §0의 "
            '"첫 사고 때 판정 모델 재설계" 원칙에 따라 반복 수동 복구를 자동 판정·복구 '
            "모델로 대체할 수 있는지 분석한다.",
            "",
        ]
    )


def recurrence_warning_messages(
    type: str, count: int, draft_path: Path, create_confirmed: bool
) -> tuple[str, ...]:
    if not crosses_threshold(count):
        return ()
    messages = (
        f"WARNING: MANUAL INTERVENTION RECURRENCE EXCEEDED: {type} count={count} "
        f"(threshold {INTERVENTION_RECURRENCE_THRESHOLD}); draft={draft_path}",
    )
    if not create_confirmed:
        messages += (
            "draft retained; set AGENTDESK_INTERVENTION_CREATE_ISSUE=confirmed "
            "to file.",
        )
    return messages


def _toml_string(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def _event_block(event: InterventionEvent) -> str:
    lines = [
        "[[intervention]]",
        f"type = {_toml_string(event.type)}",
        f"timestamp = {_toml_string(event.timestamp)}",
        f"node = {_toml_string(event.node)}",
        f"note = {_toml_string(event.note)}",
    ]
    if event.issue is not None:
        lines.append(f"issue = {event.issue}")
    lines.extend((f"count = {event.count}", ""))
    return "\n".join(lines)


def resolve_root(environ: Mapping[str, str]) -> Path:
    configured = environ.get("AGENTDESK_ROOT_DIR")
    if configured:
        return Path(configured).expanduser().resolve()
    return Path(__file__).resolve().parents[1]


def record_intervention(
    *,
    type: str,
    note: str,
    node: str,
    issue: int | None,
    history_path: Path,
    logs_dir: Path,
    environ: Mapping[str, str],
    runner: Callable[..., object] = subprocess.run,
    timestamp: str | None = None,
) -> RecordResult:
    if type not in VALID_TYPES:
        raise ValueError(f"invalid intervention type: {type!r}")
    if not note.strip() or not node.strip():
        raise ValueError("note and node must be non-empty")
    if issue is not None and (
        isinstance(issue, bool) or not isinstance(issue, int) or issue <= 0
    ):
        raise ValueError("issue must be a positive integer")

    with history_path.open("r+", encoding="utf-8") as history_file:
        fcntl.flock(history_file.fileno(), fcntl.LOCK_EX)
        history_text = history_file.read()
        events = parse_history(history_text)
        now = dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds")
        event = InterventionEvent(
            type=type,
            timestamp=timestamp or now.replace("+00:00", "Z"),
            node=node,
            note=note,
            issue=issue,
            count=next_count(events, type),
        )
        separator = "" if history_text.endswith("\n\n") else "\n"
        block = separator + _event_block(event)
        parse_history(history_text + block)
        history_file.seek(0, os.SEEK_END)
        history_file.write(block)
        history_file.flush()
        fcntl.flock(history_file.fileno(), fcntl.LOCK_UN)

    draft_path: Path | None = None
    invoked = False
    confirmed = environ.get("AGENTDESK_INTERVENTION_CREATE_ISSUE") == "confirmed"
    if crosses_threshold(event.count):
        logs_dir.mkdir(parents=True, exist_ok=True)
        draft_path = logs_dir / f"intervention-recurrence-{type}.draft.md"
        relevant = [row for row in (*events, event) if row.type == type]
        draft_path.write_text(
            build_draft_body(type, event.count, relevant), encoding="utf-8"
        )
        for message in recurrence_warning_messages(
            type, event.count, draft_path, confirmed
        ):
            print(message, file=sys.stderr)
        if confirmed and draft_path.is_file():
            invoked = True
            result = runner(
                [
                    "gh", "issue", "create",
                    "--repo", "itismyfield/AgentDesk",
                    "--title",
                    f"ops: repeated manual intervention ({type}, count {event.count})",
                    "--body-file", str(draft_path),
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            if getattr(result, "returncode", 1) != 0:
                print(
                    f"WARNING: gh issue create failed; draft retained: {draft_path}",
                    file=sys.stderr,
                )
    return RecordResult(event, draft_path, invoked)


def _positive_issue(value: str) -> int:
    if value.startswith("#"):
        raise argparse.ArgumentTypeError("issue must not include a leading '#'")
    try:
        issue = int(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError("issue must be a positive integer") from exc
    if issue <= 0:
        raise argparse.ArgumentTypeError("issue must be a positive integer")
    return issue


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    record = commands.add_parser("record", help="append one intervention event")
    record.add_argument("--type", required=True, choices=VALID_TYPES)
    record.add_argument("--note", required=True)
    record.add_argument("--node", default=socket.gethostname())
    record.add_argument("--issue", type=_positive_issue)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    root = resolve_root(os.environ)
    try:
        result = record_intervention(
            type=args.type, note=args.note, node=args.node, issue=args.issue,
            history_path=root / HISTORY_REL_PATH,
            logs_dir=root / "logs",
            environ=os.environ,
        )
    except (OSError, tomllib.TOMLDecodeError, ValueError) as exc:
        print(f"ERROR: intervention not recorded: {exc}", file=sys.stderr)
        return 2
    print(f"recorded {result.event.type} intervention count={result.event.count}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
