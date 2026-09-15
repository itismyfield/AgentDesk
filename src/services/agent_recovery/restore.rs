use crate::services::provider::ProviderKind;

use super::checkpoint::{CheckpointEvent, CheckpointEventKind, CheckpointPayload};
use super::handoff::RecoveryIntake;
use super::policy::ChannelRecoveryBinding;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreSessionMode {
    Resume,
    Fresh,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestorePlan {
    pub channel_id: String,
    pub owner_agent_id: String,
    pub fallback_agent_id: String,
    pub session_mode: RestoreSessionMode,
    pub packet: String,
    pub owner_intake: Option<RecoveryIntake>,
    pub fallback_intake: Option<RecoveryIntake>,
}

pub fn session_mode_for_provider(provider: &ProviderKind) -> RestoreSessionMode {
    if provider
        .capabilities()
        .is_some_and(|capabilities| capabilities.supports_resume)
    {
        RestoreSessionMode::Resume
    } else {
        RestoreSessionMode::Fresh
    }
}

pub fn latest_progress_or_complete(events: &[CheckpointEvent]) -> Option<&CheckpointEvent> {
    events.iter().rev().find(|event| {
        matches!(
            event.kind,
            CheckpointEventKind::Complete
                | CheckpointEventKind::FallbackProgress
                | CheckpointEventKind::OwnerProgress
        )
    })
}

pub fn format_restore_packet(
    checkpoint_id: &str,
    fallback_agent_id: &str,
    primary_agent_id: &str,
    payload: &CheckpointPayload,
    fallback_succeeded: bool,
    summary: &str,
) -> String {
    let outcome = if fallback_succeeded {
        "succeeded"
    } else {
        "failed"
    };
    let progress = if payload.progress.trim().is_empty() {
        "unknown"
    } else {
        payload.progress.as_str()
    };
    let next = if payload.next.trim().is_empty() {
        "마지막 사용자 메시지부터 재확인"
    } else {
        payload.next.as_str()
    };
    let files = if payload.files.is_empty() {
        String::new()
    } else {
        payload.files.join(", ")
    };
    format!(
        "[recovery restore checkpoint_id={checkpoint_id} from={fallback_agent_id} to={primary_agent_id}]\n\n\
Goal: {}\n\
Progress: {progress}\n\
Decisions: {}\n\
Files: {files}\n\
Next: {next}\n\n\
Fallback outcome: {outcome} — {summary}\n\
You are the primary agent restored as a checkpoint. Do not redo completed Files. Continue from Next.",
        payload.goal, payload.decisions
    )
}

pub fn build_restore_plan(
    binding: &ChannelRecoveryBinding,
    events: &[CheckpointEvent],
    fallback_succeeded: bool,
    summary: &str,
) -> RestorePlan {
    let fallback_agent_id = binding
        .policy
        .as_ref()
        .map(|policy| policy.fallback_agent_id.clone())
        .unwrap_or_default();
    let source = latest_progress_or_complete(events);
    let payload = source
        .map(|event| event.payload.clone())
        .unwrap_or_else(|| {
            CheckpointPayload::compact(
                binding.owner_agent_id.clone(),
                "",
                "unknown",
                "",
                Vec::new(),
                "마지막 사용자 메시지부터 재확인",
                "",
            )
        });
    let checkpoint_id = source.map(|event| event.id.as_str()).unwrap_or("arc_none");
    RestorePlan {
        channel_id: binding.channel_id.clone(),
        owner_agent_id: binding.owner_agent_id.clone(),
        fallback_agent_id: fallback_agent_id.clone(),
        session_mode: session_mode_for_provider(&binding.owner_provider),
        packet: format_restore_packet(
            checkpoint_id,
            &fallback_agent_id,
            &binding.owner_agent_id,
            &payload,
            fallback_succeeded,
            summary,
        ),
        owner_intake: None,
        fallback_intake: None,
    }
}
