//! The saved inflight turn fields a follow-up retry reads; the inflight store implements it.

use crate::services::cluster::attachment_transfer::uploads::PendingUploads;

pub(crate) trait FollowupRetryRow: Sync {
    fn user_msg_id(&self) -> u64;
    /// The id whose record carries the retry budget: the retry id once set, else the primary.
    fn effective_busy_followup_retry_user_msg_id(&self) -> u64;
    fn request_owner_user_id(&self) -> u64;
    fn source_message_ids(&self) -> &[u64];
    fn user_text(&self) -> &str;
    fn followup_reply_context(&self) -> &Option<String>;
    fn followup_has_reply_boundary(&self) -> bool;
    fn followup_merge_consecutive(&self) -> bool;
    fn followup_pending_uploads(&self) -> &PendingUploads;
    fn followup_preserve_on_cancel(&self) -> bool;
}
