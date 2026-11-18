"""Build schema1 summaries and evaluate them without filesystem or notification side effects."""

import copy
from datetime import datetime
import math
from urllib.parse import quote

IDENTITY = ("instance_id", "boot_id", "head", "binary_sha256", "profile_hash")
KEY_FIELDS = ("scenario", "provider", "host", "surface", "state", "origin", "busy_basis",
              "placement", "output_profile", "operation", "occurrence")
USAGE = ("native_turns", "input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens")


def _number(value):
    return type(value) in (int, float) and math.isfinite(value) and value >= 0


def _stamp(value):
    if _number(value):
        return value
    if isinstance(value, str):
        try:
            parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
            return parsed.timestamp() if parsed.tzinfo else None
        except (ValueError, OverflowError):
            pass
    return None


def _strings(value):
    return isinstance(value, list) and all(isinstance(item, str) and item for item in value)


def _identity(value):
    return (all(isinstance(value.get(key), str) and value[key] for key in IDENTITY)
            and all(len(value[key]) == length and set(value[key]) <= set("0123456789abcdef")
                    for key, length in (("head", 40), ("binary_sha256", 64), ("profile_hash", 64))))


def occurrence_key(row, profile="default"):
    """Use the planned manifest qualifiers; a driver-supplied key cannot replace the plan."""
    data = dict(row)
    cell = row.get("cell")
    if isinstance(cell, str) and "-" in cell:
        provider, host = cell.split("-", 1)
        data.setdefault("provider", provider)
        data.setdefault("host", "tmux" if host == "tui" else host)
    data.setdefault("output_profile", profile)
    for key in KEY_FIELDS:
        value = data.get(key)
        if value is None and key in {"busy_basis", "placement"}:
            continue
        if key == "occurrence" and type(value) is int and value > 0:
            continue
        if not isinstance(value, str) or not value.strip():
            data[key] = "unknown"
    return "/".join("none" if data.get(key, "unknown") is None else quote(str(data.get(key, "unknown")), safe="-_.:")
                    for key in KEY_FIELDS)


def build(record, config):
    """Copy raw evidence into the separate continuous schema; never rewrite source reports."""
    value = copy.deepcopy(record)
    value.update(schema_version=1, suite=record.get("suite", record.get("bundle", "unknown")))
    for key in IDENTITY:
        value.setdefault(key, config.get(key))
    rows = value.setdefault("occurrences", [])
    for row in rows:
        row["key"] = occurrence_key(row, record.get("profile", "default"))
        if "assertion_contract" in record or "required_assertions" in config:
            contract = record.get("assertion_contract", config.get("required_assertions"))
            row["assertions_expected"] = copy.deepcopy(contract.get(row.get("scenario"), [])) if isinstance(contract, dict) else []
        row.setdefault("assertions_expected", [])
        row.setdefault("assertions", [])
        row.setdefault("real_provider_contacted", False)
        row.setdefault("evidence_paths", [])
    value.setdefault("expected", [row["key"] for row in rows])
    value.setdefault("usage", {})
    value.setdefault("usage_complete", False)
    value.setdefault("attempts", [])
    value["elapsed_s"] = record.get("elapsed_s", max(0, (_stamp(record.get("completed_at")) or 0)
                                                   - (_stamp(record.get("started_at")) or 0)))
    verdict = evaluate(value)
    value.update(result=verdict["result"], reason=verdict["reason"])
    return value


