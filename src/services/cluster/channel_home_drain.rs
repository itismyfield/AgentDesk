//! The holder's drain of a delegated channel. New intake stops first, owed pieces still go out at
//! the same epoch, and the leaving write runs only after the final close and a fresh check.
#![cfg_attr(not(test), allow(dead_code))]

use std::future::Future;
use std::sync::Arc;

use sqlx::PgPool;
use tokio::time::Instant;

use super::channel_home::{self, HomeGate, HomeOwnership, RENEW_EVERY};
use crate::db::o_channel_activation::activation_rows;
use crate::db::o_channel_homes::{self, ChannelHome, HomeError, HomeState, HomeWrite};

/// What the channel's O actor still owes, read from its projection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Owed {
    pub(crate) owed: usize,
    pub(crate) prepared: usize,
    pub(crate) unsealed: usize,
    pub(crate) uncaptured: usize,
    pub(crate) binding_pending: usize,
}

impl Owed {
    fn is_clear(&self) -> bool {
        *self == Self::default()
    }
}

/// Reads of the channel's turn, delivery and source state. They read projections only; a read
/// that fails is `None` and the drain waits on it.
pub(crate) trait DrainPort {
    fn turn_running(&self) -> impl Future<Output = Option<bool>> + Send;
    fn owed(&self) -> impl Future<Output = Option<Owed>> + Send;
    /// POSTs admitted before the final close that have not ended.
    fn posts_in_flight(&self) -> impl Future<Output = Option<usize>> + Send;
    /// The gateway side's existing session reset; the worker side keeps its pane and row.
    fn reset_legacy_source(&self) -> impl Future<Output = Result<(), ResetRefused>> + Send;
}

/// Why the gateway side's session was not reset; the drain waits on it and never leaves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResetRefused {
    /// No reset path reaches the drain yet.
    NotWired,
    /// The existing reset path refused to touch the session, for the reason given.
    Refused(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Blocker {
    RowUnreadable,
    /// This node's gate does not hold the draining epoch yet; its lease reopens it.
    NotHeld,
    TurnRunning,
    TurnUnreadable,
    Owed(Owed),
    OwedUnreadable,
    OpenIntake(i64),
    IntakeUnreadable,
    SourceReset,
    CommandsInFlight(usize),
    PostsInFlight(usize),
    PostsUnreadable,
    LeaveFailed,
}

impl Blocker {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::RowUnreadable => "row_unreadable",
            Self::NotHeld => "not_held",
            Self::TurnRunning => "turn_running",
            Self::TurnUnreadable => "turn_unreadable",
            Self::Owed(_) => "owed",
            Self::OwedUnreadable => "owed_unreadable",
            Self::OpenIntake(_) => "open_intake",
            Self::IntakeUnreadable => "intake_unreadable",
            Self::SourceReset => "source_reset",
            Self::CommandsInFlight(_) => "commands_in_flight",
            Self::PostsInFlight(_) => "posts_in_flight",
            Self::PostsUnreadable => "posts_unreadable",
            Self::LeaveFailed => "leave_failed",
        }
    }
}

#[derive(Debug)]
pub(crate) enum DrainStep {
    /// Still draining in the same state; nothing left the holder.
    Waiting(Blocker),
    /// The leaving write landed: `released` or `reclaimed`.
    Left(ChannelHome),
    /// The row does not name this node as a draining holder.
    NotDraining,
}

/// Owed pieces and open intake, both read fresh.
async fn undelivered<P: DrainPort>(pool: &PgPool, home: &HomeGate, port: &P) -> Option<Blocker> {
    match port.owed().await {
        Some(owed) if owed.is_clear() => {}
        Some(owed) => return Some(Blocker::Owed(owed)),
        None => return Some(Blocker::OwedUnreadable),
    }
    match activation_rows(pool, home.channel_id(), home.holder()).await {
        Ok(rows) if rows.open_intake == 0 => None,
        Ok(rows) => Some(Blocker::OpenIntake(rows.open_intake)),
        Err(_) => Some(Blocker::IntakeUnreadable),
    }
}

/// Reopens the closed gate at the same epoch through a renewal write, so pieces owed after the
/// final close can still go out.
async fn resume(pool: &PgPool, home: &HomeGate, epoch: i64) {
    let sent = Instant::now();
    let renewal = o_channel_homes::renew(pool, home.channel_id(), home.holder(), epoch).await;
    if let Ok(HomeWrite::Applied(written)) = renewal {
        let _ = home.resume_drain(&written, sent);
    }
}

