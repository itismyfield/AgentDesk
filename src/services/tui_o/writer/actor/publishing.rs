//! Runs one channel's capture and delivery phases in spool order.

use super::*;
use crate::services::tui_o::writer;

/// [`run_projecting`] that also publishes what the channel still owes, beside its count.
pub(super) async fn run_publishing<P, L, A, B>(
    writer: ChannelWriter<P, L, A>,
    provider: ShadowProvider,
    bindings: Arc<B>,
    (mut stop, resumed): (watch::Receiver<bool>, watch::Sender<bool>),
    (unsettled, owing): (Unsettled, Owing),
    (utc, readers, producer_absent, _native): (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<ReaderConfig>,
        bool,
        Option<native_input::NativePort>,
    ),
    (mut commands, transitions): (
        Option<codex_submission::Receiver>,
        Option<transition_port::Receiver>,
    ),
) -> Option<StopCause>
where
    P: DiscordPort,
    L: DeliveryLease,
    A: AlarmSink,
    B: BindingEvents,
{
    let mut writer = writer.with_repost_stop(stop.clone());
    let deriver =
        fresh_resume::configured_deriver(writer.channel(), provider, writer.store().ledger());
    let mut sources = Sources::new(writer.channel(), provider, Arc::clone(&bindings))
        .with_suspension(writer.suspension.clone())
        .with_producer_absent(producer_absent)
        .with_native_inputs(_native.is_some());
    if let Some(readers) = readers {
        sources = sources.with_readers(readers);
    }
    let owed = VecDeque::new();
    let ready = admission::owed_delivery_ready(writer.channel());
    let verified = ready.then(|| writer::owed::OwedLog::new(writer.channel(), provider));
    let mut actor = Actor {
        replay_deferred: false,
        writer,
        deriver,
        owed,
        verified,
        sources,
        bindings,
        transitions: transition::Transitions::new(transitions, stop.clone()),
        notice: None,
        waiting: Waiting::default(),
        scope_hold: None,
        boot_previous_nonce: None,
        clock: (
            tokio::time::Instant::now(),
            utc.unwrap_or_else(chrono::Utc::now),
        ),
    };
    let selection = actor.writer.suspension.clone();
    let restore = provider == ShadowProvider::Codex;
    if restore {
        match selection.restore_current(actor.writer.store()) {
            Ok(restored) => {
                if !restored
                    && crate::services::tui_prompt_dedupe::clear_relaunch::fresh_scope_eligible(
                        actor.writer.channel(),
                    )
                {
                    actor.boot_previous_nonce = actor
                        .writer
                        .store()
                        .fresh_selection()
                        .ok()
                        .flatten()
                        .map(|record| record.nonce);
                }
            }
            Err(error) => {
                actor.writer.stop(WriterAlarm::Halted {
                    detail: format!("fresh selection restore: {error:?}"),
                });
                return actor.writer.stop_cause().cloned();
            }
        }
    }
    let progress = writer::progress::Publisher::new(actor.writer.channel());
    let mut sources_resumed = false;
    let mut quiescent = false;
    let mut tick = idle::Tick::new(&actor);
    let mut next = None;
    while !actor.writer.is_stopped() && !*stop.borrow() {
        if let Some(delay) = next.take()
            && !tick
                .wait(&mut actor, &owing.demand, &mut commands, &mut stop, delay)
                .await
        {
            break;
        }
        if actor.writer.is_stopped() || *stop.borrow() {
            break;
        }
        next = Some(POLL_INTERVAL);
        match actor.rescope() {
            fresh_resume::Rescope::Changed => {
                quiescent = false;
                sources_resumed = false;
                resumed.send_replace(false);
            }
            fresh_resume::Rescope::Busy => {
                quiescent = false;
                next = None;
                // Later source reads can take the same lock, so defer the whole poll.
                tokio::select! {
                    () = tokio::time::sleep(POLL_INTERVAL) => {},
                    changed = stop.changed() => if changed.is_err() { break },
                }
                continue;
            }
            fresh_resume::Rescope::Unchanged => {}
        }
        if actor.writer.is_stopped() {
            break;
        }
        let can_skip = quiescent && !owing.demand.wanted() && !actor.transitions.pending();
        if !tick.needs_work(actor.writer.store(), can_skip) {
            let serial = actor.writer.store().ledger().next_serial();
            actor.repost_if_ready().await;
            if actor.writer.store().ledger().next_serial() != serial {
                quiescent = false;
                progress.publish(actor.progress());
            }
            #[cfg(unix)]
            if let Some(port) = &_native {
                port.inner.retry().await;
            }
            continue;
        }
        let restart_ready = match writer::restart_end::refresh(
            &mut actor.writer,
            &mut actor.deriver,
            actor.bindings.as_ref(),
            provider,
        )
        .await
        {
            Ok(()) => true,
            Err(alarm) => {
                if !sources_resumed {
                    resumed.send_replace(false);
                }
                tracing::warn!(?alarm, "[tui_o] restart witness remains unreadable");
                false
            }
        };
        if restart_ready && sources_resumed {
            resumed.send_replace(true);
        }
        let mut fresh = Vec::new();
        if !sources_resumed
            && restart_ready
            && actor.sources_allowed()
            && actor.start_notice_migration()
        {
            let phase = actor.writer.boot.phase("resume");
            let (writer, deriver) = (&mut actor.writer, &mut actor.deriver);
            let restored = match &mut actor.verified {
                Some(log) => actor.sources.resume(writer, deriver, log),
                None => actor.sources.resume(writer, deriver, &mut actor.owed),
            };
            match &mut actor.verified {
                Some(log) => {
                    writer::restart_end::finish(actor.writer.store(), &mut actor.deriver, log)
                }
                None => writer::restart_end::finish(
                    actor.writer.store(),
                    &mut actor.deriver,
                    &mut actor.owed,
                ),
            }
            let restored = restored.and_then(|()| actor.sources.follow(&mut actor.writer));
            if let Err(alarm) = restored {
                actor.writer.stop(alarm);
            }
            actor.replay_deferred = actor.verified.is_none() && actor.sources.replayed.deferred;
            phase.finish(if actor.writer.is_stopped() {
                "stopped"
            } else {
                "ok"
            });
            sources_resumed = true;
            if !actor.writer.is_stopped() {
                actor.boot_previous_nonce = None;
                actor.project_operator_resumes();
                fresh = actor.persist_notice_evidence();
                if !actor.writer.is_stopped() {
                    actor.writer.halt_cleared();
                    resumed.send_replace(true);
                }
            }
        }
        actor.prepare_notices(&fresh).await;
        actor.observe_waiting(false);
        let drain = actor.writer.boot.first_drain();
        let serial_before = drain
            .as_ref()
            .map(|_| actor.writer.store().ledger().next_serial());
        let now = actor.utc_now();
        if actor.delivery_allowed() {
            actor.writer.retry_local(now).await;
            actor.refilter_after_open();
        }
        if !actor.writer.has_local_retry() {
            actor.deliver_owed(&progress).await;
        }
        // A restart-cut episode's one notice goes after this poll's owed pieces.
        if sources_resumed && actor.delivery_allowed() && !actor.writer.has_local_retry() {
            actor.writer.deliver_restart_notices().await;
        }
        if let Some(drain) = drain {
            let prepared = actor
                .writer
                .store()
                .ledger()
                .next_serial()
                .saturating_sub(serial_before.unwrap_or(0));
            drain.finish_with(
                if actor.writer.is_stopped() {
                    "stopped"
                } else {
                    "ok"
                },
                writer::boot::Details {
                    done: actor.writer.boot.done(),
                    prepared,
                    ..writer::boot::Details::default()
                },
            );
        }
        actor.repost_if_ready().await;
        actor.collect_settled();
        let fresh = if restart_ready {
            actor.read_sources()
        } else {
            Vec::new()
        };
        #[cfg(unix)]
        if let Some(port) = &_native {
            actor.sources.offer_native(port).await;
        }
        actor.prepare_notices(&fresh).await;
        actor.poll_transition(sources_resumed, &unsettled).await;
        actor.observe_waiting(sources_resumed);
        let position = actor.progress();
        progress.publish(position);
        #[cfg(test)]
        fresh_resume::note_work(actor.owed.len(), actor.deriver.has_unsealed());
        // A writer that stopped in this poll ends now, so its readiness drops before the next poll.
        if actor.writer.is_stopped() {
            break;
        }
        // Only a Herdr-configured channel's clear reads it, so other channels skip the store read.
        let channel = actor.writer.channel();
        let herdr = crate::services::session_host::thread_host::runs_herdr(channel);
        if herdr {
            unsettled.send_replace(actor.unsettled());
        }
        // A turn-mode carried turn closes on this read too, and a thread child's store retires on
        // it. Read only while a reader waits on it, so a channel nobody asks about adds no reads.
        if herdr
            || crate::services::tui_o::turn_mode::transcript_turns(channel)
            || crate::services::tui_o::cutover::thread_child(channel)
        {
            let asked = owing.demand.asked();
            if owing.demand.wanted() {
                owing.published.send_replace(actor.undelivered(asked));
            }
        }
        // A wake can capture a new piece after the delivery phase. Drain it without another timer.
        if quiescent && position.debt != 0 {
            next = Some(Duration::ZERO);
        }
        quiescent = sources_resumed
            && restart_ready
            && position.debt == 0
            && !position.unsealed
            && !actor.replay_deferred
            && actor.scope_hold.is_none()
            && actor.sources.quiescent_for_stamp()
            && !actor.deriver.has_pending_projection_probe()
            && !actor.transitions.pending();
    }
    actor.writer.stop_cause().cloned()
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink, B: BindingEvents> Actor<P, L, A, B> {
    async fn repost_if_ready(&mut self) {
        // Re-post holds no exact permit, so a verified channel never runs it.
        if self.verified.is_none()
            && self.notice_only_owed()
            && !self.writer.has_local_retry()
            && self.delivery_allowed()
        {
            self.writer.repost_turn().await;
        }
    }
}
