//! Each running actor's delivery position after its latest poll, read by health's stall verdict.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

/// What a channel's actor still had to deliver after its latest poll.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChannelProgress {
    /// Derived pieces not delivered yet, a pending local retry and a `Prepared` without a result.
    pub debt: usize,
    /// A unit was announced whose sealing record is still to come.
    pub unsealed: bool,
    /// The ledger's next serial; it grows with every piece the actor prepares.
    pub serial: u64,
}

type Board = Mutex<HashMap<u64, (u64, ChannelProgress)>>;

#[cfg(not(test))]
static BOARD: std::sync::LazyLock<Board> = std::sync::LazyLock::new(Board::default);
// Tests keep one board per thread so fixtures of parallel tests never meet.
#[cfg(test)]
thread_local! {
    static BOARD: Board = Board::default();
}

static NEXT_ACTOR: AtomicU64 = AtomicU64::new(1);

fn with_board<R>(use_board: impl FnOnce(&mut HashMap<u64, (u64, ChannelProgress)>) -> R) -> R {
    let locked =
        |board: &Board| use_board(&mut board.lock().unwrap_or_else(PoisonError::into_inner));
    #[cfg(not(test))]
    return locked(&BOARD);
    #[cfg(test)]
    BOARD.with(locked)
}

/// One actor's entry; dropping it removes the entry unless a later actor already replaced it.
pub struct Publisher {
    channel: u64,
    actor: u64,
}

impl Publisher {
    pub fn new(channel: u64) -> Self {
        let actor = NEXT_ACTOR.fetch_add(1, Ordering::Relaxed);
        Self { channel, actor }
    }

    pub fn publish(&self, progress: ChannelProgress) {
        with_board(|board| board.insert(self.channel, (self.actor, progress)));
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        with_board(|board| {
            if board
                .get(&self.channel)
                .is_some_and(|(actor, _)| *actor == self.actor)
            {
                board.remove(&self.channel);
            }
        });
    }
}

/// The position the running actor of `channel` last published; `None` when no actor runs.
pub(crate) fn read(channel: u64) -> Option<ChannelProgress> {
    with_board(|board| board.get(&channel).map(|(_, progress)| *progress))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ended_actor_leaves_no_position_and_never_removes_its_successor() {
        let first = Publisher::new(7_333_001);
        first.publish(ChannelProgress {
            debt: 1,
            unsealed: false,
            serial: 4,
        });
        let second = Publisher::new(7_333_001);
        second.publish(ChannelProgress {
            debt: 0,
            unsealed: true,
            serial: 5,
        });
        drop(first);
        assert_eq!(read(7_333_001).map(|p| p.serial), Some(5));
        drop(second);
        assert_eq!(read(7_333_001), None);
    }
}
