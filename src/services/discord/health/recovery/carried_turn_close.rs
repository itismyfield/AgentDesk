//! Closes a turn-mode channel's turn carried over a restart once its native turn ended and O
//! delivered it: the restored earlier-generation row and episode no owner would otherwise finish.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use super::*;
use crate::services::discord::inflight::{self, GuardedClearOutcome, InflightTurnState};
use crate::services::discord::turn_finalizer::{
    FinalizeContext, SyntheticClaimSnapshot, TerminalEvent, TurnKey,
};
use crate::services::discord::turn_presence::activity::{ClosedBoundary, NoBoundary};
use crate::services::tui_o::turn_mode::carried_close_mutant as mutant;

/// How long the close waits for the O actor's poll after it asked; O polls every second.
const OWED_READ_LIMIT: Duration = Duration::from_secs(3);

const ARCHIVE_REASON: &str = "turn-mode-carried-turn";

/// The episode an archived row left open, retried until it no longer holds the mailbox.
#[derive(Clone)]
struct Target {
    finalizer_turn_id: u64,
    nonce: Option<String>,
    snapshot: SyntheticClaimSnapshot,
    born_generation: u64,
}

static ARCHIVED: LazyLock<Mutex<HashMap<u64, Target>>> = LazyLock::new(Default::default);

fn archived() -> std::sync::MutexGuard<'static, HashMap<u64, Target>> {
    ARCHIVED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Closes every confirmed turn-mode channel's carried turn this tick can prove ended; returns the
/// channels closed.
pub(super) async fn run_carried_turn_close_pass(
    registry: &HealthRegistry,
    provider: &ProviderKind,
) -> HashSet<ChannelId> {
    let mut closed = HashSet::new();
    if *provider != ProviderKind::Claude {
        return closed;
    }
    for channel in crate::services::tui_o::turn_mode::confirmed_channels() {
        let channel = ChannelId::new(channel);
        let Some(shared) = registry
            .shared_for_provider_on_channel(provider, channel)
            .await
        else {
            continue;
        };
        if close_channel(&shared, provider, channel).await {
            closed.insert(channel);
        }
    }
    closed
}

/// The mailbox episode as the close judged it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Episode {
    message: Option<u64>,
    nonce: Option<String>,
    token: bool,
}

async fn episode(shared: &Arc<SharedData>, channel: ChannelId) -> Episode {
    let snapshot = discord::mailbox_snapshot(shared, channel).await;
    Episode {
        message: snapshot.active_user_message_id.map(|id| id.get()),
        nonce: snapshot.active_turn_nonce.clone(),
        token: snapshot.cancel_token.is_some(),
    }
}

impl Episode {
    fn is(&self, finalizer_turn_id: u64, nonce: Option<&str>) -> bool {
        self.token
            && self.message == Some(finalizer_turn_id)
            && (self.nonce.as_deref() == nonce || mutant("nonce"))
    }

    fn empty(&self) -> bool {
        !self.token && self.message.is_none()
    }
}

/// What the close stands on: the carried row (gone once archived) and its episode.
enum Carried {
    Row(Box<InflightTurnState>),
    Archived(Box<Target>),
}

/// The reason each channel's carried turn was last kept, logged once per change.
static KEPT: LazyLock<Mutex<HashMap<u64, String>>> = LazyLock::new(Default::default);

