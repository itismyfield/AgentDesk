use std::sync::Arc;

use crate::services::provider::ProviderKind;

use super::{FinalizeContext, TerminalEvent, TurnKey};
use crate::services::discord::SharedData;

#[derive(Clone, Copy)]
struct ReactionCleanupRequest {
    channel_id: serenity::model::id::ChannelId,
    message_id: serenity::model::id::MessageId,
    add_checkmark: bool,
    source: &'static str,
}

/// Backstop-only reaction cleanup for terminal paths that skipped the normal
/// watcher `⏳ -> ✅` block.
///
/// Reachable production matrix:
/// * `watcher` / `bridge` / `monitor` submitters pass `clear_inflight = false`,
///   so they never run this helper; watcher owns the normal committed-output
///   reaction block when it is safe to claim completion.
/// * `gate_backstop()` is not a production `submit_terminal` context. It is
///   reached only by `run_backstop_finalize -> do_finalize` after a deferred
///   busy-pane gate, where no caller remains to clear inflight or the reaction.
/// * the no-owner restored-watcher path mutates watcher context into the same
///   `clear_inflight && kickoff_queue && !completion_cleanup && !voice` shape.
///   That path also skipped the normal watcher block, so it needs this fallback.
/// * `AlreadyFinalized` losers only inherit their submitter context, so they
///   cannot become the backstop reaction owner after someone else won the gate.
pub(super) fn finalized_reaction_lifecycle(
    key: TurnKey,
    event: &TerminalEvent,
    ctx: FinalizeContext,
    shared: &Arc<SharedData>,
    source: &'static str,
    skip_completion_reaction: bool,
) {
    if !ctx.clear_inflight
        || !ctx.kickoff_queue
        || ctx.allow_completion_cleanup
        || ctx.drain_voice
        || key.user_msg_id == 0
        || skip_completion_reaction
    {
        return;
    }
    let message_id = serenity::model::id::MessageId::new(key.user_msg_id);
    schedule_reaction_cleanup(
        shared.clone(),
        ReactionCleanupRequest {
            channel_id: key.channel_id,
            message_id,
            add_checkmark: !matches!(event, TerminalEvent::Cancel),
            source,
        },
    );
}

/// #3350 ②: pure verdict — must this finalize ENSURE the #3303 DeferredClaim
/// marker for the row it is finalizing? (Same pure-gate pattern as the
/// `should_complete_…` helpers.) All six gates must hold:
///
/// * the terminal carries a real identity AND the row IS that turn (an id-0
///   orphan or a mismatched/newer row proves nothing about this anchor);
/// * the row is a TUI-direct synthetic turn (`turn_source == ExternalInput`);
/// * SC3: WATCHER-owned only — a bridge-owned turn finalizes Done with its own
///   `⏳` cleanup (`turn_bridge`), so a marker would contradict that normal
///   completion with a TTL `⚠`;
/// * I4: `injected_prompt_message_id` pins the row's OWN anchor
///   (`user_msg_id`), never a later injection's overwrite of the shared slot;
/// * a tmux session is present (the marker's reconcile scope needs it).
pub(super) fn should_ensure_synthetic_claim_marker(
    key_user_msg_id: u64,
    row_user_msg_id: u64,
    row_turn_source_external: bool,
    row_relay_owner_watcher: bool,
    row_injected_prompt_message_id: Option<u64>,
    row_tmux_session_present: bool,
) -> bool {
    key_user_msg_id != 0
        && row_user_msg_id == key_user_msg_id
        && row_turn_source_external
        && row_relay_owner_watcher
        && row_injected_prompt_message_id == Some(key_user_msg_id)
        && row_tmux_session_present
}

