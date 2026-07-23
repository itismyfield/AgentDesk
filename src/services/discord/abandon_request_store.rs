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

/// Durable identity of the turn episode that owned a live card. All fields are
/// additive defaults so records written before episode fencing remain readable.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(in crate::services::discord) struct AbandonEpisodeIdentity {
    #[serde(default)]
    pub user_msg_id: u64,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub status_panel_generation: u64,
    #[serde(default)]
    pub save_generation: u64,
}

impl AbandonEpisodeIdentity {
    fn is_legacy(&self) -> bool {
        self.user_msg_id == 0
            && self.started_at.is_empty()
            && self.status_panel_generation == 0
            && self.save_generation == 0
    }

    fn same_episode(&self, other: &Self) -> bool {
        !self.is_legacy()
            && !other.is_legacy()
            && self.user_msg_id == other.user_msg_id
            && self.started_at == other.started_at
    }
}

/// One stranded live card awaiting a terminal edit. `(msg_id, episode)` is the
/// ownership key within a channel; the remaining fields render the terminal card
/// without an inflight row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(in crate::services::discord) struct AbandonRecord {
    pub msg_id: u64,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub current_tool_line: Option<String>,
    #[serde(default)]
    pub terminal_status: TerminalCardStatus,
    #[serde(default)]
    pub episode: AbandonEpisodeIdentity,
}

impl AbandonRecord {
    fn same_queued_record(&self, other: &Self) -> bool {
        self == other
    }
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
    // A reused message id belongs to exactly one episode. Status dominance is
    // monotonic only inside that episode; a newer/different episode replaces the
    // old record without inheriting its terminal status.
    if let Some(existing) = records.iter_mut().find(|r| r.msg_id == record.msg_id) {
        if existing.episode.same_episode(&record.episode) {
            let dominant_status = if existing.terminal_status == TerminalCardStatus::Completed
                || record.terminal_status == TerminalCardStatus::Completed
            {
                TerminalCardStatus::Completed
            } else {
                TerminalCardStatus::Aborted
            };
            let existing_revision = (
                existing.episode.status_panel_generation,
                existing.episode.save_generation,
            );
            let record_revision = (
                record.episode.status_panel_generation,
                record.episode.save_generation,
            );
            if record_revision >= existing_revision {
                *existing = record;
            }
            if existing.terminal_status != dominant_status {
                existing.terminal_status = dominant_status;
            }
            return save_channel_in_root(root, provider, token_hash, channel_id, &records);
        }
        *existing = record;
        return save_channel_in_root(root, provider, token_hash, channel_id, &records);
    }
    records.push(record);
    save_channel_in_root(root, provider, token_hash, channel_id, &records)
}

fn remove_matching_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    matches: impl Fn(&AbandonRecord) -> bool,
) {
    let _guard = STORE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut records = load_channel_in_root(root, provider, token_hash, channel_id);
    let before = records.len();
    records.retain(|record| !matches(record));
    if records.len() != before {
        // Best-effort: a failed re-save leaves the record, which the next drain
        // re-processes idempotently (the card is already terminal → probe
        // AlreadyDelivered → consume). Unlike enqueue, a remove failure cannot
        // strand a placeholder, so it is not surfaced.
        let _ = save_channel_in_root(root, provider, token_hash, channel_id, &records);
    }
}

fn remove_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    msg_id: u64,
) {
    remove_matching_in_root(root, provider, token_hash, channel_id, |record| {
        record.msg_id == msg_id
    });
}

fn remove_record_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    target: &AbandonRecord,
) {
    remove_matching_in_root(root, provider, token_hash, channel_id, |record| {
        record.same_queued_record(target)
    });
}

