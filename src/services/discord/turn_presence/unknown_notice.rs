use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::time::Instant;

use super::notice_store::{Key, Receipt, Scope};
use super::{UnknownSnapshot, lock};
use crate::services::discord::turn_presence::activity::notice_identity::{
    ExecutionIdentity, NoticeObservation,
};

static OWNERS: LazyLock<Mutex<HashMap<Scope, Weak<Owner>>>> = LazyLock::new(Default::default);
const GRACE: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_secs(30);
const LOG_EVERY: Duration = Duration::from_secs(1800);

#[derive(Debug)]
pub(super) struct Owner {
    pub scope: Scope,
    state: Mutex<State>,
    io: tokio::sync::Mutex<()>,
}

#[derive(Default, Debug)]
struct State {
    key: Option<Key>,
    observed: Option<NoticeObservation>,
    settled: bool,
    agent_id: Option<String>,
    since: Option<Instant>,
    cause_at: Option<Instant>,
    unknown_since: Option<DateTime<Utc>>,
    cause_since: Option<DateTime<Utc>>,
    epoch: u64,
    receipt: Option<Receipt>,
    loaded: bool,
    invalidate_load: bool,
    dirty: bool,
    driving: bool,
    retry_at: Option<Instant>,
    log_at: Option<Instant>,
    store_error: Option<String>,
}

fn key(observed: &NoticeObservation) -> Key {
    Key {
        identity: observed.identity.clone(),
        target_label: observed.target_label.clone(),
        reason: if observed.progress {
            "catching_up"
        } else {
            observed.reason
        }
        .into(),
        leaf: observed.hold_leaf.map(str::to_string),
        provisional: observed.progress,
    }
}

fn target_continuity(old: &Key, new: &Key) -> Option<bool> {
    old.target_label
        .as_ref()
        .zip(new.target_label.as_ref())
        .map(|(a, b)| a == b)
}

fn changed_cause(old: &Key, new: &Key) -> bool {
    old.reason != new.reason || old.leaf != new.leaf
}

fn replaced(old: &Key, new: &Key) -> bool {
    target_continuity(old, new) == Some(false)
        || matches!((&old.identity, &new.identity), (ExecutionIdentity::Nonce(a), ExecutionIdentity::Nonce(b)) if a != b)
}

fn enrich(old: &mut Key, new: &Key) {
    let continuous = target_continuity(old, new) == Some(true);
    if (matches!(old.identity, ExecutionIdentity::Unresolved)
        || (continuous && matches!(old.identity, ExecutionIdentity::LegacySession(_))))
        && !matches!(new.identity, ExecutionIdentity::Unresolved)
    {
        old.identity = new.identity.clone();
    }
    if old.target_label.is_none() {
        old.target_label = new.target_label.clone();
    }
}

impl State {
    fn observe(
        &mut self,
        observed: Option<NoticeObservation>,
        agent: Option<String>,
        now: Instant,
    ) {
        self.agent_id = agent;
        self.observed = observed.clone();
        let Some(observed) = observed else {
            self.invalidate_load = true;
            self.settled = false;
            if self.key.take().is_some() {
                self.epoch += 1;
                self.receipt = None;
                self.retry_at = None;
                self.dirty = true;
            }
            self.since = None;
            self.unknown_since = None;
            self.cause_since = None;
            self.cause_at = None;
            self.log_at = None;
            return;
        };
        let new = key(&observed);
        self.settled |= !observed.progress;
        self.since.get_or_insert(now);
        self.unknown_since.get_or_insert_with(Utc::now);
        // Reader retries retain the settled cause and its grace; only actual evidence re-arms it.
        let changed = self.key.as_ref().is_some_and(|old| {
            replaced(old, &new)
                || (!observed.progress && !old.provisional && changed_cause(old, &new))
        });
        if self.key.is_none() || changed {
            self.invalidate_load |= changed;
            self.epoch += 1;
            self.key = Some(new);
            self.settled = !observed.progress;
            self.cause_at = Some(now);
            self.cause_since = Some(Utc::now());
            self.receipt = None;
            self.retry_at = None;
            self.dirty |= changed;
        } else if let Some(old) = self.key.as_mut() {
            let before = old.clone();
            enrich(old, &new);
            if !observed.progress && old.provisional {
                old.reason = new.reason;
                old.leaf = new.leaf;
                old.provisional = false;
            }
            if *old != before
                && let Some(receipt) = &mut self.receipt
            {
                receipt.key = old.clone();
                self.dirty = true;
            }
        }
    }

