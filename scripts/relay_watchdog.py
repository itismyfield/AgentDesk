#!/usr/bin/env python3
"""Out-of-band Discord relay gap watchdog (#4381).

Why out-of-band: the in-band relay audit runs INSIDE the agent whose relay is
being watched, so when the relay dies its findings cannot reach the user either.
On 2026-07-09 that produced 2h07m of silence, 1h34m of it AFTER the agent
announced "relay recovered" on the strength of a dcserver health check it never
cross-checked against actual delivery.

This process compares the SOURCE (the agent's own session transcript under
`~/.claude/projects/<slug>/*.jsonl`) against the RELAY (what actually landed in
Discord via `agentdesk discord read`) and alerts through paths that do NOT
traverse the turn-relay being watched:

  primary : `agentdesk send-to-agent --from system` (announce bot) — trips the
            target agent's intake_gate and TRIGGERS A TURN, so the agent is
            woken to investigate rather than only the human being notified.
  fallback: `agentdesk discord-sendmessage` — posts with the bot token directly
            and needs nothing but the token. On 2026-07-09 it was the ONLY path
            that survived the outage.

Absence is the thing being detected, so reading Discord alone can never find it.

Deployment (owned by `scripts/deploy-release.sh`):
  script : staged to   $ADK_REL/bin/relay-watchdog.py
  launchd: ~/Library/LaunchAgents/com.agentdesk.relay-watchdog.plist
           (RunAtLoad + KeepAlive; independent of dcserver — dcserver dying is
           precisely the moment this must stay alive, see #4379/#4381)
  config : $ADK_REL/config/relay-watchdog.json (machine-local, deploy-preserved;
           channel ids are OPERATOR CONFIG, never hardcoded here)

There is deliberately NO self-expiry / self-uninstall: the 07-09 prototype's
TTL+idle self-destruction nearly removed the watchdog on a FALSE idle reading
(it was tailing a dead worktree's transcript). Production lifetime is owned by
the deploy, not by the process itself.
"""

from __future__ import annotations

import sys

MIN_PYTHON = (3, 10)
if sys.version_info < MIN_PYTHON:  # pragma: no cover - trivial guard
    sys.stderr.write(
        "relay_watchdog requires Python %d.%d+ (found %s)\n"
        % (*MIN_PYTHON, sys.version.split()[0])
    )
    raise SystemExit(1)

import calendar
import json
import os
import re
import shutil
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

# ── Verdict states (pure judgment output, see evaluate()) ─────────────────────
STATE_OK = "ok"
STATE_LAGGING = "lagging"  # lost blocks exist, but last good delivery is recent
STATE_GAP = "gap"  # lost blocks exist AND last good delivery is old → relay down


def adk_root() -> Path:
    return Path(os.environ.get("AGENTDESK_ROOT_DIR", str(Path.home() / ".adk/release")))


def projects_root() -> Path:
    return Path(
        os.environ.get("CLAUDE_PROJECTS_ROOT", str(Path.home() / ".claude/projects"))
    )


# ── Config ─────────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class ChannelConfig:
    """One watched Discord channel and the worktree family that relays to it."""

    channel_id: str
    # Key for `agentdesk discord-sendmessage --key` (bot-token direct post).
    sendmessage_key: str
    # Absolute path whose Claude-project slug prefixes this channel's session
    # project dirs, e.g. "$HOME/.adk/release/worktrees".
    worktree_root: str
    # Worktree basename prefix, e.g. "claude-adk-cc". Only
    # `<prefix>-<YYYYMMDD>-<HHMMSS>` worktrees belong to this channel.
    worktree_prefix: str = "claude-adk-cc"
    # Agent id to wake via the announce bot; empty disables the turn-trigger
    # primary and alerts go straight to discord-sendmessage.
    announce_to: str = ""
    announce_channel_kind: str = "cc"


