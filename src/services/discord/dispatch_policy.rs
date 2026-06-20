use std::borrow::Cow;
use std::sync::OnceLock;

use super::QueueExitKind;
use super::SharedData;
use super::parse_dispatch_id;

const MONITOR_AUTO_TURN_ORIGIN_LITERAL: &str = "[origin=monitor_auto_turn]";

fn hidden_monitor_auto_turn_origin_marker() -> &'static str {
    static MARKER: OnceLock<String> = OnceLock::new();
    MARKER.get_or_init(|| {
        MONITOR_AUTO_TURN_ORIGIN_LITERAL
            .bytes()
            .flat_map(|byte| {
                (0..8).rev().map(move |shift| {
                    if (byte >> shift) & 1 == 1 {
                        '\u{200C}'
                    } else {
                        '\u{200B}'
                    }
                })
            })
            .collect()
    })
}

pub(in crate::services::discord) fn prepend_monitor_auto_turn_origin(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{}{}", hidden_monitor_auto_turn_origin_marker(), trimmed)
    }
}

pub(in crate::services::discord) fn strip_monitor_auto_turn_origin<'a>(
    text: &'a str,
) -> (Cow<'a, str>, bool) {
    if let Some(rest) = text.strip_prefix(hidden_monitor_auto_turn_origin_marker()) {
        return (Cow::Borrowed(rest), true);
    }

    if let Some(rest) = text.strip_prefix(MONITOR_AUTO_TURN_ORIGIN_LITERAL) {
        return (Cow::Owned(rest.trim_start().to_string()), true);
    }

    (Cow::Borrowed(text), false)
}

pub(super) fn should_process_allowed_bot_turn_text(text: &str) -> bool {
    let (sanitized, has_monitor_origin) = strip_monitor_auto_turn_origin(text);
    has_monitor_origin || sanitized.trim_start().starts_with("DISPATCH:")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct StaleDispatchTurn {
    pub(in crate::services::discord) dispatch_id: String,
    pub(in crate::services::discord) status: String,
    pub(in crate::services::discord) queue_exit_kind: QueueExitKind,
}

fn dispatch_status_allows_turn(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "pending" | "dispatched" | "in_progress"
    )
}

fn stale_dispatch_queue_exit_kind(
    status: Option<&str>,
    result: Option<&str>,
) -> Option<QueueExitKind> {
    let Some(status) = status.map(str::trim).filter(|value| !value.is_empty()) else {
        return Some(QueueExitKind::Superseded);
    };
    if dispatch_status_allows_turn(status) {
        return None;
    }
    let normalized_status = status.to_ascii_lowercase();
    let result_says_superseded = result
        .map(str::to_ascii_lowercase)
        .is_some_and(|value| value.contains("superseded"));
    if normalized_status == "superseded" || result_says_superseded {
        Some(QueueExitKind::Superseded)
    } else {
        Some(QueueExitKind::Cancelled)
    }
}

pub(in crate::services::discord) async fn stale_dispatch_turn_for_text(
    pg_pool: Option<&sqlx::PgPool>,
    text: &str,
) -> Option<StaleDispatchTurn> {
    let dispatch_id = parse_dispatch_id(text)?;
    let Some(pool) = pg_pool else {
        return None;
    };
    let row = match sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, result::TEXT AS result
           FROM task_dispatches
          WHERE id = $1",
    )
    .bind(&dispatch_id)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(error) => {
            tracing::warn!(
                dispatch_id = %dispatch_id,
                error = %error,
                "failed to validate dispatch turn status; allowing message to proceed"
            );
            return None;
        }
    };
    match row {
        Some((status, result)) => stale_dispatch_queue_exit_kind(Some(&status), result.as_deref())
            .map(|queue_exit_kind| StaleDispatchTurn {
                dispatch_id,
                status,
                queue_exit_kind,
            }),
        None => Some(StaleDispatchTurn {
            dispatch_id,
            status: "missing".to_string(),
            queue_exit_kind: QueueExitKind::Superseded,
        }),
    }
}

