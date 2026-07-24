use poise::serenity_prelude as serenity;

use super::{IntakeAdmission, LocalAdmissionPermit};
use crate::services::discord::{
    HistoryItem, HistoryType, auto_restore_session, auto_restore_session_with_dm_hint,
    bootstrap_thread_session,
};

pub(crate) type AttachmentAdmissionGate = Result<LocalAdmissionPermit, IntakeAdmission>;

fn resolve_attachment_admission_with<T>(
    admission: IntakeAdmission,
    on_local: impl FnOnce(LocalAdmissionPermit) -> T,
) -> Result<T, IntakeAdmission> {
    match admission {
        IntakeAdmission::Local(permit) => Ok(on_local(permit)),
        nonlocal => Err(nonlocal),
    }
}

pub(crate) fn resolve_attachment_admission(admission: IntakeAdmission) -> AttachmentAdmissionGate {
    resolve_attachment_admission_with(admission, std::convert::identity)
}

async fn record_upload_history(
    shared: &std::sync::Arc<crate::services::discord::SharedData>,
    channel_id: serenity::ChannelId,
    upload_records: &[String],
) {
    if upload_records.is_empty() {
        return;
    }
    let mut data = shared.core.lock().await;
    if let Some(session) = data.sessions.get_mut(&channel_id) {
        session
            .history
            .extend(upload_records.iter().cloned().map(|content| HistoryItem {
                item_type: HistoryType::User,
                content,
            }));
    }
}

pub(crate) async fn prepare_admitted_live_attachments(
    deps: &super::super::message_handler::IntakeDeps<'_>,
    _local_permit: &LocalAdmissionPermit,
    channel_id: serenity::ChannelId,
    effective_channel_id: serenity::ChannelId,
    is_dm: bool,
    attachments: &[super::super::message_handler::AttachmentDescriptor],
) -> Result<Vec<String>, super::super::super::Error> {
    if attachments.is_empty() {
        return Ok(Vec::new());
    }
    let ctx = deps.ctx_for_chained_dispatch.ok_or_else(|| {
        std::io::Error::other("live attachment preparation requires a gateway context")
    })?;

    auto_restore_session_with_dm_hint(deps.shared, channel_id, ctx, Some(is_dm)).await;
    if effective_channel_id != channel_id {
        let needs_parent = {
            let data = deps.shared.core.lock().await;
            !data.sessions.contains_key(&channel_id)
        };
        if needs_parent {
            auto_restore_session(deps.shared, effective_channel_id, ctx).await;
            let parent_path = {
                let data = deps.shared.core.lock().await;
                data.sessions
                    .get(&effective_channel_id)
                    .and_then(|session| session.current_path.clone())
            };
            if let Some(path) = parent_path {
                bootstrap_thread_session(deps.shared, channel_id, &path, deps.http, deps.cache)
                    .await;
            }
        }
    }

    let attachment_permit =
        super::super::message_handler::LocalAttachmentPreparationPermit::after_local_admission();
    let upload_records = super::super::message_handler::prepare_admitted_local_attachment(
        deps.http,
        channel_id,
        attachments,
        deps.shared,
        &attachment_permit,
    )
    .await?;
    record_upload_history(deps.shared, channel_id, &upload_records).await;
    Ok(upload_records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::cluster::intake_router_hook::IntakeBlockedReason;

    fn nonlocal_admissions() -> Vec<IntakeAdmission> {
        vec![
            IntakeAdmission::Forwarded {
                target_instance_id: "foreign".to_string(),
                outbox_id: 7,
            },
            IntakeAdmission::SkippedDuplicate,
            IntakeAdmission::DeferredOpenRoute {
                target_instance_id: "foreign".to_string(),
            },
            IntakeAdmission::Blocked {
                reason: IntakeBlockedReason::RoutingDependencyFailed {
                    detail: "unavailable".to_string(),
                },
            },
        ]
    }

    #[test]
    fn nonlocal_attachment_admission_never_runs_local_materialization() {
        for admission in nonlocal_admissions() {
            let root = tempfile::tempdir().expect("temporary attachment root");
            let marker = root.path().join("materialized");
            let mut local_sessions = Vec::new();

            let outcome = resolve_attachment_admission_with(admission, |permit| {
                local_sessions.push("session");
                std::fs::write(&marker, b"upload").expect("write materialization marker");
                permit
            });

            assert!(outcome.is_err());
            assert!(local_sessions.is_empty());
            assert!(!marker.exists());
        }
    }

    #[test]
    fn local_attachment_admission_runs_materialization_once() {
        let root = tempfile::tempdir().expect("temporary attachment root");
        let marker = root.path().join("materialized");
        let mut local_sessions = Vec::new();

        let outcome = resolve_attachment_admission_with(
            IntakeAdmission::Local(LocalAdmissionPermit {
                channel_id: serenity::ChannelId::new(1),
                request_owner: serenity::UserId::new(2),
                has_nonportable_uploads: true,
            }),
            |permit| {
                local_sessions.push("session");
                std::fs::write(&marker, b"upload").expect("write materialization marker");
                permit
            },
        );

        assert!(outcome.is_ok());
        assert_eq!(local_sessions, vec!["session"]);
        assert_eq!(std::fs::read(marker).expect("read marker"), b"upload");
    }
}
