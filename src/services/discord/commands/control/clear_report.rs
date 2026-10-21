//! What a committed user clear reports: the cleared notice, the waiting messages it discarded, and
//! at most one warning line for the writes, wait, stop or session end it could not confirm.

use std::collections::BTreeSet;
use std::future::Future;

use poise::serenity_prelude::MessageId;

use super::super::SESSION_CLEARED_RESPONSE;
use crate::services::turn_orchestrator::ClearChannelResult;

const RETRY_FAILED: &str = "일부 저장을 한 번 재시도했지만 완료하지 못했어요. 재시작 후 버린 메시지나 이전 세션이 복원될 수 있어요.";
const RETRY_LANDED: &str = "일부 저장이 처음에 실패했지만 재시도로 완료했어요.";
const HERDR_CLEANUP_UNCONFIRMED: &str =
    "일부 정리를 완료하지 못했어요. 재시작 후 대기 메시지나 이전 세션이 복원될 수 있어요.";
const STOP_UNCONFIRMED: &str = "실행 중이던 턴의 종료를 확인하지 못했어요.";
const TRANSITION_BUSY: &str = "진행 중이던 세션 작업이 끝나기를 기다리지 않고 초기화했어요.";
const HERDR_TRANSITION_BUSY: &str = "진행 중인 세션 작업의 완료를 확인하지 못했어요.";
const END_UNCONFIRMED: &str = "이전 세션 프로세스의 종료를 확인하지 못했어요.";
const COUNT_UNKNOWN: &str = "대기 메시지 수를 확인하지 못했어요.";

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ExecutionCleanup {
    Held(String),
    Pending(String),
    Retired,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::services::discord) struct ClearReport {
    pub(super) execution: Option<ExecutionCleanup>,
    /// Writes whose retry landed.
    pub(super) retried: Vec<&'static str>,
    /// Writes that never landed; a restart may restore what they would have removed.
    pub(super) failed: Vec<&'static str>,
    /// The session transition stayed held past its wait, so the clear went ahead without it.
    pub(super) transition_busy: bool,
    pub(super) stop_unconfirmed: bool,
    pub(super) end_unconfirmed: bool,
    /// The waiting messages the clear discarded; `None` when the mailbox could not say.
    pub(super) discarded: Option<BTreeSet<MessageId>>,
}

impl ClearReport {
    /// One line naming every cause, or none when the clear left nothing behind.
    pub(in crate::services::discord) fn warning(&self) -> Option<String> {
        let saved = match (self.failed.is_empty(), self.retried.is_empty()) {
            (false, _) if self.execution.is_some() => Some(HERDR_CLEANUP_UNCONFIRMED),
            (false, _) => Some(RETRY_FAILED),
            (true, false) => Some(RETRY_LANDED),
            (true, true) => None,
        };
        let busy_message = match &self.execution {
            Some(ExecutionCleanup::Held(_) | ExecutionCleanup::Pending(_)) => HERDR_TRANSITION_BUSY,
            _ => TRANSITION_BUSY,
        };
        let busy = self.transition_busy.then_some(busy_message);
        let stop = self.stop_unconfirmed.then_some(STOP_UNCONFIRMED);
        let end = self.end_unconfirmed.then_some(END_UNCONFIRMED);
        let causes: Vec<&str> = [saved, busy, stop, end].into_iter().flatten().collect();
        (!causes.is_empty()).then(|| format!("⚠️ {}", causes.join(" ")))
    }

    /// The cleared notice, the discarded count, and the warning line under them.
    pub(in crate::services::discord) fn reply(&self) -> String {
        let count = match &self.discarded {
            Some(ids) => format!("대기 메시지 {}건을 비웠어요.", ids.len()),
            None => COUNT_UNKNOWN.to_string(),
        };
        let execution = match &self.execution {
            Some(ExecutionCleanup::Held(reason)) => {
                format!("이전 Herdr 실행은 정리하지 못했어요. {reason}")
            }
            Some(ExecutionCleanup::Pending(reason)) => {
                format!("이전 Herdr 실행은 퇴역했지만 정리가 남아 있어요. {reason}")
            }
            Some(ExecutionCleanup::Retired)
                if !self.failed.is_empty() || self.discarded.is_none() =>
            {
                "이전 Herdr 실행을 정리했어요.".into()
            }
            _ => SESSION_CLEARED_RESPONSE.into(),
        };
        let complete = self.execution == Some(ExecutionCleanup::Retired)
            && self.failed.is_empty()
            && self.discarded.is_some();
        let mut lines = if self.execution.is_some() && !complete {
            vec![count, execution]
        } else {
            vec![execution, count]
        };
        if let Some(warning) = self.warning() {
            lines.push(warning);
        }
        lines.join("\n")
    }

    /// Takes the mailbox clear's write outcome and the queued and marker ids it discarded, each
    /// source id once, without the active or starting turn's.
    pub(super) fn absorb(&mut self, cleared: &ClearChannelResult) {
        self.retried.extend_from_slice(&cleared.persistence.retried);
        self.failed.extend_from_slice(&cleared.persistence.failed);
        self.discarded = (!cleared.persistence.count_unknown).then(|| {
            let not_waiting = &cleared.persistence.not_waiting;
            cleared
                .discarded_message_ids
                .iter()
                .filter(|id| !not_waiting.contains(id))
                .copied()
                .collect()
        });
    }

    /// The command message that asked for this clear was never waiting input.
    pub(in crate::services::discord) fn excluding(mut self, command: MessageId) -> Self {
        if let Some(ids) = self.discarded.as_mut() {
            ids.remove(&command);
        }
        self
    }

    /// A write the clear skipped or could not land; logged under `what`.
    pub(super) fn fail(&mut self, what: &'static str, error: &str) {
        tracing::error!(what, %error, "clear write not landed; a restart may restore discarded state");
        self.failed.push(what);
    }

    /// Runs `write`, and once more if it fails; the outcome is recorded under `what`.
    pub(super) async fn attempt<T, F, Fut>(&mut self, what: &'static str, mut write: F) -> Option<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = anyhow::Result<T>>,
    {
        let first = match write().await {
            Ok(value) => return Some(value),
            Err(error) => error,
        };
        tracing::warn!(what, error = %first, "clear write failed; retrying once");
        match write().await {
            Ok(value) => {
                self.retried.push(what);
                Some(value)
            }
            Err(error) => {
                self.fail(what, &error.to_string());
                None
            }
        }
    }
}

pub(super) async fn notify(
    shared: &std::sync::Arc<super::SharedData>,
    channel: poise::serenity_prelude::ChannelId,
    source: &str,
    mode: super::SoftClearNotifyMode,
    session_key: Option<&str>,
    report: &ClearReport,
) {
    if let Some((reason, content)) = super::soft_clear_lifecycle_notify_row(source, mode) {
        let content = if report.execution.is_some() {
            report.reply()
        } else {
            report
                .warning()
                .map_or(content.clone(), |warning| format!("{content}\n{warning}"))
        };
        crate::services::message_outbox::enqueue_lifecycle_notification_best_effort(
            shared.pg_pool.as_ref(),
            &format!("channel:{}", channel.get()),
            session_key,
            reason,
            &content,
        );
    }
}

#[cfg(test)]
#[path = "clear_count_tests.rs"]
mod clear_count_tests;

#[cfg(test)]
#[path = "herdr_report_tests.rs"]
mod herdr_report_tests;