def evaluate(value, *, head=None, identity=None):
    """Derive coverage from complete live evidence; a claimed PASS never supplies missing facts."""
    result = {"result": "UNKNOWN", "reason": "UNSUPPORTED_SCHEMA", "live": 0, "expected": 0,
              "assertions": 0, "assertions_expected": 0}

    def finish(status, reason):
        return {**result, "result": status, "reason": reason}

    if not isinstance(value, dict) or type(value.get("schema_version")) is not int or value.get("schema_version") != 1:
        return result
    expected, rows = value.get("expected"), value.get("occurrences")
    if not _strings(expected) or not isinstance(rows, list) or not all(isinstance(row, dict) for row in rows):
        return finish("UNKNOWN", "MALFORMED_SUMMARY")
    result["expected"] = len(expected)
    if head is not None and value.get("head") != head:
        return finish("UNKNOWN", "STALE_HEAD")
    if not _identity(value):
        return finish("UNEVALUABLE", "MISSING_IDENTITY")
    if identity is not None and any(value.get(key) != identity.get(key) for key in IDENTITY):
        return finish("UNEVALUABLE", "IDENTITY_MISMATCH")
    keys = [row.get("key") for row in rows]
    if not _strings(keys):
        return finish("UNKNOWN", "MALFORMED_SUMMARY")
    if len(set(keys)) != len(keys) or len(set(expected)) != len(expected):
        return finish("UNKNOWN", "DUPLICATE_OCCURRENCE")
    for row in rows:
        required, assertions = row.get("assertions_expected"), row.get("assertions")
        matches = (row.get("actual_occurrence_key") == row["key"] == occurrence_key(row, value.get("profile", "default"))
                   and "unknown" not in row["key"].split("/"))
        if _strings(required):
            result["assertions_expected"] += len(required)
            if matches and isinstance(assertions, list):
                result["assertions"] += len({item["key"] for item in assertions if isinstance(item, dict)
                                             and isinstance(item.get("key"), str) and item["key"] in required
                                             and item.get("complete") is True and item.get("status") == "PASS"})
        if (row.get("real_provider_contacted") is True and not value.get("fixture") and not value.get("dry_run")
                and not row.get("fixture") and not row.get("dry_run") and row.get("evidence_paths")
                and matches and all(row.get(key) == value[key] for key in IDENTITY)):
            result["live"] += 1
    if value.get("result") in {"DEFERRED_BUDGET", "DEFERRED_LEASE", "DISABLED"}:
        return finish(value["result"], value["result"])
    if value.get("result") == "FAIL" or any(row.get("status") == "FAIL" for row in rows):
        return finish("FAIL", "FUNCTIONAL_FAILURE")
    if not expected or not rows:
        return finish("UNEVALUABLE", "NO_LIVE_OCCURRENCES")
    reasons, providers = [], set()
    for row in rows:
        required, assertions = row.get("assertions_expected"), row.get("assertions")
        assertion_keys = [item.get("key") for item in assertions if isinstance(item, dict)] if isinstance(assertions, list) else []
        if isinstance(assertions, list) and any(isinstance(item, dict) and item.get("status") == "FAIL" for item in assertions):
            return finish("FAIL", "FUNCTIONAL_FAILURE")
        if row.get("status") != "PASS":
            reasons.append(("UNEVALUABLE" if row.get("status") == "UNEVALUABLE" else "PARTIAL",
                            row.get("reason") or row.get("status") or "NOT_EXECUTED"))
            continue
        if value.get("fixture") or value.get("dry_run") or row.get("fixture") or row.get("dry_run"):
            reasons.append(("UNEVALUABLE", "FIXTURE"))
        elif value.get("timeout") or row.get("timeout"):
            reasons.append(("UNEVALUABLE", "TIMEOUT"))
        elif value.get("dirty_residue") or row.get("dirty_residue"):
            reasons.append(("UNEVALUABLE", "DIRTY_RESIDUE"))
        elif row.get("real_provider_contacted") is not True:
            reasons.append(("UNEVALUABLE", "NO_PROVIDER_CONTACT"))
        elif row["key"] != occurrence_key(row, value.get("profile", "default")):
            reasons.append(("UNEVALUABLE", "OCCURRENCE_KEY_MISMATCH"))
        elif "unknown" in row["key"].split("/"):
            reasons.append(("UNEVALUABLE", "MISSING_OCCURRENCE_CONTRACT"))
        elif row.get("actual_occurrence_key") != row["key"]:
            reasons.append(("UNEVALUABLE", "OCCURRENCE_WITNESS_MISMATCH"))
        elif any(row.get(key) != value[key] for key in IDENTITY):
            reasons.append(("UNEVALUABLE", "IDENTITY_MISMATCH"))
        elif not _strings(row.get("evidence_paths")) or not row["evidence_paths"]:
            reasons.append(("UNEVALUABLE", "MISSING_EVIDENCE"))
        elif (not _strings(required) or not required or len(set(required)) != len(required)
              or not isinstance(assertions, list) or len(assertion_keys) != len(assertions)
              or not _strings(assertion_keys) or len(set(assertion_keys)) != len(assertion_keys) or set(required) - set(assertion_keys)
              or any(item.get("complete") is not True or item.get("status") != "PASS" for item in assertions)):
            reasons.append(("UNEVALUABLE", "MISSING_ASSERTIONS"))
        else:
            cell = row.get("cell")
            provider = cell.split("-")[0] if isinstance(cell, str) else row["key"].split("/")[1] if "/" in row["key"] else "unknown"
            providers.add(provider)
    if set(keys) != set(expected):
        return finish("PARTIAL", "MISSING_OCCURRENCES")
    if reasons:
        status, reason = next((item for item in reasons if item[0] == "UNEVALUABLE"), reasons[0])
        return finish(status, reason)
    usage = value.get("usage")
    if (value.get("usage_complete") is not True or not isinstance(usage, dict) or not providers <= usage.keys()
            or any(not isinstance(usage[provider], dict)
                   or any(type(usage[provider].get(key)) is not int or not _number(usage[provider][key]) for key in USAGE) for provider in providers)):
        return finish("UNEVALUABLE", "USAGE_UNKNOWN")
    if value.get("result") in {"ABANDONED", "UNEVALUABLE", "TIMEOUT"}:
        return finish("UNEVALUABLE", value["result"])
    attempts = value.get("attempts", [])
    if not isinstance(attempts, list) or not all(isinstance(item, dict) for item in attempts):
        return finish("UNKNOWN", "MALFORMED_SUMMARY")
    failures = [item for item in attempts if item.get("result") == "FAIL"]
    if failures:
        return finish("RECOVERED_ENV" if all(item.get("classification") == "ENV_SUSPECT" for item in failures)
                      else "FLAKY", "PREVIOUS_FAILURE")
    return finish("PASS", "none")


