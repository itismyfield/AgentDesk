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
import hashlib
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
# A foreground turn must show an outbound/relay write inside this window before
# a transient watcher-state desync can be treated as covered.  Ten minutes
# matches the watchdog's calibrated delivery grace while still letting a
# genuinely stalled foreground turn resume normal two-tick escalation.
COVERAGE_ACTIVITY_FRESH_SECS = 10 * 60
COVERAGE_INFLIGHT_UPDATED_AT_KEY = "coverage_inflight_updated_at"

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
# Per-path timestamp of the FIRST tick on which the owning worktree directory
# was proven absent. A dead-session retirement is admitted only after the
# absence has held continuously for DEAD_WORKTREE_CONFIRM_SECS.
DEAD_WORKTREE_ABSENT_SINCE_KEY = "dead_worktree_absent_since"
MAX_DEAD_WORKTREE_ABSENCES = 64
# 600s is five consecutive polls at the shipped Config.poll_secs default of 120
# (this file, `poll_secs: int = 120`). `git worktree remove`/`add` completes in
# seconds, so no legitimate worktree churn can stay invisible across five polls;
# a transient stat failure resets the window instead of accumulating.
DEAD_WORKTREE_CONFIRM_SECS = 600
DEAD_WORKTREE_RETIREMENT_REASON = "dead_worktree"
# #5190. Per-path timestamp of the FIRST tick on which an ORPHAN transcript (not
# the channel's active session, never observed delivering) was seen holding
# stranded blocks while another transcript on the same channel was demonstrably
# still delivering. That co-occurrence is the only positive evidence that the
# blocks are unrecoverable rather than symptoms of a live relay outage.
ORPHAN_STRANDED_SINCE_KEY = "orphan_stranded_since"
ORPHAN_STRANDED_RETIREMENT_REASON = "orphan_stranded"
# Continuous corroboration required before a stranded-orphan marker may close
# the authority. Same shape and rationale as DEAD_WORKTREE_CONFIRM_SECS: five
# consecutive polls at the shipped poll_secs default, so one anomalous tick can
# never retire anything.
ORPHAN_STRANDED_CONFIRM_SECS = 600
# An orphan WITH delivery history carries a real, measured gap, so silencing it
# on the never-observed evidence bar would risk burying an outage this watchdog
# exists to report (#5190 R2). It is admitted only on a strictly stronger bar:
# this multiple of the freeze floor, plus a delivery frontier that has itself
# been stale for a full freeze floor, plus dcserver independently confirming the
# channel is now bound to a DIFFERENT session.
ORPHAN_OBSERVED_FREEZE_MULTIPLIER = 2
# `Verdict.gap_secs` when NO delivery was ever matched for a path. It is not a
# duration and must never be rendered as one (#5190/#5052): "never observed"
# and "infinitely stale" are different claims, and `float("inf")` collapsed
# them into one that auto-passed every threshold comparison.
GAP_SECS_UNOBSERVED = -1.0
RECOVERED_GAP_GUARDS_KEY = "recovered_gap_replay_guards"
ISSUE_FILING_SUPPRESSION_REASON_KEY = "issue_filing_suppression_reason"
ISSUE_FILING_SUPPRESSION_SINCE_KEY = "issue_filing_suppression_since"
ISSUE_FILING_REACHABLE_TICKS_KEY = "issue_filing_reachable_ticks"
ISSUE_FILING_REACHABLE_TICKS_REQUIRED = 2
ISSUE_FILING_DC_UNREACHABLE_REASON = "dcserver_transport_unreachable"
LOSS_OBSERVATIONS_KEY = "permanent_loss_observations"
PERMANENT_LOSS_TOMBSTONES_KEY = "permanent_loss_tombstones"
PERMANENT_LOSS_UNANNOUNCED_KEY = "permanent_loss_unannounced"
PERMANENT_LOSS_TOTAL_KEY = "permanent_loss_total"
PERMANENT_LOSS_SUSPECTED_KEY = "permanent_loss_suspected"
PERMANENT_LOSS_OVERFLOW_TOTAL_KEY = "permanent_loss_overflow_total"
PERMANENT_LOSS_IDENTITY_WARNING_KEY = "permanent_loss_identity_warnings"
PERMANENT_LOSS_CORRUPTION_WARNING_KEY = "permanent_loss_corruption_warnings"
LAST_ACTUAL_DELIVERY_AT_KEY = "last_actual_delivery_at"
LAST_ACTUAL_DELIVERY_BY_PATH_KEY = "last_actual_delivery_by_path"
# Permanent loss requires two different delivered-frontier advances beyond the
# candidate. Re-reading the same bounded Discord window never adds evidence.
PERMANENT_LOSS_CONFIRM_ADVANCES = 2
MAX_LOSS_OBSERVATIONS = 256
MAX_PERMANENT_LOSS_TOMBSTONES = 256
MAX_TRANSCRIPT_HISTORY = 64
MAX_KNOWN_TRANSCRIPTS = 256
MAX_PENDING_TRANSCRIPTS = 32
MAX_GAP_OWNER_TRANSCRIPTS = MAX_PENDING_TRANSCRIPTS + 1
# Stranded-orphan markers are a subset of the GAP owners, so they inherit that
# budget and add no new delivery authority (#5190).
MAX_ORPHAN_STRANDED_PATHS = MAX_GAP_OWNER_TRANSCRIPTS
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
    # #5190. How long a non-selected transcript must sit frozen before its
    # undelivered blocks may be classified as stranded rather than as an active
    # relay failure. This floor is what separates #5190's dead `/clear`ed
    # session from #4435's freshly swapped-in session whose blocks are being
    # lost right now: both are unmatched and non-selected, and only elapsed
    # silence tells them apart. Two full re-alert cycles (`realert_secs` 900s),
    # so a live session between turns can never be mistaken for an abandoned
    # one. Shorter than `idle_quiet_secs` because classification additionally
    # demands positive proof that the channel is still delivering elsewhere.
    orphan_abandon_secs: int = 1800
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
        "orphan_abandon_secs",
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


def worktree_dir_for_transcript(
    transcript_path: str, worktree_root: str, pattern: re.Pattern[str]
) -> Path | None:
    """Reverse a transcript's project-dir slug back to its owning worktree dir.

    Session transcripts live at ``<projects_root>/<slug>/<uuid>.jsonl`` where
    ``slug == project_slug(worktree_dir)``.  ``project_slug`` is lossy in
    general — both ``/`` and ``.`` collapse to ``-`` — so an arbitrary slug
    cannot be inverted.  This channel's dirs are not arbitrary:
    ``main_channel_project_re`` admits only
    ``<slug(root)>-<prefix>-<YYYYMMDD>-<HHMMSS>``, and the tail after
    ``slug(root)`` contains neither ``/`` nor ``.`` by construction.  For a
    pattern-matching dir the inverse is therefore exact, and the result is
    always a direct child of ``worktree_root``.

    Returns None when the slug does not belong to this channel, so a caller can
    never draw a liveness conclusion from a path it cannot resolve.
    """
    parent = Path(transcript_path).parent.name
    if pattern.fullmatch(parent) is None:
        return None
    root = worktree_root.rstrip("/")
    slug_prefix = project_slug(root) + "-"
    if not parent.startswith(slug_prefix):
        return None
    basename = parent[len(slug_prefix):]
    if not basename or "/" in basename:
        return None
    return Path(root) / basename


def directory_presence(path: Path) -> bool | None:
    """True = directory exists, False = proven absent, None = no verdict.

    Only ENOENT/ENOTDIR count as proof of absence.  Every other error
    (permissions, I/O, a dismounted volume) is deliberately inconclusive: a
    liveness predicate that reads "unreadable" as "dead" would retire a live
    session's loss-state, which is the exact failure this file must not have.
    """
    try:
        return stat_mode.S_ISDIR(os.stat(path).st_mode)
    except (FileNotFoundError, NotADirectoryError):
        return False
    except (OSError, ValueError):
        return None


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
    block_source_ids: list[str] = field(default_factory=list)
    identity_fallbacks: int = 0


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


def gap_is_unobserved(verdict: "Verdict") -> bool:
    """True when no delivery was ever matched for this path (#5190).

    The verdict then carries no measurement at all: its `gap_secs` is the
    `GAP_SECS_UNOBSERVED` sentinel, not an elapsed time. Callers that render a
    duration, rank gaps against each other, or weigh evidence must branch here
    first.
    """
    return verdict.gap_secs < 0.0


def _validated_orphan_stranded_since(
    channel_state: Mapping[str, Any],
) -> dict[str, float]:
    """Return the bounded path -> first-stranded-tick map (#5190)."""
    raw = channel_state.get(ORPHAN_STRANDED_SINCE_KEY, {})
    if not isinstance(raw, dict):
        return {}
    stranded: dict[str, float] = {}
    for path, since in raw.items():
        if len(stranded) >= MAX_ORPHAN_STRANDED_PATHS:
            break
        if not isinstance(path, str) or not path:
            continue
        if not _is_finite_nonnegative_number(since):
            continue
        stranded[path] = float(since)
    return stranded


def _store_orphan_stranded_since(
    channel_state: dict[str, Any],
    stranded: Mapping[str, Any],
    *,
    released: set[str] | None = None,
) -> dict[str, float]:
    """Persist the retained stranded-marker set through its deletion gate.

    The currently enumerated marker exits (authority retirement and a marker
    finding being voided) supply `released` here; cap eviction and malformed
    entry cleanup also happen here.  This is a convention, not structural
    encapsulation: `channel_state` remains a mutable dict, so a future direct
    writer can bypass the gate.  Audits must therefore continue to enumerate
    writes to `ORPHAN_STRANDED_SINCE_KEY` and calls to this helper.

    Equal timestamps are retained by ascending path.  Consequently, if more
    than the cap open on the same tick, lexicographically larger paths (often
    larger UUIDs) lose the tie even when one of them is the actual orphan.
    """
    released = released or set()
    # Entries without a finite nonnegative timestamp provide no reliable age
    # for eviction, so discard them instead of letting them consume the bounded
    # marker budget. Keep the newest valid markers: if a just-opened marker is
    # dropped, this tick may suppress its alert while a later tick has no marker
    # from which to satisfy `orphan_matured`, leaving a quiet persistent gap
    # that is harder to observe than the alerting form.
    valid = [
        (path, float(since))
        for path, since in stranded.items()
        if isinstance(path, str)
        and path
        and path not in released
        and _is_finite_nonnegative_number(since)
    ]
    bounded = dict(
        sorted(valid, key=lambda item: (-item[1], item[0]))[
            :MAX_ORPHAN_STRANDED_PATHS
        ]
    )
    if bounded:
        channel_state[ORPHAN_STRANDED_SINCE_KEY] = bounded
    else:
        channel_state.pop(ORPHAN_STRANDED_SINCE_KEY, None)
    return bounded


