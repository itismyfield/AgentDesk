//! `SessionRegistry` — per-process source of truth for the set of currently
//! matched `(tmux session → channel/agent/provider)` bindings.
//!
//! Epic #2285 / E2 (issue #2344). Builds on the pure
//! [`super::session_matcher::MatchedChannel`] type (E1) and feeds into the
//! upcoming `WatcherSupervisor` (E3) and `StreamRelay` (E4) layers.
//!
//! ## Design constraints
//!
//! - **Single source of truth**: there is exactly one process-wide registry
//!   instance, exposed via [`global_session_registry`]. Multiple readers
//!   (HTTP diagnostic endpoint, future supervisor) and exactly one writer
//!   (`SessionDiscovery`, leader-only).
//! - **Lock-free reads**: the registry stores its state behind a `RwLock`
//!   so the supervisor / API readers do not block discovery.
//! - **Change notifications**: every registry mutation publishes a
//!   [`RegistryChange`] event on a `broadcast` channel. The `WatcherSupervisor`
//!   (E3) subscribes to this stream so it can spawn / shut down watchers in
//!   reaction to discovery results without polling. Lagging subscribers
//!   recover via [`SessionRegistry::list_matched`] (idempotent reconcile).
//!
//! No tmux / Discord / DB access lives here — discovery does all of that and
//! pushes results into the registry. This keeps the registry trivial to test.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};

use chrono::{DateTime, Utc};
use tokio::sync::broadcast;

use super::session_matcher::MatchedChannel;

/// Default capacity for the change-notification broadcast channel. Generous
/// enough that the `WatcherSupervisor` (E3) won't normally lag behind a single
/// discovery sweep, but bounded so a stuck consumer cannot exhaust memory —
/// `tokio::broadcast` drops the oldest events with a `Lagged` error, which the
/// consumer treats as "do a full reconcile via `list_matched`".
const CHANGE_CHANNEL_CAPACITY: usize = 256;

/// A single registry entry. Wraps the pure [`MatchedChannel`] with bookkeeping
/// metadata (when it first appeared, when it was last reconfirmed) so the
/// supervisor / API can reason about freshness without re-probing tmux.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredSession {
    pub matched: MatchedChannel,
    /// First time this `(session_name)` entered the registry. Stable across
    /// re-matches as long as the session keeps being matched.
    pub first_seen_at: DateTime<Utc>,
    /// Most recent time discovery confirmed this binding was still live.
    pub last_seen_at: DateTime<Utc>,
}

impl RegisteredSession {
    /// JSON projection for the `/api/cluster/sessions` diagnostic endpoint.
    /// `MatchedChannel` and `ProviderKind` are not `serde::Serialize`-derived
    /// upstream (provider is an enum with stringly-typed variants), so we hand-
    /// roll the projection here rather than coupling the matcher module to
    /// serde.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "matched": {
                "channel_id": self.matched.channel_id,
                "agent_id": self.matched.agent_id,
                "provider": self.matched.provider.as_str(),
                "expected_session_name": self.matched.expected_session_name,
                "expected_rollout_path": self.matched.expected_rollout_path,
            },
            "first_seen_at": self.first_seen_at,
            "last_seen_at": self.last_seen_at,
        })
    }
}

/// Mutation events published on the registry's broadcast channel. The E3
/// supervisor will use these to know when to spawn or stop a watcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryChange {
    /// A previously-unknown matched session appeared.
    Added(RegisteredSession),
    /// A known matched session was rebound (e.g. channel binding changed).
    /// Includes the new state; the supervisor should treat this as a watcher
    /// re-target rather than a fresh spawn.
    Updated(RegisteredSession),
    /// A previously-matched session is no longer present. The supervisor must
    /// gracefully shut down any watcher attached to it.
    Removed { session_name: String },
}

impl RegistryChange {
    pub fn session_name(&self) -> &str {
        match self {
            Self::Added(entry) | Self::Updated(entry) => &entry.matched.expected_session_name,
            Self::Removed { session_name } => session_name,
        }
    }
}

/// Thread-safe, in-process registry of matched sessions.
pub struct SessionRegistry {
    inner: RwLock<BTreeMap<String, RegisteredSession>>,
    changes: broadcast::Sender<RegistryChange>,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionRegistry {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        Self {
            inner: RwLock::new(BTreeMap::new()),
            changes: tx,
        }
    }

    /// Subscribe to mutation events. Subscribers that fall behind by more than
    /// [`CHANGE_CHANNEL_CAPACITY`] events receive a `Lagged` error and must
    /// reconcile via [`Self::list_matched`].
    pub fn subscribe(&self) -> broadcast::Receiver<RegistryChange> {
        self.changes.subscribe()
    }

    /// Insert or update a matched session. Returns the change that was
    /// published (or `None` if the entry was identical and no event was sent —
    /// idempotent reconciles do not spam the broadcast channel).
    pub fn upsert(&self, matched: MatchedChannel) -> Option<RegistryChange> {
        self.upsert_at(matched, Utc::now())
    }

