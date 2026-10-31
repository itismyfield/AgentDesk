//! Restored watcher seeds and monitor auto-turn bookkeeping.

use std::sync::Arc;

use poise::serenity_prelude::{ChannelId, MessageId};

use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::message_outbox::enqueue_lifecycle_notification_best_effort;
use crate::services::provider::ProviderKind;

use super::{
    MONITOR_AUTO_TURN_DEFERRED_REASON_CODE, MONITOR_AUTO_TURN_PREAMBLE_HINT, SharedData,
    reconstructed_inflight_placeholder_body,
};

#[derive(Debug, Clone)]
pub(in crate::services::discord) struct RestoredWatcherTurn {
    pub(super) current_msg_id: MessageId,
    pub(super) status_message_id: Option<MessageId>,
    pub(super) response_sent_offset: usize,
    pub(super) full_response: String,
    pub(super) last_edit_text: String,
    pub(super) task_notification_kind: Option<TaskNotificationKind>,
    pub(super) finish_mailbox_on_completion: bool,
    /// #3107 codex re-review (P2#3): the #3099 hourglass anchor from the
    /// restored inflight, carried so a watcher-owned re-acquire (after the row
    /// is cleared mid-turn) can re-pin it instead of orphaning the `⏳`.
    pub(in crate::services::discord) injected_prompt_message_id: Option<u64>,
    /// Identity of the inflight row that seeded this restore. A long-lived watcher
    /// may consume the seed only after a later direct-input turn starts; compare
    /// this to the current row before carrying response text forward.
    pub(super) turn_identity: Option<super::super::inflight::InflightTurnIdentity>,
    /// #3871: frozen streamed rollover-prefix message ids restored from the
    /// persisted row so a terminal full-body fallback in a later iteration / after
    /// a restart still deletes every accumulated prefix (no residual duplicate).
    pub(super) streaming_rollover_frozen_msg_ids: Vec<MessageId>,
    /// Same-process retry seed produced by watcher send-failure rewind. Unlike a
    /// restart-restored seed, this is the current turn and must not be discarded by
    /// idle direct-prompt stale-seed cleanup.
    pub(super) same_turn_rewind: bool,
    /// Immutable source identity captured with the restored clone at watcher
    /// handoff. The clone can predate the bridge's terminal commit, so this is
    /// only a lookup key: seed consumption must re-read the durable receipt.
    /// Missing legacy components preserve the seed.
    pub(super) delivery_source:
        Option<super::super::outbound::delivery_record::ExactJsonlSourceIdentity>,
    /// Records the producer actually read to build `full_response`; unknown for every other seed.
    pub(super) body_source: Option<SeedBodySource>,
}

/// The transcript range one producer read for a seed body, in that file and session generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) struct SeedBodySource {
    pub(in crate::services::discord) range: (u64, u64),
    pub(in crate::services::discord) file: (u64, u64),
    pub(in crate::services::discord) generation_mtime_ns: i64,
}

impl RestoredWatcherTurn {
    /// The row's `turn_start_offset` stays the turn identity; the body range starts where the
    /// producer read, and is unknown without that start, its file or a session generation.
    pub(in crate::services::discord) fn read_by_producer(
        mut self,
        read: Option<crate::services::agent_protocol::TurnReadSource>,
        end: u64,
        tmux_session_name: &str,
    ) -> Self {
        self.body_source = read.and_then(|read| {
            let crate::services::cluster::stream_relay::SourceFileIdentity::Unix { dev, ino } =
                read.file
            else {
                return None;
            };
            let generation_mtime_ns =
                super::super::outbound::delivery_record::current_generation_mtime_ns(
                    tmux_session_name,
                );
            (read.start < end && generation_mtime_ns != 0).then_some(SeedBodySource {
                range: (read.start, end),
                file: (dev, ino),
                generation_mtime_ns,
            })
        });
        self
    }
}

#[derive(Debug)]
pub(super) struct WatcherStreamSeed {
    pub(super) placeholder_msg_id: Option<MessageId>,
    pub(super) status_panel_msg_id: Option<MessageId>,
    pub(super) response_sent_offset: usize,
    pub(super) full_response: String,
    pub(super) last_edit_text: String,
    pub(super) task_notification_kind: Option<TaskNotificationKind>,
    pub(super) finish_mailbox_on_completion: bool,
    /// #3871: see [`RestoredWatcherTurn::streaming_rollover_frozen_msg_ids`].
    pub(super) streaming_rollover_frozen_msg_ids: Vec<MessageId>,
}

