use super::SharedData;
use poise::serenity_prelude::ChannelId;
use std::sync::Arc;

pub(crate) const SESSION_TRANSITION_LOCK_WAIT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionTransitionBusy;

/// The exact channel lock held by a claim; a borrowed guard cannot release its caller's lock.
pub(crate) struct SessionTransitionLease<'a> {
    shared: &'a SharedData,
    channel: ChannelId,
    hold: TransitionHold<'a>,
}

enum TransitionHold<'a> {
    Owned(tokio::sync::OwnedMutexGuard<()>),
    Borrowed(&'a tokio::sync::OwnedMutexGuard<()>),
}

impl<'a> SessionTransitionLease<'a> {
    pub(crate) fn try_acquire(
        shared: &'a SharedData,
        channel: ChannelId,
    ) -> Result<Self, SessionTransitionBusy> {
        let guard = shared
            .session_transition_lock(channel)
            .try_lock_owned()
            .map_err(|_| SessionTransitionBusy)?;
        Ok(Self {
            shared,
            channel,
            hold: TransitionHold::Owned(guard),
        })
    }

    pub(crate) fn borrow(
        shared: &'a SharedData,
        channel: ChannelId,
        guard: &'a tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<Self, SessionTransitionBusy> {
        if !Arc::ptr_eq(
            tokio::sync::OwnedMutexGuard::mutex(guard),
            &shared.session_transition_lock(channel),
        ) {
            return Err(SessionTransitionBusy);
        }
        Ok(Self {
            shared,
            channel,
            hold: TransitionHold::Borrowed(guard),
        })
    }

    pub(crate) fn matches(&self, shared: &SharedData, channel: ChannelId) -> bool {
        let guard = match &self.hold {
            TransitionHold::Owned(guard) => guard,
            TransitionHold::Borrowed(guard) => guard,
        };
        std::ptr::eq(self.shared, shared)
            && self.channel == channel
            && Arc::ptr_eq(
                tokio::sync::OwnedMutexGuard::mutex(guard),
                &shared.session_transition_lock(channel),
            )
    }
}

/// The channel's transition lock held for one unplaced-launch resolution, by the bot whose rows it
/// may resolve.
pub(crate) struct UnplacedLease {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    token_hash: String,
    channel: ChannelId,
}

impl UnplacedLease {
    /// Waits as long as any transition claim does.
    pub(crate) async fn acquire(
        shared: &SharedData,
        channel: ChannelId,
    ) -> Result<Self, SessionTransitionBusy> {
        let guard = shared.acquire_session_transition(channel).await?;
        Ok(Self::held(shared, channel, guard))
    }

    pub(crate) fn try_acquire(
        shared: &SharedData,
        channel: ChannelId,
    ) -> Result<Self, SessionTransitionBusy> {
        let guard = shared
            .session_transition_lock(channel)
            .try_lock_owned()
            .map_err(|_| SessionTransitionBusy)?;
        Ok(Self::held(shared, channel, guard))
    }

    fn held(
        shared: &SharedData,
        channel: ChannelId,
        guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> Self {
        Self {
            _guard: guard,
            token_hash: shared.token_hash.clone(),
            channel,
        }
    }

    /// Whether the row `owner` names is this bot's row of this channel.
    pub(crate) fn covers(
        &self,
        owner: &crate::db::dispatched_sessions::hosted_execution::HostedOwner,
    ) -> bool {
        owner.discord_token_hash == self.token_hash && owner.channel_id == self.channel.to_string()
    }
}

impl SharedData {
    pub(crate) fn session_transition_lock(
        &self,
        channel_id: ChannelId,
    ) -> Arc<tokio::sync::Mutex<()>> {
        if let Some(lock) = self
            .session_transition_locks
            .get(&channel_id)
            .and_then(|lock| lock.upgrade())
        {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        match self.session_transition_locks.entry(channel_id) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                if let Some(existing) = entry.get().upgrade() {
                    existing
                } else {
                    entry.insert(Arc::downgrade(&lock));
                    lock
                }
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(Arc::downgrade(&lock));
                // Prune only on registry growth. The common existing-channel path
                // above remains O(1), while dead Weak entries are still bounded by
                // subsequent vacant insertions.
                self.session_transition_locks
                    .retain(|_, candidate| candidate.strong_count() > 0);
                lock
            }
        }
    }

    pub(crate) async fn acquire_session_transition(
        &self,
        channel_id: ChannelId,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, SessionTransitionBusy> {
        tokio::time::timeout(
            SESSION_TRANSITION_LOCK_WAIT_TIMEOUT,
            self.session_transition_lock(channel_id).lock_owned(),
        )
        .await
        .map_err(|_| SessionTransitionBusy)
    }
}
