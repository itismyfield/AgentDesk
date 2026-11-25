//! A Herdr turn's start, saved on its row when recorded, lets the watcher end that turn after its
//! token is gone: only the turn's own terminal, read from that start, ends it.

use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use crate::services::discord::turn_finalizer::CompletionSignal;
use crate::services::provider::cancel_token_claude_interrupt::HerdrTurnStart;

/// A recorded start and the turn it was recorded for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct PersistedHerdrTurnStart {
    pub(in crate::services::discord) turn_nonce: String,
    #[serde(flatten)]
    pub(in crate::services::discord) start: HerdrTurnStart,
}

/// What a row shows of its Herdr turn's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum HerdrTurnEvidence {
    NotHerdr,
    Own,
    Insufficient,
}

/// Routes recorded starts to their rows; a live registry installs it.
pub(in crate::services::discord) fn install_turn_start_sink() {
    crate::services::provider::cancel_token_claude_interrupt::install_turn_start_sink(persist);
}

/// Saves `start` on the row of `turn_nonce` once. A failed save leaves that row held, never the input.
fn persist(owner: &HostedOwner, turn_nonce: &str, start: &HerdrTurnStart) {
    let provider = ProviderKind::from_str(&owner.provider);
    let (Some(provider), Ok(channel)) = (provider, owner.channel_id.parse::<u64>()) else {
        return;
    };
    let Some(root) = inflight_runtime_root() else {
        return;
    };
    let path = inflight_state_path(&root, &provider, channel);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return;
    };
    let Some(mut row) = load_inflight_state_unlocked(&path) else {
        return;
    };
    if row.turn_nonce.as_deref() != Some(turn_nonce) || row.herdr_turn_start.is_some() {
        return;
    }
    row.herdr_turn_start = Some(PersistedHerdrTurnStart {
        turn_nonce: turn_nonce.to_owned(),
        start: start.clone(),
    });
    if !matches!(
        store::persist_under_lock_with_snapshot(&root, &path, &row, "herdr_turn_start"),
        Ok(Some(_))
    ) {
        tracing::warn!(
            channel_id = channel,
            "herdr turn start not saved; the watcher holds its row"
        );
    }
}

/// A stale snapshot of the same turn keeps the saved start; a successor turn never inherits it.
pub(super) fn carry_forward(
    existing: Option<&InflightTurnState>,
    outgoing: &mut InflightTurnState,
) {
    let own = |row: &InflightTurnState, saved: &PersistedHerdrTurnStart| {
        row.turn_nonce.as_deref() == Some(saved.turn_nonce.as_str())
    };
    let kept = outgoing
        .herdr_turn_start
        .take()
        .filter(|s| own(outgoing, s));
    let carried = existing.and_then(|row| row.herdr_turn_start.clone().filter(|s| own(row, s)));
    outgoing.herdr_turn_start = kept.or(carried.filter(|s| own(outgoing, s)));
}

/// The fresh-idle signal for `row`: a Herdr turn ends on its own terminal only and is held otherwise.
pub(in crate::services::discord) fn herdr_fresh_idle_signal(
    row: Option<&InflightTurnState>,
    logical: &str,
    signal: CompletionSignal,
) -> CompletionSignal {
    let Some(row) = row else {
        return signal;
    };
    match row.herdr_turn_evidence(logical) {
        HerdrTurnEvidence::NotHerdr => signal,
        HerdrTurnEvidence::Own => CompletionSignal::Done,
        HerdrTurnEvidence::Insufficient => CompletionSignal::PausedLive,
    }
}

impl InflightTurnState {
    /// `Own` only for the terminal of this row's turn read from its saved start, in the same file
    /// of the same execution; a Herdr row without that is `Insufficient`.
    pub(in crate::services::discord) fn herdr_turn_evidence(
        &self,
        logical: &str,
    ) -> HerdrTurnEvidence {
        let listed = crate::services::tui_prompt_dedupe::herdr_execution_listed;
        if self.herdr_turn_start.is_none() && !self.has_native_herdr_input() && !listed(logical) {
            return HerdrTurnEvidence::NotHerdr;
        }
        match &self.herdr_turn_start {
            Some(saved) if own_terminal(self, saved) => HerdrTurnEvidence::Own,
            _ => HerdrTurnEvidence::Insufficient,
        }
    }
}

#[cfg(unix)]
fn own_terminal(row: &InflightTurnState, saved: &PersistedHerdrTurnStart) -> bool {
    use crate::services::claude::herdr_turn::{EndedTurn, ended_turn};
    use crate::services::codex_tui::rollout_tail::RolloutRecordDecoder;
    use std::os::unix::fs::MetadataExt;
    let start = &saved.start;
    if row.turn_nonce.as_deref() != Some(saved.turn_nonce.as_str())
        || row.hosted_execution_nonce.as_deref() != Some(start.execution_nonce.as_str())
    {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(&start.source) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    let len = meta.len();
    if start
        .file
        .is_some_and(|file| file != (meta.dev(), meta.ino()))
        || len < start.offset
    {
        return false;
    }
    let ended = match row.provider_kind() {
        Some(ProviderKind::Claude) => start
            .submitted_at
            .and_then(|at| ended_turn(&mut file, at, len).ok()),
        Some(ProviderKind::Codex) => {
            let actor = std::sync::Arc::new(crate::services::provider::CancelToken::new());
            RolloutRecordDecoder::replay_ended_herdr_turn(&mut file, start.offset, len, &actor)
        }
        _ => None,
    };
    ended == Some(EndedTurn::Terminal)
}

#[cfg(not(unix))]
fn own_terminal(_: &InflightTurnState, _: &PersistedHerdrTurnStart) -> bool {
    false
}