pub(super) fn normalize_response_sent_offset(
    full_response: &str,
    response_sent_offset: usize,
) -> usize {
    let mut offset = response_sent_offset.min(full_response.len());
    while offset > 0 && !full_response.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

pub(in crate::services::discord) fn restored_watcher_turn_from_inflight(
    state: &super::super::inflight::InflightTurnState,
    tmux_session_name: &str,
    finish_mailbox_on_completion: bool,
) -> Option<RestoredWatcherTurn> {
    if state.rebind_origin
        || state
            .tmux_session_name
            .as_deref()
            .is_some_and(|name| name != tmux_session_name)
    {
        return None;
    }

    let provider = state.provider_kind()?;
    let current_msg_id = super::super::inflight::opt_message_id(state.current_msg_id)?;
    let response_sent_offset =
        normalize_response_sent_offset(&state.full_response, state.response_sent_offset);
    let delivery_channel_id =
        super::super::inflight::opt_channel_id(state.channel_id).map(ChannelId::get);
    let offset_authority_channel_id = match state.watcher_owner_channel_id {
        Some(raw) => super::super::inflight::opt_channel_id(raw).map(ChannelId::get),
        None => delivery_channel_id,
    };
    let delivery_source = state
        .turn_start_offset
        .zip(state.turn_nonce.as_deref())
        .filter(|(start, nonce)| state.last_offset > *start && !nonce.is_empty())
        .and_then(|(start, nonce)| {
            let delivery_channel_id = delivery_channel_id?;
            let offset_authority_channel_id = offset_authority_channel_id?;
            let generation_mtime_ns =
                super::super::outbound::delivery_record::current_generation_mtime_ns(
                    tmux_session_name,
                );
            (generation_mtime_ns != 0).then(|| {
                super::super::outbound::delivery_record::ExactJsonlSourceIdentity {
                    provider: provider.as_str().to_string(),
                    tmux_session_name: tmux_session_name.to_string(),
                    turn_nonce: nonce.to_string(),
                    range: (start, state.last_offset),
                    generation_mtime_ns,
                    offset_authority_channel_id,
                    delivery_channel_id,
                }
            })
        });
    Some(RestoredWatcherTurn {
        current_msg_id,
        status_message_id: state
            .status_message_id
            .and_then(super::super::inflight::opt_message_id),
        response_sent_offset,
        full_response: state.full_response.clone(),
        last_edit_text: reconstructed_inflight_placeholder_body(state, &provider),
        task_notification_kind: state.task_notification_kind,
        finish_mailbox_on_completion,
        injected_prompt_message_id: state.injected_prompt_message_id,
        turn_identity: Some(super::super::inflight::InflightTurnIdentity::from_state(
            state,
        )),
        streaming_rollover_frozen_msg_ids: state
            .streaming_rollover_frozen_msg_ids
            .iter()
            .copied()
            .filter_map(super::super::inflight::opt_message_id)
            .collect(),
        same_turn_rewind: false,
        delivery_source,
        body_source: None,
    })
}

pub(super) fn watcher_stream_seed(restored_turn: Option<RestoredWatcherTurn>) -> WatcherStreamSeed {
    match restored_turn {
        Some(restored) => WatcherStreamSeed {
            placeholder_msg_id: Some(restored.current_msg_id),
            status_panel_msg_id: restored.status_message_id,
            response_sent_offset: restored.response_sent_offset,
            full_response: restored.full_response,
            last_edit_text: restored.last_edit_text,
            task_notification_kind: restored.task_notification_kind,
            finish_mailbox_on_completion: restored.finish_mailbox_on_completion,
            streaming_rollover_frozen_msg_ids: restored.streaming_rollover_frozen_msg_ids,
        },
        None => WatcherStreamSeed {
            placeholder_msg_id: None,
            status_panel_msg_id: None,
            response_sent_offset: 0,
            full_response: String::new(),
            last_edit_text: String::new(),
            task_notification_kind: None,
            finish_mailbox_on_completion: false,
            streaming_rollover_frozen_msg_ids: Vec::new(),
        },
    }
}

pub(super) fn should_discard_restored_seed_for_idle_direct_prompt(
    restored_turn_present: bool,
    prompt_anchor_present: bool,
    seed_has_undelivered_body: bool,
    same_turn_rewind_seed: bool,
    seed_reassigned_to_different_turn: bool,
) -> bool {
    restored_turn_present
        && !same_turn_rewind_seed
        && ((prompt_anchor_present && !seed_has_undelivered_body)
            || seed_reassigned_to_different_turn)
}

pub(super) fn restored_seed_reassigned_to_different_turn(
    restored_turn: Option<&RestoredWatcherTurn>,
    current_turn_identity: Option<&super::super::inflight::InflightTurnIdentity>,
    prompt_anchor_message_id: Option<u64>,
) -> bool {
    let Some(restored) = restored_turn else {
        return false;
    };
    if restored.same_turn_rewind {
        return false;
    }
    // The restored clone may have been taken before the bridge committed and
    // cleared its inflight row. Re-read the receipt now, at seed consumption;
    // only an exact session+generation+nonce+range+owner+destination proof can
    // turn that stale clone into a discard. Body fingerprints are intentionally
    // absent from this authority decision.
    if restored.delivery_source.as_ref().is_some_and(|source| {
        let Some(provider) = ProviderKind::from_str(&source.provider) else {
            return false;
        };
        let Some(delivery_channel) =
            super::super::inflight::opt_channel_id(source.delivery_channel_id)
        else {
            return false;
        };
        if super::super::inflight::opt_channel_id(source.offset_authority_channel_id).is_none() {
            return false;
        }
        super::super::outbound::delivery_record::confirmed_delivery_receipt_exists(
            &provider,
            delivery_channel,
            restored.current_msg_id.get(),
            source,
        )
    }) {
        return true;
    }
    if let (Some(seed_anchor), Some(current_anchor)) = (
        restored.injected_prompt_message_id,
        prompt_anchor_message_id,
    ) && seed_anchor != current_anchor
    {
        return true;
    }
    if let (Some(seed_identity), Some(current_identity)) =
        (restored.turn_identity.as_ref(), current_turn_identity)
    {
        return seed_identity != current_identity;
    }
    false
}

/// #1009: One-shot guard for `MONITOR_AUTO_TURN_PREAMBLE_HINT` injection.
/// Returns `Some(hint)` on first call per turn and flips `injected` to true;
/// subsequent calls return `None` so the hint is never repeated within a
/// single monitor auto-turn frame.
pub(in crate::services::discord) fn consume_monitor_auto_turn_preamble_once(
    injected: &mut bool,
) -> Option<&'static str> {
    if *injected {
        None
    } else {
        *injected = true;
        Some(MONITOR_AUTO_TURN_PREAMBLE_HINT)
    }
}