@dataclass(frozen=True)
class Config:
    channels: tuple[ChannelConfig, ...] = ()
    poll_secs: int = 120
    # A block younger than this may simply not be relayed yet: the relay flushes
    # on turn/tool boundaries and edits messages in place, so a block can sit
    # unposted for minutes during a long tool call. First live catch
    # (2026-07-09 05:30Z) was a FALSE POSITIVE at 300s.
    grace_secs: int = 600
    # ...and the relay is only declared DOWN when the LAST SUCCESSFUL delivery
    # is this old. Both conditions must hold. Calibration: the 07-09 outage ran
    # 2h07m, so 15m catches it early while a normal batching delay (<10m
    # observed) never trips it.
    gap_alert_secs: int = 900
    # Re-alert cadence once a gap is confirmed and still unresolved.
    realert_secs: int = 900
    # Transcript older than this ⇒ no live session; a stale gap is not a live
    # gap. Never alert on it.
    idle_quiet_secs: int = 2 * 3600
    # Deploys restart dcserver, so short gaps during a deploy window are
    # expected. deploy-release.sh touches the marker file when it stops the
    # release service; alerts are suppressed while the marker is fresh.
    deploy_quiet_secs: int = 900
    # A gap persisting this long gets a GitHub issue auto-filed (what the
    # 06-29 relay-gap-watch did for #3893). Requires github_repo.
    issue_after_secs: int = 1800
    github_repo: str = ""  # e.g. "owner/AgentDesk"; empty disables auto-issue
    # `discord read` failing is itself a signal (the prober is blind); alert
    # after this many CONSECUTIVE failures instead of skipping forever.
    read_fail_alert_after: int = 5
    dcserver_port: int = 8791


class ConfigError(Exception):
    pass


def parse_config(raw: dict[str, Any]) -> Config:
    channels_raw = raw.get("channels")
    if not isinstance(channels_raw, list) or not channels_raw:
        raise ConfigError("config must define a non-empty 'channels' list")
    channels: list[ChannelConfig] = []
    for i, ch in enumerate(channels_raw):
        if not isinstance(ch, dict):
            raise ConfigError(f"channels[{i}] must be an object")
        try:
            channel_id = str(ch["channel_id"])
            sendmessage_key = str(ch["sendmessage_key"])
        except KeyError as e:
            raise ConfigError(f"channels[{i}] missing required key: {e}") from e
        worktree_root = str(
            ch.get("worktree_root", str(adk_root() / "worktrees"))
        )
        channels.append(
            ChannelConfig(
                channel_id=channel_id,
                sendmessage_key=sendmessage_key,
                worktree_root=worktree_root,
                worktree_prefix=str(ch.get("worktree_prefix", "claude-adk-cc")),
                announce_to=str(ch.get("announce_to", "")),
                announce_channel_kind=str(ch.get("announce_channel_kind", "cc")),
            )
        )
    kwargs: dict[str, Any] = {}
    for key in (
        "poll_secs",
        "grace_secs",
        "gap_alert_secs",
        "realert_secs",
        "idle_quiet_secs",
        "deploy_quiet_secs",
        "issue_after_secs",
        "read_fail_alert_after",
        "dcserver_port",
    ):
        if key in raw:
            # A malformed number must surface as ConfigError, never ValueError:
            # main()'s retry loop only catches ConfigError, and anything else
            # would kill the process → KeepAlive crash-loop every ~30s until an
            # operator notices (r4 review, PR #4399).
            try:
                kwargs[key] = int(raw[key])
            except (ValueError, TypeError) as e:
                raise ConfigError(
                    f"config field {key!r} must be an integer, got {raw[key]!r}"
                ) from e
    if "github_repo" in raw:
        kwargs["github_repo"] = str(raw["github_repo"])
    return Config(channels=tuple(channels), **kwargs)


def load_config(path: Path) -> Config:
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as e:
        raise ConfigError(f"config file not found: {path}") from e
    except (OSError, json.JSONDecodeError) as e:
        raise ConfigError(f"config file unreadable/invalid JSON: {path}: {e}") from e
    if not isinstance(raw, dict):
        raise ConfigError(f"config root must be a JSON object: {path}")
    return parse_config(raw)


# ── Project-dir resolution (the 07-09 hotfix, productionized) ─────────────────


def project_slug(path: str) -> str:
    """Claude Code project-dir slug for an absolute path: `/` and `.` → `-`.

    e.g. /Users/me/.adk/release/worktrees → -Users-me--adk-release-worktrees
    """
    return re.sub(r"[/.]", "-", path)


