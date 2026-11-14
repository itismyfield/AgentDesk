//! The mailbox `Clear` arm: release the active turn, drain the queue, its head and
//! reservation, and report every discarded inbound id.

use std::path::Path;

use poise::serenity_prelude::{ChannelId, MessageId};

use super::closed_verdict::MailboxRefusal;
use super::pending_queue_persistence::{
    load_channel_pending_dispatch_marker_checked, load_channel_pending_queue_checked,
    pending_queue_file_path, remove_channel_pending_dispatch_marker,
};
use super::{
    ChannelMailboxState, ClearChannelResult, QueueExitEvent, QueueExitKind,
    QueuePersistenceContext, clear_pending_user_dispatch, queued_head, release_active_turn_anchor,
};
use crate::services::discord::runtime_store::{AtomicWriteContext, critical_publish};

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
    let _ = reply.send(clear_and_signal(state, channel_id, persistence, active));
}

/// Clears the state and frees turn-finished waiters when the active turn is released.
fn clear_and_signal(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: QueuePersistenceContext,
    active: ActivePolicy,
) -> ClearChannelResult {
    let result = clear_channel_state(state, channel_id, persistence, active);
    if active == ActivePolicy::Release
        && !result.persistence.failed.contains(&"queue_read_only")
        && !result.persistence.failed.contains(&"native_input")
    {
        super::mark_turn_finished_signal_done(channel_id);
    }
    result
}

impl super::ChannelMailboxRegistry {
    /// Clears through the registered actor. With none, runs a new actor's clear on a scratch state
    /// while the vacant entry blocks a racing spawn, so the files are cleared and no mailbox is minted.
    pub(crate) async fn clear_without_spawn(
        &self,
        channel_id: ChannelId,
        persistence: QueuePersistenceContext,
    ) -> Result<(ClearChannelResult, Option<super::ChannelMailboxHandle>), MailboxRefusal> {
        let handle = match self.handles.entry(channel_id) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(_spawn_blocked) => {
                let mut state =
                    ChannelMailboxState::spawned(channel_id, self.fence_cell(channel_id));
                let result =
                    clear_and_signal(&mut state, channel_id, persistence, ActivePolicy::Release);
                return Ok((result, None));
            }
        };
        let result = handle.clear_or_refused(persistence).await?;
        Ok((result, Some(handle)))
    }
}

/// What a clear could not write: each write runs again once after a failure.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClearPersistence {
    /// Writes whose retry landed.
    pub(crate) retried: Vec<&'static str>,
    /// Writes that failed twice; a restart may restore what they would have removed.
    pub(crate) failed: Vec<&'static str>,
    /// Durable input could not be counted, or the mailbox did not answer.
    pub(crate) count_unknown: bool,
    /// The running turn's ids with its absorbed sources and a starting turn's: never waiting.
    pub(crate) not_waiting: Vec<MessageId>,
}

impl ClearPersistence {
    /// Runs `write`, and once more if it fails; every failure is logged under `what`.
    pub(super) fn attempt(
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
    // Unknown snapshot versions stay untouched; supported V2 clears through its owner.
    if let Ok(path) =
        pending_queue_file_path(&persistence.provider, &persistence.token_hash, channel_id)
        && let Ok(raw) = std::fs::read_to_string(path)
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw)
        && value.is_object()
        && value.get("version").and_then(serde_json::Value::as_u64) != Some(1)
        && !matches!(
            queued_head::compat::inspect(&raw),
            Ok(queued_head::compat::SnapshotRead::ReadOnlyV2(_))
        )
    {
        return ClearChannelResult {
            persistence: ClearPersistence {
                failed: vec!["queue_read_only"],
                count_unknown: true,
                ..Default::default()
            },
            ..Default::default()
        };
    }
    if active == ActivePolicy::Release
        && super::policy_close::admit_control(state, channel_id, false).is_err()
    {
        return ClearChannelResult {
            persistence: ClearPersistence {
                failed: vec!["native_input"],
                count_unknown: true,
                ..Default::default()
            },
            ..Default::default()
        };
    }
    if let Some(result) = queued_head::clear_v2_channel(state, channel_id, &persistence, active) {
        return result;
    }
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
        critical_publish(&path, &snapshot, context)
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
        Ok(()) => crate::domain::file_sync::fsync_parent_dir(path)
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

#[cfg(all(test, unix))]
mod merge_tests {
    use super::*;
    use crate::services::provider::{CancelToken, ProviderKind};
    use crate::services::turn_orchestrator::policy_close::ControlCustody;
    use futures::FutureExt;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    struct Admission {
        allowed: AtomicBool,
        calls: AtomicUsize,
    }
    impl ControlCustody for Admission {
        fn input(&self) -> &str {
            "clear-admission"
        }
        fn owns(
            &self,
            _: ChannelId,
            _: Option<&Arc<CancelToken>>,
            _: Option<MessageId>,
            _: Option<&str>,
        ) -> bool {
            true
        }
        fn admit(&self, _: bool) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.allowed
                .load(Ordering::SeqCst)
                .then_some(())
                .ok_or("native admission unavailable".into())
        }
    }

