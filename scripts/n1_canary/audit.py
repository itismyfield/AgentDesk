"""Origin, effects and closure audits; incomplete stages never compose PASS."""
from collections import Counter

from . import Unknown
from .delivery import capture, compare, replay, require
from .discord import message_valid
from .manifest import snowflake, text, uint
from .policy import canonical

LEGACY_FIELDS = {"inputs", "confirmed_deliveries", "delivered_frontier", "owner", "allowed_messages"}
DEBT_FIELDS = {"undelivered", "abandoned", "baseline_messages", "ledger_sha256"}


def origins(native, legacy, values, manifest):
    scope, proofs, known = manifest["scope"], legacy["inputs"], {}
    require(isinstance(proofs, list), "native_origin_unverified")
    require(legacy["owner"] == dict(writer="o", turn_mode="n1", identity=manifest["binding"]["identity"]), "o_owner_unverified")
    for proof in proofs:
        require(isinstance(proof, dict) and set(proof) == {"physical_observation_key", "native_record_digest", "input_kind",
                "discord_message_id", "execution_identity"}
                and proof["execution_identity"] == manifest["binding"]["identity"]
                and proof["input_kind"] in {"tty", "monitor", "autonomous", "discord"}, "native_origin_unverified")
        physical = canonical(proof["physical_observation_key"])
        require(physical not in known, "native_origin_conflict")
        known[physical] = proof
    human_ids = {m["id"] for m in values["discord"]["messages"]
                 if m["author"]["id"] not in {str(i) for i in manifest["discord"]["bot_author_ids"]}}
    current_messages = {m["id"]: m for m in values["discord"]["messages"]}
    origin_ids = human_ids | {mid for mid, m in values["discord"].get("individual", {}).items()
                 if message_valid(m, scope["channel_id"]) and m["id"] == mid
                 and m["author"]["id"] not in {str(i) for i in manifest["discord"]["bot_author_ids"]}
                 and (mid not in current_messages or current_messages[mid] == m)}
    inputs, window_inputs, logical, eligible = set(), set(), set(), []
    for turn in native["observations"]:
        physical = canonical(turn["physical_observation_key"])
        proof = known.pop(physical, None)
        require(proof is not None and proof["native_record_digest"] == turn["native_record_digest"], "native_origin_unverified")
        if proof["input_kind"] == "discord":
            require(snowflake(proof["discord_message_id"]) and proof["discord_message_id"] in origin_ids, "native_origin_unverified")
            inputs.add(proof["discord_message_id"])
            if (proof["discord_message_id"] in human_ids
                    and turn["physical_observation_key"]["end_offset"] > scope["baseline_offset"]):
                window_inputs.add(proof["discord_message_id"])
        else:
            require(proof["discord_message_id"] is None, "native_origin_conflict")
        logical_key = canonical(turn["logical_turn_key"])
        first = logical_key not in logical
        logical.add(logical_key)
        if (first and turn["outcome"] == "completed" and proof["input_kind"] != "discord"
                and turn["physical_observation_key"]["start_offset"] >= scope["baseline_offset"]):
            eligible.append(turn)
    require(not known and human_ids == window_inputs, "native_origin_inventory_incomplete")
    for row in values["pg"]["intake_outbox"]:
        require(row.get("channel_id") == scope["channel_id"] and str(row.get("user_msg_id")) in inputs, "pg_origin_unverified")
    for row in values["pg"]["turn_lifecycle_events"]:
        require(row.get("channel_id") == scope["channel_id"] and text(row.get("turn_id"))
                and any(t["logical_turn_key"]["native_turn_id"] == row["turn_id"] for t in native["observations"]), "pg_origin_unverified")
    require(len(eligible) >= 50, "native_sample_under_50")
    require(eligible[49]["physical_observation_key"]["end_offset"] == scope["fiftieth_offset"], "native_sample_boundary_mismatch")
    return eligible[:50], inputs


def scoped_event(row, manifest):
    return (isinstance(row, dict) and row.get("schema_version") == 2 and type(row.get("schema_version")) is int
            and row.get("boot_id") == manifest["runtime"]["boot_id"] and uint(row.get("seq"))
            and manifest["observation"]["seq_start"] <= row["seq"] <= manifest["observation"]["seq_end"]
            and row.get("channel_id") == manifest["scope"]["channel_id"]
            and row.get("provider") == manifest["scope"]["provider"] and uint(row.get("observed_at_us")))


