use super::SessionLiveness;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum Event {
    Observed {
        settling: bool,
        liveness: SessionLiveness,
    },
    RuntimeReady,
    Ended(&'static str),
}

static EVENTS: Mutex<Vec<(String, Event)>> = Mutex::new(Vec::new());

std::thread_local! {
    static SETTLING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn push(name: &str, event: Event) {
    let mut events = EVENTS.lock().unwrap_or_else(|poison| poison.into_inner());
    events.push((name.to_string(), event));
}

pub(in crate::services::discord) fn observed(name: &str, liveness: SessionLiveness) {
    let settling = SETTLING.with(std::cell::Cell::get);
    push(name, Event::Observed { settling, liveness });
}

pub(in crate::services::discord) fn runtime_ready(name: &str) {
    push(name, Event::RuntimeReady);
}

pub(in crate::services::discord) fn end_of(
    read: &Result<super::RestoredRead, String>,
) -> &'static str {
    match read {
        Ok(super::RestoredRead::HandOff(_)) => "handoff",
        Ok(super::RestoredRead::Died) => "died",
        Ok(super::RestoredRead::Ended) => "ended",
        Err(_) => "error",
    }
}

/// Recorded once the reader thread has sent everything it will send.
pub(in crate::services::discord) fn ended(name: &str, end: &'static str) {
    push(name, Event::Ended(end));
}

pub(in crate::services::discord) fn events(name: &str) -> Vec<Event> {
    let events = EVENTS.lock().unwrap_or_else(|poison| poison.into_inner());
    let mine = events.iter().filter(|(owner, _)| owner == name);
    mine.map(|(_, event)| event.clone()).collect()
}

/// Marks this thread's observations as settling a held result until dropped.
pub(in crate::services::discord) struct Settling;

impl Settling {
    pub(in crate::services::discord) fn enter() -> Self {
        SETTLING.with(|settling| settling.set(true));
        Self
    }
}

impl Drop for Settling {
    fn drop(&mut self) {
        SETTLING.with(|settling| settling.set(false));
    }
}
