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
from typing import Any, Mapping

# ── Verdict states (pure judgment output, see evaluate()) ─────────────────────
STATE_OK = "ok"
STATE_LAGGING = "lagging"  # lost blocks exist, but last good delivery is recent
STATE_GAP = "gap"  # lost blocks exist AND last good delivery is old → relay down

# Independent PostgreSQL path states (#4378).  `/api/health/detail db=false` is
# the sole failure trigger; the TCP listener is only a cause discriminator.
PG_OK = "ok"
PG_TUNNEL_DOWN = "tunnel_down"
PG_UPSTREAM_DOWN = "upstream_or_half_dead"
PG_UNCLASSIFIED_DOWN = "db_down_tunnel_unknown"
PG_UNKNOWN = "unknown"
PG_STATE_KEY = "_pg_tunnel"

# Independent watcher-coverage states (#4408 phase 1).  Coverage is evaluated
# in parallel with transcript-vs-Discord gap judgment; these states must never
# suppress or replace STATE_GAP.
COVERAGE_COVERED = "covered"
COVERAGE_UNCOVERED = "uncovered"
COVERAGE_UNKNOWN = "unknown"
COVERAGE_CONFIRM_TICKS = 2

# Independent selector-sync states (#4408 phase 2, I1).  Compares the dcserver's
# asserted relay bind (B = watcher-state `bound_output_path`) against the
# watchdog's own growth-aware transcript pick (F).  Fail-closed: a missing/null
# bind is UNKNOWN and never an alarm.  Evaluated in parallel with the gap and
# coverage judgments; it must never suppress or replace either.
SELECTOR_SYNCED = "synced"
SELECTOR_DIVERGED = "diverged"
SELECTOR_UNKNOWN = "unknown"

PG_TOPOLOGY_TUNNEL = "tunnel"
PG_TOPOLOGY_DIRECT = "direct"


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
    # A direct PostgreSQL node does not expect an SSH -L listener on 15432.
    # The topology changes only the CLOSED diagnosis text; db=false remains
    # the sole PG failure signal in either topology.
    pg_topology: str = PG_TOPOLOGY_TUNNEL
    # PG must remain end-to-end unhealthy for this long before alerting.  The
    # default is >3x the supervisor's normal recovery envelope, avoiding noise
    # while launchd+ssh are doing their job.  Override only for an approved T3
    # drill; the deploy does not ship machine-local config values.
    pg_alert_after_secs: int = 300
    pg_realert_secs: int = 900
    # #4408 phase-2 (I1): a selector divergence (dcserver bound to a different
    # transcript than the one actually growing) must persist at least this long
    # before it alarms, so a legitimate post-swap rebind lag — the server still
    # briefly bound to the pre-swap transcript — is not misread as a stuck relay
    # tail. The deploy does not ship machine-local overrides.
    swap_confirm_secs: int = 300


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
        "pg_alert_after_secs",
        "pg_realert_secs",
        "swap_confirm_secs",
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
    pg_topology = raw.get("pg_topology", PG_TOPOLOGY_TUNNEL)
    if pg_topology not in (PG_TOPOLOGY_TUNNEL, PG_TOPOLOGY_DIRECT):
        raise ConfigError(
            "config field 'pg_topology' must be 'tunnel' or 'direct'"
        )
    kwargs["pg_topology"] = pg_topology
    for key in ("pg_alert_after_secs", "pg_realert_secs", "swap_confirm_secs"):
        if key in kwargs and kwargs[key] <= 0:
            raise ConfigError(f"config field {key!r} must be greater than zero")
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


@dataclass(frozen=True)
class TranscriptCandidate:
    path: Path
    size: int
    mtime: float


def transcript_candidates(dirs: list[Path]) -> list[TranscriptCandidate]:
    candidates: list[TranscriptCandidate] = []
    for d in dirs:
        try:
            paths = list(d.glob("*.jsonl"))
        except OSError:
            continue
        for path in paths:
            try:
                stat = path.stat()
            except OSError:
                continue
            candidates.append(TranscriptCandidate(path, stat.st_size, stat.st_mtime))
    return candidates


