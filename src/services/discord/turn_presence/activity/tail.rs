//! Bounded bootstrap and the current Codex boundary's evidence.
use super::*;
use crate::services::tui_o::writer::input_facts::{Boundary, BoundaryKind, ChannelFact};

#[derive(Clone, Copy)]
pub(super) struct Limits {
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
    pub(super) fn tail_start(self, len: u64) -> Option<u64> {
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
pub(super) struct TailReader {
    pub(super) facts: InputFacts,
    meta: TailMeta,
    remaining: u64,
    read_bytes: u64,
    limits: Limits,
}
impl TailReader {
    pub(super) fn open(binding: SourceBinding, limits: Limits) -> Result<Self, String> {
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
            read_bytes: 0,
            limits,
        })
    }
    fn poll(&mut self, max: u64, end: Option<u64>) -> Result<ChannelFact, String> {
        let before = self.facts.observation_progress().1;
        let result = match end {
            Some(end) => self.facts.poll_observation_until(max, end, self.remaining),
            None => self.facts.poll(max),
        };
        self.read_bytes += self.facts.observation_progress().1.saturating_sub(before);
        self.meta.absorb(self.facts.boundaries());
        if result.is_err() && self.facts.resume_point().is_none() {
            self.meta.codex = CodexBoundary::Unproven;
        }
        result
    }
    pub(super) fn resume(&mut self) -> Result<(), String> {
        #[cfg(all(test, unix))]
        tests::before_resume(self.facts.binding().channel_id);
        let resume = self
            .facts
            .resume_point()
            .cloned()
            .ok_or("not a typed block")?;
        let next = self.facts.resume_observation_checked()?;
        self.meta.carry(&resume);
        self.facts = next;
        Ok(())
    }
    pub(super) fn bootstrap(&mut self) -> Result<(), String> {
        #[cfg(all(test, unix))]
        tests::before_bootstrap(self.facts.binding().channel_id);
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
    pub(super) fn diagnostics(&self) -> (u64, u64, u64, u64) {
        (
            self.meta.window_start,
            self.meta.bootstrap_end,
            self.remaining,
            self.read_bytes,
        )
    }
    pub(super) fn allows_idle(&self, fact: &ChannelFact, caught_up: bool) -> bool {
        self.meta
            .allows_idle(self.facts.binding().provider, fact, caught_up)
    }
    pub(super) fn poll_installed(&mut self) -> Result<ChannelFact, String> {
        self.poll(self.limits.poll, None)
    }
}
pub(super) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| crate::utils::env_flag("ADK_TURN_PRESENCE_TAIL", true))
}
pub(super) fn limits(channel: u64) -> Limits {
    #[cfg(test)]
    if let Some(limits) = overrides::limits_for(channel) {
        return limits;
    }
    let _ = channel;
    Limits::default()
}
#[cfg(all(test, unix))]
#[path = "tail_tests.rs"]
pub(super) mod tests;

#[cfg(test)]
pub(super) mod overrides {
    use super::*;
    static LIMITS: LazyLock<Mutex<HashMap<u64, Limits>>> = LazyLock::new(Default::default);
    pub(super) fn limits_for(channel: u64) -> Option<Limits> {
        lock(&LIMITS).get(&channel).copied()
    }
    pub(in crate::services::discord::turn_presence::activity) struct Guard(u64);
    impl Guard {
        pub(in crate::services::discord::turn_presence::activity) fn new(
            channel: u64,
            full: u64,
            tail: u64,
            poll: u64,
            worker: u64,
        ) -> Self {
            assert!(
                lock(&LIMITS)
                    .insert(
                        channel,
                        Limits {
                            full,
                            tail,
                            poll,
                            worker
                        }
                    )
                    .is_none()
            );
            Self(channel)
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            lock(&LIMITS).remove(&self.0);
        }
    }
    #[test]
    fn s1_limits_override_keeps_the_exact_full_boundary() {
        let _guard = Guard::new(69181999, 128, 64, 16, 32);
        let limits = super::limits(69181999);
        assert_eq!(limits.tail_start(128), None);
        assert_eq!(limits.tail_start(129), Some(65));
        assert_eq!(super::limits(0).full, MAX_TRANSCRIPT_BYTES);
    }
}

#[cfg(all(test, unix))]
#[path = "tail_switch_tests.rs"]
mod switch_tests;
