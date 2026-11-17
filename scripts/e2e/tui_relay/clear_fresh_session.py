"""Provider identity evidence for clear scenarios; no session selection or mutation."""

from __future__ import annotations

import urllib.parse
import uuid
import copy

from . import assertions


def plan(scenario, *, cell):
    """Apply the YAML's explicit clear checks only to its named TUI/Herdr cells."""
    profile = scenario.get("fresh_session_after_clear")
    if profile is None or scenario.get("_clear_fresh_session_planned"):
        return scenario
    if (scenario.get("id") != "E-50" or not isinstance(profile, dict)
            or set(profile) != {"cells", "secret", "remember_prompt", "probe_prompt", "assertions"}
            or profile["cells"] != ["claude-tui", "claude-herdr"]
            or not isinstance(profile["secret"], str) or not profile["secret"]
            or profile["secret"] not in profile["remember_prompt"]
            or profile["secret"] in profile["probe_prompt"]
            or [next(iter(spec), None) for spec in profile["assertions"]]
               != ["provider_session_changed", "provider_text_present", "marker_absent"]):
        raise ValueError("E-50 fresh checks require explicit cells, private pre-clear secret and three assertions")
    if cell not in profile["cells"]:
        return scenario
    planned = copy.deepcopy(scenario)
    steps = planned["steps"]
    for label, replacement in ((":T1]", profile["remember_prompt"]),
                               (":AFTER_CLEAR]", profile["probe_prompt"])):
        matches = [step for step in steps if label in step.get("send_discord_prompt", "")]
        if len(matches) != 1:
            raise ValueError("E-50 fresh checks require one T1 and one AFTER_CLEAR input")
        matches[0]["send_discord_prompt"] = replacement
    clear = [i for i, step in enumerate(steps) if step.get("send_discord_prompt", "").startswith("!clear ")]
    if len(clear) != 1:
        raise ValueError("E-50 fresh checks require one text !clear")
    steps.insert(clear[0], {"capture_provider_session": {"label": "before_clear"}})
    steps.append({"capture_provider_session": {"label": "after_clear"}})
    planned["assertions"].extend(copy.deepcopy(profile["assertions"]))
    planned["_clear_fresh_session_planned"] = True
    return planned


def owns(step):
    return "capture_provider_session" in step


def run_step(driver, step, *, base_url, channel_id, cell, record):
    return capture(driver._read_api_json, base_url=base_url, channel_id=channel_id,
                   provider=driver.cell_provider(cell), label=step["capture_provider_session"]["label"],
                   record=record)


def capture(read_json, *, base_url, channel_id, provider, label, record):
    """Capture the channel's observed native ID, refusing incomplete or foreign evidence."""
    selector = urllib.parse.quote(str(channel_id), safe="")
    status, payload = read_json(base_url, f"/api/agents/{selector}/session-evidence")
    if (status != 200 or not isinstance(payload, dict)
            or str(payload.get("channel_id")) != str(channel_id)
            or payload.get("provider") != provider):
        raise assertions.AssertionError("clear provider-session evidence unavailable or mismatched")
    session_id = payload.get("raw_provider_session_id")
    try:
        if not isinstance(session_id, str) or str(uuid.UUID(session_id)) != session_id:
            raise ValueError
    except ValueError:
        raise assertions.AssertionError("clear requires an observed native provider session ID") from None
    identity = {"provider": provider, "session_id": session_id}
    record.setdefault("clear_provider_sessions", {})[label] = identity
    return identity


def assert_changed(params, *, record):
    """Both captures must exist and differ; an absent post-clear ID never proves fresh."""
    before_label, after_label = params.get("before"), params.get("after")
    sessions = (record or {}).get("clear_provider_sessions", {})
    before, after = sessions.get(before_label), sessions.get(after_label)
    if (not isinstance(before, dict) or not isinstance(after, dict)
            or not before.get("session_id") or not after.get("session_id")
            or before.get("provider") != after.get("provider")):
        raise assertions.AssertionError("clear requires matching before/after provider-session captures")
    if before["session_id"] == after["session_id"]:
        raise assertions.AssertionError("clear resumed the pre-clear native provider session")
