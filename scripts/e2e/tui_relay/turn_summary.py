"""Recognize only the bounded standalone terminal summaries emitted by O and Legacy."""

import re

_DURATION = r"(?:\d+시간 \d+분|\d+분(?: \d+초)?|\d+초)"
_PERCENT = r"(?:100|[1-9]?\d)"
_TIME = r"(?:[01]\d|2[0-3]):[0-5]\d"
_DAY_TIME = rf"(?:0[1-9]|1[0-2])/(?:0[1-9]|[12]\d|3[01]) {_TIME}"
_QUOTA = rf"(?: · 5h {_PERCENT}% \({_TIME}\))?(?: · 7d {_PERCENT}% \({_DAY_TIME}\))?"
TURN_SUMMARY_PATTERN = re.compile(
    rf"\A-# (?:⏱️ {_DURATION}(?: · 📦 {_PERCENT}%)?{_QUOTA}"
    rf"|✅(?: 완료| {_DURATION}(?: · 📦 {_PERCENT}%)?| · 📦 {_PERCENT}%)"
    rf"|(?:⏹ 중단|⚠️ 오류)(?: · {_DURATION})?(?: · 📦 {_PERCENT}%)?)\Z"
)


def is_turn_summary(content: str) -> bool:
    return (isinstance(content, str)
            and len(content.encode("utf-16-le", "surrogatepass")) <= (256 if content.startswith("-# ⏱️ ") else 160)
            and bool(TURN_SUMMARY_PATTERN.fullmatch(content)))


def turn_summary_body(message: dict) -> str | None:
    content = message.get("content")
    return content if is_turn_summary(content) else None
