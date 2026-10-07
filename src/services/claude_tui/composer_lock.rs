//! Per-pane Claude TUI input locks.
//!
//! A session-turn lock serializes full hosted turns, while the composer lock
//! protects only the short snapshot → key mutation → confirmation interval.
//! Keeping them distinct lets an auto `/compact` steer a busy pane without
//! waiting behind a normal turn's readiness phase.

use std::collections::HashMap;
#[cfg(unix)]
use std::sync::Arc;
use std::sync::{LazyLock, Mutex, MutexGuard};

#[cfg(unix)]
static SESSION_TURN_LOCKS: LazyLock<dashmap::DashMap<String, Arc<Mutex<()>>>> =
    LazyLock::new(dashmap::DashMap::new);

#[cfg(unix)]
static COMPOSER_MUTATION_LOCKS: LazyLock<dashmap::DashMap<String, Arc<Mutex<()>>>> =
    LazyLock::new(dashmap::DashMap::new);

/// Return the turn-lifetime lock for one hosted Claude pane.
#[cfg(unix)]
pub(crate) fn session_turn_lock(tmux_session_name: &str) -> Arc<Mutex<()>> {
    SESSION_TURN_LOCKS
        .entry(tmux_session_name.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Run one narrow composer mutation atomically with other mutations for the
/// same pane. Readiness waits deliberately stay outside this helper so a normal
/// follow-up cannot block a busy-turn `/compact` for its whole wait interval.
#[cfg(unix)]
pub(crate) fn with_composer_mutation_lock<R>(tmux_session_name: &str, f: impl FnOnce() -> R) -> R {
    let composer_lock = COMPOSER_MUTATION_LOCKS
        .entry(tmux_session_name.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let _composer_guard = composer_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    f()
}

#[cfg(not(unix))]
pub(crate) fn with_composer_mutation_lock<R>(_tmux_session_name: &str, f: impl FnOnce() -> R) -> R {
    f()
}

/// Try the existing composer fence; contention or poison leaves the callback unrun.
#[cfg(unix)]
#[allow(dead_code)]
pub(crate) fn try_with_composer_mutation_lock<R>(
    tmux_session_name: &str,
    operation: impl FnOnce() -> R,
) -> Option<R> {
    let composer_lock = COMPOSER_MUTATION_LOCKS
        .try_entry(tmux_session_name.to_string())?
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let _composer_guard = composer_lock.try_lock().ok()?;
    Some(operation())
}

#[cfg(not(unix))]
#[allow(dead_code)]
pub(crate) fn try_with_composer_mutation_lock<R>(
    _tmux_session_name: &str,
    _operation: impl FnOnce() -> R,
) -> Option<R> {
    None
}

/// Why a pane's composer is protected; in memory only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DraftGuard {
    /// Where the person's draft went is not known.
    RecoveryRequired,
    /// The person's draft is back in the composer, theirs to send or clear.
    DraftRestored,
}

/// What a capture of a protected pane shows: no stash with an empty composer, no stash with the
/// person's text, or anything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DraftSighting {
    Settled,
    PersonDraft,
    Unsettled,
}

/// What automatic writers may do to a pane now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComposerAdmission {
    Any,
    /// Only a verified stash transaction, which hands the person's draft back.
    StashOnly,
    Held,
}

static DRAFT_GUARDS: LazyLock<Mutex<HashMap<String, DraftGuard>>> = LazyLock::new(Default::default);

/// An automatic composer write refused because the pane protects a person's draft.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DraftRecoveryHold;

fn draft_guards() -> MutexGuard<'static, HashMap<String, DraftGuard>> {
    DRAFT_GUARDS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// Protects this pane's composer until a capture shows no stash and an empty composer.
pub(crate) fn guard_draft(tmux_session_name: &str, guard: DraftGuard) {
    draft_guards().insert(tmux_session_name.to_string(), guard);
}

/// Decides one automatic write under the composer lock, before any key. Only a protected pane is
/// captured: an empty composer without a stash releases it, the person's text allows a stash.
pub(crate) fn composer_admission(
    tmux_session_name: &str,
    capture: impl FnOnce() -> Option<String>,
) -> ComposerAdmission {
    if !draft_guards().contains_key(tmux_session_name) {
        return ComposerAdmission::Any;
    }
    let sighting = capture().map_or(DraftSighting::Unsettled, |capture| {
        super::busy_inject::draft_sighting(&capture)
    });
    match sighting {
        DraftSighting::Settled => {
            draft_guards().remove(tmux_session_name);
            ComposerAdmission::Any
        }
        DraftSighting::PersonDraft => {
            guard_draft(tmux_session_name, DraftGuard::DraftRestored);
            ComposerAdmission::StashOnly
        }
        DraftSighting::Unsettled => ComposerAdmission::Held,
    }
}

