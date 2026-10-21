//! The mailbox `Clear` arm: release the active turn, drain the queue, its head and
//! reservation, and report every discarded inbound id.

use std::path::Path;

use poise::serenity_prelude::{ChannelId, MessageId};

use super::pending_queue_persistence::{
    load_channel_pending_dispatch_marker_checked, load_channel_pending_queue_checked,
    pending_queue_file_path, remove_channel_pending_dispatch_marker,
};
use super::{
    ChannelMailboxState, ClearChannelResult, QueueExitEvent, QueueExitKind,
    QueuePersistenceContext, clear_pending_user_dispatch, queued_head, release_active_turn_anchor,
};
use crate::services::discord::runtime_store::{AtomicWriteContext, critical_atomic_write};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ActivePolicy {
    Release,
    Preserve,
}

pub(super) fn dispatch(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: QueuePersistenceContext,
    active: ActivePolicy,
    reply: super::closed_verdict::VerdictReply<ClearChannelResult>,
) {
    let result = clear_channel_state(state, channel_id, persistence, active);
    if active == ActivePolicy::Release {
        super::mark_turn_finished_signal_done(channel_id);
    }
    let _ = reply.send(result);
}

/// What a clear could not write: each write runs again once after a failure.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClearPersistence {
    /// Writes whose retry landed.
    pub(crate) retried: Vec<&'static str>,
    /// Writes that failed twice; a restart may restore what they would have removed.
    pub(crate) failed: Vec<&'static str>,
    /// The mailbox did not answer or the dispatch marker was unreadable, so the count is unknown.
    pub(crate) count_unknown: bool,
    /// The running turn's ids with its absorbed sources and a starting turn's: never waiting.
    pub(crate) not_waiting: Vec<MessageId>,
}

impl ClearPersistence {
    /// Runs `write`, and once more if it fails; every failure is logged under `what`.
    fn attempt(
        &mut self,
        what: &'static str,
        mut write: impl FnMut() -> Result<(), String>,
    ) -> bool {
        let Err(first) = write() else {
            return true;
        };
        tracing::warn!(what, error = %first, "clear write failed; retrying once");
        match write() {
            Ok(()) => self.retried.push(what),
            Err(error) => self.fail(what, &error),
        }
        self.retried.last() == Some(&what)
    }

    fn fail(&mut self, what: &'static str, error: &str) {
        tracing::error!(what, %error, "clear write failed twice; a restart may restore discarded state");
        self.failed.push(what);
    }
}

/// Discards everything in memory first, so no write can bring it back, then rewrites the files.
pub(super) fn clear_channel_state(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: QueuePersistenceContext,
    active: ActivePolicy,
) -> ClearChannelResult {
    state.last_persistence = Some(persistence.clone());
    let namespace = super::remint_fence::namespace(&persistence);
    let disk_trusted = state.remint_fence.trusts_queue(&namespace);
    let read = (state.remint_fence.trusts_marker(&namespace))
        .then(|| dequeued_input(state, channel_id, &persistence))
        .transpose();
    let marker_unreadable = read.is_err();
    let marker = read.ok().flatten().flatten();
    let disk = if active == ActivePolicy::Preserve && disk_trusted {
        load_channel_pending_queue_checked(
            &persistence.provider,
            &persistence.token_hash,
            channel_id,
        )
        .map(|(items, _)| items)
    } else {
        Ok(Vec::new())
    };
    let queue_unreadable = disk.is_err();
    // Read before the release, which forgets the running turn's absorbed sources.
    let protected: Vec<MessageId> = state
        .active_user_message_id
        .into_iter()
        .chain(state.active_absorbed_source_ids.iter().copied())
        .collect();
    let mut not_waiting = protected.clone();
    let mut discarded_message_ids = Vec::new();
    let removed_token = if active == ActivePolicy::Release {
        not_waiting.extend(state.pending_user_dispatch);
        not_waiting.extend(state.pending_user_dispatch_source_ids.iter().copied());
        discarded_message_ids.extend(state.active_user_message_id);
        release_active_turn_anchor(state, channel_id, Some(&persistence.provider))
    } else {
        None
    };
    // Liveness-blind: an orphaned reservation is discarded all the same.
    discarded_message_ids.extend(state.pending_user_dispatch);
    discarded_message_ids.extend(state.pending_user_dispatch_source_ids.iter().copied());
    let mut waiting: Vec<_> = marker
        .into_iter()
        .chain(state.intervention_queue.drain(..))
        .chain(disk.unwrap_or_default())
        .collect();
    if active == ActivePolicy::Preserve {
        let seen = std::cell::RefCell::new(
            protected
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
        );
        super::active_source_dedup::purge_sources_from_queue(&mut waiting, |_, id| {
            !seen.borrow_mut().insert(id)
        });
    }
    let queue_exit_events: Vec<QueueExitEvent> = waiting
        .into_iter()
        .map(|intervention| QueueExitEvent::new(intervention, QueueExitKind::Superseded))
        .collect();
    for event in &queue_exit_events {
        let item = &event.intervention;
        discarded_message_ids.push(item.message_id);
        discarded_message_ids.extend(item.source_message_ids.iter().copied());
    }
    let (injected, unknown_reservation) = if active == ActivePolicy::Preserve {
        super::injected_inputs::discard_waiting_reservation(state, &protected)
    } else {
        (None, false)
    };
    discarded_message_ids.extend(injected);
    let waiting: Vec<_> = discarded_message_ids
        .iter()
        .copied()
        .filter(|id| !protected.contains(id))
        .collect();
    if active == ActivePolicy::Preserve {
        state.remint_fence.note_discarded(&waiting);
    }
    if active == ActivePolicy::Preserve {
        discarded_message_ids = waiting;
    }
    clear_pending_user_dispatch(state);
    state.recently_valve_cleared_dispatch = None;
    // An explicit clear discards the queued head too, whatever its phase.
    queued_head::release_for_clear(state, &persistence);
    state.remint_fence.begin_clear();
    let mut persistence = write_cleared_files(state, channel_id, &persistence, disk_trusted);
    persistence.not_waiting = not_waiting;
    persistence.count_unknown = marker_unreadable || queue_unreadable || unknown_reservation;
    ClearChannelResult {
        removed_token,
        queue_exit_events,
        discarded_message_ids,
        persistence,
    }
}

