"""Independent ledger replay and bidirectional delivery/debt comparison."""
from collections import defaultdict
from datetime import datetime

from . import Unknown
from .manifest import digest, snowflake, text, uint
from .policy import canonical


def require(condition, reason):
    if not condition:
        raise Unknown(reason)


def instant(value):
    require(text(value), "audit_timestamp_invalid")
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
        require(parsed.tzinfo is not None, "audit_timestamp_invalid")
        return parsed
    except ValueError:
        raise Unknown("audit_timestamp_invalid") from None


def checkpoint(manifest):
    scope = manifest["scope"]
    return dict(boot_id=manifest["runtime"]["boot_id"], channel_id=scope["channel_id"],
                provider=scope["provider"], source=scope["source"],
                seq_end=manifest["observation"]["seq_end"], closure_offset=scope["closure_offset"])


def capture(values, name, fields, manifest):
    value = values.get(name)
    require(isinstance(value, dict) and set(value) == {"schema_version", "complete", "scope", *fields}
            and type(value["schema_version"]) is int and value["schema_version"] == 1
            and value["complete"] is True and canonical(value["scope"]) == canonical(checkpoint(manifest)),
            "audit_capture_incomplete:" + name)
    return value


def key(value, scope):
    require(isinstance(value, dict) and set(value) == {"channel_id", "provider", "native_key", "kind"}
            and uint(value["channel_id"]) and value["channel_id"] == scope["channel_id"] and value["provider"] == scope["provider"]
            and text(value["native_key"]) and value["kind"] in {"body", "tool", "tool_result"}, "ledger_unit_key_invalid")
    return canonical(value)


def replay(rows, init, manifest, transcript):
    scope = manifest["scope"]
    require(isinstance(init, dict) and set(init) == {"channel", "sources", "initial_anchor", "build_digest", "at"}
            and uint(init["channel"]) and init["channel"] == scope["channel_id"] and uint(init["initial_anchor"]) and init["initial_anchor"] > 0
            and text(init["build_digest"])
            and instant(init["at"]) <= instant(scope["start_at"]), "o_init_unverified")
    require(isinstance(init["sources"], list) and len(init["sources"]) == 1, "o_init_source_unsupported")
    source = init["sources"][0]
    require(isinstance(source, dict) and set(source) == {"source_id", "delivery_start", "prefix_hash"}
            and canonical(source["source_id"]) == canonical(scope["source"]) and uint(source["delivery_start"])
            and source["delivery_start"] <= scope["baseline_offset"]
            and source["prefix_hash"] == digest(transcript[:source["delivery_start"]]), "o_init_source_unverified")
    require(isinstance(rows, list), "o_ledger_incomplete")
    pieces, excluded, anchor, serial, opened, previous_time = [], {}, init["initial_anchor"], 0, None, instant(init["at"])
    for row in rows:
        require(isinstance(row, dict) and set(row) == {"at", "entry"} and isinstance(row["entry"], dict), "o_ledger_invalid")
        at, entry = instant(row["at"]), row["entry"]
        require(previous_time <= at <= instant(scope["end_at"]), "o_ledger_time_unknown")
        previous_time = at
        kind = entry.get("type")
        if kind == "prepared":
            require(set(entry) == {"type", "serial", "unit_key", "piece_index", "payload", "anchor_id", "epoch"}
                    and uint(entry["serial"]) and entry["serial"] == serial + 1 and opened is None
                    and uint(entry["piece_index"]) and isinstance(entry["payload"], str)
                    and uint(entry["anchor_id"]) and uint(entry["epoch"]) and entry["epoch"] > 0, "o_ledger_invalid")
            key(entry["unit_key"], scope)
            serial = entry["serial"]
            opened = dict(entry, at=row["at"], outcome=None, msg_id=None, anchor_valid=entry["anchor_id"] == anchor)
            pieces.append(opened)
        elif kind == "excluded":
            require(set(entry) == {"type", "unit_key", "reason"} and text(entry["reason"]), "o_ledger_invalid")
            unit = key(entry["unit_key"], scope)
            require(unit not in excluded or excluded[unit] == entry["reason"], "o_exclusion_conflict")
            excluded[unit] = entry["reason"]
        elif kind in {"posted", "rejected", "not_found", "not_sent", "ambiguous", "unresolved"}:
            extra = {"posted": "msg_id", "rejected": "status", "ambiguous": "candidates", "unresolved": "reason"}.get(kind)
            require(set(entry) == {"type", "serial", *([extra] if extra else [])}
                    and opened is not None and type(entry["serial"]) is int and entry["serial"] == serial, "o_ledger_invalid")
            if kind == "posted":
                require(uint(entry["msg_id"]) and entry["msg_id"] > 0, "o_ledger_invalid")
                opened["msg_id"] = entry["msg_id"]
                opened["posted_at"] = row["at"]
                anchor = entry["msg_id"]
            elif kind == "rejected":
                require(uint(entry["status"]) and 400 <= entry["status"] <= 599, "o_ledger_invalid")
                opened["status"] = entry["status"]
            elif kind == "ambiguous":
                require(isinstance(entry["candidates"], list) and all(uint(i) and i > 0 for i in entry["candidates"]), "o_ledger_invalid")
                opened["candidates"] = entry["candidates"]
            elif kind == "unresolved":
                require(text(entry["reason"]), "o_ledger_invalid")
                opened["reason"] = entry["reason"]
            opened["outcome"] = kind
            opened = None
        else:
            raise Unknown("o_ledger_record_unsupported")
    return pieces, excluded


