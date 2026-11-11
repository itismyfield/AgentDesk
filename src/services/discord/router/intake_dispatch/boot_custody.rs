//! Only the existing O Hold branch retains input here; ordinary local execution is unchanged.
use super::{IntakeBlockedReason, IntakeDeps, IntakeRoute, IntakeSubmission, intake_route};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::queue_io;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::ownership::GatewayOwnership;
use crate::services::tui_o::{cutover, ownership, writer::host};

pub(in crate::services::discord) const WRITER_HELD: &str = "NotSent(\"writer_boot_held\")";

/// Gateway custody and channel execution use independent gates on managed hosts.
pub(super) fn can_hold(provider: &str, channel: u64) -> bool {
    let gateway = ownership::gate(provider);
    if !matches!(gateway.current(), GatewayOwnership::Owned { .. }) {
        return false;
    }
    let Some(snapshot) = cutover::boot_snapshot() else {
        return false;
    };
    let kind = match provider {
        "claude" => RuntimeHandoffKind::ClaudeTui,
        "codex" => RuntimeHandoffKind::CodexTui,
        _ => return false,
    };
    if snapshot.kind(channel) != Some(kind) {
        return false;
    }
    let Some(permit) = snapshot
        .execution(channel)
        .and_then(|view| view.permit.ok())
    else {
        return false;
    };
    permit.identity.provider() == provider
        && permit.identity.raw_channel_id() == channel.to_string()
        && permit.gate().current().epoch().is_some()
}

pub(in crate::services::discord) async fn writer_boot_held(
    provider: &ProviderKind,
    channel: u64,
) -> bool {
    can_hold(provider.as_str(), channel)
        && cutover::codex_turn_awaited(channel).is_none()
        && matches!(
            intake_route::for_input_placement(provider.as_str(), channel).await,
            IntakeRoute::Hold(_)
        )
}

pub(super) async fn retain(
    deps: &IntakeDeps<'_>,
    mut submission: IntakeSubmission,
    prepared: Result<
        crate::services::cluster::attachment_transfer::uploads::PendingUploads,
        String,
    >,
) -> Result<(), crate::services::discord::Error> {
    let provider = submission.provider.clone();
    let channel = submission.request.channel_id;
    let uploads = match prepared {
        Ok(uploads) if can_hold(provider.as_str(), channel.get()) => uploads,
        Ok(_) => {
            return refuse(
                deps,
                &submission,
                "local writer ownership changed before custody".into(),
            )
            .await;
        }
        Err(detail) => return refuse(deps, &submission, detail).await,
    };
    submission.preloaded_uploads.extend(uploads);
    submission.attachments.clear();
    // Keep the notice context while the original moves into the existing durable enqueue.
    let request = submission.request.clone();
    let result = super::hold_in_custody(deps, submission, "writer is held".into()).await;
    if let Err(error) = result {
        let reason = IntakeBlockedReason::RoutingDependencyFailed {
            detail: error.to_string(),
        };
        let notice = IntakeSubmission {
            provider,
            request,
            origin: super::IntakeOrigin::LiveMessage,
            preserve_on_cancel: false,
            has_nonportable_uploads: false,
            attachments: Vec::new(),
            preloaded_uploads: Vec::new(),
            owner: None,
        };
        super::notify_blocked_intake(deps, &notice, &reason).await;
        return Err(error);
    }
    // A ready edge before this commit saw an empty queue; this level check closes that window.
    if host::channel_accepts(channel.get()) {
        queue_io::schedule_writer_ready_queue_kick(deps.shared.clone(), provider, channel);
    }
    Ok(())
}

async fn refuse(
    deps: &IntakeDeps<'_>,
    submission: &IntakeSubmission,
    detail: String,
) -> Result<(), crate::services::discord::Error> {
    let reason = IntakeBlockedReason::RoutingDependencyFailed {
        detail: detail.clone(),
    };
    super::notify_blocked_intake(deps, submission, &reason).await;
    Err(detail.into())
}
