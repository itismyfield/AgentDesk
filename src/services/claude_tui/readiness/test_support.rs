//! Readiness ledger fixtures and the reservation hook upper-crate tests drive (KRX-TS). The unit tests in
//! `tests` (readiness_tests.rs, clear_tests.rs) re-export them, so their own paths stay as they were.

use super::*;
use crate::services::tui_prompt_dedupe;
use std::fs;
use std::time::Duration;

thread_local! {
    pub(crate) static SOURCES: std::cell::RefCell<Vec<tempfile::TempDir>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Existing submission fixtures exercise keys and custody over a completed source.
pub(crate) fn idle_fixture(session: &str) {
    if Source::current(session, None).is_some_and(|source| source.path.exists()) {
        return;
    }
    ledger()
        .entries
        .retain(|source, _| source.session != session);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixture.jsonl");
    fs::write(
        &path,
        "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
    )
    .unwrap();
    ledger().entries.insert(
        Source {
            session: session.into(),
            nonce: source::nonce(session).flatten(),
            native: "fixture".into(),
            path,
        },
        Entry::default(),
    );
    SOURCES.with(|sources| sources.borrow_mut().push(dir));
}

/// Runs `act` once the wait on `session` made its first look. That look creates the ledger entry
/// after the wait kept its Stamp, so `act` lands inside the wait without a hook in the wait.
pub(crate) fn after_first_look(
    session: &str,
    act: impl FnOnce() + Send + 'static,
) -> std::thread::JoinHandle<()> {
    assert_eq!(retained_sources(session), 0, "no look precedes the wait");
    let session = session.to_owned();
    std::thread::spawn(move || {
        let until = std::time::Instant::now() + Duration::from_secs(30);
        while retained_sources(&session) == 0 {
            assert!(std::time::Instant::now() < until, "the wait never looked");
            std::thread::sleep(Duration::from_millis(1));
        }
        act();
    })
}

/// A daemon restart: the ledger forgets `session`, while its files, markers and launch remain.
pub(crate) fn daemon_restart(session: &str) {
    let mut state = ledger();
    state.entries.retain(|source, _| source.session != session);
    state.starts.retain(|(started, _), _| started != session);
}

#[cfg(unix)]
pub(crate) fn fresh_fixture(session: &str) {
    if let Some(path) = super::super::guarded_outcome::fresh_source_for_tests() {
        pre_source(session, Path::new(&path));
    }
}

pub(crate) fn pre_source(session: &str, path: &Path) {
    let source = Source::current(session, Some(path)).unwrap_or_else(|| Source {
        session: session.into(),
        nonce: source::nonce(session).flatten(),
        native: path.file_stem().unwrap().to_string_lossy().into_owned(),
        path: path.into(),
    });
    let mut state = ledger();
    state
        .entries
        .retain(|old, _| old.session != session || old == &source);
    state.entries.entry(source).or_insert_with(|| Entry {
        start: true,
        ..Entry::default()
    });
}

// A process restart forgets volatile readiness without deleting durable hook evidence.
pub(crate) fn forget_session(session: &str) {
    let mut state = ledger();
    state.entries.retain(|source, _| source.session != session);
    state.starts.retain(|(logical, _), _| logical != session);
}

// Reproduce the interval after durable certification and before volatile Start publication.
pub(crate) fn restore_bound_start(session: &str, at: DateTime<Utc>) {
    let binding = tui_prompt_dedupe::runtime_binding_for_tmux_session(session).unwrap();
    let nonce = source::nonce(session).flatten().unwrap();
    let bound = Source {
        session: session.into(),
        nonce: Some(nonce.clone()),
        native: binding.session_id.unwrap(),
        path: binding.output_path.into(),
    };
    let mut state = ledger();
    state.entries.entry(bound.clone()).or_default();
    state.starts.insert((session.into(), nonce), (at, bound));
}

pub(crate) fn retained_sources(session: &str) -> usize {
    let nonce = source::nonce(session).flatten();
    ledger()
        .entries
        .keys()
        .filter(|source| source.session == session && source.nonce == nonce)
        .count()
}

#[cfg(unix)]
use std::cell::RefCell;

// The callback the next ledger reservation runs before it returns (`reserve`, readiness/mod.rs).
#[cfg(unix)]
thread_local! {
    pub(crate) static AFTER_RESERVE: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}
#[cfg(unix)]
pub(crate) fn after_reserve() {
    if let Some(callback) = AFTER_RESERVE.with(|slot| slot.take()) {
        callback();
    }
}

#[cfg(unix)]
pub(crate) fn after_next_reserve(callback: impl FnOnce() + 'static) {
    AFTER_RESERVE.with(|slot| *slot.borrow_mut() = Some(Box::new(callback)));
}

#[cfg(test)]
pub(crate) fn revisions_for_session(session: &str) -> Vec<u64> {
    let mut revisions: Vec<_> = super::ledger()
        .entries
        .iter()
        .filter(|(source, _)| source.session == session)
        .map(|(_, entry)| entry.revision)
        .collect();
    revisions.sort_unstable();
    revisions
}
