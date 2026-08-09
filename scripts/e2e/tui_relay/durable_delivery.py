"""Read-after-write validator for the E-35 durable delivery record probe."""
import json
import time
from pathlib import Path
from typing import Any, Callable


def _json_int(value: Any, minimum: int = 1) -> int | None:
    if type(value) is not int:
        return None
    return value if value >= minimum else None


def _expected_id(value: Any) -> int | None:
    if (
        not isinstance(value, str)
        or not value
        or not value.isascii()
        or not value.isdecimal()
    ):
        return None
    parsed = int(value, 10)
    return parsed if parsed >= 1 else None


def _owner_id(value: str) -> int | None:
    if not value or not value.isascii() or not value.isdecimal():
        return None
    parsed = int(value, 10)
    return parsed if parsed >= 1 else None


def _identity_string(value: Any) -> bool:
    return isinstance(value, str) and bool(value.strip())


def _range(value: Any) -> tuple[int, int] | None:
    if not isinstance(value, list) or len(value) != 2:
        return None
    start, end = (_json_int(value[0], 0), _json_int(value[1]))
    if start is None or end is None or end <= start:
        return None
    return start, end


def _matching_receipt(receipt: Any, *, provider: str, channel_id: int,
                      message_id: int, owner_id: int) -> dict[str, Any] | None:
    if not isinstance(receipt, dict) or not isinstance(receipt.get("source"), dict):
        return None
    source = receipt["source"]
    source_range = _range(source.get("range"))
    generation = _json_int(source.get("generation_mtime_ns"))
    authoritative = (
        _identity_string(source.get("provider"))
        and source.get("provider") == provider
        and _identity_string(source.get("tmux_session_name"))
        and _identity_string(source.get("turn_nonce"))
        and _json_int(source.get("offset_authority_channel_id")) == owner_id
        and _json_int(source.get("delivery_channel_id")) == channel_id
        and _json_int(receipt.get("delivery_channel_id")) == channel_id
        and _json_int(receipt.get("message_id")) == message_id
    )
    if not authoritative or source_range is None or generation is None:
        return None
    return {"range": source_range, "generation_mtime_ns": generation}


def _invalid_query() -> dict[str, Any]:
    return {
        "status": "failed",
        "reason": (
            "invalid query identity: provider must be a nonempty string and "
            "channel_id/message_id must be nonzero ASCII decimal strings"
        ),
    }


def _scan_records(runtime_root: Path, *, provider: str, channel_id: int,
                  message_id: int) -> dict[str, Any]:
    provider_dir = runtime_root / "discord_delivery_records" / provider
    try:
        paths = sorted(provider_dir.glob("*.json"))
    except OSError as error:
        return {"status": "unevaluable", "reason": f"record scan unavailable: {error}"}
    if not provider_dir.is_dir():
        return {"status": "unevaluable", "reason": f"record directory unavailable: {provider_dir}"}
    matches: list[tuple[Path, dict[str, Any], dict[str, Any]]] = []
    malformed: list[str] = []
    for path in paths:
        try:
            owner = _owner_id(path.stem)
            record = json.loads(path.read_text(encoding="utf-8"))
            if owner is None or not isinstance(record, dict):
                raise ValueError("invalid owner filename or record object")
        except (OSError, ValueError, json.JSONDecodeError) as error:
            malformed.append(f"{path.name}: {type(error).__name__}")
            continue
        receipts = record.get("confirmed_deliveries") or []
        if not isinstance(receipts, list):
            malformed.append(f"{path.name}: confirmed_deliveries is not a list")
            continue
        for receipt in receipts:
            exact = _matching_receipt(receipt, provider=provider, channel_id=channel_id,
                                      message_id=message_id, owner_id=owner)
            if exact is not None:
                matches.append((path, exact, record))

    if len(matches) != 1:
        reason = f"expected one exact receipt, found {len(matches)}"
        if malformed:
            reason += f"; malformed={malformed[:4]}"
        return {"status": "failed", "reason": reason, "exact_receipts": len(matches)}
    path, receipt, record = matches[0]
    frontier = record.get("delivered_frontier")
    frontier_range = _range(frontier.get("range")) if isinstance(frontier, dict) else None
    covered = (
        frontier_range is not None
        and _json_int(frontier.get("generation_mtime_ns")) == receipt["generation_mtime_ns"]
        and frontier_range[1] >= receipt["range"][1]
        and _json_int(frontier.get("panel_channel_id")) == channel_id
    )
    if not covered:
        return {"status": "failed", "reason": "exact receipt lacks covering frontier"}
    return {
        "status": "evaluated",
        "reason": "exact durable receipt and covering frontier observed",
        "record": str(path),
        "response_message_id": str(message_id),
    }


def scan_records(runtime_root: Path, *, provider: str, channel_id: str,
                 message_id: str) -> dict[str, Any]:
    channel, message = _expected_id(channel_id), _expected_id(message_id)
    if not _identity_string(provider) or channel is None or message is None:
        return _invalid_query()
    return _scan_records(
        runtime_root, provider=provider, channel_id=channel, message_id=message
    )


def poll_records(
    runtime_root: Path, *, provider: str, channel_id: str, message_id: str,
    timeout_s: float = 15.0,
    interval_s: float = 0.25,
    monotonic: Callable[[], float] = time.monotonic,
    sleep: Callable[[float], None] = time.sleep,
) -> dict[str, Any]:
    started = monotonic()
    deadline = started + max(timeout_s, 0.0)
    channel, message = _expected_id(channel_id), _expected_id(message_id)
    if not _identity_string(provider) or channel is None or message is None:
        result = _invalid_query()
        result["elapsed_s"] = round(monotonic() - started, 3)
        return result
    result = _scan_records(runtime_root, provider=provider, channel_id=channel,
                           message_id=message)
    while result["status"] != "evaluated" and monotonic() < deadline:
        sleep(min(interval_s, max(0.0, deadline - monotonic())))
        result = _scan_records(runtime_root, provider=provider, channel_id=channel,
                               message_id=message)
    result["elapsed_s"] = round(monotonic() - started, 3)
    return result
