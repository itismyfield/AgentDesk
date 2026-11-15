//! The binding event log as O reads it: records in seq order and a change notice. O never
//! writes it; the log implements `BindingEvents` and is the one thing passed to the actor.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_prompt_dedupe::binding_events as p5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingCause {
    Startup,
    Resume,
    Clear,
    Compact,
    Continuation,
    Fork,
    SameSessionVerified,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingTarget {
    Source(SourceId),
    /// The transcript file did not exist yet; a later `Resolved` names it.
    Pending {
        payload_session_id: String,
        payload_transcript_path: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingEvidence {
    pub hook_event: String,
    pub received_at: DateTime<Utc>,
    /// The binding writer judged this prompt to supersede its pane's waiting Pending.
    #[serde(default)]
    pub reclaims: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingRecord {
    Bound {
        old: Option<SourceId>,
        new: BindingTarget,
        cause: BindingCause,
        parent_hint: Option<SourceId>,
        evidence: BindingEvidence,
    },
    /// Names the file of the `Pending` bind at `resolves_seq`.
    Resolved { resolves_seq: u64, source: SourceId },
    /// A refused bind may close one Pending without binding a source.
    Rejected {
        detail: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        closes: Option<u64>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingEvent {
    /// Per-channel, starting at 1 and increasing by one.
    pub seq: u64,
    pub channel_id: u64,
    pub provider: ShadowProvider,
    pub tmux_session: String,
    pub execution_nonce: String,
    pub record: BindingRecord,
    pub committed_at: DateTime<Utc>,
}

pub trait BindingEvents: Send + Sync + 'static {
    /// The channel's events with a seq above `after`, in seq order.
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String>;
    /// The channel's whole log as recorded, with no bind held by the current source policy; a log
    /// without such a policy reads as delivered.
    fn binding_history(&self, channel: u64) -> Result<Vec<BindingEvent>, String> {
        self.binding_events_since(channel, 0)
    }
    /// The channel's latest committed seq, updated after each append.
    fn subscribe(&self, channel: u64) -> watch::Receiver<u64>;
}

/// The P5 binding event log read as O's port; a corrupt or unreadable log is an error, never a
/// shorter list, so the actor alarms instead of acting on a partial history.
pub struct BindingLog;

impl BindingLog {
    /// Events past `after`, each bind held as `policy` judges it.
    fn read(
        channel: u64,
        after: u64,
        policy: fn(&[&p5::BindingEvent]) -> Vec<bool>,
    ) -> Result<Vec<BindingEvent>, String> {
        let events = p5::binding_events_judged_since(channel, after);
        let events = events.map_err(|e| e.to_string())?;
        let judged: Vec<_> = events.iter().map(|(event, _)| event).collect();
        let allowed = policy(&judged);
        (events.into_iter().zip(allowed))
            .map(|((event, reclaims), allowed)| judged_from_p5(event, reclaims, allowed))
            .collect()
    }
}

impl BindingEvents for BindingLog {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        let policy = crate::services::tui_prompt_dedupe::codex_verified_events_allowed;
        Self::read(channel, after, policy)
    }

    fn binding_history(&self, channel: u64) -> Result<Vec<BindingEvent>, String> {
        Self::read(channel, 0, |events| vec![true; events.len()])
    }

    /// A log that cannot be watched reads as always changed, so every poll retries the read.
    fn subscribe(&self, channel: u64) -> watch::Receiver<u64> {
        p5::subscribe_binding_events(channel).unwrap_or_else(|_| watch::channel(u64::MAX).1)
    }
}

/// One channel's P5 log as its actor reads it; an event of another channel or provider is an
/// error, so the actor alarms instead of following a foreign bind.
pub struct ChannelBindingLog {
    channel: u64,
    provider: ShadowProvider,
}

impl ChannelBindingLog {
    pub fn new(channel: u64, provider: ShadowProvider) -> Self {
        Self { channel, provider }
    }

    fn own(
        &self,
        channel: u64,
        read: impl FnOnce() -> Result<Vec<BindingEvent>, String>,
    ) -> Result<Vec<BindingEvent>, String> {
        if channel != self.channel {
            return Err(format!(
                "channel {}'s binding log read for {channel}",
                self.channel
            ));
        }
        let events = read()?;
        let foreign = events
            .iter()
            .find(|event| event.channel_id != channel || event.provider != self.provider);
        match foreign {
            Some(event) => Err(format!(
                "binding event {} names channel {} {:?}",
                event.seq, event.channel_id, event.provider
            )),
            None => Ok(events),
        }
    }
}

impl BindingEvents for ChannelBindingLog {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        self.own(channel, || BindingLog.binding_events_since(channel, after))
    }

    fn binding_history(&self, channel: u64) -> Result<Vec<BindingEvent>, String> {
        self.own(channel, || BindingLog.binding_history(channel))
    }

    fn subscribe(&self, _channel: u64) -> watch::Receiver<u64> {
        BindingLog.subscribe(self.channel)
    }
}

#[cfg(test)]
fn from_p5(event: p5::BindingEvent, reclaims: bool) -> Result<BindingEvent, String> {
    let allowed = crate::services::tui_prompt_dedupe::codex_verified_events_allowed(&[&event]);
    judged_from_p5(event, reclaims, allowed[0])
}

/// `event` as O reads it, where `source_allowed` is its Codex source policy verdict.
fn judged_from_p5(
    event: p5::BindingEvent,
    reclaims: bool,
    source_allowed: bool,
) -> Result<BindingEvent, String> {
    let provider = match event.provider.as_str() {
        "claude" => ShadowProvider::Claude,
        "codex" => ShadowProvider::Codex,
        other => {
            return Err(format!(
                "binding event {} names provider {other:?}",
                event.seq
            ));
        }
    };
    let record = match event.new.clone() {
        p5::BindingTarget::Source(_) | p5::BindingTarget::Resolved { .. } if !source_allowed => {
            BindingRecord::Rejected {
                detail: "Codex source proof or delivery permission held".into(),
                closes: match &event.new {
                    p5::BindingTarget::Resolved { pending_seq, .. } => Some(*pending_seq),
                    _ => None,
                },
            }
        }
        p5::BindingTarget::Source(source) => bound(&event, reclaims, BindingTarget::Source(source)),
        p5::BindingTarget::Pending {
            payload_session_id,
            payload_transcript_path,
        } => {
            let payload_transcript_path = payload_transcript_path.unwrap_or_default().into();
            let pending = BindingTarget::Pending {
                payload_session_id,
                payload_transcript_path,
            };
            bound(&event, reclaims, pending)
        }
        p5::BindingTarget::Resolved {
            pending_seq,
            source,
        } => BindingRecord::Resolved {
            resolves_seq: pending_seq,
            source,
        },
        p5::BindingTarget::Rejected { reason, .. } => BindingRecord::Rejected {
            detail: reason,
            closes: if provider == ShadowProvider::Codex {
                p5::codex::terminal_pending_at_seq(event.channel_id, event.seq)
                    .map_err(|error| error.to_string())?
            } else {
                None
            },
        },
    };
    Ok(BindingEvent {
        seq: event.seq,
        channel_id: event.channel_id,
        provider,
        tmux_session: event.tmux_session,
        execution_nonce: event.execution_nonce.unwrap_or_default(),
        record,
        committed_at: event.committed_at,
    })
}

#[cfg(all(test, unix))]
#[path = "binding/terminal_tests.rs"]
pub(crate) mod terminal_tests;

fn bound(event: &p5::BindingEvent, reclaims: bool, new: BindingTarget) -> BindingRecord {
    let cause = match event.cause {
        p5::BindingCause::Startup => BindingCause::Startup,
        p5::BindingCause::Resume => BindingCause::Resume,
        p5::BindingCause::Clear => BindingCause::Clear,
        p5::BindingCause::Compact => BindingCause::Compact,
        p5::BindingCause::Continuation => BindingCause::Continuation,
        p5::BindingCause::Fork => BindingCause::Fork,
        p5::BindingCause::SameSessionVerified => BindingCause::SameSessionVerified,
        p5::BindingCause::Unknown => BindingCause::Unknown,
    };
    let evidence = BindingEvidence {
        hook_event: event.evidence.hook_event.clone().unwrap_or_default(),
        received_at: event.evidence.received_at,
        reclaims,
    };
    BindingRecord::Bound {
        old: event.old.clone(),
        new,
        cause,
        parent_hint: event.parent_hint.clone(),
        evidence,
    }
}