def select_watch_transcript(
    candidates: list[TranscriptCandidate], previous_sizes: Mapping[str, int]
) -> Path | None:
    """Choose a transcript by observed growth, falling back to newest mtime.

    A newly discovered candidate has no growth proof yet.  Once a prior size
    exists, any file that grew wins over a newer-but-stagnant file; mtime and
    path provide deterministic tie-breaking within the chosen pool.  The
    caller owns I/O and persistence, keeping this selector pure.
    """
    if not candidates:
        return None
    growing = [
        candidate
        for candidate in candidates
        if str(candidate.path) in previous_sizes
        and candidate.size > previous_sizes[str(candidate.path)]
    ]
    pool = growing or candidates
    return max(pool, key=lambda candidate: (candidate.mtime, str(candidate.path))).path


def newest_transcript(dirs: list[Path]) -> Path | None:
    """Backward-compatible mtime selector for callers without growth state."""
    return select_watch_transcript(transcript_candidates(dirs), {})


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


@dataclass(frozen=True)
class PgHealthVerdict:
    """End-to-end PG health plus the listener-only cause discriminator."""

    state: str
    db: bool | None
    tunnel_open: bool | None


@dataclass(frozen=True)
class CoverageVerdict:
    state: str
    reason: str
    consecutive_uncovered: int
    confirmed: bool


@dataclass(frozen=True)
class SelectorVerdict:
    state: str
    reason: str
    # Raw B != F with F growing, independent of the swap-confirm age gate.  The
    # caller persists a divergence-start timestamp keyed off this flag, then
    # applies :func:`selector_divergence_confirmed` before alarming.
    diverged: bool


@dataclass(frozen=True)
class WatcherStateProbe:
    status: int | None
    attached: bool | None = None
    desynced: bool | None = None
    # #4408 phase-2 (I1): the transcript path the dcserver asserts its relay tail
    # is bound to (`bound_output_path`). `None` means an old server without the
    # field, a JSON null, or a non-200 response — all fail-closed to no alarm.
    bound_output_path: str | None = None


def evaluate_coverage(
    expected_alive: bool | None,
    watcher_status: int | None,
    attached: bool | None,
    desynced: bool | None,
    previous_uncovered: int,
) -> CoverageVerdict:
    """Pure I2 judgment for expected tmux coverage.

    E is independently enumerated tmux liveness. A is exactly
    ``attached and not desynced`` from watcher-state. Only E && !A advances
    confirmation. Transport/schema uncertainty is unknown (never an alert),
    while an authoritative watcher-state 404 is uncovered. Two consecutive
    uncovered ticks are required.
    """

    def uncovered(reason: str) -> CoverageVerdict:
        consecutive = max(0, previous_uncovered) + 1
        return CoverageVerdict(
            COVERAGE_UNCOVERED,
            reason,
            consecutive,
            consecutive >= COVERAGE_CONFIRM_TICKS,
        )

    if expected_alive is None:
        return CoverageVerdict(COVERAGE_UNKNOWN, "tmux_enumeration_unknown", 0, False)
    if expected_alive is False:
        # The reverse invariant (watcher exists but tmux is dead) belongs to
        # the stall watchdog; do not manufacture a duplicate alert here.
        return CoverageVerdict(COVERAGE_COVERED, "tmux_not_expected", 0, False)
    if watcher_status is None:
        return CoverageVerdict(COVERAGE_UNKNOWN, "dcserver_unreachable", 0, False)
    if watcher_status == 404:
        return uncovered("watcher_state_404")
    if watcher_status != 200:
        return CoverageVerdict(
            COVERAGE_UNKNOWN, f"watcher_state_http_{watcher_status}", 0, False
        )
    if attached is True and desynced is False:
        return CoverageVerdict(COVERAGE_COVERED, "attached", 0, False)
    if attached is False:
        return uncovered("detached")
    if attached is True and desynced is True:
        return uncovered("attached_but_desynced")
    return CoverageVerdict(COVERAGE_UNKNOWN, "watcher_state_malformed", 0, False)


