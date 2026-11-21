"""Recognize only the bounded standalone terminal summaries emitted by O and Legacy."""

import re

_DURATION = r"(?:\d+시간 \d+분|\d+분(?: \d+초)?|\d+초)"
_PERCENT = r"(?:100|[1-9]?\d)"
_TIME = r"(?:[01]\d|2[0-3]):[0-5]\d"
_DAY_TIME = rf"(?:0[1-9]|1[0-2])/(?:0[1-9]|[12]\d|3[01]) {_TIME}"
_END = rf" \((?:{_TIME}|{_DAY_TIME})\)"
_CONTEXT = rf" · 📦 {_PERCENT}%"
_MODEL = r"[A-Za-z0-9](?:[A-Za-z0-9 ./+_:@\[\]-]{0,62}[A-Za-z0-9./+_:@\[\]-])?"
_EFFORT = r"[A-Za-z0-9-]{1,16}(?:, fast)?"
_MODEL_OR_EFFORT = rf" · (?:{_MODEL}(?: \({_EFFORT}\))?|{_EFFORT})"
_METADATA = rf"(?:{_MODEL_OR_EFFORT}(?:{_CONTEXT})?|{_CONTEXT})"
_FIVE_HOUR = rf"5h {_PERCENT}% \({_TIME}\)"
_WEEKLY = rf"7d {_PERCENT}% \({_DAY_TIME}\)"
_FABLE = rf"Fable 7d {_PERCENT}% \({_DAY_TIME}\)"
_QUOTA_LINE = rf"\n-# (?:{_FIVE_HOUR}(?: · {_WEEKLY})?(?: · {_FABLE})?|{_WEEKLY}(?: · {_FABLE})?|{_FABLE})"
_OLD_QUOTA = rf"(?: · {_FIVE_HOUR})?(?: · {_WEEKLY})?"

# Retained Discord history may still have one-line summaries with inline quota.
_OLD = (
    rf"⏱️ {_DURATION}(?:{_CONTEXT})?{_OLD_QUOTA}"
    rf"|✅(?: 완료| {_DURATION}(?:{_CONTEXT})?|{_CONTEXT})"
    rf"|(?:⏹ 중단|⚠️ 오류)(?: · {_DURATION})?(?:{_CONTEXT})?"
)
_CURRENT = (
    rf"(?:⏱️ {_DURATION}(?:{_END})?(?:{_METADATA})?|✅ 완료(?:{_END})?|✅(?:{_END})?{_METADATA})"
    rf"(?:{_QUOTA_LINE})?|(?:⏹ 중단|⚠️ 오류)(?: · {_DURATION})?(?:{_END})?(?:{_METADATA})?"
)
TURN_SUMMARY_PATTERN = re.compile(
    rf"\A-# (?:(?P<legacy>{_OLD})|{_CURRENT})\Z"
)


def is_turn_summary(content: str) -> bool:
    if not isinstance(content, str):
        return False
    match = TURN_SUMMARY_PATTERN.fullmatch(content)
    if match is None:
        return False
    if match.group("legacy") is None:
        limit = 256
    else:
        limit = 128 if content.startswith("-# ⏱️ ") else 80
    return len(content.encode("utf-16-le", "surrogatepass")) // 2 <= limit


def turn_summary_body(message: dict) -> str | None:
    content = message.get("content")
    return content if is_turn_summary(content) else None