def effects(native, values, manifest, messages, observation, confirmation, failures):
    reasons, explained, counts, attempts, created, committed, used = [], set(), Counter(), {}, 0, 0, set()
    physical = {canonical(t["physical_observation_key"]): t for t in native["observations"]}
    matched = set()
    seq = manifest["observation"]["confirmation_seq"]
    actual = confirmation.get("actual_confirmed_at_us")
    confirmed = any(scoped_event(r, manifest) and r.get("event") == "n1_mode_confirmed" and r["seq"] == seq for r in values.get("log", []))
    for row in values.get("log", []):
        try:
            require(scoped_event(row, manifest), "audit_event_invalid")
            event, delta = row.get("event"), row["seq"] > values["baseline"]["seq_high_water"]
            after = row["observed_at_us"] >= actual if actual is not None else confirmed and row["seq"] > seq
            if event == "n1_mode_confirmed":
                require(row["seq"] == seq, "confirmation_repeated")
            elif event == "n1_turn_closed":
                turn = physical.get(canonical(row.get("physical_observation_key")))
                require(turn is not None and row.get("logical_turn_key") == turn["logical_turn_key"]
                        and type(row.get("autonomous")) is bool and row["autonomous"] == turn["autonomous"]
                        and row.get("outcome") == turn["outcome"], "native_event_mismatch")
                matched.add(canonical(turn["physical_observation_key"]))
                if delta:
                    counts["closed_events"] += 1
                    counts["aborted_events"] += int(row["outcome"] == "aborted")
            elif event == "n1_synthetic_create":
                require(row.get("phase") in {"attempt", "committed"} and text(row.get("site"))
                        and row.get("synthetic_kind") in {"watcher", "tui_direct", "rebind"}
                        and row.get("turn_source") in {"managed", "external_input", "monitor_triggered", "external_adopted"}
                        and type(row.get("rebind_origin")) is bool and uint(row.get("user_msg_id"))
                        and uint(row.get("request_owner_user_id")) and row.get("relay_owner_kind") in {
                            "none", "watcher", "standby_relay", "session_bound_relay", "unknown"}, "synthetic_origin_unknown")
                expected_kind = "rebind" if row["rebind_origin"] else "watcher" if row["request_owner_user_id"] == 0 else "tui_direct"
                require(row["synthetic_kind"] == expected_kind
                        and (row["rebind_origin"] or row["request_owner_user_id"] in {0, 1}), "synthetic_origin_unknown")
                direct = (row["turn_source"] == "external_input" and not row["rebind_origin"] and row["relay_owner_kind"] != "unknown"
                          and (row["request_owner_user_id"] == 1 or row["request_owner_user_id"] == row["user_msg_id"] == 0
                               and row["relay_owner_kind"] == "watcher"))
                if not direct:
                    input_message = messages.get(str(row["user_msg_id"]))
                    if not input_message or input_message["author"]["id"] != str(row["request_owner_user_id"]):
                        reasons.append("synthetic_origin_unknown")
                if row["rebind_origin"] or row["relay_owner_kind"] == "unknown":
                    reasons.append("synthetic_origin_unknown")
                if delta:
                    counts["create_attempt" if row["phase"] == "attempt" else "create_committed"] += 1
                if direct and after:
                    if row["phase"] == "committed":
                        committed += 1
                        failures.append("synthetic_committed")
                    else:
                        reasons.append("synthetic_attempt_present")
                elif not after and row["seq"] >= seq:
                    reasons.append("effect_confirmation_overlap")
            elif event == "n1_placeholder_lifecycle":
                op = row.get("op_id")
                require(isinstance(op, list) and len(op) == 2 and op[0] == manifest["runtime"]["boot_id"]
                        and uint(op[1]) and op[1] > 0 and row.get("origin") in {"tui_direct_synthetic", "discord_active", "discord_queued", "discord_race"}
                        and row.get("operation") in {"post_placeholder", "patch_placeholder"}
                        and row.get("phase") in {"attempt", "succeeded", "failed_or_uncertain"}, "placeholder_record_invalid")
                reference, target, input_id = row.get("reference"), row.get("target_message_id"), row.get("input_message_id")
                require((reference is None or isinstance(reference, list) and len(reference) == 2 and all(uint(n) and n > 0 for n in reference))
                        and (target is None or uint(target) and target > 0) and (input_id is None or uint(input_id) and input_id > 0)
                        and ((row["operation"] == "post_placeholder" and target is None)
                             or (row["operation"] == "patch_placeholder" and target is not None)), "placeholder_record_invalid")
                identity = (op[0], op[1])
                fields = ("origin", "operation", "reference", "target_message_id", "input_message_id")
                if row["phase"] == "attempt":
                    require(identity not in used and row.get("message_id") is None and row.get("error_class") is None,
                            "placeholder_attempt_conflict")
                    used.add(identity)
                    attempts[identity] = row
                    if delta:
                        counts["attempt"] += 1
                else:
                    attempt = attempts.pop(identity, None)
                    require(attempt is not None and all(row.get(k) == attempt.get(k) for k in fields)
                            and row["observed_at_us"] >= attempt["observed_at_us"], "placeholder_attempt_conflict")
                    if row["phase"] == "failed_or_uncertain":
                        require(row.get("message_id") is None and text(row.get("error_class")), "placeholder_result_invalid")
                        reasons.append("placeholder_request_uncertain")
                        if delta:
                            counts["failed_or_uncertain"] += 1
                        continue
                    require(uint(row.get("message_id")) and row["message_id"] > 0 and row.get("error_class") is None,
                            "placeholder_result_invalid")
                    if delta:
                        counts["succeeded_post" if row["operation"] == "post_placeholder" else "succeeded_patch"] += 1
                    started_after = attempt["observed_at_us"] >= actual if actual is not None else confirmed and attempt["seq"] > seq
                    if row["origin"] == "tui_direct_synthetic" and started_after:
                        created += 1
                        failures.append("placeholder_created")
                    elif row["origin"] == "tui_direct_synthetic":
                        reasons.append("effect_confirmation_overlap")
                    else:
                        mid, input_id = str(row["message_id"]), str(row.get("input_message_id"))
                        human = messages.get(input_id)
                        if (mid not in messages or messages[mid]["author"]["id"] not in {str(i) for i in manifest["discord"]["bot_author_ids"]}
                                or not human or human["author"]["id"] in {str(i) for i in manifest["discord"]["bot_author_ids"]}):
                            reasons.append("placeholder_origin_unverified")
                        else:
                            explained.add(mid)
                    if row["operation"] == "patch_placeholder":
                        if str(target) not in manifest["discord"]["individual_ids"] or target != row["message_id"]:
                            reasons.append("placeholder_target_unverified")
            else:
                reasons.append("audit_event_unsupported")
        except (Unknown, KeyError, TypeError) as error:
            reasons.append(str(error) if isinstance(error, Unknown) else "audit_event_invalid")
    if attempts:
        reasons.append("placeholder_request_open")
    expected = {canonical(t["physical_observation_key"]) for t in native["observations"]
                if t["physical_observation_key"]["end_offset"] > manifest["scope"]["baseline_offset"]}
    if not expected <= matched:
        reasons.append("native_event_missing")
    for name in ("inflight", "pending", "abort"):
        inventory = capture(values, name, {"records"}, manifest)
        require(isinstance(inventory["records"], list), "residue_inventory_invalid")
        for row in inventory["records"]:
            require(isinstance(row, dict) and row.get("provider") == manifest["scope"]["provider"]
                    and row.get("channel_id") == manifest["scope"]["channel_id"]
                    and row.get("turn_source") in {"managed", "external_input", "monitor_triggered", "external_adopted"}
                    and uint(row.get("user_msg_id")) and uint(row.get("request_owner_user_id"))
                    and row.get("relay_owner_kind") in {"none", "watcher", "standby_relay", "session_bound_relay", "unknown"}
                    and type(row.get("rebind_origin")) is bool, "residue_origin_unknown")
            synthetic = row["turn_source"] == "external_input" and (row["request_owner_user_id"] == 1
                        or row["request_owner_user_id"] == row["user_msg_id"] == 0 and row["relay_owner_kind"] == "watcher")
            human = messages.get(str(row["user_msg_id"]))
            if synthetic and not row["rebind_origin"]:
                failures.append("synthetic_residue")
            elif (row["turn_source"] == "managed" and human and not row["rebind_origin"]
                  and human["author"]["id"] == str(row["request_owner_user_id"])
                  and human["author"]["id"] not in {str(i) for i in manifest["discord"]["bot_author_ids"]}):
                pass
            else:
                reasons.append("residue_origin_unknown")
    before, end = values["baseline"]["counters"], values["end_snapshot"]["counters"]
    if any(end[k] - before[k] != counts[k] for k in before if k != "unknown_outcome_events"):
        reasons.append("observation_counter_mismatch")
    if not (observation.get("sequence_complete") and observation.get("dropped_events") == 0
            and observation.get("sink_errors") == 0 and observation.get("open_placeholder_attempts") == 0):
        reasons.append("effect_observation_incomplete")
    result = dict(synthetic_committed_confirmed=committed, placeholder_created_confirmed=created,
                  synthetic_committed_exact=committed if not reasons else None,
                  placeholder_created_exact=created if not reasons else None)
    return result, explained, reasons, failures


