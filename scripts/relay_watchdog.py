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
import math
import os
import re
import shutil
import stat as stat_mode
import subprocess
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping

try:
    import fcntl
except ImportError:  # pragma: no cover - Windows fallback; regular files ignore it
    fcntl = None  # type: ignore[assignment]

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
# #4504: a bare attached_but_desynced (no corroborating delivery gap) must
# persist this long before it can alarm on its own. Wall-clock, NOT tick count,
# so it is independent of poll_secs (default 120s). ~10 min: comfortably beyond
# the load-transient false positives (seconds) this suppresses, and the
# transcript-vs-Discord gap alarm independently catches real delivery loss at
# gap_alert_secs (~15 min) regardless of this backstop.
COVERAGE_DESYNC_CONFIRM_SECS = 600
# A foreground turn must show an outbound/relay write inside this window before
# a transient watcher-state desync can be treated as covered.  Ten minutes
# matches the watchdog's calibrated delivery grace while still letting a
# genuinely stalled foreground turn resume normal two-tick escalation.
COVERAGE_ACTIVITY_FRESH_SECS = 10 * 60

# Independent selector-sync states (#4408 phase 2, I1).  Compares the dcserver's
# asserted relay bind (B = watcher-state `bound_output_path`) against the
# watchdog's own growth-aware transcript pick (F).  Fail-closed: a missing/null
# bind is UNKNOWN and never an alarm.  Evaluated in parallel with the gap and
# coverage judgments; it must never suppress or replace either.
SELECTOR_SYNCED = "synced"
SELECTOR_DIVERGED = "diverged"
SELECTOR_UNKNOWN = "unknown"

SELECTOR_PATH_PROVIDER_PROJECT = "provider_project"
SELECTOR_PATH_RUNTIME_MIRROR = "runtime_session_mirror"
SELECTOR_PATH_UNCOMPARABLE = "uncomparable"
SELECTOR_DIVERGED_TRANSCRIPT_KEY = "selector_diverged_transcript"

DELIVERED_WATERMARKS_KEY = "delivered_watermarks"
SELECTED_TRANSCRIPT_KEY = "selected_transcript"
TRANSCRIPT_SIZES_KEY = "transcript_sizes"
TRANSCRIPT_SEEN_AT_KEY = "transcript_seen_at"
TRANSCRIPT_KNOWN_AT_KEY = "transcript_known_at"
PENDING_TRANSCRIPTS_KEY = "pending_transcripts"
PENDING_TRANSCRIPT_FAILURES_KEY = "pending_transcript_failures"
PENDING_TRANSCRIPT_SINCE_KEY = "pending_transcript_since"
RETIRED_TRANSCRIPTS_KEY = "retired_transcripts"
PENDING_TRANSCRIPT_OVERFLOW_KEY = "pending_transcript_overflow"
LAST_PENDING_TRANSCRIPT_OVERFLOW_ALERT_KEY = (
    "last_pending_transcript_overflow_alert"
)
LAST_PENDING_TRANSCRIPT_RETIREMENT_ALERT_KEY = (
    "last_pending_transcript_retirement_alert"
)
GAP_TRANSCRIPT_KEY = "gap_transcript"
GAP_OWNER_TRANSCRIPTS_KEY = "gap_owner_transcripts"
RECOVERED_GAP_GUARDS_KEY = "recovered_gap_replay_guards"
MAX_TRANSCRIPT_HISTORY = 64
MAX_KNOWN_TRANSCRIPTS = 256
MAX_PENDING_TRANSCRIPTS = 32
MAX_GAP_OWNER_TRANSCRIPTS = MAX_PENDING_TRANSCRIPTS + 1
MAX_RECOVERED_GAP_GUARDS = MAX_KNOWN_TRANSCRIPTS
MAX_RETIRED_TRANSCRIPTS = 32
# Selected, pending, unresolved GAP-owner, and recovered replay-guard paths
# each retain independent delivery authority. The watermark cap fits their
# full deduplicated worst-case union. Recovered guards follow the same bounded
# path-lifecycle budget as known transcripts: a guard is reclaimed only after
# one full history TTL of definite absence. If their store is full, recovery
# stays open rather than evicting an older replay floor.
MAX_DELIVERED_WATERMARKS = (
    1
    + MAX_PENDING_TRANSCRIPTS
    + MAX_GAP_OWNER_TRANSCRIPTS
    + MAX_RECOVERED_GAP_GUARDS
)
TRANSCRIPT_HISTORY_TTL_SECS = 7 * 24 * 60 * 60
RECOVERED_GAP_GUARD_TTL_SECS = TRANSCRIPT_HISTORY_TTL_SECS

PG_TOPOLOGY_TUNNEL = "tunnel"
PG_TOPOLOGY_DIRECT = "direct"


def adk_root() -> Path:
    return Path(os.environ.get("AGENTDESK_ROOT_DIR", str(Path.home() / ".adk/release")))


def projects_root() -> Path:
    return Path(
        os.environ.get("CLAUDE_PROJECTS_ROOT", str(Path.home() / ".claude/projects"))
    )


def _lexical_absolute_path(value: str | Path) -> Path | None:
    """Normalize a path without requiring the target to exist."""
    try:
        candidate = Path(value).expanduser()
    except (TypeError, ValueError, OSError, RuntimeError):
        return None
    if not candidate.is_absolute():
        return None
    return Path(os.path.normpath(str(candidate)))


