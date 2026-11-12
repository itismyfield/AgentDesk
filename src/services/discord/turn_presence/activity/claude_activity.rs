//! Presence retains the collector token until its final publication check.
use super::*;
use crate::services::claude_tui::activity::{self, Foreground, Snapshot};

pub(super) struct Busy {
    active: bool,
    token: Option<Snapshot>,
}
impl Busy {
    pub(super) fn is_busy(&self) -> bool {
        self.active && current(&self.token)
    }
}
#[cfg(test)]
impl From<bool> for Busy {
    fn from(active: bool) -> Self {
        Self {
            active,
            token: None,
        }
    }
}
pub(super) fn busy(session: &str) -> Busy {
    let snapshot = activity::observe(session);
    let active = matches!(
        snapshot.foreground,
        Foreground::Submitted | Foreground::Generating | Foreground::Tool
    );
    Busy {
        active,
        token: Some(snapshot),
    }
}
pub(super) fn current(token: &Option<Snapshot>) -> bool {
    token.as_ref().is_none_or(Snapshot::current)
}
pub(super) fn supplement(
    watch: &Mutex<Watch>,
    guard: MutexGuard<'_, Watch>,
    key: &Key,
    strict: bool,
    ask: impl FnOnce() -> Busy,
) -> Answer {
    let mut token = None;
    let mut answer = ask_pane(watch, guard, key, strict, || {
        let busy = ask();
        if busy.is_busy() {
            token = busy.token;
            observed(Activity::Busy, "open_activity_busy")
        } else {
            observed(Activity::Unknown, "open_without_progress")
        }
    });
    if answer.0.activity == Activity::Busy {
        if !current(&token) {
            return at(
                &lock(watch),
                observed(Activity::Unknown, "activity_superseded"),
            );
        }
        if let Some(stamp) = &mut answer.1 {
            stamp.activity = token;
        }
    }
    answer
}
pub(super) fn ready(session: &str) -> bool {
    crate::services::claude_tui::readiness::observe(session, None).is_some()
}