def run(native, values, raw, manifest, observation, confirmation, history_complete, identity_complete):
    reasons, failures, stages = [], [], dict.fromkeys(("native", "delivery", "effects", "reverse", "debt"), False)
    result = dict(stages=stages, reasons=reasons, failures=failures)
    history = values.get("discord", {})
    rows = history.get("messages", []) if isinstance(history, dict) else []
    messages = {m["id"]: m for m in rows if message_valid(m, manifest["scope"]["channel_id"])} if isinstance(rows, list) else {}
    individual = history.get("individual", {}) if isinstance(history, dict) else {}
    lookup_consistent = True
    for mid, message in individual.items() if isinstance(individual, dict) else []:
        if not message_valid(message, manifest["scope"]["channel_id"]):
            continue
        if mid in messages and messages[mid] != message:
            reasons.append("discord_lookup_conflict")
            lookup_consistent = False
        messages[mid] = message
    explained = set()
    legacy = None
    try:
        legacy = capture(values, "legacy", LEGACY_FIELDS, manifest)
        require(native is not None, "native_audit_incomplete")
        require(identity_complete, "native_origin_unverified")
        selected, human_inputs = origins(native, legacy, values, manifest)
        result["completed_distinct"] = len(selected)
        stages["native"] = True
        explained.update(human_inputs)
    except (Unknown, KeyError, TypeError) as error:
        reasons.append(str(error) if isinstance(error, Unknown) else "native_origin_unverified")
    try:
        effects_result, allowed, extra, _ = effects(native or {"observations": []}, values, manifest, messages, observation, confirmation, failures)
        result["effects"] = effects_result
        explained.update(allowed)
        reasons.extend(extra)
        stages["effects"] = not extra
    except (Unknown, KeyError, TypeError) as error:
        reasons.append(str(error) if isinstance(error, Unknown) else "effect_audit_incomplete")
    if not history_complete:
        reasons.append("discord_audit_incomplete")
        return result
    try:
        require(native is not None, "native_audit_incomplete")
        require(identity_complete, "delivery_source_unverified")
        require(lookup_consistent, "discord_lookup_conflict")
        pieces, excluded = replay(values.get("o_ledger"), values.get("o_init"), manifest, raw["transcript"])
        debt = None
        try:
            debt = capture(values, "debt", DEBT_FIELDS, manifest)
        except Unknown as error:
            reasons.append(str(error))
        safe_legacy = legacy or dict(confirmed_deliveries=[], delivered_frontier=None, allowed_messages=[])
        safe_debt = debt or dict(undelivered=[], abandoned=[], baseline_messages=[], ledger_sha256=manifest["files"]["o_ledger"]["sha256"])
        delivered, historical, allowed, extra, _ = compare(native, pieces, excluded, values, manifest, safe_legacy, safe_debt, messages, failures)
        result.update(delivery=delivered, historical_debt=historical)
        explained.update(allowed)
        reasons.extend(extra)
        stages["delivery"] = legacy is not None and not any("historical" not in r and "baseline" not in r for r in extra)
        stages["debt"] = debt is not None and not any("historical" in r or "baseline" in r for r in extra)
        if debt is None:
            result["historical_debt"] = None
        require(legacy is not None, "legacy_audit_incomplete")
        require(isinstance(legacy["allowed_messages"], list), "allowed_identity_unverified")
        for allowed in legacy["allowed_messages"]:
            require(isinstance(allowed, dict) and set(allowed) == {"message_id", "kind", "unit_key"}
                    and snowflake(allowed["message_id"]) and allowed["kind"] == "tool_panel", "allowed_identity_unverified")
            unit = next((u for u in native["units"] if u["unit_key"] == allowed["unit_key"]), None)
            message = messages.get(allowed["message_id"])
            require(unit is not None and unit["excluded"] == "tool_call_panel" and message is not None
                    and message.get("content") == "" and isinstance(message.get("embeds"), list)
                    and any(e["footer"].get("text") == "native_key=" + unit["unit_key"]["native_key"]
                            for e in message["embeds"] if isinstance(e, dict) and isinstance(e.get("footer"), dict)), "allowed_identity_unverified")
            explained.add(allowed["message_id"])
        unexplained = [m["id"] for m in messages.values()
                       if m["author"]["id"] in {str(i) for i in manifest["discord"]["bot_author_ids"]} and m["id"] not in explained]
        result["unexplained_bot_messages"] = len(unexplained)
        if unexplained:
            reasons.append("discord_message_unexplained")
        stages["reverse"] = not unexplained
    except (Unknown, KeyError, TypeError) as error:
        reasons.append(str(error) if isinstance(error, Unknown) else "delivery_audit_incomplete")
    return result


def finish(result):
    complete = bool(result.get("audit")) and all(result["audit"]["stages"].values()) and not result["reasons"]
    result["evidence_complete"] = complete
    if result.get("failures"):
        result["verdict"] = "FAIL"
        return 1
    if complete:
        result["verdict"] = "PASS"
        return 0
    result["verdict"] = "[U]"
    return 2
