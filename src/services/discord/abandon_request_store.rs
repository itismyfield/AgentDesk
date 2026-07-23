//! #3859: durable abandon-request store for stranded "🔄 처리 중" placeholders.
//!
//! A turn starts by posting a "🔄 처리 중" placeholder and persisting an inflight
//! row. On the FAILURE path — the turn-task `InflightCleanupGuard` Drop (panic /
//! early-return) or the heartbeat-gap `inflight_heartbeat_sweeper` — that SYNC
//! site must evict the inflight row, but it cannot drive the async Discord edit
//! needed to finalize the placeholder. The pre-#3859 code simply deleted the row,
//! which STRANDED the placeholder forever (the `placeholder_sweeper` can only
//! reach a placeholder through its still-present inflight row).
//!
//! Rather than KEEP the inflight row alive as a placeholder carrier (which couples
//! the abandon to the inflight lifecycle and forces an unbounded liveness-vs-
//! housekeeping write enumeration), this store records the placeholder
//! `(channel_id, msg_id, started_at, current_tool_line)` DURABLY and independent
//! of the inflight lifecycle — exactly the pattern `status_panel_orphan_store`
//! (#3003) uses for orphaned panel deletes. The failure-path site enqueues a
//! record and deletes the inflight row IMMEDIATELY (freeing the channel, like the
//! pre-#3859 path). The `placeholder_sweeper` then [`drain`]s this store and edits
//! each placeholder to its terminal "중단됨" card BY MESSAGE ID.
//!
//! Why this dissolves the whole bug class:
//!   * the inflight row is gone, so there is no flag-on-live-row race, no
//!     liveness/housekeeping clear enumeration, and no GC/loader mismatch;
//!   * a re-adopt creates a NEW inflight row + NEW placeholder (a different
//!     `msg_id`), so it never collides with the queued abandon (old `msg_id`);
//!   * the drain re-probes the message before editing — `AlreadyDelivered` /
//!     `MessageGone` consume the record WITHOUT clobbering delivered content, and
//!     a live inflight row that still anchors this exact `msg_id` defers the edit.
//!
//! Layout (atomic temp+rename writes, mirroring `status_panel_orphan_store`):
//!
//! ```text
//! runtime/discord_abandon_requests/<provider>/<token_hash>/<channel_id>.json
//! ```
//!
//! Each file holds a JSON array of [`AbandonRecord`]s scoped to that channel.
//! `token_hash` scoping keeps one bot's sweeper from editing another bot's
//! message (a cross-bot edit would 403 forever and leak the record).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};

use super::formatting::{
    MonitorHandoffReason, MonitorHandoffStatus, build_monitor_handoff_placeholder,
};
use super::placeholder_sweeper::{PlaceholderProbe, probe_placeholder_state};
use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;

/// Terminal state requested for a stranded live card. Legacy records predate
/// this field and therefore deserialize as `Aborted`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum TerminalCardStatus {
    Completed,
    #[default]
    Aborted,
}

/// One stranded live card awaiting a terminal edit. `msg_id` is the dedup key
/// within a channel; the remaining fields render the terminal card without an
/// inflight row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(in crate::services::discord) struct AbandonRecord {
    pub msg_id: u64,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub current_tool_line: Option<String>,
    #[serde(default)]
    pub terminal_status: TerminalCardStatus,
}

/// Serializes the read-modify-write of `enqueue`/`remove` across the failure-path
/// sites (Drop guard / heartbeat sweeper) and the sweeper drain that all touch
/// this store concurrently. The critical section is purely synchronous file IO;
/// per-file `atomic_write` keeps individual writes crash-safe, this lock keeps two
/// concurrent RMW cycles from clobbering each other. (Mirrors
/// `status_panel_orphan_store::STORE_WRITE_LOCK`.)
static STORE_WRITE_LOCK: Mutex<()> = Mutex::new(());

fn provider_dir_in_root(root: &Path, provider: &ProviderKind, token_hash: &str) -> PathBuf {
    root.join(provider.as_str()).join(token_hash)
}

fn channel_file_path_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> PathBuf {
    provider_dir_in_root(root, provider, token_hash).join(format!("{channel_id}.json"))
}

fn load_channel_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> Vec<AbandonRecord> {
    let path = channel_file_path_in_root(root, provider, token_hash, channel_id);
    let Ok(raw) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<AbandonRecord>>(&raw).unwrap_or_default()
}