fn commands_blocker(home: &HomeGate) -> Option<Blocker> {
    #[cfg(test)]
    if channel_home::command_mutant("drain_ignores_commands") {
        return None;
    }
    let count = home.commands_in_flight();
    (count != 0).then_some(Blocker::CommandsInFlight(count))
}

/// One pass of the drain for the row as read now; the caller repeats it until it ends.
pub(crate) async fn drain_round<P: DrainPort>(
    pool: &PgPool,
    home: &HomeGate,
    port: &P,
) -> DrainStep {
    let row = match o_channel_homes::read_home(pool, home.channel_id()).await {
        Ok(row) => row,
        Err(_) => return DrainStep::Waiting(Blocker::RowUnreadable),
    };
    let draining = |row: &ChannelHome| {
        row.holder.as_deref() == Some(home.holder())
            && matches!(row.state, HomeState::Releasing | HomeState::Reclaiming)
    };
    let Some(row) = row.filter(draining) else {
        return DrainStep::NotDraining;
    };
    if home.withdrawn() {
        return DrainStep::NotDraining;
    }
    let epoch = row.epoch;
    if !home.final_closed(epoch) {
        match home.ownership() {
            HomeOwnership::Owned { home_epoch, .. } if home_epoch == epoch => home.close_intake(),
            _ => return DrainStep::Waiting(Blocker::NotHeld),
        }
        if let Some(blocker) = commands_blocker(home) {
            return DrainStep::Waiting(blocker);
        }
        match port.turn_running().await {
            Some(false) => {}
            Some(true) => return DrainStep::Waiting(Blocker::TurnRunning),
            None => return DrainStep::Waiting(Blocker::TurnUnreadable),
        }
        if let Some(blocker) = undelivered(pool, home, port).await {
            return DrainStep::Waiting(blocker);
        }
        if row.state == HomeState::Releasing && port.reset_legacy_source().await.is_err() {
            return DrainStep::Waiting(Blocker::SourceReset);
        }
        // Takes the admission lock, so an admission in progress finishes first and none follows.
        home.close();
    }
    #[cfg(test)]
    let skip_recheck = channel_home::command_mutant("recheck_after_close_removed")
        || channel_home::command_mutant("recheck_only_in_first_close");
    #[cfg(not(test))]
    let skip_recheck = false;
    if !skip_recheck && let Some(blocker) = commands_blocker(home) {
        return DrainStep::Waiting(blocker);
    }
    match port.posts_in_flight().await {
        Some(0) => {}
        Some(count) => return DrainStep::Waiting(Blocker::PostsInFlight(count)),
        None => return DrainStep::Waiting(Blocker::PostsUnreadable),
    }
    if let Some(blocker) = undelivered(pool, home, port).await {
        resume(pool, home, epoch).await;
        return DrainStep::Waiting(blocker);
    }
    let (channel, holder) = (home.channel_id(), home.holder());
    let left = match row.state {
        HomeState::Releasing => o_channel_homes::finish_release(pool, channel, holder, epoch).await,
        _ => o_channel_homes::finish_reclaim(pool, channel, holder, epoch).await,
    };
    match left {
        Ok(HomeWrite::Applied(row)) => DrainStep::Left(row),
        Ok(HomeWrite::Stale) => DrainStep::NotDraining,
        Err(_) => DrainStep::Waiting(Blocker::LeaveFailed),
    }
}

/// Drains until the leaving write lands or the row stops naming this node as a draining
/// holder; each wait is shown in health as `home_draining`.
pub(crate) async fn run_drain<P: DrainPort>(
    pool: PgPool,
    home: Arc<HomeGate>,
    port: P,
) -> Option<ChannelHome> {
    loop {
        match drain_round(&pool, &home, &port).await {
            DrainStep::Waiting(blocker) => home.note_drain(Some(blocker.as_str())),
            DrainStep::Left(row) => {
                home.note_drain(None);
                return Some(row);
            }
            DrainStep::NotDraining => {
                home.note_drain(None);
                return None;
            }
        }
        tokio::time::sleep(RENEW_EVERY).await;
    }
}

/// The gateway's last step of a reclaim: it drops the reclaimed row, and once no row is left the
/// channel leaves the registry and follows the gateway rules again, unless `home` was replaced.
pub(crate) async fn finish_return(
    pool: &PgPool,
    home: &HomeGate,
    epoch: i64,
) -> Result<bool, HomeError> {
    let (channel, gateway) = (home.channel_id(), home.holder());
    o_channel_homes::remove_reclaimed(pool, channel, gateway, epoch).await?;
    if o_channel_homes::read_home(pool, channel).await?.is_some() {
        return Ok(false);
    }
    Ok(channel_home::unregister_if_same(home))
}

#[cfg(test)]
#[path = "channel_home_drain_tests.rs"]
mod tests;
