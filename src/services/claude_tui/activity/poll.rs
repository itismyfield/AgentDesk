//! Re-read known sources even when their mtime is unchanged; events shorten the wait.
use super::{Source, ledger};
use chrono::Utc;
use std::sync::Condvar;
use std::time::Duration;

pub(super) fn tick() {
    visit_sources(|session| {
        let _snapshot = super::observe_at(session, Utc::now());
    });
}

pub(super) static CHANGED: Condvar = Condvar::new();
const READ_EVERY: Duration = Duration::from_secs(1);
fn visit_sources(mut read: impl FnMut(&str)) {
    let sources: Vec<Source> = ledger().entries.keys().cloned().collect();
    for source in sources {
        read(&source.session);
    }
}
#[cfg(not(test))]
pub(super) fn start() {
    static STARTED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);
    let mut started = STARTED.lock().unwrap_or_else(|e| e.into_inner());
    if *started {
        return;
    }
    let worker = std::thread::Builder::new()
        .name("claude-activity".into())
        .spawn(|| {
            loop {
                let waited = CHANGED
                    .wait_timeout(ledger(), READ_EVERY)
                    .unwrap_or_else(|e| e.into_inner());
                drop(waited);
                tick();
            }
        });
    *started = worker.is_ok();
}
#[cfg(test)]
pub(super) fn start() {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scheduled_read_is_bounded_and_replays_sources_without_mtime_filter() {
        assert_eq!(READ_EVERY, Duration::from_secs(1));
        let source = Source {
            session: "poll-callback".into(),
            channel: 0,
            owner: "owner".into(),
            host: None,
            nonce: "nonce".into(),
            native: "native".into(),
            path: std::path::PathBuf::new(),
            file: (0, 0),
            visit: Utc::now(),
        };
        ledger().entries.insert(source.clone(), Default::default());
        let mut visits = 0;
        for _ in 0..2 {
            visit_sources(|session| {
                let guard = ledger();
                if session == source.session {
                    assert!(guard.entries.contains_key(&source));
                    visits += 1;
                }
            });
        }
        assert_eq!(
            visits, 2,
            "both scheduled reads replay the unchanged source"
        );
        ledger().entries.remove(&source);
    }
}
