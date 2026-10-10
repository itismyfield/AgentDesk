//! Hold causes a channel supervisor reports as health, and the Notices it sends for them.

use super::fence::modes::TRANSITION_HELD;
use super::supervisor::mapping::Edge;

/// Why a supervised channel stopped short of admission; each cause owns one health line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HoldCause {
    BindingUnreadable,
    LedgerUnreadable,
    /// A writer map edge at a discovery boundary; it stays until a new process rechecks.
    MappingPresent(Edge, &'static str),
    MappingUnavailable,
    RuntimeViewPending,
    ModeRefused(&'static str),
    TransitionHeld(&'static str),
    Unbound(Vec<u64>),
    /// Input waits at the head row: why, the row, and how many rows wait.
    InputHeld(&'static str, u64, usize),
}

impl HoldCause {
    /// A later report for the same slot replaces the earlier line.
    pub(crate) fn slot(&self) -> &'static str {
        match self {
            Self::BindingUnreadable => "binding",
            Self::LedgerUnreadable => "ledger",
            Self::MappingPresent(..) => "mapped_thread",
            Self::MappingUnavailable => "mapping_unavailable",
            Self::RuntimeViewPending => "runtime_view_pending",
            Self::ModeRefused(_) => "mode",
            Self::TransitionHeld(_) => "transition",
            Self::Unbound(_) => "unbound",
            Self::InputHeld(..) => "input",
        }
    }

    pub(crate) fn health(&self, provider: &str, channel: u64) -> String {
        let at = format!("provider={provider} channel={channel}");
        match self {
            Self::BindingUnreadable => {
                format!("input_reconcile_required {at} reason=binding_unreadable")
            }
            Self::LedgerUnreadable => format!("ledger_unreadable {at}"),
            Self::MappingPresent(edge, boundary) => format!(
                "{TRANSITION_HELD} {at} reason=mapped_thread writer={} parent={} thread={} at={boundary} recheck=next_boot",
                edge.writer, edge.parent, edge.thread
            ),
            Self::MappingUnavailable | Self::RuntimeViewPending => {
                format!("{TRANSITION_HELD} {at} reason={}", self.slot())
            }
            Self::ModeRefused(reason) => format!("turn_mode_refused {at} reason={reason}"),
            Self::InputHeld(reason, head, held) => {
                format!("input_reconcile_required {at} reason={reason} head={head} held={held}")
            }
            Self::TransitionHeld(reason) => format!("{TRANSITION_HELD} {at} reason={reason}"),
            Self::Unbound(keys) => {
                let shown: Vec<String> = keys.iter().take(8).map(u64::to_string).collect();
                let more = match keys.len().saturating_sub(8) {
                    0 => String::new(),
                    more => format!("+{more}"),
                };
                format!(
                    "input_reconcile_required {at} reason=unbound keys={}{more}",
                    shown.join(",")
                )
            }
        }
    }
}

/// Every unbound report about one key is the same Notice episode.
pub(crate) fn topic(reason: &'static str) -> &'static str {
    if reason.ends_with("unbound") {
        "unbound"
    } else {
        reason
    }
}

pub(crate) fn notice(key: Option<u64>, reason: &str) -> String {
    match (key, reason) {
        (Some(key), reason) if reason.ends_with("unbound") => {
            // Only a collected population proves the Legacy copy still exists.
            let copy = if reason == "move_unbound" {
                "있음"
            } else {
                "확인 불가"
            };
            format!(
                "입력 `{key}`는 원장에 책임만 있고 내용이 없어 처리할 수 없어요. 자동으로 처리하지 않으며, 이 입력이 정리될 때까지 이 채널의 입력 전환·되돌리기·`/clear`가 보류됩니다(Legacy 사본: {copy}). 정리: `/cancel-queued message_id:{key}`."
            )
        }
        (_, "clear_retry_exhausted") => "/clear 완료를 확인하지 못한 채 재시도 한도를 넘어 입력 제출을 계속 보류해요. 입력 책임은 보존되며, `/clear`를 다시 실행하면 이어서 정리합니다.".to_owned(),
        (Some(key), "row_held") => format!(
            "입력 `{key}`의 처리 결과를 확인할 수 없어 보류했어요. 입력 책임은 보존되며 자동으로 다시 제출하지 않습니다."
        ),
        (Some(key), reason) => {
            format!("입력 `{key}`의 전환을 보류했어요 ({reason}). 입력 책임은 보존됩니다.")
        }
        (None, reason) => format!("입력 전환을 보류했어요 ({reason}). 입력 책임은 보존됩니다."),
    }
}

pub(crate) fn input_notice(head: u64, held: usize) -> String {
    format!(
        "새 세션의 transcript가 아직 확인되지 않아 대기 입력 {held}건(첫 입력 `{head}`)을 제출하지 않고 보류했어요. 입력은 보존되며 자동으로 제출하지 않습니다. 취소: `/cancel-queued message_id:{head}`. 입력 모드를 끄면 대기 입력은 기존 경로로 돌아갑니다."
    )
}
