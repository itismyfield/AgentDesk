"""Durable notifications drained by one supervisor under its channel lock.

The transport acknowledges delivery with True and receives a stable event id for deduplication.
Exhausted or unacknowledged messages stay pending for the status consumer.
"""
import hashlib
import json
from pathlib import Path

from .store import read_json, write_json

QUIET = {"DISABLED", "DEFERRED_BUDGET", "DEFERRED_LEASE", "NOT_DUE"}


def _path(state_dir):
    return Path(state_dir) / "outbox.json"


def _load(state_dir):
    value = read_json(_path(state_dir), {"schema_version": 1, "observed": {}, "events": {}})
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        raise ValueError("unsupported notification outbox schema")
    return value


def _hash(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def enqueue(state_dir, summary, now):
    """Observe a completed summary; enqueue first trouble and each subsequent state change."""
    if summary.get("schema_version") != 1:
        raise ValueError("notification requires continuous summary schema 1")
    result = summary["result"]
    if result in QUIET:
        return None
    qualifier = [summary.get(k) for k in ("suite", "qualifier", "profile_hash", "profile")]
    rows = summary.get("occurrences", [])
    planned = summary.get("expected", [str(row.get("key")) for row in rows])
    planned = sorted(str(key) for key in planned)
    scope = _hash([summary.get("head"), qualifier, planned])
    problems = []
    for row in rows:
        assertions = row.get("assertions")
        assertions = assertions if isinstance(assertions, list) else []
        failed = sorted(str(item.get("key")) for item in assertions
                        if isinstance(item, dict) and item.get("status") == "FAIL")
        if row.get("status") != "PASS" or failed:
            problems.append([str(row.get("key")), row.get("status"), row.get("reason"), failed])
    problems.sort(key=lambda item: json.dumps(item, sort_keys=True))
    reasons = sorted({str(row.get("reason") or row.get("status"))
                      for row in summary.get("occurrences", []) if row.get("status") != "PASS"})
    if not reasons and result != "PASS":
        reasons = [str(summary.get("reason") or result)]
    signature = _hash([result, summary.get("reason"), reasons, problems])
    value = _load(state_dir)
    previous = value["observed"].get(scope)
    if previous and previous["signature"] == signature:
        return None
    sequence = previous["sequence"] + 1 if previous else 1
    value["observed"][scope] = {"signature": signature, "sequence": sequence}
    event_id = None
    if result != "PASS" or previous:
        event_id = _hash([scope, signature, sequence])
        value["events"][event_id] = {
            "event_id": event_id, "run_id": summary.get("run_id"), "head": summary.get("head"),
            "suite": summary.get("suite"), "qualifier": qualifier, "result": result, "reasons": reasons,
            "occurrences": problems,
            "created_at": now, "next_attempt_at": now, "attempts": 0, "status": "pending", "exhausted": False,
        }
    write_json(_path(state_dir), value)
    return event_id


def pending(state_dir):
    """Count all unacknowledged events, including exhausted deliveries."""
    return sum(event["status"] == "pending" for event in _load(state_dir)["events"].values())


def dispatch(state_dir, send, now, *, max_attempts=3, retry_s=60, limit=1):
    """Try a bounded number of due events; send(event) must return True for confirmed delivery."""
    if type(max_attempts) is not int or max_attempts < 1 or retry_s < 0 or type(limit) is not int or limit < 1:
        raise ValueError("notification retry bounds must be positive")
    value = _load(state_dir)
    attempted = sent = 0
    for event in sorted(value["events"].values(), key=lambda e: (e["created_at"], e["event_id"])):
        if (attempted >= limit or event["status"] != "pending" or event["attempts"] >= max_attempts
                or now < event["next_attempt_at"]):
            continue
        attempted += 1
        event.update(attempts=event["attempts"] + 1, next_attempt_at=now + retry_s)
        event["exhausted"] = event["attempts"] >= max_attempts
        # Charge the attempt before calling transport, so a crashed sender cannot retry forever.
        write_json(_path(state_dir), value)
        try:
            acknowledged = send(dict(event)) is True
        except Exception as error:
            acknowledged = False
            event["last_error"] = type(error).__name__
        if acknowledged:
            event.update(status="sent", sent_at=now, exhausted=False)
            event.pop("last_error", None)
            sent += 1
        write_json(_path(state_dir), value)
    return {"attempted": attempted, "sent": sent,
            "pending": sum(event["status"] == "pending" for event in value["events"].values())}
