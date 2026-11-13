//! One owned Claude busy effect; the prepared host operation preserves its key-time guards.
use std::path::PathBuf;

use super::{Completion, DraftNotice, Leaf, refused};
use crate::services::claude_tui::busy_inject;
use crate::services::claude_tui::host_input::{BusyOperation, BusyOutcome};
use crate::services::claude_tui::owner_input;

pub(crate) struct BusyClaude {
    pub(crate) operation: BusyOperation,
    pub(crate) session: String,
    pub(crate) transcript: PathBuf,
    pub(crate) channel: u64,
    pub(crate) source: String,
    pub(crate) author: String,
    pub(crate) name: String,
    pub(crate) nonce: String,
    pub(crate) text: String,
    pub(crate) draft_notice: Option<DraftNotice>,
}

impl BusyClaude {
    pub(crate) fn leaf(self) -> Leaf {
        Box::new(move |frame| {
            let Self {
                operation,
                session,
                transcript,
                channel,
                source,
                author,
                name,
                nonce,
                text,
                draft_notice,
            } = self;
            if busy_inject::frame(&source, &author, &name, &nonce, &text) != frame {
                owner_input::vetoed("frame_mismatch");
                return Ok(Completion::NotSent("frame_mismatch".into()));
            }
            let request = busy_inject::Request {
                session: &session,
                transcript: &transcript,
                source: &source,
                author: &author,
                name: &name,
                nonce: &nonce,
                text: &text,
            };
            let host = operation.host_tag();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                operation.submit(&request)
            }));
            let alert = |detail: &str| {
                let mut payload = serde_json::json!({
                    "detail": detail, "nonce": nonce, "source": source,
                    "tmux_session": session, "owner": true,
                });
                if let Some(host) = host {
                    payload["host"] = host.into();
                }
                tracing::warn!(channel_id = channel, %payload, "owner busy input unconfirmed");
                crate::services::observability::events::record_simple(
                    "busy_inject_unconfirmed",
                    Some(channel),
                    Some("claude"),
                    payload,
                );
                detail.to_string()
            };
            let report = match outcome {
                Ok(report) => report,
                Err(_) => return Ok(Completion::WriteUncertain(alert("executor_failed"))),
            };
            if report.draft_unproven
                && let Some(notice) = draft_notice
            {
                tracing::info!(channel_id = channel, %nonce, "owned paste met an unparked draft");
                notice();
            }
            Ok(match report.outcome {
                BusyOutcome::NotSent {
                    reason,
                    waits,
                    probe,
                } => {
                    if let Some(probe) = probe {
                        super::super::held_watch::arm(channel, probe);
                    }
                    refused(reason, waits)
                }
                BusyOutcome::Submitted => Completion::Submitted,
                BusyOutcome::Failed(detail) => Completion::Failed(alert(detail)),
                BusyOutcome::Uncertain(detail) => Completion::WriteUncertain(alert(detail)),
                BusyOutcome::Stopped => {
                    owner_input::stop_took_gate();
                    Completion::WriteUncertain("stopped_before_key".into())
                }
            })
        })
    }
}
