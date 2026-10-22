use super::*;

pub(in crate::services::discord::router::message_handler) fn claude_tui_busy_followup_refusal_notice(
    reason: Option<crate::services::turn_orchestrator::EnqueueRefusalReason>,
) -> &'static str {
    use crate::services::turn_orchestrator::EnqueueRefusalReason as E;
    match reason {
        Some(
            E::AlreadyActiveTurn
            | E::AbsorbedByActiveTurn
            | E::ClaimedSinceObservation
            | E::SourceIdPendingOrActive,
        ) => CLAUDE_TUI_BUSY_FOLLOWUP_ALREADY_ACTIVE_NOTICE,
        Some(E::SupersededByClear) => "이 입력은 !clear로 폐기했어요.",
        Some(E::SourceIdAlreadyQueued) => CLAUDE_TUI_BUSY_FOLLOWUP_ALREADY_QUEUED_NOTICE,
        Some(E::LastItemDedup) => CLAUDE_TUI_BUSY_FOLLOWUP_DEDUP_NOTICE,
        Some(E::ActorUnreachable | E::MailboxClosed) => {
            CLAUDE_TUI_BUSY_FOLLOWUP_QUEUE_UNREACHABLE_NOTICE
        }
        None => CLAUDE_TUI_BUSY_FOLLOWUP_NOTICE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::turn_orchestrator::EnqueueRefusalReason as E;

    #[test]
    fn a_clear_superseded_followup_reports_discard_without_changing_existing_notices() {
        for reason in [
            E::AlreadyActiveTurn,
            E::AbsorbedByActiveTurn,
            E::ClaimedSinceObservation,
            E::SourceIdPendingOrActive,
        ] {
            assert_eq!(
                claude_tui_busy_followup_refusal_notice(Some(reason)),
                "📬 이 메시지는 이미 처리 중이라 추가 적재하지 않았습니다. 현재 결과를 기다려 주세요."
            );
        }
        for reason in [E::ActorUnreachable, E::MailboxClosed] {
            assert_eq!(
                claude_tui_busy_followup_refusal_notice(Some(reason)),
                "⚠ 내부 처리 큐에 접근하지 못해 이 메시지를 적재하지 못했습니다. 잠시 후 다시 보내 주세요."
            );
        }
        assert_eq!(
            claude_tui_busy_followup_refusal_notice(Some(E::SourceIdAlreadyQueued)),
            "📬 이 메시지는 이미 큐에 들어가 있어 추가 적재하지 않았습니다. 큐 결과를 기다려 주세요."
        );
        assert_eq!(
            claude_tui_busy_followup_refusal_notice(Some(E::LastItemDedup)),
            "📬 방금 동일한 메시지가 큐에 적재되어 중복으로 무시했습니다. 큐 결과를 기다려 주세요."
        );
        assert_eq!(
            claude_tui_busy_followup_refusal_notice(None),
            "⚠ Claude TUI가 아직 이전 터미널 턴을 처리 중이라 이 메시지를 주입하지 않았습니다. 현재 응답이 끝난 뒤 다시 보내 주세요."
        );
        assert_eq!(
            claude_tui_busy_followup_refusal_notice(Some(E::SupersededByClear)),
            "이 입력은 !clear로 폐기했어요."
        );
    }
}
