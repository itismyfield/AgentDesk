"""Recognize complete inline tool-card messages without treating them as answers."""

from typing import Any


def is_tool_card(content: str) -> bool:
    lines = [line for line in content.splitlines() if line.strip()]
    return bool(lines) and all(line.startswith("-# ▸ ") for line in lines)


def tool_card_body(message: dict[str, Any]) -> str | None:
    content = message.get("content") or ""
    return content if isinstance(content, str) and is_tool_card(content) else None