def _release_pending_authority(
    channel_state: dict[str, Any],
    remaining_pending: list[str],
    pending_failures: dict[str, int],
    pending_since: dict[str, float],
    stranded_since: dict[str, float],
    released: set[str],
) -> tuple[list[str], dict[str, int], dict[str, float], dict[str, float]]:
    """Drop released paths from pending and stranded authority in one step.

    The pending maps and stranded marker are related lifecycle records, so a
    partial release can leave retired authority occupying a bounded store.
    Callers that couple release to an external side effect (a retirement notice
    that actually left the box) can place this single state transition after it.

    Besides removing `released`, this validates the entire retained stranded
    map, orders it newest-first with a path tie-break, applies its cap, and
    persists that bounded result.  The returned stranded map is exactly the
    persisted retained map, not merely the caller's input minus `released`.
    """
    remaining_pending = [
        path for path in remaining_pending if path not in released
    ]
    pending_failures = {
        path: failures
        for path, failures in pending_failures.items()
        if path not in released
    }
    pending_since = {
        path: since
        for path, since in pending_since.items()
        if path not in released
    }
    channel_state[PENDING_TRANSCRIPTS_KEY] = remaining_pending
    if pending_failures:
        channel_state[PENDING_TRANSCRIPT_FAILURES_KEY] = pending_failures
    else:
        channel_state.pop(PENDING_TRANSCRIPT_FAILURES_KEY, None)
    if pending_since:
        channel_state[PENDING_TRANSCRIPT_SINCE_KEY] = pending_since
    else:
        channel_state.pop(PENDING_TRANSCRIPT_SINCE_KEY, None)
    stranded_since = _store_orphan_stranded_since(
        channel_state, stranded_since, released=released
    )
    return remaining_pending, pending_failures, pending_since, stranded_since


def _orphan_authority_matured(
    channel_state: Mapping[str, Any], path: str, now: float
) -> bool:
    """True once a stranded-orphan marker has held long enough to close out.

    The marker itself carries the evidence — it is only written on a tick where
    the path was already frozen past its freeze floor AND the channel was
    provably delivering on another transcript — so this only asks whether that
    finding has survived the confirmation window. Reading the persisted marker
    means the answer is available before any of this tick's verdicts exist,
    which is what lets the pending authority be released in the same pass that
    evaluates it.
    """
    since = _validated_orphan_stranded_since(channel_state).get(path)
    if since is None:
        return False
    return now - since >= ORPHAN_STRANDED_CONFIRM_SECS


def _channel_has_live_delivery(
    evaluated: list[tuple[TranscriptCandidate, Verdict]],
    fresh_undelivered_by_path: Mapping[str, int],
    current_delivered_by_path: Mapping[str, float],
    exclude_path: str,
    now: float,
    gap_alert_secs: int,
    newer_than: float = 0.0,
) -> bool:
    """Is some OTHER transcript on this channel demonstrably still delivering?

    This is the whole safety hinge of the #5190 orphan handling, so it demands
    positive proof rather than the absence of a complaint: a sibling path with a
    clean verdict, nothing fresh outstanding, and a recent block of its own
    matched in THIS tick's haystack.

    That last clause is why `Verdict.delivered_ts` cannot be the evidence
    (#5190 R4): it is `max(persisted watermark, current match)`, so an idle
    transcript that delivered ten minutes ago and matches nothing now still
    carried a fresh-looking timestamp — a stale watermark voting as if it were a
    live delivery, for up to `gap_alert_secs`. `current_delivered_by_path`
    carries only what this tick actually matched. When the relay is genuinely
    down nothing matches, this returns False, and the gap alert proceeds
    untouched.

    KNOWN LIMIT of the `gap_alert_secs` bound (#5190 R3 P2-D). The timestamps in
    `current_delivered_by_path` are block WRITE epochs, not delivery times. A
    match proves the block was delivered somewhere in `[epoch, now]`, so the
    bound really says "a delivery happened within the last `gap_alert_secs`" —
    NOT "a delivery is happening now". An idle sibling whose last block was
    written `gap_alert_secs - 1` ago and is still inside the bounded Discord
    read window therefore keeps voting "alive" for that whole span, even if the
    relay died immediately after. Nothing in the haystack carries a Discord
    message timestamp, so this cannot be tightened here.

    `newer_than` is how the caller closes that window when it matters: passing
    the tick a finding was first made requires a match whose block was WRITTEN
    after that moment, which no relay that was already dead could have produced.
    It is a lower bound on the evidence, not a replacement for the freshness
    bound above — both must hold.
    """
    for candidate, verdict in evaluated:
        path = str(candidate.path)
        if path == exclude_path:
            continue
        if verdict.state != STATE_OK:
            continue
        if fresh_undelivered_by_path.get(path, 0) > 0:
            continue
        matched_ts = current_delivered_by_path.get(path, 0.0)
        if matched_ts <= 0.0:
            continue
        if matched_ts <= newer_than:
            continue
        if now - matched_ts > gap_alert_secs:
            continue
        return True
    return False


