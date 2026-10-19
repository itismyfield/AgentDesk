//! Durable store + commit-tombstone I/O for the aborted/deferred-claim anchor
//! markers (#3296/#3303) — extracted from `mod.rs` so the reconcile logic and
//! the filesystem surface stay independently reviewable. Pure move: every item
//! keeps its `mod.rs`-era contract and is re-exported there, so external
//! callers (`tmux_watcher`, `placeholder_sweeper`, `tui_prompt_relay`) are
//! unchanged.

use serde::{Deserialize, Serialize};

use super::AbortedAnchorMarker;

// ---------------------------------------------------------------------------
// Durable store (mirrors `tui_direct_pending_start`'s store + atomic writes)
// ---------------------------------------------------------------------------

// Thread-local test seam for the durable BASE root both sibling stores
// (markers + commit tombstones) join subdirs onto (the
// `TEST_TMUX_ALIVE_OVERRIDE` convention, inflight.rs). Tests inject a tempdir
// here, never the process-global `AGENTDESK_ROOT_DIR` env: env mutation races
// lock-free root readers in other tests, while a thread-local needs no lock
// (the tests' current-thread `block_on` runtimes stay on this thread).
#[cfg(test)]
thread_local! {
    static TEST_ROOT_OVERRIDE: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(in crate::services::discord) fn set_test_root_override(path: Option<std::path::PathBuf>) {
    TEST_ROOT_OVERRIDE.with(|cell| *cell.borrow_mut() = path);
}

pub(super) fn root() -> Option<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(base) = TEST_ROOT_OVERRIDE.with(|cell| cell.borrow().clone()) {
        return Some(base.join("discord_tui_direct_abort_marker"));
    }
    crate::services::discord::runtime_store::tui_direct_abort_marker_root()
}

pub(super) fn tombstone_root() -> Option<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(base) = TEST_ROOT_OVERRIDE.with(|cell| cell.borrow().clone()) {
        return Some(base.join("discord_tui_direct_commit_tombstone"));
    }
    crate::services::discord::runtime_store::tui_direct_commit_tombstone_root()
}

/// Persist (or update) a marker. Recorded by the ABORT path BEFORE any http
/// availability check so a restart or late-arriving http can still reconcile.
/// Zero anchor ids are rejected (I5: nothing could ever be reconciled on them).
pub(in crate::services::discord) fn record(marker: &AbortedAnchorMarker) -> Result<(), String> {
    if marker.anchor_message_id == 0 {
        return Err("refusing to record aborted-anchor marker with zero anchor_message_id".into());
    }
    let Some(root) = root() else {
        return Ok(()); // tests / unconfigured root — nothing durable to write
    };
    let path = root.join(format!("{}.json", marker.file_stem()));
    let data = serde_json::to_string_pretty(marker).map_err(|e| e.to_string())?;
    crate::services::discord::runtime_store::critical_atomic_write(
        &path,
        &data,
        crate::services::discord::runtime_store::AtomicWriteContext::new("tui_direct_abort_marker")
            .provider(&marker.provider)
            .channel_id(marker.channel_id),
    )
}

/// Drop a marker once its correction was delivered (plus its claim sidecar so
/// the store does not accumulate lock files). Idempotent. The sidecar unlink
/// is benign even if a contender still holds an fd on the old inode: it
/// re-reads the marker under its claim and finds it gone (stems are keyed on
/// unique anchor snowflakes — never reused for a different logical marker).
pub(in crate::services::discord) fn delete(marker: &AbortedAnchorMarker) {
    if let Some(root) = root() {
        let stem = marker.file_stem();
        let _ = std::fs::remove_file(root.join(format!("{stem}.json")));
        let _ = std::fs::remove_file(root.join(format!("{stem}.json.lock")));
    }
}

/// Load every durable marker (sweep + restart survival: the store IS the
/// restart state). Unparseable JSON (atomic writes ⇒ corruption or schema
/// drift, never a torn write) is QUARANTINED via a `.bad` rename instead of
/// silently re-skipped forever (verify r1 fix #3 — one WARN per file).
pub(in crate::services::discord) fn load_all() -> Vec<AbortedAnchorMarker> {
    let Some(root) = root() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue; // transient read failure — retry next pass
        };
        match serde_json::from_str::<AbortedAnchorMarker>(&text) {
            Ok(marker) => out.push(marker),
            Err(error) => {
                let quarantine = path.with_extension("json.bad");
                let renamed = std::fs::rename(&path, &quarantine);
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    renamed_ok = renamed.is_ok(),
                    "tui_direct_abort_marker: unparseable marker quarantined to .bad (never re-parsed; #3296 verify r1)"
                );
            }
        }
    }
    out
}

