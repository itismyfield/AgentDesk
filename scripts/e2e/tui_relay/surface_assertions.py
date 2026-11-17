"""Reaction-history and session-host assertions read from public surfaces.

Reactions come from the Discord message API the harness already reads; hosts come
from `tmux has-session`, `/api/health/detail` and `agentdesk herdr status`.
A missing or failed observation is unevaluable, never a pass.
"""

from __future__ import annotations

import json
import subprocess
import threading
import time
from typing import Any, Callable

from . import assertions, herdr
from .assertion_registry import ASSERTION_OPTIONS

REACTION_OPS = ("reaction_history", "busy_injection_within")
HOST_OPS = ("thread_session_started", "session_host")
OPTIONS = {op: ASSERTION_OPTIONS[op] for op in REACTION_OPS + HOST_OPS}
# Marks the reconciler owns (turn_view_reconciler.rs TURN_VIEW, QUEUE_EXIT and INJECTION sets).
LIFECYCLE = frozenset("📬📥🔄⏳✅⚠🛑🚫⌛⏏➕❓")
INJECTED = "➕"
# A busy-input 📬 transient lasted about 1 s in production, so reads stay dense until the expected mark shows.
DENSE_INTERVAL_S = 0.5
SLOW_INTERVAL_S = 5.0
MAX_OBSERVE_S = 1800.0
DEFAULT_MAX_GAP_S = 2.0
# Healthy busy injection lands in about 1 s; the Herdr hold regression kept input about 240 s.
DEFAULT_INJECTION_MAX_S = 30.0


class Unevaluable(assertions.AssertionError):
    """The surface was not observed well enough to judge."""


def owns(spec: dict[str, Any]) -> bool:
    return any(op in spec for op in OPTIONS)


def _op(spec):
    return next(op for op in OPTIONS if op in spec)


def validate(spec: dict[str, Any]) -> None:
    """Reject bad option values when the scenario loads, not after a live run."""
    if not owns(spec):
        return
    op = _op(spec)
    value = spec[op]
    if op == "thread_session_started":
        if value is not True:
            raise ValueError("thread_session_started must be true")
        return
    if not isinstance(value, dict):
        raise ValueError(f"{op} requires mapping options")
    if op == "session_host":
        if value.get("expect") not in {"configured", "tmux", "herdr"}:
            raise ValueError("session_host expect must be configured, tmux or herdr")
        return
    if type(value.get("send")) is not int or value["send"] < 1:
        raise ValueError(f"{op} send must be a 1-based send number")
    if op == "reaction_history":
        final = value.get("final")
        if not isinstance(final, list) or not final or not set(final) <= LIFECYCLE:
            raise ValueError("reaction_history final must list lifecycle marks")
        forbid = value.get("forbid", [])
        if not isinstance(forbid, list) or set(forbid) & set(final):
            raise ValueError("reaction_history forbid must be a list disjoint from final")
        _positive(value.get("max_gap_s", DEFAULT_MAX_GAP_S), "max_gap_s")
    else:
        _positive(value.get("max_seconds", DEFAULT_INJECTION_MAX_S), "max_seconds")
        if not isinstance(value.get("before_marker"), str) or not value["before_marker"].strip():
            raise ValueError("busy_injection_within needs the turn-end before_marker")


def _positive(value, name):
    if type(value) not in (int, float) or not 0 < value < MAX_OBSERVE_S:
        raise ValueError(f"{name} must be a positive number of seconds")


def reaction_names(message: dict[str, Any]) -> list[str]:
    rows = message.get("reactions") or []
    if not isinstance(rows, list):
        raise ValueError("reactions is not a list")
    names = set()
    for row in rows:
        emoji = row.get("emoji") if isinstance(row, dict) else None
        name = emoji.get("name") if isinstance(emoji, dict) else None
        if not isinstance(name, str) or not name or type(row.get("count")) is not int:
            raise ValueError(f"malformed reaction row: {row!r}")
        if row["count"] > 0:
            names.add(name.replace("️", ""))
    return sorted(names)


