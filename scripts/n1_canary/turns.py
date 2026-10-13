"""Logical deduplication, physical observations, and independent native unit plans."""
from . import Unknown
from .manifest import digest
from .native import parse
from .policy import canonical, plan


def derive(turns, scope):
    logical, physical, sealed, observations, candidates = {}, {}, {}, [], []
    repeated, physical_repeated, unit_repeated, aborted = 0, 0, 0, 0
    for turn in turns:
        key = (scope["provider"], scope["channel_id"], scope["session_id"], turn["native_turn_id"])
        signature = digest(canonical([turn["records"], turn["outcome"], turn["autonomous"]]))
        if key in logical and logical[key] != signature:
            raise Unknown("native_identity_conflict")
        position = (key, canonical(scope["source"]), turn["start_offset"], turn["end_offset"])
        if position in physical:
            if physical[position] != signature:
                raise Unknown("native_physical_conflict")
            physical_repeated += 1
            continue
        physical[position] = signature
        logical_key = dict(provider=key[0], channel_id=key[1], provider_session_id=key[2], native_turn_id=key[3])
        physical_key = dict(provider=key[0], channel_id=key[1], source_id=scope["source"],
                            start_offset=turn["start_offset"], end_offset=turn["end_offset"], native_turn_id=key[3])
        observation = dict(logical_turn_key=logical_key, physical_observation_key=physical_key,
                           outcome=turn["outcome"], autonomous=turn["autonomous"], native_record_digest=signature)
        observations.append(observation)
        for item in turn["units"]:
            planned = plan(item, scope)
            unit_key = canonical(planned["unit_key"])
            position = {"range": item["range"], "logical_turn_key": logical_key}
            if unit_key in sealed:
                if sealed[unit_key]["seal_signature"] != planned["seal_signature"]:
                    raise Unknown("native_unit_conflict")
                unit_repeated += 1
                sealed[unit_key]["observations"].append(position)
            else:
                sealed[unit_key] = {**planned, "observations": [position]}
        if key in logical:
            repeated += 1
            continue
        logical[key] = signature
        if turn["start_offset"] < scope["baseline_offset"]:
            continue
        if turn["outcome"] == "aborted":
            aborted += 1
        else:
            candidates.append(observation)
    reasons = ["native_origin_unverified"] if candidates else []
    if len(candidates) < 50:
        reasons.append("native_sample_under_50")
    elif candidates[49]["physical_observation_key"]["end_offset"] != scope["fiftieth_offset"]:
        reasons.append("native_sample_boundary_mismatch")
    units = list(sealed.values())
    if not any(u["pieces"] for u in units):
        reasons.append("native_no_publishable_units")
    summary = dict(completed_distinct=None, completed_native_distinct=len(candidates), logical_reobservations=repeated,
                   physical_reobservations=physical_repeated, aborted_distinct=aborted, unit_reobservations=unit_repeated)
    return dict(turns=summary, observations=observations, candidates=candidates[:50], units=units, reasons=reasons)


def reconstruct(raw, scope):
    return derive(parse(raw, scope), scope)