def main_channel_project_re(worktree_root: str, worktree_prefix: str) -> re.Pattern[str]:
    """Regex matching ONLY this channel's main-session project dirs.

    Two hard-won invariants (2026-07-09 incident, #4381):

    1. NEVER pin a project dir (or session UUID). The worktree changes every
       session family; a watchdog tailing a dead worktree's transcript reports
       `lost=0` forever while the live session goes unwatched. The prototype
       hardcoded a 06-29 dir and was blind for 5 hours. Resolve on every tick.

    2. EXCLUDE thread sessions. Thread worktrees carry an extra `-t<thread_id>-`
       segment (`<prefix>-t123…-<date>-<time>`) and relay to a DIFFERENT Discord
       channel — comparing their transcripts against this channel's messages
       would manufacture false LOST blocks. Only `<prefix>-<YYYYMMDD>-<HHMMSS>`
       matches; the `t…` segment fails the `\\d{8}` requirement by construction.
       Guarded by tests/test_relay_watchdog.py::ProjectDirMatchingTests.
    """
    prefix = project_slug(worktree_root.rstrip("/")) + "-" + worktree_prefix + "-"
    return re.compile("^" + re.escape(prefix) + r"\d{8}-\d{6}$")


def channel_project_dirs(root: Path, pattern: re.Pattern[str]) -> list[Path]:
    try:
        return [d for d in root.iterdir() if d.is_dir() and pattern.match(d.name)]
    except OSError:
        return []


def newest_transcript(dirs: list[Path]) -> Path | None:
    js: list[Path] = []
    for d in dirs:
        try:
            js.extend(d.glob("*.jsonl"))
        except OSError:
            continue
    if not js:
        return None
    return max(js, key=lambda f: f.stat().st_mtime)


# ── Transcript parsing ─────────────────────────────────────────────────────────


def parse_transcript_ts(ts: str) -> float | None:
    """Transcript timestamps are UTC ISO-8601. Use timegm, NOT
    `mktime(...) - time.timezone`: mktime interprets the tuple as LOCAL time and
    `timezone` ignores DST (`altzone` applies then), so the prototype was off by
    an hour during DST."""
    try:
        return float(calendar.timegm(time.strptime(ts[:19], "%Y-%m-%dT%H:%M:%S")))
    except (ValueError, TypeError):
        return None


def assistant_blocks_from_lines(lines) -> list[tuple[float, str]]:
    """(epoch, text) for every assistant text block in a transcript's lines."""
    out: list[tuple[float, str]] = []
    for line in lines:
        try:
            r = json.loads(line)
        except (json.JSONDecodeError, TypeError):
            continue
        if not isinstance(r, dict) or r.get("type") != "assistant":
            continue
        epoch = parse_transcript_ts(r.get("timestamp", ""))
        if epoch is None:
            continue
        for c in (r.get("message") or {}).get("content") or []:
            if isinstance(c, dict) and c.get("type") == "text":
                t = (c.get("text") or "").strip()
                if t:
                    out.append((epoch, t))
    return out


def assistant_blocks(transcript: Path) -> list[tuple[float, str]]:
    try:
        with transcript.open(encoding="utf-8") as f:
            return assistant_blocks_from_lines(f)
    except OSError:
        return []


# ── Delivery matching + judgment (pure) ────────────────────────────────────────


def norm(s: str) -> str:
    return re.sub(r"\s+", " ", s).strip()