pub(super) fn enqueue_monitor_auto_turn_deferred_notification(
    pg_pool: Option<&sqlx::PgPool>,
    channel_id: ChannelId,
    data_start_offset: u64,
) -> bool {
    let target = format!("channel:{}", channel_id.get());
    let session_key = format!(
        "monitor_auto_turn_deferred:ch:{}:off:{}",
        channel_id.get(),
        data_start_offset
    );
    enqueue_lifecycle_notification_best_effort(
        pg_pool,
        target.as_str(),
        Some(session_key.as_str()),
        MONITOR_AUTO_TURN_DEFERRED_REASON_CODE,
        "🔔 Monitor 트리거 유예 (유저 턴 종료 후 처리)",
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MonitorAutoTurnStart {
    pub(super) acquired: bool,
    pub(super) deferred: bool,
    /// The synthetic mailbox message id this monitor turn started under
    /// (`data_start_offset.max(1)`), `Some` only when `acquired`. #3016 P1:
    /// the finalizer keys its ledger on this so SEQUENTIAL monitor turns in the
    /// same channel within `FINALIZED_TTL` are DISTINCT entries — a finalized
    /// monitor turn must not make the next one resolve to `AlreadyFinalized`
    /// (which would strand its mailbox token + counter).
    pub(super) synthetic_message_id: Option<MessageId>,
    /// #3016 P1 (codex r3): a PROCESS-MONOTONIC ledger generation for this
    /// monitor turn, `Some` only when `acquired`. The synthetic mailbox message
    /// id is derived from the JSONL byte offset, which REPEATS after a wrapper
    /// respawn / JSONL truncation while `current_generation` stays the same — so
    /// two monitor turns at the same offset within `FINALIZED_TTL` would share a
    /// finalizer ledger key and the second would resolve to `AlreadyFinalized`,
    /// stranding its mailbox token + counter. Keying the ledger generation on
    /// this never-repeating counter makes every monitor turn a DISTINCT entry.
    /// (It is used ONLY for ledger keying; `do_finalize`'s identity-guarded
    /// mailbox finish matches on `channel_id` + `synthetic_message_id`, not the
    /// generation, so this does not affect which mailbox token is released.)
    pub(super) ledger_generation: Option<u64>,
}

/// Process-monotonic counter for monitor-auto-turn finalizer ledger
/// generations (#3016 P1). Starts high to avoid colliding with the
/// `current_generation` (dcserver restart) namespace used by ordinary turns.
static MONITOR_AUTO_TURN_LEDGER_GENERATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1 << 48);

pub(super) fn next_monitor_auto_turn_ledger_generation() -> u64 {
    MONITOR_AUTO_TURN_LEDGER_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

async fn finish_monitor_auto_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    synthetic_message_id: Option<MessageId>,
    ledger_generation: Option<u64>,
) {
    // #3016 phase 4: route the monitor-auto-turn terminal through the
    // single-authority finalizer instead of calling mailbox_finish_turn +
    // counter + kickoff inline. The monitor turn was `register_start`'d under
    // its synthetic message id keyed by a PROCESS-MONOTONIC ledger generation,
    // so we finalize under THAT exact key — keeping sequential monitor turns in
    // the same channel within `FINALIZED_TTL` as DISTINCT ledger entries even
    // when the byte-offset-derived synthetic id repeats after a wrapper respawn
    // (codex P1: a colliding key would make the second monitor turn resolve to
    // `AlreadyFinalized` and strand its mailbox token + counter). `do_finalize`
    // takes the identity-guarded `mailbox_finish_turn_if_matches` path for the
    // real id (generation is ledger-keying only). The ledger phase gate makes a
    // racing watcher/bridge terminal exactly-once safe. `FinalizeContext::monitor`
    // reproduces the inline side-effect set: no inflight clear, no
    // completion-cleanup, no voice drain, but kick off any queued backlog.
    let user_msg_id = synthetic_message_id.map(|id| id.get()).unwrap_or(0);
    let generation = ledger_generation.unwrap_or(shared.restart.current_generation);
    let _ = shared
        .turn_finalizer
        .submit_terminal(
            super::super::turn_finalizer::TurnKey::new(channel_id, user_msg_id, generation),
            provider.clone(),
            super::super::turn_finalizer::TerminalEvent::Complete,
            super::super::turn_finalizer::FinalizeContext::monitor(),
            shared.clone(),
        )
        .await;
    shared.turn_start_times.remove(&channel_id);
    if let Ok(mut last) = shared.last_turn_at.lock() {
        *last = Some(chrono::Local::now().to_rfc3339());
    }
}

pub(super) async fn finish_monitor_auto_turn_if_claimed(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    claimed: &mut bool,
    finished: &mut bool,
    synthetic_message_id: &mut Option<MessageId>,
    ledger_generation: &mut Option<u64>,
) {
    if *claimed {
        finish_monitor_auto_turn(
            shared,
            provider,
            channel_id,
            *synthetic_message_id,
            *ledger_generation,
        )
        .await;
        *claimed = false;
        *finished = true;
        *synthetic_message_id = None;
        *ledger_generation = None;
    }
}

pub(in crate::services::discord) fn build_monitor_triggered_inflight_state(
    mut state: super::super::inflight::InflightTurnState,
) -> super::super::inflight::InflightTurnState {
    state.request_owner_user_id = 0;
    state.user_msg_id = 0;
    state.current_msg_id = 0;
    state.rebind_origin = true;
    state.turn_source = super::super::inflight::TurnSource::MonitorTriggered;
    state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
    state
}
