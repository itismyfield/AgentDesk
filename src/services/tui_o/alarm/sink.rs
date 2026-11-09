use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use super::{AlarmRouter, READER_RETENTION_GRACE, locked};
use crate::services::tui_o::writer::{AlarmSink, WriterAlarm, rotation::MAX_READERS};

impl AlarmSink for AlarmRouter {
    fn raise(&self, channel: u64, alarm: WriterAlarm) {
        self.raise_at(channel, &alarm, Instant::now());
    }

    fn reconcile_reader_count(&self, channel: u64, count: usize) {
        if count <= MAX_READERS {
            locked(&self.health.active).remove(&format!("tui_o:too_many_readers:{channel}"));
        }
    }

    fn reconcile_retained_readers(&self, channel: u64, reading: usize, retained: &[String]) {
        let reason = format!("tui_o:reader_retention_stalled:{channel}");
        let now = tokio::time::Instant::now();
        let was_stalled = locked(&self.health.active).contains(&reason);
        let pressured = reading >= MAX_READERS || was_stalled;
        let mut since = locked(&self.health.retained_readers);
        let stalled = if pressured && !retained.is_empty() {
            let current: HashSet<_> = retained.iter().map(String::as_str).collect();
            let readers = since.entry(channel).or_default();
            readers.retain(|key, _| current.contains(key.as_str()));
            for key in retained {
                readers.entry(key.clone()).or_insert(now);
            }
            was_stalled
                || retained.len() > MAX_READERS
                || readers.values().any(|started| {
                    now.saturating_duration_since(*started) >= READER_RETENTION_GRACE
                })
        } else {
            since.remove(&channel);
            false
        };
        drop(since);
        if stalled {
            self.raise(
                channel,
                WriterAlarm::ReaderRetentionStalled {
                    reading,
                    retained: retained.len(),
                },
            );
        } else {
            locked(&self.health.active).remove(&reason);
        }
    }

    fn reconcile_boundary_pending(&self, channel: u64, count: usize) {
        let reason = format!("tui_o:boundary_pending:{channel}");
        let mut active = locked(&self.health.active);
        if count == 0 {
            active.remove(&reason);
        } else {
            active.insert(reason);
        }
    }

    fn resume_pending(&self, channel: u64, pending: bool) {
        self.health.set_resume_pending(channel, pending);
    }

    fn halt_cleared(&self, channel: u64) {
        self.health.clear_halted(channel);
    }

    fn waiting_cleared(&self, channel: u64) {
        self.health.clear_waiting(channel);
    }
}

impl AlarmSink for Arc<AlarmRouter> {
    fn raise(&self, channel: u64, alarm: WriterAlarm) {
        self.as_ref().raise(channel, alarm);
    }

    fn reconcile_reader_count(&self, channel: u64, count: usize) {
        self.as_ref().reconcile_reader_count(channel, count);
    }

    fn reconcile_retained_readers(&self, channel: u64, reading: usize, retained: &[String]) {
        self.as_ref()
            .reconcile_retained_readers(channel, reading, retained);
    }

    fn reconcile_boundary_pending(&self, channel: u64, count: usize) {
        self.as_ref().reconcile_boundary_pending(channel, count);
    }

    fn resume_pending(&self, channel: u64, pending: bool) {
        self.as_ref().resume_pending(channel, pending);
    }

    fn halt_cleared(&self, channel: u64) {
        self.as_ref().halt_cleared(channel);
    }

    fn waiting_cleared(&self, channel: u64) {
        self.as_ref().waiting_cleared(channel);
    }
}