/// #3859 r5: returns `Err` when the durable write FAILED (ENOSPC / permission /
/// path-conflict). The caller MUST surface this so the failure-path site does not
/// delete the inflight row without a persisted record (which would re-strand the
/// placeholder forever — the original #3859 bug on the error path). The empty-set
/// case (`remove`) is best-effort and returns `Ok`.
fn save_channel_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    records: &[AbandonRecord],
) -> Result<(), String> {
    let path = channel_file_path_in_root(root, provider, token_hash, channel_id);
    if records.is_empty() {
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    let json = serde_json::to_string_pretty(records).map_err(|e| e.to_string())?;
    runtime_store::atomic_write(&path, &json)
}

/// `Ok(())` means the record is DURABLY queued (or was already present, or the
/// ids are degenerate so there is no real Discord message to finalize). `Err`
/// means the durable write failed and NOTHING was persisted — the caller must not
/// delete the inflight row.
fn enqueue_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    record: AbandonRecord,
) -> Result<(), String> {
    if channel_id == 0 || record.msg_id == 0 {
        // No addressable Discord message → nothing to finalize, nothing to strand.
        return Ok(());
    }
    let _guard = STORE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut records = load_channel_in_root(root, provider, token_hash, channel_id);
    // Idempotent set semantics keyed by msg_id: a heartbeat sweeper that
    // re-observes the same stranded placeholder must not grow the file.
    if let Some(existing) = records.iter_mut().find(|r| r.msg_id == record.msg_id) {
        if existing.terminal_status == TerminalCardStatus::Aborted
            && record.terminal_status == TerminalCardStatus::Completed
        {
            *existing = record;
            return save_channel_in_root(root, provider, token_hash, channel_id, &records);
        }
        return Ok(());
    }
    records.push(record);
    save_channel_in_root(root, provider, token_hash, channel_id, &records)
}

fn remove_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    msg_id: u64,
) {
    let _guard = STORE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut records = load_channel_in_root(root, provider, token_hash, channel_id);
    let before = records.len();
    records.retain(|r| r.msg_id != msg_id);
    if records.len() != before {
        // Best-effort: a failed re-save leaves the record, which the next drain
        // re-processes idempotently (the card is already terminal → probe
        // AlreadyDelivered → consume). Unlike enqueue, a remove failure cannot
        // strand a placeholder, so it is not surfaced.
        let _ = save_channel_in_root(root, provider, token_hash, channel_id, &records);
    }
}

fn is_queued_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    msg_id: u64,
) -> bool {
    load_channel_in_root(root, provider, token_hash, channel_id)
        .iter()
        .any(|r| r.msg_id == msg_id)
}

fn load_pending_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
) -> Vec<(u64, AbandonRecord)> {
    let dir = provider_dir_in_root(root, provider, token_hash);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(channel_id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse::<u64>().ok())
        else {
            continue;
        };
        for record in load_channel_in_root(root, provider, token_hash, channel_id) {
            out.push((channel_id, record));
        }
    }
    out
}

/// Record a stranded placeholder for durable finalize. Idempotent (set semantics
/// keyed by `(channel_id, msg_id)`). #3859 r5: returns `Err` when the durable
/// write failed (or no runtime root is resolvable) so the failure-path caller can
/// PRESERVE the inflight row instead of deleting it without a record.
pub(in crate::services::discord) fn enqueue(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    record: AbandonRecord,
) -> Result<(), String> {
    let Some(root) = runtime_store::discord_abandon_requests_root() else {
        return Err("abandon-requests runtime root unavailable".to_string());
    };
    enqueue_in_root(&root, provider, token_hash, channel_id, record)
}

/// All pending `(channel_id, record)` abandon requests for this bot.
pub(in crate::services::discord) fn load_pending(
    provider: &ProviderKind,
    token_hash: &str,
) -> Vec<(u64, AbandonRecord)> {
    let Some(root) = runtime_store::discord_abandon_requests_root() else {
        return Vec::new();
    };
    load_pending_in_root(&root, provider, token_hash)
}

/// Drop a record once its placeholder has been finalized (or is permanently
/// gone / already delivered). No-op when absent.
pub(in crate::services::discord) fn remove(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    msg_id: u64,
) {
    let Some(root) = runtime_store::discord_abandon_requests_root() else {
        return;
    };
    remove_in_root(&root, provider, token_hash, channel_id, msg_id);
}

/// Is this placeholder still queued? Used by [`drain`] to re-validate a record
/// immediately before editing, so a record removed since the snapshot is skipped.
pub(in crate::services::discord) fn is_queued(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    msg_id: u64,
) -> bool {
    let Some(root) = runtime_store::discord_abandon_requests_root() else {
        return false;
    };
    is_queued_in_root(&root, provider, token_hash, channel_id, msg_id)
}

/// Render the requested terminal card for a record (no inflight row).
fn build_terminal_card(record: &AbandonRecord) -> String {
    let started_at_unix = super::inflight::parse_started_at_unix(&record.started_at)
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    let status = match record.terminal_status {
        TerminalCardStatus::Completed => MonitorHandoffStatus::Completed,
        TerminalCardStatus::Aborted => MonitorHandoffStatus::Aborted,
    };
    build_monitor_handoff_placeholder(
        status,
        MonitorHandoffReason::AsyncDispatch,
        started_at_unix,
        record.current_tool_line.as_deref(),
        None,
    )
}