/// Re-read ONE marker fresh from disk (the under-claim read: a reconciler must
/// decide on the post-claim state, never its pre-claim snapshot).
pub(super) fn reload(marker: &AbortedAnchorMarker) -> Option<AbortedAnchorMarker> {
    let root = root()?;
    let path = root.join(format!("{}.json", marker.file_stem()));
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Markers scoped to one `(provider, channel)` — the terminal-commit drain's
/// working set.
pub(in crate::services::discord) fn load_for_channel(
    provider: &str,
    channel_id: u64,
) -> Vec<AbortedAnchorMarker> {
    load_all()
        .into_iter()
        .filter(|m| m.channel_id == channel_id && m.provider.eq_ignore_ascii_case(provider))
        .collect()
}

// ---------------------------------------------------------------------------
// Commit tombstones (codex r2 — durable terminal-commit evidence)
// ---------------------------------------------------------------------------

/// Tombstone retention = the marker hard cap (1h): any marker a tombstone
/// could still cover resolves within that bound, so older evidence has no
/// consumer. GC runs at the END of each sweep pass, so a first post-restart
/// pass still 대조s against evidence that aged out during downtime.
pub(in crate::services::discord) const COMMIT_TOMBSTONE_RETENTION_MS: u64 =
    super::ABORT_MARKER_TTL.as_millis() as u64 * super::ABORT_MARKER_HARD_CAP_TTL_MULTIPLIER;

/// Durable record of one body-visible terminal commit, written by the watcher
/// chokepoint BEFORE it clears the inflight row (codex r2). Write-before-clear
/// is the load-bearing invariant: whenever a reconciler observes "the foreign
/// row is gone", a commit-caused deletion has ALREADY made its tombstone
/// visible — row-absence + matching tombstone proves the prior owner committed
/// (`✅`); row-absence WITHOUT one is a non-commit force-clear/stop/recovery
/// deletion (bounded `⚠`). Primitive fields survive a dcserver version swap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct CommitTombstone {
    pub provider: String,
    pub channel_id: u64,
    pub tmux_session_name: String,
    /// Identity of the COMMITTED turn (`inflight.rs` `InflightTurnIdentity`
    /// convention) — 대조 against a marker's recorded foreign identity is the
    /// same positive correlation the drain cover uses (codex r1).
    pub committed_user_msg_id: u64,
    pub committed_started_at: String,
    /// JSONL byte offset at which the committed turn began. Present for new
    /// inflight-backed commits; absent on legacy tombstones, which makes id-0
    /// same-timestamp DeferredClaim cover checks fail closed.
    #[serde(default)]
    pub committed_turn_start_offset: Option<u64>,
    /// JSONL byte offset where the terminal evidence line was observed. New
    /// writers set `committed_terminal_evidence_offset_recorded=true`; if that
    /// new-format record lacks this offset, DeferredClaim cover checks fail
    /// closed. Legacy tombstones leave the recorded flag false and keep prior
    /// behavior.
    #[serde(default)]
    pub committed_terminal_evidence_offset: Option<u64>,
    #[serde(default)]
    pub committed_terminal_evidence_offset_recorded: bool,
    /// Wall-clock ms of the commit (the chokepoint's clock). Also the GC key.
    pub committed_at_ms: u64,
}

/// Persist a terminal-commit tombstone. Best-effort by design (a failed write
/// degrades to the conservative `⚠`-after-cap path, never a false `✅`); the
/// stem is keyed on the write instant PLUS a process-monotonic sequence so
/// same-ms commits on one channel never overwrite earlier still-retained
/// evidence (codex r3 — the ms-only stem ERASED the first commit's tombstone;
/// the seq resets per process, but a restart spans more than one ms).
pub(in crate::services::discord) fn record_commit_tombstone(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    committed_user_msg_id: u64,
    committed_started_at: &str,
) {
    record_commit_tombstone_with_offset(
        provider,
        tmux_session_name,
        channel_id,
        committed_user_msg_id,
        committed_started_at,
        None,
    );
}

pub(in crate::services::discord) fn record_commit_tombstone_with_offset(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    committed_user_msg_id: u64,
    committed_started_at: &str,
    committed_turn_start_offset: Option<u64>,
) {
    record_commit_tombstone_at_with_offsets(
        super::now_ms(),
        provider,
        tmux_session_name,
        channel_id,
        committed_user_msg_id,
        committed_started_at,
        committed_turn_start_offset,
        false,
        None,
    );
}

pub(in crate::services::discord) fn record_commit_tombstone_with_offsets(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    committed_user_msg_id: u64,
    committed_started_at: &str,
    committed_turn_start_offset: Option<u64>,
    committed_terminal_evidence_offset: Option<u64>,
) {
    record_commit_tombstone_at_with_offsets(
        super::now_ms(),
        provider,
        tmux_session_name,
        channel_id,
        committed_user_msg_id,
        committed_started_at,
        committed_turn_start_offset,
        true,
        committed_terminal_evidence_offset,
    );
}

pub(in crate::services::discord) fn record_commit_tombstone_at(
    now_ms: u64,
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    committed_user_msg_id: u64,
    committed_started_at: &str,
) {
    record_commit_tombstone_at_with_offset(
        now_ms,
        provider,
        tmux_session_name,
        channel_id,
        committed_user_msg_id,
        committed_started_at,
        None,
    );
}