def evaluate_selector_sync(
    bound_output_path: str | None,
    selected_transcript: str | None,
    f_growing: bool,
) -> "SelectorVerdict":
    """Pure I1 judgment: does the dcserver's asserted relay bind match F?

    ``B`` is ``bound_output_path`` from watcher-state; ``F`` is the watchdog's
    own growth-aware transcript pick.  A missing/null ``B`` means an old server
    that does not expose the bind — fail closed to UNKNOWN, never an alarm.  When
    ``F`` is not growing there is no proof ``F`` is the live transcript, so a
    mismatch is not actionable.  A raw divergence (``diverged``) is ``B != F``
    with ``F`` growing; the time-based swap-confirm gate is applied separately by
    :func:`selector_divergence_confirmed` so the caller can persist the window.
    """
    if bound_output_path is None:
        return SelectorVerdict(SELECTOR_UNKNOWN, "bound_output_path_absent", False)
    if not selected_transcript:
        return SelectorVerdict(SELECTOR_UNKNOWN, "no_transcript", False)
    if bound_output_path == selected_transcript:
        return SelectorVerdict(SELECTOR_SYNCED, "selector_synced", False)
    if not f_growing:
        return SelectorVerdict(SELECTOR_SYNCED, "f_not_growing", False)
    return SelectorVerdict(SELECTOR_DIVERGED, "selector_diverged", True)


def selector_divergence_confirmed(
    diverged: bool, divergence_age_secs: float, swap_confirm_secs: int
) -> bool:
    """A raw selector divergence only alarms after it persists ``swap_confirm_secs``.

    During a legitimate transcript swap the server can still be bound to the
    pre-swap transcript for a moment while it rebinds; gating on the divergence
    age prevents that transient from being misread as a stuck relay tail.
    """
    return diverged and divergence_age_secs >= swap_confirm_secs


