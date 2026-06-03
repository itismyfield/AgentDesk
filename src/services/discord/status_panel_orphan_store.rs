//! #3003: durable retry store for orphaned status-panel-v2 message deletes.
//!
//! The watcher reclaims a TUI-direct status panel inline when its turn ends, and
//! the placeholder sweeper reclaims panels left on lingering inflight rows. Both
//! fast paths can fail transiently (Discord 5xx / rate-limit / transport), and
//! when the owning inflight row has *already been cleared* — e.g. a
//! stopped/cancelled turn — there is no per-turn handle left to retry from, so
//! the panel would stay stuck at "계속 처리 중".
//!
//! This store records `(channel_id, panel_msg_id)` durably, independent of the
//! inflight lifecycle, so [`drain`] can retry the delete across sweeps and
//! restarts until it commits or the message is permanently gone (404/403/410).
//!
//! Layout (atomic temp+rename writes, mirroring `queued_placeholders_store`):
//!
//! ```text
//! runtime/discord_status_panel_orphans/<provider>/<token_hash>/<channel_id>.json
//! ```
//!
//! Each file holds a JSON array of panel message ids scoped to that channel.
//! `token_hash` scoping keeps one bot's sweeper from trying to delete another
//! bot's messages (a cross-bot delete would fail forever and leak the row).
//!
//! Path resolution is split into `*_in_root` helpers so tests inject an explicit
//! temp root instead of mutating the global `AGENTDESK_ROOT_DIR` env var.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use poise::serenity_prelude as serenity;

use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;

/// Serializes the read-modify-write of `enqueue`/`remove` across the watcher
/// tasks and the sweeper task that all touch this store concurrently (codex P2
/// r14). The critical section is purely synchronous file IO (no await), and
/// these operations only run on the rare delete-failure / drain paths, so a
/// single process-wide lock has negligible contention. Per-file `atomic_write`
/// keeps individual writes crash-safe; this lock keeps two concurrent
/// read-modify-write cycles from clobbering each other.
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
) -> Vec<u64> {
    let path = channel_file_path_in_root(root, provider, token_hash, channel_id);
    let Ok(raw) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<u64>>(&raw).unwrap_or_default()
}

fn save_channel_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    ids: &[u64],
) {
    let path = channel_file_path_in_root(root, provider, token_hash, channel_id);
    if ids.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(ids) {
        let _ = runtime_store::atomic_write(&path, &json);
    }
}

fn enqueue_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    if channel_id == 0 || panel_msg_id == 0 {
        return;
    }
    let _guard = STORE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut ids = load_channel_in_root(root, provider, token_hash, channel_id);
    if ids.contains(&panel_msg_id) {
        return;
    }
    ids.push(panel_msg_id);
    save_channel_in_root(root, provider, token_hash, channel_id, &ids);
}

fn remove_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    let _guard = STORE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut ids = load_channel_in_root(root, provider, token_hash, channel_id);
    let before = ids.len();
    ids.retain(|id| *id != panel_msg_id);
    if ids.len() != before {
        save_channel_in_root(root, provider, token_hash, channel_id, &ids);
    }
}

fn is_queued_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) -> bool {
    load_channel_in_root(root, provider, token_hash, channel_id).contains(&panel_msg_id)
}

fn load_pending_in_root(root: &Path, provider: &ProviderKind, token_hash: &str) -> Vec<(u64, u64)> {
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
        for id in load_channel_in_root(root, provider, token_hash, channel_id) {
            out.push((channel_id, id));
        }
    }
    out
}

/// Record a panel id for durable delete-retry. Idempotent (set semantics) so a
/// sweeper that re-observes the same orphan every pass does not grow the file.
pub(in crate::services::discord) fn enqueue(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    let Some(root) = runtime_store::discord_status_panel_orphans_root() else {
        return;
    };
    enqueue_in_root(&root, provider, token_hash, channel_id, panel_msg_id);
}

