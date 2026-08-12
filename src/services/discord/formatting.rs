use poise::serenity_prelude as serenity;
use regex::Regex;
use serenity::{ChannelId, CreateAttachment, MessageId};
use std::collections::HashSet;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::{Arc, LazyLock};

use super::{
    DISCORD_MSG_LIMIT, SharedData,
    placeholder_cleanup::{PlaceholderCleanupOutcome, classify_delete_error},
    rate_limit_wait,
    response_sanitizer::subagent_notification_card,
};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, super::Data, Error>;
const STREAMING_PLACEHOLDER_MARGIN: usize = 10;
const THINKING_STATUS_MAX_BYTES: usize = 600;
const TOOL_STATUS_MAX_BYTES: usize = 300;
/// Invisible marker appended to newly-rendered placeholder cards so probes can
/// distinguish status surfaces from delivered answers that happen to start
/// with the same handoff header text.
pub(super) const PLACEHOLDER_PROBE_MARKER: &str = "\u{2063}\u{2062}\u{2063}\u{2062}";

fn watcher_send_failure_message(
    class: super::replace_outcome_policy::WatcherSendFailureClass,
    message: impl std::fmt::Display,
) -> String {
    super::replace_outcome_policy::watcher_send_failure_classified_message(class, message)
}

pub(super) use super::reaction_lifecycle::is_real_discord_message_id;
#[cfg(test)]
pub(super) use super::reaction_lifecycle::reaction_target_channel_for_shared;

#[path = "formatting/long_send_rollback.rs"]
pub(in crate::services::discord) mod long_send_rollback;
#[path = "formatting/rollback_journal.rs"]
mod rollback_journal;

use self::long_send_rollback::delete_rollback_channel_message;
pub(in crate::services::discord) use self::long_send_rollback::message_ids_from_receipts;
pub(in crate::services::discord) use self::long_send_rollback::send_long_message_raw_with_reference_rollback;
pub(in crate::services::discord) use self::long_send_rollback::send_long_message_raw_with_reference_rollback_returning_receipts;
pub(in crate::services::discord) use self::long_send_rollback::send_long_message_raw_with_rollback;
pub(in crate::services::discord) use self::long_send_rollback::send_long_message_raw_with_rollback_returning_receipts;
#[cfg(test)]
use self::rollback_journal::{
    REPLACE_CONTINUATION_ROLLBACKS, force_next_replace_continuation_rollback_remove_failure,
    replace_continuation_rollback_path,
};
use self::rollback_journal::{
    ReplaceContinuationRollbackClaim, claim_replace_continuation_rollback,
    clear_replace_continuation_rollback, clear_replace_continuation_rollback_memory_only,
    record_replace_continuation_rollback, record_replace_continuation_rollback_memory_only,
    replace_continuation_rollback_key, task_response_continuation_rollback_key,
    unclaim_replace_continuation_rollback,
};

#[cfg(test)]
pub(in crate::services::discord) mod rollback_transport_test_hook {
    use super::*;

    // #5071 T1 S2: the hook yields (returned channel, message id) rather than a
    // bare id so tests can reproduce a Discord response that lands in a channel
    // other than the requested one. Collapsing the pair here would hide the
    // very `channel_mismatch` the delivery journal is meant to observe.
    type SendHook = Box<
        dyn Fn(
                ChannelId,
                &str,
                Option<(ChannelId, MessageId)>,
                Option<&str>,
                bool,
            ) -> Option<Result<(ChannelId, MessageId), String>>
            + Send
            + Sync,
    >;
    type DeleteHook = Box<dyn Fn(ChannelId, MessageId) -> Option<Result<(), String>> + Send + Sync>;

    static SEND_HOOK: LazyLock<Mutex<Option<SendHook>>> = LazyLock::new(|| Mutex::new(None));
    static DELETE_HOOK: LazyLock<Mutex<Option<DeleteHook>>> = LazyLock::new(|| Mutex::new(None));

    pub(in crate::services::discord) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            *SEND_HOOK.lock().unwrap_or_else(|error| error.into_inner()) = None;
            *DELETE_HOOK
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
        }
    }

    pub(in crate::services::discord) fn install(send: SendHook, delete: DeleteHook) -> Guard {
        *SEND_HOOK.lock().unwrap_or_else(|error| error.into_inner()) = Some(send);
        *DELETE_HOOK
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(delete);
        Guard
    }

    pub(super) fn send(
        channel_id: ChannelId,
        content: &str,
        reference: Option<(ChannelId, MessageId)>,
        nonce: Option<&str>,
        enforce_nonce: bool,
    ) -> Option<Result<(ChannelId, MessageId), Error>> {
        SEND_HOOK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .and_then(|hook| {
                hook(channel_id, content, reference, nonce, enforce_nonce)
                    .map(|result| result.map_err(Into::into))
            })
    }

    pub(super) fn delete(
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> Option<Result<(), Error>> {
        DELETE_HOOK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .and_then(|hook| hook(channel_id, message_id).map(|result| result.map_err(Into::into)))
    }
}