class ReactionWatch:
    """Reads each targeted input message's reactions on a timer until stopped."""

    # A scenario that raised mid-steps never reaches stop(); the next scenario ends its reads.
    _active: list[ReactionWatch] = []

    def __init__(self, targets, fetch, channel_id, store, *, markers=None, clock=time.monotonic):
        self.targets, self.fetch, self.channel_id, self.store, self.clock = targets, fetch, channel_id, store, clock
        self.markers = markers or {}
        self._stop = threading.Event()
        self._threads: list[threading.Thread] = []

    @classmethod
    def for_scenario(cls, scenario, client, channel_id, record, *, run_id=""):
        while cls._active:
            cls._active.pop()._stop.set()
        targets: dict[int, set[str]] = {}
        markers: dict[int, str] = {}
        for spec in scenario.get("assertions") or []:
            for op in REACTION_OPS:
                if isinstance(spec, dict) and op in spec:
                    settle = set(spec[op].get("final", [])) if op == "reaction_history" else {INJECTED}
                    targets.setdefault(spec[op]["send"], set()).update(settle)
                    if op == "busy_injection_within":
                        markers[spec[op]["send"]] = spec[op]["before_marker"].replace("{run_id}", run_id)
        if not targets:
            return None
        watch = cls(targets, client.fetch_messages, channel_id, record.setdefault("reaction_watch", {}),
                    markers=markers)
        cls._active.append(watch)
        return watch

    def on_send(self, record) -> None:
        sends = record.get("prompt_sends") or []
        if len(sends) not in self.targets:
            return
        row = self.store[str(len(sends))] = {"message_id": sends[-1].get("message_id"),
                                             "acked_at": self.clock(), "polls": []}
        if len(sends) in self.markers:
            row["turn_end_marker"] = self.markers[len(sends)]
        if row["message_id"]:
            thread = threading.Thread(target=self._observe, args=(row, self.targets[len(sends)]), daemon=True)
            self._threads.append(thread)
            thread.start()

    def stop(self) -> None:
        self._stop.set()
        for thread in self._threads:
            thread.join(timeout=60)

    def _observe(self, row, settle) -> None:
        settled = False
        while True:
            poll = self.poll(row)
            row["polls"].append(poll)
            settled = settled or bool(set(poll.get("reactions") or ()) & settle)
            if self._stop.is_set() or poll["t"] >= MAX_OBSERVE_S:
                return
            if self._stop.wait(SLOW_INTERVAL_S if settled else DENSE_INTERVAL_S):
                row["polls"].append(self.poll(row))  # The verdict reads the state after the steps.
                return

    def poll(self, row) -> dict[str, Any]:
        mid, start = str(row["message_id"]), row["acked_at"]
        marker = row.get("turn_end_marker")
        begun = self.clock()
        try:
            rows = self.fetch(self.channel_id, after_id=str(int(mid) - 1), limit=100 if marker else 1)
            match = [m for m in rows if str(m.get("id")) == mid]
            if len(match) != 1:
                raise ValueError("input message absent from the Discord read")
            reactions: dict[str, Any] = {"reactions": reaction_names(match[0])}
            if marker:
                # One read gives both marks; a full page cannot prove the marker absent.
                seen = any(marker in (assertions.relay_body(m) or "") for m in rows)
                reactions["turn_end"] = True if seen else (None if len(rows) >= 100 else False)
        except Exception as error:  # Every read failure is kept as evidence, never as an empty set.
            reactions = {"error": str(error)[:200]}
        return {"t0": round(begun - start, 3), "t": round(self.clock() - start, 3), **reactions}


def _watched(record, send):
    row = (record.get("reaction_watch") or {}).get(str(send))
    if row is None:
        raise Unevaluable(f"send {send} was not a watched Discord send")
    if not row.get("message_id"):
        raise Unevaluable(f"send {send} has no acknowledged Discord message id")
    polls = row.get("polls") or []
    if not polls or "error" in polls[-1]:
        last = polls[-1]["error"] if polls else "no read"
        raise Unevaluable(f"send {send} final reaction read failed: {last}")
    return row, polls


def _changes(polls):
    changes, last = [], None
    for poll in polls:
        state = poll.get("reactions", "error")
        if state != last:
            changes.append([poll["t"], state])
            last = state
    return {"reads": len(polls), "errors": sum("error" in p for p in polls), "changes": changes}


def judge_reactions(polls, *, final, forbid=(), max_gap_s=DEFAULT_MAX_GAP_S):
    """Final lifecycle mark is exactly one of `final`; no read ever showed `forbid`."""
    history = _changes(polls)
    for poll in polls:
        hit = sorted(set(poll.get("reactions") or ()) & set(forbid))
        if hit:
            raise assertions.AssertionError(f"forbidden reaction {hit} seen at +{poll['t']}s; history={history}")
    last = sorted(set(polls[-1]["reactions"]) & LIFECYCLE)
    if len(last) != 1 or last[0] not in final:
        raise assertions.AssertionError(f"final reaction {last} is not exactly one of {final}; history={history}")
    if forbid:
        # Absence is proven only if reads were dense from send until the final mark first showed.
        previous = 0.0
        for poll in polls:
            if "error" in poll:
                continue
            if poll["t"] - previous > max_gap_s:
                raise Unevaluable(f"reaction read gap {poll['t'] - previous:.1f}s > {max_gap_s}s before the "
                                  f"final mark; {forbid} absence unproven; history={history}")
            if set(poll["reactions"]) & set(final):
                break
            previous = poll["t0"]
    return history


