//! Test-only entry points to the actor's private state, for tests outside this crate layer.

use super::*;

pub(in crate::services) const WAIT_LIMIT_FOR_TEST: Duration = WAIT_LIMIT;

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink, B: BindingEvents> Actor<P, L, A, B> {
    /// An actor over `writer` with nothing owed, no transition receiver and no verified log.
    pub(in crate::services) fn new_for_test(
        writer: ChannelWriter<P, L, A>,
        deriver: UnitDeriver,
        sources: Sources<B>,
        bindings: Arc<B>,
    ) -> Self {
        Self {
            replay_deferred: false,
            writer,
            deriver,
            owed: VecDeque::new(),
            verified: None,
            sources,
            bindings,
            transitions: transition::Transitions::new(None, watch::channel(false).1),
            notice: None,
            waiting: Waiting::default(),
            clock: (tokio::time::Instant::now(), chrono::Utc::now()),
            scope_hold: None,
            boot_previous_nonce: None,
        }
    }

    /// The writer and bindings a fresh `run_channel_at` takes over; the rest of the actor drops.
    pub(in crate::services) fn into_writer_and_bindings_for_test(
        self,
    ) -> (ChannelWriter<P, L, A>, Arc<B>) {
        (self.writer, self.bindings)
    }

    pub(in crate::services) fn writer_for_test(&self) -> &ChannelWriter<P, L, A> {
        &self.writer
    }

    pub(in crate::services) fn writer_mut_for_test(&mut self) -> &mut ChannelWriter<P, L, A> {
        &mut self.writer
    }

    pub(in crate::services) fn deriver_mut_for_test(&mut self) -> &mut UnitDeriver {
        &mut self.deriver
    }

    pub(in crate::services) fn owed_for_test(&self) -> &VecDeque<Derived> {
        &self.owed
    }

    pub(in crate::services) fn owed_mut_for_test(&mut self) -> &mut VecDeque<Derived> {
        &mut self.owed
    }

    pub(in crate::services) fn sources_for_test(&self) -> &Sources<B> {
        &self.sources
    }

    pub(in crate::services) fn bindings_for_test(&self) -> &Arc<B> {
        &self.bindings
    }

    pub(in crate::services) fn replay_deferred_for_test(&self) -> bool {
        self.replay_deferred
    }

    pub(in crate::services) fn set_replay_deferred_for_test(&mut self, deferred: bool) {
        self.replay_deferred = deferred;
    }

    pub(in crate::services) fn waiting_ready_for_test(
        &self,
    ) -> &HashMap<(UnitKey, u32), tokio::time::Instant> {
        &self.waiting.ready
    }

    pub(in crate::services) fn waiting_incident_for_test(&self) -> Option<&str> {
        self.waiting.incident.as_deref()
    }

    /// Loads the restart projection into the actor's deriver, as the actor's start does.
    pub(in crate::services) async fn refresh_for_test(
        &mut self,
        provider: ShadowProvider,
    ) -> Result<(), WriterAlarm> {
        let bindings = self.bindings.as_ref();
        super::super::restart_end::refresh(&mut self.writer, &mut self.deriver, bindings, provider)
            .await
    }

    /// Restores the actor's sources into its writer, deriver and owed queue.
    pub(in crate::services) fn resume_sources_for_test(&mut self) -> Result<(), WriterAlarm> {
        self.sources
            .resume(&mut self.writer, &mut self.deriver, &mut self.owed)
    }

    pub(in crate::services) fn observe_waiting_for_test(&mut self, restored: bool) {
        self.observe_waiting(restored);
    }

    pub(in crate::services) async fn deliver_owed_for_test(
        &mut self,
        progress: &super::super::progress::Publisher,
    ) {
        self.deliver_owed(progress).await;
    }

    pub(in crate::services) fn collect_settled_for_test(&mut self) {
        self.collect_settled();
    }

    pub(in crate::services) fn progress_for_test(
        &mut self,
    ) -> super::super::progress::ChannelProgress {
        self.progress()
    }

    pub(in crate::services) fn undelivered_for_test(&mut self, asked: u64) -> Option<Undelivered> {
        self.undelivered(asked)
    }

    pub(in crate::services) fn read_sources_for_test(&mut self) -> Vec<UnitKey> {
        self.read_sources()
    }
}
