//! Dormant bounded bootstrap and the current Codex boundary's evidence.
use super::*;
use crate::services::tui_o::writer::input_facts::{Boundary, BoundaryKind, ChannelFact};

#[derive(Clone, Copy)]
struct Limits {
    full: u64,
    tail: u64,
    poll: u64,
    worker: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            full: MAX_TRANSCRIPT_BYTES,
            tail: 32 * 1024 * 1024,
            poll: CHUNK_BUDGET,
            worker: WORKER_BUDGET,
        }
    }
}
impl Limits {
    fn tail_start(self, len: u64) -> Option<u64> {
        (len > self.full).then(|| len.saturating_sub(self.tail))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum CodexBoundary {
    Unproven,
    Open {
        id: String,
        opener: (u64, u64),
    },
    Closed {
        id: String,
        opener: (u64, u64),
        closer: (u64, u64),
    },
}
struct TailMeta {
    window_start: u64,
    bootstrap_end: u64,
    codex: CodexBoundary,
}
impl TailMeta {
    fn absorb(&mut self, boundaries: &[Boundary]) {
        for boundary in boundaries {
            let Some(id) = boundary.native_turn_id.as_ref().filter(|id| !id.is_empty()) else {
                // An anonymous boundary invalidates any earlier named turn's proof.
                self.codex = CodexBoundary::Unproven;
                continue;
            };
            self.codex = match (&self.codex, boundary.kind) {
                (_, BoundaryKind::Start) => CodexBoundary::Open {
                    id: id.clone(),
                    opener: boundary.range,
                },
                (CodexBoundary::Open { id: open, opener }, BoundaryKind::Close) if open == id => {
                    CodexBoundary::Closed {
                        id: id.clone(),
                        opener: *opener,
                        closer: boundary.range,
                    }
                }
                (CodexBoundary::Closed { id: closed, .. }, BoundaryKind::Close) if closed == id => {
                    self.codex.clone()
                }
                _ => CodexBoundary::Unproven,
            };
        }
    }
    fn carry(&mut self, resume: &Resume) {
        let matches = match (&resume.carried, &self.codex) {
            (
                TurnState::Open {
                    native_turn_id: Some(carried),
                },
                CodexBoundary::Open { id, .. },
            ) => carried == id,
            (TurnState::Idle, CodexBoundary::Closed { .. }) => true,
            _ => false,
        };
        if !matches {
            self.codex = CodexBoundary::Unproven;
        }
    }
    fn allows_idle(&self, provider: ShadowProvider, fact: &ChannelFact, caught_up: bool) -> bool {
        caught_up
            && fact.state == TurnState::Idle
            && (provider == ShadowProvider::Claude
                || matches!(self.codex, CodexBoundary::Closed { .. }))
    }
}
struct TailReader {
    facts: InputFacts,
    meta: TailMeta,
    remaining: u64,
    limits: Limits,
}
impl TailReader {
    fn open(binding: SourceBinding, limits: Limits) -> Result<Self, String> {
        let end = std::fs::metadata(&binding.source.path)
            .map_err(|e| e.to_string())?
            .len();
        let start = end.saturating_sub(limits.tail);
        let facts = InputFacts::open_observation_at(binding, start)?;
        if facts.source_len()? < end {
            return Err("source shrank before bootstrap".into());
        }
        Ok(Self {
            facts,
            meta: TailMeta {
                window_start: start,
                bootstrap_end: end,
                codex: CodexBoundary::Unproven,
            },
            remaining: limits.tail,
            limits,
        })
    }
    fn poll(&mut self, max: u64, end: Option<u64>) -> Result<ChannelFact, String> {
        let result = match end {
            Some(end) => self.facts.poll_observation_until(max, end, self.remaining),
            None => self.facts.poll(max),
        };
        self.meta.absorb(self.facts.boundaries());
        if result.is_err() && self.facts.resume_point().is_none() {
            self.meta.codex = CodexBoundary::Unproven;
        }
        result
    }
    fn resume(&mut self) -> Result<(), String> {
        let resume = self
            .facts
            .resume_point()
            .cloned()
            .ok_or("not a typed block")?;
        self.meta.carry(&resume);
        self.facts = InputFacts::resume_observation(self.facts.binding().clone(), &resume)?;
        Ok(())
    }
    fn bootstrap(&mut self) -> Result<(), String> {
        loop {
            let before = self.facts.observation_progress();
            let result = self.poll(
                self.limits.worker.min(self.remaining),
                Some(self.meta.bootstrap_end),
            );
            let after = self.facts.observation_progress();
            self.remaining = self
                .remaining
                .saturating_sub(after.1.saturating_sub(before.1));
            if let Err(error) = result {
                if self.facts.resume_point().is_none() {
                    return Err(error);
                }
                self.resume()?;
            }
            if self.remaining == 0
                || before == after
                || self.facts.observation_progress().0 >= self.meta.bootstrap_end
            {
                break;
            }
        }
        Ok(())
    }
    fn poll_installed(&mut self) -> Result<ChannelFact, String> {
        self.poll(self.limits.poll, None)
    }
}
#[cfg(test)]
#[path = "tail_tests.rs"]
mod tests;