/// All pending `(channel_id, panel_msg_id)` records for this bot.
pub(in crate::services::discord) fn load_pending(
    provider: &ProviderKind,
    token_hash: &str,
) -> Vec<(u64, u64)> {
    let Some(root) = runtime_store::discord_status_panel_orphans_root() else {
        return Vec::new();
    };
    load_pending_in_root(&root, provider, token_hash)
}

/// Drop a record once its delete has committed (or the message is permanently
/// gone). No-op when the id is not present.
pub(in crate::services::discord) fn remove(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    let Some(root) = runtime_store::discord_status_panel_orphans_root() else {
        return;
    };
    remove_in_root(&root, provider, token_hash, channel_id, panel_msg_id);
}

/// Is this panel still queued for deletion? Used by [`drain`] to re-validate a
/// record immediately before deleting it, so a record the completion path
/// removed (the panel became valid) after the drain's snapshot is not deleted.
pub(in crate::services::discord) fn is_queued(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) -> bool {
    let Some(root) = runtime_store::discord_status_panel_orphans_root() else {
        return false;
    };
    is_queued_in_root(&root, provider, token_hash, channel_id, panel_msg_id)
}

fn delete_error_is_permanent(err: &serenity::Error) -> bool {
    matches!(err, serenity::Error::Http(http_err)
        if http_err
            .status_code()
            .is_some_and(|status| matches!(status.as_u16(), 404 | 403 | 410)))
}

/// EPIC #3078 PR-3 — the parity gate between the legacy orphan-store turn-aware
/// defer gate (the inflight-row `status_message_id == panel_msg_id` read) and the
/// `StatusPanelController`'s view of whether the live turn still owns this exact
/// orphan. They must agree: a divergence means the controller would defer (or not
/// defer) a delete the legacy gate would not, so routing the gate through it later
/// would change behaviour. `debug_assert` so test/dev builds fail loudly; release
/// builds emit a bounded `warn!` (no `panic!`) so a never-before-seen orphan shape
/// can never crash a production drain over the legacy gate, which continues to
/// decide the defer regardless.
fn assert_orphan_gate_parity(
    controller_owns: bool,
    legacy_owns: bool,
    channel_id: u64,
    panel_msg_id: u64,
) {
    if controller_owns == legacy_owns {
        return;
    }
    debug_assert_eq!(
        controller_owns, legacy_owns,
        "#3078 PR-3 orphan-store drain turn-aware-defer parity mismatch (channel {channel_id}, panel {panel_msg_id}): controller owns={controller_owns}, legacy owns={legacy_owns}"
    );
    tracing::warn!(
        channel = channel_id,
        panel = panel_msg_id,
        controller_owns,
        legacy_owns,
        "#3078 PR-3 orphan-store drain turn-aware-defer parity mismatch — StatusPanelController disagreed with the legacy inflight-row gate on panel ownership; legacy gate decided the defer (no behaviour change), divergence logged for the later controller-executes cutover"
    );
}

