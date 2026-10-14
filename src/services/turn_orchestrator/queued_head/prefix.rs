//! Which queued input an automatic busy-TUI paste may take: the first ready one behind nothing but
//! parked inputs, and the shape a typed paste needs. The caller's look and the actor's claim both
//! decide here, so they cannot disagree on the rule.
use crate::services::discord::busy_followup_retry_store::RetryClass;
use crate::services::turn_orchestrator::{Intervention, InterventionMode};

/// Input a typed paste must not carry: commands, mentions and dispatch prefixes.
const NOT_TYPED_LEAD: [&str; 4] = ["!", "/", "<@", "DISPATCH:"];

/// Where the automatic walk over a queue stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Prefix {
    /// The first ready input sits at `at`; every input before it is parked.
    Ready { at: usize },
    /// An input whose retry state cannot be read: nothing at or behind it is taken.
    Unavailable,
    /// No input is ready.
    None,
}

/// Parked inputs are passed over and the first ready input ends the walk. An unreadable input
/// stops it, so nothing behind overtakes.
pub(crate) fn select(
    queue: &[Intervention],
    class: impl Fn(&Intervention) -> RetryClass,
) -> Prefix {
    for (at, item) in queue.iter().enumerate() {
        match class(item) {
            RetryClass::Ready => return Prefix::Ready { at },
            RetryClass::Parked => {}
            RetryClass::Unavailable(_) => return Prefix::Unavailable,
        }
    }
    Prefix::None
}

/// A lone human source whose single segment is the whole plain-text body: the shape a live
/// enqueue normalizes to and a disk round trip restores.
pub(crate) fn eligible(item: &Intervention) -> bool {
    let id = item.message_id;
    let [source] = item.source_message_queued_generations.as_slice() else {
        return false;
    };
    let raw_segments = match item.source_text_segments.as_slice() {
        [] => true,
        [segment] => segment.message_id == id && segment.text == item.text,
        _ => false,
    };
    let segments = item.source_text_segments();
    let lead = item.text.trim_start();
    item.mode == InterventionMode::Soft
        && !item.author_is_bot
        && source.message_id == id
        && source.queued_generation == item.queued_generation
        && source.preserve_on_cancel
        && item.source_message_ids == [id]
        && raw_segments
        && matches!(segments.as_slice(), [s] if s.message_id == id && s.text == item.text)
        && !lead.is_empty()
        && !NOT_TYPED_LEAD.iter().any(|prefix| lead.starts_with(prefix))
        && item.pending_uploads.is_empty()
        && item.voice_announcement.is_none()
        && item.reply_context.is_none()
        && !item.has_reply_boundary
}