/// #3350 codex r1-1: submit-time snapshot of the inflight-row fields the
/// finalize-time marker ensure authenticates against. The production watcher
/// submitters clear the row BEFORE submitting the finalize (tmux.rs
/// `finish_restored_watcher_active_turn` docs), so a row re-load inside
/// `do_finalize` is a guaranteed no-op for exactly the turns the ensure
/// exists for — the snapshot, captured from the caller's pre-clear row pinned
/// to the submitted turn, closes that guarantee hole.
#[derive(Clone, Debug)]
pub(in crate::services::discord) struct SyntheticClaimSnapshot {
    pub(in crate::services::discord) user_msg_id: u64,
    pub(in crate::services::discord) turn_source_external: bool,
    pub(in crate::services::discord) relay_owner_watcher: bool,
    pub(in crate::services::discord) injected_prompt_message_id: Option<u64>,
    pub(in crate::services::discord) tmux_session_name: Option<String>,
    pub(in crate::services::discord) started_at: String,
    pub(in crate::services::discord) relay_ownership_only: bool,
}

impl SyntheticClaimSnapshot {
    pub(in crate::services::discord) fn from_row(
        row: &crate::services::discord::inflight::InflightTurnState,
    ) -> Self {
        use crate::services::discord::inflight::{RelayOwnerKind, TurnSource};
        Self {
            user_msg_id: row.user_msg_id,
            turn_source_external: row.turn_source == TurnSource::ExternalInput,
            relay_owner_watcher: row.relay_owner_kind == RelayOwnerKind::Watcher,
            injected_prompt_message_id: row.injected_prompt_message_id,
            tmux_session_name: row.tmux_session_name.clone(),
            started_at: row.started_at.clone(),
            relay_ownership_only: row.relay_ownership_only,
        }
    }
}

pub(super) fn relay_ownership_only_for_finalize(
    key: TurnKey,
    provider: &ProviderKind,
    submit_snapshot: Option<&SyntheticClaimSnapshot>,
) -> bool {
    if key.user_msg_id == 0 {
        return false;
    }
    if let Some(snapshot) = submit_snapshot
        && snapshot.user_msg_id == key.user_msg_id
    {
        return snapshot.relay_ownership_only;
    }
    let Some(row) =
        crate::services::discord::inflight::load_inflight_state(provider, key.channel_id.get())
    else {
        return false;
    };
    row.user_msg_id == key.user_msg_id && row.relay_ownership_only
}

/// #3350 ②: `do_finalize` entry hook — whatever submitter (watcher / bridge /
/// monitor / backstop) finalizes a watcher-owned TUI-direct synthetic turn,
/// guarantee the durable #3303 DeferredClaim marker exists for its anchor.
/// Idempotent (an existing own-pin/covered marker is never touched; an
/// uncovered stale Abort pin is replaced per the #3303 contract — see
/// `ensure_marker_for_own_synthetic_turn`) and reaction-free: the `⏳`
/// verdict belongs exclusively to the #3303 reconcilers (drain `✅` / sweep
/// TTL `⚠`), so output that commits late after a Stopped event never races a
/// false-`⚠` here. Runs for Cancel too — a cancelled turn with no commit
/// converging to the TTL `⚠` is the honest signal.
///
/// Evidence source (codex r1-1): the SUBMIT-TIME snapshot wins when the
/// submitter carried one — the watcher clears the row before submitting, so
/// for its turns the re-load below proves nothing. The row re-load (which
/// must then run BEFORE the `do_finalize` (A) inflight clear) remains the
/// fallback for submitters that did not capture a snapshot.
pub(super) fn ensure_synthetic_claim_marker_before_clear(
    key: TurnKey,
    provider: &ProviderKind,
    submit_snapshot: Option<&SyntheticClaimSnapshot>,
) {
    if key.user_msg_id == 0 {
        return;
    }
    let snapshot = match submit_snapshot {
        Some(snapshot) => snapshot.clone(),
        None => {
            let Some(row) = crate::services::discord::inflight::load_inflight_state(
                provider,
                key.channel_id.get(),
            ) else {
                return;
            };
            SyntheticClaimSnapshot::from_row(&row)
        }
    };
    if !should_ensure_synthetic_claim_marker(
        key.user_msg_id,
        snapshot.user_msg_id,
        snapshot.turn_source_external,
        snapshot.relay_owner_watcher,
        snapshot.injected_prompt_message_id,
        snapshot.tmux_session_name.is_some(),
    ) {
        return;
    }
    let Some(tmux) = snapshot.tmux_session_name.as_deref() else {
        return;
    };
    let _ = crate::services::discord::tui_direct_abort_marker::ensure_marker_for_own_synthetic_turn(
        provider.as_str(),
        key.channel_id.get(),
        key.user_msg_id,
        tmux,
        &snapshot.started_at,
    );
}