/// Retry every pending panel delete once. A committed delete, or a permanent
/// "message gone" (404/403/410), drops the record; a transient failure keeps it
/// for the next pass. Returns the number of records cleared this pass.
pub(in crate::services::discord) async fn drain(
    http: &Arc<serenity::Http>,
    shared: &Arc<crate::services::discord::SharedData>,
    provider: &ProviderKind,
    token_hash: &str,
) -> usize {
    let pending = load_pending(provider, token_hash);
    let mut cleared = 0usize;
    for (channel_id, panel_msg_id) in pending {
        // #3003 (codex P2 r26): re-validate against the live store immediately
        // before deleting. Between `load_pending` and here, the completion path may
        // have removed this record (the panel was completed and is now valid);
        // deleting from a stale snapshot would skip a record already cleaned up.
        // NOTE: this narrows but does not by itself close the check→delete gap — the
        // inflight gate below is what closes the TOCTOU against an in-flight
        // completion (see #3003 workflow r27).
        if !is_queued(provider, token_hash, channel_id, panel_msg_id) {
            continue;
        }
        // #3003 (workflow r28): defer the delete only while the live inflight row
        // still owns THIS EXACT panel (`status_message_id == panel_msg_id`). In that
        // window the turn's completion/reclaim path may be editing the panel into its
        // final state, and the unlocked `delete_message` round-trip below would race
        // that completion and erase a freshly-finalized valid panel — the residual
        // TOCTOU the r26 `is_queued` recheck only narrows.
        //
        // Keying on turn identity (not bare channel presence, as r27 did) is required:
        // a channel-coarse gate deferred whenever ANY inflight existed, so a newer
        // turn re-occupying the channel — or a stale row pinned alive by a long-lived
        // tmux pane — would defer an OLD turn's orphan forever (the store is its only
        // reclaim path). A different/absent `status_message_id` means the live turn
        // does not own this orphan, so it is safe to delete now.
        let inflight_panel_id =
            crate::services::discord::inflight::load_inflight_state(provider, channel_id)
                .and_then(|state| state.status_message_id);
        let legacy_owns = inflight_panel_id == Some(panel_msg_id);
        // EPIC #3078 PR-3 — route this turn-aware defer gate through the
        // `StatusPanelController` behind a SHADOW parity check. The controller
        // seeds its ledger from the inflight row's currently-owned panel id and
        // reports whether the live turn still owns THIS EXACT orphan; it must agree
        // with the legacy inflight-row read above, INCLUDING the channel-only
        // (`user_msg_id == 0`) collapse for the #3003 turn-aware defer. The legacy
        // `legacy_owns` gate below still decides the actual defer/delete, so the
        // delete behaviour is verifiably unchanged — only the (shadow) controller
        // agreement is observed. The controller actor is only spawned when v2 is
        // enabled, so this is inert and the legacy gate is byte-for-byte unchanged
        // when v2 is off. The controller actor task is NOT spawned when v2 is off,
        // so we MUST skip the awaited shadow read (its ack oneshot would never be
        // answered) — this guard is the v2-off short-circuit.
        if shared.status_panel_v2_enabled {
            let controller_owns = shared
                .status_panel_controller
                .orphan_gate_owns_panel(
                    crate::services::discord::turn_finalizer::TurnKey::new(
                        serenity::ChannelId::new(channel_id),
                        0,
                        crate::services::discord::runtime_store::load_generation(),
                    ),
                    provider.clone(),
                    inflight_panel_id.map(serenity::MessageId::new),
                    serenity::MessageId::new(panel_msg_id),
                )
                .await;
            assert_orphan_gate_parity(controller_owns, legacy_owns, channel_id, panel_msg_id);
        }
        if legacy_owns {
            continue;
        }
        let channel = serenity::ChannelId::new(channel_id);
        let message = serenity::MessageId::new(panel_msg_id);
        match channel.delete_message(http, message).await {
            Ok(_) => {
                remove(provider, token_hash, channel_id, panel_msg_id);
                cleared += 1;
            }
            Err(err) if delete_error_is_permanent(&err) => {
                remove(provider, token_hash, channel_id, panel_msg_id);
                cleared += 1;
            }
            Err(err) => {
                tracing::debug!(
                    "[status_panel_orphan_store] retry delete for {channel_id}/{panel_msg_id} \
                     failed transiently — keeping for next drain: {err}"
                );
            }
        }
    }
    cleared
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_is_idempotent_and_removable() {
        let root = tempfile::tempdir().expect("tempdir");
        let root = root.path();
        let provider = ProviderKind::Codex;
        let token = "tok";
        enqueue_in_root(root, &provider, token, 100, 5001);
        enqueue_in_root(root, &provider, token, 100, 5001); // duplicate
        enqueue_in_root(root, &provider, token, 100, 5002);
        let mut pending = load_pending_in_root(root, &provider, token);
        pending.sort();
        assert_eq!(pending, vec![(100, 5001), (100, 5002)]);

        remove_in_root(root, &provider, token, 100, 5001);
        assert_eq!(
            load_pending_in_root(root, &provider, token),
            vec![(100, 5002)]
        );

        // Removing the last id deletes the channel file → empty pending.
        remove_in_root(root, &provider, token, 100, 5002);
        assert!(load_pending_in_root(root, &provider, token).is_empty());
    }

    /// EPIC #3078 PR-3: for representative drain-gate inputs, the
    /// `StatusPanelController`'s turn-aware-defer decision (seed ledger from the
    /// inflight-row panel id, then ask whether it owns the orphan) equals the
    /// legacy inflight-row gate (`inflight.status_message_id == Some(candidate)`),
    /// so `assert_orphan_gate_parity` never fires and the legacy delete behaviour is
    /// unchanged. Covers the #3003 channel-only (`user_msg_id == 0`) collapse, the
    /// different-panel case, and the row-gone (`None`) case.
    #[tokio::test(flavor = "current_thread")]
    async fn controller_drain_gate_matches_legacy_for_representative_inputs() {
        use crate::services::discord::status_panel_controller::StatusPanelController;
        use crate::services::discord::turn_finalizer::TurnKey;

        // The orphan-store drain keys on the channel only (`user_msg_id == 0`) and
        // the controller collapses onto the single adopted live entry.
        let candidate = serenity::MessageId::new(7001);

        // Case A: inflight row still owns THIS exact panel → both defer (true).
        let ctl = StatusPanelController::spawn(true);
        let key = TurnKey::new(serenity::ChannelId::new(900), 0, 0);
        let inflight_a = Some(7001u64);
        let legacy_a = inflight_a == Some(candidate.get());
        let controller_a = ctl
            .orphan_gate_owns_panel(
                key,
                ProviderKind::Claude,
                inflight_a.map(serenity::MessageId::new),
                candidate,
            )
            .await;
        assert_eq!(controller_a, legacy_a);
        assert!(legacy_a);
        assert_orphan_gate_parity(controller_a, legacy_a, 900, candidate.get());

        // Case B: inflight row owns a DIFFERENT panel → both do not defer (false).
        let ctl_b = StatusPanelController::spawn(true);
        let key_b = TurnKey::new(serenity::ChannelId::new(901), 0, 0);
        let inflight_b = Some(8002u64);
        let legacy_b = inflight_b == Some(candidate.get());
        let controller_b = ctl_b
            .orphan_gate_owns_panel(
                key_b,
                ProviderKind::Claude,
                inflight_b.map(serenity::MessageId::new),
                candidate,
            )
            .await;
        assert_eq!(controller_b, legacy_b);
        assert!(!legacy_b);
        assert_orphan_gate_parity(controller_b, legacy_b, 901, candidate.get());

        // Case C: inflight row gone (`None`) → both do not defer (false), so the
        // orphan store (the only reclaim path here) does not defer forever.
        let ctl_c = StatusPanelController::spawn(true);
        let key_c = TurnKey::new(serenity::ChannelId::new(902), 0, 0);
        let inflight_c: Option<u64> = None;
        let legacy_c = inflight_c == Some(candidate.get());
        let controller_c = ctl_c
            .orphan_gate_owns_panel(
                key_c,
                ProviderKind::Claude,
                inflight_c.map(serenity::MessageId::new),
                candidate,
            )
            .await;
        assert_eq!(controller_c, legacy_c);
        assert!(!legacy_c);
        assert_orphan_gate_parity(controller_c, legacy_c, 902, candidate.get());
    }

    #[test]
    fn enqueue_skips_zero_ids_and_scopes_by_token() {
        let root = tempfile::tempdir().expect("tempdir");
        let root = root.path();
        let provider = ProviderKind::Claude;
        enqueue_in_root(root, &provider, "tok2", 0, 5001);
        enqueue_in_root(root, &provider, "tok2", 100, 0);
        assert!(load_pending_in_root(root, &provider, "tok2").is_empty());

        // token_hash scoping isolates bots sharing a provider.
        enqueue_in_root(root, &provider, "bot_a", 100, 5001);
        enqueue_in_root(root, &provider, "bot_b", 100, 6001);
        assert_eq!(
            load_pending_in_root(root, &provider, "bot_a"),
            vec![(100, 5001)]
        );
        assert_eq!(
            load_pending_in_root(root, &provider, "bot_b"),
            vec![(100, 6001)]
        );
    }
}
