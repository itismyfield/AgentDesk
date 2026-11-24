use super::{SessionTransitionHost, SessionTransitionLease, SessionTransitionLocks, UnplacedLease};
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use poise::serenity_prelude::ChannelId;
use std::sync::Arc;

/// A host over a registry it may share with another host, as no production runtime does.
struct FixtureHost {
    locks: Arc<SessionTransitionLocks>,
    token: &'static str,
}

impl FixtureHost {
    fn own(token: &'static str) -> Self {
        Self {
            locks: Arc::default(),
            token,
        }
    }

    fn sharing_the_registry_of(maker: &Self) -> Self {
        Self {
            locks: Arc::clone(&maker.locks),
            token: maker.token,
        }
    }
}

impl SessionTransitionHost for FixtureHost {
    fn transition_locks(&self) -> &SessionTransitionLocks {
        &self.locks
    }

    fn transition_token_hash(&self) -> &str {
        self.token
    }
}

fn owner(token: &str, channel: u64) -> HostedOwner {
    HostedOwner {
        provider: "claude".into(),
        discord_token_hash: token.into(),
        channel_id: channel.to_string(),
        logical_key: "fixture".into(),
        owner_node: "node".into(),
        runtime_root: "/fixture".into(),
    }
}

#[test]
fn another_host_with_the_same_token_and_channel_does_not_fit_a_lease() {
    let ours = FixtureHost::own("token");
    let theirs = FixtureHost::own("token");
    let channel = ChannelId::new(7_353_101);
    let lease = SessionTransitionLease::try_acquire(&ours, channel).unwrap();
    assert!(lease.matches(&ours, channel));
    assert!(!lease.matches(&theirs, channel));
    assert!(!lease.matches(&ours, ChannelId::new(7_353_102)));
    drop(lease);

    let guard = ours
        .transition_locks()
        .lock(channel)
        .try_lock_owned()
        .unwrap();
    assert!(SessionTransitionLease::borrow(&theirs, channel, &guard).is_err());
    assert!(SessionTransitionLease::borrow(&ours, channel, &guard).is_ok());
}

#[test]
fn a_lease_fits_only_its_maker_even_over_a_shared_registry() {
    let maker = FixtureHost::own("token");
    let alias = FixtureHost::sharing_the_registry_of(&maker);
    let channel = ChannelId::new(7_353_111);
    let lease = SessionTransitionLease::try_acquire(&maker, channel).unwrap();
    assert!(lease.matches(&maker, channel));
    assert!(
        !lease.matches(&alias, channel),
        "the same registry and lock must not stand in for the host that made the lease"
    );
}

#[test]
fn two_handles_on_one_host_name_the_same_host() {
    let first = Arc::new(FixtureHost::own("token"));
    let second = Arc::clone(&first);
    let channel = ChannelId::new(7_353_121);
    let lease = SessionTransitionLease::try_acquire(first.as_ref(), channel).unwrap();
    assert!(lease.matches(second.as_ref(), channel));
}

#[test]
fn a_new_host_starts_free_while_an_old_host_still_holds_its_lease() {
    let old = FixtureHost::own("token");
    let new = FixtureHost::own("token");
    let channel = ChannelId::new(7_353_131);
    let old_lease = SessionTransitionLease::try_acquire(&old, channel).unwrap();
    assert!(!old_lease.matches(&new, channel));
    let new_lease = SessionTransitionLease::try_acquire(&new, channel).unwrap();
    assert!(new_lease.matches(&new, channel));
    assert!(!new_lease.matches(&old, channel));
}

#[test]
fn an_unplaced_lease_covers_its_makers_token_and_channel() {
    let host = FixtureHost::own("token-a");
    let channel = ChannelId::new(7_353_141);
    let lease = UnplacedLease::try_acquire(&host, channel).unwrap();
    assert!(lease.covers(&owner("token-a", 7_353_141)));
    assert!(!lease.covers(&owner("token-b", 7_353_141)));
    assert!(!lease.covers(&owner("token-a", 7_353_142)));
    assert!(
        UnplacedLease::try_acquire(&host, channel).is_err(),
        "the lease holds its maker's channel lock"
    );

    // `covers` reads the token and the channel only, so a host carrying the same token is covered
    // although its own lock stays free.
    let twin = FixtureHost::own("token-a");
    assert!(UnplacedLease::try_acquire(&twin, channel).is_ok());
}