def judge_injection(polls, *, max_seconds):
    """➕ (busy injection) showed within `max_seconds` of the send and before the turn-end marker.

    Order is judged inside single Discord reads: ➕ with the marker absent passes, the marker
    with ➕ absent fails, anything else is unevaluable."""
    history = _changes(polls)
    first = next((i for i, p in enumerate(polls) if INJECTED in (p.get("reactions") or ())), None)
    if first is None:
        raise assertions.AssertionError(f"{INJECTED} never shown; input was not injected; history={history}")
    # The mark appeared after the latest read that still lacked it and by the read that showed it.
    absent_until = max((p["t0"] for p in polls[:first] if "error" not in p), default=0.0)
    shown_by = polls[first]["t"]
    if absent_until >= max_seconds:
        raise assertions.AssertionError(f"injection later than {max_seconds}s (absent at +{absent_until}s); history={history}")
    if shown_by > max_seconds:
        raise Unevaluable(f"injection between +{absent_until}s and +{shown_by}s straddles {max_seconds}s; history={history}")
    ended_first = next((p for p in polls[:first] if p.get("turn_end") is True), None)
    if ended_first is not None:
        raise assertions.AssertionError(f"injected after the turn-end marker (seen at +{ended_first['t']}s without "
                                        f"{INJECTED}); history={history}")
    before_end = next((p for p in polls[first:] if INJECTED in (p.get("reactions") or ()) and p.get("turn_end") is False),
                      None)
    if before_end is None:
        raise Unevaluable(f"no read showed {INJECTED} with the turn-end marker still absent; history={history}")
    return {"injected_between_s": [absent_until, shown_by], "turn_end_absent_at_s": before_end["t0"], **history}


# tmux has-session stderr that means a clean "no such session"; any other failure is unknown.
TMUX_ABSENT = ("can't find session", "no server running")
# `agentdesk herdr status` pane words (cli/herdr.rs pane_text) that prove no running provider.
HERDR_PANE_GONE = frozenset({"missing", "provider_exited", "root_replaced", "root_shell_unrecorded"})


def surface_context(*, base_url, provider, agent, channel_id, parent_channel_id, thread, herdr_status_bin):
    suffix = f"-t{channel_id}" if thread else ""
    return {"base_url": base_url, "channel_id": str(channel_id), "parent_channel_id": str(parent_channel_id),
            "provider": provider, "tmux_session": f"AgentDesk-{provider}-{agent}{suffix}",
            "herdr_status_bin": herdr_status_bin}


def _tmux_present(proc):
    if proc.returncode == 0:
        return True
    stderr = proc.stderr or ""
    if proc.returncode == 1 and (any(text in stderr for text in TMUX_ABSENT)
                                 or ("error connecting to" in stderr and "No such file or directory" in stderr)):
        return False
    raise ValueError(f"tmux has-session failed rc={proc.returncode}: {stderr.strip()[:200]}")


def _herdr_present(rows, context):
    if any(not isinstance(r, dict) or r.get("record") == "unreadable" or "channel" not in r for r in rows):
        raise ValueError("herdr status has an unreadable row")
    target = [r for r in rows if str(r["channel"]) == context["channel_id"] and r.get("state") != "retired"]
    if not target:
        return False
    if len(target) > 1:
        raise ValueError(f"herdr status has {len(target)} rows for the target")
    row = target[0]
    if (row.get("provider") == context["provider"] and row.get("state") == "bound"
            and row.get("pane") == "provider_running" and row.get("launch_evidence") == "recorded"):
        return True
    if row.get("pane") in HERDR_PANE_GONE:
        return False
    raise ValueError(f"herdr target row unverified: state={row.get('state')} pane={row.get('pane')}")


