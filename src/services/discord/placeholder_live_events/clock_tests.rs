use super::*;
use std::cell::Cell;
use std::time::Duration;

thread_local! {
    static OFFSET: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}

pub(super) fn offset() -> Duration {
    OFFSET.get()
}

pub(super) struct Guard(Duration);
impl Guard {
    pub(super) fn new() -> Self {
        let previous = OFFSET.replace(Duration::ZERO);
        Self(previous)
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        OFFSET.set(self.0);
    }
}

pub(super) fn aged_stamp(state: &mut StatusPanelState, age: Duration) -> Instant {
    let recorded = panel_now();
    for slot in &mut state.tasks {
        slot.created_at += age;
    }
    for slot in &mut state.subagents {
        slot.started_at += age;
    }
    state
        .recently_evicted_subagent_keys
        .forward_date_for_tests(age);
    OFFSET.set(OFFSET.get() + age);
    recorded
}