/// Pure defer decision: defer the abandon edit while a LIVE inflight row still
/// anchors this exact `msg_id` as its placeholder (a re-adopt reused the id, or a
/// late in-turn completion is editing it). Split out for unit testing.
fn abandon_drain_defers_for_live_anchor(current_msg_id: Option<u64>, msg_id: u64) -> bool {
    msg_id != 0 && current_msg_id == Some(msg_id)
}

/// Finalize every pending stranded placeholder once. `StillPlaceholder` → edit to
/// the "중단됨" card (committed → drop the record; transient edit failure → keep
/// for the next drain). `AlreadyDelivered` / `MessageGone` → consume the record
/// WITHOUT clobbering (the message already carries real content or is gone). A
/// live inflight row still anchoring the `msg_id` defers. Returns the number of
/// records cleared this pass. Mirrors `status_panel_orphan_store::drain`.
pub(in crate::services::discord) async fn drain(
    http: &Arc<serenity::Http>,
    shared: &Arc<crate::services::discord::SharedData>,
    provider: &ProviderKind,
    token_hash: &str,
) -> usize {
    let pending = load_pending(provider, token_hash);
    let mut cleared = 0usize;
    for (channel_id, record) in pending {
        // Re-validate against the live store: a record removed between the
        // snapshot and here (e.g. a peer drain finalized it) must be skipped.
        if !is_queued(provider, token_hash, channel_id, record.msg_id) {
            continue;
        }
        // Defer while a live inflight row still anchors this exact message. The
        // same durable store also carries separate status-panel reconciliation,
        // so both assistant placeholders and status panels participate in this
        // ownership fence.
        let inflight = super::inflight::load_inflight_state(provider, channel_id);
        let inflight_anchor = inflight.as_ref().map(|state| state.current_msg_id);
        let live_status_panel_anchor =
            inflight.as_ref().and_then(|state| state.status_message_id) == Some(record.msg_id);
        if abandon_drain_defers_for_live_anchor(inflight_anchor, record.msg_id)
            || live_status_panel_anchor
        {
            continue;
        }
        let probe = probe_placeholder_state(http, channel_id, record.msg_id).await;
        match probe {
            PlaceholderProbe::StillPlaceholder => {
                let text = build_terminal_card(&record);
                let channel = serenity::ChannelId::new(channel_id);
                let message = serenity::MessageId::new(record.msg_id);
                match super::gateway::edit_outbound_message(
                    http.clone(),
                    shared.clone(),
                    channel,
                    message,
                    &text,
                )
                .await
                {
                    Ok(_) => {
                        remove(provider, token_hash, channel_id, record.msg_id);
                        cleared += 1;
                        tracing::info!(
                            "[abandon_request_store] finalized stranded placeholder {}/{} → 중단됨",
                            channel_id,
                            record.msg_id
                        );
                    }
                    Err(err) => {
                        tracing::debug!(
                            "[abandon_request_store] finalize edit for {channel_id}/{} failed \
                             transiently — keeping for next drain: {err}",
                            record.msg_id
                        );
                    }
                }
            }
            PlaceholderProbe::AlreadyDelivered | PlaceholderProbe::MessageGone => {
                // Real content already on screen, or the message is permanently
                // gone — do NOT clobber; just consume the record.
                remove(provider, token_hash, channel_id, record.msg_id);
                cleared += 1;
            }
            PlaceholderProbe::ProbeFailed => {
                // Transient — keep for the next drain.
            }
        }
    }
    cleared
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(msg_id: u64) -> AbandonRecord {
        AbandonRecord {
            msg_id,
            started_at: "2026-05-17 10:00:00".to_string(),
            current_tool_line: None,
            terminal_status: TerminalCardStatus::Aborted,
        }
    }

    #[test]
    fn enqueue_is_idempotent_and_removable() {
        let root = tempfile::tempdir().expect("tempdir");
        let root = root.path();
        let provider = ProviderKind::Codex;
        let token = "tok";
        enqueue_in_root(root, &provider, token, 100, rec(5001)).expect("enqueue");
        enqueue_in_root(root, &provider, token, 100, rec(5001)).expect("dup enqueue"); // duplicate
        enqueue_in_root(root, &provider, token, 100, rec(5002)).expect("enqueue");
        let mut pending: Vec<u64> = load_pending_in_root(root, &provider, token)
            .into_iter()
            .map(|(_, r)| r.msg_id)
            .collect();
        pending.sort();
        assert_eq!(pending, vec![5001, 5002]);

        remove_in_root(root, &provider, token, 100, 5001);
        let pending: Vec<u64> = load_pending_in_root(root, &provider, token)
            .into_iter()
            .map(|(_, r)| r.msg_id)
            .collect();
        assert_eq!(pending, vec![5002]);

        // Removing the last record deletes the channel file.
        remove_in_root(root, &provider, token, 100, 5002);
        assert!(load_pending_in_root(root, &provider, token).is_empty());
    }

    #[test]
    fn completed_terminal_request_upgrades_abort_for_same_panel() {
        let root = tempfile::tempdir().expect("tempdir");
        let root = root.path();
        let provider = ProviderKind::Claude;
        enqueue_in_root(root, &provider, "tok", 100, rec(5001)).expect("abort enqueue");
        enqueue_in_root(
            root,
            &provider,
            "tok",
            100,
            AbandonRecord {
                terminal_status: TerminalCardStatus::Completed,
                ..rec(5001)
            },
        )
        .expect("completion upgrade");

        let pending = load_pending_in_root(root, &provider, "tok");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1.terminal_status, TerminalCardStatus::Completed);
        assert!(build_terminal_card(&pending[0].1).contains("응답 완료"));
    }

    #[test]
    fn legacy_record_defaults_to_aborted_terminal_status() {
        let record: AbandonRecord = serde_json::from_str(
            r#"{"msg_id":7,"started_at":"2026-05-17 09:00:00","current_tool_line":null}"#,
        )
        .expect("legacy record");
        assert_eq!(record.terminal_status, TerminalCardStatus::Aborted);
    }

    /// #3859 r5: a failed durable write surfaces as `Err` (so the failure-path
    /// caller preserves the inflight row instead of stranding the placeholder).
    #[test]
    fn enqueue_surfaces_write_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Make the store root a FILE so `create_dir_all(root/provider/token)` —
        // and thus the atomic_write — fails.
        let bad_root = tmp.path().join("not_a_dir");
        std::fs::write(&bad_root, b"x").expect("seed file at root path");
        let provider = ProviderKind::Claude;
        assert!(
            enqueue_in_root(&bad_root, &provider, "tok", 100, rec(5001)).is_err(),
            "atomic_write failure must surface as Err"
        );
    }

    #[test]
    fn enqueue_skips_zero_ids_and_scopes_by_token() {
        let root = tempfile::tempdir().expect("tempdir");
        let root = root.path();
        let provider = ProviderKind::Claude;
        // Degenerate ids are a no-op success (no addressable message).
        enqueue_in_root(root, &provider, "tok2", 0, rec(5001)).expect("zero channel ok");
        enqueue_in_root(root, &provider, "tok2", 100, rec(0)).expect("zero msg ok");
        assert!(load_pending_in_root(root, &provider, "tok2").is_empty());

        // token_hash scoping isolates bots sharing a provider.
        enqueue_in_root(root, &provider, "bot_a", 100, rec(5001)).expect("enqueue");
        enqueue_in_root(root, &provider, "bot_b", 100, rec(6001)).expect("enqueue");
        let a: Vec<u64> = load_pending_in_root(root, &provider, "bot_a")
            .into_iter()
            .map(|(_, r)| r.msg_id)
            .collect();
        let b: Vec<u64> = load_pending_in_root(root, &provider, "bot_b")
            .into_iter()
            .map(|(_, r)| r.msg_id)
            .collect();
        assert_eq!(a, vec![5001]);
        assert_eq!(b, vec![6001]);
    }

    #[test]
    fn record_round_trips_started_at_and_tool_line() {
        let root = tempfile::tempdir().expect("tempdir");
        let root = root.path();
        let provider = ProviderKind::Claude;
        enqueue_in_root(
            root,
            &provider,
            "tok",
            100,
            AbandonRecord {
                msg_id: 7,
                started_at: "2026-05-17 09:00:00".to_string(),
                current_tool_line: Some("⚙ Bash: cargo build".to_string()),
                terminal_status: TerminalCardStatus::Aborted,
            },
        )
        .expect("enqueue");
        let pending = load_pending_in_root(root, &provider, "tok");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1.started_at, "2026-05-17 09:00:00");
        assert_eq!(
            pending[0].1.current_tool_line.as_deref(),
            Some("⚙ Bash: cargo build")
        );
    }

    #[test]
    fn defer_only_for_exact_live_anchor() {
        assert!(abandon_drain_defers_for_live_anchor(Some(5555), 5555));
        assert!(!abandon_drain_defers_for_live_anchor(Some(0), 0));
        assert!(!abandon_drain_defers_for_live_anchor(Some(9999), 5555));
        assert!(!abandon_drain_defers_for_live_anchor(None, 5555));
    }

    #[test]
    fn abandoned_card_renders_terminal_marker() {
        let card = build_terminal_card(&rec(9001));
        // Aborted handoff header — the terminal "중단됨" card the drain edits in.
        assert!(card.contains("응답 중단"), "card was: {card}");
    }
}
