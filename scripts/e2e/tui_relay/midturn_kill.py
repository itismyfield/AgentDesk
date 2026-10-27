"""Opt-in mid-turn provider SIGKILL on a tmux E2E cell: one re-verified PID, then settlement checks.

Only ``run_tui_relay.py --midturn-kill`` builds this scenario; YAML scenarios cannot carry its steps.
"""

from __future__ import annotations

import dataclasses
import datetime as dt
import json
import os
import re
import signal
import subprocess
import time
import uuid
from pathlib import Path
from typing import Any

from . import assertions

SCENARIO_ID = "MK-1"
CELLS = ("claude-tui", "codex-tui")
STEPS = ("preflight_e2e_channel", "identify_provider_process", "kill_provider_process", "await_killed_turn_settled", "assert_provider_replaced")
HOLD_SECONDS = 90
SETTLE_TIMEOUT_S = 300.0
# A settled turn may still flush pre-kill text; wait this long before calling it lost.
DELIVERY_GRACE_S = 30.0
DEATH_TIMEOUT_S = 10.0
IDENTITY_KEYS = ("session", "pane_id", "pane_pid", "binding_context", "channel_id", "pid", "ppid", "started", "args")
# In-progress status surfaces; a finished or failed turn leaves none of them as a message's current text.
LIVE_STATUS = re.compile(
    r"^(?:Processing\.\.\.|[\s\u200b\ufeff\u2060]*(?:\.\.\.|…)[\s\u200b\ufeff\u2060]*$"
    r"|(?:-# )?[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏] (?:계속 처리 중|진행 중|도구 실행 중|마지막 도구|subagent 실행 중|workflow 실행 중|응답 지연)"
    r"|-# (?:🟡 응답 지연|🔧 마지막 도구|🧵 subagent 실행 중|🧬 workflow 실행 중))")
PS_LINE = re.compile(r"^\s*(\d+)\s+(\d+)\s+(\S+)\s+(\w{3} \w{3} [ \d]\d \d\d:\d\d:\d\d \d{4})\s+(.*)$")


class Refused(assertions.AssertionError):
    """The harness declined to signal; no process was touched."""


@dataclasses.dataclass(frozen=True)
class Proc:
    pid: int
    ppid: int
    stat: str
    started: str
    args: str

    @property
    def exe(self) -> str:
        return os.path.basename(self.args.split(" ", 1)[0])


@dataclasses.dataclass(frozen=True)
class Pane:
    pane_id: str
    pid: int
    dead: bool


def add_arguments(parser) -> None:
    parser.add_argument("--midturn-kill", action="store_true",
                        help="Run only the destructive mid-turn provider SIGKILL scenario on a tmux E2E cell; "
                             "also needs --allow-destructive and AGENTDESK_E2E_ALLOW_DESTRUCTIVE=1.")


def validate_arguments(parser, args) -> None:
    if args.midturn_kill and (args.cell not in CELLS or args.filter or getattr(args, "herdr_restart_observe_only", None)):
        parser.error(f"--midturn-kill needs --cell in {CELLS} and forbids --filter and observe-only")
    if args.midturn_kill:
        # The pre-scenario force cancel tears down an idle tmux session; preflight requires idle instead.
        args.reset_before_each = False


def scenario(run_id: str) -> dict[str, Any]:
    tag = run_id[:12]
    ok, late = f"[E2E:MK:{tag}:BEFORE]", f"[E2E:MK:{tag}:AFTER]"
    following = f"[E2E:MK:{tag}:NEXT:{uuid.uuid4().hex[:8]}]"
    return {
        "id": SCENARIO_ID, "title": "턴 도중 provider SIGKILL 뒤 턴 정리·출력 보존·새 provider 후속",
        "agent_mode": "real_live", "coverage_class": "live", "cells": list(CELLS), "destructive": True,
        "report_marker_counts": [ok, late, following],
        "steps": [
            {"preflight_e2e_channel": {}},
            {"send_provider_hold_prompt": {"ok_marker": ok, "late_marker": late, "hold_seconds": HOLD_SECONDS}},
            {"wait_for_provider_hold_state": {"ok_marker": ok, "late_marker": late, "timeout_s": 240, "poll_interval_s": 1}},
            {"identify_provider_process": {}},
            {"kill_provider_process": {"ok_marker": ok}},
            {"await_killed_turn_settled": {"ok_marker": ok, "timeout_s": SETTLE_TIMEOUT_S}},
            {"send_prompt": f"Reply with exactly {following}"},
            {"wait_for_discord_text": following, "relay_author": "provider", "timeout_s": 300},
            {"assert_provider_replaced": {"marker": following}},
        ],
        "assertions": [{"provider_text_present": ok}, {"provider_text_present": following},
                       {"no_duplicate_marker": ok}, {"no_duplicate_marker": following}],
    }


