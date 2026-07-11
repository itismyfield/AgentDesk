#!/usr/bin/env python3
"""Restart-safe incident state for ``auto-queue-monitor.sh``.

The shell monitor owns condition detection and delivery.  This helper owns the
small durable state machine so an alert is recorded only after the HTTP send
succeeds, persistent conditions respect a cooldown, and a resolved condition
emits one recovery notification.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

if os.name != "nt":
    import fcntl


STATE_VERSION = 1
MIN_COOLDOWN_SECS = 30 * 60
CONDITION_KINDS = frozenset({"STUCK", "ANOMALY", "REVIEW_LONG"})


class StateError(ValueError):
    """The persisted monitor state does not match the fail-closed schema."""


def clamp_cooldown(value: int) -> int:
    return max(value, MIN_COOLDOWN_SECS)


def _nonempty_string(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise StateError(f"{field} must be a non-empty string")
    return value


def normalize_condition(raw: Any) -> dict[str, str]:
    if not isinstance(raw, dict):
        raise StateError("condition must be an object")
    condition = {
        "key": _nonempty_string(raw.get("key"), "condition.key"),
        "kind": _nonempty_string(raw.get("kind"), "condition.kind"),
        "alert": _nonempty_string(raw.get("alert"), "condition.alert"),
        "recovery": _nonempty_string(raw.get("recovery"), "condition.recovery"),
    }
    if condition["kind"] not in CONDITION_KINDS:
        raise StateError(f"unsupported condition kind: {condition['kind']}")
    return condition


def _normalize_entry(key: str, raw: Any) -> dict[str, Any]:
    if not isinstance(raw, dict):
        raise StateError(f"state entry {key!r} must be an object")
    condition = normalize_condition(raw.get("condition"))
    if condition["key"] != key:
        raise StateError(f"state entry key mismatch for {key!r}")
    last_alert_at = raw.get("last_alert_at")
    if last_alert_at is not None and (
        not isinstance(last_alert_at, int) or isinstance(last_alert_at, bool) or last_alert_at < 0
    ):
        raise StateError(f"state entry {key!r} has invalid last_alert_at")
    suppress_until = raw.get("suppress_until", 0)
    if (
        not isinstance(suppress_until, int)
        or isinstance(suppress_until, bool)
        or suppress_until < 0
    ):
        raise StateError(f"state entry {key!r} has invalid suppress_until")
    return {
        "condition": condition,
        "last_alert_at": last_alert_at,
        "suppress_until": suppress_until,
    }


def _empty_state() -> dict[str, Any]:
    return {"version": STATE_VERSION, "conditions": {}}


def load_state(path: Path) -> dict[str, Any]:
    if not path.exists():
        return _empty_state()
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise StateError(f"cannot read state: {error}") from error
    if not isinstance(raw, dict) or raw.get("version") != STATE_VERSION:
        raise StateError("state version/object is invalid")
    conditions = raw.get("conditions")
    if not isinstance(conditions, dict):
        raise StateError("state.conditions must be an object")
    normalized: dict[str, Any] = {}
    for key, entry in conditions.items():
        normalized[_nonempty_string(key, "state condition key")] = _normalize_entry(key, entry)
    return {"version": STATE_VERSION, "conditions": normalized}


def save_state(path: Path, state: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temp_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(state, handle, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temp_name, 0o600)
        os.replace(temp_name, path)
    finally:
        try:
            os.unlink(temp_name)
        except FileNotFoundError:
            pass


def _quarantine_corrupt_state(path: Path, now: int) -> None:
    if not path.exists():
        return
    quarantine = path.with_name(f"{path.name}.corrupt.{now}.{os.getpid()}")
    os.replace(path, quarantine)
    print(
        f"auto-queue monitor: quarantined malformed state at {quarantine}",
        file=sys.stderr,
    )


def _active_by_key(active: list[Any]) -> dict[str, dict[str, str]]:
    result: dict[str, dict[str, str]] = {}
    for raw in active:
        condition = normalize_condition(raw)
        if condition["key"] in result:
            raise StateError(f"duplicate active condition key: {condition['key']}")
        result[condition["key"]] = condition
    return result


def _unknown_key_set(unknown: list[Any] | None) -> set[str]:
    result: set[str] = set()
    for raw in unknown or []:
        result.add(_nonempty_string(raw, "unknown condition key"))
    return result


def plan_actions(
    state_path: Path,
    active: list[Any],
    now: int,
    cooldown_secs: int,
    unknown: list[Any] | None = None,
) -> list[dict[str, Any]]:
    cooldown_secs = clamp_cooldown(cooldown_secs)
    active_by_key = _active_by_key(active)
    unknown_keys = _unknown_key_set(unknown)
    try:
        state = load_state(state_path)
    except StateError as error:
        print(
            f"auto-queue monitor: malformed state; quarantining and re-alerting active incidents: {error}",
            file=sys.stderr,
        )
        _quarantine_corrupt_state(state_path, now)
        # A corrupt file cannot prove that any alert was delivered. Starting
        # from an empty state can duplicate a prior alert, but never consumes
        # a brand-new incident behind a fabricated cooldown.
        state = _empty_state()

    stored: dict[str, dict[str, Any]] = state["conditions"]
    actions: list[dict[str, Any]] = []
    changed = False

    # A condition seeded only to fail closed after state corruption was never
    # announced, so resolving it must be silent rather than claiming recovery.
    for key in list(stored):
        entry = stored[key]
        if key not in active_by_key and entry["last_alert_at"] is None:
            del stored[key]
            changed = True

    for key in sorted(active_by_key):
        if key in unknown_keys:
            continue
        condition = active_by_key[key]
        entry = stored.get(key)
        if entry is None:
            actions.append(
                {
                    "action": "alert",
                    "condition": condition,
                    "now": now,
                    "expected_last_alert_at": None,
                }
            )
            continue
        last_alert_at = entry["last_alert_at"]
        if last_alert_at is None:
            if now >= entry["suppress_until"]:
                actions.append(
                    {
                        "action": "alert",
                        "condition": condition,
                        "now": now,
                        "expected_last_alert_at": None,
                    }
                )
            continue
        if now - last_alert_at >= cooldown_secs:
            actions.append(
                {
                    "action": "alert",
                    "condition": condition,
                    "now": now,
                    "expected_last_alert_at": last_alert_at,
                }
            )

    for key in sorted(stored):
        if key in active_by_key:
            continue
        if key in unknown_keys:
            continue
        entry = stored[key]
        last_alert_at = entry["last_alert_at"]
        if last_alert_at is not None:
            actions.append(
                {
                    "action": "recovery",
                    "condition": entry["condition"],
                    "now": now,
                    "expected_last_alert_at": last_alert_at,
                }
            )

    if changed:
        save_state(state_path, state)
    return actions


def commit_action(state_path: Path, raw_action: Any) -> bool:
    if not isinstance(raw_action, dict):
        raise StateError("action must be an object")
    action = raw_action.get("action")
    if action not in {"alert", "recovery"}:
        raise StateError(f"unsupported action: {action!r}")
    condition = normalize_condition(raw_action.get("condition"))
    now = raw_action.get("now")
    expected = raw_action.get("expected_last_alert_at")
    if not isinstance(now, int) or isinstance(now, bool) or now < 0:
        raise StateError("action.now must be a non-negative integer")
    if expected is not None and (
        not isinstance(expected, int) or isinstance(expected, bool) or expected < 0
    ):
        raise StateError("action.expected_last_alert_at is invalid")

    try:
        state = load_state(state_path)
    except StateError:
        _quarantine_corrupt_state(state_path, now)
        state = _empty_state()
    stored: dict[str, dict[str, Any]] = state["conditions"]
    current = stored.get(condition["key"])
    current_last = current["last_alert_at"] if current is not None else None
    if current_last != expected:
        return False

    if action == "alert":
        stored[condition["key"]] = {
            "condition": condition,
            "last_alert_at": now,
            "suppress_until": 0,
        }
    else:
        if current is None:
            return False
        del stored[condition["key"]]
    save_state(state_path, state)
    return True


def _load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise StateError(f"cannot load {path}: {error}") from error


def run_locked(state_path: Path, command: list[str]) -> int:
    """Run one complete detect/send/commit cycle under an OS-released lock."""

    if os.name == "nt":
        raise StateError("run-locked is supported only for the Unix shell monitor")
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        raise StateError("run-locked requires a command")

    state_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = state_path.with_name(f"{state_path.name}.lock")
    with lock_path.open("a+", encoding="utf-8") as lock_handle:
        os.chmod(lock_path, 0o600)
        fcntl.flock(lock_handle.fileno(), fcntl.LOCK_EX)
        return subprocess.run(command, check=False).returncode


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    plan = subparsers.add_parser("plan")
    plan.add_argument("--state-file", required=True, type=Path)
    plan.add_argument("--active-file", required=True, type=Path)
    plan.add_argument("--unknown-file", type=Path)
    plan.add_argument("--now", required=True, type=int)
    plan.add_argument("--cooldown-secs", required=True, type=int)

    commit = subparsers.add_parser("commit")
    commit.add_argument("--state-file", required=True, type=Path)
    commit.add_argument("--action-file", required=True, type=Path)

    locked = subparsers.add_parser("run-locked")
    locked.add_argument("--state-file", required=True, type=Path)
    locked.add_argument("command", nargs=argparse.REMAINDER)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    try:
        if args.command == "plan":
            active = _load_json(args.active_file)
            if not isinstance(active, list):
                raise StateError("active condition file must contain a JSON array")
            unknown: Any = []
            if args.unknown_file is not None:
                unknown = _load_json(args.unknown_file)
                if not isinstance(unknown, list):
                    raise StateError("unknown condition file must contain a JSON array")
            for action in plan_actions(
                args.state_file, active, args.now, args.cooldown_secs, unknown
            ):
                print(json.dumps(action, ensure_ascii=False, sort_keys=True))
            return 0

        if args.command == "commit":
            action = _load_json(args.action_file)
            return 0 if commit_action(args.state_file, action) else 3

        return run_locked(args.state_file, args.command)
    except StateError as error:
        print(f"auto-queue monitor state error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
