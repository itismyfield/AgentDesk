//! Timed writer stages and distinct request-start and durable-Posted observations.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use super::ReplaySummary;
use crate::services::tui_o::shadow::ShadowProvider;

#[derive(Clone)]
pub(super) struct Trace(Arc<Run>);

struct Run {
    channel: u64,
    provider: Option<ShadowProvider>,
    generation: u64,
    attempt: u32,
    started: Instant,
    post: AtomicBool,
    posted: AtomicBool,
    drain: AtomicBool,
    done: AtomicU64,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Details {
    pub records: u64,
    pub done: u64,
    pub prepared: u64,
    pub serial: u64,
    pub msg_id: u64,
    pub epoch: u64,
    pub replay: ReplaySummary,
}

impl Trace {
    pub(super) fn new(
        channel: u64,
        provider: Option<ShadowProvider>,
        generation: u64,
        attempt: u32,
    ) -> Self {
        Self(Arc::new(Run {
            channel,
            provider,
            generation,
            attempt,
            started: Instant::now(),
            post: AtomicBool::new(false),
            posted: AtomicBool::new(false),
            drain: AtomicBool::new(false),
            done: AtomicU64::new(0),
        }))
    }

    pub(super) fn attempt(&self, attempt: u32) -> Self {
        if self.0.attempt == attempt {
            self.clone()
        } else {
            Self::new(self.0.channel, self.0.provider, self.0.generation, attempt)
        }
    }

    pub(super) fn phase(&self, stage: &'static str) -> Phase {
        self.emit(stage, "start", 0, Details::default());
        Phase {
            trace: self.clone(),
            stage,
            started: Instant::now(),
            finished: false,
        }
    }

    pub(super) fn first_drain(&self) -> Option<Phase> {
        (!self.0.drain.swap(true, Ordering::Relaxed)).then(|| self.phase("first_drain"))
    }

    pub(super) fn drained(&self) {
        self.0.done.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn done(&self) -> u64 {
        self.0.done.load(Ordering::Relaxed)
    }

    pub(super) fn first_post(&self, serial: u64, epoch: u64) {
        if !self.0.post.swap(true, Ordering::Relaxed) {
            self.emit(
                "first_post",
                "ok",
                0,
                Details {
                    serial,
                    epoch,
                    ..Details::default()
                },
            );
        }
    }

    pub(super) fn posted(
        &self,
        ledger: &crate::services::tui_o::store::ledger::LedgerState,
        serial: u64,
        msg_id: u64,
    ) {
        let epoch = ledger.piece(serial).map_or(0, |piece| piece.epoch);
        self.first_posted(serial, msg_id, epoch);
    }

    #[cfg(test)]
    pub(super) fn before_record(&self, entry: &crate::services::tui_o::store::ledger::LedgerEntry) {
        if super::replay::mutant("posted_before_append")
            && let crate::services::tui_o::store::ledger::LedgerEntry::Posted { serial, msg_id } =
                entry
        {
            self.first_posted(*serial, *msg_id, 0);
        }
    }

    #[cfg(test)]
    pub(super) fn constructing(&self, serial: u64, epoch: u64) {
        if super::replay::mutant("post_at_construction") {
            self.first_post(serial, epoch);
        }
    }

    pub(super) fn first_posted(&self, serial: u64, msg_id: u64, epoch: u64) {
        if !self.0.posted.swap(true, Ordering::Relaxed) {
            self.emit(
                "first_posted",
                "ok",
                0,
                Details {
                    serial,
                    msg_id,
                    epoch,
                    ..Details::default()
                },
            );
        }
    }

    pub(super) fn replay(
        &self,
        stage: &'static str,
        records: u64,
        summary: ReplaySummary,
        elapsed: u64,
    ) {
        self.emit(
            stage,
            "ok",
            elapsed,
            Details {
                records,
                replay: summary,
                ..Details::default()
            },
        );
    }

    fn emit(&self, stage: &'static str, status: &'static str, elapsed_ms: u64, details: Details) {
        let run = &self.0;
        let provider = match run.provider {
            Some(ShadowProvider::Claude) => "claude",
            Some(ShadowProvider::Codex) => "codex",
            None => "unknown",
        };
        tracing::info!(
            channel = run.channel,
            provider,
            generation = run.generation,
            attempt = run.attempt,
            stage,
            status,
            elapsed_ms,
            boot_ms = run.started.elapsed().as_millis() as u64,
            records = details.records,
            drain_steps = details.done,
            new_prepared = details.prepared,
            serial = details.serial,
            msg_id = details.msg_id,
            epoch = details.epoch,
            derived = details.replay.before,
            posted_skip = details.replay.posted,
            excluded_skip = details.replay.excluded,
            kept = details.replay.kept,
            deferred = details.replay.deferred,
            "[tui_o] writer boot stage"
        );
        #[cfg(test)]
        EVENTS.with_borrow_mut(|events| {
            if let Some(events) = events {
                events.push(Event {
                    channel: run.channel,
                    stage,
                    status,
                    details,
                });
            }
        });
    }
}

pub(super) struct Phase {
    trace: Trace,
    stage: &'static str,
    started: Instant,
    finished: bool,
}

impl Phase {
    pub(super) fn finish(self, status: &'static str) {
        self.finish_with(status, Details::default());
    }

    pub(super) fn finish_with(mut self, status: &'static str, details: Details) {
        self.trace.emit(
            self.stage,
            status,
            self.started.elapsed().as_millis() as u64,
            details,
        );
        self.finished = true;
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if !self.finished {
            self.trace.emit(
                self.stage,
                "cancelled",
                self.started.elapsed().as_millis() as u64,
                Details::default(),
            );
        }
    }
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub(super) struct Event {
    pub channel: u64,
    pub stage: &'static str,
    pub status: &'static str,
    pub details: Details,
}

#[cfg(test)]
thread_local! {
    static EVENTS: std::cell::RefCell<Option<Vec<Event>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn capture() {
    EVENTS.with_borrow_mut(|events| *events = Some(Vec::new()));
}

#[cfg(test)]
pub(super) fn events() -> Vec<Event> {
    EVENTS.with_borrow(|events| events.clone().unwrap_or_default())
}
