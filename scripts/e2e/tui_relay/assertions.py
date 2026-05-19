"""Assertion primitives for E2E scenarios."""

from __future__ import annotations

import dataclasses
from typing import Any


class AssertionError(Exception):
    pass


@dataclasses.dataclass
class Window:
    setup_marker_id: str
    teardown_marker_id: str | None = None
    messages: list[dict[str, Any]] = dataclasses.field(default_factory=list)

    def add(self, message: dict[str, Any]) -> None:
        self.messages.append(message)


def message_count_between_markers(window: Window, *, low: int, high: int) -> None:
    actual = len(window.messages)
    if not (low <= actual <= high):
        raise AssertionError(
            f"message count {actual} outside [{low}, {high}] in window between markers"
        )


def no_duplicate_content(window: Window) -> None:
    seen: set[str] = set()
    for message in window.messages:
        body = (message.get("content") or "").strip()
        if not body:
            continue
        if body in seen:
            raise AssertionError(f"duplicate Discord message body: {body[:80]!r}")
        seen.add(body)


def text_present(window: Window, *, needle: str) -> None:
    for message in window.messages:
        if needle in (message.get("content") or ""):
            return
    raise AssertionError(f"expected to find {needle!r} in window, got {len(window.messages)} messages")


def no_control_chars(window: Window) -> None:
    forbidden = {chr(c) for c in (0x07, 0x08, 0x0C, 0x1B, 0x7F, 0x85)}
    for message in window.messages:
        body = message.get("content") or ""
        leaked = forbidden.intersection(body)
        if leaked:
            raise AssertionError(f"control byte leaked into Discord message: {sorted(leaked)!r}")