fn is_record_queued_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    target: &AbandonRecord,
) -> bool {
    load_channel_in_root(root, provider, token_hash, channel_id)
        .iter()
        .any(|record| record.same_queued_record(target))
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
/// keyed by `(channel_id, msg_id, episode)`, with a same-message replacement when
/// a different episode takes ownership). #3859 r5: returns `Err` when the durable
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

/// Is this exact episode record still queued? Used by [`drain`] to re-validate
/// ownership immediately before editing, so a replacement record for the same
/// message id cannot authorize a stale snapshot.
fn is_record_queued(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    record: &AbandonRecord,
) -> bool {
    let Some(root) = runtime_store::discord_abandon_requests_root() else {
        return false;
    };
    is_record_queued_in_root(&root, provider, token_hash, channel_id, record)
}

fn remove_record(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    record: &AbandonRecord,
) {
    let Some(root) = runtime_store::discord_abandon_requests_root() else {
        return;
    };
    remove_record_in_root(&root, provider, token_hash, channel_id, record);
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

fn inflight_matches_record_episode(
    state: &super::inflight::InflightTurnState,
    record: &AbandonRecord,
) -> bool {
    !record.episode.is_legacy()
        && state.user_msg_id == record.episode.user_msg_id
        && state.started_at == record.episode.started_at
}

fn inflight_matches_record_revision(
    state: &super::inflight::InflightTurnState,
    record: &AbandonRecord,
) -> bool {
    inflight_matches_record_episode(state, record)
        && state.status_panel_generation == record.episode.status_panel_generation
        && state.save_generation == record.episode.save_generation
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainOwnership {
    Editable,
    DeferSameRevision,
    DeferSameEpisodeNewRevision,
    DropForNewerOwner,
}

fn drain_ownership(
    inflight: Option<&super::inflight::InflightTurnState>,
    record: &AbandonRecord,
) -> DrainOwnership {
    let Some(state) = inflight else {
        return DrainOwnership::Editable;
    };
    let anchors_message =
        state.current_msg_id == record.msg_id || state.status_message_id == Some(record.msg_id);
    if !anchors_message {
        return DrainOwnership::Editable;
    }
    if inflight_matches_record_revision(state, record) {
        DrainOwnership::DeferSameRevision
    } else if inflight_matches_record_episode(state, record) {
        // A newer revision of the same episode still owns the surface. It blocks
        // editing but keeps the durable terminal request for after the row clears.
        DrainOwnership::DeferSameEpisodeNewRevision
    } else {
        DrainOwnership::DropForNewerOwner
    }
}

fn current_drain_ownership(
    provider: &ProviderKind,
    channel_id: u64,
    record: &AbandonRecord,
) -> DrainOwnership {
    drain_ownership(
        super::inflight::load_inflight_state(provider, channel_id).as_ref(),
        record,
    )
}

fn clear_record_for_live_owner_race(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    record: &AbandonRecord,
    after_edit: bool,
    live_events: Option<&super::placeholder_live_events::PlaceholderLiveEvents>,
) {
    remove_record(provider, token_hash, channel_id, record);
    if after_edit {
        if let Some(live_events) = live_events {
            live_events.invalidate_panel_cache(serenity::ChannelId::new(channel_id), record.msg_id);
        }
        // Discord offers no compare-and-swap edit. If ownership changed during the
        // HTTP await, force the live bridge/watcher owner past its byte-stable cache
        // gate on the next tick so the stale terminal card is deterministically healed.
        tracing::warn!(
            target: "agentdesk::discord::live_panel",
            provider = provider.as_str(),
            channel_id,
            message_id = record.msg_id,
            record_user_msg_id = record.episode.user_msg_id,
            record_status_panel_generation = record.episode.status_panel_generation,
            "abandon drain edit committed after live ownership appeared during the HTTP await; removed the stale record and invalidated the live owner's panel cache"
        );
    }
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
        // Re-validate the exact episode record: a same-message replacement must
        // not let this stale snapshot pass the queue fence.
        if !is_record_queued(provider, token_hash, channel_id, &record) {
            continue;
        }
        match current_drain_ownership(provider, channel_id, &record) {
            DrainOwnership::Editable => {}
            DrainOwnership::DeferSameRevision | DrainOwnership::DeferSameEpisodeNewRevision => {
                continue;
            }
            DrainOwnership::DropForNewerOwner => {
                clear_record_for_live_owner_race(
                    provider, token_hash, channel_id, &record, false, None,
                );
                cleared += 1;
                continue;
            }
        }
        let probe = probe_placeholder_state(http, channel_id, record.msg_id).await;
        match probe {
            PlaceholderProbe::StillPlaceholder => {
                // Close the probe→edit ownership window. The probe establishes
                // shape only; this second fence establishes the owning episode.
                let text = build_terminal_card(&record);
                let channel = serenity::ChannelId::new(channel_id);
                let message = serenity::MessageId::new(record.msg_id);
                if !is_record_queued(provider, token_hash, channel_id, &record) {
                    continue;
                }
                // Keep the final owner check adjacent to the outbound call. The
                // gateway can still await its rate lane, so Discord cannot provide a
                // zero-width ownership window; the post-edit fence below detects and
                // bounds any race that commits during that await.
                match current_drain_ownership(provider, channel_id, &record) {
                    DrainOwnership::Editable => {}
                    DrainOwnership::DeferSameRevision
                    | DrainOwnership::DeferSameEpisodeNewRevision => continue,
                    DrainOwnership::DropForNewerOwner => {
                        clear_record_for_live_owner_race(
                            provider, token_hash, channel_id, &record, false, None,
                        );
                        cleared += 1;
                        continue;
                    }
                }
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
                        let live_owner_raced = !matches!(
                            current_drain_ownership(provider, channel_id, &record),
                            DrainOwnership::Editable
                        );
                        if live_owner_raced {
                            clear_record_for_live_owner_race(
                                provider,
                                token_hash,
                                channel_id,
                                &record,
                                true,
                                Some(shared.ui.placeholder_live_events.as_ref()),
                            );
                        } else {
                            remove_record(provider, token_hash, channel_id, &record);
                        }
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
                remove_record(provider, token_hash, channel_id, &record);
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

    fn rec_for_episode(msg_id: u64, user_msg_id: u64, started_at: &str) -> AbandonRecord {
        AbandonRecord {
            msg_id,
            started_at: started_at.to_string(),
            current_tool_line: None,
            terminal_status: TerminalCardStatus::Aborted,
            episode: AbandonEpisodeIdentity {
                user_msg_id,
                started_at: started_at.to_string(),
                status_panel_generation: 1,
                save_generation: 1,
            },
        }
    }

    fn rec(msg_id: u64) -> AbandonRecord {
        rec_for_episode(msg_id, 7001, "2026-05-17 10:00:00")
    }

    fn inflight_for_record(record: &AbandonRecord) -> super::super::inflight::InflightTurnState {
        let mut state = super::super::inflight::InflightTurnState::new(
            ProviderKind::Claude,
            100,
            None,
            1,
            record.episode.user_msg_id,
            record.msg_id,
            "turn".to_string(),
            Some("session".to_string()),
            Some("tmux".to_string()),
            Some("/tmp/abandon-episode.jsonl".to_string()),
            None,
            10,
        );
        state.started_at = record.episode.started_at.clone();
        state.status_message_id = Some(record.msg_id);
        state.status_panel_generation = record.episode.status_panel_generation;
        state.save_generation = record.episode.save_generation;
        state
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
    fn reused_message_new_episode_does_not_inherit_completed_status() {
        let root = tempfile::tempdir().expect("tempdir");
        let provider = ProviderKind::Claude;
        let mut turn_a = rec_for_episode(5001, 7001, "2026-05-17 10:00:00");
        turn_a.terminal_status = TerminalCardStatus::Completed;
        enqueue_in_root(root.path(), &provider, "tok", 100, turn_a).expect("turn A enqueue");

        let turn_b = rec_for_episode(5001, 7002, "2026-05-17 10:01:00");
        enqueue_in_root(root.path(), &provider, "tok", 100, turn_b.clone())
            .expect("turn B enqueue");

        let pending = load_pending_in_root(root.path(), &provider, "tok");
        assert_eq!(pending, vec![(100, turn_b)]);
        assert_eq!(pending[0].1.terminal_status, TerminalCardStatus::Aborted);
    }

    #[test]
    fn newer_live_owner_blocks_old_record_edit_and_consumes_it() {
        let old = rec_for_episode(5001, 7001, "2026-05-17 10:00:00");
        let newer = rec_for_episode(5001, 7002, "2026-05-17 10:01:00");
        let newer_inflight = inflight_for_record(&newer);

        assert_eq!(
            drain_ownership(Some(&newer_inflight), &old),
            DrainOwnership::DropForNewerOwner
        );
        assert_eq!(
            drain_ownership(Some(&newer_inflight), &newer),
            DrainOwnership::DeferSameRevision
        );

        let mut same_episode_new_revision = newer_inflight.clone();
        same_episode_new_revision.save_generation += 1;
        assert_eq!(
            drain_ownership(Some(&same_episode_new_revision), &newer),
            DrainOwnership::DeferSameEpisodeNewRevision
        );
    }

    #[test]
    fn post_edit_fence_detects_every_live_anchor_for_invalidation() {
        let old = rec_for_episode(5001, 7001, "2026-05-17 10:00:00");

        let newer = rec_for_episode(5001, 7002, "2026-05-17 10:01:00");
        let newer_inflight = inflight_for_record(&newer);
        assert_eq!(
            drain_ownership(Some(&newer_inflight), &old),
            DrainOwnership::DropForNewerOwner
        );

        let same_revision = inflight_for_record(&old);
        assert_eq!(
            drain_ownership(Some(&same_revision), &old),
            DrainOwnership::DeferSameRevision
        );

        let mut same_episode_new_revision = same_revision.clone();
        same_episode_new_revision.save_generation += 1;
        assert_eq!(
            drain_ownership(Some(&same_episode_new_revision), &old),
            DrainOwnership::DeferSameEpisodeNewRevision
        );
    }

    #[test]
    fn legacy_record_defaults_to_aborted_terminal_status_and_identity() {
        let record: AbandonRecord = serde_json::from_str(
            r#"{"msg_id":7,"started_at":"2026-05-17 09:00:00","current_tool_line":null}"#,
        )
        .expect("legacy record");
        assert_eq!(record.terminal_status, TerminalCardStatus::Aborted);
        assert!(record.episode.is_legacy());
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
                episode: AbandonEpisodeIdentity {
                    user_msg_id: 8,
                    started_at: "2026-05-17 09:00:00".to_string(),
                    status_panel_generation: 2,
                    save_generation: 3,
                },
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