def evaluate_pg_health(db: object, tunnel_open: bool | None) -> PgHealthVerdict:
    """Classify without letting a bare TCP listener claim PG is healthy.

    `db is False` from the detailed dcserver health endpoint is the only down
    signal.  OPEN then means the listener accepted TCP but forwarding or PG is
    unhealthy (the 07-09 half-dead mode); CLOSED identifies the supervised
    local tunnel itself.  Missing/malformed health is unknown, never a PG
    alert, because the dcserver process could be unavailable for another cause.
    """
    if db is True:
        return PgHealthVerdict(PG_OK, True, tunnel_open)
    if db is not False:
        return PgHealthVerdict(PG_UNKNOWN, None, tunnel_open)
    if tunnel_open is False:
        return PgHealthVerdict(PG_TUNNEL_DOWN, False, False)
    if tunnel_open is True:
        return PgHealthVerdict(PG_UPSTREAM_DOWN, False, True)
    # The classifier being unavailable must not erase the primary db=false
    # signal; alert with an explicit unknown cause after the same persistence
    # threshold.
    return PgHealthVerdict(PG_UNCLASSIFIED_DOWN, False, None)


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
        self.dcserver_pg_alert_state = root / "logs/dcserver-pg-alert.state"

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

    def live_tmux_sessions(self) -> set[str] | None:
        """Independently enumerate sessions with at least one live pane.

        This intentionally does not consult SessionRegistry/WatcherSupervisor:
        I2 must still detect their own discovery or reconcile failures.
        ``None`` means the independent expectation probe itself is unknown.
        """
        try:
            p = subprocess.run(
                [
                    "tmux",
                    "list-panes",
                    "-a",
                    "-F",
                    "#{session_name}\t#{pane_dead}",
                ],
                capture_output=True,
                text=True,
                timeout=10,
            )
        except (OSError, subprocess.SubprocessError):
            return None
        if p.returncode != 0:
            error = (p.stderr or p.stdout).lower()
            if "no server running" in error or "failed to connect to server" in error:
                return set()
            return None
        live: set[str] = set()
        parsed = 0
        for line in p.stdout.splitlines():
            name, separator, pane_dead = line.partition("\t")
            if not separator or pane_dead not in ("0", "1"):
                continue
            parsed += 1
            if name and pane_dead == "0":
                live.add(name)
        if p.stdout.strip() and parsed == 0:
            return None
        return live

    def watcher_state(self, channel_id: str) -> WatcherStateProbe:
        """Read watcher-state without advancing any relay watermark."""
        url = (
            f"http://127.0.0.1:{self.cfg.dcserver_port}/api/channels/"
            f"{channel_id}/watcher-state"
        )
        try:
            p = subprocess.run(
                [
                    "curl",
                    "-sS",
                    "--max-time",
                    "4",
                    "-w",
                    "\n%{http_code}",
                    url,
                ],
                capture_output=True,
                text=True,
                timeout=10,
            )
        except (OSError, subprocess.SubprocessError):
            return WatcherStateProbe(None)
        if p.returncode != 0:
            return WatcherStateProbe(None)
        body, separator, status_text = p.stdout.rpartition("\n")
        if not separator:
            return WatcherStateProbe(None)
        try:
            status = int(status_text)
        except ValueError:
            return WatcherStateProbe(None)
        if status != 200:
            return WatcherStateProbe(status)
        try:
            payload = json.loads(body)
        except json.JSONDecodeError:
            return WatcherStateProbe(200)
        if not isinstance(payload, dict):
            return WatcherStateProbe(200)
        attached = payload.get("attached")
        desynced = payload.get("desynced")
        bound_output_path = payload.get("bound_output_path")
        return WatcherStateProbe(
            200,
            attached if isinstance(attached, bool) else None,
            desynced if isinstance(desynced, bool) else None,
            bound_output_path if isinstance(bound_output_path, str) else None,
        )

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
        def well_formed(m: object) -> bool:
            # A parseable entry is a dict whose `author` is a dict (or absent)
            # and whose `content` is a string (or absent). Malformed dicts
            # (`{"author": "bot"}`, non-string content) would raise
            # AttributeError/TypeError below — surfacing as a generic tick
            # error that BYPASSES the read_failures escalation, the exact
            # failure class r4/r5 closed for non-dict shapes (r6 review,
            # PR #4399).
            if not isinstance(m, dict):
                return False
            author = m.get("author")
            if author is not None and not isinstance(author, dict):
                return False
            content = m.get("content")
            return content is None or isinstance(content, str)

        ok_msgs = [m for m in msgs if well_formed(m)]
        if msgs and not ok_msgs:
            # A NON-EMPTY payload with ZERO parseable entries is schema drift,
            # not an empty channel: silently skipping them all would yield ''
            # — a "successful" read that never increments read_failures and
            # bypasses the watchdog-blind escalation (r5 review, PR #4399).
            # An empty list ([]) stays a normal empty channel.
            return None
        skipped = len(msgs) - len(ok_msgs)
        if skipped:
            # Mixed payload: partial data beats blindness, but schema drift
            # must still leave a trace.
            self.log(
                f"discord read: skipped {skipped} malformed message "
                f"entries (schema drift?)"
            )
        bot = [m for m in ok_msgs if (m.get("author") or {}).get("bot")]
        return norm(" ".join((m.get("content") or "") for m in bot))

    def pg_health(self) -> PgHealthVerdict:
        """Probe dcserver's end-to-end DB view, then classify with local TCP.

        Do not use curl `--fail`: the health endpoint can legitimately return a
        non-2xx status while still carrying the `db=false` JSON we need.
        """
        base = f"http://127.0.0.1:{self.cfg.dcserver_port}/api/health/detail"
        db: object = None
        try:
            p = subprocess.run(
                ["curl", "-sS", "--max-time", "4", base],
                capture_output=True,
                text=True,
                timeout=10,
            )
            if p.returncode == 0 and p.stdout:
                health = json.loads(p.stdout)
                if isinstance(health, dict):
                    db = health.get("db")
        except (OSError, subprocess.SubprocessError, json.JSONDecodeError):
            pass

        if db is not False:
            # db=true is authoritative end-to-end health.  A missing/malformed
            # health response is a dcserver probe failure, not evidence that
            # the PG tunnel failed, so avoid manufacturing a P1 alert.
            return evaluate_pg_health(db, None)

        tunnel_open: bool | None
        try:
            p = subprocess.run(
                ["nc", "-z", "-G", "3", "127.0.0.1", "15432"],
                capture_output=True,
                timeout=8,
            )
            tunnel_open = p.returncode == 0
        except (OSError, subprocess.SubprocessError):
            tunnel_open = None
        return evaluate_pg_health(False, tunnel_open)

    def recent_dcserver_pg_alert(self, now: float) -> bool:
        """Read #4379's successful-alert stamp for one-tick de-duplication.

        The Rust writer stores integer UNIX seconds.  Invalid/future content is
        fail-open (not recent), matching its own rate-limit semantics so a bad
        state file can never silence this independent watchdog.
        """
        try:
            sent_at = float(self.dcserver_pg_alert_state.read_text().strip())
        except (OSError, ValueError):
            return False
        elapsed = now - sent_at
        return 0 <= elapsed < self.cfg.pg_realert_secs

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
        if self.cfg.pg_topology == PG_TOPOLOGY_DIRECT:
            bits.append("pg-topology DIRECT (15432 listener not expected)")
        else:
            try:
                p = subprocess.run(
                    ["nc", "-z", "-G", "3", "127.0.0.1", "15432"],
                    capture_output=True,
                    timeout=8,
                )
                bits.append(
                    "pg-tunnel " + ("OPEN" if p.returncode == 0 else "CLOSED")
                )
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


