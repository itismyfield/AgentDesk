use std::sync::LazyLock;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use super::{IntakeSubmission, message_handler};
use crate::services::cluster::intake_router_hook::IntakeBlockedReason;

const BLOCK_NOTICE_INTERVAL: Duration = Duration::from_secs(60);
static BLOCK_NOTICE_AT: LazyLock<DashMap<(String, u64), Instant>> = LazyLock::new(DashMap::new);

fn claim_blocked_notice_slot(key: (String, u64), now: Instant) -> bool {
    match BLOCK_NOTICE_AT.entry(key) {
        dashmap::mapref::entry::Entry::Occupied(mut entry) => {
            if now.saturating_duration_since(*entry.get()) < BLOCK_NOTICE_INTERVAL {
                return false;
            }
            entry.insert(now);
        }
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            entry.insert(now);
        }
    }
    true
}

pub(super) async fn notify_blocked_intake(
    deps: &message_handler::IntakeDeps<'_>,
    submission: &IntakeSubmission,
    reason: &IntakeBlockedReason,
) {
    let key = (
        submission.provider.as_str().to_string(),
        submission.request.channel_id.get(),
    );
    let now = Instant::now();
    if !claim_blocked_notice_slot(key, now) {
        return;
    }

    let (detail, recovery) = match reason {
        IntakeBlockedReason::NonPortableAttachmentForeignOwner { owner_instance_id } => (
            format!(
                "기존 세션 owner `{owner_instance_id}`는 다른 노드에 있고 첨부파일 경로는 현재 노드 전용입니다."
            ),
            "현재 mac-mini routed session은 파일 첨부를 지원하지 않습니다. text-only로 다시 보내세요.",
        ),
        IntakeBlockedReason::NonPortableAttachmentRoutedTarget { target_instance_id } => (
            format!(
                "새 routed target `{target_instance_id}`는 현재 노드 전용 첨부파일 경로를 받을 수 없습니다."
            ),
            "현재 mac-mini routed session은 파일 첨부를 지원하지 않습니다. text-only로 다시 보내세요.",
        ),
        IntakeBlockedReason::StaleSessionOwners { instance_ids } => (
            format!(
                "기존 세션 owner 상태를 확인할 수 없습니다: `{}`.",
                instance_ids.join(", ")
            ),
            "기존 세션을 stop/clear한 뒤 다시 보내세요.",
        ),
        IntakeBlockedReason::ConflictingLiveSessionOwners { instance_ids } => (
            format!(
                "여러 live 세션 owner가 충돌합니다: `{}`.",
                instance_ids.join(", ")
            ),
            "기존 세션을 stop/clear한 뒤 다시 보내세요.",
        ),
        IntakeBlockedReason::OwnerProtocolIncompatible { instance_id } => (
            format!(
                "기존 세션 owner `{instance_id}`가 이 turn의 intake protocol을 지원하지 않습니다."
            ),
            "기존 세션을 stop/clear한 뒤 다시 보내세요.",
        ),
        IntakeBlockedReason::OverrideUnavailable { target_instance_id } => (
            format!(
                "선택한 `/node` 대상 `{target_instance_id}`가 현재 이 provider의 intake를 받을 수 없습니다."
            ),
            "기존 세션을 stop/clear한 뒤 다시 보내세요.",
        ),
        IntakeBlockedReason::OwnerLookupFailed { .. }
        | IntakeBlockedReason::RoutingDependencyFailed { .. } => (
            "세션 owner를 안전하게 확인하지 못했습니다.".to_string(),
            "기존 세션을 stop/clear한 뒤 다시 보내세요.",
        ),
    };
    let content = format!(
        "⛔ {detail} 잘못된 노드에 새 세션을 만들지 않도록 turn을 시작하지 않았습니다. {recovery}"
    );
    if let Err(error) = crate::services::discord::http::send_channel_message(
        deps.http.as_ref(),
        submission.request.channel_id,
        &content,
    )
    .await
    {
        tracing::warn!(%error, "[intake_dispatch] failed to send blocked-intake notice");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intake_dispatch_invariant_blocked_notice_is_throttled_atomically() {
        let key = ("claude".to_string(), 4_350_991);
        let now = Instant::now();
        assert!(claim_blocked_notice_slot(key.clone(), now));
        assert!(!claim_blocked_notice_slot(
            key.clone(),
            now + BLOCK_NOTICE_INTERVAL - Duration::from_millis(1)
        ));
        assert!(claim_blocked_notice_slot(key, now + BLOCK_NOTICE_INTERVAL));
    }
}