def consume(value, status, *, head, now):
    """One read-only decision shared by briefing and monitor consumers."""
    check = evaluate(value, head=head) if value is not None else {
        "result": "NO_DATA", "reason": "NO_DATA", "live": 0, "expected": 0, "assertions": 0, "assertions_expected": 0}
    source = value if isinstance(value, dict) else {}
    check.update(suite=source.get("suite", "none"), head=source.get("head", head), age_s=None, stale=0)
    state = status if isinstance(status, dict) else {}
    monitor = {"enabled": int(state.get("enabled") is True), "heartbeat_age_s": None,
               "last_run": state.get("last_completed_run") or "none", "pending_alerts": state.get("outbox_pending", 0),
               "next_due": state.get("next_due") if state.get("next_due") is not None else "none"}

    heartbeat = _stamp(state.get("heartbeat_at"))
    if heartbeat is not None and _number(now) and heartbeat <= now:
        monitor["heartbeat_age_s"] = int(now - heartbeat)

    def override(result, reason):
        check.update(result=result, reason=reason)

    if value is None and status is None:
        return {"check": check, "monitor": monitor}
    if state.get("unavailable") is True or source.get("unavailable") is True:
        override("UNKNOWN", "HOST_UNAVAILABLE")
    elif type(state.get("schema_version")) is not int or state.get("schema_version") != 1 or type(state.get("enabled")) is not bool:
        override("UNKNOWN", "UNSUPPORTED_STATUS")
    elif state["enabled"] is False:
        override("DISABLED", "DISABLED")
    elif not isinstance(head, str) or len(head) != 40 or not set(head) <= set("0123456789abcdef"):
        override("UNKNOWN", "HEAD_REQUIRED")
    else:
        heartbeat, completed = _stamp(state.get("heartbeat_at")), _stamp(source.get("completed_at"))
        started = _stamp(source.get("started_at"))
        tick = state.get("tick_interval_s")
        if heartbeat is None or not _number(tick) or tick == 0 or not _number(now):
            override("UNKNOWN", "INVALID_TIMESTAMP")
        elif heartbeat > now or completed is not None and completed > now or started is not None and started > now:
            override("UNKNOWN", "FUTURE_TIMESTAMP")
        else:
            monitor["heartbeat_age_s"] = int(now - heartbeat)
            if now - heartbeat >= 2 * tick:
                override("UNKNOWN", "HEARTBEAT_STALE")
            elif value is not None and (completed is None or started is None or completed < started):
                override("UNKNOWN", "INVALID_TIMESTAMP")
            elif completed is not None:
                check["age_s"] = int(now - completed)
                interval = state.get("rotation_interval_s")
                bounds = [interval, state.get("max_run_s"), state.get("tick_grace_s")]
                if interval is not None and (not all(_number(bound) for bound in bounds) or interval == 0):
                    override("UNKNOWN", "INVALID_FRESHNESS")
                elif interval is not None and now - completed > sum(bounds):
                    check["stale"] = 1
                    override("UNKNOWN", "STALE")
    return {"check": check, "monitor": monitor}


def _token(value):
    return "none" if value is None else "_".join(str(value).split()) or "none"


def format_lines(view):
    check, monitor = view["check"], view["monitor"]
    return ("E2E_CHECK " + " ".join(f"{key}={_token(check[key])}" for key in ("suite", "head", "result"))
            + f" live={check['live']}/{check['expected']} assertions={check['assertions']}/{check['assertions_expected']}"
            + " " + " ".join(f"{key}={_token(check[key])}" for key in ("age_s", "stale", "reason")),
            "E2E_MONITOR " + " ".join(f"{key}={_token(monitor[key])}" for key in
                                     ("enabled", "heartbeat_age_s", "last_run", "pending_alerts", "next_due")))
