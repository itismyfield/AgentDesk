"""Unknown outcome witnesses retain raw emissions and scoped distinct evidence."""
from .manifest import SOURCE, shape, text, uint
from .policy import canonical


def unknown_checks(manifest, values):
    scope, runtime = manifest["scope"], manifest["runtime"]
    baseline = values["baseline"]
    rows = [r for r in values["log"] if r.get("event") == "n1_outcome_unknown"]
    before = baseline["counters"]["unknown_outcome_events"]
    end = values["end_snapshot"]["counters"]["unknown_outcome_events"]
    raw = sum(r["seq"] > baseline["seq_high_water"] for r in rows)
    reasons, witnesses = [], set()
    if end < before or end - before != raw:
        reasons.append("unknown_counter_mismatch")
    if before != len(rows) - raw:
        reasons.append("unknown_history_incomplete")
    keys = {"event", "schema_version", "boot_id", "channel_id", "provider", "seq", "observed_at_us",
            "source_id", "witness_offset", "logical_turn_key", "reason"}
    logical_spec = {"provider": text, "channel_id": uint, "provider_session_id": text, "native_turn_id": text}
    closed = {canonical(r.get("logical_turn_key")) for r in values["log"] if r.get("event") == "n1_turn_closed"}
    for row in rows:
        logical = row.get("logical_turn_key")
        valid = (set(row) == keys and shape(row.get("source_id"), SOURCE)
                 and canonical(row["source_id"]) == canonical(scope["source"])
                 and uint(row.get("witness_offset")) and row["witness_offset"] <= scope["closure_offset"]
                 and text(row.get("reason")) and len(row["reason"].encode()) <= 4096
                 and (logical is None or shape(logical, logical_spec)
                      and canonical(logical["channel_id"]) == canonical(scope["channel_id"])
                      and logical["provider"] == scope["provider"] and logical["provider_session_id"] == scope["session_id"]
                      and len(logical["native_turn_id"].encode()) <= 4096))
        if not valid:
            reasons.append("unknown_witness_invalid")
            continue
        witness = canonical([runtime["boot_id"], scope["provider"], scope["channel_id"], row["source_id"],
                             row["witness_offset"], row["reason"], logical["native_turn_id"] if logical else None])
        witnesses.add(witness)
        if logical is not None and canonical(logical) in closed:
            reasons.append("unknown_outcome_conflict")
    if witnesses:
        reasons.append("unknown_outcome_present")
    return dict(unknown_outcome_events=end-before, unknown_witnesses_distinct=len(witnesses)), reasons