    fn context() -> QueuePersistenceContext {
        QueuePersistenceContext::new(&ProviderKind::Claude, "merge-clear", None)
    }

    #[test]
    fn native_8148_merge_clear_admission_precedes_cold_and_adopted_owner() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        for adopted_first in [false, true] {
            let channel = ChannelId::new(8_148_987 + u64::from(adopted_first));
            let (path, namespace) = queued_head::v2_queue(&context(), channel).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({
                "version":2,"namespace":namespace,"items":[],"head_injection":null,
                "retained_inputs":[],"terminal_identities":[]}))
                .unwrap(),
            )
            .unwrap();
            let item: super::super::PendingQueueItem = serde_json::from_value(serde_json::json!({
                "author_id":7,"message_id":101,"channel_id":channel.get(),
                "source_message_ids":[101],"queued_generation":1,"text":"waiting"}))
            .unwrap();
            let mut snapshot: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            snapshot["items"] = serde_json::json!([item]);
            std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
            let actor = Arc::new(CancelToken::new());
            let mut state = ChannelMailboxState::default();
            state.cancel_token = Some(actor.clone());
            state.active_user_message_id = Some(MessageId::new(100));
            if adopted_first {
                let adopted = queued_head::clear_v2_channel(
                    &mut state,
                    channel,
                    &context(),
                    ActivePolicy::Preserve,
                )
                .unwrap();
                assert!(adopted.persistence.failed.is_empty());
                std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
            }
            let before = std::fs::read(&path).unwrap();
            let admission = Arc::new(Admission {
                allowed: AtomicBool::new(false),
                calls: AtomicUsize::new(0),
            });
            state.control_custody = Some(admission.clone());
            let signal = super::super::turn_finished_signal(channel);
            signal.reset();

            let refused = clear_and_signal(&mut state, channel, context(), ActivePolicy::Release);
            assert_eq!(refused.persistence.failed, ["native_input"]);
            assert!(refused.removed_token.is_none());
            assert_eq!(admission.calls.load(Ordering::SeqCst), 1);
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(Arc::ptr_eq(state.cancel_token.as_ref().unwrap(), &actor));
            assert!(signal.wait().now_or_never().is_none());

            admission.allowed.store(true, Ordering::SeqCst);
            let cleared = clear_and_signal(&mut state, channel, context(), ActivePolicy::Release);
            assert!(cleared.persistence.failed.is_empty());
            assert!(cleared.removed_token.is_some());
            assert_eq!(admission.calls.load(Ordering::SeqCst), 2);
            assert!(state.cancel_token.is_none());
            assert!(signal.wait().now_or_never().is_some());
            let after: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(after["version"], 2);
            assert_eq!(after["items"], serde_json::json!([]));
            assert_eq!(
                after["admission_binds"],
                serde_json::from_slice::<serde_json::Value>(&before).unwrap()["admission_binds"]
            );
        }
    }

    #[test]
    fn native_8148_merge_clear_read_only_refusal_preserves_signal_and_native_intent() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let channel = ChannelId::new(8_148_989);
        let path =
            pending_queue_file_path(&context().provider, &context().token_hash, channel).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let before = br#"{"version":3,"items":[]}"#;
        std::fs::write(&path, before).unwrap();
        let admission = Arc::new(Admission {
            allowed: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
        });
        let mut state = ChannelMailboxState::default();
        state.cancel_token = Some(Arc::new(CancelToken::new()));
        state.control_custody = Some(admission.clone());
        let signal = super::super::turn_finished_signal(channel);
        signal.reset();
        let refused = clear_and_signal(&mut state, channel, context(), ActivePolicy::Release);
        assert_eq!(refused.persistence.failed, ["queue_read_only"]);
        assert!(refused.removed_token.is_none());
        assert!(state.cancel_token.is_some());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(admission.calls.load(Ordering::SeqCst), 0);
        assert!(signal.wait().now_or_never().is_none());
    }
}