    fn notice_state(&self) -> &'static str {
        if self.store_error.is_some() {
            "store_error"
        } else if self.receipt.is_some() && !self.settled {
            "reconciling"
        } else if self.receipt.is_some() {
            "sent"
        } else if self.driving {
            "sending"
        } else {
            "armed"
        }
    }

    fn restore(&mut self, receipt: Option<Receipt>, now: Instant) {
        self.loaded = true;
        let Some(mut receipt) = receipt else {
            return;
        };
        let Some(current) = &self.key else {
            self.dirty = true;
            return;
        };
        let actual = self.observed.as_ref().is_some_and(|o| !o.progress);
        if self.invalidate_load
            || replaced(&receipt.key, current)
            || (actual && !receipt.key.provisional && changed_cause(&receipt.key, current))
        {
            self.dirty = true;
            return;
        }
        // A cold reader's initial rebuild is not evidence against a committed receipt.
        let mut restored = receipt.key.clone();
        enrich(&mut restored, current);
        if actual && restored.provisional {
            restored.reason = current.reason.clone();
            restored.leaf = current.leaf.clone();
            restored.provisional = false;
        }
        self.dirty |= restored != receipt.key;
        receipt.key = restored.clone();
        self.key = Some(restored);
        let elapsed = (Utc::now() - receipt.unknown_since)
            .to_std()
            .unwrap_or_default();
        self.since = Some(now.checked_sub(elapsed).unwrap_or(now));
        self.unknown_since = Some(receipt.unknown_since);
        self.cause_since = Some(receipt.cause_since);
        let cause_elapsed = (Utc::now() - receipt.cause_since)
            .to_std()
            .unwrap_or_default();
        self.cause_at = Some(now.checked_sub(cause_elapsed).unwrap_or(now));
        self.receipt = Some(receipt);
    }
}

impl Owner {
    pub fn get(scope: Scope) -> Arc<Self> {
        let mut owners = lock(&OWNERS);
        owners.retain(|_, owner| owner.strong_count() > 0);
        if let Some(owner) = owners.get(&scope).and_then(Weak::upgrade) {
            return owner;
        }
        let owner = Arc::new(Self {
            scope: scope.clone(),
            state: Mutex::new(State::default()),
            io: tokio::sync::Mutex::new(()),
        });
        owners.insert(scope, Arc::downgrade(&owner));
        owner
    }

    pub fn observe(
        &self,
        observed: Option<NoticeObservation>,
        agent: Option<String>,
        now: Instant,
    ) -> bool {
        let mut state = lock(&self.state);
        state.observe(observed, agent, now);
        let due = state
            .cause_at
            .is_some_and(|at| now.duration_since(at) >= GRACE)
            && state.receipt.is_none()
            && state.key.is_some();
        if state.driving
            || state.retry_at.is_some_and(|at| at > now)
            || (state.loaded && !state.dirty && !due)
        {
            return false;
        }
        state.driving = true;
        true
    }

    pub fn posted_at(&self) -> Option<DateTime<Utc>> {
        lock(&self.state).receipt.as_ref().map(|r| r.posted_at)
    }

    pub async fn drive<F, Fut>(&self, send: F)
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = Result<(), String>>,
    {
        let _io = self.io.lock().await;
        let result = self.drive_inner(send).await;
        let mut state = lock(&self.state);
        if let Err(ref error) = result {
            tracing::error!(channel_id = self.scope.channel, %error, "[turn_presence] notice store failed");
            state.store_error = Some(error.clone());
            state.retry_at = Some(Instant::now() + RETRY);
        }
        if result.is_ok() {
            state.store_error = None;
        }
        state.driving = false;
    }

