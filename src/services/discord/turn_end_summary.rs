//! Live Legacy terminal chrome, sharing the O summary format without claiming its body.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::gateway::{DiscordGateway, TurnGateway};
use super::inflight::InflightTurnState;
use super::turn_finalizer::{LedgerKey, TurnKey};
use super::{ChannelId, SharedData, serenity};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::provider::ProviderKind;
use crate::services::turn_summary_line::{Outcome, render};

const TTL: Duration = Duration::from_secs(60 * 60);
const CAP: usize = 1024;
static SENT: LazyLock<Mutex<Latch>> = LazyLock::new(|| Mutex::new(Latch::default()));

#[derive(Default)]
struct Latch(HashMap<LedgerKey, Attempt>);

struct Attempt {
    at: Instant,
    finished: bool,
}

impl Latch {
    fn take(&mut self, key: TurnKey, now: Instant) -> bool {
        // Pending sends are never evicted: an interrupted future may already have posted.
        self.0.retain(|_, attempt| {
            !attempt.finished || now.saturating_duration_since(attempt.at) < TTL
        });
        let key = key.exact_key();
        if self.0.contains_key(&key) {
            return false;
        }
        if self.0.len() >= CAP {
            let oldest = self
                .0
                .iter()
                .filter(|(_, attempt)| attempt.finished)
                .min_by_key(|(_, attempt)| attempt.at)
                .map(|(key, _)| *key);
            let Some(oldest) = oldest else { return false };
            self.0.remove(&oldest);
        }
        self.0.insert(
            key,
            Attempt {
                at: now,
                finished: false,
            },
        );
        true
    }

    fn finish(&mut self, key: TurnKey, now: Instant) {
        if let Some(attempt) = self.0.get_mut(&key.exact_key()) {
            attempt.at = now;
            attempt.finished = true;
        }
    }
}

#[derive(Clone, Copy)]
pub(in crate::services::discord) struct LiveSummary {
    key: TurnKey,
    runtime_kind: Option<RuntimeHandoffKind>,
    outcome: Outcome,
    elapsed_ms: Option<u64>,
    context_pct: Option<u8>,
}

pub(in crate::services::discord) fn outcome(cancelled: bool, failed: bool) -> Outcome {
    if cancelled {
        Outcome::Cancelled
    } else if failed {
        Outcome::Failed
    } else {
        Outcome::Completed
    }
}

impl LiveSummary {
    pub(in crate::services::discord) fn bridge(
        channel: ChannelId,
        state: &InflightTurnState,
        generation: u64,
        outcome: Outcome,
        elapsed_ms: Option<u64>,
    ) -> Self {
        Self {
            key: TurnKey::new(channel, state.effective_finalizer_turn_id(), generation)
                .with_episode_nonce(state.turn_nonce.as_deref()),
            runtime_kind: state.runtime_kind,
            outcome,
            elapsed_ms,
            context_pct: None,
        }
    }

    pub(in crate::services::discord) fn with_context(mut self, shared: &SharedData) -> Self {
        self.context_pct = shared
            .ui
            .placeholder_live_events
            .context_panel_snapshot(self.key.channel_id)
            .filter(|snapshot| snapshot.context_window_tokens != 0)
            .map(|snapshot| {
                ((u128::from(snapshot.used_tokens) * 100)
                    / u128::from(snapshot.context_window_tokens))
                .min(100) as u8
            });
        self
    }

    pub(in crate::services::discord) fn watcher(
        shared: &SharedData,
        key: Option<TurnKey>,
        state: Option<&InflightTurnState>,
        cancelled: bool,
        outcome: Outcome,
    ) -> Option<Self> {
        let (key, state) = (key?, state?);
        if key.user_msg_id == 0
            || key.channel_id.get() != state.channel_id
            || key.user_msg_id != state.effective_finalizer_turn_id()
            || !key.matches_episode_nonce(state.turn_nonce.as_deref())
        {
            return None;
        }
        let elapsed_ms =
            chrono::NaiveDateTime::parse_from_str(&state.started_at, "%Y-%m-%d %H:%M:%S")
                .ok()
                .and_then(|started| elapsed_since(started, chrono::Local::now().naive_local()));
        Some(
            Self {
                key,
                runtime_kind: state.runtime_kind,
                outcome: if cancelled {
                    Outcome::Cancelled
                } else {
                    outcome
                },
                elapsed_ms,
                context_pct: None,
            }
            .with_context(shared),
        )
    }
}