def tick_pg_tunnel(rt: Runtime, state: dict[str, Any], now: float) -> None:
    """Independently monitor the end-to-end PG path once per watchdog tick."""
    if not rt.cfg.channels:
        return
    ch = rt.cfg.channels[0]
    pgs: dict[str, Any] = state.setdefault(PG_STATE_KEY, {})
    verdict = rt.pg_health()

    if verdict.state == PG_UNKNOWN:
        # Unknown is not recovery from an already-alerting incident, but it
        # breaks a pending "N minutes continuously db=false" interval.
        if not pgs.get("alerting"):
            for key in ("unhealthy_since", "dedup_deferred", "dcserver_alert_seen"):
                pgs.pop(key, None)
        rt.log("[pg-tunnel] health/detail db unknown — PG timer not advanced")
        return

    if verdict.state == PG_OK:
        if pgs.get("alerting"):
            previous = pgs.get("cause", "unknown")
            rt.alert(
                ch,
                "✅ **PG 경로 복구 (relay watchdog)**\n\n"
                "`/api/health/detail`에서 `db=true`를 확인했습니다. "
                f"이전 판정: `{previous}`. PG 터널 장애 알림을 해제합니다.",
                trigger_turn=False,
            )
            rt.log("[pg-tunnel] RECOVERED — db=true, alert state cleared")
        # Keep last_alert across recovery to enforce the 15-minute anti-flap
        # cooldown, but clear all incident-local state.
        for key in (
            "alerting",
            "unhealthy_since",
            "cause",
            "dedup_deferred",
            "dcserver_alert_seen",
        ):
            pgs.pop(key, None)
        return

    tunnel_closed_text = (
        "CLOSED — direct-node topology에서는 127.0.0.1:15432 리스너가 "
        "필수가 아니므로 SSH -L 장애로 단정하지 않음; direct PostgreSQL "
        "또는 upstream 경로 장애로 판정"
        if rt.cfg.pg_topology == PG_TOPOLOGY_DIRECT
        else (
            "CLOSED — 로컬 127.0.0.1:15432 리스너가 없어 "
            "SSH -L supervisor 재기동 루프 실패로 판정"
        )
    )
    cause_text = {
        PG_TUNNEL_DOWN: tunnel_closed_text,
        PG_UPSTREAM_DOWN: (
            "OPEN — 로컬 리스너는 열렸지만 db=false; "
            "half-dead SSH 포워딩 또는 upstream PostgreSQL 장애로 판정"
        ),
        PG_UNCLASSIFIED_DOWN: (
            "UNKNOWN — db=false이나 nc 원인 판별자를 실행하지 못함"
        ),
    }[verdict.state]
    cause_state = (
        "direct_postgres_down"
        if rt.cfg.pg_topology == PG_TOPOLOGY_DIRECT
        and verdict.state == PG_TUNNEL_DOWN
        else verdict.state
    )
    pgs["cause"] = cause_state
    if "unhealthy_since" not in pgs:
        pgs["unhealthy_since"] = now
    unhealthy_for = now - float(pgs["unhealthy_since"])
    if unhealthy_for < rt.cfg.pg_alert_after_secs:
        rt.log(
            f"[pg-tunnel] db=false cause={cause_state} for "
            f"{int(unhealthy_for)}s (< {rt.cfg.pg_alert_after_secs}s threshold)"
        )
        return

    last_alert = float(pgs.get("last_alert", 0))
    if now - last_alert < rt.cfg.pg_realert_secs:
        rt.log(
            f"[pg-tunnel] db=false persists cause={cause_state} "
            "(alert suppressed, cooldown)"
        )
        return

    # #4379 may just have emitted its PG-independent boot alert.  Defer only
    # the FIRST watchdog alert by exactly one tick; the next tick still sends
    # (with correlation text) so de-duplication can never turn into silence.
    if not pgs.get("alerting") and not pgs.get("dedup_deferred"):
        if rt.recent_dcserver_pg_alert(now):
            pgs["dedup_deferred"] = True
            pgs["dcserver_alert_seen"] = True
            rt.log(
                "[pg-tunnel] dcserver PG boot alert is recent — "
                "deferring watchdog alert by one tick"
            )
            return

    correlation = (
        "\n\n참고: dcserver 부트 PG 알림이 먼저 발화해 이 알림을 1 tick 보류했습니다."
        if pgs.get("dcserver_alert_seen")
        else ""
    )
    minutes = max(1, int(unhealthy_for // 60))
    rt.alert(
        ch,
        "🚨 **PG 경로 지속 장애 (relay watchdog)**\n\n"
        f"`/api/health/detail`의 `db=false`가 **{minutes}분** 지속되었습니다.\n"
        f"원인 판별: **{cause_text}**.\n\n"
        f"런타임: {rt.dcserver_snapshot()}"
        f"{correlation}",
    )
    pgs["last_alert"] = now
    pgs["alerting"] = True
    pgs.pop("dedup_deferred", None)
    pgs.pop("dcserver_alert_seen", None)
    rt.log(
        f"[pg-tunnel] ALERT db=false cause={cause_state} "
        f"duration={int(unhealthy_for)}s"
    )


def expected_tmux_session_name(ch: ChannelConfig) -> str:
    """Canonical session name encoded by the configured worktree family."""
    return f"AgentDesk-{ch.worktree_prefix}"


def tick_coverage(
    rt: Runtime,
    ch: ChannelConfig,
    chs: dict[str, Any],
    now: float,
) -> None:
    """Observe I2 only; never repair, return early from, or suppress gap checks."""
    expected_name = expected_tmux_session_name(ch)
    live_sessions = rt.live_tmux_sessions()
    expected_alive = None if live_sessions is None else expected_name in live_sessions
    probe = (
        rt.watcher_state(ch.channel_id)
        if expected_alive is True
        else WatcherStateProbe(None)
    )
    previous = chs.get("coverage_uncovered_ticks", 0)
    if not isinstance(previous, int) or isinstance(previous, bool):
        previous = 0
    verdict = evaluate_coverage(
        expected_alive,
        probe.status,
        probe.attached,
        probe.desynced,
        previous,
    )
    if verdict.consecutive_uncovered:
        chs["coverage_uncovered_ticks"] = verdict.consecutive_uncovered
    else:
        chs.pop("coverage_uncovered_ticks", None)

    cid = ch.channel_id
    if verdict.state == COVERAGE_UNKNOWN:
        rt.log(f"[{cid}] coverage unknown reason={verdict.reason} — no alert")
        return
    if verdict.state == COVERAGE_COVERED:
        if chs.pop("coverage_alerting", None):
            if verdict.reason == "tmux_not_expected":
                rt.log(
                    f"[{cid}] coverage expectation ended for {expected_name} "
                    "— tmux session no longer live"
                )
            else:
                rt.log(f"[{cid}] coverage restored for {expected_name}")
        # Keep last_coverage_alert across recovery as an anti-flap cooldown,
        # matching the independent PG monitor's persistence semantics.
        return

    if not verdict.confirmed:
        rt.log(
            f"[{cid}] coverage uncovered reason={verdict.reason} "
            f"confirm={verdict.consecutive_uncovered}/{COVERAGE_CONFIRM_TICKS}"
        )
        return
    if rt.in_deploy_window(now):
        rt.log(
            f"[{cid}] coverage violation reason={verdict.reason} suppressed — "
            f"deploy window (marker < {rt.cfg.deploy_quiet_secs}s old)"
        )
        return
    raw_last_alert = chs.get("last_coverage_alert", 0)
    last_alert = (
        float(raw_last_alert)
        if isinstance(raw_last_alert, (int, float))
        and not isinstance(raw_last_alert, bool)
        else 0.0
    )
    if now - last_alert < rt.cfg.realert_secs:
        rt.log(
            f"[{cid}] coverage violation persists reason={verdict.reason} "
            "(alert suppressed, coverage cooldown)"
        )
        return
    rt.alert(
        ch,
        "🚨 **릴레이 와쳐 커버리지 불변식 위반**\n\n"
        f"독립 tmux 열거에서 `{expected_name}` 세션의 live pane을 확인했지만 "
        f"watcher-state가 **{verdict.reason}** 상태입니다.\n"
        f"`attached=true && desynced=false`가 아닌 상태가 "
        f"**{verdict.consecutive_uncovered} tick 연속** 관측되었습니다.\n\n"
        "워치독은 read-only이며 자동 수리를 수행하지 않습니다 — 이 알람을 받은 "
        "에이전트가 조치해야 합니다.\n"
        f"런타임: {rt.dcserver_snapshot()}",
    )
    chs["last_coverage_alert"] = now
    chs["coverage_alerting"] = True
    rt.log(
        f"[{cid}] COVERAGE ALERT session={expected_name} "
        f"reason={verdict.reason} ticks={verdict.consecutive_uncovered}"
    )


def tick_selector_sync(
    rt: Runtime,
    ch: ChannelConfig,
    chs: dict[str, Any],
    selected_transcript: Path | None,
    f_growing: bool,
    now: float,
) -> None:
    """Observe I1 (selector sync) only; never repair or suppress gap/I2 checks.

    B is the dcserver's asserted relay bind (``bound_output_path`` from
    watcher-state).  F is the watchdog's own growth-aware transcript pick.  When
    the server is bound to a different transcript than the one actually growing
    and stays diverged past ``swap_confirm_secs``, the relay tail is stuck on a
    dead transcript (the #4423 selector-swap blind spot) → out-of-band alarm.
    Owns a private ``selector_*`` cooldown/window key so it cannot perturb the
    gap or coverage state machines.
    """
    cid = ch.channel_id
    if not f_growing or not selected_transcript:
        # No growing live transcript to compare against — a mismatch is not
        # actionable, and any pending divergence window is stale.  Skip the HTTP
        # probe entirely (matches tick_coverage's probe-only-when-needed shape).
        chs.pop("selector_diverged_since", None)
        return

    probe = rt.watcher_state(cid)
    bound = probe.bound_output_path if probe.status == 200 else None
    verdict = evaluate_selector_sync(bound, str(selected_transcript), f_growing)

    if not verdict.diverged:
        chs.pop("selector_diverged_since", None)
        if verdict.state == SELECTOR_UNKNOWN:
            # Fail-closed: old server without the field, JSON null, or dcserver
            # unreachable → never alarm on an unknown bind.
            rt.log(f"[{cid}] selector-sync unknown reason={verdict.reason} — no alert")
        elif chs.pop("selector_alerting", None):
            rt.log(f"[{cid}] selector-sync restored (B==F) reason={verdict.reason}")
        return

    raw_since = chs.get("selector_diverged_since")
    if isinstance(raw_since, (int, float)) and not isinstance(raw_since, bool):
        since = float(raw_since)
    else:
        since = now
    chs["selector_diverged_since"] = since
    age = now - since
    if not selector_divergence_confirmed(verdict.diverged, age, rt.cfg.swap_confirm_secs):
        rt.log(
            f"[{cid}] selector-sync diverged B={bound!r} F={selected_transcript} "
            f"age={int(age)}s (< {rt.cfg.swap_confirm_secs}s swap-confirm — not yet alarmed)"
        )
        return
    if rt.in_deploy_window(now):
        rt.log(
            f"[{cid}] selector-sync divergence suppressed — deploy window "
            f"(marker < {rt.cfg.deploy_quiet_secs}s old)"
        )
        return
    raw_last_alert = chs.get("last_selector_alert", 0)
    last_alert = (
        float(raw_last_alert)
        if isinstance(raw_last_alert, (int, float)) and not isinstance(raw_last_alert, bool)
        else 0.0
    )
    if now - last_alert < rt.cfg.realert_secs:
        rt.log(
            f"[{cid}] selector-sync divergence persists B={bound!r} "
            "(alert suppressed, selector cooldown)"
        )
        return
    rt.alert(
        ch,
        "🚨 **릴레이 셀렉터 동기화 불변식 위반 (I1)**\n\n"
        f"dcserver는 릴레이 tail을 `{bound}`에 바인딩하고 있으나, 실제로 성장 중인 "
        f"트랜스크립트는 `{selected_transcript}` 입니다.\n"
        f"이 불일치가 **{int(age)}초**(swap-confirm {rt.cfg.swap_confirm_secs}s 초과) 지속 — "
        "세션 스왑 후 릴레이 tail이 죽은 트랜스크립트에 고착된 상태입니다 (#4423 blind spot).\n\n"
        "복구 런북 (#4423):\n"
        "1. `sessions` 테이블에서 해당 채널 행의 output_path/session_id를 성장 중인 "
        "트랜스크립트로 `UPDATE`.\n"
        "2. `POST /api/inflight/rebind` 로 inflight 바인딩을 성장 중인 트랜스크립트로 재지정.\n\n"
        "워치독은 read-only이며 자동 수리를 수행하지 않습니다 — 이 알람을 받은 "
        "에이전트가 조치해야 합니다.\n"
        f"런타임: {rt.dcserver_snapshot()}",
    )
    chs["last_selector_alert"] = now
    chs["selector_alerting"] = True
    rt.log(
        f"[{cid}] SELECTOR ALERT B={bound!r} F={selected_transcript} age={int(age)}s"
    )


def tick_channel(rt: Runtime, ch: ChannelConfig, state: dict[str, Any], now: float) -> None:
    cfg = rt.cfg
    cid = ch.channel_id
    chs: dict[str, Any] = state.setdefault(cid, {})

    # I2 is intentionally parallel to gap evaluation (#4424): this helper has
    # no return value that can short-circuit the existing transcript/haystack
    # verdict path and it owns a separate alert cooldown key.
    try:
        tick_coverage(rt, ch, chs, now)
    except Exception as e:  # noqa: BLE001 — coverage must never suppress gap checks
        rt.log(f"[{cid}] coverage tick error: {type(e).__name__}: {e}")

    pattern = main_channel_project_re(ch.worktree_root, ch.worktree_prefix)
    dirs = channel_project_dirs(projects_root(), pattern)
    candidates = transcript_candidates(dirs)
    raw_previous_sizes = chs.get("transcript_sizes", {})
    previous_sizes = {
        str(path): size
        for path, size in (
            raw_previous_sizes.items()
            if isinstance(raw_previous_sizes, dict)
            else ()
        )
        if isinstance(size, int) and not isinstance(size, bool) and size >= 0
    }
    tr = select_watch_transcript(candidates, previous_sizes)
    chs["transcript_sizes"] = {
        str(candidate.path): candidate.size for candidate in candidates
    }
    selected = next((candidate for candidate in candidates if candidate.path == tr), None)
    # I1 selector sync (#4408 phase 2): compare the dcserver's asserted relay
    # bind (B) against F. Parallel to gap/coverage — its own cooldown key, wrapped
    # so it can never short-circuit or suppress the gap verdict below.
    f_growing = (
        selected is not None
        and str(selected.path) in previous_sizes
        and selected.size > previous_sizes[str(selected.path)]
    )
    try:
        tick_selector_sync(rt, ch, chs, tr, f_growing, now)
    except Exception as e:  # noqa: BLE001 — selector sync must never suppress gap checks
        rt.log(f"[{cid}] selector-sync tick error: {type(e).__name__}: {e}")
    idle = (now - selected.mtime) if selected else float("inf")
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
        try:
            tick_pg_tunnel(rt, state, now)
        except Exception as e:  # noqa: BLE001 — infra probe must not kill relay checks
            rt.log(f"[pg-tunnel] tick error: {type(e).__name__}: {e}")
        for ch in cfg.channels:
            try:
                tick_channel(rt, ch, state, now)
            except Exception as e:  # noqa: BLE001 — one channel must not kill the loop
                rt.log(f"[{ch.channel_id}] tick error: {type(e).__name__}: {e}")
        save_state_guarded(rt, state)
        time.sleep(cfg.poll_secs)


if __name__ == "__main__":
    sys.exit(main())
