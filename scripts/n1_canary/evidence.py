"""Pinned file input, conservative observation checks, and SELECT-only PG input."""
import os
from pathlib import Path
import subprocess
from urllib.parse import parse_qsl, quote, urlencode, urlsplit, urlunsplit

from . import Unknown, loads
from .manifest import RUNTIME, digest, shape, snowflake, uint
from .outcomes import unknown_checks

JSONL = {"transcript", "bindings", "log", "o_ledger"}
COUNTERS = ("closed_events", "aborted_events", "unknown_outcome_events", "create_attempt",
            "create_committed", "attempt", "succeeded_post", "succeeded_patch", "failed_or_uncertain")
PG_OPTIONS = "-c default_transaction_read_only=on -c statement_timeout=10000"
PG_SELECT = """SELECT json_build_object(
 'intake_outbox', (SELECT coalesce(json_agg(t), '[]'::json) FROM
 (SELECT id, channel_id, user_msg_id, turn_kind, status, created_at, completed_at
 FROM public.intake_outbox WHERE channel_id = :'channel'
 AND created_at BETWEEN :'start'::timestamptz AND :'end'::timestamptz ORDER BY id) t),
 'turn_lifecycle_events', (SELECT coalesce(json_agg(t), '[]'::json) FROM
 (SELECT turn_id, channel_id, kind, details_json, created_at
 FROM public.turn_lifecycle_events WHERE channel_id = :'channel'
 AND created_at BETWEEN :'start'::timestamptz AND :'end'::timestamptz
 ORDER BY created_at, turn_id) t));"""


def pg_connection(dsn):
    # Passwords use PGPASSWORD; URI options cannot weaken the SELECT connection limits.
    try:
        url = urlsplit(dsn)
        if url.scheme in {"postgres", "postgresql"}:
            pairs = parse_qsl(url.query.replace("+", "%2B"), keep_blank_values=True, strict_parsing=True)
            if url.password is not None or url.fragment or any(
                    key in {"password", "passfile", "service", "servicefile"} for key, _ in pairs):
                raise Unknown("pg_dsn_invalid")
            pairs = [(key, value) for key, value in pairs if key not in {"options", "connect_timeout"}]
            pairs.extend((("options", PG_OPTIONS), ("connect_timeout", "5")))
            return urlunsplit(url._replace(query=urlencode(pairs, quote_via=quote)))
        if url.scheme or "=" in dsn or "://" in dsn:
            raise Unknown("pg_dsn_invalid")
        name = dsn.replace("\\", "\\\\").replace("'", "\\'")
        return f"dbname='{name}' options='{PG_OPTIONS}' connect_timeout=5"
    except ValueError:
        raise Unknown("pg_dsn_invalid") from None


def pg_select(env_name, scope):
    dsn = os.environ.get(env_name)
    if not dsn:
        raise Unknown("pg_unavailable")
    env = {**os.environ, "PGCONNECT_TIMEOUT": "5", "PGOPTIONS": PG_OPTIONS}
    args = ["psql", "--dbname", pg_connection(dsn), "-X", "-w", "-qAt", "-v", "ON_ERROR_STOP=1",
            "-v", f"channel={scope['channel_id']}", "-v", f"start={scope['start_at']}",
            "-v", f"end={scope['end_at']}"]
    try:
        result = subprocess.run(args, input=PG_SELECT, capture_output=True, text=True,
                                env=env, timeout=20, check=True)
        value = loads(result.stdout)
        if not pg_valid(value):
            raise Unknown("pg_invalid")
        # PG stores channel IDs as TEXT; audits use exact Python integers.
        for rows in value.values():
            for row in rows:
                if not snowflake(row.get("channel_id")) or int(row["channel_id"]) != scope["channel_id"]:
                    raise Unknown("pg_invalid")
                row["channel_id"] = int(row["channel_id"])
        return value
    except (OSError, ValueError, subprocess.SubprocessError):
        raise Unknown("pg_unavailable") from None


def pg_valid(value):
    return (isinstance(value, dict) and set(value) == {"intake_outbox", "turn_lifecycle_events"}
            and all(isinstance(rows, list) and all(isinstance(row, dict) for row in rows)
                    for rows in value.values()))


def read_files(manifest, base, overrides=None, omit=()):
    values, raw, reasons, refs = {}, {}, [], []
    for name, ref in manifest["files"].items():
        if name in omit:
            continue
        path = Path((overrides or {}).get(name) or base / ref["path"])
        try:
            data = path.read_bytes()
        except OSError:
            reasons.append(f"file_unavailable:{name}")
            continue
        if digest(data) != ref["sha256"]:
            reasons.append(f"file_digest_mismatch:{name}")
            continue
        try:
            if name == "executable":
                value = data
            elif name in JSONL:
                if data and not data.endswith(b"\n"):
                    raise Unknown("partial_jsonl")
                value = [loads(line) for line in data.splitlines()]
                if not all(isinstance(row, dict) for row in value):
                    raise Unknown("invalid_jsonl")
            else:
                value = loads(data)
                if not isinstance(value, (dict, list)):
                    raise Unknown("invalid_document")
            raw[name], values[name] = data, value
            refs.append({"input": name, "sha256": digest(data)})
        except (ValueError, UnicodeError):
            reasons.append(f"file_invalid:{name}")
    return values, raw, reasons, refs


