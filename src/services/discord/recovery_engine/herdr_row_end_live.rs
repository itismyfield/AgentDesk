//! A live Herdr turn whose execution ended under it: its own source is read after the end, then
//! one body-free notice settles the captured turn and wakes its reader. Every unclear read keeps it.

use super::*;
use crate::services::discord::herdr_terminate::{TerminationResult, capture_turn, settle_captured};
use crate::services::provider::CancelToken;
use crate::services::provider::cancel_token_claude_interrupt::{HerdrInterruptState, herdr_turn};

/// This process's one Herdr state for the row's own turn, while no terminal kind or receipt owns it.
pub(super) fn live_target(
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    current: u64,
) -> Option<Arc<HerdrInterruptState>> {
    let hosted = match provider {
        ProviderKind::Claude => true,
        ProviderKind::Codex => crate::services::turn_host::herdr_turn_switched_on_for(provider),
        _ => false,
    };
    if !hosted
        || row.channel_id == 0
        || !inflight::row_is_current_generation(row, current)
        || row.tui_terminal_kind.is_some()
        || row.herdr_before_start_stop.is_some()
    {
        return None;
    }
    let nonce = row
        .turn_nonce
        .as_deref()
        .filter(|nonce| !nonce.is_empty())?;
    let logical = recovery_tmux_session_name(provider, row)?;
    herdr_turn(&logical, nonce).filter(|state| state.owner.logical_key == logical)
}

/// The captured actor's turn is settled only once its pinned execution reads as ended (E) and its
/// own source, read after E, shows no terminal of it.
pub(super) async fn end_live(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    picked: inflight::InflightTurnState,
) {
    let channel = ChannelId::new(picked.channel_id);
    let Ok(transition) = shared.session_transition_lock(channel).try_lock_owned() else {
        return;
    };
    let current = shared.restart.current_generation;
    let fresh = inflight::load_inflight_state_read_only(provider, channel.get());
    let Some(row) = fresh.filter(|row| {
        row.turn_nonce == picked.turn_nonce && row.save_generation == picked.save_generation
    }) else {
        return;
    };
    let Some(state) = live_target(provider, &row, current) else {
        return;
    };
    // Only the mailbox actor holding this very state is settled here.
    let Ok(Some(turn)) = capture_turn(shared, provider, channel).await else {
        return;
    };
    let own = turn.token.herdr_interrupt_state();
    let execution = state
        .submission
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .execution
        .clone();
    let (Some(pool), Some(http), Some(nonce)) = (
        shared.pg_pool.as_ref(),
        shared.serenity_http_or_token_fallback(),
        execution.filter(|_| own.is_some_and(|own| Arc::ptr_eq(&own, &state))),
    ) else {
        return;
    };
    let early = mutant("prb-read-before-end").then(|| read_ended(provider, &state, &turn.token));
    let Some(retire) = execution_gone(pool, &state, &nonce).await else {
        return;
    };
    let read = match early {
        Some(read) => read,
        None => {
            let (kind, state, actor) = (provider.clone(), state.clone(), turn.token.clone());
            tokio::task::spawn_blocking(move || read_ended(&kind, &state, &actor))
                .await
                .ok()
                .flatten()
        }
    };
    match read {
        // Its own terminal is the reader's to admit: the native settlement owns the turn.
        Some(true) => return,
        Some(false) => {}
        None => return source_unread(&row, &state, &turn.token),
    }
    if let Some((observation, record)) = &retire
        && retire_confirmed(pool, observation, record).await.is_err()
    {
        return kept(&row, "the ended execution's retire did not land");
    }
    if release_hold(&nonce) != HoldRelease::Released {
        return kept(&row, "retired, hold release incomplete");
    }
    let text = interrupted_recovery_message(&row, "");
    let outcome = relay_recovery_body_notice(&http, shared, provider, &row, &text).await;
    if outcome != RecoveryRelayOutcome::Delivered {
        return kept(&row, "retired, notice not delivered");
    }
    let token = turn.token.clone();
    match settle_captured(shared, provider, Some(turn)).await {
        TerminationResult::Retired if !mutant("prb-no-wake") => {
            // Watchers read the cleanup first, so the cancel below reads as a finished turn.
            token.mark_completion_cleanup();
            token.cancelled.store(true, Ordering::Release);
        }
        TerminationResult::Retired => {}
        other => tracing::warn!(
            event = "herdr_live_end_unconfirmed",
            channel_id = row.channel_id,
            ?other,
            "herdr row end: the ended turn's settlement is unconfirmed"
        ),
    }
    drop(transition);
}

/// The pinned execution's end: `Some(None)` once it is already retired, `Some(Some(cas))` on a
/// fresh E; another execution, or an unconfirmed end, is `None`.
async fn execution_gone(
    pool: &sqlx::PgPool,
    state: &HerdrInterruptState,
    nonce: &str,
) -> Option<Option<(HostedObservation, HostedExecution)>> {
    let HostedLookup::Found(found) =
        crate::services::claude::herdr_turn::load(pool, &state.owner).await
    else {
        return None;
    };
    let HostedRecord::Known(record) = found.record.clone() else {
        return None;
    };
    let pinned = record.execution_nonce == nonce || mutant("prb-successor-reread");
    if !pinned || record.owner != state.owner {
        return None;
    }
    if record.state == HostedState::Retired {
        // A retire this pass or an earlier one made; the notice and settlement still follow.
        return (!mutant("prb-retired-holds")).then_some(None);
    }
    let target = herdr_endpoints().target(&record)?;
    let ended = execution_end(target, record.clone()).await == ExecutionEnd::Ended;
    ended.then_some(Some((found, record)))
}

/// After E, whether the turn's own source from its input shows its terminal; `None` while the
/// source is unknown, unreadable, another file, or cut below the input.
fn read_ended(
    provider: &ProviderKind,
    state: &HerdrInterruptState,
    actor: &Arc<CancelToken>,
) -> Option<bool> {
    let start = state.turn_start.get()?;
    let mut file = std::fs::File::open(&start.source).ok()?;
    let meta = file.metadata().ok()?;
    if start.file != Some((meta.dev(), meta.ino())) || meta.len() < start.offset {
        return None;
    }
    match provider {
        ProviderKind::Claude => {
            let at = start.submitted_at?;
            let read = crate::services::claude::herdr_turn::ended_turn(&mut file, at, meta.len());
            Some(read.ok()? == crate::services::claude::herdr_turn::EndedTurn::Terminal)
        }
        _ => {
            use crate::services::codex_tui::rollout_tail::RolloutRecordDecoder as Decoder;
            let read = Decoder::replay_ended_herdr_turn(&mut file, start.offset, meta.len(), actor);
            Some(read? == crate::services::claude::herdr_turn::EndedTurn::Terminal)
        }
    }
}

/// Keeps the row; an ended Codex execution whose source never published is logged once per turn.
fn source_unread(
    row: &inflight::InflightTurnState,
    state: &HerdrInterruptState,
    token: &CancelToken,
) {
    let mut input = state.submission.lock().unwrap_or_else(|e| e.into_inner());
    if state.turn_start.get().is_none()
        && !std::mem::replace(&mut input.source_unknown_logged, true)
    {
        tracing::info!(
            event = "herdr_turn_held",
            provider = state.owner.provider,
            channel_id = state.owner.channel_id,
            logical = state.owner.logical_key,
            turn_nonce = token.turn_nonce(),
            hold_kind = "provider_terminal",
            hold_reason = "execution_ended_source_unknown",
            "Herdr turn remains held"
        );
    }
    drop(input);
    kept(row, "the ended turn's own source is unread");
}