/// Admits a write that is not a stash transaction: submits, cleanups, clears and plain keys.
pub(crate) fn admit_composer_write(
    tmux_session_name: &str,
    capture: impl FnOnce() -> Option<String>,
) -> Result<(), DraftRecoveryHold> {
    match composer_admission(tmux_session_name, capture) {
        ComposerAdmission::Any => Ok(()),
        ComposerAdmission::StashOnly | ComposerAdmission::Held => Err(DraftRecoveryHold),
    }
}

/// Run a blocking hosted-turn operation under the pane's full turn lock.
#[cfg(unix)]
pub(crate) fn with_session_turn_lock<R>(tmux_session_name: &str, f: impl FnOnce() -> R) -> R {
    let turn_lock = session_turn_lock(tmux_session_name);
    let _turn_guard = turn_lock.lock().unwrap_or_else(|error| error.into_inner());
    f()
}

#[cfg(not(unix))]
pub(crate) fn with_session_turn_lock<R>(_tmux_session_name: &str, f: impl FnOnce() -> R) -> R {
    f()
}

#[cfg(all(test, unix))]
mod claude_tui_composer_lock_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, mpsc};
    use std::time::Duration;

    #[test]
    fn try_composer_lock_rejects_held_and_poisoned_lock() {
        let session = format!("composer-try-{}", uuid::Uuid::new_v4());
        let (sent, received) = mpsc::channel();
        let result = with_composer_mutation_lock(&session, || {
            let session = session.clone();
            std::thread::spawn(move || {
                let _ = sent.send(try_with_composer_mutation_lock(&session, || 7));
            });
            received.recv_timeout(Duration::from_secs(1))
        });
        assert_eq!(result, Ok(None));
        let _ = std::panic::catch_unwind(|| {
            with_composer_mutation_lock(&session, || panic!("poison composer"));
        });
        assert_eq!(try_with_composer_mutation_lock(&session, || 7), None);
        assert_eq!(with_composer_mutation_lock(&session, || 7), 7);
    }

    #[test]
    fn try_composer_lock_serializes_existing_writer() {
        let session = format!("composer-try-writer-{}", uuid::Uuid::new_v4());
        let (started_tx, started_rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let mut writer = None;
        let result = try_with_composer_mutation_lock(&session, || {
            let session = session.clone();
            writer = Some(std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                with_composer_mutation_lock(&session, || {
                    let _ = entered_tx.send(());
                });
            }));
            started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            entered_rx.recv_timeout(Duration::from_millis(100))
        });
        assert_eq!(result, Some(Err(mpsc::RecvTimeoutError::Timeout)));
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        writer.unwrap().join().unwrap();
        assert_eq!(try_with_composer_mutation_lock(&session, || 7), Some(7));
    }

    #[test]
    fn compact_composer_lock_proceeds_while_turn_lifetime_lock_is_held() {
        let session = format!("claude-4591-turn-lock-{}", uuid::Uuid::new_v4());
        let turn_lock = session_turn_lock(&session);
        let _turn_guard = turn_lock.lock().unwrap();
        let (sent, received) = mpsc::channel();
        let worker_session = session.clone();
        std::thread::spawn(move || {
            with_composer_mutation_lock(&worker_session, || {
                sent.send(()).unwrap();
            });
        });
        received
            .recv_timeout(Duration::from_millis(250))
            .expect("composer mutation must not wait on the turn-lifetime lock");
    }

    #[test]
    fn composer_mutation_lock_serializes_two_mutations() {
        let session = format!("claude-4591-composer-lock-{}", uuid::Uuid::new_v4());
        let (first_entered_tx, first_entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (second_entered_tx, second_entered_rx) = mpsc::channel();
        let first_session = session.clone();
        std::thread::spawn(move || {
            with_composer_mutation_lock(&first_session, || {
                first_entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
        });
        first_entered_rx
            .recv_timeout(Duration::from_millis(250))
            .unwrap();
        let second_session = session.clone();
        std::thread::spawn(move || {
            with_composer_mutation_lock(&second_session, || {
                second_entered_tx.send(()).unwrap();
            });
        });
        assert!(
            second_entered_rx
                .recv_timeout(Duration::from_millis(40))
                .is_err(),
            "a second composer mutation must not interleave before the first releases"
        );
        release_tx.send(()).unwrap();
        second_entered_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("second mutation should proceed after the first releases");
    }

    #[test]
    fn turn_locks_are_shared_per_session_only() {
        let session = format!("claude-tui-lock-{}", uuid::Uuid::new_v4());
        let first = session_turn_lock(&session);
        let second = session_turn_lock(&session);
        let other = session_turn_lock(&format!("claude-tui-lock-{}", uuid::Uuid::new_v4()));

        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &other));
    }

    #[test]
    fn turn_lock_serializes_concurrent_callbacks() {
        let session = "claude-tui-turn-lock-callback";
        let inside = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(2));

        let mut handles = Vec::new();
        for _ in 0..2 {
            let inside = Arc::clone(&inside);
            let max_concurrent = Arc::clone(&max_concurrent);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..50 {
                    with_session_turn_lock(session, || {
                        let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                        max_concurrent.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_micros(50));
                        inside.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);
    }
}