#[cfg(test)]
mod dispatch_turn_gate_tests {
    use super::{QueueExitKind, dispatch_status_allows_turn, stale_dispatch_queue_exit_kind};

    #[test]
    fn dispatch_turn_status_allows_only_live_statuses() {
        for status in ["pending", "dispatched", "in_progress", " DISPATCHED "] {
            assert!(dispatch_status_allows_turn(status));
        }
        for status in [
            "cancelled",
            "completed",
            "failed",
            "superseded",
            "",
            "missing",
        ] {
            assert!(!dispatch_status_allows_turn(status));
        }
    }

    #[test]
    fn stale_dispatch_queue_exit_kind_classifies_terminal_statuses() {
        assert_eq!(stale_dispatch_queue_exit_kind(Some("pending"), None), None);
        assert_eq!(
            stale_dispatch_queue_exit_kind(
                Some("cancelled"),
                Some("Cancelled: superseded by rereview")
            ),
            Some(QueueExitKind::Superseded)
        );
        assert_eq!(
            stale_dispatch_queue_exit_kind(Some("failed"), Some("tmux session died")),
            Some(QueueExitKind::Cancelled)
        );
        assert_eq!(
            stale_dispatch_queue_exit_kind(None, None),
            Some(QueueExitKind::Superseded)
        );
    }
}

#[cfg(test)]
mod allowed_turn_sender_tests {
    use super::is_allowed_turn_sender;

    const ANNOUNCE_ID: u64 = 1001;
    const OTHER_BOT_ID: u64 = 2002;
    const HUMAN_ID: u64 = 3003;

    #[test]
    fn announce_bot_triggers_without_dispatch_marker() {
        // #3576: announce-authored PM-triage / deadlock / send-to-agent
        // text triggers a turn even without the DISPATCH:/monitor marker.
        assert!(is_allowed_turn_sender(
            &[ANNOUNCE_ID, OTHER_BOT_ID],
            Some(ANNOUNCE_ID),
            ANNOUNCE_ID,
            true,
            "PM triage: please pick up issue #42",
        ));
    }

    #[test]
    fn announce_bot_with_dispatch_marker_triggers() {
        assert!(is_allowed_turn_sender(
            &[ANNOUNCE_ID],
            Some(ANNOUNCE_ID),
            ANNOUNCE_ID,
            true,
            "DISPATCH:1f3c2b1a-0000-4000-8000-000000000000\n── implementation dispatch ──",
        ));
    }

    #[test]
    fn announce_bot_legacy_issue_card_is_suppressed() {
        // Conservative guard: catch-up replays of announce-authored issue /
        // completion cards must NOT trigger turns.
        assert!(!is_allowed_turn_sender(
            &[ANNOUNCE_ID],
            Some(ANNOUNCE_ID),
            ANNOUNCE_ID,
            true,
            "📋 **새 이슈 #42** — fix the thing\n> 상태: 🟡 open",
        ));
        assert!(!is_allowed_turn_sender(
            &[ANNOUNCE_ID],
            Some(ANNOUNCE_ID),
            ANNOUNCE_ID,
            true,
            "✅ **#42 완료** — fix the thing",
        ));
    }

    #[test]
    fn non_announce_allowed_bot_still_requires_dispatch_marker() {
        // Security (#706): a non-announce allowed bot is dropped when its
        // message lacks the DISPATCH:/monitor-origin marker.
        assert!(!is_allowed_turn_sender(
            &[ANNOUNCE_ID, OTHER_BOT_ID],
            Some(ANNOUNCE_ID),
            OTHER_BOT_ID,
            true,
            "just a status note, no marker",
        ));
        // …but the same bot WITH the marker triggers.
        assert!(is_allowed_turn_sender(
            &[ANNOUNCE_ID, OTHER_BOT_ID],
            Some(ANNOUNCE_ID),
            OTHER_BOT_ID,
            true,
            "DISPATCH:1f3c2b1a-0000-4000-8000-000000000000",
        ));
    }

