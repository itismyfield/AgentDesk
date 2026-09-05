"""Narrow, pure E-22 observation predicate; KNOWN_GAP is not a dedup repair.

Captures come from unfiltered fetch responses, not reconstructed latest bodies.
Only complete single-page observations are supported; no pagination inference,
normalization, extra polling, or cleanup/deletion inference is performed here.
"""

from datetime import datetime, timedelta
import math
import re

PROFILE = "e22_headless_cumulative_republication_v1"
CHANNEL_ID = "1509350490461180105"
BOT_ID = "1474932782395293736"
SETUP_BOT_ID = "1481522187197218816"
PRE = "[E2E:E22:PRE]"
MARKERS = (PRE, "[E2E:E22:HEAD]", "TOOL_USE_TEXT_E22_OK", "[E2E:E22:TAIL]")
BODY = PRE + MARKERS[1] + "\n" + MARKERS[2] + "\n" + MARKERS[3]
COMMAND = "`python3 -c \"import time; time.sleep(20); print('E22_TOOL_DONE')\"`"
# tool_markdown::format_command_tool_input renders an optional, dynamic caption
# clamped to 45 UTF-8 bytes. Do not promote six historical captions to a contract.
PREVIEW = re.compile(
    re.escape(PRE) + r"\n\n[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏] ⚙ Bash: (?:(?P<caption>[^\r\n`]{1,45}): )?"
    + re.escape(COMMAND) + r"\n• \[Bash\] 실행(?: · 2회)?"
)


def _need(condition, reason):
    if not condition:
        raise ValueError(reason)


def _utc(value):
    parsed = datetime.fromisoformat(value)
    _need(parsed.utcoffset() == timedelta(0), "UTC_timestamp_required")
    return parsed.timestamp()


def _supported_preview(content):
    match = PREVIEW.fullmatch(content)
    return match is not None and len((match["caption"] or "").encode("utf-8")) <= 45