def _is_path_within(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
    except ValueError:
        return False
    return True


def classify_selector_path(value: str) -> str:
    """Classify paths by representation before comparing selector identity.

    Provider project transcripts and AgentDesk runtime-session mirrors can
    contain the same logical session while intentionally having different
    paths.  Only two provider-project paths are identity-comparable.
    """
    path = _lexical_absolute_path(value)
    provider_root = _lexical_absolute_path(projects_root())
    mirror_root = _lexical_absolute_path(adk_root() / "runtime" / "sessions")
    if path is None or provider_root is None or mirror_root is None:
        return SELECTOR_PATH_UNCOMPARABLE
    if _is_path_within(path, mirror_root):
        return SELECTOR_PATH_RUNTIME_MIRROR
    if _is_path_within(path, provider_root):
        return SELECTOR_PATH_PROVIDER_PROJECT
    return SELECTOR_PATH_UNCOMPARABLE


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
        entries = list(root.iterdir())
    except OSError:
        return []
    return [
        entry
        for entry in entries
        if _directory_without_symlink(entry) and pattern.match(entry.name)
    ]


def _directory_without_symlink(path: Path) -> bool:
    try:
        path_stat = path.stat(follow_symlinks=False)
    except (OSError, ValueError, UnicodeError):
        return False
    return stat_mode.S_ISDIR(path_stat.st_mode)


def _regular_file_stat_without_symlink(path: Path) -> os.stat_result | None:
    try:
        parent_stat = path.parent.stat(follow_symlinks=False)
        path_stat = path.stat(follow_symlinks=False)
    except (OSError, ValueError, UnicodeError):
        return None
    if not stat_mode.S_ISDIR(parent_stat.st_mode):
        return None
    if not stat_mode.S_ISREG(path_stat.st_mode):
        return None
    return path_stat


def _same_file_identity(left: os.stat_result, right: os.stat_result) -> bool:
    return (left.st_dev, left.st_ino) == (right.st_dev, right.st_ino)


def _open_regular_file_beneath_parent(
    path: Path, flags: int, trusted_root: Path
) -> int | None:
    """Open one regular file beneath a pinned, explicitly trusted root.

    A final-component ``O_NOFOLLOW`` is insufficient: after discovery an
    attacker can rename an ancestor such as the provider ``projects`` root and
    replace it with a symlink to an outside, same-shaped tree.  Pin the trusted
    root, then resolve *every* descendant directory with ``openat`` plus
    ``O_NOFOLLOW|O_DIRECTORY``.  The trusted root itself must be an absolute,
    non-symlink directory; symlinks above that explicit trust boundary retain
    their normal platform semantics (notably macOS ``/var`` -> ``/private/var``).

    Every pre-open identity is revalidated against the resulting descriptor.
    The final open is nonblocking so a raced FIFO/device cannot hang the
    watchdog before its regular-file ``fstat`` gate.  Unsupported platforms,
    malformed paths, and every race fail closed, with all intermediate file
    descriptors released.
    """
    directory_flag = getattr(os, "O_DIRECTORY", 0)
    nofollow_flag = getattr(os, "O_NOFOLLOW", 0)
    nonblock_flag = getattr(os, "O_NONBLOCK", 0)
    if not directory_flag or not nofollow_flag or not nonblock_flag:
        return None

    normalized_path = _lexical_absolute_path(path)
    normalized_root = _lexical_absolute_path(trusted_root)
    if (
        normalized_path is None
        or normalized_root is None
        or normalized_path != path
        or normalized_root != trusted_root
    ):
        return None
    try:
        relative = normalized_path.relative_to(normalized_root)
    except ValueError:
        return None
    components = relative.parts
    if not components or any(
        component in ("", ".", "..") for component in components
    ):
        return None

    directory_descriptors: list[int] = []
    descriptor = -1
    directory_flags = (
        os.O_RDONLY
        | getattr(os, "O_CLOEXEC", 0)
        | directory_flag
        | nofollow_flag
        | nonblock_flag
    )
    file_flags = (
        flags
        | getattr(os, "O_CLOEXEC", 0)
        | nofollow_flag
        | nonblock_flag
    )
    try:
        expected_root = normalized_root.stat(follow_symlinks=False)
        if not stat_mode.S_ISDIR(expected_root.st_mode):
            return None
        root_descriptor = os.open(normalized_root, directory_flags)
        directory_descriptors.append(root_descriptor)
        try:
            opened_root = os.fstat(root_descriptor)
        except OSError:
            return None
        if not stat_mode.S_ISDIR(opened_root.st_mode) or not _same_file_identity(
            expected_root, opened_root
        ):
            return None

        for component in components[:-1]:
            parent_descriptor = directory_descriptors[-1]
            expected_directory = os.stat(
                component, dir_fd=parent_descriptor, follow_symlinks=False
            )
            if not stat_mode.S_ISDIR(expected_directory.st_mode):
                return None
            child_descriptor = os.open(
                component, directory_flags, dir_fd=parent_descriptor
            )
            directory_descriptors.append(child_descriptor)
            try:
                opened_directory = os.fstat(child_descriptor)
            except OSError:
                return None
            if not stat_mode.S_ISDIR(
                opened_directory.st_mode
            ) or not _same_file_identity(expected_directory, opened_directory):
                return None

        parent_descriptor = directory_descriptors[-1]
        filename = components[-1]
        expected_file = os.stat(
            filename, dir_fd=parent_descriptor, follow_symlinks=False
        )
        if not stat_mode.S_ISREG(expected_file.st_mode):
            return None
        descriptor = os.open(filename, file_flags, dir_fd=parent_descriptor)
        opened_file = os.fstat(descriptor)
        if not stat_mode.S_ISREG(opened_file.st_mode) or not _same_file_identity(
            expected_file, opened_file
        ):
            return None
        opened = descriptor
        descriptor = -1
        return opened
    except (OSError, NotImplementedError, TypeError, ValueError, UnicodeError):
        return None
    finally:
        if descriptor >= 0:
            try:
                os.close(descriptor)
            except OSError:
                pass
        for opened_directory in reversed(directory_descriptors):
            try:
                os.close(opened_directory)
            except OSError:
                pass


@dataclass(frozen=True)
class TranscriptCandidate:
    path: Path
    size: int
    mtime: float


@dataclass(frozen=True)
class TranscriptReadResult:
    blocks: list[tuple[float, str]]
    error: str | None = None
    incomplete_tail: bool = False
    semantic_end_offset: int = 0
    observed_size: int = 0


def transcript_candidates(dirs: list[Path]) -> list[TranscriptCandidate]:
    candidates: list[TranscriptCandidate] = []
    for d in dirs:
        try:
            paths = list(d.glob("*.jsonl"))
        except OSError:
            continue
        for path in paths:
            path_stat = _regular_file_stat_without_symlink(path)
            if path_stat is None:
                continue
            candidates.append(
                TranscriptCandidate(path, path_stat.st_size, path_stat.st_mtime)
            )
    return candidates


def recheck_selected_transcript(
    value: object,
    project_root: Path,
    pattern: re.Pattern[str],
    tracked_paths: set[str],
) -> TranscriptCandidate | None:
    """Recover a tracked selection omitted by a partial directory listing.

    Only an exact, absolute provider-project path already present in persisted
    size/watermark state is eligible.  This keeps malformed state from gaining
    sticky authority while a direct stat closes the transient discovery gap.
    """
    if not isinstance(value, str) or not value or value not in tracked_paths:
        return None
    path = _lexical_absolute_path(value)
    root = _lexical_absolute_path(project_root)
    if (
        path is None
        or root is None
        or str(path) != value
        or path.suffix != ".jsonl"
        or path.parent.parent != root
        or pattern.fullmatch(path.parent.name) is None
    ):
        return None
    path_stat = _regular_file_stat_without_symlink(path)
    if path_stat is None:
        return None
    return TranscriptCandidate(path, path_stat.st_size, path_stat.st_mtime)


def _validated_transcript_sizes(channel_state: Mapping[str, Any]) -> dict[str, int]:
    raw = channel_state.get(TRANSCRIPT_SIZES_KEY, {})
    if not isinstance(raw, dict):
        return {}
    return {
        path: size
        for path, size in raw.items()
        if isinstance(path, str)
        and path
        and isinstance(size, int)
        and not isinstance(size, bool)
        and size >= 0
    }


def _validated_transcript_seen_at(
    channel_state: Mapping[str, Any], sizes: Mapping[str, int], now: float
) -> dict[str, float]:
    raw = channel_state.get(TRANSCRIPT_SEEN_AT_KEY, {})
    raw = raw if isinstance(raw, dict) else {}
    return {
        path: (
            float(raw[path])
            if path in raw and _is_finite_nonnegative_number(raw[path])
            else now
        )
        for path in sizes
    }


def _validated_transcript_known_at(
    channel_state: Mapping[str, Any], now: float
) -> dict[str, float]:
    raw = channel_state.get(TRANSCRIPT_KNOWN_AT_KEY, {})
    if not isinstance(raw, dict):
        return {}
    return {
        path: float(seen_at)
        for path, seen_at in raw.items()
        if isinstance(path, str)
        and path
        and _is_finite_nonnegative_number(seen_at)
        and now - float(seen_at) <= TRANSCRIPT_HISTORY_TTL_SECS
    }


def _validated_pending_transcripts(channel_state: Mapping[str, Any]) -> list[str]:
    raw = channel_state.get(PENDING_TRANSCRIPTS_KEY, [])
    if not isinstance(raw, list):
        return []
    pending: list[str] = []
    for path in raw:
        if isinstance(path, str) and path and path not in pending:
            pending.append(path)
    return pending[:MAX_PENDING_TRANSCRIPTS]


def _read_failure_authority_paths(
    channel_state: Mapping[str, Any], pending_paths: list[str]
) -> set[str]:
    """Paths whose consecutive read-failure counter must survive this tick.

    Selected, unresolved GAP-owner, and recovered replay-guard transcripts have
    independent evaluation authority in addition to the bounded pending queue.
    Folding any into a full 32-path queue would evict an existing authority, so
    pin their counters separately while leaving the pending cap unchanged.
    """
    authorities = set(pending_paths)
    selected = channel_state.get(SELECTED_TRANSCRIPT_KEY)
    if isinstance(selected, str) and selected:
        authorities.add(selected)
    authorities.update(_validated_gap_owner_transcripts(channel_state))
    authorities.update(_validated_recovered_gap_guards(channel_state))
    return authorities


def _validated_pending_failures(
    channel_state: Mapping[str, Any], pending_paths: list[str]
) -> dict[str, int]:
    raw = channel_state.get(PENDING_TRANSCRIPT_FAILURES_KEY, {})
    if not isinstance(raw, dict):
        return {}
    pending = _read_failure_authority_paths(channel_state, pending_paths)
    return {
        path: failures
        for path, failures in raw.items()
        if path in pending
        and isinstance(failures, int)
        and not isinstance(failures, bool)
        and failures > 0
    }


def _validated_pending_since(
    channel_state: Mapping[str, Any], pending_paths: list[str], now: float
) -> dict[str, float]:
    raw = channel_state.get(PENDING_TRANSCRIPT_SINCE_KEY, {})
    raw = raw if isinstance(raw, dict) else {}
    return {
        path: (
            float(raw[path])
            if path in raw
            and _is_finite_nonnegative_number(raw[path])
            and float(raw[path]) <= now
            else now
        )
        for path in pending_paths
    }


def _validated_gap_owner_transcripts(
    channel_state: Mapping[str, Any],
) -> list[str]:
    """Return bounded unresolved GAP owners, including the legacy singleton."""
    owners: list[str] = []
    raw = channel_state.get(GAP_OWNER_TRANSCRIPTS_KEY, [])
    if isinstance(raw, list):
        for path in raw:
            if isinstance(path, str) and path and path not in owners:
                owners.append(path)
    legacy = channel_state.get(GAP_TRANSCRIPT_KEY)
    if isinstance(legacy, str) and legacy and legacy not in owners:
        owners.append(legacy)
    return owners[:MAX_GAP_OWNER_TRANSCRIPTS]


def _store_gap_owner_transcripts(
    channel_state: dict[str, Any], owners: list[str]
) -> list[str]:
    bounded: list[str] = []
    for path in owners:
        if isinstance(path, str) and path and path not in bounded:
            bounded.append(path)
    bounded = bounded[:MAX_GAP_OWNER_TRANSCRIPTS]
    if bounded:
        channel_state[GAP_OWNER_TRANSCRIPTS_KEY] = bounded
    else:
        channel_state.pop(GAP_OWNER_TRANSCRIPTS_KEY, None)
    return bounded


def _validated_recovered_gap_guards(
    channel_state: Mapping[str, Any],
) -> dict[str, tuple[int, float, float, float | None]]:
    """Return bounded recovered replay guards with absence lifecycle state."""
    raw = channel_state.get(RECOVERED_GAP_GUARDS_KEY, {})
    if not isinstance(raw, dict):
        return {}
    guards: dict[str, tuple[int, float, float, float | None]] = {}
    for path, entry in raw.items():
        if len(guards) >= MAX_RECOVERED_GAP_GUARDS:
            break
        if not isinstance(path, str) or not path or not isinstance(entry, dict):
            continue
        size = entry.get("size")
        confirmed_at = entry.get("confirmed_at")
        last_seen_at = entry.get("last_seen_at")
        absent_since = entry.get("absent_since")
        if (
            isinstance(size, int)
            and not isinstance(size, bool)
            and size >= 0
            and _is_finite_nonnegative_number(confirmed_at)
            and _is_finite_nonnegative_number(last_seen_at)
            and (
                absent_since is None
                or (
                    _is_finite_nonnegative_number(absent_since)
                    and float(absent_since) >= float(last_seen_at)
                )
            )
        ):
            guards[path] = (
                size,
                float(confirmed_at),
                float(last_seen_at),
                None if absent_since is None else float(absent_since),
            )
    return guards


def _store_recovered_gap_guards(
    channel_state: dict[str, Any],
    guards: Mapping[str, tuple[int, float, float, float | None]],
) -> None:
    bounded = dict(list(guards.items())[:MAX_RECOVERED_GAP_GUARDS])
    if bounded:
        channel_state[RECOVERED_GAP_GUARDS_KEY] = {
            path: {
                "size": size,
                "confirmed_at": confirmed_at,
                "last_seen_at": last_seen_at,
                "absent_since": absent_since,
            }
            for path, (
                size,
                confirmed_at,
                last_seen_at,
                absent_since,
            ) in bounded.items()
        }
    else:
        channel_state.pop(RECOVERED_GAP_GUARDS_KEY, None)


RECOVERED_GAP_PATH_PRESENT = "present"
RECOVERED_GAP_PATH_ABSENT = "absent"
RECOVERED_GAP_PATH_AMBIGUOUS = "ambiguous"
RECOVERED_GAP_PATH_INVALID = "invalid"


def _recovered_gap_path_presence_beneath_root(
    path: Path, trusted_root: Path
) -> str:
    """Prove path presence/absence beneath a no-follow descriptor root."""
    directory_flag = getattr(os, "O_DIRECTORY", 0)
    nofollow_flag = getattr(os, "O_NOFOLLOW", 0)
    nonblock_flag = getattr(os, "O_NONBLOCK", 0)
    if not directory_flag or not nofollow_flag or not nonblock_flag:
        return RECOVERED_GAP_PATH_AMBIGUOUS
    normalized_path = _lexical_absolute_path(path)
    normalized_root = _lexical_absolute_path(trusted_root)
    if (
        normalized_path is None
        or normalized_root is None
        or normalized_path != path
        or normalized_root != trusted_root
    ):
        return RECOVERED_GAP_PATH_INVALID
    try:
        relative = normalized_path.relative_to(normalized_root)
    except ValueError:
        return RECOVERED_GAP_PATH_INVALID
    components = relative.parts
    if not components or any(component in ("", ".", "..") for component in components):
        return RECOVERED_GAP_PATH_INVALID

    descriptors: list[int] = []
    directory_flags = (
        os.O_RDONLY
        | getattr(os, "O_CLOEXEC", 0)
        | directory_flag
        | nofollow_flag
        | nonblock_flag
    )
    try:
        try:
            expected_root = normalized_root.stat(follow_symlinks=False)
            if not stat_mode.S_ISDIR(expected_root.st_mode):
                return RECOVERED_GAP_PATH_AMBIGUOUS
            root_descriptor = os.open(normalized_root, directory_flags)
        except (OSError, NotImplementedError, TypeError, ValueError, UnicodeError):
            return RECOVERED_GAP_PATH_AMBIGUOUS
        descriptors.append(root_descriptor)
        try:
            opened_root = os.fstat(root_descriptor)
        except OSError:
            return RECOVERED_GAP_PATH_AMBIGUOUS
        if not stat_mode.S_ISDIR(opened_root.st_mode) or not _same_file_identity(
            expected_root, opened_root
        ):
            return RECOVERED_GAP_PATH_AMBIGUOUS

        for component in components[:-1]:
            parent_descriptor = descriptors[-1]
            try:
                expected_directory = os.stat(
                    component,
                    dir_fd=parent_descriptor,
                    follow_symlinks=False,
                )
            except FileNotFoundError:
                return RECOVERED_GAP_PATH_ABSENT
            except (OSError, NotImplementedError, TypeError, ValueError):
                return RECOVERED_GAP_PATH_AMBIGUOUS
            if not stat_mode.S_ISDIR(expected_directory.st_mode):
                return RECOVERED_GAP_PATH_AMBIGUOUS
            try:
                child_descriptor = os.open(
                    component, directory_flags, dir_fd=parent_descriptor
                )
            except (OSError, NotImplementedError, TypeError, ValueError):
                return RECOVERED_GAP_PATH_AMBIGUOUS
            descriptors.append(child_descriptor)
            try:
                opened_directory = os.fstat(child_descriptor)
            except OSError:
                return RECOVERED_GAP_PATH_AMBIGUOUS
            if not stat_mode.S_ISDIR(
                opened_directory.st_mode
            ) or not _same_file_identity(expected_directory, opened_directory):
                return RECOVERED_GAP_PATH_AMBIGUOUS

        try:
            leaf = os.stat(
                components[-1],
                dir_fd=descriptors[-1],
                follow_symlinks=False,
            )
        except FileNotFoundError:
            return RECOVERED_GAP_PATH_ABSENT
        except (OSError, NotImplementedError, TypeError, ValueError):
            return RECOVERED_GAP_PATH_AMBIGUOUS
        return (
            RECOVERED_GAP_PATH_PRESENT
            if stat_mode.S_ISREG(leaf.st_mode)
            else RECOVERED_GAP_PATH_AMBIGUOUS
        )
    finally:
        for descriptor in reversed(descriptors):
            try:
                os.close(descriptor)
            except OSError:
                pass


def _recovered_gap_path_presence(
    value: str,
    discovered_paths: set[str],
    project_root: Path,
    pattern: re.Pattern[str],
) -> str:
    """Classify a guarded path without treating discovery/I/O errors as absence."""
    if value in discovered_paths:
        return RECOVERED_GAP_PATH_PRESENT
    path = _lexical_absolute_path(value)
    root = _lexical_absolute_path(project_root)
    if (
        path is None
        or root is None
        or str(path) != value
        or path.suffix != ".jsonl"
        or path.parent.parent != root
        or pattern.fullmatch(path.parent.name) is None
    ):
        return RECOVERED_GAP_PATH_INVALID
    return _recovered_gap_path_presence_beneath_root(
        path,
        root,
    )


def _refresh_recovered_gap_guards(
    guards: Mapping[str, tuple[int, float, float, float | None]],
    discovered_paths: set[str],
    protected_paths: set[str],
    project_root: Path,
    pattern: re.Pattern[str],
    now: float,
) -> tuple[dict[str, tuple[int, float, float, float | None]], list[str]]:
    """Advance guard lifecycles and reclaim only continuously absent paths."""
    refreshed: dict[str, tuple[int, float, float, float | None]] = {}
    reclaimed: list[str] = []
    for path, (size, confirmed_at, last_seen_at, absent_since) in guards.items():
        presence = _recovered_gap_path_presence(
            path, discovered_paths, project_root, pattern
        )
        if presence == RECOVERED_GAP_PATH_INVALID:
            reclaimed.append(path)
            continue
        if presence == RECOVERED_GAP_PATH_PRESENT:
            refreshed[path] = (size, confirmed_at, max(last_seen_at, now), None)
            continue
        if presence == RECOVERED_GAP_PATH_AMBIGUOUS:
            # Ambiguity breaks proof of continuous absence. Never expire a
            # replay floor because a directory read, stat, or file type was
            # unsafe to interpret.
            refreshed[path] = (size, confirmed_at, last_seen_at, None)
            continue
        if last_seen_at > now:
            # A wall-clock rollback cannot establish elapsed absence. Keep the
            # floor armed until time catches up and a fresh absence interval
            # can be observed.
            refreshed[path] = (size, confirmed_at, last_seen_at, None)
            continue
        missing_since = absent_since
        if missing_since is None or missing_since > now:
            missing_since = now
        if (
            path not in protected_paths
            and now - missing_since >= RECOVERED_GAP_GUARD_TTL_SECS
        ):
            reclaimed.append(path)
            continue
        refreshed[path] = (size, confirmed_at, last_seen_at, missing_since)
    return refreshed, reclaimed


def _upsert_recovered_gap_guard(
    guards: Mapping[str, tuple[int, float, float, float | None]],
    path: str,
    size: int,
    confirmed_at: float,
) -> tuple[dict[str, tuple[int, float, float, float | None]], bool]:
    """Re-arm one guard, refusing new admission when the bound is full."""
    updated = dict(guards)
    if (
        not path
        or not isinstance(size, int)
        or isinstance(size, bool)
        or size < 0
        or not _is_finite_nonnegative_number(confirmed_at)
    ):
        return updated, False
    if path not in updated and len(updated) >= MAX_RECOVERED_GAP_GUARDS:
        return updated, False
    updated[path] = (
        size,
        float(confirmed_at),
        float(confirmed_at),
        None,
    )
    return updated, True


def _bounded_retired_transcripts(
    entries: Mapping[str, tuple[int, float]],
) -> dict[str, tuple[int, float]]:
    ordered = sorted(
        entries.items(), key=lambda item: (-item[1][1], item[0])
    )[:MAX_RETIRED_TRANSCRIPTS]
    return dict(ordered)


def _validated_retired_transcripts(
    channel_state: Mapping[str, Any],
) -> dict[str, tuple[int, float]]:
    raw = channel_state.get(RETIRED_TRANSCRIPTS_KEY, {})
    if not isinstance(raw, dict):
        return {}
    valid: dict[str, tuple[int, float]] = {}
    for path, entry in raw.items():
        if not isinstance(path, str) or not path or not isinstance(entry, dict):
            continue
        size = entry.get("size")
        retired_at = entry.get("retired_at")
        if (
            isinstance(size, int)
            and not isinstance(size, bool)
            and size >= 0
            and _is_finite_nonnegative_number(retired_at)
        ):
            valid[path] = (size, float(retired_at))
    return _bounded_retired_transcripts(valid)


def _store_retired_transcripts(
    channel_state: dict[str, Any], entries: Mapping[str, tuple[int, float]]
) -> None:
    bounded = _bounded_retired_transcripts(entries)
    if bounded:
        channel_state[RETIRED_TRANSCRIPTS_KEY] = {
            path: {"size": size, "retired_at": retired_at}
            for path, (size, retired_at) in bounded.items()
        }
    else:
        channel_state.pop(RETIRED_TRANSCRIPTS_KEY, None)


def _bounded_transcript_history(
    sizes: Mapping[str, int],
    seen_at: Mapping[str, float],
    now: float,
    priority_paths: list[str],
) -> tuple[dict[str, int], dict[str, float]]:
    """Bound path baselines without dropping a transiently undiscovered path."""
    priority = {
        path: index
        for index, path in enumerate(dict.fromkeys(priority_paths))
    }
    entries: list[tuple[str, int, float]] = []
    for path, size in sizes.items():
        if not isinstance(path, str) or not path:
            continue
        if not isinstance(size, int) or isinstance(size, bool) or size < 0:
            continue
        observed_at = seen_at.get(path, now)
        if not _is_finite_nonnegative_number(observed_at):
            observed_at = now
        observed_at = float(observed_at)
        if (
            path not in priority
            and now - observed_at > TRANSCRIPT_HISTORY_TTL_SECS
        ):
            continue
        entries.append((path, size, observed_at))
    entries.sort(
        key=lambda entry: (
            priority.get(entry[0], len(priority)),
            -entry[2],
            entry[0],
        )
    )
    bounded = entries[:MAX_TRANSCRIPT_HISTORY]
    return (
        {path: size for path, size, _ in bounded},
        {path: observed_at for path, _, observed_at in bounded},
    )


def _bounded_pending_transcripts(
    paths: list[str], history_paths: set[str]
) -> list[str]:
    pending: list[str] = []
    for path in paths:
        if path in history_paths and path not in pending:
            pending.append(path)
    return pending[:MAX_PENDING_TRANSCRIPTS]


def _bounded_transcript_known_at(
    known_at: Mapping[str, float], now: float, priority_paths: list[str]
) -> dict[str, float]:
    priority = {
        path: index
        for index, path in enumerate(dict.fromkeys(priority_paths))
    }
    entries = [
        (path, float(seen_at))
        for path, seen_at in known_at.items()
        if isinstance(path, str)
        and path
        and _is_finite_nonnegative_number(seen_at)
        and (
            path in priority
            or now - float(seen_at) <= TRANSCRIPT_HISTORY_TTL_SECS
        )
    ]
    entries.sort(
        key=lambda entry: (
            priority.get(entry[0], len(priority)),
            -entry[1],
            entry[0],
        )
    )
    return dict(entries[:MAX_KNOWN_TRANSCRIPTS])


def select_watch_transcript(
    candidates: list[TranscriptCandidate],
    previous_sizes: Mapping[str, int],
    previous_selected: str | Path | None = None,
    semantic_growth_paths: set[str] | None = None,
) -> Path | None:
    """Choose by semantic growth, then retain the previous selected path.

    The caller proves growth by finding a newly appended, timestamped,
    deliverable assistant block beyond the prior byte baseline.  Raw bytes,
    metadata rows, blank lines, and mtime touches never grant selection
    authority.  The caller owns I/O and persistence, keeping this selector
    pure.
    """
    return select_watch_transcript_with_reason(
        candidates,
        previous_sizes,
        previous_selected,
        semantic_growth_paths,
    )[0]


def select_watch_transcript_with_reason(
    candidates: list[TranscriptCandidate],
    previous_sizes: Mapping[str, int],
    previous_selected: object = None,
    semantic_growth_paths: set[str] | None = None,
) -> tuple[Path | None, str]:
    if not candidates:
        return None, "no_candidates"
    semantic_growth_paths = semantic_growth_paths or set()
    growing = [
        candidate
        for candidate in candidates
        if str(candidate.path) in previous_sizes
        and str(candidate.path) in semantic_growth_paths
    ]
    if growing:
        selected = max(
            growing, key=lambda candidate: (candidate.mtime, str(candidate.path))
        )
        return selected.path, "growth"
    prior = (
        str(previous_selected)
        if isinstance(previous_selected, (str, Path)) and str(previous_selected)
        else None
    )
    if prior is not None:
        retained = next(
            (candidate for candidate in candidates if str(candidate.path) == prior),
            None,
        )
        if retained is not None:
            unseen_newer = [
                candidate
                for candidate in candidates
                if candidate.path != retained.path
                and str(candidate.path) not in previous_sizes
                and candidate.mtime > retained.mtime
            ]
            if unseen_newer:
                selected = max(
                    unseen_newer,
                    key=lambda candidate: (candidate.mtime, str(candidate.path)),
                )
                return selected.path, "unseen_newer"
            return retained.path, "sticky"
    selected = max(
        candidates, key=lambda candidate: (candidate.mtime, str(candidate.path))
    )
    return selected.path, "prior_missing" if prior is not None else "bootstrap"


def newest_transcript(dirs: list[Path]) -> Path | None:
    """Backward-compatible mtime selector for callers without growth state."""
    return select_watch_transcript(transcript_candidates(dirs), {}, None, set())


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


def is_harness_control_assistant_record(record: object) -> bool:
    """Whether an assistant JSONL row is synthetic harness control data.

    The visible banner text is deliberately irrelevant: users and normal
    assistant responses may legitimately discuss the same words. Claude marks
    every non-deliverable harness-authored assistant row with the synthetic
    model identity, independent of API status/error shape.
    """
    if not isinstance(record, dict):
        return False
    message = record.get("message")
    return isinstance(message, dict) and message.get("model") == "<synthetic>"


def assistant_blocks_from_lines(lines) -> list[tuple[float, str]]:
    """(epoch, text) for every assistant text block in a transcript's lines."""
    out: list[tuple[float, str]] = []
    for line in lines:
        try:
            r = json.loads(line)
        except (json.JSONDecodeError, TypeError):
            continue
        if (
            not isinstance(r, dict)
            or r.get("type") != "assistant"
            or is_harness_control_assistant_record(r)
        ):
            continue
        epoch = parse_transcript_ts(r.get("timestamp", ""))
        if epoch is None:
            continue
        message = r.get("message")
        if not isinstance(message, dict):
            continue
        for c in message.get("content") or []:
            if isinstance(c, dict) and c.get("type") == "text":
                t = (c.get("text") or "").strip()
                if t:
                    out.append((epoch, t))
    return out


def assistant_blocks(
    transcript: Path, trusted_root: Path | None = None
) -> TranscriptReadResult:
    flags = (
        os.O_RDONLY
        | getattr(os, "O_CLOEXEC", 0)
        | getattr(os, "O_NOFOLLOW", 0)
        | getattr(os, "O_NONBLOCK", 0)
    )
    descriptor = -1
    try:
        # Provider transcripts have shape ``projects/<session>/<uuid>.jsonl``;
        # the projects directory is the narrowest stable trust boundary that
        # still lets the component walk reject a swapped session directory.
        # Production passes it explicitly; the derived value keeps this helper
        # fail-closed and useful for focused tests.
        root = trusted_root if trusted_root is not None else transcript.parent.parent
        opened = _open_regular_file_beneath_parent(transcript, flags, root)
        if opened is None:
            return TranscriptReadResult([], "UnsafePath")
        descriptor = opened
        if not stat_mode.S_ISREG(os.fstat(descriptor).st_mode):
            return TranscriptReadResult([], "UnsafePath")
        if fcntl is not None and getattr(os, "O_NONBLOCK", 0):
            current_flags = fcntl.fcntl(descriptor, fcntl.F_GETFL)
            fcntl.fcntl(
                descriptor,
                fcntl.F_SETFL,
                current_flags & ~getattr(os, "O_NONBLOCK", 0),
            )
        stream = os.fdopen(descriptor, "rb")
        descriptor = -1
        with stream as f:
            raw_lines = f.readlines()
            lines = [line.decode("utf-8") for line in raw_lines]
            incomplete_tail = False
            if raw_lines and not raw_lines[-1].endswith(b"\n"):
                try:
                    json.loads(lines[-1])
                except (json.JSONDecodeError, TypeError):
                    incomplete_tail = True
            blocks: list[tuple[float, str]] = []
            semantic_end_offset = 0
            byte_offset = 0
            for raw_line, line in zip(raw_lines, lines):
                line_blocks = assistant_blocks_from_lines([line])
                blocks.extend(line_blocks)
                byte_offset += len(raw_line)
                if line_blocks:
                    semantic_end_offset = byte_offset
            return TranscriptReadResult(
                blocks,
                incomplete_tail=incomplete_tail,
                semantic_end_offset=semantic_end_offset,
                observed_size=byte_offset,
            )
    except (OSError, UnicodeError, ValueError) as exc:
        return TranscriptReadResult([], type(exc).__name__)
    finally:
        if descriptor >= 0:
            try:
                os.close(descriptor)
            except OSError:
                pass


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
class CoverageActivityVerdict:
    state: str
    reason: str


@dataclass(frozen=True)
class CoverageActivityProbe:
    """Exact watcher-state fields that can corroborate a foreground stream."""

    relay_stall_state: str | None = None
    active_turn: str | None = None
    queue_depth: int | None = None
    tmux_alive: bool | None = None
    watcher_attached: bool | None = None
    watcher_attached_stale: bool | None = None
    watcher_owns_live_relay: bool | None = None
    last_outbound_activity_ms: int | None = None
    last_relay_ts_ms: int | None = None
    desynced: bool | None = None
    malformed: bool = False


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
    # #4458: derived foreground/liveness evidence from top-level
    # `relay_stall_state` plus nested `relay_health`. `None` is a legacy server
    # with neither field and preserves the pre-#4458 desync behavior.
    relay_activity: CoverageActivityProbe | None = None
    # Provider session identity is authoritative across runtime-mirror/provider-
    # project path representations. It is optional for legacy watcher-state.
    bound_session_id: str | None = None


def canonical_session_uuid(value: object) -> str | None:
    if not isinstance(value, str) or not value:
        return None
    try:
        parsed = uuid.UUID(value)
    except (ValueError, AttributeError):
        return None
    canonical = str(parsed)
    return canonical if value == canonical else None


def parse_watcher_state_probe(
    status: int | None, payload: object
) -> WatcherStateProbe:
    """Pure, exact-type parser for the watcher-state response contract."""
    if status != 200:
        return WatcherStateProbe(status)
    if not isinstance(payload, Mapping):
        return WatcherStateProbe(200)

    attached_raw = payload.get("attached")
    desynced_raw = payload.get("desynced")
    bound_output_path = payload.get("bound_output_path")
    bound_session_id_raw = payload.get("bound_session_id")
    attached = attached_raw if isinstance(attached_raw, bool) else None
    desynced = desynced_raw if isinstance(desynced_raw, bool) else None
    bound = bound_output_path if isinstance(bound_output_path, str) else None
    bound_session_id = canonical_session_uuid(bound_session_id_raw)

    has_activity_schema = (
        "relay_stall_state" in payload or "relay_health" in payload
    )
    if not has_activity_schema:
        return WatcherStateProbe(
            200, attached, desynced, bound, bound_session_id=bound_session_id
        )

    malformed = False
    stall_raw = payload.get("relay_stall_state")
    if stall_raw is None:
        relay_stall_state = None
    elif isinstance(stall_raw, str) and stall_raw:
        relay_stall_state = stall_raw
    else:
        relay_stall_state = None
        malformed = True

    relay_raw = payload.get("relay_health")
    if not isinstance(relay_raw, Mapping):
        return WatcherStateProbe(
            200,
            attached,
            desynced,
            bound,
            CoverageActivityProbe(
                relay_stall_state=relay_stall_state,
                malformed=True,
            ),
            bound_session_id,
        )

    def string_field(key: str) -> tuple[str | None, bool]:
        if key not in relay_raw:
            return None, False
        value = relay_raw.get(key)
        if isinstance(value, str) and value:
            return value, False
        return None, True

    def bool_field(
        key: str, *, nullable: bool = False
    ) -> tuple[bool | None, bool]:
        if key not in relay_raw:
            return None, False
        value = relay_raw.get(key)
        if isinstance(value, bool):
            return value, False
        if value is None and nullable:
            return None, False
        return None, True

    def int_field(
        key: str, *, nullable: bool = False
    ) -> tuple[int | None, bool]:
        if key not in relay_raw:
            return None, False
        value = relay_raw.get(key)
        if isinstance(value, int) and not isinstance(value, bool) and value >= 0:
            return value, False
        if value is None and nullable:
            return None, False
        return None, True

    active_turn, bad_active_turn = string_field("active_turn")
    queue_depth, bad_queue_depth = int_field("queue_depth")
    tmux_alive, bad_tmux_alive = bool_field("tmux_alive", nullable=True)
    watcher_attached, bad_watcher_attached = bool_field("watcher_attached")
    watcher_attached_stale, bad_watcher_attached_stale = bool_field(
        "watcher_attached_stale"
    )
    watcher_owns_live_relay, bad_watcher_owns_live_relay = bool_field(
        "watcher_owns_live_relay"
    )
    last_outbound_activity_ms, bad_last_outbound_activity_ms = int_field(
        "last_outbound_activity_ms", nullable=True
    )
    last_relay_ts_ms, bad_last_relay_ts_ms = int_field(
        "last_relay_ts_ms", nullable=True
    )
    relay_desynced, bad_relay_desynced = bool_field("desynced")
    malformed = malformed or any(
        (
            bad_active_turn,
            bad_queue_depth,
            bad_tmux_alive,
            bad_watcher_attached,
            bad_watcher_attached_stale,
            bad_watcher_owns_live_relay,
            bad_last_outbound_activity_ms,
            bad_last_relay_ts_ms,
            bad_relay_desynced,
        )
    )
    return WatcherStateProbe(
        200,
        attached,
        desynced,
        bound,
        CoverageActivityProbe(
            relay_stall_state=relay_stall_state,
            active_turn=active_turn,
            queue_depth=queue_depth,
            tmux_alive=tmux_alive,
            watcher_attached=watcher_attached,
            watcher_attached_stale=watcher_attached_stale,
            watcher_owns_live_relay=watcher_owns_live_relay,
            last_outbound_activity_ms=last_outbound_activity_ms,
            last_relay_ts_ms=last_relay_ts_ms,
            desynced=relay_desynced,
            malformed=malformed,
        ),
        bound_session_id,
    )


def evaluate_active_foreground_coverage(
    activity: CoverageActivityProbe | None,
    now_ms: object,
    freshness_secs: object = COVERAGE_ACTIVITY_FRESH_SECS,
) -> CoverageActivityVerdict:
    """Prove that an attached desync is a live foreground streaming snapshot."""
    if activity is None:
        # Legacy watcher-state: retain the original attached+desynced failure.
        return CoverageActivityVerdict(
            COVERAGE_UNCOVERED, "active_foreground_schema_absent"
        )

    # Explicit negative evidence always wins over partial/malformed positive
    # evidence. These are real coverage failures, not parser uncertainty.
    if (
        activity.relay_stall_state is not None
        and activity.relay_stall_state != "active_foreground_stream"
    ):
        return CoverageActivityVerdict(
            COVERAGE_UNCOVERED, "relay_stall_state_not_active_foreground"
        )
    if activity.active_turn is not None and activity.active_turn != "foreground":
        return CoverageActivityVerdict(
            COVERAGE_UNCOVERED, "active_turn_not_foreground"
        )
    if activity.queue_depth is not None and activity.queue_depth != 0:
        return CoverageActivityVerdict(
            COVERAGE_UNCOVERED, "active_foreground_queue_not_empty"
        )
    if activity.tmux_alive is False:
        return CoverageActivityVerdict(COVERAGE_UNCOVERED, "tmux_not_alive")
    if activity.watcher_attached is False:
        return CoverageActivityVerdict(COVERAGE_UNCOVERED, "watcher_detached")
    if activity.watcher_attached_stale is True:
        return CoverageActivityVerdict(
            COVERAGE_UNCOVERED, "watcher_attachment_stale"
        )
    if activity.watcher_owns_live_relay is False:
        return CoverageActivityVerdict(
            COVERAGE_UNCOVERED, "watcher_does_not_own_live_relay"
        )
    if activity.desynced is False:
        return CoverageActivityVerdict(
            COVERAGE_UNCOVERED, "watcher_state_desync_inconsistent"
        )

    active_hint = (
        activity.relay_stall_state == "active_foreground_stream"
        or activity.active_turn == "foreground"
    )
    required_evidence_missing = any(
        field is None
        for field in (
            activity.relay_stall_state,
            activity.active_turn,
            activity.queue_depth,
            activity.tmux_alive,
            activity.watcher_attached,
            activity.watcher_attached_stale,
            activity.watcher_owns_live_relay,
            activity.desynced,
        )
    )
    if activity.malformed or required_evidence_missing:
        return CoverageActivityVerdict(
            COVERAGE_UNCOVERED, "active_foreground_evidence_incomplete"
        )
    if not active_hint:
        return CoverageActivityVerdict(
            COVERAGE_UNCOVERED, "active_foreground_not_observed"
        )
    if not (
        activity.relay_stall_state == "active_foreground_stream"
        and activity.active_turn == "foreground"
        and activity.queue_depth == 0
        and activity.tmux_alive is True
        and activity.watcher_attached is True
        and activity.watcher_attached_stale is False
        and activity.watcher_owns_live_relay is True
        and activity.desynced is True
    ):
        return CoverageActivityVerdict(
            COVERAGE_UNCOVERED, "active_foreground_evidence_rejected"
        )

    if not (
        _is_finite_nonnegative_number(now_ms)
        and _is_finite_nonnegative_number(freshness_secs)
        and float(freshness_secs) > 0
    ):
        return CoverageActivityVerdict(
            COVERAGE_UNCOVERED, "active_foreground_clock_unknown"
        )
    now_value = float(now_ms)
    freshness_ms = float(freshness_secs) * 1000
    if not math.isfinite(freshness_ms):
        return CoverageActivityVerdict(
            COVERAGE_UNCOVERED, "active_foreground_clock_unknown"
        )
    timestamps: list[float] = []
    for raw_timestamp in (
        activity.last_outbound_activity_ms,
        activity.last_relay_ts_ms,
    ):
        if not (
            isinstance(raw_timestamp, int)
            and not isinstance(raw_timestamp, bool)
            and raw_timestamp > 0
        ):
            continue
        try:
            timestamp = float(raw_timestamp)
        except (OverflowError, ValueError):
            return CoverageActivityVerdict(
                COVERAGE_UNCOVERED, "active_foreground_activity_invalid"
            )
        if not math.isfinite(timestamp):
            return CoverageActivityVerdict(
                COVERAGE_UNCOVERED, "active_foreground_activity_invalid"
            )
        timestamps.append(timestamp)
    if not timestamps:
        return CoverageActivityVerdict(
            COVERAGE_UNCOVERED, "active_foreground_activity_absent"
        )
    freshest = max(timestamps)
    age_ms = now_value - freshest
    if age_ms < 0:
        return CoverageActivityVerdict(
            COVERAGE_UNCOVERED, "active_foreground_activity_future"
        )
    if age_ms < freshness_ms:
        return CoverageActivityVerdict(
            COVERAGE_COVERED, "active_foreground_recent_activity"
        )
    return CoverageActivityVerdict(
        COVERAGE_UNCOVERED, "active_foreground_activity_stale"
    )


def evaluate_coverage(
    expected_alive: bool | None,
    watcher_status: int | None,
    attached: bool | None,
    desynced: bool | None,
    previous_uncovered: int,
    relay_activity: CoverageActivityProbe | None = None,
    now_ms: object = None,
    activity_freshness_secs: object = COVERAGE_ACTIVITY_FRESH_SECS,
) -> CoverageVerdict:
    """Pure I2 judgment for expected tmux coverage.

    E is independently enumerated tmux liveness. A is normally
    ``attached and not desynced`` from watcher-state; #4458 also accepts an
    exact, fresh active-foreground relay proof while the snapshot is transiently
    desynced. Only E && !A advances confirmation. Core watcher-state transport/
    schema uncertainty is unknown, but optional activity evidence can only prove
    the exception; incomplete or malformed activity never weakens an otherwise
    authoritative attached+desynced failure. An authoritative watcher-state 404
    is uncovered. Two consecutive uncovered ticks are required.
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
        activity_verdict = evaluate_active_foreground_coverage(
            relay_activity,
            now_ms,
            activity_freshness_secs,
        )
        if activity_verdict.state == COVERAGE_COVERED:
            return CoverageVerdict(
                COVERAGE_COVERED, activity_verdict.reason, 0, False
            )
        # Supplemental activity is a one-way exception proof. Any outcome
        # other than exact COVERED retains the original desync invariant.
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
    bound_kind = classify_selector_path(bound_output_path)
    selected_kind = classify_selector_path(selected_transcript)
    if SELECTOR_PATH_RUNTIME_MIRROR in (bound_kind, selected_kind):
        return SelectorVerdict(
            SELECTOR_UNKNOWN, "runtime_session_mirror_uncomparable", False
        )
    if (
        bound_kind != SELECTOR_PATH_PROVIDER_PROJECT
        or selected_kind != SELECTOR_PATH_PROVIDER_PROJECT
    ):
        return SelectorVerdict(SELECTOR_UNKNOWN, "selector_paths_uncomparable", False)
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
    prior_delivered_ts: float = 0.0,
) -> Verdict:
    """Core relay-gap judgment descended from the 07-09 logic and subsequently
    extended through the #4140→#4178→#4181 lineage. The health watermark is the
    LAST SUCCESSFUL delivery, not `any lost`: a historic gap (already reported,
    already recovered) must not re-alert forever, and relay chunking can deliver
    a later block while an earlier one is still missing. Both conditions — lost
    blocks exist AND the watermark is older than gap_alert_secs — must hold to
    declare a gap.
    """
    prior = (
        float(prior_delivered_ts)
        if _is_finite_nonnegative_number(prior_delivered_ts)
        else 0.0
    )
    current_delivered_ts = max(
        (e for e, t in blocks if delivered(t, hay)), default=0.0
    )
    # Discord reads are bounded.  Absence from today's haystack cannot erase a
    # delivery confirmed by an earlier tick or process lifetime.
    delivered_ts = max(prior, current_delivered_ts)
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


def _is_finite_nonnegative_number(value: object) -> bool:
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        return False
    try:
        numeric = float(value)
    except (OverflowError, ValueError):
        return False
    return math.isfinite(numeric) and numeric >= 0.0


def _bounded_delivered_watermarks(
    entries: Mapping[str, tuple[float, float]],
    preferred_path: str | None = None,
    pinned_paths: list[str] | None = None,
) -> dict[str, tuple[float, float]]:
    pinned = {
        path: index
        for index, path in enumerate(dict.fromkeys(pinned_paths or []))
        if path
    }
    ordered = sorted(
        entries.items(),
        key=lambda item: (
            pinned.get(item[0], len(pinned)),
            -item[1][1],
            0 if item[0] == preferred_path else 1,
            item[0],
        ),
    )[:MAX_DELIVERED_WATERMARKS]
    return dict(ordered)


def _delivered_watermark_authority_paths(
    channel_state: Mapping[str, Any],
) -> list[str]:
    """Return every bounded path that still owns delivery authority."""
    selected = channel_state.get(SELECTED_TRANSCRIPT_KEY)
    authorities = [selected] if isinstance(selected, str) and selected else []
    authorities.extend(_validated_pending_transcripts(channel_state))
    authorities.extend(_validated_gap_owner_transcripts(channel_state))
    authorities.extend(_validated_recovered_gap_guards(channel_state))
    return list(dict.fromkeys(authorities))


def delivered_watermarks(
    channel_state: Mapping[str, Any],
) -> dict[str, tuple[float, float]]:
    """Return validated ``path -> (delivered_ts, updated_at)`` state.

    Malformed legacy/operator-edited state is ignored fail-open.  The returned
    map is deterministically bounded even if the persisted input was not.
    """
    raw = channel_state.get(DELIVERED_WATERMARKS_KEY, {})
    if not isinstance(raw, dict):
        return {}
    valid: dict[str, tuple[float, float]] = {}
    for path, entry in raw.items():
        if not isinstance(path, str) or not path or not isinstance(entry, dict):
            continue
        delivered_ts = entry.get("delivered_ts")
        updated_at = entry.get("updated_at")
        if not (
            _is_finite_nonnegative_number(delivered_ts)
            and _is_finite_nonnegative_number(updated_at)
        ):
            continue
        valid[path] = (float(delivered_ts), float(updated_at))
    return _bounded_delivered_watermarks(
        valid,
        pinned_paths=_delivered_watermark_authority_paths(channel_state),
    )


def delivered_watermark_for_path(
    channel_state: Mapping[str, Any], transcript: str | Path
) -> float:
    entry = delivered_watermarks(channel_state).get(str(transcript))
    return entry[0] if entry is not None else 0.0


def advance_delivered_watermark(
    channel_state: dict[str, Any],
    transcript: str | Path,
    delivered_ts: object,
    now: object,
) -> bool:
    """Persist a genuine per-path monotonic delivery advancement."""
    if not (
        _is_finite_nonnegative_number(delivered_ts)
        and _is_finite_nonnegative_number(now)
    ):
        return False
    path = str(transcript)
    if not path:
        return False
    entries = delivered_watermarks(channel_state)
    prior = entries.get(path, (0.0, 0.0))[0]
    candidate = float(delivered_ts)
    if candidate <= prior:
        return False
    entries[path] = (candidate, float(now))
    bounded = _bounded_delivered_watermarks(
        entries,
        preferred_path=path,
        pinned_paths=_delivered_watermark_authority_paths(channel_state),
    )
    channel_state[DELIVERED_WATERMARKS_KEY] = {
        key: {"delivered_ts": watermark, "updated_at": updated_at}
        for key, (watermark, updated_at) in bounded.items()
    }
    return True


def _forget_reclaimed_recovered_gap_lifecycles(
    channel_state: dict[str, Any], paths: list[str]
) -> None:
    """Drop all replay identity for guards proven absent for one full TTL."""
    reclaimed = set(paths)
    if not reclaimed:
        return

    selected = channel_state.get(SELECTED_TRANSCRIPT_KEY)
    if isinstance(selected, str) and selected in reclaimed:
        channel_state.pop(SELECTED_TRANSCRIPT_KEY, None)

    for key in (
        TRANSCRIPT_SIZES_KEY,
        TRANSCRIPT_SEEN_AT_KEY,
        TRANSCRIPT_KNOWN_AT_KEY,
        PENDING_TRANSCRIPT_FAILURES_KEY,
        PENDING_TRANSCRIPT_SINCE_KEY,
        RETIRED_TRANSCRIPTS_KEY,
    ):
        raw = channel_state.get(key)
        if not isinstance(raw, dict):
            continue
        retained = {path: entry for path, entry in raw.items() if path not in reclaimed}
        if retained:
            channel_state[key] = retained
        else:
            channel_state.pop(key, None)

    raw_pending = channel_state.get(PENDING_TRANSCRIPTS_KEY)
    if isinstance(raw_pending, list):
        retained_pending = [path for path in raw_pending if path not in reclaimed]
        if retained_pending:
            channel_state[PENDING_TRANSCRIPTS_KEY] = retained_pending
        else:
            channel_state.pop(PENDING_TRANSCRIPTS_KEY, None)

    watermarks = {
        path: entry
        for path, entry in delivered_watermarks(channel_state).items()
        if path not in reclaimed
    }
    if watermarks:
        bounded = _bounded_delivered_watermarks(
            watermarks,
            pinned_paths=_delivered_watermark_authority_paths(channel_state),
        )
        channel_state[DELIVERED_WATERMARKS_KEY] = {
            path: {"delivered_ts": delivered_ts, "updated_at": updated_at}
            for path, (delivered_ts, updated_at) in bounded.items()
        }
    else:
        channel_state.pop(DELIVERED_WATERMARKS_KEY, None)


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
        return parse_watcher_state_probe(200, payload)

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
        probe.relay_activity,
        int(now * 1000),
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
        chs.pop("coverage_desync_since", None)
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

    desync_since: float | None = None
    if verdict.reason == "attached_but_desynced":
        raw_desync_since = chs.get("coverage_desync_since")
        if (
            _is_finite_nonnegative_number(raw_desync_since)
            and float(raw_desync_since) <= now
        ):
            desync_since = float(raw_desync_since)
        else:
            desync_since = now
            chs["coverage_desync_since"] = desync_since

    if not verdict.confirmed:
        rt.log(
            f"[{cid}] coverage uncovered reason={verdict.reason} "
            f"confirm={verdict.consecutive_uncovered}/{COVERAGE_CONFIRM_TICKS}"
        )
        return
    # #4504: attached_but_desynced is a watcher-state desync, not proof of
    # delivery loss. Require a corroborating real delivery gap OR much longer
    # persistence before this alarms on its own. detached / watcher_state_404
    # keep the 2-tick behavior (different reason strings).
    if verdict.reason == "attached_but_desynced":
        delivery_gap_active = bool(chs.get("gap_since") or chs.get("alerting"))
        desync_for = (
            max(0.0, now - desync_since) if desync_since is not None else 0.0
        )
        if (
            not delivery_gap_active
            and desync_for < COVERAGE_DESYNC_CONFIRM_SECS
        ):
            rt.log(
                f"[{cid}] coverage desync uncorroborated "
                f"duration={int(desync_for)}s/"
                f"{COVERAGE_DESYNC_CONFIRM_SECS}s — not alarming"
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
    if not selected_transcript:
        chs.pop("selector_diverged_since", None)
        chs.pop(SELECTOR_DIVERGED_TRANSCRIPT_KEY, None)
        return
    selected_path = str(selected_transcript)
    window_path = chs.get(SELECTOR_DIVERGED_TRANSCRIPT_KEY)
    if not f_growing:
        # A quiet/tool-only tick supplies no new F evidence.  It must not erase
        # a previously proven divergence for the same selection; otherwise one
        # normal long tool call resets the 300s confirmation forever.  A changed
        # selection does invalidate the old window.
        if (
            isinstance(chs.get("selector_diverged_since"), (int, float))
            and not isinstance(chs.get("selector_diverged_since"), bool)
            and window_path == selected_path
        ):
            rt.log(
                f"[{cid}] selector-sync quiet; retained divergence window "
                f"F={selected_path}"
            )
            return
        chs.pop("selector_diverged_since", None)
        chs.pop(SELECTOR_DIVERGED_TRANSCRIPT_KEY, None)
        return

    probe = rt.watcher_state(cid)
    bound = probe.bound_output_path if probe.status == 200 else None
    verdict = evaluate_selector_sync(bound, selected_path, f_growing)

    if not verdict.diverged:
        chs.pop("selector_diverged_since", None)
        chs.pop(SELECTOR_DIVERGED_TRANSCRIPT_KEY, None)
        if verdict.state == SELECTOR_UNKNOWN:
            # Fail-closed: old server without the field, JSON null, or dcserver
            # unreachable → never alarm on an unknown bind.
            rt.log(f"[{cid}] selector-sync unknown reason={verdict.reason} — no alert")
        elif chs.pop("selector_alerting", None):
            rt.log(f"[{cid}] selector-sync restored (B==F) reason={verdict.reason}")
        return

    raw_since = chs.get("selector_diverged_since")
    if (
        window_path == selected_path
        and isinstance(raw_since, (int, float))
        and not isinstance(raw_since, bool)
    ):
        since = float(raw_since)
    else:
        since = now
    chs["selector_diverged_since"] = since
    chs[SELECTOR_DIVERGED_TRANSCRIPT_KEY] = selected_path
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


def _alert_pending_retirement(
    rt: Runtime,
    ch: ChannelConfig,
    channel_state: dict[str, Any],
    paths: list[str],
    now: float,
    *,
    reason: str,
) -> bool:
    if not paths:
        return False
    raw_last_alert = channel_state.get(
        LAST_PENDING_TRANSCRIPT_RETIREMENT_ALERT_KEY, 0.0
    )
    last_alert = (
        float(raw_last_alert)
        if _is_finite_nonnegative_number(raw_last_alert)
        else 0.0
    )
    if now - last_alert < rt.cfg.realert_secs:
        rt.log(
            f"[{ch.channel_id}] transcript-retirement-alert suppressed "
            f"reason={reason} count={len(paths)} cooldown"
        )
        return False
    if reason == "idle":
        title = "릴레이 트랜스크립트 평가 권한 만료"
        detail = (
            "세션 활동이 idle 한계를 넘겨 더 이상 live GAP으로 반복 평가하지 "
            "않습니다. 미도달 여부가 해결됐다고 주장하는 복구 알림은 보내지 않습니다."
        )
    else:
        title = "릴레이 트랜스크립트 평가 불능 에스컬레이션"
        detail = (
            "연속 읽기 실패 한계를 넘어 해당 pending 권한을 격리했습니다. 정상으로 "
            "판정한 것이 아니며 원본 트랜스크립트 점검이 필요합니다."
        )
    sample = "\n".join(f"- `{path}`" for path in paths[:3])
    if len(paths) > 3:
        sample += f"\n- 외 {len(paths) - 3}개"
    rt.alert(
        ch,
        f"🚨 **{title}**\n\n{detail}\n\n{sample}\n\n"
        f"런타임: {rt.dcserver_snapshot()}",
    )
    channel_state[LAST_PENDING_TRANSCRIPT_RETIREMENT_ALERT_KEY] = now
    return True


def _clear_gap_alert_without_recovery(
    rt: Runtime,
    channel_state: dict[str, Any],
    channel_id: str,
    authority_paths: list[str],
) -> bool:
    retired = set(authority_paths)
    gap_path = channel_state.get(GAP_TRANSCRIPT_KEY)
    previous_owners = _validated_gap_owner_transcripts(channel_state)
    retained_owners = [path for path in previous_owners if path not in retired]
    gap_state_open = bool(
        previous_owners
        or channel_state.get("alerting")
        or channel_state.get("gap_since")
        or channel_state.get("issue_url")
    )
    if not retained_owners and gap_state_open:
        # Legacy singleton state may not yet have the owner set.  Preserve any
        # still-live selected/pending authority until this tick evaluates it;
        # only an explicit OK verdict may claim recovery.
        selected = channel_state.get(SELECTED_TRANSCRIPT_KEY)
        fallback = (
            [selected] if isinstance(selected, str) and selected else []
        ) + _validated_pending_transcripts(channel_state)
        retained_owners = [path for path in fallback if path not in retired]
    retained_owners = _store_gap_owner_transcripts(
        channel_state, retained_owners
    )
    if retained_owners:
        if gap_path not in retained_owners:
            selected = channel_state.get(SELECTED_TRANSCRIPT_KEY)
            channel_state[GAP_TRANSCRIPT_KEY] = (
                selected if selected in retained_owners else retained_owners[0]
            )
        rt.log(
            f"[{channel_id}] unrelated transcript retirement preserved live "
            f"gap authority owners={len(retained_owners)}"
        )
        return False
    if channel_state.get("alerting"):
        rt.log(
            f"[{channel_id}] alert state transitioned to unresolved transcript "
            "escalation — no clean recovery claimed"
        )
    channel_state.pop("alerting", None)
    channel_state.pop("gap_since", None)
    channel_state.pop("issue_url", None)
    channel_state.pop(GAP_TRANSCRIPT_KEY, None)
    channel_state.pop(GAP_OWNER_TRANSCRIPTS_KEY, None)
    return True


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
    project_root = projects_root()
    dirs = channel_project_dirs(project_root, pattern)
    candidates = transcript_candidates(dirs)
    discovered_paths = {str(candidate.path) for candidate in candidates}
    pending_paths = _validated_pending_transcripts(chs)
    protected_guard_paths = set(pending_paths) | set(
        _validated_gap_owner_transcripts(chs)
    )
    recovered_gap_guards, reclaimed_guard_paths = _refresh_recovered_gap_guards(
        _validated_recovered_gap_guards(chs),
        discovered_paths,
        protected_guard_paths,
        project_root,
        pattern,
        now,
    )
    _store_recovered_gap_guards(chs, recovered_gap_guards)
    if reclaimed_guard_paths:
        _forget_reclaimed_recovered_gap_lifecycles(chs, reclaimed_guard_paths)
        rt.log(
            f"[{cid}] recovered-gap-guard-reclaimed "
            f"count={len(reclaimed_guard_paths)}"
        )
    previous_sizes = _validated_transcript_sizes(chs)
    previous_seen_at = _validated_transcript_seen_at(chs, previous_sizes, now)
    known_state_persisted = isinstance(chs.get(TRANSCRIPT_KNOWN_AT_KEY), dict)
    known_at = _validated_transcript_known_at(chs, now)
    pending_paths = _validated_pending_transcripts(chs)
    pending_failures = _validated_pending_failures(chs, pending_paths)
    pending_since = _validated_pending_since(chs, pending_paths, now)
    retired_transcripts = _validated_retired_transcripts(chs)
    read_cache: dict[str, TranscriptReadResult] = {}

    def read_candidate(candidate: TranscriptCandidate) -> TranscriptReadResult:
        path = str(candidate.path)
        if path not in read_cache:
            read_cache[path] = assistant_blocks(candidate.path, project_root)
        return read_cache[path]

    reactivated_paths: list[str] = []
    for candidate in candidates:
        path = str(candidate.path)
        retired = retired_transcripts.get(path)
        if retired is None or candidate.size <= retired[0]:
            continue
        read_result = read_candidate(candidate)
        if (
            read_result.error is None
            and not read_result.incomplete_tail
            and read_result.semantic_end_offset > retired[0]
        ):
            retired_transcripts.pop(path, None)
            reactivated_paths.append(path)
            rt.log(f"[{cid}] transcript-retired-reactivated path={path}")
    _store_retired_transcripts(chs, retired_transcripts)
    retired_paths = set(retired_transcripts)
    selectable_candidates = [
        candidate
        for candidate in candidates
        if str(candidate.path) not in retired_paths
    ]
    previous_selected = chs.get(SELECTED_TRANSCRIPT_KEY)
    persisted_selected = previous_selected
    watermarks = delivered_watermarks(chs)
    known_before = (
        set(known_at)
        | set(previous_sizes)
        | set(watermarks)
        | set(recovered_gap_guards)
        | retired_paths
    )

    tracking_initialized = bool(
        previous_sizes
        or pending_paths
        or watermarks
        or recovered_gap_guards
        or retired_transcripts
        or (isinstance(previous_selected, str) and previous_selected)
    )
    # Upgrade boundary: pre-known_at state already has a selected/size/
    # watermark authority.  Its first post-upgrade unseen path is still a true
    # first observation; otherwise an unreadable debut is labelled
    # unproven_stale once, persisted as known, then skipped forever.
    known_state_initialized = known_state_persisted or tracking_initialized
    bootstrapped_from_watermark = False
    candidate_paths = {str(candidate.path) for candidate in candidates}
    selectable_candidate_paths = {
        str(candidate.path) for candidate in selectable_candidates
    }
    if (
        not isinstance(previous_selected, str)
        or previous_selected not in selectable_candidate_paths
    ):
        rechecked = (
            None
            if isinstance(previous_selected, str)
            and previous_selected in retired_paths
            else recheck_selected_transcript(
                previous_selected,
                project_root,
                pattern,
                set(previous_sizes) | set(watermarks) | set(recovered_gap_guards),
            )
        )
        if rechecked is not None:
            candidates.append(rechecked)
            selectable_candidates.append(rechecked)
            candidate_paths.add(str(rechecked.path))
            selectable_candidate_paths.add(str(rechecked.path))
            rt.log(f"[{cid}] transcript-recheck recovered path={rechecked.path}")
    pending_paths = [
        path
        for path in pending_paths
        if path in candidate_paths
        or now
        - (
            previous_seen_at[path]
            if path in previous_seen_at
            else pending_since.get(path, 0.0)
        )
        <= TRANSCRIPT_HISTORY_TTL_SECS
    ]
    tracked_anchor_missing = (
        isinstance(persisted_selected, str)
        and persisted_selected in known_before
        and persisted_selected not in selectable_candidate_paths
    )
    if (
        not isinstance(previous_selected, str)
        or not previous_selected
        or previous_selected in retired_paths
        or (
            selectable_candidate_paths
            and previous_selected not in selectable_candidate_paths
        )
    ):
        previous_selected = None
    selection_sizes = dict(previous_sizes)
    for path, (size, _, _, _) in recovered_gap_guards.items():
        selection_sizes.setdefault(path, size)
    live_debut_paths: list[str] = []
    if tracking_initialized:
        debut_candidates = sorted(
            (
                candidate
                for candidate in selectable_candidates
                if str(candidate.path) not in selection_sizes
            ),
            key=lambda candidate: (-candidate.mtime, str(candidate.path)),
        )
        for candidate in debut_candidates:
            path = str(candidate.path)
            idle = now - candidate.mtime
            if idle >= cfg.idle_quiet_secs:
                rt.log(
                    f"[{cid}] transcript-debut-skip reason=idle "
                    f"idle_min={int(min(idle, 86400 * 365) // 60)} path={path}"
                )
                selection_sizes[path] = candidate.size
                continue
            read_result = read_candidate(candidate)
            content_is_recent = any(
                now - epoch < cfg.idle_quiet_secs
                for epoch, _ in read_result.blocks
            )
            first_observation = known_state_initialized and path not in known_before
            first_observation_without_readable_history = first_observation and (
                read_result.error is not None or not read_result.blocks
            )
            if not content_is_recent and not first_observation_without_readable_history:
                reason = (
                    "known_stale_content"
                    if path in known_before
                    else "unproven_stale_content"
                )
                rt.log(f"[{cid}] transcript-debut-skip reason={reason} path={path}")
                selection_sizes[path] = candidate.size
                continue
            if first_observation_without_readable_history:
                selection_sizes[path] = candidate.size
            live_debut_paths.append(path)
        live_debut_set = set(live_debut_paths)
        pending_paths = live_debut_paths + [
            path for path in pending_paths if path not in live_debut_set
        ]
        for path in live_debut_paths:
            pending_since.setdefault(path, now)
    if reactivated_paths:
        reactivated_set = set(reactivated_paths)
        pending_paths = reactivated_paths + [
            path for path in pending_paths if path not in reactivated_set
        ]
        for path in reactivated_paths:
            pending_failures.pop(path, None)
            pending_since[path] = now
    watermarked_candidates = [
        (watermarks[str(candidate.path)][1], str(candidate.path))
        for candidate in selectable_candidates
        if str(candidate.path) in watermarks
    ]
    if previous_selected is None and watermarked_candidates:
        all_candidates_watermarked = len(watermarked_candidates) == len(
            selectable_candidates
        )
        if all_candidates_watermarked or tracked_anchor_missing:
            previous_selected = max(watermarked_candidates)[1]
            bootstrapped_from_watermark = True
    if bootstrapped_from_watermark:
        for candidate in selectable_candidates:
            path = str(candidate.path)
            if path in watermarks:
                selection_sizes.setdefault(path, candidate.size)
    semantic_growth_paths: set[str] = set()
    for candidate in selectable_candidates:
        path = str(candidate.path)
        guard = recovered_gap_guards.get(path)
        prior_size = guard[0] if guard is not None else previous_sizes.get(path)
        if prior_size is None or candidate.size <= prior_size:
            continue
        read_result = read_candidate(candidate)
        if (
            read_result.error is None
            and not read_result.incomplete_tail
            and read_result.semantic_end_offset > prior_size
        ):
            semantic_growth_paths.add(path)
        elif read_result.error is None and not read_result.incomplete_tail:
            if guard is not None:
                _, confirmed_at, last_seen_at, absent_since = guard
                recovered_gap_guards[path] = (
                    max(prior_size, read_result.observed_size),
                    confirmed_at,
                    last_seen_at,
                    absent_since,
                )
            rt.log(
                f"[{cid}] transcript-growth-ignored reason=non-semantic "
                f"path={path}"
            )
    _store_recovered_gap_guards(chs, recovered_gap_guards)
    tr, selection_reason = select_watch_transcript_with_reason(
        selectable_candidates,
        selection_sizes,
        previous_selected,
        semantic_growth_paths,
    )
    if bootstrapped_from_watermark and selection_reason == "sticky":
        selection_reason = (
            "watermark_anchor_recovery"
            if tracked_anchor_missing
            else "watermark_bootstrap"
        )
    if tr is not None:
        chs[SELECTED_TRANSCRIPT_KEY] = str(tr)
    rt.log(f"[{cid}] transcript-select reason={selection_reason} path={tr}")
    merged_sizes = dict(previous_sizes)
    merged_seen_at = dict(previous_seen_at)
    for candidate in candidates:
        path = str(candidate.path)
        read_result = read_cache.get(path)
        if read_result is not None and (
            read_result.error is not None or read_result.incomplete_tail
        ):
            # A torn/unreadable path has not established a trustworthy growth
            # baseline.  Preserve the last complete baseline so continuous raw
            # bytes cannot reset pending age or manufacture later activity.
            continue
        merged_sizes[path] = (
            read_result.observed_size
            if read_result is not None
            else candidate.size
        )
        merged_seen_at[path] = now
    priority_paths = (
        ([str(tr)] if tr is not None else [])
        + (
            [previous_selected]
            if isinstance(previous_selected, str) and previous_selected
            else []
        )
        + pending_paths
        + list(recovered_gap_guards)
        + [
            str(candidate.path)
            for candidate in sorted(
                candidates,
                key=lambda candidate: (-candidate.mtime, str(candidate.path)),
            )
        ]
        + [
            path
            for path, _ in sorted(
                watermarks.items(), key=lambda item: (-item[1][1], item[0])
            )
        ]
    )
    merged_known_at = dict(known_at)
    for candidate in candidates:
        merged_known_at[str(candidate.path)] = now
    merged_known_at = _bounded_transcript_known_at(
        merged_known_at, now, priority_paths
    )
    merged_sizes, merged_seen_at = _bounded_transcript_history(
        merged_sizes, merged_seen_at, now, priority_paths
    )
    bounded_pending_paths = _bounded_pending_transcripts(
        pending_paths,
        set(merged_sizes) | candidate_paths | set(pending_since),
    )
    bounded_pending_set = set(bounded_pending_paths)
    dropped_pending_paths = [
        path for path in pending_paths if path not in bounded_pending_set
    ]
    if dropped_pending_paths:
        chs[PENDING_TRANSCRIPT_OVERFLOW_KEY] = {
            "at": now,
            "dropped": len(dropped_pending_paths),
            "kept": len(bounded_pending_paths),
        }
        rt.log(
            f"[{cid}] transcript-debut-overflow "
            f"kept={len(bounded_pending_paths)} "
            f"dropped={len(dropped_pending_paths)}"
        )
        last_overflow_alert = chs.get(
            LAST_PENDING_TRANSCRIPT_OVERFLOW_ALERT_KEY, 0.0
        )
        if not _is_finite_nonnegative_number(last_overflow_alert):
            last_overflow_alert = 0.0
        if now - float(last_overflow_alert) >= cfg.realert_secs:
            rt.alert(
                ch,
                "🚨 **릴레이 트랜스크립트 평가 큐 포화**\n\n"
                f"한 번의 감시 틱에서 보존 가능한 신규 트랜스크립트 "
                f"**{len(bounded_pending_paths)}개**를 초과해 "
                f"**{len(dropped_pending_paths)}개**의 평가 권한을 유지하지 "
                "못했습니다. 최신 후보를 우선 보존했지만 평가 커버리지가 "
                "불완전하므로 정상 상태로 간주할 수 없습니다.\n\n"
                f"런타임: {rt.dcserver_snapshot()}",
            )
            chs[LAST_PENDING_TRANSCRIPT_OVERFLOW_ALERT_KEY] = now
    else:
        chs.pop(PENDING_TRANSCRIPT_OVERFLOW_KEY, None)
    pending_paths = bounded_pending_paths
    failure_authorities = _read_failure_authority_paths(chs, pending_paths)
    pending_failures = {
        path: failures
        for path, failures in pending_failures.items()
        if path in failure_authorities
    }
    pending_since = {
        path: pending_since.get(path, now) for path in pending_paths
    }
    for path in pending_paths:
        if path in semantic_growth_paths:
            pending_since[path] = now
    chs[TRANSCRIPT_SIZES_KEY] = merged_sizes
    chs[TRANSCRIPT_SEEN_AT_KEY] = merged_seen_at
    chs[TRANSCRIPT_KNOWN_AT_KEY] = merged_known_at
    chs[PENDING_TRANSCRIPTS_KEY] = pending_paths
    if pending_failures:
        chs[PENDING_TRANSCRIPT_FAILURES_KEY] = pending_failures
    else:
        chs.pop(PENDING_TRANSCRIPT_FAILURES_KEY, None)
    if pending_since:
        chs[PENDING_TRANSCRIPT_SINCE_KEY] = pending_since
    else:
        chs.pop(PENDING_TRANSCRIPT_SINCE_KEY, None)
    candidate_by_path = {str(candidate.path): candidate for candidate in candidates}
    expired_pending_paths: list[str] = []
    for path in pending_paths:
        candidate = candidate_by_path.get(path)
        last_activity = (
            candidate.mtime
            if candidate is not None
            else max(
                previous_seen_at.get(path, 0.0),
                pending_since.get(path, now),
            )
        )
        pending_age = now - pending_since.get(path, now)
        if (
            now - last_activity >= cfg.idle_quiet_secs
            or pending_age >= cfg.idle_quiet_secs
        ):
            expired_pending_paths.append(path)
    if expired_pending_paths:
        expired = set(expired_pending_paths)
        for path in expired_pending_paths:
            candidate = candidate_by_path.get(path)
            size = candidate.size if candidate is not None else merged_sizes.get(path)
            if isinstance(size, int) and not isinstance(size, bool) and size >= 0:
                retired_transcripts[path] = (size, now)
        _store_retired_transcripts(chs, retired_transcripts)
        pending_paths = [path for path in pending_paths if path not in expired]
        pending_failures = {
            path: failures
            for path, failures in pending_failures.items()
            if path not in expired
        }
        pending_since = {
            path: since
            for path, since in pending_since.items()
            if path not in expired
        }
        chs[PENDING_TRANSCRIPTS_KEY] = pending_paths
        if pending_failures:
            chs[PENDING_TRANSCRIPT_FAILURES_KEY] = pending_failures
        else:
            chs.pop(PENDING_TRANSCRIPT_FAILURES_KEY, None)
        if pending_since:
            chs[PENDING_TRANSCRIPT_SINCE_KEY] = pending_since
        else:
            chs.pop(PENDING_TRANSCRIPT_SINCE_KEY, None)
        rt.log(
            f"[{cid}] transcript-pending-expired count={len(expired_pending_paths)}"
        )
        _alert_pending_retirement(
            rt, ch, chs, expired_pending_paths, now, reason="idle"
        )
        _clear_gap_alert_without_recovery(
            rt, chs, cid, expired_pending_paths
        )
        if tr is not None and str(tr) in expired:
            chs.pop(SELECTED_TRANSCRIPT_KEY, None)
            tr = None
    retired_pending_paths = list(expired_pending_paths)
    selected = next((candidate for candidate in candidates if candidate.path == tr), None)
    # I1 selector sync (#4408 phase 2): compare the dcserver's asserted relay
    # bind (B) against F. Parallel to gap/coverage — its own cooldown key, wrapped
    # so it can never short-circuit or suppress the gap verdict below.
    f_growing = (
        selected is not None
        and str(selected.path) in semantic_growth_paths
    )
    try:
        tick_selector_sync(rt, ch, chs, tr, f_growing, now)
    except Exception as e:  # noqa: BLE001 — selector sync must never suppress gap checks
        rt.log(f"[{cid}] selector-sync tick error: {type(e).__name__}: {e}")

    pending_set = set(pending_paths)
    gap_owner_paths = _validated_gap_owner_transcripts(chs)
    gap_owner_set = set(gap_owner_paths)
    recovered_guard_paths = list(_validated_recovered_gap_guards(chs))
    recovered_guard_set = set(recovered_guard_paths)
    evaluation_candidates: list[TranscriptCandidate] = []
    if selected is not None:
        evaluation_candidates.append(selected)
    growing_guard_paths = [
        path for path in recovered_guard_paths if path in semantic_growth_paths
    ]
    for path in [*pending_paths, *gap_owner_paths, *growing_guard_paths]:
        candidate = candidate_by_path.get(path)
        if candidate is not None and candidate not in evaluation_candidates:
            evaluation_candidates.append(candidate)
    active_candidates: list[TranscriptCandidate] = []
    for candidate in evaluation_candidates:
        path = str(candidate.path)
        idle = now - candidate.mtime
        if (
            path not in pending_set
            and path not in gap_owner_set
            and path not in recovered_guard_set
            and idle >= cfg.idle_quiet_secs
        ):
            rt.log(
                f"[{cid}] idle {int(min(idle, 86400 * 365) // 60)}m "
                f"path={path} — no live session, skipping"
            )
            continue
        active_candidates.append(candidate)
    if not active_candidates:
        if retired_pending_paths:
            _clear_gap_alert_without_recovery(
                rt, chs, cid, retired_pending_paths
            )
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

    evaluated: list[tuple[TranscriptCandidate, Verdict]] = []
    fresh_undelivered_by_path: dict[str, int] = {}
    unreadable_paths: list[str] = []
    escalated_pending_paths: list[str] = []
    remaining_pending = list(pending_paths)
    for candidate in active_candidates:
        path = str(candidate.path)
        read_result = read_candidate(candidate)
        if read_result.error is not None or read_result.incomplete_tail:
            if read_result.error is not None:
                rt.log(
                    f"[{cid}] transcript-read-error path={path} "
                    f"error={read_result.error}"
                )
            else:
                rt.log(f"[{cid}] transcript-read-incomplete path={path}")
            owns_read_authority = (
                path in pending_set
                or path in gap_owner_set
                or path in recovered_guard_set
                or (selected is not None and candidate.path == selected.path)
            )
            if owns_read_authority:
                if path not in pending_set and path not in pending_failures:
                    rt.log(
                        f"[{cid}] transcript-selected-read-failure-tracked "
                        f"path={path}"
                    )
                failures = pending_failures.get(path, 0) + 1
                pending_failures[path] = failures
                if failures >= cfg.read_fail_alert_after:
                    remaining_pending = [
                        pending
                        for pending in remaining_pending
                        if pending != path
                    ]
                    pending_failures.pop(path, None)
                    escalated_pending_paths.append(path)
                    continue
            unreadable_paths.append(path)
            continue
        pending_failures.pop(path, None)
        prior_delivered_ts = delivered_watermark_for_path(chs, candidate.path)
        verdict = evaluate(
            read_result.blocks,
            hay,
            now,
            cfg.grace_secs,
            cfg.gap_alert_secs,
            prior_delivered_ts,
        )
        if verdict.delivered_ts > prior_delivered_ts:
            advance_delivered_watermark(
                chs, candidate.path, verdict.delivered_ts, now
            )
        fresh_undelivered = sum(
            1
            for epoch, text in read_result.blocks
            if now - epoch <= cfg.grace_secs
            and epoch > verdict.delivered_ts
            and not delivered(text, hay)
        )
        fresh_undelivered_by_path[path] = fresh_undelivered
        if path in pending_set:
            rt.log(
                f"[{cid}] transcript-debut-eval path={path} "
                f"state={verdict.state} lost={verdict.lost} "
                f"fresh_undelivered={fresh_undelivered}"
            )
            if (
                verdict.state == STATE_OK
                and fresh_undelivered == 0
                and read_result.blocks
            ):
                remaining_pending = [
                    pending for pending in remaining_pending if pending != path
                ]
        evaluated.append((candidate, verdict))
    remaining_pending = _bounded_pending_transcripts(
        remaining_pending,
        set(merged_sizes) | candidate_paths | set(pending_since),
    )
    chs[PENDING_TRANSCRIPTS_KEY] = remaining_pending
    failure_authorities = _read_failure_authority_paths(chs, remaining_pending)
    pending_failures = {
        path: failures
        for path, failures in pending_failures.items()
        if path in failure_authorities
    }
    pending_since = {
        path: pending_since.get(path, now) for path in remaining_pending
    }
    if pending_failures:
        chs[PENDING_TRANSCRIPT_FAILURES_KEY] = pending_failures
    else:
        chs.pop(PENDING_TRANSCRIPT_FAILURES_KEY, None)
    if pending_since:
        chs[PENDING_TRANSCRIPT_SINCE_KEY] = pending_since
    else:
        chs.pop(PENDING_TRANSCRIPT_SINCE_KEY, None)
    if escalated_pending_paths:
        retired_pending_paths.extend(escalated_pending_paths)
        escalated = set(escalated_pending_paths)
        for path in escalated_pending_paths:
            candidate = candidate_by_path.get(path)
            size = candidate.size if candidate is not None else merged_sizes.get(path)
            if isinstance(size, int) and not isinstance(size, bool) and size >= 0:
                retired_transcripts[path] = (size, now)
        _store_retired_transcripts(chs, retired_transcripts)
        rt.log(
            f"[{cid}] transcript-pending-escalated "
            f"count={len(escalated_pending_paths)}"
        )
        _alert_pending_retirement(
            rt,
            ch,
            chs,
            escalated_pending_paths,
            now,
            reason="read_failure",
        )
        _clear_gap_alert_without_recovery(
            rt, chs, cid, escalated_pending_paths
        )
        if tr is not None and str(tr) in escalated:
            chs.pop(SELECTED_TRANSCRIPT_KEY, None)
            tr = None
    if not evaluated:
        if retired_pending_paths:
            _clear_gap_alert_without_recovery(
                rt, chs, cid, retired_pending_paths
            )
        return

    # A cleared/restarted provider session creates a new transcript. Retire an
    # old GAP owner only when dcserver proves that the channel is now bound to the
    # delivered successor and the old file has crossed the normal idle boundary.
    # File mtime ordering alone is not a session boundary: concurrent writers and
    # delayed flushes can invert it. This is a retirement, not proof that the old
    # LOST blocks arrived, so the normal RECOVERED notification stays suppressed.
    selected_path = str(selected.path) if selected is not None else None
    selected_verdict = next(
        (
            verdict
            for candidate, verdict in evaluated
            if str(candidate.path) == selected_path
        ),
        None,
    )
    selected_probe = rt.watcher_state(cid) if selected_path else WatcherStateProbe(None)
    selected_session_id = (
        canonical_session_uuid(selected.path.stem) if selected is not None else None
    )
    successor_binding_proven = selected_probe.status == 200 and (
        selected_probe.bound_output_path == selected_path
        or (
            selected_session_id is not None
            and selected_probe.bound_session_id == selected_session_id
        )
    )
    superseded_gap_owners: list[str] = []
    if (
        selected_path
        and selected_verdict is not None
        and selected_verdict.state == STATE_OK
        and fresh_undelivered_by_path.get(selected_path, 0) == 0
        and delivered_watermark_for_path(chs, selected_path) > 0
        and successor_binding_proven
    ):
        for path in _validated_gap_owner_transcripts(chs):
            if path == selected_path or path in semantic_growth_paths:
                continue
            candidate = candidate_by_path.get(path)
            if candidate is None or now - candidate.mtime < cfg.idle_quiet_secs:
                continue
            superseded_gap_owners.append(path)
            retired_transcripts[path] = (candidate.size, now)
        if superseded_gap_owners:
            superseded = set(superseded_gap_owners)
            _store_retired_transcripts(chs, retired_transcripts)
            retired_pending_paths.extend(superseded_gap_owners)
            remaining_pending = [
                path for path in remaining_pending if path not in superseded
            ]
            pending_failures = {
                path: failures
                for path, failures in pending_failures.items()
                if path not in superseded
            }
            pending_since = {
                path: since
                for path, since in pending_since.items()
                if path not in superseded
            }
            chs[PENDING_TRANSCRIPTS_KEY] = remaining_pending
            if pending_failures:
                chs[PENDING_TRANSCRIPT_FAILURES_KEY] = pending_failures
            else:
                chs.pop(PENDING_TRANSCRIPT_FAILURES_KEY, None)
            if pending_since:
                chs[PENDING_TRANSCRIPT_SINCE_KEY] = pending_since
            else:
                chs.pop(PENDING_TRANSCRIPT_SINCE_KEY, None)
            evaluated = [
                (candidate, verdict)
                for candidate, verdict in evaluated
                if str(candidate.path) not in superseded
            ]
            rt.log(
                f"[{cid}] historical-gap-owner-retired "
                f"successor={selected_path} count={len(superseded_gap_owners)}"
            )

    state_rank = {STATE_OK: 0, STATE_LAGGING: 1, STATE_GAP: 2}
    verdict_candidate, v = max(
        evaluated,
        key=lambda item: (
            state_rank[item[1].state],
            item[1].gap_secs,
            item[1].lost,
            item[0].mtime,
            str(item[0].path),
        ),
    )
    verdict_path = str(verdict_candidate.path)
    previous_gap_owners = _validated_gap_owner_transcripts(chs)
    evaluated_paths = {str(candidate.path) for candidate, _ in evaluated}
    incident_open = bool(
        previous_gap_owners
        or chs.get("alerting")
        or chs.get("gap_since")
        or chs.get("issue_url")
    )
    recovered_gap_guards = _validated_recovered_gap_guards(chs)
    rejected_recoveries: list[str] = []
    for candidate, verdict in evaluated:
        path = str(candidate.path)
        owns_guard = path in recovered_gap_guards
        recovering_gap_owner = path in previous_gap_owners
        if verdict.state != STATE_OK or not (owns_guard or recovering_gap_owner):
            continue
        if fresh_undelivered_by_path.get(path, 0) > 0:
            if recovering_gap_owner:
                rejected_recoveries.append(path)
            continue
        if delivered_watermark_for_path(chs, path) <= 0:
            if recovering_gap_owner:
                rejected_recoveries.append(path)
            continue
        recovered_gap_guards, admitted = _upsert_recovered_gap_guard(
            recovered_gap_guards,
            path,
            read_cache[path].observed_size,
            now,
        )
        if not admitted and recovering_gap_owner:
            rejected_recoveries.append(path)
    _store_recovered_gap_guards(chs, recovered_gap_guards)

    next_gap_owners = [
        path
        for path in previous_gap_owners
        if path not in evaluated_paths and path not in superseded_gap_owners
    ]
    for candidate, verdict in evaluated:
        path = str(candidate.path)
        unresolved = verdict.state == STATE_GAP or (
            incident_open and verdict.state == STATE_LAGGING
        )
        if unresolved and path not in next_gap_owners:
            next_gap_owners.append(path)
    for path in rejected_recoveries:
        if path not in next_gap_owners:
            next_gap_owners.append(path)
    if rejected_recoveries:
        rt.log(
            f"[{cid}] recovered-gap-guard-capacity-blocked "
            f"count={len(rejected_recoveries)} — recovery remains open"
        )
    next_gap_owners = _store_gap_owner_transcripts(chs, next_gap_owners)
    if next_gap_owners and chs.get(GAP_TRANSCRIPT_KEY) not in next_gap_owners:
        chs[GAP_TRANSCRIPT_KEY] = (
            verdict_path if verdict_path in next_gap_owners else next_gap_owners[0]
        )
    if unreadable_paths and v.state == STATE_OK:
        rt.log(
            f"[{cid}] transcript-verdict-incomplete "
            f"read_errors={len(unreadable_paths)}"
        )
        return
    if retired_pending_paths and v.state == STATE_OK:
        if not next_gap_owners:
            if chs.get("alerting"):
                rt.log(
                    f"[{cid}] alert state transitioned to unresolved transcript "
                    "escalation — no clean recovery claimed"
                )
            chs.pop("alerting", None)
            chs.pop("gap_since", None)
            chs.pop("issue_url", None)
            chs.pop(GAP_TRANSCRIPT_KEY, None)
            chs.pop(GAP_OWNER_TRANSCRIPTS_KEY, None)
        rt.log(
            f"[{cid}] transcript-verdict-unresolved-retirement "
            f"retired={len(retired_pending_paths)}"
        )
        return
    if next_gap_owners and v.state == STATE_OK:
        rt.log(
            f"[{cid}] transcript-verdict-incomplete "
            f"unresolved_gap_owners={len(next_gap_owners)}"
        )
        return

    if v.state == STATE_GAP:
        chs[GAP_TRANSCRIPT_KEY] = verdict_path
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
            rt.log(
                f"[{cid}] ALERT path={verdict_path} lost={v.lost} gap_min={gap_min}"
            )
        else:
            rt.log(
                f"[{cid}] gap persists path={verdict_path} lost={v.lost} "
                "(alert suppressed, cooldown)"
            )
    elif v.state == STATE_LAGGING:
        rt.log(
            f"[{cid}] lagging path={verdict_path} lost={v.lost} "
            f"gap={int(v.gap_secs)}s "
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
        chs.pop(GAP_TRANSCRIPT_KEY, None)
        chs.pop(GAP_OWNER_TRANSCRIPTS_KEY, None)
        rt.log(
            f"[{cid}] ok path={verdict_path} blocks={v.blocks} "
            f"stale={v.stale} lost=0"
        )


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