/// Late `AlreadyFinalized` losers still perform guarded active-state cleanup.
/// This is intentionally narrower than `do_finalize`: only the same real turn id
/// can lose mailbox/inflight state, so a newer active turn is preserved.
pub(super) async fn already_finalized_active_state(
    key: TurnKey,
    provider: &ProviderKind,
    event: &TerminalEvent,
    ctx: FinalizeContext,
    shared: &Arc<SharedData>,
) {
    if key.user_msg_id == 0 {
        return;
    }

    let _ = crate::services::discord::inflight::clear_inflight_state_if_matches(
        provider,
        key.channel_id.get(),
        key.user_msg_id,
    );

    let finish = super::super::mailbox_finish_turn_if_matches(
        shared,
        provider,
        key.channel_id,
        serenity::model::id::MessageId::new(key.user_msg_id),
    )
    .await;
    let Some(token) = finish.removed_token.as_ref() else {
        return;
    };

    if ctx.allow_completion_cleanup && !matches!(event, TerminalEvent::Cancel) {
        token.mark_completion_cleanup();
    }
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    super::super::saturating_decrement_global_active(shared);
    super::super::clear_watchdog_deadline_override(key.channel_id.get()).await;
    shared
        .dispatch
        .thread_parents
        .retain(|_, thread| *thread != key.channel_id);
    if !finish.has_pending {
        shared.dispatch.role_overrides.remove(&key.channel_id);
    }
}

#[cfg(not(test))]
fn schedule_reaction_cleanup(shared: Arc<SharedData>, request: ReactionCleanupRequest) {
    super::super::task_supervisor::spawn_observed("turn_finalizer_reaction_cleanup", async move {
        apply_reaction(
            &shared,
            request.channel_id,
            request.message_id,
            '⏳',
            false,
            request.source,
        )
        .await;
        if request.add_checkmark {
            apply_reaction(
                &shared,
                request.channel_id,
                request.message_id,
                '✅',
                true,
                request.source,
            )
            .await;
        }
    });
}

#[cfg(not(test))]
async fn apply_reaction(
    shared: &Arc<SharedData>,
    channel_id: serenity::model::id::ChannelId,
    message_id: serenity::model::id::MessageId,
    emoji: char,
    add: bool,
    source: &'static str,
) {
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        tracing::warn!(
            channel = channel_id.get(),
            message = message_id.get(),
            emoji = %emoji,
            source,
            "turn finalizer reaction cleanup skipped; provider serenity http unavailable"
        );
        return;
    };
    if add {
        super::super::formatting::add_reaction_raw(&http, channel_id, message_id, emoji).await;
    } else {
        super::super::formatting::remove_reaction_raw(&http, channel_id, message_id, emoji).await;
    }
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReactionCleanupRecord {
    pub(super) channel_id: u64,
    pub(super) message_id: u64,
    pub(super) emoji: char,
    pub(super) add: bool,
    pub(super) source: &'static str,
}

#[cfg(test)]
static REACTION_CLEANUP_RECORDS: std::sync::LazyLock<
    std::sync::Mutex<Option<Vec<ReactionCleanupRecord>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
pub(super) fn begin_reaction_cleanup_recording() {
    *REACTION_CLEANUP_RECORDS
        .lock()
        .expect("reaction cleanup recorder lock") = Some(Vec::new());
}

#[cfg(test)]
pub(super) fn take_reaction_cleanup_records() -> Vec<ReactionCleanupRecord> {
    REACTION_CLEANUP_RECORDS
        .lock()
        .expect("reaction cleanup recorder lock")
        .take()
        .unwrap_or_default()
}