/// #5071 T1 S2: PATCH interception for `replace_long_message`'s chunk-0 edit.
/// Separate from [`rollback_transport_test_hook`] so installing one never
/// changes which requests the other intercepts. Yields `(returned channel,
/// message id)` so a channel mismatch is expressible.
#[cfg(test)]
pub(in crate::services::discord) mod chunk_transport_test_hook {
    use super::*;

    type ChunkEditHook = Box<
        dyn Fn(ChannelId, MessageId, &str) -> Option<Result<(ChannelId, MessageId), String>>
            + Send
            + Sync,
    >;

    static EDIT_HOOK: LazyLock<Mutex<Option<ChunkEditHook>>> = LazyLock::new(|| Mutex::new(None));

    pub(in crate::services::discord) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            *EDIT_HOOK.lock().unwrap_or_else(|error| error.into_inner()) = None;
        }
    }

    pub(in crate::services::discord) fn install(edit: ChunkEditHook) -> Guard {
        *EDIT_HOOK.lock().unwrap_or_else(|error| error.into_inner()) = Some(edit);
        Guard
    }

    pub(super) fn edit(
        channel_id: ChannelId,
        message_id: MessageId,
        content: &str,
    ) -> Option<Result<(ChannelId, MessageId), Error>> {
        EDIT_HOOK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .and_then(|hook| {
                hook(channel_id, message_id, content).map(|result| result.map_err(Into::into))
            })
    }
}

#[path = "formatting/tool_markdown.rs"]
mod tool_markdown;

use self::tool_markdown::convert_markdown_tables;
// These facade exports preserve the pre-split module paths even when a given
// build target has no in-crate consumer.
#[allow(unused_imports)]
pub(super) use self::tool_markdown::{
    ALL_TOOLS, BUILTIN_SKILLS, canonical_tool_name, escape_for_code_fence,
    extract_skill_description, filter_codex_tool_logs, format_for_discord_with_provider,
    format_for_discord_with_status_panel, format_tool_input, is_command_tool_name,
    normalize_empty_lines, risk_badge, shorten_path, strip_codex_tool_log_lines, tool_info,
    truncate_str,
};
pub(crate) use self::tool_markdown::{normalize_allowed_tools, redact_sensitive_for_placeholder};

#[path = "formatting/streaming_status.rs"]
mod streaming_status;

#[cfg(test)]
use self::streaming_status::char_count;
use self::streaming_status::strip_placeholder_lines;
#[allow(unused_imports)]
pub(super) use self::streaming_status::{
    LongRunningCloseTrigger, MonitorHandoffReason, MonitorHandoffStatus, StreamingRolloverPlan,
    build_monitor_handoff_placeholder, build_monitor_handoff_placeholder_with_context,
    build_monitor_handoff_placeholder_with_live_events, build_placeholder_status_block,
    build_processing_status_block, build_status_panel_streaming_edit_text,
    build_streaming_placeholder_text, byte_index_at_discord_message_units,
    classify_long_running_tool, discord_message_units, finalize_in_progress_tool_status,
    finalize_stale_streaming_footer, floor_char_boundary, format_for_discord, humanize_tool_status,
    is_streaming_placeholder_status_line, plan_streaming_rollover, preserve_previous_tool_status,
    resolve_raw_tool_status, streaming_split_boundary, text_ends_with_streaming_footer,
};

#[path = "formatting/delivery.rs"]
mod delivery;

use self::delivery::send_channel_message_with_optional_reference;
pub(super) use self::delivery::{
    build_long_message_attachment, needs_multiple_messages, send_long_message_ctx,
    send_long_message_raw, split_message,
};
#[allow(unused_imports)]
pub(in crate::services::discord) use self::delivery::{
    long_message_reply_builders, send_long_message_raw_with_reference,
    send_long_message_raw_with_reference_returning_message_ids, send_long_message_reply_ctx,
    send_single_message_returning_receipt,
};

#[cfg(test)]
#[path = "formatting/status_panel_v2_formatter_tests.rs"]
mod status_panel_v2_formatter_tests;

#[path = "formatting/replace_long_message.rs"]
mod replace_long_message;

#[cfg(test)]
pub(in crate::services::discord) use self::replace_long_message::ReplaceLastChunkAnchor;
// #5071 T1 S3a: the deferred receipt variant gained a production consumer — the
// watcher's cutover short-replace arm, which needs the tail-continuation receipt
// its `_with_outcome_` sibling does not surface.
pub(in crate::services::discord) use self::replace_long_message::replace_long_message_raw_deferred_returning_receipt;
pub(in crate::services::discord) use self::replace_long_message::{
    DeferredReplaceLongMessageOutcome, ReplaceLongMessageOutcome,
    cleanup_replace_continuations_after_failure, replace_long_message_outcome_to_result,
    replace_long_message_raw, replace_long_message_raw_deferred,
    replace_long_message_raw_with_outcome, replace_long_message_raw_with_outcome_returning_receipt,
    watcher_completion_footer_anchor,
};

#[cfg(test)]
mod relay_state_contract_refs {
    #[test]
    fn contract_symbols_exist() {
        use super::replace_long_message_raw_deferred as _;
    }
}

#[cfg(test)]
#[path = "formatting/replace_long_message_tests.rs"]
mod replace_long_message_tests;
