"""Footer-retirement assertions over complete messages and every observed edit."""

import re

from .tool_cards import is_tool_card
from .turn_summary import is_turn_summary


def _retired_chrome(content):
    from . import assertions

    if not isinstance(content, str) or not content or is_tool_card(content) or is_turn_summary(content):
        return False
    if content.strip() in ("...", "…"):
        return True
    if re.fullmatch(r"[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏] 계속 처리 중", content):
        return False
    lines = [line.strip() for line in content.splitlines() if line.strip()]
    panel = re.compile(r"^(?:Processing\.\.\.|[🟢🔴📋🔚]|✅ (?:응답 완료|완료)|📦 응답 완료 ·|⏸(?:️)? (?:중단|일시 정지)|⚠️ (?:오류|응답 지연))")
    return any(panel.search(line) or assertions._is_completion_footer_head(line)
               or any(pattern.search(line) for pattern in assertions._COMPLETION_FOOTER_CONTINUATION_PATTERNS)
               for line in lines)


def no_status_chrome(window):
    from . import assertions

    rows = {str(row.get("id") or ""): row for row in window.raw_messages}
    views = [(str(row.get("id") or ""), "raw", row.get("content"))
             for row in window.raw_messages if not assertions.is_our_send(row)]
    views.extend((str(update.get("id") or ""), field, update.get(field))
                 for update in window.message_updates
                 if not assertions.is_our_send(rows.get(str(update.get("id") or ""), {}))
                 for field in ("before", "after"))
    for message_id, snapshot, content in views:
        if _retired_chrome(content):
            raise assertions.AssertionError(
                f"retired status chrome in {snapshot} snapshot {message_id}: {content[:120]!r}")


def turn_summary_after_body(window, *, body_marker, header="-# ✅", before_marker=None, tool_label=None):
    from . import assertions

    if header not in ("-# ✅", "-# ⏹ 중단", "-# ⚠️ 오류"):
        raise assertions.AssertionError("turn_summary_after_body requires an exact terminal header")
    if not isinstance(body_marker, str) or not body_marker:
        raise assertions.AssertionError("turn_summary_after_body requires a body marker")
    rows = [row for row in window.raw_messages if not assertions.is_our_send(row)
            and str(row.get("id") or "") not in window.deleted_ids
            and (row.get("author") or {}).get("bot") and row.get("type") in (None, 0, 19)]
    bodies = [row for row in rows if body_marker in (assertions.relay_body(row) or "")]
    summaries = [row for row in rows if is_turn_summary(row.get("content"))]
    if len(bodies) != 1 or sum((assertions.relay_body(row) or "").count(body_marker) for row in bodies) != 1:
        raise assertions.AssertionError("turn_summary_after_body requires one exact body marker")
    if len(summaries) != 1 or not (summaries[0]["content"] == header or summaries[0]["content"].startswith(header + " ")):
        raise assertions.AssertionError("turn_summary_after_body requires one standalone summary with the exact header")
    order = assertions._message_order_key
    if order(summaries[0]) <= order(bodies[0]):
        raise assertions.AssertionError("turn summary did not follow the final body")
    if before_marker is not None or tool_label is not None:
        if not isinstance(before_marker, str) or not before_marker or not isinstance(tool_label, str) or not tool_label:
            raise assertions.AssertionError("inline card order requires before_marker and tool_label")
        before = [row for row in rows if before_marker in (assertions.relay_body(row) or "")]
        pattern = re.compile(r"-# ▸ " + re.escape(tool_label) + r"  .+  ✓")
        cards = [row for row in rows if is_tool_card(row.get("content") or "")
                 and any(pattern.fullmatch(line) for line in row["content"].splitlines())]
        if len(before) != 1 or len(cards) != 1 or not order(before[0]) < order(cards[0]) < order(bodies[0]):
            raise assertions.AssertionError("expected body → successful inline tool card → body → summary")