async fn close_channel(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> bool {
    let result = try_close(shared, provider, channel).await;
    let mut last = KEPT.lock().unwrap_or_else(|e| e.into_inner());
    match &result {
        Err(Kept::Because(reason)) if last.get(&channel.get()) != Some(reason) => {
            tracing::info!(
                provider = provider.as_str(),
                channel_id = channel.get(),
                reason = reason.as_str(),
                "[tui_o] carried-over turn kept"
            );
            last.insert(channel.get(), reason.clone());
        }
        Err(Kept::Because(_)) => {}
        Ok(()) | Err(Kept::NoRow) => {
            last.remove(&channel.get());
        }
    }
    result.is_ok()
}

/// Why a pass left a channel's carried turn open.
enum Kept {
    /// Nothing carried: the channel's ordinary state, not logged.
    NoRow,
    Because(String),
}

fn kept(reason: impl Into<String>) -> Kept {
    Kept::Because(reason.into())
}

async fn try_close(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<(), Kept> {
    if !crate::services::tui_o::turn_mode::transcript_turns(channel.get()) {
        return Err(kept("not_turn_mode"));
    }
    if !super::super::transcript_binding_stall::o_owns(channel.get()) {
        return Err(kept("not_o_owned"));
    }
    let generation = shared.restart.current_generation;
    let row = inflight::load_inflight_state(provider, channel.get());
    let remembered = archived().get(&channel.get()).cloned();
    let carried = match (row, remembered) {
        (Some(row), _) => {
            if !discord::tui_direct_pending_start::inflight_generation_precedes_current(
                &row, generation,
            ) && !mutant("generation")
            {
                return Err(kept("current_generation"));
            }
            Carried::Row(Box::new(row))
        }
        (None, Some(target)) => Carried::Archived(Box::new(target)),
        (None, None) => return Err(Kept::NoRow),
    };
    let before = episode(shared, channel).await;
    let target_episode = match &carried {
        Carried::Row(row) => {
            let nonce = row.turn_nonce.as_deref();
            if !before.empty() && !before.is(row.effective_finalizer_turn_id(), nonce) {
                let other = before.message == Some(row.effective_finalizer_turn_id());
                return Err(kept(match other {
                    true => "episode_other_nonce",
                    false => "episode_other_turn",
                }));
            }
            !before.empty()
        }
        Carried::Archived(target) => {
            if !before.is(target.finalizer_turn_id, target.nonce.as_deref()) {
                archived().remove(&channel.get());
                return Err(kept("archived_episode_gone"));
            }
            true
        }
    };
    let end = native_end(shared, provider, channel).await.map_err(|no| {
        kept(format!(
            "no_native_end:{}:owed_inputs={}",
            no.reason, no.owed_inputs
        ))
    })?;
    o_delivered_through(channel, &end).await.map_err(kept)?;
    // Injection holds this lock through its paste and settlement; a busy lock is this tick's no-op.
    let transition = match shared.session_transition_lock(channel).try_lock_owned() {
        Ok(transition) => Some(transition),
        Err(_) if mutant("lock") => None,
        Err(_) => return Err(kept("transition_lock_busy")),
    };
    #[cfg(test)]
    tests::pause("locked", channel.get()).await;
    if !still_closed(shared, provider, channel, &end).await {
        return Err(kept("native_moved_under_lock"));
    }
    if input_delivered_since(provider, channel, &end) && !mutant("delivery") {
        return Err(kept("unconfirmed_injection_since_growth"));
    }
    if episode(shared, channel).await != before {
        return Err(kept("episode_changed_under_lock"));
    }
    let target = match carried {
        Carried::Row(row) => {
            let identity = inflight::InflightTurnIdentity::from_state(&row);
            #[cfg(test)]
            tests::pause("archive", channel.get()).await;
            let archive = match mutant("archive") {
                true => GuardedClearOutcome::Cleared,
                false => inflight::archive_inflight_state_if_matches_identity_generation(
                    provider,
                    channel.get(),
                    &identity,
                    &row.updated_at,
                    row.save_generation,
                    ARCHIVE_REASON,
                ),
            };
            if archive != GuardedClearOutcome::Cleared {
                tracing::info!(
                    provider = provider.as_str(),
                    channel_id = channel.get(),
                    ?archive,
                    "[tui_o] carried-over turn kept: its row changed before the archive"
                );
                return Err(kept(format!("archive_{archive:?}")));
            }
            let target = Target {
                finalizer_turn_id: row.effective_finalizer_turn_id(),
                nonce: row.turn_nonce.clone(),
                snapshot: SyntheticClaimSnapshot::from_row(&row),
                born_generation: row.born_generation,
            };
            if target_episode {
                archived().insert(channel.get(), target.clone());
            }
            target
        }
        Carried::Archived(target) => *target,
    };
    #[cfg(test)]
    let withheld = tests::withhold_finalize(channel.get());
    #[cfg(not(test))]
    let withheld = false;
    if target_episode && !withheld {
        #[cfg(test)]
        tests::note_event(
            channel.get(),
            match end.aborted && !mutant("event") {
                true => "cancel",
                false => "complete",
            },
        );
        let event = if end.aborted && !mutant("event") {
            TerminalEvent::Cancel
        } else {
            TerminalEvent::Complete
        };
        let _ = shared
            .turn_finalizer
            .submit_terminal_with_claim_snapshot(
                TurnKey::new(channel, target.finalizer_turn_id, generation),
                provider.clone(),
                event,
                // The row is archived already; nothing is left for the finalizer to clear.
                FinalizeContext {
                    clear_inflight: mutant("archive"),
                    allow_completion_cleanup: false,
                    drain_voice: false,
                    kickoff_queue: true,
                    expected_idempotent_guard_miss: false,
                },
                Some(target.snapshot.clone()),
                shared.clone(),
            )
            .await;
    }
    drop(transition);
    let released = !episode(shared, channel)
        .await
        .is(target.finalizer_turn_id, target.nonce.as_deref());
    let row_gone = inflight::load_inflight_state(provider, channel.get())
        .is_none_or(|row| row.effective_finalizer_turn_id() != target.finalizer_turn_id);
    if !(released && row_gone) {
        tracing::warn!(
            provider = provider.as_str(),
            channel_id = channel.get(),
            finalizer_turn_id = target.finalizer_turn_id,
            released,
            row_gone,
            "[tui_o] carried-over turn archived but its episode is still open; retrying next pass"
        );
        return Err(kept("archived_episode_open"));
    }
    archived().remove(&channel.get());
    tracing::info!(
        provider = provider.as_str(),
        channel_id = channel.get(),
        finalizer_turn_id = target.finalizer_turn_id,
        born_generation = target.born_generation,
        aborted = end.aborted,
        episode_released = target_episode,
        "[tui_o] carried-over turn closed: its native turn ended and O delivered it"
    );
    Ok(())
}

/// The transcript's native end, read fresh.
async fn native_end(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<ClosedBoundary, NoBoundary> {
    #[cfg(test)]
    if let Some(end) = tests::native_end(channel.get()) {
        let scripted = NoBoundary {
            reason: "scripted",
            owed_inputs: 0,
        };
        return end.ok_or(scripted);
    }
    let reading = discord::turn_presence::activity::reading_now(shared, provider, channel).await;
    reading.closed_boundary().cloned()
}

/// The native end read again under the lock: the same source and position, nothing appended.
async fn still_closed(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    end: &ClosedBoundary,
) -> bool {
    native_end(shared, provider, channel)
        .await
        .is_ok_and(|now| now.source == end.source && now.through == end.through)
}

/// An unconfirmed injection settled at or after the transcript last grew may have reached the pane
/// with no record yet; an observed one left a receipt the reader tracks per input.
fn input_delivered_since(
    provider: &ProviderKind,
    channel: ChannelId,
    end: &ClosedBoundary,
) -> bool {
    discord::inject_disposition::last_unconfirmed_delivery(provider, channel)
        .is_some_and(|delivered| delivered >= end.grew_at)
}

/// O, asked after the native end was read, owes nothing and captured that source through it.
async fn o_delivered_through(channel: ChannelId, end: &ClosedBoundary) -> Result<(), String> {
    let readiness = crate::services::tui_o::writer::host::process_readiness();
    let Some(owed) = readiness
        .undelivered_after(channel.get(), OWED_READ_LIMIT)
        .await
    else {
        let hosted = readiness.undelivered(channel.get()).is_some();
        return Err(format!("o_unanswered:hosted={hosted}"));
    };
    let drained = mutant("debt")
        || (owed.owed == 0
            && owed.prepared == 0
            && owed.unsealed == 0
            && owed.uncaptured == 0
            && owed.binding_pending == 0
            && owed.verified_work == 0
            && owed.posting == 0);
    if !drained {
        return Err(format!(
            "o_owes:owed={} prepared={} unsealed={} uncaptured={} binding_pending={} \
             verified_work={} posting={}",
            owed.owed,
            owed.prepared,
            owed.unsealed,
            owed.uncaptured,
            owed.binding_pending,
            owed.verified_work,
            owed.posting
        ));
    }
    let source = owed
        .captured
        .iter()
        .find(|(path, _)| same_source(path, &end.source));
    match source {
        Some((_, through)) if *through >= end.through || mutant("capture") => Ok(()),
        Some((_, through)) => Err(format!("o_capture_behind:{through}<{}", end.through)),
        None => Err(format!("o_source_unbound:sources={}", owed.captured.len())),
    }
}

fn same_source(left: &Path, right: &Path) -> bool {
    left == right
        || matches!(
            (left.canonicalize(), right.canonicalize()),
            (Ok(left), Ok(right)) if left == right
        )
}

#[cfg(test)]
#[path = "carried_turn_close_tests.rs"]
mod tests;
