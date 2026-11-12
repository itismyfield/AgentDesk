#![cfg(unix)]

use super::IntakeOutboxRow;
use crate::domain::intake_consent::{PayloadProjection, RowProjection};

pub(crate) fn project(row: &IntakeOutboxRow) -> RowProjection<'_> {
    RowProjection {
        id: row.id,
        provider: &row.provider,
        channel_id: &row.channel_id,
        user_msg_id: &row.user_msg_id,
        attempt_no: row.attempt_no,
        spawned: row.status == super::IntakeOutboxStatus::Spawned,
        claim_owner: row.claim_owner.as_deref(),
        owner_instance_id: row.owner_instance_id.as_deref(),
        owner_generation: row.owner_generation,
        payload: PayloadProjection {
            id: row.id,
            target_instance_id: &row.target_instance_id,
            forwarded_by_instance_id: &row.forwarded_by_instance_id,
            required_labels: &row.required_labels,
            execution_requirements: &row.execution_requirements,
            attachment_refs: &row.attachment_refs,
            channel_id: &row.channel_id,
            user_msg_id: &row.user_msg_id,
            replay_source_message_ids: row.replay_source_message_ids.as_deref(),
            request_owner_id: &row.request_owner_id,
            request_owner_name: row.request_owner_name.as_deref(),
            user_text: &row.user_text,
            reply_context: row.reply_context.as_deref(),
            has_reply_boundary: row.has_reply_boundary,
            dm_hint: row.dm_hint,
            turn_kind: &row.turn_kind,
            merge_consecutive: row.merge_consecutive,
            reply_to_user_message: row.reply_to_user_message,
            defer_watcher_resume: row.defer_watcher_resume,
            wait_for_completion: row.wait_for_completion,
            preserve_on_cancel: row.preserve_on_cancel,
            agent_id: &row.agent_id,
            provider: &row.provider,
            attempt_no: row.attempt_no,
            parent_outbox_id: row.parent_outbox_id,
            admission_kind: &row.admission_kind,
        },
    }
}