fn elapsed_since(started: chrono::NaiveDateTime, ended: chrono::NaiveDateTime) -> Option<u64> {
    let elapsed = ended - started;
    if elapsed < chrono::Duration::zero() {
        return None;
    }
    u64::try_from(elapsed.num_milliseconds()).ok()
}

/// Facts observed only as the watcher consumes this turn, before a boundary is reinserted.
#[derive(Clone, Copy, Default)]
pub(in crate::services::discord) struct WatcherOutcome {
    failed: bool,
    aborted: bool,
}

impl WatcherOutcome {
    pub(in crate::services::discord) fn observe(&mut self, value: &serde_json::Value) {
        match value["type"].as_str() {
            Some("assistant") => self.failed = value["isApiErrorMessage"] == true,
            Some("result") => self.failed |= value["is_error"] == true,
            Some("user") => {
                let content = &value["message"]["content"];
                let interrupted = |text: &str| text.starts_with("[Request interrupted by user");
                self.aborted |= content.as_str().is_some_and(interrupted)
                    || content.as_array().is_some_and(|parts| {
                        parts
                            .iter()
                            .any(|part| part["text"].as_str().is_some_and(interrupted))
                    });
            }
            _ => {}
        }
    }

    pub(in crate::services::discord) fn native(
        &mut self,
        message: &crate::services::agent_protocol::StreamMessage,
    ) {
        use crate::services::agent_protocol::{NativeTerminalKind, StreamMessage};
        match message {
            StreamMessage::Error { .. } => self.failed = true,
            StreamMessage::CodexTuiTerminalDone {
                kind: NativeTerminalKind::Aborted,
                ..
            } => self.aborted = true,
            _ => {}
        }
    }

    pub(in crate::services::discord) fn outcome(self) -> Outcome {
        outcome(self.aborted, self.failed)
    }
}

pub(in crate::services::discord) async fn post_live<G: TurnGateway + ?Sized>(
    gateway: &G,
    provider: &ProviderKind,
    summary: LiveSummary,
) {
    if crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(
        summary.key.channel_id.get(),
        summary.runtime_kind,
    )
    .unwrap_or(true)
        || !SENT
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take(summary.key, Instant::now())
    {
        return;
    }
    // An error may follow a successful POST, so the attempt remains consumed even on cancellation.
    let text = render(summary.outcome, summary.elapsed_ms, summary.context_pct);
    if let Err(error) = gateway.send_message(summary.key.channel_id, &text).await {
        tracing::warn!(channel = summary.key.channel_id.get(), provider = provider.as_str(), %error,
            "Legacy terminal summary was not confirmed; it will not be replayed");
    }
    SENT.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .finish(summary.key, Instant::now());
}

/// A summary may outlive the panel await, so retain its actor witness until the send boundary.
pub(in crate::services::discord) struct WatcherSummary {
    summary: LiveSummary,
    actor: Option<std::sync::Weak<crate::services::provider::CancelToken>>,
}

impl WatcherSummary {
    pub(in crate::services::discord) fn capture(
        summary: Option<LiveSummary>,
        actor: Option<&std::sync::Weak<crate::services::provider::CancelToken>>,
    ) -> Option<Self> {
        summary.map(|summary| Self {
            summary,
            actor: actor.cloned(),
        })
    }

    async fn permits_send(&self, shared: &SharedData) -> bool {
        let Some(mailbox) = shared.mailbox_peek(self.summary.key.channel_id) else {
            return true;
        };
        let snapshot = mailbox.snapshot().await;
        if snapshot.cancel_token.is_none() && snapshot.active_user_message_id.is_none() {
            return true;
        }
        self.actor.as_ref().is_some_and(|actor| {
            snapshot
                .cancel_token
                .as_ref()
                .is_some_and(|active| std::sync::Weak::ptr_eq(actor, &Arc::downgrade(active)))
                && self
                    .summary
                    .key
                    .matches_episode_nonce(snapshot.active_turn_nonce.as_deref())
        })
    }
}

pub(in crate::services::discord) async fn post_watcher(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    summary: Option<WatcherSummary>,
) {
    let Some(summary) = summary else { return };
    if !summary.permits_send(shared).await {
        return;
    }
    let gateway = DiscordGateway::new(http.clone(), shared.clone(), provider.clone(), None);
    post_live(&gateway, provider, summary.summary).await;
}

#[cfg(test)]
#[path = "turn_end_summary_tests.rs"]
pub(in crate::services::discord) mod tests;

#[cfg(all(test, unix))]
#[path = "watcher_summary_authority_tests.rs"]
mod watcher_authority_tests;