def _confirmed_gap_minutes(
    verdict: Verdict, last_actual_delivery_at: float, now: float
) -> int | None:
    """Minutes since the last CONFIRMED delivery, or None when there is none.

    None means "no delivery is on record for this session" — a fact the caller
    has to state in words. The old code substituted `999` here and the alert
    rendered it as "999 minutes since the last delivery" (#5190 §2): a sentinel
    wearing the clothes of a measurement, which then propagated into the title
    of the issue the watchdog filed about itself.
    """
    if last_actual_delivery_at:
        return int(max(0.0, now - last_actual_delivery_at) // 60)
    if gap_is_unobserved(verdict) or not math.isfinite(verdict.gap_secs):
        return None
    return int(verdict.gap_secs // 60)


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


def _validated_worktree_absences(
    channel_state: Mapping[str, Any],
) -> dict[str, float]:
    """Return the persisted first-absence timestamps, malformed rows dropped.

    A malformed row must never read as "absent since the epoch": that would
    grant an instant retirement. Dropping it restarts the dwell instead.
    """
    raw = channel_state.get(DEAD_WORKTREE_ABSENT_SINCE_KEY, {})
    if not isinstance(raw, dict):
        return {}
    valid: dict[str, float] = {}
    for path, since in raw.items():
        if not isinstance(path, str) or not path:
            continue
        if _is_finite_nonnegative_number(since):
            valid[path] = float(since)
    return dict(sorted(valid.items(), key=lambda item: (item[1], item[0]))[
        :MAX_DEAD_WORKTREE_ABSENCES
    ])


def _store_worktree_absences(
    channel_state: dict[str, Any], absences: Mapping[str, float]
) -> None:
    bounded = dict(
        sorted(absences.items(), key=lambda item: (item[1], item[0]))[
            :MAX_DEAD_WORKTREE_ABSENCES
        ]
    )
    if bounded:
        channel_state[DEAD_WORKTREE_ABSENT_SINCE_KEY] = bounded
    else:
        channel_state.pop(DEAD_WORKTREE_ABSENT_SINCE_KEY, None)


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


def parse_local_timestamp(ts: object) -> float | None:
    """Parse dcserver's local-time ``YYYY-MM-DD HH:MM:SS`` timestamps."""
    if not isinstance(ts, str):
        return None
    try:
        return float(time.mktime(time.strptime(ts, "%Y-%m-%d %H:%M:%S")))
    except (ValueError, OverflowError):
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


def _assistant_blocks_from_record(
    record: object,
) -> list[tuple[float, str]]:
    if (
        not isinstance(record, dict)
        or record.get("type") != "assistant"
        or is_harness_control_assistant_record(record)
    ):
        return []
    epoch = parse_transcript_ts(record.get("timestamp", ""))
    if epoch is None:
        return []
    message = record.get("message")
    if not isinstance(message, dict):
        return []
    out: list[tuple[float, str]] = []
    for content in message.get("content") or []:
        if isinstance(content, dict) and content.get("type") == "text":
            text = (content.get("text") or "").strip()
            if text:
                out.append((epoch, text))
    return out


def assistant_blocks_from_lines(lines) -> list[tuple[float, str]]:
    """(epoch, text) for every assistant text block in a transcript's lines."""
    out: list[tuple[float, str]] = []
    for line in lines:
        try:
            record = json.loads(line)
        except (json.JSONDecodeError, TypeError):
            continue
        out.extend(_assistant_blocks_from_record(record))
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
            block_source_ids: list[str] = []
            identity_fallbacks = 0
            semantic_end_offset = 0
            byte_offset = 0
            for raw_line, line in zip(raw_lines, lines):
                line_start_offset = byte_offset
                try:
                    record = json.loads(line)
                except (json.JSONDecodeError, TypeError):
                    record = None
                line_blocks = _assistant_blocks_from_record(record)
                blocks.extend(line_blocks)
                record_uuid = record.get("uuid") if isinstance(record, dict) else None
                if isinstance(record_uuid, str) and record_uuid:
                    block_source_ids.extend(
                        f"uuid:{record_uuid}:{index}"
                        for index in range(len(line_blocks))
                    )
                else:
                    block_source_ids.extend(
                        f"offset:{line_start_offset + index}"
                        for index in range(len(line_blocks))
                    )
                    identity_fallbacks += len(line_blocks)
                byte_offset += len(raw_line)
                if line_blocks:
                    semantic_end_offset = byte_offset
            return TranscriptReadResult(
                blocks,
                incomplete_tail=incomplete_tail,
                semantic_end_offset=semantic_end_offset,
                observed_size=byte_offset,
                block_source_ids=block_source_ids,
                identity_fallbacks=identity_fallbacks,
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
    """Whether at least one delivery probe is present in the bounded haystack."""
    n = norm(text)
    if len(n) < 60:
        return n in hay
    probes = [n[:60], n[len(n) // 2 : len(n) // 2 + 50], n[-60:]]
    return any(p and p in hay for p in probes)


def delivered_flags(blocks: list[tuple[float, str]], hay: str) -> list[bool]:
    """Cardinality-aware matching for duplicate source text.

    Identical source blocks consume distinct occurrences of their strongest
    delivery probe. This prevents one Discord copy from satisfying two source
    obligations. Ambiguous duplicates remain active and are never tombstoned.
    """
    capacities: dict[str, int] = {}
    consumed: dict[str, int] = {}
    flags: list[bool] = []
    for _, text in blocks:
        normalized = norm(text)
        if normalized not in capacities:
            probes = (
                [normalized]
                if len(normalized) < 60
                else [
                    normalized[:60],
                    normalized[len(normalized) // 2 : len(normalized) // 2 + 50],
                    normalized[-60:],
                ]
            )
            capacities[normalized] = max(
                (hay.count(probe) for probe in probes if probe), default=0
            )
        used = consumed.get(normalized, 0)
        flags.append(used < capacities[normalized])
        consumed[normalized] = used + 1
    return flags


@dataclass(frozen=True)
class Verdict:
    state: str
    blocks: int
    stale: int
    lost: int
    delivered_ts: float
    # `GAP_SECS_UNOBSERVED` when no delivery was ever matched for this path.
    # Read it through `gap_is_unobserved()`; never render it as a duration.
    gap_secs: float


@dataclass(frozen=True)
class PermanentLossUpdate:
    active_blocks: list[tuple[float, str]]
    newly_tombstoned: tuple[str, ...]
    retracted: tuple[str, ...]
    suspected: int = 0
    overflowed: int = 0
    corrupted: bool = False


def _assistant_block_id(
    transcript: str | Path, epoch: float, text: str, source_id: str
) -> str:
    """Stable identity from a record UUID plus its text-block index."""
    digest = hashlib.sha256(norm(text).encode("utf-8")).hexdigest()
    identity = f"{transcript}\0{source_id}\0{epoch:.6f}\0{digest}"
    return hashlib.sha256(identity.encode("utf-8")).hexdigest()


def _bounded_loss_entries(
    entries: Mapping[str, dict[str, Any]], limit: int
) -> dict[str, dict[str, Any]]:
    ordered = sorted(
        entries.items(),
        key=lambda item: (
            -float(item[1].get("last_observed_at", item[1].get("confirmed_at", 0.0))),
            item[0],
        ),
    )
    return dict(ordered[:limit])


def _validated_loss_observations_with_status(
    channel_state: Mapping[str, Any], now: float | None = None
) -> tuple[dict[str, dict[str, Any]], bool]:
    raw = channel_state.get(LOSS_OBSERVATIONS_KEY, {})
    if not isinstance(raw, dict):
        return {}, LOSS_OBSERVATIONS_KEY in channel_state
    valid: dict[str, dict[str, Any]] = {}
    corrupted = False
    for block_id, entry in raw.items():
        advances = entry.get("evidence_frontiers") if isinstance(entry, dict) else None
        if not (
            isinstance(block_id, str)
            and len(block_id) == 64
            and isinstance(entry, dict)
            and isinstance(entry.get("path"), str)
            and entry.get("path")
            and _is_finite_nonnegative_number(entry.get("epoch"))
            and _is_finite_nonnegative_number(entry.get("last_observed_at"))
            and isinstance(advances, list)
            and all(_is_finite_nonnegative_number(value) for value in advances)
        ):
            corrupted = True
            continue
        last_observed_at = float(entry["last_observed_at"])
        if now is not None and now - last_observed_at > TRANSCRIPT_HISTORY_TTL_SECS:
            continue
        distinct = sorted({float(value) for value in advances})[-PERMANENT_LOSS_CONFIRM_ADVANCES:]
        if not distinct:
            corrupted = True
            continue
        valid[block_id] = {
            "path": entry["path"],
            "epoch": float(entry["epoch"]),
            "evidence_frontiers": distinct,
            "last_observed_at": last_observed_at,
        }
    return _bounded_loss_entries(valid, MAX_LOSS_OBSERVATIONS), corrupted


def _validated_loss_observations(
    channel_state: Mapping[str, Any], now: float | None = None
) -> dict[str, dict[str, Any]]:
    return _validated_loss_observations_with_status(channel_state, now)[0]


def _permanent_loss_tombstones_with_status(
    channel_state: Mapping[str, Any], now: float | None = None
) -> tuple[dict[str, dict[str, Any]], bool]:
    raw = channel_state.get(PERMANENT_LOSS_TOMBSTONES_KEY, {})
    if not isinstance(raw, dict):
        return {}, PERMANENT_LOSS_TOMBSTONES_KEY in channel_state
    valid: dict[str, dict[str, Any]] = {}
    corrupted = False
    for block_id, entry in raw.items():
        if not (
            isinstance(block_id, str)
            and len(block_id) == 64
            and isinstance(entry, dict)
            and isinstance(entry.get("path"), str)
            and entry.get("path")
            and _is_finite_nonnegative_number(entry.get("epoch"))
            and _is_finite_nonnegative_number(entry.get("confirmed_at"))
        ):
            corrupted = True
            continue
        confirmed_at = float(entry["confirmed_at"])
        if now is not None and now - confirmed_at > TRANSCRIPT_HISTORY_TTL_SECS:
            continue
        valid[block_id] = {
            "path": entry["path"],
            "epoch": float(entry["epoch"]),
            "confirmed_at": confirmed_at,
            "last_observed_at": confirmed_at,
        }
    bounded = _bounded_loss_entries(valid, MAX_PERMANENT_LOSS_TOMBSTONES)
    for entry in bounded.values():
        entry.pop("last_observed_at", None)
    return bounded, corrupted


def permanent_loss_tombstones(
    channel_state: Mapping[str, Any], now: float | None = None
) -> dict[str, dict[str, Any]]:
    """Return bounded durable loss identities; malformed state fails open."""
    return _permanent_loss_tombstones_with_status(channel_state, now)[0]


def permanent_loss_total(channel_state: Mapping[str, Any]) -> int:
    """Current retained losses, retaining the last projection on corruption."""
    tombstones, corrupted = _permanent_loss_tombstones_with_status(channel_state)
    if corrupted:
        projected = channel_state.get(PERMANENT_LOSS_TOTAL_KEY, 0)
        if isinstance(projected, int) and not isinstance(projected, bool) and projected >= 0:
            return projected
    return len(tombstones)


def _store_loss_state(
    channel_state: dict[str, Any],
    observations: Mapping[str, dict[str, Any]],
    tombstones: Mapping[str, dict[str, Any]],
) -> None:
    if observations:
        channel_state[LOSS_OBSERVATIONS_KEY] = dict(observations)
    else:
        channel_state.pop(LOSS_OBSERVATIONS_KEY, None)
    if tombstones:
        channel_state[PERMANENT_LOSS_TOMBSTONES_KEY] = dict(tombstones)
    else:
        channel_state.pop(PERMANENT_LOSS_TOMBSTONES_KEY, None)


def update_permanent_loss_tombstones(
    channel_state: dict[str, Any],
    transcript: str | Path,
    blocks: list[tuple[float, str]],
    block_source_ids: list[str],
    hay: str,
    now: float,
    grace_secs: int,
    prior_delivered_ts: float,
    current_delivered_ts: float,
) -> PermanentLossUpdate:
    """Confirm only skips proven by distinct delivered-frontier advances.

    A candidate must be newer than the already-confirmed frontier. Each evidence
    point is a different later source timestamp that newly advances that frontier;
    repeated reads of one bounded Discord window cannot confirm a loss. A later
    match retracts a tombstone and decrements the cumulative total.
    """
    path = str(transcript)
    tombstones, tombstones_corrupted = _permanent_loss_tombstones_with_status(
        channel_state, now
    )
    observations, observations_corrupted = _validated_loss_observations_with_status(
        channel_state, now
    )
    corrupted = tombstones_corrupted or observations_corrupted
    durable_tombstone_ids = set(tombstones)
    if len(block_source_ids) != len(blocks):
        raise ValueError("block_source_ids must identify every source block")
    block_ids = [
        _assistant_block_id(path, epoch, text, source_id)
        for (epoch, text), source_id in zip(blocks, block_source_ids)
    ]
    matched = delivered_flags(blocks, hay)

    retracted: list[str] = []
    for block_id, is_matched in zip(block_ids, matched):
        if block_id in tombstones and is_matched:
            tombstones.pop(block_id, None)
            observations.pop(block_id, None)
            retracted.append(block_id)

    frontier_advanced = current_delivered_ts > prior_delivered_ts
    newly_tombstoned: list[str] = []
    suspected = 0
    for index, ((epoch, _), block_id) in enumerate(zip(blocks, block_ids)):
        if block_id in tombstones or matched[index]:
            observations.pop(block_id, None)
            continue
        prior = observations.get(block_id)
        eligible = now - epoch > grace_secs and (
            epoch > prior_delivered_ts or prior is not None
        )
        later_advance = frontier_advanced and current_delivered_ts > epoch
        if not (eligible and later_advance):
            if prior is not None and now - epoch > grace_secs:
                suspected += 1
            continue
        frontiers = set(prior.get("evidence_frontiers", [])) if prior else set()
        frontiers.add(current_delivered_ts)
        evidence = sorted(frontiers)[-PERMANENT_LOSS_CONFIRM_ADVANCES:]
        if len(evidence) >= PERMANENT_LOSS_CONFIRM_ADVANCES:
            tombstones[block_id] = {
                "path": path,
                "epoch": epoch,
                "confirmed_at": now,
            }
            observations.pop(block_id, None)
            newly_tombstoned.append(block_id)
        else:
            observations[block_id] = {
                "path": path,
                "epoch": epoch,
                "evidence_frontiers": evidence,
                "last_observed_at": now,
            }
            suspected += 1

    observation_overflow = max(0, len(observations) - MAX_LOSS_OBSERVATIONS)
    tombstone_overflow = max(0, len(tombstones) - MAX_PERMANENT_LOSS_TOMBSTONES)
    overflowed = observation_overflow + tombstone_overflow
    observations = _bounded_loss_entries(observations, MAX_LOSS_OBSERVATIONS)
    tombstones = _bounded_loss_entries(tombstones, MAX_PERMANENT_LOSS_TOMBSTONES)
    active_blocks = [
        block for block, identity in zip(blocks, block_ids) if identity not in tombstones
    ]
    if not corrupted:
        _store_loss_state(channel_state, observations, tombstones)
        channel_state[PERMANENT_LOSS_TOTAL_KEY] = len(tombstones)
    else:
        active_blocks = [
            block
            for block, identity in zip(blocks, block_ids)
            if identity not in durable_tombstone_ids
        ]
        newly_tombstoned.clear()
        retracted.clear()
    if overflowed and not corrupted:
        previous_overflow = channel_state.get(PERMANENT_LOSS_OVERFLOW_TOTAL_KEY, 0)
        if not isinstance(previous_overflow, int) or isinstance(previous_overflow, bool):
            previous_overflow = 0
        channel_state[PERMANENT_LOSS_OVERFLOW_TOTAL_KEY] = previous_overflow + overflowed
    return PermanentLossUpdate(
        active_blocks=active_blocks,
        newly_tombstoned=tuple(newly_tombstoned),
        retracted=tuple(retracted),
        suspected=suspected,
        overflowed=overflowed,
        corrupted=corrupted,
    )


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
class CoverageTranscriptProbe:
    """Independent selected-transcript evidence for a desync judgment."""

    growing: bool
    blocks: int = 0
    lost: int = 0


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
    # Local-time bridge heartbeat copied from the watcher-state snapshot. Missing
    # or malformed values remain None so coverage corroboration fails closed.
    inflight_updated_at: float | None = None
    response_malformed: bool = False


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
        return WatcherStateProbe(200, response_malformed=True)

    attached_raw = payload.get("attached")
    desynced_raw = payload.get("desynced")
    bound_output_path = payload.get("bound_output_path")
    bound_session_id_raw = payload.get("bound_session_id")
    attached = attached_raw if isinstance(attached_raw, bool) else None
    desynced = desynced_raw if isinstance(desynced_raw, bool) else None
    response_malformed = not isinstance(attached_raw, bool) or not isinstance(
        desynced_raw, bool
    )
    bound = bound_output_path if isinstance(bound_output_path, str) else None
    bound_session_id = canonical_session_uuid(bound_session_id_raw)
    inflight_updated_at = parse_local_timestamp(payload.get("inflight_updated_at"))

    has_activity_schema = (
        "relay_stall_state" in payload or "relay_health" in payload
    )
    if not has_activity_schema:
        return WatcherStateProbe(
            200,
            attached,
            desynced,
            bound,
            bound_session_id=bound_session_id,
            inflight_updated_at=inflight_updated_at,
            response_malformed=response_malformed,
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
            inflight_updated_at,
            True,
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
        inflight_updated_at,
        response_malformed or malformed,
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
    matches = delivered_flags(blocks, hay)
    current_delivered_ts = max(
        (epoch for (epoch, _), matched in zip(blocks, matches) if matched),
        default=0.0,
    )
    # Discord reads are bounded. Absence from today's haystack cannot erase the
    # health watermark, but source classification remains independent: an older
    # unmatched block is ignored only after durable skip/tombstone confirmation.
    delivered_ts = max(prior, current_delivered_ts)
    stale = [
        ((epoch, text), matched)
        for (epoch, text), matched in zip(blocks, matches)
        if now - epoch > grace_secs
    ]
    lost = [
        block
        for block, matched in stale
        if block[0] > delivered_ts and not matched
    ]
    # #5190/#5052: "this path never delivered anything we could match" is
    # UNKNOWN, not "the gap is infinitely old". `float("inf")` collapsed the two
    # so an orphan transcript — a `/clear`ed session that can never deliver
    # again — auto-passed the threshold comparison AND outranked every real,
    # measured gap in the verdict ordering below. The unobserved case still
    # reaches STATE_GAP on its own explicit branch (a relay that died before it
    # ever delivered must alert); what it no longer does is masquerade as the
    # oldest measurement on the channel.
    delivered_observed = delivered_ts > 0.0
    gap_secs = (
        (now - delivered_ts) if delivered_observed else GAP_SECS_UNOBSERVED
    )
    if lost and (not delivered_observed or gap_secs > gap_alert_secs):
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
    channel_state: dict[str, Any],
    paths: list[str],
    *,
    loss_state_corrupted: bool,
) -> None:
    """Drop the replay records enumerated below after one full absent TTL."""
    reclaimed = set(paths)
    if not reclaimed:
        return

    pending = _validated_pending_transcripts(channel_state)
    raw_failures = channel_state.get(PENDING_TRANSCRIPT_FAILURES_KEY, {})
    raw_since = channel_state.get(PENDING_TRANSCRIPT_SINCE_KEY, {})
    _release_pending_authority(
        channel_state,
        pending,
        dict(raw_failures) if isinstance(raw_failures, dict) else {},
        dict(raw_since) if isinstance(raw_since, dict) else {},
        _validated_orphan_stranded_since(channel_state),
        reclaimed,
    )

    selected = channel_state.get(SELECTED_TRANSCRIPT_KEY)
    if isinstance(selected, str) and selected in reclaimed:
        channel_state.pop(SELECTED_TRANSCRIPT_KEY, None)

    for key in (
        TRANSCRIPT_SIZES_KEY,
        TRANSCRIPT_SEEN_AT_KEY,
        TRANSCRIPT_KNOWN_AT_KEY,
        RETIRED_TRANSCRIPTS_KEY,
        DEAD_WORKTREE_ABSENT_SINCE_KEY,
    ):
        raw = channel_state.get(key)
        if not isinstance(raw, dict):
            continue
        retained = {path: entry for path, entry in raw.items() if path not in reclaimed}
        if retained:
            channel_state[key] = retained
        else:
            channel_state.pop(key, None)

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

    if not loss_state_corrupted:
        observations = {
            block_id: entry
            for block_id, entry in _validated_loss_observations(channel_state).items()
            if entry["path"] not in reclaimed
        }
        tombstones = {
            block_id: entry
            for block_id, entry in permanent_loss_tombstones(channel_state).items()
            if entry["path"] not in reclaimed
        }
        _store_loss_state(channel_state, observations, tombstones)
    raw_actual = channel_state.get(LAST_ACTUAL_DELIVERY_BY_PATH_KEY)
    if isinstance(raw_actual, dict):
        retained_actual = {
            path: observed_at
            for path, observed_at in raw_actual.items()
            if path not in reclaimed
        }
        if retained_actual:
            channel_state[LAST_ACTUAL_DELIVERY_BY_PATH_KEY] = retained_actual
        else:
            channel_state.pop(LAST_ACTUAL_DELIVERY_BY_PATH_KEY, None)


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
            return WatcherStateProbe(200, response_malformed=True)
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

    def alert(self, ch: ChannelConfig, body: str, trigger_turn: bool = True) -> bool:
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
                    return True
                self.log(
                    f"announce bot failed rc={p.returncode}: "
                    f"{(p.stderr or p.stdout)[:160]!r}; falling back"
                )
            except (OSError, subprocess.SubprocessError) as e:
                self.log(f"announce bot error: {e}; falling back")
        elif trigger_turn:
            # #5155 correction: with `announce_to` empty the primary is never
            # ATTEMPTED, so the log shows only discord-sendmessage successes and
            # an operator counting them reads a healthy config as a 100% primary
            # failure rate. Say which of the two it is, on every alert.
            self.log(
                "announce bot skipped — announce_to empty for channel "
                f"{ch.channel_id}; posting via discord-sendmessage (bot token)"
            )
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
            if p.returncode == 0:
                self.log("alert delivered via discord-sendmessage rc=0")
                return True
            self.log(
                f"discord-sendmessage failed rc={p.returncode}: "
                f"{(p.stderr or p.stdout)[:160]!r}"
            )
        except (OSError, subprocess.SubprocessError) as e:
            self.log(f"discord-sendmessage error: {e}")
        return False

    def file_github_issue(
        self, ch: ChannelConfig, gap_min: int | None, lost: int
    ) -> str:
        """Auto-file a GitHub issue for a persistent gap (06-29 relay-gap-watch
        behavior, see #3893). Best-effort: failure is logged, never fatal.

        `gap_min is None` means no delivery was ever confirmed for the session.
        It is reported as that sentence, never as a number: #5190 was filed by
        this method with `999m since last delivery` in its own title, and the
        false measurement survived into human triage.
        """
        gh = shutil.which("gh") or "/opt/homebrew/bin/gh"
        elapsed_phrase = (
            f"{gap_min}m since last delivery"
            if gap_min is not None
            else "no confirmed delivery on record for this session"
        )
        title = (
            f"[auto][relay-watchdog] relay gap on channel {ch.channel_id}: "
            f"{lost} undelivered blocks, {elapsed_phrase}"
        )
        elapsed_line = (
            f"- minutes since last successful delivery: **{gap_min}**\n"
            if gap_min is not None
            else "- last successful delivery: **none on record for this "
            "session**\n"
        )
        body = (
            f"Filed automatically by the out-of-band relay watchdog (#4381).\n\n"
            f"- channel: `{ch.channel_id}`\n"
            f"- undelivered assistant blocks: **{lost}**\n"
            f"{elapsed_line}"
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
    transcript_probe: CoverageTranscriptProbe | None = None,
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
    previous_inflight_updated_at = chs.get(COVERAGE_INFLIGHT_UPDATED_AT_KEY)
    current_inflight_updated_at = probe.inflight_updated_at
    inflight_update_advanced = bool(
        _is_finite_nonnegative_number(previous_inflight_updated_at)
        and current_inflight_updated_at is not None
        and current_inflight_updated_at > float(previous_inflight_updated_at)
    )
    inflight_update_recent = bool(
        current_inflight_updated_at is not None
        and 0
        <= now - current_inflight_updated_at
        < COVERAGE_ACTIVITY_FRESH_SECS
    )
    if current_inflight_updated_at is not None:
        chs[COVERAGE_INFLIGHT_UPDATED_AT_KEY] = current_inflight_updated_at

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
    # #4504/#4841: attached_but_desynced is a watcher-state desync, not proof
    # of delivery loss. Alarm only with a corroborating real delivery gap;
    # duration alone never upgrades an idle zero-loss session. detached /
    # watcher_state_404 keep the 2-tick behavior (different reason strings).
    if verdict.reason == "attached_but_desynced":
        activity = probe.relay_activity
        relay_ts = activity.last_relay_ts_ms if activity is not None else None
        recent_relay = False
        if (
            activity is not None
            and not activity.malformed
            and isinstance(relay_ts, int)
            and not isinstance(relay_ts, bool)
            and relay_ts > 0
        ):
            relay_age_ms = int(now * 1000) - relay_ts
            recent_relay = (
                0 <= relay_age_ms < COVERAGE_ACTIVITY_FRESH_SECS * 1000
            )
        zero_loss_observed = bool(
            transcript_probe is not None
            and transcript_probe.blocks > 0
            and transcript_probe.lost == 0
        )
        growing_without_loss = bool(
            zero_loss_observed and transcript_probe is not None and transcript_probe.growing
        )
        inflight_progress_alive = bool(
            inflight_update_advanced or inflight_update_recent
        )
        growing_relay_stall = bool(
            growing_without_loss and not recent_relay and not inflight_progress_alive
        )
        delivery_gap_active = bool(
            chs.get("gap_since")
            or chs.get("alerting")
            or growing_relay_stall
        )
        if (
            not delivery_gap_active
            and growing_without_loss
            and not recent_relay
            and inflight_progress_alive
        ):
            evidence = "advanced" if inflight_update_advanced else "recent"
            rt.log(
                f"[{cid}] coverage desync growth has live inflight update "
                f"evidence={evidence} — not alarming"
            )
            return
        if not delivery_gap_active and zero_loss_observed and recent_relay:
            rt.log(
                f"[{cid}] coverage desync has zero loss and recent relay "
                f"blocks={transcript_probe.blocks} growing={transcript_probe.growing} "
                "— not alarming"
            )
            return
        if not delivery_gap_active:
            desync_for = (
                max(0.0, now - desync_since) if desync_since is not None else 0.0
            )
            rt.log(
                f"[{cid}] coverage desync uncorroborated "
                f"duration={int(desync_for)}s — not alarming"
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
) -> str:
    """Send a retirement notice; report WHY it did or did not go out.

    #5190 R3 P2-E: this used to answer a bare bool, and every caller read the
    False as "nobody could be told". Two very different things produce it. A
    send failure means the notice never left the box. A cooldown hit means the
    box is fine and something else — possibly an unrelated `idle` or
    `read_failure` retirement on the SAME shared cooldown key — spoke within
    `realert_secs`. Callers gate identically on both (neither is a notice about
    THESE paths), but the log line must not call a cooldown an undelivered
    message: that is a false statement about the relay's health, which is the
    exact defect class this campaign keeps closing.

    Returns one of: `"sent"`, `"cooldown"`, `"undelivered"`, `"empty"`. Only
    `"sent"` means someone was told about `paths`.
    """
    if not paths:
        return "empty"
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
        return "cooldown"
    if reason == "idle":
        title = "릴레이 트랜스크립트 평가 권한 만료"
        detail = (
            "세션 활동이 idle 한계를 넘겨 더 이상 live GAP으로 반복 평가하지 "
            "않습니다. 미도달 여부가 해결됐다고 주장하는 복구 알림은 보내지 않습니다."
        )
    elif reason == ORPHAN_STRANDED_RETIREMENT_REASON:
        title = "고아 세션의 미도달 블록 회수 불가 확정"
        detail = (
            "현재 활성 세션이 아닌 과거 세션의 트랜스크립트가 계속 정지해 있는 "
            "동안 이 채널의 다른 세션은 정상 배달을 이어갔습니다 — 릴레이 장애가 "
            "아니라 그 세션이 다시 배달할 수 없는 상태입니다. 해당 미도달 블록을 "
            "회수 불가로 확정하고 반복 평가에서 내립니다. 배달이 확인됐다는 뜻이 "
            "아니며(복구 알림은 보내지 않습니다), 이 경보가 해당 경로에 대한 "
            "마지막 통지입니다."
        )
    elif reason == DEAD_WORKTREE_RETIREMENT_REASON:
        title = "죽은 세션의 릴레이 loss-state 종결"
        detail = (
            "해당 트랜스크립트를 소유한 워크트리 디렉터리가 사라졌습니다 — 그 "
            "세션은 다시 배달할 수 없으므로 미도달 블록을 재평가 대상에서 "
            "내립니다. 이는 배달이 확인됐다는 뜻이 아니며(복구 알림은 보내지 "
            "않습니다), 이 경보가 해당 경로에 대한 마지막 통지입니다."
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
    delivered_notice = rt.alert(
        ch,
        f"🚨 **{title}**\n\n{detail}\n\n{sample}\n\n"
        f"런타임: {rt.dcserver_snapshot()}",
    )
    if not delivered_notice:
        # Do not burn the cooldown on a notice that never left the box: the
        # next tick must be free to retry. Callers that gate termination on this
        # return value therefore keep the authority open until someone is told.
        rt.log(
            f"[{ch.channel_id}] transcript-retirement-alert undelivered "
            f"reason={reason} count={len(paths)}"
        )
        return "undelivered"
    channel_state[LAST_PENDING_TRANSCRIPT_RETIREMENT_ALERT_KEY] = now
    return "sent"


def _reset_issue_filing_suppression(channel_state: dict[str, Any]) -> None:
    channel_state.pop(ISSUE_FILING_SUPPRESSION_REASON_KEY, None)
    channel_state.pop(ISSUE_FILING_SUPPRESSION_SINCE_KEY, None)
    channel_state.pop(ISSUE_FILING_REACHABLE_TICKS_KEY, None)


def _issue_filing_stable(
    channel_state: dict[str, Any], probe: WatcherStateProbe, now: float
) -> bool:
    """Defer filing only for transport-level dcserver unreachability."""
    suppression_active = (
        channel_state.get(ISSUE_FILING_SUPPRESSION_REASON_KEY)
        == ISSUE_FILING_DC_UNREACHABLE_REASON
        and _is_finite_nonnegative_number(
            channel_state.get(ISSUE_FILING_SUPPRESSION_SINCE_KEY)
        )
    )
    if not suppression_active:
        _reset_issue_filing_suppression(channel_state)

    if probe.status is None:
        if not suppression_active:
            channel_state[ISSUE_FILING_SUPPRESSION_REASON_KEY] = (
                ISSUE_FILING_DC_UNREACHABLE_REASON
            )
            channel_state[ISSUE_FILING_SUPPRESSION_SINCE_KEY] = now
        channel_state[ISSUE_FILING_REACHABLE_TICKS_KEY] = 0
        return False

    if probe.status in (200, 404) and not probe.response_malformed:
        if not suppression_active:
            return True
        raw_ticks = channel_state.get(ISSUE_FILING_REACHABLE_TICKS_KEY, 0)
        ticks = (
            raw_ticks
            if isinstance(raw_ticks, int)
            and not isinstance(raw_ticks, bool)
            and raw_ticks >= 0
            else 0
        )
        ticks += 1
        channel_state[ISSUE_FILING_REACHABLE_TICKS_KEY] = ticks
        if ticks < ISSUE_FILING_REACHABLE_TICKS_REQUIRED:
            return False
        _reset_issue_filing_suppression(channel_state)
        return True

    if suppression_active:
        channel_state[ISSUE_FILING_REACHABLE_TICKS_KEY] = 0
        return False
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
    _reset_issue_filing_suppression(channel_state)
    channel_state.pop(GAP_TRANSCRIPT_KEY, None)
    channel_state.pop(GAP_OWNER_TRANSCRIPTS_KEY, None)
    return True


def _retire_dead_worktree_authorities(
    rt: Runtime,
    ch: ChannelConfig,
    chs: dict[str, Any],
    candidates: list[TranscriptCandidate],
    pattern: re.Pattern[str],
    now: float,
) -> list[str]:
    """Terminate loss-state whose owning session provably cannot come back.

    Pending and unresolved-GAP-owner paths are the two authorities exempt from
    the normal idle skip, so once one is bound to a session that no longer
    exists nothing retires it: the incident stays `alerting` and re-fires on its
    cooldown forever (#5155 — 2,835 `gap persists` ticks against a transcript
    whose last write was five days earlier).  Growth gating (#5158) stopped the
    selector from *chasing* that file; it cannot close the incident, because
    supersession still needs the channel to produce a live, bound, fully
    delivered successor, and an idle channel never does.

    The liveness predicate is the existence of the owning worktree DIRECTORY,
    recovered from the transcript's project-dir slug.  It is chosen precisely
    because it cannot confuse idle with dead: a session that is merely quiet
    still owns its checkout, and only an operator or `git worktree remove`
    takes the directory away.  Three further conditions keep an unreadable
    filesystem from ever reading as a dead session:

    1. The worktree ROOT must itself stat as a directory. A dismounted or
       renamed root would otherwise retire every authority at once.
    2. Absence must be proven by ENOENT/ENOTDIR (`directory_presence`), and
       must hold continuously for DEAD_WORKTREE_CONFIRM_SECS.
    3. The transcript must also be idle past `idle_quiet_secs`. Redundant with
       a deleted worktree, and deliberately so: a file still being written is
       never retired no matter what its directory stat says.

    Retirement is not a recovery claim. No RECOVERED notice is sent, the
    permanent-loss ledger is untouched, and `transcript-retired-reactivated`
    still readmits the path if it ever grows semantically again.
    """
    cid = ch.channel_id
    absences = _validated_worktree_absences(chs)
    if directory_presence(Path(ch.worktree_root)) is not True:
        if absences:
            chs.pop(DEAD_WORKTREE_ABSENT_SINCE_KEY, None)
            rt.log(
                f"[{cid}] dead-worktree-probe-reset reason=worktree_root_unreadable"
            )
        return []

    authorities: list[str] = []
    for path in (
        *_validated_gap_owner_transcripts(chs),
        *_validated_pending_transcripts(chs),
    ):
        if path not in authorities:
            authorities.append(path)
    candidate_by_path = {str(candidate.path): candidate for candidate in candidates}
    persisted_sizes = _validated_transcript_sizes(chs)

    next_absences: dict[str, float] = {}
    dead: list[str] = []
    for path in authorities:
        worktree = worktree_dir_for_transcript(path, ch.worktree_root, pattern)
        if worktree is None:
            continue
        if directory_presence(worktree) is not False:
            continue
        since = absences.get(path, now)
        next_absences[path] = since
        absent_secs = max(0.0, now - since)
        if absent_secs < DEAD_WORKTREE_CONFIRM_SECS:
            rt.log(
                f"[{cid}] dead-worktree-absence-pending worktree={worktree} "
                f"absent_secs={int(absent_secs)} "
                f"(< {DEAD_WORKTREE_CONFIRM_SECS}s confirm) path={path}"
            )
            continue
        candidate = candidate_by_path.get(path)
        # Fail CLOSED on a missing candidate, exactly like the supersession
        # guard below. `channel_project_dirs` returns [] on ANY OSError from
        # `root.iterdir()` and `_regular_file_stat_without_symlink` returns None
        # on a single failed stat, so one transient listing failure empties
        # `candidate_by_path` — treating that as "no live transcript" would
        # retire a file written seconds ago.
        if candidate is None or now - candidate.mtime < rt.cfg.idle_quiet_secs:
            rt.log(
                f"[{cid}] dead-worktree-retirement-declined reason="
                f"{'transcript_unobserved' if candidate is None else 'transcript_still_active'}"
                f" path={path}"
            )
            continue
        dead.append(path)

    if not dead:
        _store_worktree_absences(chs, next_absences)
        return []

    # Terminating a dead session's loss-state is the LAST point at which anyone
    # can learn those blocks are gone, so termination is gated on the notice
    # actually going out. The retirement notice shares ONE cooldown key across
    # every reason, so an idle/read-failure retirement a few seconds earlier
    # would otherwise swallow this one and close the incident in total silence.
    # Nothing below has mutated state yet, so deferring simply retries next tick
    # with the absence window intact.
    dead_notice = _alert_pending_retirement(
        rt, ch, chs, dead, now, reason=DEAD_WORKTREE_RETIREMENT_REASON
    )
    if dead_notice != "sent":
        _store_worktree_absences(chs, next_absences)
        rt.log(
            f"[{cid}] dead-worktree-retirement-deferred "
            f"notice={dead_notice} count={len(dead)}"
        )
        return []

    dead_set = set(dead)
    retired_transcripts = _validated_retired_transcripts(chs)
    for path in dead:
        candidate = candidate_by_path.get(path)
        size = candidate.size if candidate is not None else persisted_sizes.get(path)
        if not (isinstance(size, int) and not isinstance(size, bool) and size >= 0):
            size = 0
        retired_transcripts[path] = (size, now)
    _store_retired_transcripts(chs, retired_transcripts)

    remaining_pending = _validated_pending_transcripts(chs)
    raw_failures = chs.get(PENDING_TRANSCRIPT_FAILURES_KEY, {})
    raw_since = chs.get(PENDING_TRANSCRIPT_SINCE_KEY, {})
    _release_pending_authority(
        chs,
        remaining_pending,
        dict(raw_failures) if isinstance(raw_failures, dict) else {},
        dict(raw_since) if isinstance(raw_since, dict) else {},
        _validated_orphan_stranded_since(chs),
        dead_set,
    )
    if chs.get(SELECTED_TRANSCRIPT_KEY) in dead_set:
        chs.pop(SELECTED_TRANSCRIPT_KEY, None)
    if chs.get(SELECTOR_DIVERGED_TRANSCRIPT_KEY) in dead_set:
        # The I1 window was measured against a transcript that no longer has a
        # session behind it, so the window is void — not repaired. Dropping it
        # releases the stuck `selector_alerting` flag without asserting that
        # dcserver's bind is now correct; the next growing selection re-decides.
        chs.pop("selector_diverged_since", None)
        chs.pop(SELECTOR_DIVERGED_TRANSCRIPT_KEY, None)
        was_alerting = bool(chs.pop("selector_alerting", None))
        rt.log(
            f"[{cid}] selector-divergence-window-voided reason=dead_worktree "
            f"was_alerting={was_alerting}"
        )
    _store_worktree_absences(
        chs, {path: since for path, since in next_absences.items() if path not in dead_set}
    )
    rt.log(
        f"[{cid}] dead-worktree-loss-state-retired count={len(dead)} "
        f"paths={dead[:3]}"
    )
    _clear_gap_alert_without_recovery(rt, chs, cid, dead)
    return dead


def tick_channel(rt: Runtime, ch: ChannelConfig, state: dict[str, Any], now: float) -> None:
    cfg = rt.cfg
    cid = ch.channel_id
    chs: dict[str, Any] = state.setdefault(cid, {})

    def observe_coverage(
        transcript_probe: CoverageTranscriptProbe | None = None,
    ) -> None:
        try:
            tick_coverage(rt, ch, chs, now, transcript_probe)
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
        _, observations_corrupted = _validated_loss_observations_with_status(chs)
        _, tombstones_corrupted = _permanent_loss_tombstones_with_status(chs)
        loss_state_corrupted = observations_corrupted or tombstones_corrupted
        _forget_reclaimed_recovered_gap_lifecycles(
            chs,
            reclaimed_guard_paths,
            loss_state_corrupted=loss_state_corrupted,
        )
        rt.log(
            f"[{cid}] recovered-gap-guard-reclaimed "
            f"count={len(reclaimed_guard_paths)}"
        )
        if loss_state_corrupted:
            rt.log(
                f"[{cid}] permanent-loss-state-corrupt during lifecycle reclaim; "
                "preserving raw state"
            )
    # Runs before any authority is read into locals below, so a dead session's
    # pending/GAP-owner rows are gone from `chs` by the time selection, growth,
    # and the gap verdict look at them.
    _retire_dead_worktree_authorities(rt, ch, chs, candidates, pattern, now)
    previous_sizes = _validated_transcript_sizes(chs)
    previous_seen_at = _validated_transcript_seen_at(chs, previous_sizes, now)
    known_state_persisted = isinstance(chs.get(TRANSCRIPT_KNOWN_AT_KEY), dict)
    known_at = _validated_transcript_known_at(chs, now)
    pending_paths = _validated_pending_transcripts(chs)
    pending_failures = _validated_pending_failures(chs, pending_paths)
    pending_since = _validated_pending_since(chs, pending_paths, now)
    stranded_since = _validated_orphan_stranded_since(chs)
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
        observed_before = previous_sizes.get(path)
        if observed_before is not None and candidate.size <= observed_before:
            # Selection asks "which transcript is being written to *now*". A
            # guard floor is a delivery watermark, not a growth observation:
            # undelivered content parked above the floor must never make a
            # transcript that has not changed since the last tick look like it
            # is growing. Leaving this ungated pinned the selector to a
            # worktree dead for days and held the I1 alert open against a
            # healthy relay (#5072).
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
        (
            pending_paths,
            pending_failures,
            pending_since,
            stranded_since,
        ) = _release_pending_authority(
            chs,
            pending_paths,
            pending_failures,
            pending_since,
            stranded_since,
            expired,
        )
        rt.log(
            f"[{cid}] transcript-pending-expired count={len(expired_pending_paths)}"
        )
        # #5190 R3 R7 — TWO CONVENTIONS NOW COEXIST IN THIS FILE, on purpose and
        # under protest. This path retires FIRST and notifies second; the
        # orphan-stranded and dead-worktree retirements below treat the notice
        # AS the retirement and defer the whole thing when nobody was told.
        # Aligning this one means deferring state that the block above has
        # already mutated, which is a distinct change with its own regression
        # surface and is tracked as a #5190 follow-up. What does not wait is
        # saying so: an unannounced retirement is logged rather than dropped, so
        # the divergence is visible in the record instead of being silent.
        idle_notice = _alert_pending_retirement(
            rt, ch, chs, expired_pending_paths, now, reason="idle"
        )
        if idle_notice != "sent":
            rt.log(
                f"[{cid}] transcript-pending-expired-unannounced "
                f"notice={idle_notice} count={len(expired_pending_paths)}"
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
        observe_coverage()
        return

    hay = rt.discord_haystack(cid)
    if hay is None:
        observe_coverage()
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
    # #5190 R4: the newest block epoch this tick actually matched in the
    # haystack, per path. Distinct from `Verdict.delivered_ts`, which folds in
    # the persisted watermark and so cannot answer "is it delivering NOW?".
    current_delivered_by_path: dict[str, float] = {}
    suspected_permanent_losses = 0
    new_permanent_losses = 0
    raw_delivery_by_path = chs.get(LAST_ACTUAL_DELIVERY_BY_PATH_KEY, {})
    last_actual_delivery_by_path = (
        {
            path: float(observed_at)
            for path, observed_at in raw_delivery_by_path.items()
            if isinstance(path, str)
            and path
            and _is_finite_nonnegative_number(observed_at)
        }
        if isinstance(raw_delivery_by_path, dict)
        else {}
    )
    legacy_actual_delivery = chs.get(LAST_ACTUAL_DELIVERY_AT_KEY)
    if not last_actual_delivery_by_path:
        if _is_finite_nonnegative_number(legacy_actual_delivery):
            migrated_at = min(float(legacy_actual_delivery), now)
        else:
            migrated_at = now
        for candidate in active_candidates:
            last_actual_delivery_by_path[str(candidate.path)] = migrated_at
        rt.log(
            f"[{cid}] initialized last actual delivery timestamps "
            f"paths={len(active_candidates)} source="
            f"{'legacy' if _is_finite_nonnegative_number(legacy_actual_delivery) else 'now'}"
        )
    chs.pop(LAST_ACTUAL_DELIVERY_AT_KEY, None)
    unreadable_paths: list[str] = []
    escalated_pending_paths: list[str] = []
    orphan_retired_paths: list[str] = []
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
        # Advance the health watermark from the unfiltered source before removing
        # permanent-loss tombstones. Elapsed time must describe real transport
        # progress, never the age of an unmatched block.
        observed_verdict = evaluate(
            read_result.blocks,
            hay,
            now,
            cfg.grace_secs,
            cfg.gap_alert_secs,
            prior_delivered_ts,
        )
        if observed_verdict.delivered_ts > prior_delivered_ts:
            advance_delivered_watermark(
                chs, candidate.path, observed_verdict.delivered_ts, now
            )
            last_actual_delivery_by_path[path] = now
        tombstone_update = update_permanent_loss_tombstones(
            chs,
            candidate.path,
            read_result.blocks,
            read_result.block_source_ids,
            hay,
            now,
            cfg.grace_secs,
            prior_delivered_ts,
            observed_verdict.delivered_ts,
        )
        suspected_permanent_losses += tombstone_update.suspected
        new_permanent_losses += len(tombstone_update.newly_tombstoned)
        raw_identity_warnings = chs.get(PERMANENT_LOSS_IDENTITY_WARNING_KEY, {})
        identity_warnings = (
            {
                warning_path: int(count)
                for warning_path, count in raw_identity_warnings.items()
                if isinstance(warning_path, str)
                and isinstance(count, int)
                and not isinstance(count, bool)
                and count > 0
            }
            if isinstance(raw_identity_warnings, dict)
            else {}
        )
        prior_fallbacks = identity_warnings.get(path)
        if read_result.identity_fallbacks:
            identity_warnings[path] = read_result.identity_fallbacks
            if prior_fallbacks != read_result.identity_fallbacks:
                rt.log(
                    f"[{cid}] permanent-loss-identity-offset-fallback path={path} "
                    f"blocks={read_result.identity_fallbacks}; identity may change "
                    "after head truncation"
                )
        else:
            identity_warnings.pop(path, None)
        if identity_warnings:
            chs[PERMANENT_LOSS_IDENTITY_WARNING_KEY] = identity_warnings
        else:
            chs.pop(PERMANENT_LOSS_IDENTITY_WARNING_KEY, None)
        raw_corruption_warnings = chs.get(
            PERMANENT_LOSS_CORRUPTION_WARNING_KEY, {}
        )
        corruption_warnings = (
            {
                warning_path: fingerprint
                for warning_path, fingerprint in raw_corruption_warnings.items()
                if isinstance(warning_path, str)
                and isinstance(fingerprint, str)
                and fingerprint
            }
            if isinstance(raw_corruption_warnings, dict)
            else {}
        )
        prior_corruption = corruption_warnings.get(path)
        if tombstone_update.corrupted:
            corruption_fingerprint = hashlib.sha256(
                json.dumps(
                    {
                        LOSS_OBSERVATIONS_KEY: {
                            "present": LOSS_OBSERVATIONS_KEY in chs,
                            "value": chs.get(LOSS_OBSERVATIONS_KEY),
                        },
                        PERMANENT_LOSS_TOMBSTONES_KEY: {
                            "present": PERMANENT_LOSS_TOMBSTONES_KEY in chs,
                            "value": chs.get(PERMANENT_LOSS_TOMBSTONES_KEY),
                        },
                    },
                    sort_keys=True,
                    separators=(",", ":"),
                    default=repr,
                ).encode("utf-8")
            ).hexdigest()
            corruption_warnings[path] = corruption_fingerprint
            if prior_corruption != corruption_fingerprint:
                rt.log(
                    f"[{cid}] permanent-loss-state-corrupt path={path}; "
                    "preserving raw state"
                )
        else:
            corruption_warnings.pop(path, None)
        if corruption_warnings:
            chs[PERMANENT_LOSS_CORRUPTION_WARNING_KEY] = corruption_warnings
        else:
            chs.pop(PERMANENT_LOSS_CORRUPTION_WARNING_KEY, None)
        if tombstone_update.overflowed and not tombstone_update.corrupted:
            rt.log(
                f"[{cid}] permanent-loss-state-overflow path={path} "
                f"dropped={tombstone_update.overflowed} "
                f"total={chs[PERMANENT_LOSS_OVERFLOW_TOTAL_KEY]}"
            )
        verdict = evaluate(
            tombstone_update.active_blocks,
            hay,
            now,
            cfg.grace_secs,
            cfg.gap_alert_secs,
            prior_delivered_ts,
        )
        active_matches = delivered_flags(tombstone_update.active_blocks, hay)
        fresh_undelivered = sum(
            1
            for (epoch, _), matched in zip(
                tombstone_update.active_blocks, active_matches
            )
            if now - epoch <= cfg.grace_secs
            and epoch > verdict.delivered_ts
            and not matched
        )
        fresh_undelivered_by_path[path] = fresh_undelivered
        current_delivered_by_path[path] = max(
            (
                epoch
                for (epoch, _), matched in zip(
                    tombstone_update.active_blocks, active_matches
                )
                if matched
            ),
            default=0.0,
        )
        # #5190: a stranded-orphan marker written on an earlier tick matures
        # into an abandonment once the window has elapsed and the file is still
        # frozen. Resurrection (semantic growth) voids it below, so a session
        # that starts writing again is never closed out behind its own back.
        marker_matured = _orphan_authority_matured(chs, path, now)
        # #5190 R3: elapsed time is not the finding — the finding is "these
        # blocks cannot be recovered", and the confirmation window exists to let
        # that be falsified. A stranded block that finally reached Discord
        # during the window recovers the verdict, so retiring on the timer alone
        # would announce "회수 불가" about blocks that had already arrived. The
        # claim is re-checked against THIS tick's verdict before it is made.
        orphan_matured = (
            marker_matured
            and path not in semantic_growth_paths
            and verdict.state == STATE_GAP
        )
        if marker_matured and not orphan_matured:
            rt.log(
                f"[{cid}] orphan-stranded-maturity-voided path={path} "
                f"state={verdict.state} lost={verdict.lost}"
            )
        if orphan_matured:
            orphan_retired_paths.append(path)
        if tombstone_update.newly_tombstoned:
            rt.log(
                f"[{cid}] permanent-loss-confirmed path={path} "
                f"new={len(tombstone_update.newly_tombstoned)} "
                f"total={permanent_loss_total(chs)}"
            )
        if tombstone_update.retracted:
            rt.log(
                f"[{cid}] permanent-loss-retracted path={path} "
                f"count={len(tombstone_update.retracted)} "
                f"total={permanent_loss_total(chs)}"
            )
        if path in pending_set:
            rt.log(
                f"[{cid}] transcript-debut-eval path={path} "
                f"state={verdict.state} lost={verdict.lost} "
                f"fresh_undelivered={fresh_undelivered}"
            )
            # A clean verdict is the normal release. It is also unreachable for
            # an orphan holding permanently undelivered blocks — STATE_OK can
            # never arrive on a session that will never write again — which is
            # what pinned #5190's pending authority open forever. A matured
            # stranded-orphan marker is the second, evidence-backed exit, but it
            # is NOT applied here: that release now happens below, atomically
            # with the retirement notice actually reaching a human (#5190 R1).
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
    if last_actual_delivery_by_path:
        chs[LAST_ACTUAL_DELIVERY_BY_PATH_KEY] = dict(
            sorted(last_actual_delivery_by_path.items())[-MAX_KNOWN_TRANSCRIPTS:]
        )
    else:
        chs.pop(LAST_ACTUAL_DELIVERY_BY_PATH_KEY, None)
    if escalated_pending_paths:
        retired_pending_paths.extend(escalated_pending_paths)
        escalated = set(escalated_pending_paths)
        for path in escalated_pending_paths:
            candidate = candidate_by_path.get(path)
            size = candidate.size if candidate is not None else merged_sizes.get(path)
            if isinstance(size, int) and not isinstance(size, bool) and size >= 0:
                retired_transcripts[path] = (size, now)
        _store_retired_transcripts(chs, retired_transcripts)
        (
            remaining_pending,
            pending_failures,
            pending_since,
            stranded_since,
        ) = _release_pending_authority(
            chs,
            remaining_pending,
            pending_failures,
            pending_since,
            stranded_since,
            escalated,
        )
        rt.log(
            f"[{cid}] transcript-pending-escalated "
            f"count={len(escalated_pending_paths)}"
        )
        # Same divergence as the idle expiry above (#5190 R3 R7): retired first,
        # announced second. Logged rather than silently dropped.
        escalated_notice = _alert_pending_retirement(
            rt,
            ch,
            chs,
            escalated_pending_paths,
            now,
            reason="read_failure",
        )
        if escalated_notice != "sent":
            rt.log(
                f"[{cid}] transcript-pending-escalated-unannounced "
                f"notice={escalated_notice} count={len(escalated_pending_paths)}"
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
        observe_coverage()
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
    selected_probe = rt.watcher_state(cid)
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
            (
                remaining_pending,
                pending_failures,
                pending_since,
                stranded_since,
            ) = (
                _release_pending_authority(
                    chs,
                    remaining_pending,
                    pending_failures,
                    pending_since,
                    stranded_since,
                    superseded,
                )
            )
            evaluated = [
                (candidate, verdict)
                for candidate, verdict in evaluated
                if str(candidate.path) not in superseded
            ]
            rt.log(
                f"[{cid}] historical-gap-owner-retired "
                f"successor={selected_path} count={len(superseded_gap_owners)}"
            )

    # ── Stranded orphan transcripts (#5190) ──────────────────────────────────
    # A session abandoned by `/clear` can leave blocks that no future delivery
    # can ever recover: its delivery frontier cannot advance, so the tombstone
    # path (which requires two advances) never confirms; its worktree is still
    # alive, so dead-worktree retirement never fires; and STATE_OK can never
    # arrive, so the pending authority never releases. Every exit the watchdog
    # had was blocked by the very condition it was built to resolve, and the
    # channel re-alerted every `realert_secs` about a session that had been
    # frozen for hours.
    #
    # The exit here rests on positive evidence rather than on silence: the path
    # is NOT the active session, is not growing, has been frozen past its freeze
    # floor, and — decisively — another transcript on the same channel is
    # delivering right now. A relay that is actually down produces none of that
    # last part, so nothing below can silence a real outage.
    orphan_unattributable_paths: set[str] = set()
    # Paths whose stranded finding is re-corroborated on THIS tick, and so the
    # only ones a matured marker may actually close out (#5190 R3 P2-A).
    orphan_corroborated_paths: set[str] = set()
    for candidate, verdict in evaluated:
        path = str(candidate.path)
        # The freeze floor is load-bearing, not cosmetic. Without it this same
        # shape swallows #4435: a session that has just been swapped in, is not
        # yet selected, and whose blocks are being lost RIGHT NOW looks
        # identical to an abandoned one except for how long it has been silent.
        # #5190 R2: an orphan that once delivered is the SAME defect — it re-
        # alerts every `realert_secs` until an unrelated idle timer expires —
        # but its gap is a measurement, not an unknown, so excluding it was
        # wrong and admitting it on the same terms would be reckless. It gets a
        # strictly stronger bar: twice the freeze floor, a delivery frontier
        # that has itself gone a full freeze floor without advancing (it is not
        # simply lagging), and dcserver proving the channel has moved to another
        # session.
        #
        # #5190 R3 P2-B — what those three actually are. An earlier revision of
        # this comment claimed "every one of those is falsified by a live relay
        # outage". That was false, and false in the direction that flatters the
        # code:
        #   · the doubled freeze floor is a TIME FILTER. An outage neither
        #     satisfies nor falsifies it; it only sets how long a corpse must
        #     lie still before anyone may say so.
        #   · the stale delivery frontier is an ANTI-LAGGING filter. An outage
        #     does not falsify it — an outage SATISFIES it, because a frontier
        #     that stops advancing is exactly what a stopped relay produces. It
        #     argues FOR the marker during an outage, not against it.
        #   · successor binding is evidence of SESSION ROTATION, not of
        #     delivery. The watcher stays bound to the current session while the
        #     relay is stone dead, so dcserver goes on answering yes.
        # Exactly one check in this mechanism can tell a dead session from a
        # dead relay: `_channel_has_live_delivery` — a SIBLING transcript
        # matching a block of its own in THIS tick's haystack. It holds that
        # line alone. Everything above only decides who is eligible to be asked;
        # that one call is the only thing that answers, which is why it is
        # re-run before any marker is allowed to close (#5190 R3 P2-A) and why
        # its own freshness limits are documented rather than glossed.
        observed_history = not gap_is_unobserved(verdict)
        freeze_floor = cfg.orphan_abandon_secs * (
            ORPHAN_OBSERVED_FREEZE_MULTIPLIER if observed_history else 1
        )
        eligible = (
            path != selected_path
            and verdict.state == STATE_GAP
            and path not in semantic_growth_paths
            and now - candidate.mtime >= freeze_floor
            and (
                not observed_history
                or (
                    successor_binding_proven
                    and now - verdict.delivered_ts >= cfg.orphan_abandon_secs
                )
            )
        )
        if not eligible:
            # Resurrection, recovery, or promotion to the active session voids
            # the marker: this path is back under ordinary judgment.
            if path in stranded_since:
                stranded_since = _store_orphan_stranded_since(
                    chs, stranded_since, released={path}
                )
                rt.log(f"[{cid}] orphan-stranded-marker-cleared path={path}")
            continue
        # #5190 R3 P2-A: the marker records a finding made on ONE earlier tick,
        # and `_orphan_authority_matured` only asks whether the clock has run
        # out since. Everything that justified opening it has to still hold at
        # the moment it is cashed in, or the retirement rests on evidence nobody
        # re-read. Reaching this line already re-establishes ①③ and the freeze
        # floor for this tick — `eligible` above is the very predicate that
        # opened the marker — so what is left to re-prove is ④, which is also
        # the only one of them a relay outage can falsify.
        marker_since = stranded_since.get(path)
        if marker_since is None:
            if not _channel_has_live_delivery(
                evaluated,
                fresh_undelivered_by_path,
                current_delivered_by_path,
                path,
                now,
                cfg.gap_alert_secs,
            ):
                # No corroborating delivery anywhere on the channel — this may
                # well be a live outage. Leave it to the normal gap path.
                continue
            stranded_since[path] = now
            rt.log(
                f"[{cid}] orphan-stranded-marker-opened path={path} "
                f"lost={verdict.lost} selected={selected_path} "
                f"observed_history={observed_history} "
                f"freeze_floor={int(freeze_floor)}s"
            )
        elif _channel_has_live_delivery(
            evaluated,
            fresh_undelivered_by_path,
            current_delivered_by_path,
            path,
            now,
            cfg.gap_alert_secs,
            newer_than=marker_since,
        ):
            # Corroboration strong enough to CLOSE on, which is a stricter thing
            # than the corroboration that opened the marker: the sibling's block
            # was WRITTEN after the finding was made, so no relay that had
            # already died by then could have delivered it. This is what shuts
            # the write-epoch window documented on `_channel_has_live_delivery`
            # (#5190 R3 P2-D), where an idle sibling's one old-but-still-matched
            # block could vote "alive" for a full `gap_alert_secs` — longer than
            # the confirmation window it would have been carrying.
            orphan_corroborated_paths.add(path)
        orphan_unattributable_paths.add(path)
    if orphan_retired_paths:
        # #5190 R3 P2-A: a matured marker is a claim, not a licence. Three
        # shapes reach here with an expired timer and no corroboration this
        # tick, and all three used to retire anyway:
        #   (a) the channel stopped delivering after the marker was opened — a
        #       live outage read as proof of abandonment, which is precisely the
        #       inversion this whole mechanism is built to avoid;
        #   (b) the orphan was PROMOTED to this channel's active session, so
        #       retiring it drops the channel's gap ownership to zero and the
        #       currently-running session loses its own loss record;
        #   (c) the path went unevaluated for a tick and came back to find its
        #       timer already run out, closing without ④ ever being asked.
        # (a) and (c) fail the ④ re-check below; (b) never reaches it, because
        # `eligible` above rejects a selected path and clears its marker.
        unconfirmed = [
            path
            for path in orphan_retired_paths
            if path not in orphan_corroborated_paths
        ]
        if unconfirmed:
            # The marker itself survives: the finding may well still be true,
            # and nothing about this tick disproves it. What it no longer does
            # is close anything out on its own authority.
            rt.log(
                f"[{cid}] orphan-stranded-maturity-unconfirmed "
                f"count={len(unconfirmed)} selected={selected_path} "
                f"paths={','.join(sorted(unconfirmed))}"
            )
            orphan_retired_paths = [
                path
                for path in orphan_retired_paths
                if path in orphan_corroborated_paths
            ]
    if orphan_retired_paths:
        orphan_notice = _alert_pending_retirement(
            rt,
            ch,
            chs,
            orphan_retired_paths,
            now,
            reason=ORPHAN_STRANDED_RETIREMENT_REASON,
        )
        if orphan_notice != "sent":
            # #5190 R1: the notice IS the retirement. Retiring anyway would drop
            # the pending authority, clear the gap, and delete the only
            # surviving record of these blocks with nobody ever told — an
            # observation failure collapsed into a success. The marker survives
            # untouched, so the next tick resumes from exactly here and the
            # alert keeps firing meanwhile.
            #
            # #5190 R3 P2-E: `notice=` reports WHICH of those happened.
            # `undelivered` means the message never left the box; `cooldown`
            # means it was suppressed by the retirement cooldown key that all
            # reasons share, so an unrelated idle/read_failure/dead_worktree
            # notice within `realert_secs` delays this one by up to that long.
            # The old line said `notice=undelivered` for both, which made the
            # log lie about the relay in the cooldown case.
            rt.log(
                f"[{cid}] orphan-stranded-retirement-deferred "
                f"count={len(orphan_retired_paths)} notice={orphan_notice}"
            )
            orphan_retired_paths = []
    if orphan_retired_paths:
        retired_orphans = set(orphan_retired_paths)
        for path in orphan_retired_paths:
            candidate = candidate_by_path.get(path)
            if candidate is not None:
                retired_transcripts[path] = (candidate.size, now)
        _store_retired_transcripts(chs, retired_transcripts)
        # Released only now, downstream of a notice that provably left the box.
        (
            remaining_pending,
            pending_failures,
            pending_since,
            stranded_since,
        ) = (
            _release_pending_authority(
                chs,
                remaining_pending,
                pending_failures,
                pending_since,
                stranded_since,
                retired_orphans,
            )
        )
        evaluated = [
            (candidate, verdict)
            for candidate, verdict in evaluated
            if str(candidate.path) not in retired_orphans
        ]
        orphan_unattributable_paths -= retired_orphans
        rt.log(
            f"[{cid}] orphan-stranded-authority-retired "
            f"count={len(orphan_retired_paths)} "
            f"frozen_floor={int(cfg.orphan_abandon_secs)}s "
            f"confirm={ORPHAN_STRANDED_CONFIRM_SECS}s"
        )
        _clear_gap_alert_without_recovery(rt, chs, cid, orphan_retired_paths)
        retired_pending_paths.extend(orphan_retired_paths)
    _store_orphan_stranded_since(chs, stranded_since)
    if not evaluated:
        observe_coverage()
        return

    state_rank = {STATE_OK: 0, STATE_LAGGING: 1, STATE_GAP: 2}
    # An unattributable orphan must never be the channel's alert subject while a
    # path with real evidence is available; it stays in `evaluated` (and thus a
    # GAP owner) so it keeps being judged until it matures or resurrects.
    rankable = [
        item
        for item in evaluated
        if str(item[0].path) not in orphan_unattributable_paths
    ] or evaluated
    verdict_candidate, v = max(
        rankable,
        key=lambda item: (
            state_rank[item[1].state],
            item[1].gap_secs,
            item[1].lost,
            item[0].mtime,
            str(item[0].path),
        ),
    )
    verdict_path = str(verdict_candidate.path)
    selected_coverage = next(
        (
            verdict
            for candidate, verdict in evaluated
            if selected is not None and candidate.path == selected.path
        ),
        None,
    )
    coverage_transcript_probe = (
        CoverageTranscriptProbe(
            growing=f_growing,
            blocks=selected_coverage.blocks,
            lost=sum(verdict.lost for _, verdict in evaluated),
        )
        if selected_coverage is not None
        else None
    )
    observe_coverage(coverage_transcript_probe)
    raw_unannounced = chs.get(PERMANENT_LOSS_UNANNOUNCED_KEY, 0)
    unannounced = (
        raw_unannounced
        if isinstance(raw_unannounced, int)
        and not isinstance(raw_unannounced, bool)
        and raw_unannounced >= 0
        else 0
    ) + new_permanent_losses
    if unannounced:
        cumulative_losses = permanent_loss_total(chs)
        delivered_notice = rt.alert(
            ch,
            "🚨 **새 영구 릴레이 유실 확정 (out-of-band 워치독)**\n\n"
            f"서로 다른 후속 delivery frontier 전진 뒤에도 미매칭으로 남은 "
            f"블록 **{unannounced}건**을 재전송 불가 tombstone으로 전환했습니다.\n"
            f"현재 미도달 **{sum(verdict.lost for _, verdict in evaluated)}건**, "
            f"영구 유실 **{cumulative_losses}건 누적**.\n\n"
            f"런타임: {rt.dcserver_snapshot()}",
        )
        if delivered_notice:
            chs.pop(PERMANENT_LOSS_UNANNOUNCED_KEY, None)
        else:
            chs[PERMANENT_LOSS_UNANNOUNCED_KEY] = unannounced
        rt.log(
            f"[{cid}] PERMANENT LOSS ALERT new={unannounced} "
            f"cumulative={cumulative_losses} delivered={delivered_notice}"
        )
    if suspected_permanent_losses:
        chs[PERMANENT_LOSS_SUSPECTED_KEY] = suspected_permanent_losses
        rt.log(
            f"[{cid}] permanent-loss-suspected count={suspected_permanent_losses} "
            "awaiting independent frontier evidence"
        )
    else:
        chs.pop(PERMANENT_LOSS_SUSPECTED_KEY, None)
    preserve_gap_incident = (
        selected_probe.status is None
        and chs.get(ISSUE_FILING_SUPPRESSION_REASON_KEY)
        == ISSUE_FILING_DC_UNREACHABLE_REASON
        and bool(
            _validated_gap_owner_transcripts(chs)
            or chs.get("alerting")
            or chs.get("gap_since")
            or chs.get("issue_url")
        )
    )
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
        if (
            path not in evaluated_paths
            or (preserve_gap_incident and path in evaluated_paths)
        )
        and path not in superseded_gap_owners
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
            _reset_issue_filing_suppression(chs)
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
        issue_filing_stable = _issue_filing_stable(chs, selected_probe, now)
        last_actual_delivery_at = last_actual_delivery_by_path.get(
            verdict_path, 0.0
        )
        gap_min = _confirmed_gap_minutes(v, last_actual_delivery_at, now)
        if not chs.get("gap_since"):
            chs["gap_since"] = now
        issue_due = (
            bool(cfg.github_repo)
            and not chs.get("issue_url")
            and now - float(chs["gap_since"]) >= cfg.issue_after_secs
        )
        if issue_due and issue_filing_stable:
            url = rt.file_github_issue(ch, gap_min, v.lost)
            if url:
                chs["issue_url"] = url
        elif issue_due:
            rt.log(
                f"[{cid}] issue filing deferred "
                f"reason={chs.get(ISSUE_FILING_SUPPRESSION_REASON_KEY)} "
                f"reachable_ticks={chs.get(ISSUE_FILING_REACHABLE_TICKS_KEY, 0)}"
            )
        if now - float(chs.get("last_alert", 0)) >= cfg.realert_secs:
            issue_line = (
                f"\n자동 등록 이슈: {chs['issue_url']}" if chs.get("issue_url") else ""
            )
            # #5190 R2: an unknown elapsed time is stated as unknown. The old
            # code printed the sentinel 999 into "마지막 정상 도달 이후 999분
            # 경과" and into the title of the issue it filed about itself.
            elapsed_line = (
                f"마지막 정상 도달 이후 **{gap_min}분** 경과.\n"
                if gap_min is not None
                else "이 세션에서 배달이 확인된 이력이 없습니다 "
                "(경과 시간을 측정할 기준점 없음).\n"
            )
            # #5190 R5: say only what this tick proved, one clause per piece of
            # evidence. Non-selection alone does NOT make a transcript a past
            # session — a concurrent one swapped in moments ago is also
            # non-selected, and calling it "과거 세션" while its blocks are being
            # lost right now is exactly backwards. It also proves nothing about
            # the relay's health; that claim belongs to the delivering sibling,
            # which is a separate check. A true enumeration beats a false
            # universal.
            # #5190 R3 P2-F: this is a THREE-state question, not two. There is a
            # tick on which `selected_path` is None — reached whenever the
            # selected transcript expires out of the pending set in this very
            # pass, because `tr` is cleared there and `selected` is resolved
            # from it immediately after, while a different path keeps its GAP
            # ownership and stays the alert subject. Folding that case into the
            # `not not_selected` arm rendered "대상 세션: X (현재 활성 세션)" on a
            # channel with no active session at all — an unproven PRESENTNESS
            # asserted, the exact mirror of the unproven pastness R5 removed.
            selection_known = bool(selected_path)
            not_selected = selection_known and verdict_path != selected_path
            frozen_secs = max(0.0, now - verdict_candidate.mtime)
            frozen_min = int(frozen_secs) // 60
            orphan_gap = (
                not_selected
                and verdict_path not in semantic_growth_paths
                and frozen_secs >= cfg.orphan_abandon_secs
            )
            delivering_elsewhere = _channel_has_live_delivery(
                evaluated,
                fresh_undelivered_by_path,
                current_delivered_by_path,
                verdict_path,
                now,
                cfg.gap_alert_secs,
            )
            headline = (
                "🚨 **릴레이 갭 감지 (out-of-band 워치독) — 고아 세션**"
                if orphan_gap
                else "🚨 **릴레이 갭 감지 (out-of-band 워치독)**"
            )
            if not selection_known:
                scope_line = (
                    f"대상 세션: `{Path(verdict_path).name}` — 이 채널에는 현재 "
                    "활성 세션으로 판정된 트랜스크립트가 없습니다(이번 점검에서 "
                    "선택 권한이 확정되지 않음). 따라서 이 경로가 활성 세션인지 "
                    "과거 세션인지는 이번 점검으로 판정되지 않았습니다.\n"
                    + (
                        "이 채널의 다른 세션은 이번 점검에서 실제로 배달이 "
                        "확인됐습니다 — 릴레이 전체 장애가 아니라 이 "
                        "트랜스크립트의 미도달 블록 문제입니다.\n"
                        if delivering_elsewhere
                        else "이 채널에서 지금 배달 중인 다른 세션은 확인되지 "
                        "않았습니다 — 릴레이 장애 가능성이 남아 있습니다.\n"
                    )
                )
            elif not not_selected:
                scope_line = (
                    f"대상 세션: `{Path(verdict_path).name}` (현재 활성 세션).\n"
                )
            else:
                scope_line = (
                    f"대상 세션: `{Path(verdict_path).name}` — 이 채널의 현재 "
                    f"활성 세션(`{Path(str(selected_path)).name}`)이 아닙니다. "
                    + (
                        f"이 트랜스크립트는 **{frozen_min}분째 새 기록이 "
                        "없습니다** (고아 세션으로 판정).\n"
                        if orphan_gap
                        else f"마지막 기록으로부터 {frozen_min}분 경과 — 아직 "
                        "과거 세션이라고 단정할 수 없습니다 (판정 기준 "
                        f"{cfg.orphan_abandon_secs // 60}분).\n"
                    )
                    + (
                        "이 채널의 다른 세션은 이번 점검에서 실제로 배달이 "
                        "확인됐습니다 — 릴레이 전체 장애가 아니라 이 "
                        "트랜스크립트의 미도달 블록 문제입니다.\n"
                        if delivering_elsewhere
                        else "이 채널에서 지금 배달 중인 다른 세션은 확인되지 "
                        "않았습니다 — 릴레이 장애 가능성이 남아 있습니다.\n"
                    )
                )
            rt.alert(
                ch,
                f"{headline}\n\n"
                f"소스(세션 트랜스크립트)에는 있는데 Discord에 도착하지 않은 "
                f"assistant 블록 **{v.lost}건**.\n"
                f"{scope_line}"
                f"{elapsed_line}\n"
                f"런타임: {rt.dcserver_snapshot()}{issue_line}\n\n"
                f"이 알림은 turn-relay 경로가 아니라 out-of-band로 직접 나갑니다 — "
                f"릴레이가 죽어도 도착합니다.",
            )
            chs["last_alert"] = now
            chs["alerting"] = True
            rt.log(
                f"[{cid}] ALERT path={verdict_path} lost={v.lost} "
                f"gap_min={gap_min if gap_min is not None else 'unobserved'} "
                f"orphan={orphan_gap}"
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
        if preserve_gap_incident:
            rt.log(
                f"[{cid}] dcserver transport unreachable — preserving gap "
                "incident state across permanent-loss transition"
            )
            return
        if chs.get("alerting") and new_permanent_losses:
            # Tombstoning closes the retryable incident but does not prove that
            # the skipped blocks arrived; the dedicated permanent-loss alert is
            # the only transition notice.
            rt.log(
                f"[{cid}] gap transitioned to permanent loss — "
                "alert state cleared without recovery claim"
            )
        elif chs.get("alerting"):
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
        _reset_issue_filing_suppression(chs)
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
