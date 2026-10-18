//! The THREAD-GUARD clean releases only the witnessed stale episode: its actor matches it,
//! withdraws its parent routes and releases its token in one step; the rest follows outside.

use super::*;
use crate::services::discord::inflight::{GuardedClearOutcome, InflightTurnIdentity};
use crate::services::turn_orchestrator::witnessed_release::{
    ReleaseWithdrawal, WitnessedEpisodeRelease,
};

/// The stale episode a THREAD-GUARD verdict proved, and the parents that then pointed at it.
#[derive(Clone)]
pub(in crate::services::discord) struct ThreadGuardStaleWitness {
    thread: serenity::ChannelId,
    anchor: serenity::MessageId,
    nonce: String,
    /// Taken before the row and mailbox reads, so an episode started later fails the release.
    observed_before: std::time::Instant,
    row: InflightTurnIdentity,
    parents: Vec<serenity::ChannelId>,
}

impl ThreadGuardStaleWitness {
    /// The mailbox anchor and nonce and the stale row filed under `thread` must name one
    /// episode; an unanchored (id 0) episode yields none.
    pub(super) fn bind(
        shared: &SharedData,
        thread: serenity::ChannelId,
        proof: &StaleActiveTurnProof,
    ) -> Option<Self> {
        let row = &proof.inflight;
        let anchor = (proof.snapshot.mailbox_active_user_msg_id)
            .filter(|id| *id != 0 && *id == row.user_msg_id)?;
        let nonce = (proof.snapshot.mailbox_active_turn_nonce.clone())
            .filter(|nonce| !nonce.is_empty() && row.turn_nonce.as_ref() == Some(nonce))?;
        if row.channel_id != thread.get() || row.restart_mode.is_some() || row.rebind_origin {
            return None;
        }
        let parents = (shared.dispatch.thread_parents.iter())
            .filter(|route| *route.value() == thread)
            .map(|route| *route.key())
            .collect();
        Some(Self {
            thread,
            anchor: serenity::MessageId::new(anchor),
            nonce,
            observed_before: proof.observed_before,
            row: InflightTurnIdentity::from_state(row),
            parents,
        })
    }

    /// A witness of the episode `row` names, filed under `thread`, as tests stage it.
    #[cfg(test)]
    pub(in crate::services::discord) fn for_test(
        thread: serenity::ChannelId,
        row: &crate::services::discord::inflight::InflightTurnState,
        observed_before: std::time::Instant,
        parents: Vec<serenity::ChannelId>,
    ) -> Self {
        Self {
            thread,
            anchor: serenity::MessageId::new(row.user_msg_id),
            nonce: row.turn_nonce.clone().expect("a nonce"),
            observed_before,
            row: InflightTurnIdentity::from_state(row),
            parents,
        }
    }
}

/// Drops the witnessed parents still pointing at the thread; a route mapped since, remapped to
/// another thread or foreign stays. Runs inside the thread's actor, so it only touches the map.
struct ThreadParentWithdrawal {
    shared: Arc<SharedData>,
    thread: serenity::ChannelId,
    parents: Vec<serenity::ChannelId>,
}

impl ReleaseWithdrawal for ThreadParentWithdrawal {
    fn withdraw(self: Box<Self>) -> Vec<serenity::ChannelId> {
        let thread = self.thread;
        (self.parents.iter().copied())
            .filter(|parent| {
                self.shared
                    .dispatch
                    .thread_parents
                    .remove_if(parent, |_, mapped| *mapped == thread)
                    .is_some()
            })
            .collect()
    }
}

/// Releases the witnessed episode, clears its row, finalizes it without a session kill and kicks;
/// `false` (queue behind the thread) on a successor, a before-start receipt or a gone actor.
pub(super) async fn thread_guard_force_clean_stale_thread(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    witness: ThreadGuardStaleWitness,
) -> bool {
    use crate::services::discord::inflight;
    let thread = witness.thread;
    if inflight::before_start_receipt_present(provider, thread.get())
        && !inflight::receipt_mutant("stale-a")
    {
        return false;
    }
    let withdrawal = Box::new(ThreadParentWithdrawal {
        shared: shared.clone(),
        thread,
        parents: witness.parents.clone(),
    });
    let persistence = crate::services::discord::queue_persistence_context(shared, provider, thread);
    let (anchor, nonce, cutoff) = (
        witness.anchor,
        witness.nonce.clone(),
        witness.observed_before,
    );
    let Some(release) =
        WitnessedEpisodeRelease::new(anchor, nonce, cutoff, withdrawal, persistence)
    else {
        return false;
    };
    let handle = shared.mailbox(thread);
    let released = handle.release_witnessed_episode(release).await;
    let finish = released.finish;
    let Some(removed) = finish.removed_token.clone() else {
        tracing::info!(
            thread_id = thread.get(),
            "THREAD-GUARD: thread episode changed since the stale proof; keeping it"
        );
        return false;
    };
    crate::services::discord::apply_queue_exit_feedback(shared, thread, &finish.queue_exit_events)
        .await;
    handle.recovery_done().mark_done();
    #[cfg(test)]
    pause::reached(thread).await;
    let cleared = inflight::clear_inflight_state_for_captured_episode(
        provider,
        thread.get(),
        &witness.row,
        Some(&witness.nonce),
    );
    if !matches!(
        cleared,
        GuardedClearOutcome::Cleared | GuardedClearOutcome::Missing
    ) {
        tracing::warn!(
            thread_id = thread.get(),
            ?cleared,
            "THREAD-GUARD: released the stale episode but kept a row it no longer names"
        );
    }
    tracing::info!(
        thread_id = thread.get(),
        "THREAD-GUARD: released the stale thread episode and proceeding"
    );
    crate::services::discord::stall_recovery::finalize_orphaned_clear_preserve_session(
        shared,
        thread,
        Some(removed),
        "1446_thread_guard_stale_inflight",
    );
    // A parent remapped to another thread between the withdrawal and this kick is still kicked:
    // the same race kind as every edge release, though not the same window.
    crate::services::discord::turn_finalizer::cleanup::kickoff_thread_parents_after_finalize(
        shared,
        provider,
        released.withdrawn,
    );
    crate::services::discord::turn_completion_events::publish_mailbox_release_completion_event(
        shared,
        thread,
        Some(anchor.get()),
        &finish,
    );
    if finish.has_pending {
        let reason = "thread_guard_stale_finish";
        crate::services::discord::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            thread,
            reason,
        );
    }
    true
}

/// Test seam: a clean of an armed thread waits after its release until the test resumes it.
#[cfg(test)]
pub(in crate::services::discord) mod pause {
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock, Mutex};

    use poise::serenity_prelude::ChannelId;
    use tokio::sync::Notify;

    #[derive(Default)]
    pub(in crate::services::discord) struct Gate {
        pub(in crate::services::discord) reached: Notify,
        pub(in crate::services::discord) resume: Notify,
    }

    static GATES: LazyLock<Mutex<HashMap<u64, Arc<Gate>>>> = LazyLock::new(Default::default);

    /// Arms the next clean of `thread`.
    pub(in crate::services::discord) fn arm(thread: ChannelId) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        GATES.lock().unwrap().insert(thread.get(), gate.clone());
        gate
    }

    pub(super) async fn reached(thread: ChannelId) {
        let gate = GATES.lock().unwrap().remove(&thread.get());
        if let Some(gate) = gate {
            gate.reached.notify_one();
            gate.resume.notified().await;
        }
    }
}