def compare(native, pieces, excluded, values, manifest, legacy, debt, messages, failures):
    scope, reasons, explained = manifest["scope"], [], set()
    current, historical, source_units = {}, {}, {}
    for unit in native["units"]:
        identity = key(unit["unit_key"], scope)
        source_units[identity] = unit
        old = unit["observations"][0]["range"][0] < scope["baseline_offset"]
        for piece in unit["pieces"]:
            (historical if old else current)[(identity, piece["piece_index"])] = piece["payload_hash"]
        if unit["excluded"] and excluded.get(identity) != unit["excluded"]:
            reasons.append("o_exclusion_unverified")
    seen, old_seen, sent_order = defaultdict(set), defaultdict(set), []
    historical_unverified, historical_attempts = set(), []
    if not current:
        reasons.append("native_no_publishable_units")
    for piece in pieces:
        identity = key(piece["unit_key"], scope)
        pair = (identity, piece["piece_index"])
        old = instant(piece.get("posted_at", piece["at"])) < instant(scope["start_at"])
        if piece["outcome"] != "posted":
            uncertain = piece["outcome"] in {None, "ambiguous", "unresolved"} or piece.get("status", 0) >= 500
            if uncertain:
                if old:
                    historical_attempts.append({k: piece[k] for k in ("serial", "unit_key", "piece_index", "at", "outcome",
                                                "candidates", "reason", "status") if k in piece})
                    reasons.append("historical_delivery_uncertain")
                else:
                    reasons.append("o_delivery_uncertain")
            continue
        msg_id = str(piece["msg_id"])
        message = messages.get(msg_id)
        if message is None:
            if pair in historical or old:
                historical_unverified.add(msg_id)
                continue
            failures.append("delivery_missing")
            continue
        payload_hash = digest(piece["payload"].encode())
        if message["author"]["id"] not in {str(i) for i in manifest["discord"]["bot_author_ids"]}:
            failures.append("delivery_author_mismatch")
            continue
        if not isinstance(message.get("content"), str):
            reasons.append("discord_payload_unknown")
            continue
        if digest(message["content"].encode()) != payload_hash:
            failures.append("delivery_payload_mismatch")
            continue
        explained.add(msg_id)
        if pair not in current and pair not in historical:
            failures.append("delivery_source_missing")
            if identity in source_units and not old:
                failures.append("delivery_piece_order")
        elif (current | historical)[pair] != payload_hash:
            failures.append("delivery_plan_mismatch")
        if old or pair in historical:
            if not old and old_seen[pair] and msg_id not in old_seen[pair]:
                failures.append("delivery_duplicate")
            old_seen[pair].add(msg_id)
        else:
            if pair in seen and msg_id not in seen[pair]:
                failures.append("delivery_duplicate")
            if any(msg_id in ids and other != pair for other, ids in seen.items()):
                failures.append("delivery_identity_conflict")
            seen[pair].add(msg_id)
            sent_order.append((pair, int(msg_id)))
            if not piece["anchor_valid"]:
                failures.append("delivery_anchor_order")
    expected_order = {pair: n for n, pair in enumerate(current)}
    order = [expected_order[pair] for pair, _ in sent_order if pair in expected_order]
    ids = [identity for _, identity in sent_order]
    if order != sorted(order) or ids != sorted(set(ids)):
        failures.append("delivery_piece_order")
    for pair in current:
        if not seen[pair]:
            if any(key(p["unit_key"], scope) == pair[0] and p["outcome"] in {None, "ambiguous", "unresolved"} for p in pieces):
                reasons.append("o_delivery_uncertain")
            else:
                failures.append("delivery_missing")
    receipts = legacy["confirmed_deliveries"]
    frontier = legacy["delivered_frontier"]
    require(isinstance(receipts, list) and (frontier is None or isinstance(frontier, dict)), "legacy_record_invalid")
    legacy_ids = set()
    for receipt in receipts:
        require(isinstance(receipt, dict) and set(receipt) == {"source", "delivery_channel_id", "message_id"}
                and receipt["delivery_channel_id"] == scope["channel_id"] and uint(receipt["message_id"])
                and receipt["message_id"] > 0, "legacy_record_invalid")
        src = receipt["source"]
        require(isinstance(src, dict) and set(src) == {"provider", "tmux_session_name", "turn_nonce", "range", "generation_mtime_ns",
                "offset_authority_channel_id", "delivery_channel_id"} and src["provider"] == scope["provider"]
                and src["tmux_session_name"] == manifest["binding"]["identity"]["session"] and text(src["turn_nonce"])
                and type(src["generation_mtime_ns"]) is int and src["generation_mtime_ns"] != 0
                and src["offset_authority_channel_id"] == src["delivery_channel_id"] == scope["channel_id"], "legacy_identity_unverified")
        bounds = src["range"]
        require(isinstance(bounds, list) and len(bounds) == 2 and all(uint(n) for n in bounds)
                and 0 <= bounds[0] < bounds[1] <= scope["closure_offset"], "legacy_range_invalid")
        mid = str(receipt["message_id"])
        legacy_ids.add(mid)
        msg = messages.get(mid)
        if not msg or not isinstance(msg.get("content"), str):
            reasons.append("legacy_delivery_unverified")
            continue
        matching = [u for u in source_units.values() if any(bounds[0] <= o["range"][0] and o["range"][1] <= bounds[1] for o in u["observations"])]
        hashes = {(key(u["unit_key"], scope), p["piece_index"]): p["payload_hash"] for u in matching for p in u["pieces"]}
        matches = [pair for pair, h in hashes.items() if digest(msg["content"].encode()) == h]
        if len(matches) != 1 or msg["author"]["id"] not in {str(i) for i in manifest["discord"]["bot_author_ids"]}:
            reasons.append("legacy_delivery_unverified")
            continue
        explained.add(mid)
        for pair in matches:
            if pair in current:
                if mid in seen[pair]:
                    reasons.append("legacy_shared_receipt_origin_unknown")
                else:
                    failures.append("legacy_body_effect")
            else:
                if int(mid) >= int(manifest["discord"]["lower_inclusive"]) and old_seen[pair] and mid not in old_seen[pair]:
                    failures.append("delivery_duplicate")
                old_seen[pair].add(mid)
    if frontier is not None:
        require(set(frontier) == {"range", "generation_mtime_ns", "attempts", "panel_msg_id", "panel_channel_id"}
                and isinstance(frontier["range"], list) and len(frontier["range"]) == 2
                and all(uint(n) for n in frontier["range"]) and frontier["range"][0] < frontier["range"][1]
                and type(frontier["generation_mtime_ns"]) is int and frontier["generation_mtime_ns"] != 0
                and uint(frontier["attempts"]) and (frontier["panel_msg_id"] is None or uint(frontier["panel_msg_id"])), "legacy_frontier_invalid")
        if str(frontier["panel_msg_id"]) not in legacy_ids or frontier["panel_channel_id"] != scope["channel_id"]:
            reasons.append("legacy_frontier_unverified")
    require(isinstance(debt["undelivered"], list) and isinstance(debt["abandoned"], list)
            and isinstance(debt["baseline_messages"], list)
            and debt["ledger_sha256"] == manifest["files"]["o_ledger"]["sha256"], "historical_inventory_incomplete")
    unresolved = 0
    for item in debt["undelivered"] + debt["abandoned"]:
        require(isinstance(item, dict) and set(item) == {"source", "start", "end", "reason"}
                and uint(item["start"]) and uint(item["end"]) and item["start"] <= item["end"] <= scope["baseline_offset"]
                and text(item["reason"]), "historical_record_invalid")
        matches = [pair for pair in historical if any(item["start"] <= o["range"][0] < o["range"][1] <= item["end"]
                   for o in source_units[pair[0]]["observations"])]
        if item["source"] != scope["source"] or not matches or any(not old_seen[pair] for pair in matches):
            unresolved += 1
            reasons.append("historical_debt_unverified")
    old_missing = sum(not old_seen[pair] for pair in historical)
    old_duplicates = sum(max(0, len(ids) - 1) for ids in old_seen.values())
    if old_missing or old_duplicates:
        reasons.append("historical_debt_unresolved")
    if historical_unverified:
        reasons.append("historical_message_unverified")
    for baseline in debt["baseline_messages"]:
        require(isinstance(baseline, dict) and set(baseline) == {"message_id", "payload_hash", "last_changed_seq"}
                and snowflake(baseline["message_id"]) and uint(baseline["last_changed_seq"]), "baseline_message_unverified")
        mid = baseline["message_id"]
        msg = messages.get(mid)
        if (mid not in manifest["discord"]["individual_ids"] or not msg or not isinstance(msg.get("content"), str)
                or int(mid) >= int(manifest["discord"]["lower_inclusive"])
                or baseline["last_changed_seq"] >= manifest["observation"]["confirmation_seq"]
                or baseline["payload_hash"] != digest(msg["content"].encode())):
            reasons.append("baseline_message_unverified")
        else:
            explained.add(mid)
    anchor = str(values["o_init"]["initial_anchor"])
    if anchor not in manifest["discord"]["individual_ids"] or anchor not in explained:
        reasons.append("baseline_anchor_unverified")
    delivery = dict(expected_pieces=len(current), delivered_pieces=sum(bool(seen[p]) for p in current))
    historical_result = dict(expected_pieces=len(historical), undelivered=old_missing, duplicates=old_duplicates,
                             inventory_ranges=len(debt["undelivered"]) + len(debt["abandoned"]), unresolved=unresolved + old_missing + old_duplicates)
    if historical_unverified or historical_attempts:
        historical_result.update(duplicates=None, duplicates_confirmed=old_duplicates,
                                 unverified_messages=sorted(historical_unverified, key=int), uncertain_attempts=historical_attempts)
        historical_result["unresolved"] += len(historical_unverified) + len(historical_attempts)
    return delivery, historical_result, explained, reasons, failures
