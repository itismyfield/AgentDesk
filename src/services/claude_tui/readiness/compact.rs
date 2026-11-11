//! A generation-bound compact waits for current terminal evidence and never replays a mutation.
use super::{evidence, source::Source};
use crate::services::claude_tui::{busy_inject::screen, composer_lock, host_input, input};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Pending,
    Submitted,
    Ambiguous,
    Obsolete,
}

pub(crate) struct Pending {
    source: Source,
    file: Option<(u64, u64)>,
    start: Option<chrono::DateTime<chrono::Utc>>,
    finished: bool,
}
impl Pending {
    pub(crate) fn capture(session: &str) -> Option<Self> {
        let source = Source::current(session, None)?;
        let file = evidence::Stamp::read(&source.path).map(|stamp| stamp.file);
        let start = source.nonce.as_ref().and_then(|nonce| {
            super::ledger()
                .starts
                .get(&(source.session.clone(), nonce.clone()))
                .map(|(at, _)| *at)
        });
        Some(Self {
            source,
            file,
            start,
            finished: false,
        })
    }

    /// The caller holds the composer lock and has rechecked generation, occupancy and authority.
    pub(crate) fn poll(&mut self, _permit: composer_lock::ComposerPermit<'_>) -> Outcome {
        let session = &self.source.session;
        if self.finished {
            return Outcome::Obsolete;
        }
        if super::source::nonce(session).is_some_and(|nonce| nonce != self.source.nonce)
            || Source::current(session, None).is_some_and(|source| source != self.source)
        {
            return Outcome::Obsolete;
        }
        let start = self.source.nonce.as_ref().and_then(|nonce| {
            super::ledger()
                .starts
                .get(&(session.clone(), nonce.clone()))
                .map(|(at, _)| *at)
        });
        if start != self.start {
            return Outcome::Obsolete;
        }
        let stamp = evidence::Stamp::read(&self.source.path);
        if self.file.is_some()
            && stamp
                .as_ref()
                .is_some_and(|stamp| Some(stamp.file) != self.file)
        {
            return Outcome::Obsolete;
        }
        if self.file.is_none() {
            self.file = stamp.map(|stamp| stamp.file);
        }
        let proof = super::observe(session, Some(&self.source.path)).filter(|proof| {
            evidence::read(&self.source.path).is_some_and(|tail| {
                Some(tail.stamp) == proof.stamp && tail.terminal > 0 && !tail.empty
            })
        });
        let capture = host_input::observe_draft(session);
        let empty = capture.as_deref().is_some_and(|capture| {
            !super::idle::focus_modal(capture)
                && screen::input_box(capture) == screen::Composer::Empty
        });
        if !empty
            || composer_lock::composer_admission(session, || capture.clone())
                != composer_lock::ComposerAdmission::Any
        {
            return Outcome::Pending;
        }
        let reservation = proof.and_then(super::reserve);
        if reservation
            .as_ref()
            .is_none_or(|reservation| !reservation.current())
        {
            if let Some(reservation) = reservation {
                reservation.not_sent();
            }
            return Outcome::Pending;
        }
        match host_input::run_legacy(
            session,
            &[input::TuiInputAction::Literal("/compact".into())],
            None,
        ) {
            host_input::InputRun::Applied => {}
            host_input::InputRun::Refused(_) => {
                if let Some(reservation) = reservation {
                    reservation.not_sent();
                }
                return Outcome::Pending;
            }
            _ => {
                self.finished = true;
                return Outcome::Ambiguous;
            }
        }
        self.finished = true;
        std::thread::sleep(Duration::from_millis(120));
        let own_control = host_input::observe_draft(session).is_some_and(|capture| {
            !super::idle::focus_modal(&capture)
                && screen::input_box(&capture) == screen::Composer::Text(vec!["/compact".into()])
        });
        if !own_control
            || reservation
                .as_ref()
                .is_none_or(|reservation| !reservation.current())
        {
            return Outcome::Ambiguous;
        }
        if host_input::run_legacy(session, &[input::TuiInputAction::Enter], None)
            != host_input::InputRun::Applied
        {
            return Outcome::Ambiguous;
        }
        if let Some(reservation) = reservation {
            reservation.sent();
        }
        Outcome::Submitted
    }
}

static WAKE: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);
/// Wakes after the later hook ledger guard has dropped, without reservation self-wakes.
pub(super) struct HookWake;
impl Drop for HookWake {
    fn drop(&mut self) {
        WAKE.notify_waiters();
    }
}

pub(crate) fn spawn(
    session: &str,
    poll: impl Fn(&mut Pending) -> Option<Outcome> + Send + Sync + 'static,
) -> bool {
    let Some(mut pending) = Pending::capture(session) else {
        return false;
    };
    let poll = Arc::new(poll);
    tokio::spawn(async move {
        loop {
            let notified = WAKE.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let callback = Arc::clone(&poll);
            let Ok((returned, outcome)) = tokio::task::spawn_blocking(move || {
                let outcome = callback(&mut pending);
                (pending, outcome)
            })
            .await
            else {
                return;
            };
            pending = returned;
            if outcome != Some(Outcome::Pending) {
                return;
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            }
        }
    });
    true
}