    /// Test-friendly variant that lets the caller pin the timestamp.
    pub fn upsert_at(&self, matched: MatchedChannel, now: DateTime<Utc>) -> Option<RegistryChange> {
        let session_name = matched.expected_session_name.clone();
        let mut guard = self.inner.write().expect("session registry write lock");
        let change = match guard.get(&session_name) {
            Some(existing) if existing.matched == matched => {
                // Idempotent re-confirmation — only refresh `last_seen_at` so
                // staleness pruning has up-to-date data; no broadcast.
                let mut refreshed = existing.clone();
                refreshed.last_seen_at = now;
                guard.insert(session_name, refreshed);
                return None;
            }
            Some(existing) => {
                let updated = RegisteredSession {
                    matched: matched.clone(),
                    first_seen_at: existing.first_seen_at,
                    last_seen_at: now,
                };
                guard.insert(session_name.clone(), updated.clone());
                RegistryChange::Updated(updated)
            }
            None => {
                let added = RegisteredSession {
                    matched: matched.clone(),
                    first_seen_at: now,
                    last_seen_at: now,
                };
                guard.insert(session_name.clone(), added.clone());
                RegistryChange::Added(added)
            }
        };
        drop(guard);
        // Broadcast errors (no subscribers) are not fatal — the next subscriber
        // will reconcile via `list_matched`.
        let _ = self.changes.send(change.clone());
        Some(change)
    }

    /// Remove a session by name. Returns `Some(change)` iff the entry existed.
    pub fn remove(&self, session_name: &str) -> Option<RegistryChange> {
        let mut guard = self.inner.write().expect("session registry write lock");
        if guard.remove(session_name).is_some() {
            drop(guard);
            let change = RegistryChange::Removed {
                session_name: session_name.to_string(),
            };
            let _ = self.changes.send(change.clone());
            Some(change)
        } else {
            None
        }
    }

    /// Snapshot of all matched sessions, sorted by session name (BTreeMap key).
    /// Cheap clone — used by the supervisor for full reconcile and by the
    /// diagnostic endpoint.
    pub fn list_matched(&self) -> Vec<RegisteredSession> {
        self.inner
            .read()
            .expect("session registry read lock")
            .values()
            .cloned()
            .collect()
    }

    /// Single-entry lookup by tmux session name. Lock-cheap read.
    pub fn lookup(&self, session_name: &str) -> Option<RegisteredSession> {
        self.inner
            .read()
            .expect("session registry read lock")
            .get(session_name)
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.read().expect("session registry read lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner
            .read()
            .expect("session registry read lock")
            .is_empty()
    }

    /// Replace the registry contents with the exact set of `current_matches`
    /// — anything not in the set is removed, anything new is added. Used by
    /// `SessionDiscovery` at the end of each poll cycle so the registry stays
    /// in sync with the live tmux world without callers having to compute the
    /// diff manually.
    ///
    /// Returns the ordered list of changes that were broadcast (handy for
    /// tests and for tracing).
    pub fn reconcile(&self, current_matches: Vec<MatchedChannel>) -> Vec<RegistryChange> {
        self.reconcile_at(current_matches, Utc::now())
    }

    pub fn reconcile_at(
        &self,
        current_matches: Vec<MatchedChannel>,
        now: DateTime<Utc>,
    ) -> Vec<RegistryChange> {
        // Collect names we expect to be present after reconcile.
        let mut keep: BTreeMap<String, MatchedChannel> = BTreeMap::new();
        for matched in current_matches {
            keep.insert(matched.expected_session_name.clone(), matched);
        }

        let existing_names: Vec<String> = self
            .inner
            .read()
            .expect("session registry read lock")
            .keys()
            .cloned()
            .collect();

        let mut emitted = Vec::new();
        for name in existing_names {
            if !keep.contains_key(&name) {
                if let Some(change) = self.remove(&name) {
                    emitted.push(change);
                }
            }
        }
        for (_name, matched) in keep {
            if let Some(change) = self.upsert_at(matched, now) {
                emitted.push(change);
            }
        }
        emitted
    }
}

impl std::fmt::Debug for SessionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRegistry")
            .field("len", &self.len())
            .field("subscribers", &self.changes.receiver_count())
            .finish()
    }
}

static GLOBAL_REGISTRY: OnceLock<Arc<SessionRegistry>> = OnceLock::new();