def delivered(text: str, hay: str) -> bool:
    """3 probes (head/middle/tail); ≥1 hit counts as delivered, because the
    relay chunks long blocks across messages and edits messages in place."""
    n = norm(text)
    if len(n) < 60:
        return n in hay
    probes = [n[:60], n[len(n) // 2 : len(n) // 2 + 50], n[-60:]]
    return any(p and p in hay for p in probes)


@dataclass(frozen=True)
class Verdict:
    state: str
    blocks: int
    stale: int
    lost: int
    delivered_ts: float
    gap_secs: float


def evaluate(
    blocks: list[tuple[float, str]],
    hay: str,
    now: float,
    grace_secs: int,
    gap_alert_secs: int,
) -> Verdict:
    """Core judgment, ported verbatim from the proven 07-09 logic (#4140→#4178→
    #4181 lineage). The health watermark is the LAST SUCCESSFUL delivery, not
    `any lost`: a historic gap (already reported, already recovered) must not
    re-alert forever, and relay chunking can deliver a later block while an
    earlier one is still missing. Both conditions — lost blocks exist AND the
    watermark is older than gap_alert_secs — must hold to declare a gap.
    """
    delivered_ts = max((e for e, t in blocks if delivered(t, hay)), default=0.0)
    stale = [(e, t) for e, t in blocks if now - e > grace_secs]
    lost = [(e, t) for e, t in stale if e > delivered_ts and not delivered(t, hay)]
    gap_secs = (now - delivered_ts) if delivered_ts else float("inf")
    if lost and gap_secs > gap_alert_secs:
        state = STATE_GAP
    elif lost:
        state = STATE_LAGGING
    else:
        state = STATE_OK
    return Verdict(
        state=state,
        blocks=len(blocks),
        stale=len(stale),
        lost=len(lost),
        delivered_ts=delivered_ts,
        gap_secs=gap_secs,
    )


# ── Persistent state (survives process restarts; launchd may respawn us) ──────


def load_state(path: Path) -> dict[str, Any]:
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
        return raw if isinstance(raw, dict) else {}
    except (OSError, json.JSONDecodeError, ValueError):
        return {}


def save_state(path: Path, state: dict[str, Any]) -> None:
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(state, indent=1, sort_keys=True), encoding="utf-8")
    tmp.replace(path)


def save_state_guarded(rt: "Runtime", state: dict[str, Any]) -> None:
    """Persist state, but NEVER die on a write failure (r2 review, PR #4399).

    Same "KeepAlive would crash-loop us" invariant as the config-retry loop in
    main(): if a disk-full/unwritable-logs OSError killed the process here,
    launchd (KeepAlive, ThrottleInterval=30) would respawn it every ~30s with
    EMPTY in-memory state. The alert goes out BEFORE the save, so each respawn
    would forget last_alert and re-alert — a live gap becomes an ~2/min alert
    storm, amplified by announce-triggered agent turns; gap_since would also
    never persist, so the auto-issue threshold could never fire. Log and
    continue: the caller keeps the SAME dict across ticks, so cooldown state
    survives in memory and persistence resumes when the disk does.
    """
    try:
        save_state(rt.state_path, state)
    except OSError as e:
        rt.log(f"state save failed ({e}); continuing with in-memory state")


# ── Runtime side (subprocess/IO); kept thin so judgment stays pure ─────────────


class Runtime:
    def __init__(self, cfg: Config, root: Path) -> None:
        self.cfg = cfg
        self.root = root
        self.agentdesk = str(root / "bin/agentdesk")
        self.log_path = root / "logs/relay-watchdog.log"
        self.state_path = root / "logs/relay-watchdog.state.json"
        self.deploy_marker = root / "logs/relay-watchdog.deploy-marker"

    def log(self, msg: str) -> None:
        line = f"{time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())} {msg}\n"
        try:
            self.log_path.parent.mkdir(parents=True, exist_ok=True)
            with self.log_path.open("a", encoding="utf-8") as f:
                f.write(line)
        except OSError:
            sys.stderr.write(line)

    def in_deploy_window(self, now: float) -> bool:
        try:
            return now - self.deploy_marker.stat().st_mtime < self.cfg.deploy_quiet_secs
        except OSError:
            return False

    def discord_haystack(self, channel_id: str) -> str | None:
        try:
            p = subprocess.run(
                [self.agentdesk, "discord", "read", channel_id, "--limit", "100"],
                capture_output=True,
                text=True,
                timeout=60,
            )
            if p.returncode != 0:
                return None
            d = json.loads(p.stdout)
        except (OSError, subprocess.SubprocessError, json.JSONDecodeError):
            return None
        # Shape-validate: rc=0 with VALID but non-list/dict JSON (`null`, a bare
        # number/string) must join the read-failure path (return None) — an
        # AttributeError here would skip the read_failures escalation and leave
        # the prober silently blind (r4 review, PR #4399).
        if isinstance(d, list):
            msgs = d
        elif isinstance(d, dict):
            msgs = d.get("messages", d.get("data", []))
        else:
            return None
        if not isinstance(msgs, list):
            return None
        dict_msgs = [m for m in msgs if isinstance(m, dict)]
        if msgs and not dict_msgs:
            # A NON-EMPTY payload with ZERO parseable entries is schema drift,
            # not an empty channel: silently skipping them all would yield ''
            # — a "successful" read that never increments read_failures and
            # bypasses the watchdog-blind escalation (r5 review, PR #4399).
            # An empty list ([]) stays a normal empty channel.
            return None
        skipped = len(msgs) - len(dict_msgs)
        if skipped:
            # Mixed payload: partial data beats blindness, but schema drift
            # must still leave a trace.
            self.log(
                f"discord read: skipped {skipped} non-object message "
                f"entries (schema drift?)"
            )
        bot = [m for m in dict_msgs if (m.get("author") or {}).get("bot")]
        return norm(" ".join((m.get("content") or "") for m in bot))

    def dcserver_snapshot(self) -> str:
        bits = []
        # /api/health/detail, NOT /api/health: the public projection strips live
        # `degraded_reasons` and exposes only startup_degraded_reasons, which do
        # not drive `degraded` — reading it misattributes the cause (#4382).
        base = f"http://127.0.0.1:{self.cfg.dcserver_port}/api/health"
        for url, tag in ((base + "/detail", "health/detail"), (base, "health")):
            try:
                p = subprocess.run(
                    ["curl", "-sf", "--max-time", "4", url],
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                if p.returncode == 0 and p.stdout:
                    h = json.loads(p.stdout)
                    b = f"{tag} db={h.get('db')} degraded={h.get('degraded')}"
                    reasons = h.get("degraded_reasons")
                    if reasons:
                        b += f" reasons={','.join(str(r) for r in reasons)[:200]}"
                    bits.append(b)
                    break
            except (OSError, subprocess.SubprocessError, json.JSONDecodeError):
                continue
        else:
            bits.append("health UNREACHABLE")
        try:
            p = subprocess.run(
                ["nc", "-z", "-G", "3", "127.0.0.1", "15432"],
                capture_output=True,
                timeout=8,
            )
            bits.append("pg-tunnel " + ("OPEN" if p.returncode == 0 else "CLOSED"))
        except (OSError, subprocess.SubprocessError):
            bits.append("pg-tunnel UNKNOWN")
        try:
            p = subprocess.run(
                ["/bin/ps", "-axo", "pid=,etime=,command="],
                capture_output=True,
                text=True,
                timeout=10,
            )
            for line in p.stdout.splitlines():
                if "agentdesk" in line and "dcserver" in line and "grep" not in line:
                    pid, etime = line.split(None, 2)[:2]
                    bits.append(f"dcserver pid={pid} uptime={etime}")
                    break
            else:
                bits.append("dcserver NOT RUNNING")
        except (OSError, subprocess.SubprocessError, ValueError):
            bits.append("dcserver UNKNOWN")
        return " | ".join(bits)

    def alert(self, ch: ChannelConfig, body: str, trigger_turn: bool = True) -> None:
        """Deliver an alert OUT OF BAND (never through the watched turn relay).

        Primary: announce bot (`send-to-agent --from system`). Trips the target
        agent's intake_gate and TRIGGERS A TURN; a wedged mailbox queues it (📬)
        instead of dropping it. `--start-turn` is NOT used because it 409s on
        exactly the busy mailbox we are alerting about. `--from` must be
        `system` (LOOPBACK_ONLY; other labels are rejected by send_gate).
        `--expect-reply` MUST be `false`: `true` appends a reply contract
        targeting `--to system`, which has no Discord channel binding — an
        unfulfillable contract. expect_reply only selects that appended text;
        `false` still wakes the agent (verified 2026-07-09).

        But send-to-agent requires a live PG pool, so when Postgres is down —
        precisely the failure that killed dcserver on 2026-07-09 — this path
        dies too. Fallback: `discord-sendmessage`, bot-token direct, proven the
        only survivor of the 07-09 outage. Never let a fancier primary silently
        swallow the alert.
        """
        if trigger_turn and ch.announce_to:
            try:
                p = subprocess.run(
                    [
                        self.agentdesk,
                        "send-to-agent",
                        "--from",
                        "system",
                        "--to",
                        ch.announce_to,
                        "--channel-kind",
                        ch.announce_channel_kind,
                        "--expect-reply",
                        "false",
                        "--message",
                        body,
                    ],
                    capture_output=True,
                    text=True,
                    timeout=60,
                )
                if p.returncode == 0:
                    self.log("alert delivered via announce bot (turn trigger)")
                    return
                self.log(
                    f"announce bot failed rc={p.returncode}: "
                    f"{(p.stderr or p.stdout)[:160]!r}; falling back"
                )
            except (OSError, subprocess.SubprocessError) as e:
                self.log(f"announce bot error: {e}; falling back")
        try:
            p = subprocess.run(
                [
                    self.agentdesk,
                    "discord-sendmessage",
                    "--channel",
                    ch.channel_id,
                    "--key",
                    ch.sendmessage_key,
                    "--message",
                    body,
                ],
                capture_output=True,
                text=True,
                timeout=45,
            )
            self.log(f"alert delivered via discord-sendmessage rc={p.returncode}")
        except (OSError, subprocess.SubprocessError) as e:
            self.log(f"discord-sendmessage error: {e}")

    def file_github_issue(self, ch: ChannelConfig, gap_min: int, lost: int) -> str:
        """Auto-file a GitHub issue for a persistent gap (06-29 relay-gap-watch
        behavior, see #3893). Best-effort: failure is logged, never fatal."""
        gh = shutil.which("gh") or "/opt/homebrew/bin/gh"
        title = (
            f"[auto][relay-watchdog] relay gap on channel {ch.channel_id}: "
            f"{lost} undelivered blocks, {gap_min}m since last delivery"
        )
        body = (
            f"Filed automatically by the out-of-band relay watchdog (#4381).\n\n"
            f"- channel: `{ch.channel_id}`\n"
            f"- undelivered assistant blocks: **{lost}**\n"
            f"- minutes since last successful delivery: **{gap_min}**\n"
            f"- runtime snapshot: {self.dcserver_snapshot()}\n\n"
            f"The watchdog compares session transcripts against delivered "
            f"Discord messages; see `scripts/relay_watchdog.py`."
        )
        try:
            p = subprocess.run(
                [
                    gh,
                    "issue",
                    "create",
                    "--repo",
                    self.cfg.github_repo,
                    "--title",
                    title,
                    "--body",
                    body,
                ],
                capture_output=True,
                text=True,
                timeout=60,
            )
            if p.returncode == 0:
                url = p.stdout.strip().splitlines()[-1] if p.stdout.strip() else ""
                self.log(f"auto-filed issue: {url}")
                return url
            self.log(
                f"gh issue create failed rc={p.returncode}: "
                f"{(p.stderr or p.stdout)[:160]!r}"
            )
        except (OSError, subprocess.SubprocessError) as e:
            self.log(f"gh issue create error: {e}")
        return ""


# ── Per-channel tick ───────────────────────────────────────────────────────────


def tick_channel(rt: Runtime, ch: ChannelConfig, state: dict[str, Any], now: float) -> None:
    cfg = rt.cfg
    cid = ch.channel_id
    chs: dict[str, Any] = state.setdefault(cid, {})

    pattern = main_channel_project_re(ch.worktree_root, ch.worktree_prefix)
    dirs = channel_project_dirs(projects_root(), pattern)
    tr = newest_transcript(dirs)
    idle = (now - tr.stat().st_mtime) if tr else float("inf")
    if idle >= cfg.idle_quiet_secs:
        # Stale transcript: any "gap" is historic, not live. Never alert.
        # (No self-uninstall here — see module docstring.)
        rt.log(f"[{cid}] idle {int(min(idle, 86400 * 365) // 60)}m — no live session, skipping")
        return

    hay = rt.discord_haystack(cid)
    if hay is None:
        # A blind prober is itself a signal: persistent read failure means we
        # cannot vouch for the relay at all. Alert after N consecutive misses.
        fails = int(chs.get("read_failures", 0)) + 1
        chs["read_failures"] = fails
        rt.log(f"[{cid}] discord read failed ({fails} consecutive); skipping tick")
        if fails >= cfg.read_fail_alert_after and now - float(
            chs.get("last_alert", 0)
        ) >= cfg.realert_secs:
            rt.alert(
                ch,
                f"🚨 **릴레이 워치독 자체 실명 감지**\n\n"
                f"`agentdesk discord read`가 **{fails}회 연속 실패** — 워치독이 "
                f"릴레이 상태를 검증할 수 없는 상태입니다 (이것 자체가 신호).\n\n"
                f"런타임: {rt.dcserver_snapshot()}",
            )
            chs["last_alert"] = now
        return
    chs["read_failures"] = 0

    blocks = assistant_blocks(tr) if tr else []
    v = evaluate(blocks, hay, now, cfg.grace_secs, cfg.gap_alert_secs)

    if v.state == STATE_GAP:
        if rt.in_deploy_window(now):
            rt.log(
                f"[{cid}] gap lost={v.lost} suppressed — deploy window "
                f"(marker < {cfg.deploy_quiet_secs}s old)"
            )
            return
        gap_min = int(v.gap_secs // 60) if v.delivered_ts else 999
        if not chs.get("gap_since"):
            chs["gap_since"] = now
        if (
            cfg.github_repo
            and not chs.get("issue_url")
            and now - float(chs["gap_since"]) >= cfg.issue_after_secs
        ):
            url = rt.file_github_issue(ch, gap_min, v.lost)
            if url:
                chs["issue_url"] = url
        if now - float(chs.get("last_alert", 0)) >= cfg.realert_secs:
            issue_line = (
                f"\n자동 등록 이슈: {chs['issue_url']}" if chs.get("issue_url") else ""
            )
            rt.alert(
                ch,
                f"🚨 **릴레이 갭 감지 (out-of-band 워치독)**\n\n"
                f"소스(세션 트랜스크립트)에는 있는데 Discord에 도착하지 않은 "
                f"assistant 블록 **{v.lost}건**.\n"
                f"마지막 정상 도달 이후 **{gap_min}분** 경과.\n\n"
                f"런타임: {rt.dcserver_snapshot()}{issue_line}\n\n"
                f"이 알림은 turn-relay 경로가 아니라 out-of-band로 직접 나갑니다 — "
                f"릴레이가 죽어도 도착합니다.",
            )
            chs["last_alert"] = now
            chs["alerting"] = True
            rt.log(f"[{cid}] ALERT lost={v.lost} gap_min={gap_min}")
        else:
            rt.log(f"[{cid}] gap persists lost={v.lost} (alert suppressed, cooldown)")
    elif v.state == STATE_LAGGING:
        rt.log(
            f"[{cid}] lagging lost={v.lost} gap={int(v.gap_secs)}s "
            f"(< {cfg.gap_alert_secs}s alert threshold — relay batching, not down)"
        )
    else:
        if chs.get("alerting"):
            # Auto-clear: tell the same audience the gap resolved, then reset.
            rt.alert(
                ch,
                f"✅ **릴레이 갭 해소 (out-of-band 워치독)**\n\n"
                f"미도달 블록 0건으로 복구 확인. "
                f"(감시 재개; 이전 알림은 무시해도 됩니다)"
                + (
                    f"\n자동 등록 이슈 확인 필요: {chs['issue_url']}"
                    if chs.get("issue_url")
                    else ""
                ),
                trigger_turn=False,
            )
            rt.log(f"[{cid}] RECOVERED — alert state cleared")
        chs.pop("alerting", None)
        chs.pop("gap_since", None)
        chs.pop("issue_url", None)
        rt.log(f"[{cid}] ok blocks={v.blocks} stale={v.stale} lost=0")


# ── Main loop ──────────────────────────────────────────────────────────────────


def config_path() -> Path:
    return Path(
        os.environ.get(
            "RELAY_WATCHDOG_CONFIG", str(adk_root() / "config/relay-watchdog.json")
        )
    )


def main() -> int:
    root = adk_root()
    cfg: Config | None = None
    rt: Runtime | None = None
    # Loaded from disk ONCE (per Runtime), then kept in memory across ticks so
    # cooldown/issue-dedup state survives even while saves fail — see
    # save_state_guarded().
    state: dict[str, Any] | None = None
    last_cfg_err = ""
    while True:
        try:
            cfg = load_config(config_path())
        except ConfigError as e:
            # KeepAlive would crash-loop us; instead poll for config to appear.
            msg = f"config error: {e} — retrying in 600s"
            if msg != last_cfg_err:
                Runtime(Config(), root).log(msg)
                last_cfg_err = msg
            time.sleep(600)
            continue
        last_cfg_err = ""
        if rt is None or rt.cfg != cfg:
            rt = Runtime(cfg, root)
            state = None
            rt.log(
                f"watchdog armed channels={[c.channel_id for c in cfg.channels]} "
                f"poll={cfg.poll_secs}s grace={cfg.grace_secs}s "
                f"gap_alert={cfg.gap_alert_secs}s"
            )
        if state is None:
            state = load_state(rt.state_path)
        now = time.time()
        for ch in cfg.channels:
            try:
                tick_channel(rt, ch, state, now)
            except Exception as e:  # noqa: BLE001 — one channel must not kill the loop
                rt.log(f"[{ch.channel_id}] tick error: {type(e).__name__}: {e}")
        save_state_guarded(rt, state)
        time.sleep(cfg.poll_secs)


if __name__ == "__main__":
    sys.exit(main())
