"""Readiness policy and same-response attribution for explicitly waived Unknowns."""

import datetime as dt
import re
from collections import Counter, defaultdict
from urllib.parse import unquote_to_bytes


PREFIX = "turn_presence_unknown"
ATOM = re.compile(r"[a-z][a-z0-9_]*\Z")
GROUP = re.compile(r"turn_presence_unknown:(claude|codex):(~|(?:[A-Za-z0-9_.-]|%[0-9A-F]{2})+):"
                   r"([a-z][a-z0-9_]*):([a-z][a-z0-9_]*):([1-9][0-9]*):(0|[1-9][0-9]*)\Z")
STAMP = re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?(?:Z|[+-]\d\d:\d\d)\Z")


def unknown_family(reason):
    return isinstance(reason, str) and reason.startswith(PREFIX)


def reason_allowed(reason, allowed):
    return any(reason.startswith(p[:-1]) if p.endswith("*") else reason == p for p in allowed)


def read_health(read, base_url, allowed):
    status, body = read(base_url, "/api/health", timeout=5)
    reasons = body.get("degraded_reasons") if isinstance(body, dict) else None
    if isinstance(reasons, list) and any(unknown_family(r) and reason_allowed(r, allowed) for r in reasons):
        status, body = read(base_url, "/api/health/detail", timeout=5)
        return status, body, True
    return status, body, False


def agent_component(value):
    if value is None:
        return "~"
    return "".join(chr(b) if 65 <= b <= 90 or 97 <= b <= 122 or 48 <= b <= 57 or b in b"_.-"
                   else f"%{b:02X}" for b in value.encode("utf-8"))


def channel_id(value):
    if type(value) is str and re.fullmatch(r"[1-9][0-9]*", value):
        value = int(value)
    return value if type(value) is int and 0 < value < 2**64 else None


def unknown_error(body, target_channel):
    """A group may be waived only when every complete detail row proves it is foreign."""
    reasons = body.get("degraded_reasons")
    if not isinstance(reasons, list) or any(not isinstance(r, str) for r in reasons):
        return "Unknown reasons must be a string array"
    groups = [r for r in reasons if unknown_family(r)]
    if not groups and not body.get(PREFIX):
        return None
    target = channel_id(target_channel)
    rows = body.get(PREFIX)
    if target is None or not isinstance(rows, list) or not rows or not groups:
        return "Unknown target/detail missing or invalid"
    expected, actual, seen = Counter(), defaultdict(list), set()
    try:
        for group in groups:
            match = GROUP.fullmatch(group)
            if match is None:
                return "Unknown group malformed"
            provider, agent, reason, leaf, count, age = match.groups()
            decoded = None if agent == "~" else unquote_to_bytes(agent).decode("utf-8")
            if agent_component(decoded) != agent or int(age) < 60:
                return "Unknown group is not canonical"
            key = (provider, agent, reason, leaf)
            if any(item[:4] == key for item in expected):
                return "Unknown group duplicated"
            expected[(*key, int(count), int(age))] += 1
        for row in rows:
            if not isinstance(row, dict):
                return "Unknown detail row malformed"
            channel, provider, agent = row["channel_id"], row["provider"], row["agent_id"]
            reason, leaf, age, stamp = row["reason"], row["hold_leaf"], row["elapsed_secs"], row["unknown_since"]
            if type(channel) is not int or channel_id(channel) is None or channel in seen:
                return "Unknown detail channel invalid or duplicated"
            seen.add(channel)
            if channel == target:
                return "Unknown target channel is never waived"
            if (provider not in ("claude", "codex") or not isinstance(reason, str) or not ATOM.fullmatch(reason)
                    or (leaf is not None and (not isinstance(leaf, str) or not ATOM.fullmatch(leaf)))
                    or (agent is not None and not isinstance(agent, str)) or agent == ""
                    or type(age) is not int or age < 60):
                return "Unknown detail fields invalid"
            if not isinstance(stamp, str) or not STAMP.fullmatch(stamp) or dt.datetime.fromisoformat(stamp).utcoffset() is None:
                return "Unknown detail timestamp invalid"
            actual[(provider, agent_component(agent), reason, leaf or "none")].append(age)
    except (KeyError, TypeError, ValueError, UnicodeError):
        return "Unknown detail/group invalid"
    reconstructed = Counter({(*key, len(ages), max(ages)): 1 for key, ages in actual.items()})
    if reconstructed != expected:
        return "Unknown group/detail mismatch"
    return None


def ready_violations(payload, *, allowed_statuses=("healthy",), allowed_degraded_reasons=(), target_channel=None):
    status = str(payload.get("status") or "").lower()
    allowed_status_set = {s.lower() for s in allowed_statuses}
    strict_healthy = allowed_status_set == {"healthy"}
    violations = []
    if status not in allowed_status_set:
        violations.append(f"status={status or '<missing>'}")
    if strict_healthy and payload.get("ok") is False:
        violations.append("ok=false")
    if strict_healthy and payload.get("degraded") is True:
        violations.append("degraded=true")
    if payload.get("fully_recovered") is False:
        violations.append("fully_recovered=false")
    reasons = payload.get("degraded_reasons") or []
    if isinstance(reasons, list):
        candidates = [r for r in reasons if unknown_family(r) and reason_allowed(r, allowed_degraded_reasons)]
        if candidates or payload.get(PREFIX):
            error = unknown_error(payload, target_channel)
            if error:
                violations.append(error)
            if (status != "degraded" or payload.get("fully_recovered") is not True
                    or ("ok" in payload and payload["ok"] is not False)
                    or ("degraded" in payload and payload["degraded"] is not True)):
                violations.append("Unknown detail readiness inconsistent")
        blocked = [str(r) for r in reasons if not reason_allowed(str(r), allowed_degraded_reasons)]
        if blocked:
            violations.append(f"degraded_reasons={blocked}")
    elif reasons:
        violations.append(f"degraded_reasons={reasons!r}")
    return violations
