//! The reader's own active-status writes to its session row (30 s heartbeat, post-loop
//! `turn_active`) stop for good once the inflight row it owned is gone or held by another turn.

use crate::services::discord::inflight::GuardedSaveOutcome;

const HEARTBEAT_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone, Copy, Debug)]
pub(in crate::services::discord::turn_bridge) struct SessionRowWriter {
    last_heartbeat: std::time::Instant,
    owned: bool,
    released: bool,
}

impl SessionRowWriter {
    pub(in crate::services::discord::turn_bridge) fn new() -> Self {
        Self {
            last_heartbeat: std::time::Instant::now(),
            owned: false,
            released: false,
        }
    }

    /// A saved row proves ownership; after that a vanished row or a successor's row releases it.
    /// A pinned or unnameable row may still be this live turn's, so it releases nothing.
    pub(in crate::services::discord::turn_bridge) fn observe(
        &mut self,
        outcome: GuardedSaveOutcome,
    ) {
        match outcome {
            GuardedSaveOutcome::Saved => self.owned = true,
            GuardedSaveOutcome::RowAbsent => self.released |= self.owned,
            GuardedSaveOutcome::SuccessorOwned => self.released = true,
            GuardedSaveOutcome::AuthorityPinned
            | GuardedSaveOutcome::Unnameable
            | GuardedSaveOutcome::IoError => {}
        }
    }

    pub(in crate::services::discord::turn_bridge) fn released(&self) -> bool {
        self.released
    }

    pub(in crate::services::discord::turn_bridge) fn heartbeat_due(&self) -> bool {
        !self.released && self.last_heartbeat.elapsed() >= HEARTBEAT_EVERY
    }

    pub(in crate::services::discord::turn_bridge) fn heartbeat_posted(&mut self) {
        self.last_heartbeat = std::time::Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use GuardedSaveOutcome::*;

    /// Ownership is released only after a saved row then vanishes or passes to a successor.
    #[test]
    fn only_a_lost_owned_row_stops_the_heartbeat() {
        let past = std::time::Instant::now() - HEARTBEAT_EVERY;
        for (seen, released) in [
            (&[Saved, RowAbsent][..], true),
            (&[Saved, SuccessorOwned, Saved], true),
            (&[RowAbsent, IoError, AuthorityPinned, Unnameable], false),
            (&[Saved, IoError, AuthorityPinned, Unnameable, Saved], false),
        ] {
            let mut writer = SessionRowWriter {
                last_heartbeat: past,
                owned: false,
                released: false,
            };
            seen.iter().for_each(|outcome| writer.observe(*outcome));
            assert_eq!(
                (writer.released(), writer.heartbeat_due()),
                (released, !released),
                "{seen:?}"
            );
        }
    }
}