    #[test]
    fn human_message_is_unaffected() {
        assert!(is_allowed_turn_sender(
            &[ANNOUNCE_ID],
            Some(ANNOUNCE_ID),
            HUMAN_ID,
            false,
            "hello agent",
        ));
        // An unknown bot (not announce, not allowed) is still dropped.
        assert!(!is_allowed_turn_sender(
            &[ANNOUNCE_ID],
            Some(ANNOUNCE_ID),
            OTHER_BOT_ID,
            true,
            "spam",
        ));
    }
}

pub(in crate::services::discord) async fn resolve_announce_bot_user_id(
    shared: &SharedData,
) -> Option<u64> {
    let registry = shared.health_registry()?;
    registry.utility_bot_user_id("announce").await
}

/// Cached lookup for the notify bot's Discord user id. Used by the message
/// router to classify incoming messages as `BackgroundTrigger` turns —
/// see `TurnKind` in `router/message_handler.rs` and the race-handler
/// preservation rule from #796.
pub(in crate::services::discord) async fn resolve_notify_bot_user_id(
    shared: &SharedData,
) -> Option<u64> {
    let registry = shared.health_registry()?;
    registry.utility_bot_user_id("notify").await
}

pub(in crate::services::discord) fn is_allowed_turn_sender(
    allowed_bot_ids: &[u64],
    announce_bot_id: Option<u64>,
    author_id: u64,
    author_is_bot: bool,
    text: &str,
) -> bool {
    if announce_bot_id.is_some_and(|id| id == author_id) {
        // #3576 (restores the announce branch removed by #3478): the
        // `announce` bot is the authoritative trigger source. Its live
        // traffic — dispatch envelopes, PM-triage / deadlock / escalation
        // cards, and agent-to-agent `/api/discord/send` messages — must
        // start turns WITHOUT requiring the `DISPATCH:` / monitor-origin
        // marker that gates other allowed bots. The `should_process_*`
        // marker gate (#706 security) only applies to non-announce bots.
        //
        // The lone exception is the legacy issue-announcement / completion
        // card (📋/✅) shape: issue cards now route through notify-bot
        // (#1448 follow-up, and #3478 removed the announce-token fallback
        // in `issue_announcements.rs`), so announce never authors them in
        // live traffic. This guard remains a conservative safety net for
        // catch-up replays of pre-cutover announce-authored cards so they
        // don't spawn spurious turns.
        return !is_legacy_announce_issue_card(text);
    }
    if allowed_bot_ids.contains(&author_id) {
        return should_process_allowed_bot_turn_text(text);
    }
    !author_is_bot
}

/// Conservative guard (#3576) that suppresses announce-authored issue
/// announcement / completion cards from triggering turns. Live issue cards
/// route through notify-bot, which never reaches the announce branch above;
/// this only catches catch-up replays of pre-cutover announce-authored cards.
fn is_legacy_announce_issue_card(text: &str) -> bool {
    let head = text.trim_start();
    if head.starts_with("📋 **새 이슈 #") {
        return true;
    }
    if let Some(rest) = head.strip_prefix("✅ **#") {
        let digits_end = rest
            .char_indices()
            .find(|(_, ch)| !ch.is_ascii_digit())
            .map(|(idx, _)| idx)
            .unwrap_or(rest.len());
        if digits_end > 0 && rest[digits_end..].starts_with(" 완료** —") {
            return true;
        }
    }
    false
}

pub(in crate::services::discord) fn should_phase2_recover_message(
    message_id: u64,
    checkpoint: Option<u64>,
    existing_ids: &std::collections::HashSet<u64>,
) -> bool {
    if existing_ids.contains(&message_id) {
        return false;
    }
    if checkpoint.is_some_and(|saved| message_id <= saved) {
        return false;
    }
    true
}
