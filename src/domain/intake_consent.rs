#![cfg(unix)]

//! Consent custody values preserve the original input's wire identity.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

mod decision;
pub(crate) use decision::{
    HoldInput, IntentResolution, Marker, PREFIX, REARM_PREFIX, RearmMarker, is_unresolved, marker,
    permits_custody,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IntentRef {
    pub store: String,
    pub id: String,
    pub provider: String,
    pub channel: u64,
    pub source_message: u64,
    pub fifo: u64,
}

pub(crate) struct PayloadProjection<'a> {
    pub id: i64,
    pub target_instance_id: &'a str,
    pub forwarded_by_instance_id: &'a str,
    pub required_labels: &'a Value,
    pub execution_requirements: &'a Value,
    pub attachment_refs: &'a Value,
    pub channel_id: &'a str,
    pub user_msg_id: &'a str,
    pub replay_source_message_ids: Option<&'a [String]>,
    pub request_owner_id: &'a str,
    pub request_owner_name: Option<&'a str>,
    pub user_text: &'a str,
    pub reply_context: Option<&'a str>,
    pub has_reply_boundary: bool,
    pub dm_hint: Option<bool>,
    pub turn_kind: &'a str,
    pub merge_consecutive: bool,
    pub reply_to_user_message: bool,
    pub defer_watcher_resume: bool,
    pub wait_for_completion: bool,
    pub preserve_on_cancel: Option<bool>,
    pub agent_id: &'a str,
    pub provider: &'a str,
    pub attempt_no: i32,
    pub parent_outbox_id: Option<i64>,
    pub admission_kind: &'a str,
}

impl PayloadProjection<'_> {
    pub(crate) fn digest(&self) -> Result<String, String> {
        let fields = serde_json::json!([
            self.id,
            self.target_instance_id,
            self.forwarded_by_instance_id,
            self.required_labels,
            self.execution_requirements,
            self.attachment_refs,
            self.channel_id,
            self.user_msg_id,
            self.replay_source_message_ids,
            self.request_owner_id,
            self.request_owner_name,
            self.user_text,
            self.reply_context,
            self.has_reply_boundary,
            self.dm_hint,
            self.turn_kind,
            self.merge_consecutive,
            self.reply_to_user_message,
            self.defer_watcher_resume,
            self.wait_for_completion,
            self.preserve_on_cancel,
            self.agent_id,
            self.provider,
            self.attempt_no,
            self.parent_outbox_id,
            self.admission_kind,
            // Existing custody digests retain the removed epoch column's null slot.
            Value::Null
        ]);
        Ok(hex::encode(Sha256::digest(
            serde_json::to_vec(&fields).map_err(|error| error.to_string())?,
        )))
    }
}

pub(crate) struct RowProjection<'a> {
    pub id: i64,
    pub provider: &'a str,
    pub channel_id: &'a str,
    pub user_msg_id: &'a str,
    pub attempt_no: i32,
    pub spawned: bool,
    pub claim_owner: Option<&'a str>,
    pub owner_instance_id: Option<&'a str>,
    pub owner_generation: Option<i64>,
    pub payload: PayloadProjection<'a>,
}
