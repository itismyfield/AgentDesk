use poise::serenity_prelude::ChannelId;
use std::sync::Arc;

pub const SESSION_TRANSITION_LOCK_WAIT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTransitionBusy;

/// The runtime whose channel transitions a lease stands for. A lease is told apart by the address
/// of its host, so only the runtime object itself implements this, never a pointer to one.
pub trait SessionTransitionHost {
    fn transition_locks(&self) -> &SessionTransitionLocks;
    fn transition_token_hash(&self) -> &str;
}

/// The exact channel lock held by a claim; a borrowed guard cannot release its caller's lock.
pub struct SessionTransitionLease<'a, S> {
    shared: &'a S,
    channel: ChannelId,
    hold: TransitionHold<'a>,
}

enum TransitionHold<'a> {
    Owned(tokio::sync::OwnedMutexGuard<()>),
    Borrowed(&'a tokio::sync::OwnedMutexGuard<()>),
}

impl<'a, S: SessionTransitionHost> SessionTransitionLease<'a, S> {
    pub fn try_acquire(shared: &'a S, channel: ChannelId) -> Result<Self, SessionTransitionBusy> {
        let guard = shared
            .transition_locks()
            .lock(channel)
            .try_lock_owned()
            .map_err(|_| SessionTransitionBusy)?;
        Ok(Self {
            shared,
            channel,
            hold: TransitionHold::Owned(guard),
        })
    }

    pub fn borrow(
        shared: &'a S,
        channel: ChannelId,
        guard: &'a tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<Self, SessionTransitionBusy> {
        if !Arc::ptr_eq(
            tokio::sync::OwnedMutexGuard::mutex(guard),
            &shared.transition_locks().lock(channel),
        ) {
            return Err(SessionTransitionBusy);
        }
        Ok(Self {
            shared,
            channel,
            hold: TransitionHold::Borrowed(guard),
        })
    }

    pub fn matches(&self, shared: &S, channel: ChannelId) -> bool {
        let guard = match &self.hold {
            TransitionHold::Owned(guard) => guard,
            TransitionHold::Borrowed(guard) => guard,
        };
        std::ptr::eq(self.shared, shared)
            && self.channel == channel
            && Arc::ptr_eq(
                tokio::sync::OwnedMutexGuard::mutex(guard),
                &shared.transition_locks().lock(channel),
            )
    }
}

/// The channel's transition lock held for one unplaced-launch resolution, by the bot whose rows it
/// may resolve.
pub struct UnplacedLease {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    token_hash: String,
    channel: ChannelId,
}

impl UnplacedLease {
    /// Waits as long as any transition claim does.
    pub async fn acquire<S: SessionTransitionHost>(
        shared: &S,
        channel: ChannelId,
    ) -> Result<Self, SessionTransitionBusy> {
        let guard = shared.transition_locks().acquire(channel).await?;
        Ok(Self::held(shared, channel, guard))
    }

    pub fn try_acquire<S: SessionTransitionHost>(
        shared: &S,
        channel: ChannelId,
    ) -> Result<Self, SessionTransitionBusy> {
        let guard = shared
            .transition_locks()
            .lock(channel)
            .try_lock_owned()
            .map_err(|_| SessionTransitionBusy)?;
        Ok(Self::held(shared, channel, guard))
    }

    fn held<S: SessionTransitionHost>(
        shared: &S,
        channel: ChannelId,
        guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> Self {
        Self {
            _guard: guard,
            token_hash: shared.transition_token_hash().to_owned(),
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

/// One runtime's channel transition locks. An entry is a `Weak`: the lock lives while a guard or a
/// handle holds it, and dead entries are pruned when a new channel is inserted.
#[derive(Default)]
pub struct SessionTransitionLocks(
    dashmap::DashMap<ChannelId, std::sync::Weak<tokio::sync::Mutex<()>>>,
);

impl SessionTransitionLocks {
    pub fn lock(&self, channel_id: ChannelId) -> Arc<tokio::sync::Mutex<()>> {
        if let Some(lock) = self.0.get(&channel_id).and_then(|lock| lock.upgrade()) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        match self.0.entry(channel_id) {
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
                self.0.retain(|_, candidate| candidate.strong_count() > 0);
                lock
            }
        }
    }

    pub async fn acquire(
        &self,
        channel_id: ChannelId,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, SessionTransitionBusy> {
        tokio::time::timeout(
            SESSION_TRANSITION_LOCK_WAIT_TIMEOUT,
            self.lock(channel_id).lock_owned(),
        )
        .await
        .map_err(|_| SessionTransitionBusy)
    }

    /// Whether the registry still carries an entry for `channel_id`, live or dead.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn has_entry(&self, channel_id: ChannelId) -> bool {
        self.0.contains_key(&channel_id)
    }
}

#[cfg(test)]
mod host_tests;
#[cfg(test)]
mod registry_tests;