/// The input a dispatch marker still holds beyond what the queue or the active turn holds, so the
/// clear reports it discarded like the queue; an unreadable marker is an error.
fn dequeued_input(
    state: &ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
) -> Result<Option<super::Intervention>, String> {
    let (provider, token_hash) = (&persistence.provider, persistence.token_hash.as_str());
    let marker = load_channel_pending_dispatch_marker_checked(provider, token_hash, channel_id)
        .inspect_err(|error| tracing::warn!(%error, "clear could not read the dispatch marker"))?;
    let Some((mut marker, _)) = marker else {
        return Ok(None);
    };
    let held = |id: &MessageId| {
        state.active_user_message_id == Some(*id)
            || state
                .intervention_queue
                .iter()
                .any(|item| item.message_id == *id || item.source_message_ids.contains(id))
    };
    let mut ids = marker.source_message_ids.clone();
    if !ids.contains(&marker.message_id) {
        ids.push(marker.message_id);
    }
    let held: Vec<MessageId> = ids.iter().copied().filter(held).collect();
    if held.len() == ids.len() {
        return Ok(None);
    }
    for id in held {
        super::active_source_dedup::strip_source_message_id_from_intervention(&mut marker, id);
    }
    Ok(Some(marker))
}

/// Removes the queue file and dispatch marker and publishes the empty snapshot, each on its own, so
/// a restart reads as little old state as possible; only rewritten namespaces are trusted again.
fn write_cleared_files(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
    disk_trusted: bool,
) -> ClearPersistence {
    let mut report = ClearPersistence::default();
    let (provider, token_hash) = (&persistence.provider, persistence.token_hash.as_str());
    let Some(path) = pending_queue_file_path(provider, token_hash, channel_id) else {
        report.fail("queue_files", "pending queue root unavailable");
        return report;
    };
    if disk_trusted {
        queued_head::keep_disk_terminal(state, &path);
    }
    let unlinked = report.attempt("queue_unlink", || {
        #[cfg(test)]
        if let Some(after) = take_fault(channel_id, Op::Queue) {
            if after {
                let _ = std::fs::remove_file(&path);
            }
            return Err(format!("injected removal failure {}", path.display()));
        }
        unlink(&path)
    });
    let marker_removed = report.attempt("marker_unlink", || {
        #[cfg(test)]
        if let Some(after) = take_fault(channel_id, Op::Marker) {
            if after {
                remove_channel_pending_dispatch_marker(provider, token_hash, channel_id)?;
            }
            return Err("injected dispatch marker removal failure".to_string());
        }
        remove_channel_pending_dispatch_marker(provider, token_hash, channel_id)
    });
    let meta = Some(&state.queued_head.meta).filter(|meta| meta.is_set());
    let snapshot =
        queued_head::snapshot_json(Vec::new(), meta).unwrap_or_else(|_| "[]".to_string());
    let published = report.attempt("queue_publish", || {
        #[cfg(test)]
        if super::pending_queue_persistence::save_fault::take(channel_id) {
            return Err(format!(
                "injected pending queue save failure {}",
                path.display()
            ));
        }
        let context = AtomicWriteContext::new("discord_pending_queue")
            .provider(provider.as_str())
            .token_hash(token_hash)
            .channel_id(channel_id.get());
        critical_atomic_write(&path, &snapshot, context)
    });
    let namespace = super::remint_fence::namespace(persistence);
    if published {
        queued_head::record_write(state);
    } else if unlinked {
        state.remint_fence.trust_queue(&namespace);
    }
    if marker_removed {
        state.remint_fence.trust_marker(&namespace);
    }
    report
}

/// Removes `path`; a missing file is already removed. The parent is synced so the removal survives.
fn unlink(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => crate::services::discord::runtime_store::fsync_parent_dir(path)
            .map_err(|error| format!("sync after removing {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove {}: {error}", path.display())),
    }
}

/// Which removal a test fault fails.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Queue,
    Marker,
}

#[cfg(test)]
static FAULTS: std::sync::Mutex<Vec<(ChannelId, Op, bool)>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn take_fault(channel: ChannelId, op: Op) -> Option<bool> {
    let mut armed = FAULTS.lock().unwrap_or_else(|e| e.into_inner());
    let index = armed
        .iter()
        .position(|(c, o, _)| *c == channel && *o == op)?;
    Some(armed.remove(index).2)
}

#[cfg(test)]
impl super::ChannelMailboxRegistry {
    /// Test-only: fail the channel's next `times` clear removals of its queue file, or of its
    /// dispatch marker; `after_removal` removes the file before reporting the failure.
    pub(crate) fn fail_clear_removals(
        channel: ChannelId,
        marker: bool,
        times: usize,
        after_removal: bool,
    ) {
        let op = if marker { Op::Marker } else { Op::Queue };
        let mut armed = FAULTS.lock().unwrap_or_else(|e| e.into_inner());
        armed.extend(std::iter::repeat_n((channel, op, after_removal), times));
    }
}