def probe_hosts(context, read_api, run=None) -> dict[str, Any]:
    run = run or subprocess.run
    try:
        status, detail = read_api(context["base_url"], "/api/health/detail")
    except Exception as error:
        raise Unevaluable(f"health detail read failed: {error}") from error
    if status != 200 or not isinstance(detail, dict):
        raise Unevaluable(f"health detail unreadable: HTTP {status}")
    try:
        tmux = run(["tmux", "has-session", "-t", context["tmux_session"]], capture_output=True, text=True, timeout=10)
        hosts = {"tmux": _tmux_present(tmux), "herdr": False}
        if detail.get("herdr") is not None:
            proc = run([context["herdr_status_bin"], "herdr", "status"], capture_output=True, text=True,
                       check=True, timeout=30)
            rows = json.loads(proc.stdout).get("executions")
            if not isinstance(rows, list):
                raise ValueError("herdr status has no executions list")
            hosts["herdr"] = _herdr_present(rows, context)
        configured = "herdr" if herdr.hosts_channel(detail, context["parent_channel_id"]) else "tmux"
    except (OSError, subprocess.SubprocessError, ValueError, AttributeError) as error:
        raise Unevaluable(f"session host probe failed: {error}") from error
    return {"configured": configured, "hosts": hosts}


def before_steps(scenario, record, make_context, read_api, run=None) -> None:
    """Scenarios without host checks return before any context, probe or record write."""
    specs = [spec for spec in scenario.get("assertions") or [] if isinstance(spec, dict)]
    if not any(op in spec for spec in specs for op in HOST_OPS):
        return
    record["surface_context"] = context = make_context()
    if any("thread_session_started" in spec for spec in specs):
        try:
            record["session_probe_before"] = probe_hosts(context, read_api, run)
        except Unevaluable as error:
            record["session_probe_before"] = {"error": str(error)}


def after_answer(record, read_api, run=None) -> None:
    """Probe hosts once, right after the first awaited answer, before later steps can start a session."""
    if "session_probe_before" not in record or "session_probe_after_first" in record:
        return
    try:
        record["session_probe_after_first"] = probe_hosts(record["surface_context"], read_api, run)
    except Unevaluable as error:
        record["session_probe_after_first"] = {"error": str(error)}


def judge_session_started(before, after, first_send):
    if (not first_send or first_send.get("step") != "send_discord_prompt"
            or first_send.get("send_state") != "acknowledged" or not first_send.get("message_id")):
        raise Unevaluable(f"first input was not an acknowledged Discord message: {first_send}")
    if before is None or "error" in before:
        raise Unevaluable(f"no usable host probe before the first prompt: {before}")
    if any(before["hosts"].values()):
        return {"status": "not_applicable", "passed": None, "details": {"before": before},
                "reason": "thread already had a session; rerun with a fresh thread or --hard-reset-session-each"}
    if after is None or "error" in after:
        raise Unevaluable(f"no usable host probe after the first answer: {after}")
    started = [host for host, present in after["hosts"].items() if present]
    if not started:
        raise assertions.AssertionError(f"first thread message started no session: before={before} after={after}")
    return {"status": "pass", "passed": True, "details": {"before": before, "after": after}}


def judge_session_host(probe, expect):
    expected = probe["configured"] if expect == "configured" else expect
    present = sorted(host for host, up in probe["hosts"].items() if up)
    if present != [expected]:
        raise assertions.AssertionError(f"session on {present or 'no host'}, expected {expected}: {probe}")
    return {"status": "pass", "passed": True, "details": probe}


def run(spec, *, record, run_id, read_api: Callable, run_cmd=None) -> dict[str, Any]:
    validate(spec)
    op = _op(spec)
    params = spec[op]
    record = record or {}
    if op in REACTION_OPS:
        row, polls = _watched(record, params["send"])
        if op == "reaction_history":
            details = judge_reactions(polls, final=params["final"], forbid=params.get("forbid", []),
                                      max_gap_s=params.get("max_gap_s", DEFAULT_MAX_GAP_S))
        else:
            if row.get("turn_end_marker") != params["before_marker"].replace("{run_id}", run_id or ""):
                raise Unevaluable(f"send {params['send']} reads did not look for the turn-end marker")
            details = judge_injection(polls, max_seconds=params.get("max_seconds", DEFAULT_INJECTION_MAX_S))
        return {"status": "pass", "passed": True, "details": details}
    context = record.get("surface_context")
    if context is None:
        raise Unevaluable(f"{op} has no session host context for this runner")
    if op == "session_host":
        return judge_session_host(probe_hosts(context, read_api, run_cmd), params["expect"])
    return judge_session_started(record.get("session_probe_before"), record.get("session_probe_after_first"),
                                 (record.get("prompt_sends") or [None])[0])
