use super::super::intake_queue_transaction::SoftInterventionSpec;
use super::busy_inject::LiveText;

/// Live intake and a veto handback carry exactly the same queue payload.
pub(super) fn spec(live: &LiveText<'_>) -> SoftInterventionSpec {
    SoftInterventionSpec {
        channel_id: live.channel_id,
        author_id: live.message.author.id,
        author_is_bot: live.message.author.bot,
        author_is_allowed_automation: live.author_is_allowed_automation,
        message_id: live.message.id,
        text: live.text.to_string(),
        reply_context: live.reply_context.map(str::to_string),
        has_reply_boundary: live.has_reply_boundary,
        merge_consecutive: live.merge_consecutive,
        pending_uploads: live.uploads.to_vec(),
    }
}