/// Returns the process-wide [`SessionRegistry`]. Lazily initialised on first
/// access so test binaries that never touch cluster discovery don't pay any
/// cost.
pub fn global_session_registry() -> Arc<SessionRegistry> {
    GLOBAL_REGISTRY
        .get_or_init(|| Arc::new(SessionRegistry::new()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider::ProviderKind;

    fn matched(channel: &str, agent: &str, provider: ProviderKind) -> MatchedChannel {
        let session = provider.build_tmux_session_name(channel);
        MatchedChannel {
            channel_id: channel.to_string(),
            agent_id: agent.to_string(),
            provider: provider.clone(),
            expected_session_name: session.clone(),
            expected_rollout_path: super::super::session_matcher::expected_rollout_path_for(
                &session,
            ),
        }
    }

    #[test]
    fn upsert_emits_added_then_idempotent() {
        let registry = SessionRegistry::new();
        let mut rx = registry.subscribe();
        let m = matched("c1", "agent-a", ProviderKind::Claude);

        let change = registry.upsert(m.clone()).expect("first upsert publishes");
        assert!(matches!(change, RegistryChange::Added(_)));
        assert_eq!(rx.try_recv().ok(), Some(change.clone()));

        // Idempotent re-upsert with same value: no broadcast.
        assert!(registry.upsert(m.clone()).is_none());
        assert!(rx.try_recv().is_err());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn upsert_emits_updated_when_binding_changes() {
        let registry = SessionRegistry::new();
        let mut rx = registry.subscribe();
        let session = ProviderKind::Claude.build_tmux_session_name("c1");

        // Same expected_session_name (we keep that stable) but different
        // agent_id triggers Updated.
        let m1 = MatchedChannel {
            channel_id: "c1".to_string(),
            agent_id: "agent-a".to_string(),
            provider: ProviderKind::Claude,
            expected_session_name: session.clone(),
            expected_rollout_path: super::super::session_matcher::expected_rollout_path_for(
                &session,
            ),
        };
        let m2 = MatchedChannel {
            agent_id: "agent-b".to_string(),
            ..m1.clone()
        };
        let _ = registry.upsert(m1);
        rx.try_recv().expect("first add");
        let change = registry.upsert(m2.clone()).expect("update publishes");
        match change {
            RegistryChange::Updated(entry) => assert_eq!(entry.matched.agent_id, "agent-b"),
            other => panic!("expected Updated, got {other:?}"),
        }
    }

    #[test]
    fn remove_emits_removed_only_when_present() {
        let registry = SessionRegistry::new();
        let mut rx = registry.subscribe();
        let m = matched("c1", "td", ProviderKind::Codex);
        let _ = registry.upsert(m.clone());
        rx.try_recv().expect("added");

        let removed = registry.remove(&m.expected_session_name);
        assert!(matches!(removed, Some(RegistryChange::Removed { .. })));
        rx.try_recv().expect("removed broadcast");

        // Removing again is a no-op.
        assert!(registry.remove(&m.expected_session_name).is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn list_matched_and_lookup() {
        let registry = SessionRegistry::new();
        let a = matched("ca", "agent-a", ProviderKind::Claude);
        let b = matched("cb", "agent-b", ProviderKind::Codex);
        registry.upsert(a.clone());
        registry.upsert(b.clone());

        let listed = registry.list_matched();
        assert_eq!(listed.len(), 2);
        // BTreeMap → sorted by session name.
        assert!(listed[0].matched.expected_session_name < listed[1].matched.expected_session_name);

        assert_eq!(
            registry
                .lookup(&a.expected_session_name)
                .unwrap()
                .matched
                .agent_id,
            "agent-a"
        );
        assert!(registry.lookup("nope").is_none());
    }

    #[test]
    fn reconcile_removes_missing_and_adds_new() {
        let registry = SessionRegistry::new();
        let mut rx = registry.subscribe();
        let a = matched("ca", "agent-a", ProviderKind::Claude);
        let b = matched("cb", "agent-b", ProviderKind::Codex);
        registry.upsert(a.clone());
        registry.upsert(b.clone());
        // Drain the two Added events.
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        // New world: b is gone, c is new, a unchanged.
        let c = matched("cc", "agent-c", ProviderKind::Claude);
        let changes = registry.reconcile(vec![a.clone(), c.clone()]);
        assert_eq!(changes.len(), 2);
        // Removal happens before addition in the reconcile order; this keeps
        // supervisor logic predictable (free a port before spawning the
        // replacement watcher).
        assert!(matches!(changes[0], RegistryChange::Removed { .. }));
        assert!(matches!(changes[1], RegistryChange::Added(_)));

        let names: Vec<String> = registry
            .list_matched()
            .into_iter()
            .map(|s| s.matched.expected_session_name)
            .collect();
        assert!(names.contains(&a.expected_session_name));
        assert!(names.contains(&c.expected_session_name));
        assert!(!names.contains(&b.expected_session_name));
    }

    #[test]
    fn last_seen_refreshes_on_idempotent_upsert() {
        let registry = SessionRegistry::new();
        let m = matched("c1", "td", ProviderKind::Codex);
        let t0 = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let t1 = DateTime::<Utc>::from_timestamp(1_700_000_060, 0).unwrap();

        registry.upsert_at(m.clone(), t0);
        let snap0 = registry.lookup(&m.expected_session_name).unwrap();
        assert_eq!(snap0.first_seen_at, t0);
        assert_eq!(snap0.last_seen_at, t0);

        registry.upsert_at(m.clone(), t1);
        let snap1 = registry.lookup(&m.expected_session_name).unwrap();
        assert_eq!(snap1.first_seen_at, t0, "first_seen is stable");
        assert_eq!(snap1.last_seen_at, t1, "last_seen refreshes");
    }

    #[test]
    fn global_registry_is_singleton() {
        let a = global_session_registry();
        let b = global_session_registry();
        assert!(Arc::ptr_eq(&a, &b));
    }
}