    async fn drive_inner<F, Fut>(&self, send: F) -> Result<(), String>
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = Result<(), String>>,
    {
        {
            let mut state = lock(&self.state);
            if !state.loaded && state.invalidate_load {
                // Published recovery or replacement invalidates any old receipt, even corrupt bytes.
                state.loaded = true;
                state.dirty = true;
            }
        }
        if !lock(&self.state).loaded {
            let scope = self.scope.clone();
            let receipt = tokio::task::spawn_blocking(move || scope.load())
                .await
                .map_err(|e| e.to_string())??;
            lock(&self.state).restore(receipt, Instant::now());
        }
        self.flush().await?;
        let ticket = {
            let state = lock(&self.state);
            match state.key.as_ref().filter(|_| {
                state.receipt.is_none() && state.cause_at.is_some_and(|at| at.elapsed() >= GRACE)
            }) {
                Some(key) => Some((
                    state.epoch,
                    key.reason.clone(),
                    state
                        .unknown_since
                        .ok_or("notice unknown clock unavailable")?,
                    state.cause_since.ok_or("notice cause clock unavailable")?,
                )),
                None => None,
            }
        };
        if let Some((epoch, reason, unknown_since, cause_since)) = ticket {
            tracing::warn!(channel_id = self.scope.channel, provider = self.scope.provider, %reason, "[turn_presence] unknown_notice");
            let sent = send(reason).await;
            let mut state = lock(&self.state);
            if let Some(key) = state.key.clone().filter(|_| state.epoch == epoch) {
                match sent {
                    Ok(()) => {
                        // Keep this success in memory even if the durable commit must retry.
                        state.receipt = Some(Receipt {
                            schema: 1,
                            provider: self.scope.provider.clone(),
                            token_hash: self.scope.token_hash.clone(),
                            channel: self.scope.channel,
                            key,
                            unknown_since,
                            cause_since,
                            posted_at: Utc::now(),
                            id: uuid::Uuid::new_v4().to_string(),
                        });
                        state.dirty = true;
                    }
                    Err(error) => {
                        tracing::warn!(channel_id = self.scope.channel, %error, "[turn_presence] notice failed");
                        state.retry_at = Some(Instant::now() + RETRY);
                    }
                }
            }
        }
        self.flush().await
    }

    async fn flush(&self) -> Result<(), String> {
        loop {
            let (epoch, receipt) = {
                let state = lock(&self.state);
                if !state.dirty {
                    return Ok(());
                }
                (state.epoch, state.receipt.clone())
            };
            let scope = self.scope.clone();
            let saved = receipt.clone();
            tokio::task::spawn_blocking(move || scope.save(saved.as_ref()))
                .await
                .map_err(|e| e.to_string())??;
            let mut state = lock(&self.state);
            // A newer recovery or cause wins over a write that completed after it.
            if state.epoch == epoch && state.receipt == receipt {
                state.dirty = false;
                state.store_error = None;
                return Ok(());
            }
        }
    }

    pub fn heartbeat(&self, now: Instant) {
        let mut state = lock(&self.state);
        let Some(since) = state.since else {
            return;
        };
        if now.duration_since(since) < GRACE
            || state
                .log_at
                .is_some_and(|at| now.duration_since(at) < LOG_EVERY)
        {
            return;
        }
        state.log_at = Some(now);
        if let Some(key) = &state.key {
            tracing::warn!(channel_id = self.scope.channel, provider = self.scope.provider, reason = key.reason, hold_leaf = ?key.leaf, unknown_for_secs = now.duration_since(since).as_secs(), cause_for_secs = state.cause_at.map_or(0, |at| now.duration_since(at).as_secs()), observed_reason = state.observed.as_ref().map(|o| o.reason), observation_progress = state.observed.as_ref().is_some_and(|o| o.progress), notice_state = state.notice_state(), notice_retry_in_secs = state.retry_at.map(|at| at.saturating_duration_since(now).as_secs()), "[turn_presence] unknown_still_held");
        }
    }

    fn snapshot(&self, now: Instant) -> Option<UnknownSnapshot> {
        let state = lock(&self.state);
        let elapsed = now
            .checked_duration_since(state.since?)
            .unwrap_or_default()
            .as_secs();
        if elapsed < GRACE.as_secs() {
            return None;
        }
        let key = state.key.as_ref()?;
        let observed = state.observed.as_ref()?;
        Some(UnknownSnapshot {
            channel_id: self.scope.channel,
            provider: self.scope.provider.clone(),
            agent_id: state.agent_id.clone(),
            reason: key.reason.clone(),
            hold_leaf: key.leaf.clone(),
            unknown_since: state.unknown_since?,
            elapsed_secs: elapsed,
            observed_reason: observed.reason.into(),
            observation_progress: if observed.progress {
                "rebuilding"
            } else {
                "settled"
            }
            .into(),
            notice_state: state.notice_state().into(),
            notice_store_error: state.store_error.clone(),
        })
    }
}

pub(super) fn snapshots() -> Vec<UnknownSnapshot> {
    let owners: Vec<_> = lock(&OWNERS).values().filter_map(Weak::upgrade).collect();
    let now = Instant::now();
    owners
        .into_iter()
        .filter(|owner| crate::services::tui_o::turn_mode::transcript_turns(owner.scope.channel))
        .filter_map(|owner| owner.snapshot(now))
        .collect()
}

#[cfg(test)]
#[path = "unknown_notice_tests.rs"]
mod tests;

#[cfg(test)]
pub(super) use tests::pause_store_for_tests;
