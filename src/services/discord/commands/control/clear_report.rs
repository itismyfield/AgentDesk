//! What a committed user clear reports: the cleared notice, the waiting messages it discarded, and
//! at most one warning line for the writes, wait, stop or session end it could not confirm.

use std::collections::BTreeSet;
use std::future::Future;

use poise::serenity_prelude::MessageId;

use super::super::SESSION_CLEARED_RESPONSE;
use crate::services::turn_orchestrator::ClearChannelResult;

const RETRY_FAILED: &str = "일부 저장을 한 번 재시도했지만 완료하지 못했어요. 재시작 후 버린 메시지나 이전 세션이 복원될 수 있어요.";
const RETRY_LANDED: &str = "일부 저장이 처음에 실패했지만 재시도로 완료했어요.";
const STOP_UNCONFIRMED: &str = "실행 중이던 턴의 종료를 확인하지 못했어요.";
const TRANSITION_BUSY: &str = "진행 중이던 세션 작업이 끝나기를 기다리지 않고 초기화했어요.";
const END_UNCONFIRMED: &str = "이전 세션 프로세스의 종료를 확인하지 못했어요.";
const COUNT_UNKNOWN: &str = "대기 메시지 수를 확인하지 못했어요.";

#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::services::discord) struct ClearReport {
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
            (false, _) => Some(RETRY_FAILED),
            (true, false) => Some(RETRY_LANDED),
            (true, true) => None,
        };
        let busy = self.transition_busy.then_some(TRANSITION_BUSY);
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
        match self.warning() {
            Some(warning) => format!("{SESSION_CLEARED_RESPONSE}\n{count}\n{warning}"),
            None => format!("{SESSION_CLEARED_RESPONSE}\n{count}"),
        }
    }

    /// Takes the mailbox clear's write outcome and the queued and marker ids it discarded, each
    /// source id once, without the active or starting turn's.
    pub(super) fn absorb(&mut self, cleared: &ClearChannelResult) {
        self.retried.clone_from(&cleared.persistence.retried);
        self.failed.clone_from(&cleared.persistence.failed);
        self.discarded = (!cleared.persistence.count_unknown).then(|| {
            let items = cleared.queue_exit_events.iter().map(|e| &e.intervention);
            let ids =
                items.flat_map(|item| item.source_message_ids.iter().chain([&item.message_id]));
            let not_waiting = &cleared.persistence.not_waiting;
            ids.filter(|id| !not_waiting.contains(id))
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

#[cfg(test)]
#[path = "clear_count_tests.rs"]
mod clear_count_tests;