pub(in crate::services::discord) fn record_commit_tombstone_at_with_offset(
    now_ms: u64,
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    committed_user_msg_id: u64,
    committed_started_at: &str,
    committed_turn_start_offset: Option<u64>,
) {
    record_commit_tombstone_at_with_offsets(
        now_ms,
        provider,
        tmux_session_name,
        channel_id,
        committed_user_msg_id,
        committed_started_at,
        committed_turn_start_offset,
        false,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn record_commit_tombstone_at_with_offsets(
    now_ms: u64,
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    committed_user_msg_id: u64,
    committed_started_at: &str,
    committed_turn_start_offset: Option<u64>,
    committed_terminal_evidence_offset_recorded: bool,
    committed_terminal_evidence_offset: Option<u64>,
) {
    let Some(root) = tombstone_root() else {
        return; // tests / unconfigured root — nothing durable to write
    };
    let tombstone = CommitTombstone {
        provider: provider.to_string(),
        channel_id,
        tmux_session_name: tmux_session_name.to_string(),
        committed_user_msg_id,
        committed_started_at: committed_started_at.to_string(),
        committed_turn_start_offset,
        committed_terminal_evidence_offset,
        committed_terminal_evidence_offset_recorded,
        committed_at_ms: now_ms,
    };
    static STEM_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = STEM_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = root.join(format!("{provider}_{channel_id}_{now_ms}_{seq}.json"));
    let written = serde_json::to_string_pretty(&tombstone)
        .map_err(|e| e.to_string())
        .and_then(|data| {
            crate::services::discord::runtime_store::critical_atomic_write(
                &path,
                &data,
                crate::services::discord::runtime_store::AtomicWriteContext::new(
                    "tui_direct_commit_tombstone",
                )
                .provider(provider)
                .channel_id(channel_id),
            )
        });
    if let Err(error) = written {
        tracing::warn!(
            provider,
            channel_id,
            committed_user_msg_id,
            error = %error,
            "tui_direct_abort_marker: failed to persist commit tombstone; a racing marker degrades to the bounded ⚠ fallback (#3296 r2)"
        );
    }
}

/// Every parseable tombstone as `(path, tombstone)`. Unparseable JSON is
/// deleted outright (unlike markers there is nothing to reconcile from
/// corrupt evidence — losing it degrades to the conservative `⚠`).
fn tombstone_files() -> Vec<(std::path::PathBuf, CommitTombstone)> {
    let Some(root) = tombstone_root() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue; // transient read failure — retry next pass
        };
        match serde_json::from_str::<CommitTombstone>(&text) {
            Ok(t) => out.push((path, t)),
            Err(error) => {
                let removed = std::fs::remove_file(&path);
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    removed_ok = removed.is_ok(),
                    "tui_direct_abort_marker: unparseable commit tombstone deleted (#3296 r2)"
                );
            }
        }
    }
    out
}

/// Retained tombstones for one `(provider, channel)` — the 대조 working set.
pub(in crate::services::discord) fn load_commit_tombstones(
    provider: &str,
    channel_id: u64,
) -> Vec<CommitTombstone> {
    tombstone_files()
        .into_iter()
        .map(|(_, t)| t)
        .filter(|t| t.channel_id == channel_id && t.provider.eq_ignore_ascii_case(provider))
        .collect()
}

/// Drop tombstones older than [`COMMIT_TOMBSTONE_RETENTION_MS`]. Called at the
/// END of each sweep pass (post-대조, see the retention const's rationale).
pub(in crate::services::discord) fn gc_expired_commit_tombstones(now_ms: u64) {
    for (path, t) in tombstone_files() {
        if now_ms.saturating_sub(t.committed_at_ms) >= COMMIT_TOMBSTONE_RETENTION_MS {
            let _ = std::fs::remove_file(&path);
        }
    }
}

// ---------------------------------------------------------------------------
// Per-marker claim (verify r1 fix #2 — the inflight.rs sidecar-flock pattern)
// ---------------------------------------------------------------------------

/// Held for the whole claim → re-read → react → delete/restamp span of one
/// reconciler pass. Crash-safe (the kernel releases a flock with the process,
/// so a mid-claim crash never orphans the marker — unlike a rename-claim).
pub(in crate::services::discord) struct MarkerClaim {
    _file: std::fs::File,
}

impl Drop for MarkerClaim {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // Best effort unlock; closing the fd would release it anyway.
            let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

/// NON-BLOCKING exclusive claim on one marker's `<stem>.json.lock` sidecar.
/// `None` means the OTHER reconciler (drain vs sweep) owns the marker this
/// instant — skip; the loser's pass is idempotent and retries later. The claim
/// is held across the Discord delivery deliberately: it is a file flock (no
/// `await_holding_lock` site) and the only possible waiter skips, not blocks.
pub(in crate::services::discord) fn try_claim_marker(
    marker: &AbortedAnchorMarker,
) -> Option<MarkerClaim> {
    let root = root()?;
    let lock_path = root.join(format!("{}.json.lock", marker.file_stem()));
    std::fs::create_dir_all(&root).ok()?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .ok()?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return None;
        }
    }
    Some(MarkerClaim { _file: file })
}
