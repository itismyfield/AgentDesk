//! A live Herdr turn whose execution ended under it: its own source is read after the end, then
//! one body-free notice settles the captured turn and wakes its reader. Every unclear read keeps it.

use super::*;
use crate::services::discord::herdr_terminate::{TerminationResult, capture_turn, settle_captured};
use crate::services::provider::CancelToken;
use crate::services::provider::cancel_token_claude_interrupt::{HerdrInterruptState, herdr_turn};
use crate::services::provider::herdr_before_start::HerdrInputState;

/// This process's one Herdr state for the row's own turn, while no terminal kind or receipt owns it.
pub(super) fn live_target(
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    current: u64,
) -> Option<Arc<HerdrInterruptState>> {
    let open = row.tui_terminal_kind.is_none() && row.herdr_before_start_stop.is_none();
    if !hosted_provider(provider) || !open || !inflight::row_is_current_generation(row, current) {
        return None;
    }
    let nonce = row
        .turn_nonce
        .as_deref()
        .filter(|nonce| !nonce.is_empty())?;
    let logical = recovery_tmux_session_name(provider, row)?;
    herdr_turn(&logical, nonce).filter(|state| state.owner.logical_key == logical)
}

/// The channel's transition is held from the read through the settlement.
pub(super) async fn end_live(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    picked: inflight::InflightTurnState,
) {
    let channel = ChannelId::new(picked.channel_id);
    if let Ok(transition) = shared.session_transition_lock(channel).try_lock_owned() {
        settle(shared, provider, &picked).await;
        drop(transition);
    }
}

/// The captured actor's turn ends only once its pinned execution reads as ended (E) and its own
/// source, read after E, shows no terminal of it.
async fn settle(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    picked: &InflightTurnState,
) -> Option<()> {
    let fresh = inflight::load_inflight_state_read_only(provider, picked.channel_id);
    let same = |row: &InflightTurnState| {
        row.turn_nonce == picked.turn_nonce && row.save_generation == picked.save_generation
    };
    let row = fresh.filter(same)?;
    let state = live_target(provider, &row, shared.restart.current_generation)?;
    // Only the mailbox actor holding this very state is settled here.
    let channel = ChannelId::new(row.channel_id);
    let turn = capture_turn(shared, provider, channel).await.ok()??;
    let own = turn.token.herdr_interrupt_state();
    own.filter(|own| Arc::ptr_eq(own, &state))?;
    let pin = |input: &HerdrInputState| input.execution.clone();
    let nonce = pin(&state.submission.lock().unwrap_or_else(|e| e.into_inner()))?;
    let (pool, http) = (
        shared.pg_pool.as_ref()?,
        shared.serenity_http_or_token_fallback()?,
    );
    let read = || read_ended(provider, &state, &turn.token);
    let early = mutant("prb-read-before-end").then(read);
    let retire = execution_gone(pool, &shared.token_hash, provider, &row, &nonce).await?;
    let (kind, held, actor) = (provider.clone(), state.clone(), turn.token.clone());
    let late = || tokio::task::spawn_blocking(move || read_ended(&kind, &held, &actor));
    let read = match early {
        Some(read) => read,
        None => late().await.ok().flatten(),
    };
    match read {
        Some(false) => {}
        // Its own terminal is the reader's to admit: the native settlement owns the turn.
        Some(true) => return None,
        None => return source_unread(&row, &state, &turn.token),
    }
    if let Some((observation, record)) = &retire {
        retire_confirmed(pool, observation, record)
            .await
            .ok()
            .or_else(|| keep(&row, "retire"))?;
    }
    if release_hold(&nonce) != HoldRelease::Released {
        return keep(&row, "retired, hold release incomplete");
    }
    let text = interrupted_recovery_message(&row, "");
    let outcome = relay_recovery_body_notice(&http, shared, provider, &row, &text).await;
    if outcome != RecoveryRelayOutcome::Delivered {
        return keep(&row, "retired, notice not delivered");
    }
    let token = turn.token.clone();
    let settled = settle_captured(shared, provider, Some(turn)).await;
    if settled != TerminationResult::Retired {
        return keep(&row, "retired, settlement unconfirmed");
    }
    if !mutant("prb-no-wake") {
        // Watchers read the cleanup first, so the cancel below reads as a finished turn.
        token.mark_completion_cleanup();
        token.cancelled.store(true, Ordering::Release);
    }
    Some(())
}

fn keep<T>(row: &InflightTurnState, why: &str) -> Option<T> {
    kept(row, why);
    None
}

/// The pinned execution's end: `Some(None)` once it is already retired, `Some(Some(cas))` on a
/// fresh E; another execution, or an unconfirmed end, is `None`.
async fn execution_gone(
    pool: &sqlx::PgPool,
    token_hash: &str,
    provider: &ProviderKind,
    row: &InflightTurnState,
    nonce: &str,
) -> Option<Option<(HostedObservation, HostedExecution)>> {
    let (found, record) = hosted_record(pool, token_hash, provider, row).await?;
    if record.execution_nonce != nonce && !mutant("prb-successor-reread") {
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
    use crate::services::claude::herdr_turn::{EndedTurn, ended_turn};
    use crate::services::codex_tui::rollout_tail::RolloutRecordDecoder as Codex;
    let start = state.turn_start.get()?;
    let mut file = std::fs::File::open(&start.source).ok()?;
    let meta = file.metadata().ok()?;
    if start.file != Some((meta.dev(), meta.ino())) || meta.len() < start.offset {
        return None;
    }
    let read = match provider {
        ProviderKind::Claude => ended_turn(&mut file, start.submitted_at?, meta.len()).ok(),
        _ => Codex::replay_ended_herdr_turn(&mut file, start.offset, meta.len(), actor),
    };
    Some(read? == EndedTurn::Terminal)
}

/// An ended Codex execution whose source never published is logged once per turn.
fn source_unread(
    row: &InflightTurnState,
    state: &HerdrInterruptState,
    token: &CancelToken,
) -> Option<()> {
    let mut input = state.submission.lock().unwrap_or_else(|e| e.into_inner());
    let first = !std::mem::replace(&mut input.source_unknown_logged, true);
    if state.turn_start.get().is_none() && first {
        let reason = "execution_ended_source_unknown";
        let (logical, nonce) = (&state.owner.logical_key, token.turn_nonce());
        tracing::info!(
            event = "herdr_turn_held",
            logical,
            turn_nonce = nonce,
            hold_reason = reason,
            "Herdr turn remains held"
        );
    }
    drop(input);
    keep(row, "the ended turn's own source is unread")
}
