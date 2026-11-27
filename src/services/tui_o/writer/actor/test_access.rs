//! Test-only entry points to the actor's private state, for tests outside this crate layer.

use super::*;
use crate::services::tui_o::shadow::{CapturedRecord, SourceId};
use crate::services::tui_o::store::ledger::input_notices::NoticeEvidence;
use crate::services::tui_o::writer::input_notices::NoticeOrigin;

pub const WAIT_LIMIT_FOR_TEST: Duration = WAIT_LIMIT;

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink, B: BindingEvents> Actor<P, L, A, B> {
    /// An actor over `writer` with nothing owed, no transition receiver and no verified log.
    pub fn new_for_test(
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
    pub fn into_writer_and_bindings_for_test(self) -> (ChannelWriter<P, L, A>, Arc<B>) {
        (self.writer, self.bindings)
    }

    pub fn writer_for_test(&self) -> &ChannelWriter<P, L, A> {
        &self.writer
    }

    pub fn writer_mut_for_test(&mut self) -> &mut ChannelWriter<P, L, A> {
        &mut self.writer
    }

    pub fn deriver_mut_for_test(&mut self) -> &mut UnitDeriver {
        &mut self.deriver
    }

    pub fn notice_origin_for_test(
        &self,
        source: &SourceId,
        record: &CapturedRecord,
    ) -> Result<NoticeOrigin, String> {
        self.sources.notice_origin(source, record)
    }

    pub fn derive_scoped_for_test(
        &mut self,
        record: &CapturedRecord,
        source: &SourceId,
        version: u32,
        origin: Result<NoticeOrigin, String>,
        logical_session: impl Fn() -> Option<String>,
    ) -> Vec<Derived> {
        self.deriver
            .derive_scoped(record, source, version, origin, logical_session)
    }

    pub fn take_notice_evidence_for_test(&mut self) -> Vec<NoticeEvidence> {
        self.deriver.take_notice_evidence()
    }

    pub fn owed_for_test(&self) -> &VecDeque<Derived> {
        &self.owed
    }

    pub fn owed_mut_for_test(&mut self) -> &mut VecDeque<Derived> {
        &mut self.owed
    }

    pub fn sources_for_test(&self) -> &Sources<B> {
        &self.sources
    }

    pub fn bindings_for_test(&self) -> &Arc<B> {
        &self.bindings
    }

    pub fn replay_deferred_for_test(&self) -> bool {
        self.replay_deferred
    }

    pub fn set_replay_deferred_for_test(&mut self, deferred: bool) {
        self.replay_deferred = deferred;
    }

    pub fn waiting_ready_for_test(&self) -> &HashMap<(UnitKey, u32), tokio::time::Instant> {
        &self.waiting.ready
    }

    pub fn waiting_incident_for_test(&self) -> Option<&str> {
        self.waiting.incident.as_deref()
    }

    /// Loads the restart projection into the actor's deriver, as the actor's start does.
    pub async fn refresh_for_test(&mut self, provider: ShadowProvider) -> Result<(), WriterAlarm> {
        let bindings = self.bindings.as_ref();
        super::super::restart_end::refresh(&mut self.writer, &mut self.deriver, bindings, provider)
            .await
    }

    /// Restores the actor's sources into its writer, deriver and owed queue.
    pub fn resume_sources_for_test(&mut self) -> Result<(), WriterAlarm> {
        self.sources
            .resume(&mut self.writer, &mut self.deriver, &mut self.owed)
    }

    pub fn observe_waiting_for_test(&mut self, restored: bool) {
        self.observe_waiting(restored);
    }

    pub async fn deliver_owed_for_test(&mut self, progress: &super::super::progress::Publisher) {
        self.deliver_owed(progress).await;
    }

    pub fn collect_settled_for_test(&mut self) {
        self.collect_settled();
    }

    pub fn progress_for_test(&mut self) -> super::super::progress::ChannelProgress {
        self.progress()
    }

    pub fn undelivered_for_test(&mut self, asked: u64) -> Option<Undelivered> {
        self.undelivered(asked)
    }

    pub fn read_sources_for_test(&mut self) -> Vec<UnitKey> {
        self.read_sources()
    }
}
