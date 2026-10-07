//! O's own writer over the real binding log of one Codex channel, for executor tests that need
//! O's delivery: each step follows the log, captures what is owed and posts it.

use super::*;
use crate::services::tui_o::writer::binding::BindingLog;
use crate::services::tui_o::writer::rotation::Sources;
use crate::services::tui_prompt_dedupe::binding_events::SourceId;

pub(crate) const O_CHANNEL: u64 = CHANNEL;

pub(crate) struct ODrive {
    harness: Harness,
    writer: Writer,
    sources: Sources<BindingLog>,
    deriver: UnitDeriver,
    owed: VecDeque<Derived>,
}

impl ODrive {
    /// A writer on a ready channel's store, which holds a binding checkpoint, here before any
    /// logged record; call it on a thread that sees the test log.
    pub(crate) fn new() -> Self {
        let harness = Harness::new();
        harness.channel().set_binding_checkpoint(0).unwrap();
        harness.gate.acquired();
        let mut drive = Self {
            writer: harness.writer(),
            sources: Sources::new(CHANNEL, ShadowProvider::Codex, Arc::new(BindingLog)),
            deriver: UnitDeriver::new(CHANNEL, ShadowProvider::Codex),
            owed: VecDeque::new(),
            harness,
        };
        let resumed = drive
            .sources
            .resume(&mut drive.writer, &mut drive.deriver, &mut drive.owed);
        resumed.unwrap();
        drive
    }

    /// Every post so far, after following the log and posting what it owes now.
    pub(crate) async fn step(&mut self) -> Vec<String> {
        self.sources.follow(&mut self.writer).unwrap();
        let (writer, deriver, owed) = (&mut self.writer, &mut self.deriver, &mut self.owed);
        self.sources.capture(writer, deriver, owed).unwrap();
        while let Some(item) = self.owed.pop_front() {
            assert_eq!(self.writer.deliver(&item).await, Step::Done);
        }
        self.harness.port.posts()
    }

    /// How far O has captured `source`, from its own store.
    pub(crate) fn captured_through(&mut self, source: &SourceId) -> Option<u64> {
        let cursor = self.writer.store().cursor(source);
        cursor.map(|cursor| cursor.captured_through)
    }
}