def input_checks(manifest, values, raw):
    reasons = []
    proof = values.get("process_proof")
    if (not shape(proof, {**RUNTIME, "executable_path": lambda v: isinstance(v, str)})
            or any(proof[k] != v for k, v in manifest["runtime"].items())
            or proof["executable_path"] != f"/proc/{manifest['runtime']['pid']}/exe"
            or digest(raw.get("executable", b"")) != manifest["runtime"]["executable_sha256"]):
        reasons.append("build_identity_unverified")
    scope = manifest["scope"]
    native = raw.get("transcript", b"")
    start, end = scope["baseline_offset"], scope["closure_offset"]
    if (end > len(native) or digest(native[:start]) != manifest["source_digests"]["prefix_sha256"]
            or digest(native[start:end]) != manifest["source_digests"]["window_sha256"]):
        reasons.append("source_range_unverified")
    rows, binding = values.get("bindings", []), manifest["binding"]
    expected = {"boot_id": manifest["runtime"]["boot_id"], "channel_id": scope["channel_id"],
                "provider": scope["provider"], "source": scope["source"],
                "identity": binding["identity"], "status": "resolved"}
    if (len(rows) != binding["seq_end"] - binding["seq_start"] + 1
            or any(type(row.get("seq")) is not int or row["seq"] != n
                   or any(row.get(k) != v for k, v in expected.items())
                   for n, row in enumerate(rows, binding["seq_start"]))):
        reasons.append("binding_discontinuous")
    return reasons


def observation_checks(manifest, values, producer_supported=False):
    reasons = [] if producer_supported else ["unknown_outcome_unproduced"]
    summary = dict(snapshot_status="unavailable", dropped_events=None, sink_errors=None,
                   open_placeholder_attempts=None, unknown_outcome_events=None,
                   unknown_witnesses_distinct=None, sequence_complete=False)
    keys = {"status", "schema_version", "boot_id", "channel_id", "seq_high_water", "counters",
            "dropped_events", "sink_errors", "observer_unhealthy", "open_attempts",
            "open_attempts_complete", "first_confirmation", "confirmation_observed"}
    snaps = [values.get(name, {}) for name in ("baseline", "end_snapshot")]
    for snap in snaps:
        if not isinstance(snap, dict) or snap.get("status") != "available":
            reasons.append("snapshot_unavailable")
            return summary, {}, reasons
        counters = snap.get("counters")
        if (set(snap) != keys or snap["schema_version"] != 2
                or snap["boot_id"] != manifest["runtime"]["boot_id"]
                or snap["channel_id"] != manifest["scope"]["channel_id"]
                or not all(uint(snap[k]) for k in ("seq_high_water", "dropped_events", "sink_errors", "open_attempts"))
                or not isinstance(counters, dict) or set(counters) != set(COUNTERS)
                or not all(uint(x) for x in counters.values())
                or any(type(snap[k]) is not bool for k in ("observer_unhealthy", "open_attempts_complete", "confirmation_observed"))):
            return summary, {}, reasons + ["snapshot_invalid"]
    summary["snapshot_status"] = "available"
    healthy = all(s["dropped_events"] == 0 and s["sink_errors"] == 0 and not s["observer_unhealthy"] for s in snaps)
    if not healthy:
        reasons.append("observation_unhealthy")
    else:
        summary.update(dropped_events=0, sink_errors=0)
    complete = all(s["open_attempts_complete"] for s in snaps)
    if not complete:
        reasons.append("open_attempts_incomplete")
    if healthy and complete:
        summary["open_placeholder_attempts"] = snaps[1]["open_attempts"]
        if any(s["open_attempts"] for s in snaps):
            reasons.append("open_attempts_present")
    interval = manifest["observation"]
    rows = values.get("log", [])
    scope = manifest["scope"]
    valid = (len(rows) == interval["seq_end"] - interval["seq_start"] + 1
             and interval["seq_start"] <= snaps[0]["seq_high_water"] <= snaps[1]["seq_high_water"] == interval["seq_end"]
             and all(type(row.get("seq")) is int and row["seq"] == n
                     and row.get("boot_id") == manifest["runtime"]["boot_id"]
                     and row.get("channel_id") == scope["channel_id"] and row.get("provider") == scope["provider"]
                     and row.get("schema_version") == 2 and uint(row.get("observed_at_us"))
                     for n, row in enumerate(rows, interval["seq_start"])))
    summary["sequence_complete"] = valid
    if not valid:
        reasons.append("observation_sequence_incomplete")
    events = [row for row in rows if row.get("event") == "n1_mode_confirmed" and row.get("seq") == interval["confirmation_seq"]]
    event = events[0] if len(events) == 1 and valid else {}
    actual, observed, before = event.get("actual_confirmed_at_us"), event.get("observed_at_us"), event.get("confirmation_window_start")
    confirmation = {"event_observed_at_us": observed, "actual_confirmed_at_us": None}
    if (not uint(actual) or not isinstance(before, dict) or set(before) != {"seq", "at_us"}
            or not uint(before["at_us"]) or not uint(before["seq"])
            or not before["at_us"] <= actual <= observed
            or before["seq"] >= event["seq"] or actual != interval["actual_confirmed_at_us"]
            or not all(s["confirmation_observed"] and s["first_confirmation"] == before for s in snaps)):
        reasons.append("confirmation_boundary_unknown")
    else:
        confirmation["actual_confirmed_at_us"] = actual
    if producer_supported and valid:
        unknown, extra = unknown_checks(manifest, values)
        reasons.extend(extra)
        if healthy and complete and valid and not any(r != "unknown_outcome_present" and r != "unknown_outcome_conflict" for r in extra):
            summary.update(unknown)
    return summary, confirmation, reasons