#[cfg(test)]
fn schedule_reaction_cleanup(_shared: Arc<SharedData>, request: ReactionCleanupRequest) {
    record_reaction(
        request.channel_id,
        request.message_id,
        '⏳',
        false,
        request.source,
    );
    if request.add_checkmark {
        record_reaction(
            request.channel_id,
            request.message_id,
            '✅',
            true,
            request.source,
        );
    }
}

#[cfg(test)]
fn record_reaction(
    channel_id: serenity::model::id::ChannelId,
    message_id: serenity::model::id::MessageId,
    emoji: char,
    add: bool,
    source: &'static str,
) {
    if let Some(records) = REACTION_CLEANUP_RECORDS
        .lock()
        .expect("reaction cleanup recorder lock")
        .as_mut()
    {
        records.push(ReactionCleanupRecord {
            channel_id: channel_id.get(),
            message_id: message_id.get(),
            emoji,
            add,
            source,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        FinalizeContext, FinalizeOutcome, GATE_BACKSTOP, RECONCILE_INTERVAL, TerminalEvent,
        TurnFinalizer, TurnKey,
    };
    use super::*;
    use crate::services::discord::inflight::{
        InflightTurnState, RelayOwnerKind, TurnSource, clear_inflight_state, save_inflight_state,
    };
    use crate::services::provider::{CancelToken, ProviderKind};
    use serenity::model::id::{ChannelId, MessageId, UserId};

    fn test_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
    }

    fn with_isolated_runtime_root(f: impl FnOnce()) {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        let tmp = tempfile::tempdir().expect("create temp runtime dir for reaction cleanup test");
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap());
        }
        f();
        unsafe {
            match prev {
                Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
        }
    }

    async fn seed_active_turn(
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        user_msg_id: u64,
    ) -> Arc<CancelToken> {
        let token = Arc::new(CancelToken::new());
        shared
            .mailbox(channel_id)
            .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(user_msg_id))
            .await;
        token
    }

    fn recorded_actions(records: &[ReactionCleanupRecord]) -> Vec<(u64, u64, char, bool)> {
        records
            .iter()
            .map(|record| {
                (
                    record.channel_id,
                    record.message_id,
                    record.emoji,
                    record.add,
                )
            })
            .collect()
    }

    /// #3350 ②: `should_ensure_synthetic_claim_marker` truth table — each of
    /// the six gates flips the verdict alone (RED per gate), and the all-green
    /// row ensures (the `should_complete_…` truth-table pattern).
    #[test]
    fn should_ensure_synthetic_claim_marker_truth_table() {
        // GREEN: real identity + own row + ExternalInput + Watcher + own
        // injected pin + tmux present.
        assert!(should_ensure_synthetic_claim_marker(
            77,
            77,
            true,
            true,
            Some(77),
            true
        ));
        // RED: id-0 orphan terminal — no identity to authenticate against.
        assert!(!should_ensure_synthetic_claim_marker(
            0,
            0,
            true,
            true,
            Some(0),
            true
        ));
        // RED: the row is a different (newer) turn than the terminal's key.
        assert!(!should_ensure_synthetic_claim_marker(
            77,
            78,
            true,
            true,
            Some(77),
            true
        ));
        // RED: not a TUI-direct synthetic turn (Discord-origin / monitor row).
        assert!(!should_ensure_synthetic_claim_marker(
            77,
            77,
            false,
            true,
            Some(77),
            true
        ));
        // RED (SC3): bridge-owned rows complete their own ⏳ via turn_bridge —
        // a marker would contradict the normal completion with a TTL ⚠.
        assert!(!should_ensure_synthetic_claim_marker(
            77,
            77,
            true,
            false,
            Some(77),
            true
        ));
        // RED (I4): the injected ⏳ slot pins a LATER injection, not this
        // anchor — and an absent pin proves nothing.
        assert!(!should_ensure_synthetic_claim_marker(
            77,
            77,
            true,
            true,
            Some(78),
            true
        ));
        assert!(!should_ensure_synthetic_claim_marker(
            77, 77, true, true, None, true
        ));
        // RED: no tmux session — the marker's reconcile scope needs one.
        assert!(!should_ensure_synthetic_claim_marker(
            77,
            77,
            true,
            true,
            Some(77),
            false
        ));
    }

    /// #3350 ② integration: a terminal finalize over a watcher-owned
    /// TUI-direct synthetic row ensures the durable DeferredClaim marker
    /// pinned to the row's OWN identity — and sends NO reaction (delivery
    /// belongs exclusively to the #3303 reconcilers, so late-committing
    /// output after a Stopped event can never race a false-⚠ here). RED
    /// pre-#3350: a turn claimed before the inline-claim record existed (or
    /// whose record failed) finalized with no marker → eternal anchor ⏳.
    #[test]
    fn finalize_ensures_deferred_claim_marker_for_synthetic_watcher_row() {
        use crate::services::discord::inflight::{
            InflightTurnState, TurnSource, save_inflight_state,
        };

        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared =
                    super::super::super::make_shared_data_for_tests_with_storage(None, None);
                let ch = ChannelId::new(3_350_100);
                let tid = 3_350_101_u64;
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                // The watcher-owned TUI-direct synthetic row the finalize reads
                // (the exact shape the inline claim persists).
                let mut row = InflightTurnState::new(
                    ProviderKind::Claude,
                    ch.get(),
                    None,
                    0,
                    tid,
                    0,
                    "/loop tick".to_string(),
                    None,
                    Some("tmux-3350".to_string()),
                    None,
                    None,
                    0,
                );
                row.turn_source = TurnSource::ExternalInput;
                row.set_relay_owner_kind(RelayOwnerKind::Watcher);
                row.injected_prompt_message_id = Some(tid);
                save_inflight_state(&row).expect("persist synthetic watcher row");

                begin_reaction_cleanup_recording();
                let outcome = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::bridge(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));

                let markers = crate::services::discord::tui_direct_abort_marker::load_for_channel(
                    "claude",
                    ch.get(),
                );
                assert_eq!(
                    markers.len(),
                    1,
                    "RED pre-#3350: finalize left no marker — the anchor ⏳ of a \
                     turn claimed before the inline record existed had no \
                     reconcile owner"
                );
                let marker = &markers[0];
                assert_eq!(
                    marker.origin,
                    crate::services::discord::tui_direct_abort_marker::MarkerOrigin::DeferredClaim
                );
                assert_eq!(marker.anchor_message_id, tid);
                assert_eq!(
                    marker.foreign_user_msg_id,
                    Some(tid),
                    "the pin is the row's OWN identity (SC1 — never a foreign turn)"
                );
                assert_eq!(
                    marker.foreign_started_at.as_deref(),
                    Some(row.started_at.as_str())
                );
                assert!(
                    take_reaction_cleanup_records().is_empty(),
                    "the ensure must never send reactions — delivery is owned by \
                     the #3303 drain ✅ / sweep TTL ⚠ (I1: zero new reaction sites)"
                );
            });
        });
    }

    /// #3350 codex r1-1 (the production watcher shape): the watcher clears the
    /// row BEFORE submitting the finalize, so the row re-load inside
    /// `do_finalize` proves nothing — the submit-time snapshot must carry the
    /// identity. RED pre-fix: with the row already gone the ensure was a
    /// guaranteed no-op on exactly the watcher path it was built for (a turn
    /// claimed before the inline record existed finalized with no marker —
    /// eternal anchor ⏳). Also pins the negative: a snapshot pinned to a
    /// DIFFERENT turn than the submitted key must ensure NOTHING.
    #[test]
    fn finalize_ensures_marker_from_submit_snapshot_when_watcher_precleared_row() {
        use crate::services::discord::inflight::{
            InflightTurnState, TurnSource, save_inflight_state,
        };

        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared =
                    super::super::super::make_shared_data_for_tests_with_storage(None, None);
                let ch = ChannelId::new(3_350_200);
                let tid = 3_350_201_u64;
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                let mut row = InflightTurnState::new(
                    ProviderKind::Claude,
                    ch.get(),
                    None,
                    0,
                    tid,
                    0,
                    "/loop tick".to_string(),
                    None,
                    Some("tmux-3350-pre".to_string()),
                    None,
                    None,
                    0,
                );
                row.turn_source = TurnSource::ExternalInput;
                row.set_relay_owner_kind(RelayOwnerKind::Watcher);
                row.injected_prompt_message_id = Some(tid);
                save_inflight_state(&row).expect("persist synthetic watcher row");

                // The watcher's exact production sequence: snapshot, clear, submit.
                let snapshot = super::SyntheticClaimSnapshot::from_row(&row);
                assert!(snapshot.turn_source_external && snapshot.relay_owner_watcher);
                assert_eq!(snapshot.user_msg_id, tid);
                assert_eq!(snapshot.injected_prompt_message_id, Some(tid));
                crate::services::discord::inflight::clear_inflight_state(
                    &ProviderKind::Claude,
                    ch.get(),
                );

                begin_reaction_cleanup_recording();
                let outcome = fin
                    .submit_terminal_with_claim_snapshot(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::watcher(),
                        Some(snapshot),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));

                let markers = crate::services::discord::tui_direct_abort_marker::load_for_channel(
                    "claude",
                    ch.get(),
                );
                assert_eq!(
                    markers.len(),
                    1,
                    "RED pre-r1-1: the row-reload ensure no-op'd on the precleared watcher path"
                );
                assert_eq!(markers[0].anchor_message_id, tid);
                assert_eq!(markers[0].foreign_user_msg_id, Some(tid), "OWN pin (SC1)");
                assert_eq!(
                    markers[0].foreign_started_at.as_deref(),
                    Some(row.started_at.as_str()),
                    "the pin is the SUBMIT-TIME snapshot identity"
                );
                assert!(
                    take_reaction_cleanup_records().is_empty(),
                    "the ensure stays reaction-free (#3303 reconcilers own delivery)"
                );

                // Negative: a snapshot for a DIFFERENT turn (newer row captured
                // by mistake) fails the row-is-this-turn gate — no marker.
                let ch2 = ChannelId::new(3_350_300);
                let key2 = TurnKey::new(ch2, 3_350_301, 0);
                let mut foreign = snapshot_for_other_turn(&row, 3_350_999);
                foreign.tmux_session_name = Some("tmux-3350-pre".to_string());
                let outcome = fin
                    .submit_terminal_with_claim_snapshot(
                        key2,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::watcher(),
                        Some(foreign),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));
                assert!(
                    crate::services::discord::tui_direct_abort_marker::load_for_channel(
                        "claude",
                        ch2.get(),
                    )
                    .is_empty(),
                    "a mismatched snapshot must never pin a marker onto the submitted key"
                );
            });
        });
    }

    /// Helper for the negative leg: the same row's snapshot re-pinned to a
    /// different `user_msg_id` (what a buggy caller passing a newer row's
    /// snapshot would produce).
    fn snapshot_for_other_turn(
        row: &crate::services::discord::inflight::InflightTurnState,
        other_user_msg_id: u64,
    ) -> super::SyntheticClaimSnapshot {
        let mut snapshot = super::SyntheticClaimSnapshot::from_row(row);
        snapshot.user_msg_id = other_user_msg_id;
        snapshot.injected_prompt_message_id = Some(other_user_msg_id);
        snapshot
    }

    #[test]
    fn reconciler_backstop_finalize_removes_hourglass_and_marks_complete() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared =
                    super::super::super::make_shared_data_for_tests_with_storage(None, None);
                let ch = ChannelId::new(3_334_100);
                let tid = 3_334_101_u64;
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                begin_reaction_cleanup_recording();
                let deferred = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(deferred, FinalizeOutcome::Deferred));

                tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 3).await;
                tokio::task::yield_now().await;

                let records = take_reaction_cleanup_records();
                assert_eq!(
                    recorded_actions(&records),
                    vec![(ch.get(), tid, '⏳', false), (ch.get(), tid, '✅', true)]
                );
                assert!(records.iter().all(|record| record.source == "finalized"));
            });
        });
    }

    #[test]
    fn relay_ownership_only_snapshot_skips_backstop_reaction_cleanup() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared =
                    super::super::super::make_shared_data_for_tests_with_storage(None, None);
                let ch = ChannelId::new(3_334_150);
                let tid = 3_334_151_u64;
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                let mut row = InflightTurnState::new(
                    ProviderKind::Claude,
                    ch.get(),
                    None,
                    0,
                    tid,
                    0,
                    "This session is being continued from a previous conversation".to_string(),
                    None,
                    Some("tmux-3334-relay-only".to_string()),
                    None,
                    None,
                    0,
                );
                row.turn_source = TurnSource::ExternalInput;
                row.set_relay_owner_kind(RelayOwnerKind::Watcher);
                row.injected_prompt_message_id = Some(tid);
                row.relay_ownership_only = true;
                save_inflight_state(&row).expect("persist relay-only synthetic row");
                let snapshot = SyntheticClaimSnapshot::from_row(&row);
                clear_inflight_state(&ProviderKind::Claude, ch.get());

                begin_reaction_cleanup_recording();
                let outcome = fin
                    .submit_terminal_with_claim_snapshot(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::gate_backstop(),
                        Some(snapshot),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));
                assert!(
                    take_reaction_cleanup_records().is_empty(),
                    "relay_ownership_only compact-note anchors must not receive the backstop ⏳ removal / ✅ add reaction lifecycle"
                );
            });
        });
    }

    #[test]
    fn already_finalized_loser_does_not_claim_reaction_cleanup() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared =
                    super::super::super::make_shared_data_for_tests_with_storage(None, None);
                let ch = ChannelId::new(3_334_200);
                let tid = 3_334_201_u64;
                shared
                    .restart.global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                begin_reaction_cleanup_recording();
                let first = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(first, FinalizeOutcome::Finalized { .. }));
                assert!(take_reaction_cleanup_records().is_empty());

                begin_reaction_cleanup_recording();
                let late = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::bridge(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
                assert!(
                    take_reaction_cleanup_records().is_empty(),
                    "reachable AlreadyFinalized losers inherit watcher/bridge/monitor context and must not masquerade as the backstop reaction owner"
                );
            });
        });
    }

    #[test]
    fn watcher_context_skips_extra_reaction_calls() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared =
                    super::super::super::make_shared_data_for_tests_with_storage(None, None);
                let ch = ChannelId::new(3_334_400);
                let tid = 3_334_401_u64;
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                begin_reaction_cleanup_recording();
                let outcome = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));
                assert!(take_reaction_cleanup_records().is_empty());
            });
        });
    }

    #[test]
    fn cleanup_targets_turn_identity_and_skips_synthetic_id_zero() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared =
                    super::super::super::make_shared_data_for_tests_with_storage(None, None);
                let ch = ChannelId::new(3_334_500);
                let old_tid = 3_334_501_u64;
                let newer_tid = 3_334_502_u64;
                shared
                    .restart
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _newer = seed_active_turn(&shared, ch, newer_tid).await;
                let fin = TurnFinalizer::spawn();

                begin_reaction_cleanup_recording();
                let outcome = fin
                    .submit_terminal(
                        TurnKey::new(ch, old_tid, 0),
                        ProviderKind::Claude,
                        TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(
                    outcome,
                    FinalizeOutcome::Finalized {
                        removed_token: None,
                        ..
                    }
                ));
                let records = take_reaction_cleanup_records();
                assert_eq!(recorded_actions(&records).len(), 2);
                assert!(records.iter().all(|record| record.message_id == old_tid));
                assert!(records.iter().all(|record| record.message_id != newer_tid));
                assert!(shared.mailbox(ch).has_active_turn().await);

                begin_reaction_cleanup_recording();
                let zero = fin
                    .submit_terminal(
                        TurnKey::new(ChannelId::new(3_334_600), 0, 0),
                        ProviderKind::Claude,
                        TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(zero, FinalizeOutcome::Finalized { .. }));
                assert!(take_reaction_cleanup_records().is_empty());
            });
        });
    }
}
