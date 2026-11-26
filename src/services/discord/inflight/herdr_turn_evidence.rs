//! A Herdr turn's start, saved on its row when recorded, lets the watcher end that turn after its
//! token is gone: only the turn's own terminal, read from that start, ends it.

use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use crate::services::discord::SharedData;
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

/// Saves `start` on the row of `turn_nonce` once, then only pins a cold start's file to the first
/// one its reader opened. A failed save leaves that row held, never the input.
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
    let recorded = PersistedHerdrTurnStart {
        turn_nonce: turn_nonce.to_owned(),
        start: start.clone(),
    };
    if row.turn_nonce.as_deref() != Some(turn_nonce)
        || row
            .herdr_turn_start
            .as_ref()
            .is_some_and(|saved| !pins_cold_file(saved, &recorded))
    {
        return;
    }
    row.herdr_turn_start = Some(recorded);
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

/// Whether `recorded` only pins `saved`, the same turn's unpinned start, to the file its reader opened.
fn pins_cold_file(saved: &PersistedHerdrTurnStart, recorded: &PersistedHerdrTurnStart) -> bool {
    let unpinned = HerdrTurnStart {
        file: None,
        ..recorded.start.clone()
    };
    saved.turn_nonce == recorded.turn_nonce
        && recorded.start.file.is_some()
        && saved.start == unpinned
}

/// A save of the same turn keeps the saved start and may only pin its file; a successor turn never
/// inherits it.
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
    outgoing.herdr_turn_start = match (kept, carried.filter(|s| own(outgoing, s))) {
        (Some(kept), Some(saved)) if !pins_cold_file(&saved, &kept) => Some(saved),
        (kept, carried) => kept.or(carried),
    };
}

/// The fresh-idle signal for `row`: a Herdr turn ends on its own terminal only and is held otherwise.
pub(in crate::services::discord) async fn herdr_fresh_idle_signal(
    shared: &SharedData,
    provider: &ProviderKind,
    row: Option<&InflightTurnState>,
    logical: &str,
    signal: CompletionSignal,
) -> CompletionSignal {
    match watcher_herdr_evidence(shared, provider, row, logical).await {
        HerdrTurnEvidence::NotHerdr => signal,
        HerdrTurnEvidence::Own => CompletionSignal::Done,
        HerdrTurnEvidence::Insufficient => CompletionSignal::PausedLive,
    }
}

/// Whether a watcher path that would clear `row`, release its mailbox or cancel its token must keep
/// them: a Herdr row whose own end is not proven.
pub(in crate::services::discord) async fn herdr_turn_held(
    shared: &SharedData,
    provider: &ProviderKind,
    row: Option<&InflightTurnState>,
    logical: &str,
) -> bool {
    watcher_herdr_evidence(shared, provider, row, logical).await == HerdrTurnEvidence::Insufficient
}

/// The watcher's evidence for `row`, judged against the channel's current hosted execution.
pub(in crate::services::discord) async fn watcher_herdr_evidence(
    shared: &SharedData,
    provider: &ProviderKind,
    row: Option<&InflightTurnState>,
    logical: &str,
) -> HerdrTurnEvidence {
    let Some(row) = row.filter(|row| row.herdr_row(logical)) else {
        return HerdrTurnEvidence::NotHerdr;
    };
    let bound = current_bound_nonce(shared, provider, row).await;
    row.herdr_turn_evidence(logical, bound.as_deref())
}

/// The channel's current hosted execution nonce while it is Bound; `None` for any other state,
/// a missing record or a failed read.
async fn current_bound_nonce(
    shared: &SharedData,
    provider: &ProviderKind,
    row: &InflightTurnState,
) -> Option<String> {
    use crate::db::dispatched_session_canonical_identity::{
        CanonicalSessionIdentity, SessionIdentityKind,
    };
    use crate::db::dispatched_sessions::hosted_execution::{
        self as hosted, HostedLookup, HostedLookupKey, HostedRecord, HostedState,
    };
    let pool = shared.pg_pool.as_ref()?;
    let channel = row.channel_id.to_string();
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: &shared.token_hash,
        channel_id: &channel,
    };
    let key = HostedLookupKey::Canonical {
        provider: provider.as_str(),
        identity,
    };
    let HostedLookup::Found(found) = hosted::load_hosted_execution_pg(pool, key).await else {
        return None;
    };
    match found.record {
        HostedRecord::Known(record) if record.state == HostedState::Bound => {
            Some(record.execution_nonce)
        }
        _ => None,
    }
}

impl InflightTurnState {
    /// `Own` only for the terminal of this row's turn read from its saved start, in the same file
    /// of the current Bound execution `bound`; a Herdr row without that is `Insufficient`.
    pub(in crate::services::discord) fn herdr_turn_evidence(
        &self,
        logical: &str,
        bound: Option<&str>,
    ) -> HerdrTurnEvidence {
        if !self.herdr_row(logical) {
            return HerdrTurnEvidence::NotHerdr;
        }
        match &self.herdr_turn_start {
            Some(saved) if own_terminal(self, saved, bound) => HerdrTurnEvidence::Own,
            _ => HerdrTurnEvidence::Insufficient,
        }
    }

    /// A saved Herdr start, a Herdr locator, or a pane a launch listed under `logical`.
    fn herdr_row(&self, logical: &str) -> bool {
        let listed = crate::services::tui_prompt_dedupe::herdr_execution_listed;
        self.herdr_turn_start.is_some() || self.has_native_herdr_input() || listed(logical)
    }
}

#[cfg(unix)]
fn own_terminal(
    row: &InflightTurnState,
    saved: &PersistedHerdrTurnStart,
    bound: Option<&str>,
) -> bool {
    use crate::services::claude::herdr_turn::{EndedTurn, ended_turn};
    use crate::services::codex_tui::rollout_tail::RolloutRecordDecoder;
    use std::os::unix::fs::MetadataExt;
    let start = &saved.start;
    let execution = Some(start.execution_nonce.as_str());
    if row.turn_nonce.as_deref() != Some(saved.turn_nonce.as_str())
        || row.hosted_execution_nonce.as_deref() != execution
        || bound != execution
    {
        return false;
    }
    // A cold start whose reader never opened its file has no proven source.
    let Some(pinned) = start.file else {
        return false;
    };
    let Ok(mut file) = std::fs::File::open(&start.source) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    let len = meta.len();
    if pinned != (meta.dev(), meta.ino()) || len < start.offset {
        return false;
    }
    let ended = match row.provider_kind() {
        Some(ProviderKind::Claude) => start
            .submitted_at
            .and_then(|at| ended_turn(&mut file, at, len).ok()),
        Some(ProviderKind::Codex) => {
            RolloutRecordDecoder::replay_restored_herdr_turn(&mut file, start.offset, len)
        }
        _ => None,
    };
    ended == Some(EndedTurn::Terminal)
}

#[cfg(not(unix))]
fn own_terminal(_: &InflightTurnState, _: &PersistedHerdrTurnStart, _: Option<&str>) -> bool {
    false
}
