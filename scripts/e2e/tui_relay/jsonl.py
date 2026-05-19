"""Helpers for tailing Claude / Codex JSONL transcripts."""

from __future__ import annotations

import json
import os
import time
from pathlib import Path
from typing import Any, Iterable


def claude_transcript_path(cwd: Path, session_id: str, claude_home: Path | None = None) -> Path:
    """Replicate `claude_tui::transcript_tail::claude_transcript_path` from Rust."""

    home = claude_home or Path(os.path.expanduser("~/.claude"))
    encoded = encode_project_path(cwd.resolve())
    return home / "projects" / encoded / f"{session_id}.jsonl"


def encode_project_path(path: Path) -> str:
    return str(path).replace("/", "-")


def codex_rollout_dir(claude_home: Path | None = None) -> Path:
    return Path(os.path.expanduser("~/.codex/sessions"))


def tail_jsonl_lines(path: Path, start_offset: int = 0) -> Iterable[tuple[int, dict[str, Any]]]:
    """Yield `(line_end_offset, parsed_json)` from the given JSONL file."""

    if not path.exists():
        return
    with path.open("rb") as fp:
        fp.seek(start_offset)
        offset = start_offset
        for raw in fp:
            offset += len(raw)
            text = raw.decode("utf-8", errors="replace").strip()
            if not text:
                continue
            try:
                parsed = json.loads(text)
            except json.JSONDecodeError:
                continue
            yield offset, parsed


def wait_for_envelope(
    path: Path,
    *,
    predicate,
    timeout_s: float = 30.0,
    poll_interval_s: float = 0.5,
    start_offset: int = 0,
) -> tuple[int, dict[str, Any]] | None:
    deadline = time.monotonic() + timeout_s
    last_offset = start_offset
    while time.monotonic() < deadline:
        for offset, envelope in tail_jsonl_lines(path, start_offset=last_offset):
            last_offset = offset
            if predicate(envelope):
                return offset, envelope
        time.sleep(poll_interval_s)
    return None
