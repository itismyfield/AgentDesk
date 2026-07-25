use std::sync::atomic::{AtomicU64, Ordering};

use poise::serenity_prelude::MessageId;

use super::super::replace_outcome_policy::{
    WatcherSendFailureClass, classify_watcher_send_failure, watcher_send_failure_classified_message,
};

pub(super) fn watcher_classified_failure_message(
    class: WatcherSendFailureClass,
    message: impl std::fmt::Display,
) -> String {
    watcher_send_failure_classified_message(class, message)
}

pub(super) fn watcher_classified_error_string(error: &(dyn std::error::Error + 'static)) -> String {
    watcher_classified_failure_message(classify_watcher_send_failure(error), error)
}

pub(super) fn next_headless_message_id() -> MessageId {
    static HEADLESS_MESSAGE_ID_SEQ: AtomicU64 = AtomicU64::new(9_000_000_000_000_000_000);
    MessageId::new(HEADLESS_MESSAGE_ID_SEQ.fetch_add(1, Ordering::Relaxed))
}