def evaluate_e22_known_gap(captures, *, run_id, cell, scenario, channel_id, bot_id, after_id):
    """Return KNOWN_GAP/#5731, FAIL, or NOT_EVALUABLE; never normal PASS.

    Each capture is {pages: [{channel_id, after_id, limit, messages, observed_at}]}.
    Request metadata and UTC epoch response time must be supplied by the fetch
    boundary. An empty actual page is valid; absent or partial capture is not.
    """
    if captures is None or captures == []:
        return {"classification": "NOT_EVALUABLE", "reason": "missing_captures"}
    try:
        _need(channel_id == CHANNEL_ID and bot_id == BOT_ID, "target_binding")
        _need(cell == "claude-tui" and scenario == "E-22", "scenario_binding")
        _need(isinstance(run_id, str) and bool(run_id) and not any(c.isspace() for c in run_id), "run_binding")
        _need(isinstance(after_id, str) and after_id.isdecimal(), "cursor_binding")
        _need(isinstance(captures, list), "capture_list_required")
        pages, all_rows, previous_time = [], [], 0.0
        for capture in captures:
            raw_pages = capture["pages"]
            _need(isinstance(raw_pages, list), "page_list_required")
            if len(raw_pages) != 1:
                return {"classification": "NOT_EVALUABLE", "reason": "single_page_required"}
            page = raw_pages[0]
            _need(page["channel_id"] == channel_id and page["after_id"] == after_id, "request_binding")
            _need(type(page["limit"]) is int and page["limit"] == 100, "request_limit")
            rows, observed = page["messages"], page["observed_at"]
            _need(isinstance(rows, list), "raw_array_required")
            if len(rows) >= page["limit"]:
                return {"classification": "NOT_EVALUABLE", "reason": "possibly_truncated_page"}
            _need(type(observed) in (int, float) and math.isfinite(observed)
                  and observed > 0 and observed >= previous_time, "capture_clock")
            previous_time = observed
            ids = []
            for row in rows:
                _need(isinstance(row["id"], str) and row["id"].isdecimal(), "raw_id")
                ids.append(int(row["id"]))
                _need(int(row["id"]) > int(after_id), "raw_cursor")
                _need(row["channel_id"] == channel_id and isinstance(row["content"], str), "raw_channel_content")
                _need(isinstance(row["author"]["id"], str) and row["author"]["id"].isdecimal(), "raw_author")
                created = _utc(row["timestamp"])
                _need(created <= observed, "future_creation")
                if row["edited_timestamp"] is not None:
                    _need(created < _utc(row["edited_timestamp"]) <= observed, "invalid_edit_clock")
            _need(len(ids) == len(set(ids)), "duplicate_page_id")
            pages.append(page)
            all_rows.extend(rows)

        setup_text = f"### E2E SETUP E-22 cell={cell} run={run_id}"
        setups = [row for row in all_rows if row["content"] == setup_text]
        _need(len({row["id"] for row in setups}) == 1, "unique_setup")
        setup = setups[0]
        _need(all(row == setup for row in setups), "setup_changed")
        _need(setup in pages[-1]["messages"], "setup_missing_from_final_capture")
        _need(setup["author"]["id"] == SETUP_BOT_ID and setup["author"].get("bot") is True
              and setup["edited_timestamp"] is None, "setup_author_edit")
        lower = int(setup["id"])
        boundaries = [int(row["id"]) for row in all_rows if int(row["id"]) > lower
                      and row["content"].startswith(("### E2E SETUP ", "### E2E TEARDOWN "))]
        upper = min(boundaries) if boundaries else max(int(row["id"]) for row in all_rows) + 1
        current = [row for row in pages[-1]["messages"] if lower < int(row["id"]) < upper]
        past = [row for row in all_rows if lower < int(row["id"]) < upper]
        pair = sorted((row for row in current if PRE in row["content"]), key=lambda row: int(row["id"]))
        _need(len(pair) == 2, "exactly_two_current_PRE_ids")
        old, new = pair
        _need(old["content"] == PRE and new["content"] == BODY, "exact_raw_bodies")
        _need(_utc(setup["timestamp"]) < _utc(old["timestamp"]) < _utc(new["timestamp"]), "creation_order")
        _need(old["edited_timestamp"] is not None and new["edited_timestamp"] is None, "final_edit_shape")
        for marker in MARKERS:
            hits = {row["id"] for row in past if marker in row["content"]}
            _need(len(hits) == (2 if marker == PRE else 1), "historical_marker_ids")
            _need(all(row["content"].count(marker) <= 1 for row in past), "raw_marker_multiplicity")
        histories = {}
        for final in pair:
            states = []
            for page in pages:
                found = [row for row in page["messages"] if row["id"] == final["id"]]
                _need(bool(found) or not states, "observed_id_disappeared")
                if not found:
                    continue
                row = found[0]
                _need(row["author"]["id"] == bot_id and row["author"].get("bot") is True, "candidate_author")
                _need(all(row.get(key) == [] for key in ("attachments", "embeds", "components")), "nontext_candidate")
                _need(row["timestamp"] == final["timestamp"], "creation_changed")
                if states and row == states[-1][0]:
                    continue
                if states:
                    _need(_utc(states[-1][0]["edited_timestamp"] or row["timestamp"])
                          < _utc(row["edited_timestamp"] or row["timestamp"]), "history_edit_order")
                states.append((row, page["observed_at"]))
            _need(bool(states) and states[-1][0] == final, "latest_history_mismatch")
            if final is old:
                _need(len(states) >= 2, "missing_before_edit_witness")
                _need(all(_supported_preview(row["content"]) for row, _ in states[:-1]), "unsupported_preview")
            else:
                _need(len(states) == 1, "new_body_history_changed")
            histories[final["id"]] = states
        before, after = histories[old["id"]][-2:]
        return {"classification": "KNOWN_GAP", "known_gap": "#5731", "profile": PROFILE,
                "message_ids": [old["id"], new["id"]], "setup_id": setup["id"],
                "raw_duplicate_preserved": True,
                "witness": {"message_id": old["id"], "before": before[0]["content"], "after": PRE,
                            "before_edited_timestamp": before[0]["edited_timestamp"],
                            "after_edited_timestamp": after[0]["edited_timestamp"],
                            "before_observed_at": before[1], "after_observed_at": after[1]}}
    except (KeyError, TypeError, ValueError, AttributeError, OverflowError, IndexError) as error:
        return {"classification": "FAIL", "reason": str(error)}