def owns(step: dict[str, Any]) -> bool:
    return any(key in step for key in STEPS)


def _run(command: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(command, capture_output=True, text=True, check=False, env={**os.environ, "LC_ALL": "C"})


def _ps(pid: int) -> Proc | None:
    proc = _run(["ps", "-p", str(pid), "-o", "pid=,ppid=,stat=,lstart=,args="])
    match = PS_LINE.match(proc.stdout.strip()) if proc.returncode == 0 else None
    if not match or int(match.group(1)) != pid:
        return None
    return Proc(pid, int(match.group(2)), match.group(3), match.group(4), match.group(5))


def _children(pid: int) -> list[int]:
    return [int(line) for line in _run(["pgrep", "-P", str(pid)]).stdout.split() if line.isdigit()]


def _panes(session: str) -> list[Pane]:
    proc = _run(["tmux", "list-panes", "-s", "-t", f"={session}", "-F", "#{pane_id}|#{pane_pid}|#{pane_dead}"])
    rows = [line.split("|") for line in proc.stdout.splitlines()] if proc.returncode == 0 else []
    return [Pane(row[0], int(row[1]), row[2] == "1") for row in rows if len(row) == 3 and row[1].isdigit()]


def _tmux_server_pid() -> int | None:
    value = _run(["tmux", "display-message", "-p", "#{pid}"]).stdout.strip()
    return int(value) if value.isdigit() else None


def _kill(pid: int, sig: int) -> None:
    os.kill(pid, sig)


def _read(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8").strip()
    except OSError:
        return ""


def _e2e_session(driver, cell: str) -> tuple[str, str]:
    if cell not in CELLS:
        raise Refused(f"{cell} has no tmux provider process; mid-turn kill supports {CELLS}")
    provider, agent = driver.cell_provider(cell), driver.cell_default_agent(cell)
    session = driver.cell_session_name(cell)
    if not agent.endswith("-e2e") or session != f"AgentDesk-{provider}-{agent}":
        raise Refused(f"refusing non-E2E cell session {session!r} (agent {agent!r})")
    return provider, session


def channel_guard(driver, cell: str, channel_id: str, runtime_root: str | Path) -> dict[str, Any]:
    """Before any send: the newest runtime binding of the cell's E2E session must name this channel."""
    provider, session = _e2e_session(driver, cell)
    owner = str(Path(runtime_root).parent)
    bindings = []
    for path in sorted((Path(runtime_root) / "binding_contexts" / provider).glob("*.json")):
        try:
            binding = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        if isinstance(binding, dict) and binding.get("tmux_session") == session and binding.get("owner_runtime_root") == owner:
            bindings.append((str(binding.get("created_at") or ""), str(binding.get("channel_id")), str(path)))
    if not bindings or max(bindings)[1] != str(channel_id):
        raise Refused(f"no newest {session} binding names channel {channel_id}: {max(bindings) if bindings else None}")
    return {"session": session, "binding_context": max(bindings)[2], "channel_id": str(channel_id)}


def identify(driver, cell: str, channel_id: str, runtime_root: str | Path) -> dict[str, Any]:
    """The cell's provider PID, traced session record → binding → tmux pane → provider executable."""
    provider, session = _e2e_session(driver, cell)
    runtime = Path(runtime_root)
    owner = str(runtime.parent)
    records = [path for path in sorted((runtime / "sessions").glob(f"*-{session}.owner")) if _read(path) == owner]
    if len(records) != 1:
        raise Refused(f"expected one runtime session record for {session} owned by {owner}, found {len(records)}")
    base = str(records[0])[: -len(".owner")]
    if _read(Path(f"{base}.runtime-kind")) != f"{provider}_tui":
        raise Refused(f"runtime session {session} is not a {provider}_tui session")
    script = _read(Path(base + (".claude-tui.sh" if provider == "claude" else ".sh")))
    exports = re.findall(r"^export AGENTDESK_BINDING_CONTEXT='([^']+)'$", script, re.M)
    if len(exports) != 1:
        raise Refused(f"launch script for {session} names {len(exports)} binding contexts")
    try:
        binding = json.loads(Path(exports[0]).read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise Refused(f"binding context unreadable: {error}") from error
    expected = {"provider": provider, "tmux_session": session, "channel_id": str(channel_id), "owner_runtime_root": owner}
    actual = {key: str(binding.get(key)) for key in expected} if isinstance(binding, dict) else {}
    if actual != expected:
        raise Refused(f"session binding {actual} does not match {expected}")
    panes = _panes(session)
    if len(panes) != 1 or panes[0].dead:
        raise Refused(f"session {session} needs exactly one live pane, got {panes}")
    pane = panes[0]
    root = _ps(pane.pid)
    if root is None:
        raise Refused(f"pane process {pane.pid} is gone")
    target = root
    if root.exe != provider:
        kids = [kid for kid in map(_ps, _children(root.pid)) if kid and kid.ppid == root.pid and kid.exe == provider]
        if len(kids) != 1:
            raise Refused(f"wrapper {root.pid} ({root.exe}) has {len(kids)} {provider} provider children")
        target = kids[0]
    protected = {0, 1, os.getpid(), os.getppid(), _tmux_server_pid(), int(_read(runtime / "dcserver.pid") or 0) or None}
    if target.pid in protected:
        raise Refused(f"refusing protected pid {target.pid}")
    return {"session": session, "pane_id": pane.pane_id, "pane_pid": pane.pid, "binding_context": exports[0],
            "channel_id": str(channel_id), "pid": target.pid, "ppid": target.ppid, "started": target.started,
            "args": target.args, "wrapper_pid": root.pid if target is not root else None}


def _mailbox_state(driver, args, channel_id: str, provider: str) -> tuple[int, list[str]]:
    """(matching mailbox count, busy reasons) for the target channel, queue files included."""
    detail = driver._read_health_detail(args.base_url)
    boxes = [box for box in detail["mailboxes"] if isinstance(box, dict)
             and driver._mailbox_channel_id(box) == str(channel_id) and driver._mailbox_provider(box) == provider]
    reasons = [reason for box in boxes for reason in driver._mailbox_busy_reasons(box)]
    reasons += driver._runtime_queue_violations(runtime_root=Path(args.queue_runtime_root), provider=provider,
                                                channel_id=str(channel_id))
    return len(boxes), reasons


def _alive(pid: int, started: str) -> bool:
    proc = _ps(pid)
    return proc is not None and proc.started == started and not proc.stat.startswith("Z")


def judge(rows, *, deleted_ids, exclude_ids, mailbox_busy, health_error, marker, provider_author) -> dict[str, Any]:
    """Settled only with a readable idle mailbox and no live status left as any message's current text."""
    current = [row for row in rows if str(row.get("id") or "") not in deleted_ids]
    live = [{"id": str(row.get("id")), "content": (row.get("content") or "")[:120]} for row in current
            if str(row.get("id") or "") not in exclude_ids
            and any(LIVE_STATUS.search(line) for line in (row.get("content") or "").splitlines())]
    reasons = ([f"health unreadable: {health_error}"] if health_error else []) + list(mailbox_busy)
    reasons += [f"live status {row['id']}: {row['content']!r}" for row in live]
    delivered = any(marker in (assertions.author_relay_body(row, provider_author) or "") for row in current)
    return {"turn": "stuck" if reasons else "settled", "stuck_reasons": reasons, "live_status_messages": live,
            "pre_kill_output": "delivered" if delivered else "lost"}


def replacement(killed: dict[str, Any], current: dict[str, Any], *, body_ids, followup_id: int) -> dict[str, Any]:
    if (current["pid"], current["started"]) == (killed["pid"], killed["started"]):
        raise assertions.AssertionError(f"follow-up ran on the same provider process {killed['pid']}")
    if not any(int(mid) > followup_id for mid in body_ids):
        raise assertions.AssertionError(f"no provider body after the follow-up prompt {followup_id}: {body_ids}")
    return {"status": "pass", "killed": {k: killed[k] for k in ("pid", "started")},
            "current": {k: current[k] for k in ("pid", "started")}}


def run_step(driver, step, *, cell, channel_id, args, record, window, refresh) -> None:
    state = record.setdefault("midturn_kill", {"capabilities": dict.fromkeys(
        ("provider_killed", "turn_settled", "pre_kill_output_delivered", "next_turn_new_provider"), "not_reached")})
    if getattr(args, "midturn_kill", False) is not True:
        raise Refused("mid-turn kill steps run only under --midturn-kill")
    provider = driver.cell_provider(cell)
    caps = state["capabilities"]
    if "preflight_e2e_channel" in step:
        state["preflight"] = channel_guard(driver, cell, channel_id, args.queue_runtime_root)
        boxes, busy = _mailbox_state(driver, args, channel_id, provider)
        if busy:
            raise Refused(f"cell not idle before the scenario (mailboxes={boxes}): {busy}")
    elif "identify_provider_process" in step:
        state["baseline"] = identify(driver, cell, channel_id, args.queue_runtime_root)
    elif "kill_provider_process" in step:
        baseline = state.get("baseline")
        hold = (record.get("provider_hold_states") or [{}])[-1]
        if not baseline or hold.get("classification") != "provider_hold_observed":
            raise Refused(f"provider not observed mid-turn before kill: {hold.get('classification')}")
        boxes, busy = _mailbox_state(driver, args, channel_id, provider)
        if not boxes or not busy:
            raise Refused(f"target mailbox not busy before kill (mailboxes={boxes})")
        current = identify(driver, cell, channel_id, args.queue_runtime_root)
        changed = {key: (baseline[key], current[key]) for key in IDENTITY_KEYS if baseline[key] != current[key]}
        if changed:
            raise Refused(f"provider identity changed since preflight: {changed}")
        _kill(current["pid"], signal.SIGKILL)
        state["kill"] = {"identity": current, "signal": "SIGKILL", "mailbox_busy_before": busy,
                         "killed_at": dt.datetime.now(dt.timezone.utc).isoformat()}
        deadline = time.monotonic() + DEATH_TIMEOUT_S
        while _alive(current["pid"], current["started"]):
            if time.monotonic() >= deadline:
                caps["provider_killed"] = "fail"
                raise assertions.AssertionError(f"provider {current['pid']} survived SIGKILL")
            time.sleep(0.2)
        caps["provider_killed"] = "pass"
    elif "await_killed_turn_settled" in step:
        params = step["await_killed_turn_settled"]
        timeout = float(params.get("timeout_s", SETTLE_TIMEOUT_S))
        exclude = {str(send.get("message_id")) for send in record.get("prompt_sends") or []} | {window.setup_marker_id}
        started = time.monotonic()
        settled_at, streak = None, 0
        while True:
            try:
                boxes, busy = _mailbox_state(driver, args, channel_id, provider)
                busy, error = (busy if boxes else ["no matching mailbox"]), None
            except Exception as exc:  # noqa: BLE001 - an unreadable poll is never a settled one
                busy, error = [], f"{type(exc).__name__}: {exc}"
            refresh()
            verdict = judge(window.raw_messages, deleted_ids=window.deleted_ids, exclude_ids=exclude,
                            mailbox_busy=busy, health_error=error, marker=params["ok_marker"],
                            provider_author=assertions.provider_bot_id(provider))
            now = time.monotonic()
            # Two settled polls in a row, so one transient idle read cannot pass a stuck turn.
            streak = streak + 1 if verdict["turn"] == "settled" else 0
            settled_at = (settled_at or now) if streak else None
            if streak >= 2 and (verdict["pre_kill_output"] == "delivered" or now - settled_at >= DELIVERY_GRACE_S):
                break
            if now - started >= timeout:
                break
            time.sleep(float(params.get("poll_interval_s", 2.0)))
        state["settlement"] = {**verdict, "elapsed_s": round(now - started, 1), "timeout_s": timeout}
        caps["turn_settled"] = "pass" if verdict["turn"] == "settled" else "fail"
        caps["pre_kill_output_delivered"] = "pass" if verdict["pre_kill_output"] == "delivered" else "fail"
        if verdict["turn"] != "settled":
            message = f"killed turn did not settle within {timeout:.0f}s: {verdict['stuck_reasons']}"
            raise (driver.HarnessEvidenceError if error and not verdict["live_status_messages"] else assertions.AssertionError)(message)
    elif "assert_provider_replaced" in step:
        marker = step["assert_provider_replaced"]["marker"]
        killed = state["kill"]["identity"]
        caps["next_turn_new_provider"] = "fail"
        if _alive(killed["pid"], killed["started"]):
            raise assertions.AssertionError(f"killed provider {killed['pid']} is alive again")
        sends = [send for send in record.get("prompt_sends") or [] if send.get("step") == "send_prompt"]
        refresh()
        author = assertions.provider_bot_id(provider)
        bodies = [row["id"] for row in window.raw_messages if str(row.get("id")) not in window.deleted_ids
                  and marker in (assertions.author_relay_body(row, author) or "")]
        state["replacement"] = replacement(killed, identify(driver, cell, channel_id, args.queue_runtime_root),
                                           body_ids=bodies, followup_id=int(sends[-1]["message_id"]))
        caps["next_turn_new_provider"] = "pass"
