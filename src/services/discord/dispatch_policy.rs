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
    reason: Option<&str>,
) -> Option<QueueExitKind> {
    let Some(status) = status.map(str::trim).filter(|value| !value.is_empty()) else {
        return None;
    };
    if dispatch_status_allows_turn(status) {
        return None;
    }
    let normalized_status = status.to_ascii_lowercase();
    if normalized_status != "cancelled" {
        return None;
    }
    let reason = reason.map(str::trim).filter(|value| !value.is_empty());
    if reason.is_some_and(|value| value.starts_with("superseded_by_")) {
        Some(QueueExitKind::Superseded)
    } else if crate::dispatch::is_user_cancel_reason(reason)
        || crate::dispatch::is_system_cancel_reason(reason)
    {
        Some(QueueExitKind::Cancelled)
    } else {
        None
    }
}

const STALE_DISPATCH_REASON_QUERY: &str = "SELECT status, result
       FROM task_dispatches
      WHERE id = $1";

fn structured_cancel_reason(result: Option<&str>) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(result?)
        .ok()?
        .as_object()?
        .get("reason")?
        .as_str()
        .map(str::to_string)
}

fn stale_dispatch_turn_from_row(
    dispatch_id: String,
    row: Option<(String, Option<String>)>,
) -> Option<StaleDispatchTurn> {
    let (status, result) = row?;
    let reason = structured_cancel_reason(result.as_deref());
    stale_dispatch_queue_exit_kind(Some(&status), reason.as_deref()).map(|queue_exit_kind| {
        StaleDispatchTurn {
            dispatch_id,
            status,
            queue_exit_kind,
        }
    })
}

/// Returns a queue-exit decision only when the dispatch row contains structured
/// evidence of an explicit supersede, user cancellation, or recognized system
/// cleanup/close. Terminal status by itself, an empty status, a missing row,
/// and legacy plaintext result text are not exit evidence: the queued
/// instruction is allowed to proceed so retention cleanup or an incomplete
/// terminal write cannot silently discard user input.
pub(in crate::services::discord) async fn stale_dispatch_turn_for_text(
    pg_pool: Option<&sqlx::PgPool>,
    text: &str,
) -> Option<StaleDispatchTurn> {
    let dispatch_id = parse_dispatch_id(text)?;
    let Some(pool) = pg_pool else {
        return None;
    };
    let row = match sqlx::query_as::<_, (String, Option<String>)>(STALE_DISPATCH_REASON_QUERY)
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
    stale_dispatch_turn_from_row(dispatch_id, row)
}

#[cfg(test)]
mod dispatch_turn_gate_tests {
    use super::{
        QueueExitKind, STALE_DISPATCH_REASON_QUERY, dispatch_status_allows_turn,
        stale_dispatch_queue_exit_kind, stale_dispatch_turn_from_row, structured_cancel_reason,
    };

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
    fn queued_user_instruction_without_exit_evidence_is_preserved() {
        assert_eq!(stale_dispatch_queue_exit_kind(Some("pending"), None), None);
        for terminal_status in ["completed", "failed", "cancelled"] {
            assert_eq!(
                stale_dispatch_queue_exit_kind(Some(terminal_status), None),
                None,
                "terminal dispatch status alone is not queue-exit evidence"
            );
        }
        assert_eq!(stale_dispatch_queue_exit_kind(Some(""), None), None);
        assert_eq!(stale_dispatch_queue_exit_kind(None, None), None);
        assert!(stale_dispatch_turn_from_row("retained-message".to_string(), None).is_none());
        assert!(
            stale_dispatch_turn_from_row("empty-status".to_string(), Some((String::new(), None)),)
                .is_none()
        );
    }

    #[test]
    fn stale_dispatch_result_extracts_structured_evidence_without_database_cast() {
        assert!(STALE_DISPATCH_REASON_QUERY.contains("SELECT status, result"));
        assert!(!STALE_DISPATCH_REASON_QUERY.contains("::jsonb"));
        assert_eq!(
            structured_cancel_reason(Some("Cancelled: superseded by rereview")),
            None,
            "legacy plaintext is ambiguous, not a database error or exit proof"
        );
        assert_eq!(
            structured_cancel_reason(Some(r#"{"reason":"superseded_by_rereview"}"#)),
            Some("superseded_by_rereview".to_string())
        );
        assert!(
            stale_dispatch_turn_from_row(
                "legacy-plaintext".to_string(),
                Some((
                    "cancelled".to_string(),
                    Some("Cancelled: superseded by rereview".to_string()),
                )),
            )
            .is_none()
        );
    }

    #[test]
    fn retry_supersede_is_dropped_without_resurrecting_the_old_dispatch() {
        let kanban_source = include_str!("../../server/routes/kanban.rs");
        let retry_source = kanban_source
            .split_once("pub async fn retry_card")
            .expect("retry_card production path exists")
            .1
            .split_once("pub async fn redispatch_card")
            .expect("retry_card is followed by redispatch_card")
            .0;
        assert!(
            retry_source.contains("Some(crate::dispatch::SUPERSEDE_REASON_RETRY_CARD)"),
            "retry_card must persist supersede evidence before creating its replacement"
        );
        assert_eq!(
            stale_dispatch_queue_exit_kind(
                Some("cancelled"),
                Some(crate::dispatch::SUPERSEDE_REASON_RETRY_CARD),
            ),
            Some(QueueExitKind::Superseded)
        );
    }

    #[test]
    fn resume_supersede_drops_the_old_dispatch_before_replacement() {
        let resume_source = include_str!("../../server/routes/resume.rs");
        let cancel_and_clear_source = resume_source
            .split_once("fn cancel_and_clear")
            .expect("resume cancel_and_clear production path exists")
            .1
            .split_once("fn create_and_notify")
            .expect("resume cancel_and_clear is followed by create_and_notify")
            .0;
        assert!(
            cancel_and_clear_source.contains("Some(crate::dispatch::SUPERSEDE_REASON_RESUME)"),
            "resume must persist supersede evidence before creating its replacement"
        );
        assert_eq!(
            stale_dispatch_queue_exit_kind(
                Some("cancelled"),
                Some(crate::dispatch::SUPERSEDE_REASON_RESUME),
            ),
            Some(QueueExitKind::Superseded)
        );
    }

    #[test]
    fn dismissed_review_dispatch_is_dropped_without_resurrection() {
        let review_decision_source = include_str!("../review_decision.rs");
        let dismiss_cleanup_source = review_decision_source
            .split_once("pub async fn dismiss_review_cleanup")
            .expect("dismiss_review_cleanup production path exists")
            .1
            .split_once("// ── Review loopback request DTOs")
            .expect("dismiss cleanup is followed by review DTOs")
            .0;
        assert!(
            dismiss_cleanup_source
                .contains("Some(crate::dispatch::USER_CANCEL_REASON_REVIEW_DISMISS)"),
            "review dismiss must persist explicit user-cancel evidence"
        );
        assert_eq!(
            stale_dispatch_queue_exit_kind(
                Some("cancelled"),
                Some(crate::dispatch::USER_CANCEL_REASON_REVIEW_DISMISS),
            ),
            Some(QueueExitKind::Cancelled)
        );
    }

    #[test]
    fn system_cleanup_cancelled_dispatches_are_dropped_without_resurrection() {
        let transition_source = include_str!("../../engine/transition_executor_pg.rs");
        assert_eq!(
            transition_source
                .matches("Some(crate::dispatch::SYSTEM_CANCEL_REASON_TERMINAL_CARD)")
                .count(),
            1,
            "terminal-card transition cleanup must persist recognized system evidence"
        );
        assert_eq!(
            transition_source
                .matches("Some(crate::dispatch::SYSTEM_CANCEL_REASON_TRANSITION_INTENT)")
                .count(),
            1,
            "transition-intent cleanup must persist recognized system evidence"
        );

        let terminal_cleanup_source = include_str!("../../kanban/terminal_cleanup.rs");
        assert!(
            terminal_cleanup_source
                .contains("Some(crate::dispatch::SYSTEM_CANCEL_REASON_TERMINAL_CARD)"),
            "kanban terminal cleanup must persist recognized system evidence"
        );

        let dispute_source = include_str!("../review_decision/dispute.rs");
        assert!(
            dispute_source.contains("crate::dispatch::SYSTEM_CANCEL_REASON_SCOPE_MISMATCH_CLOSED"),
            "scope-mismatch close must persist recognized system close evidence"
        );

        for reason in [
            crate::dispatch::SYSTEM_CANCEL_REASON_TERMINAL_CARD,
            crate::dispatch::SYSTEM_CANCEL_REASON_TRANSITION_INTENT,
            crate::dispatch::SYSTEM_CANCEL_REASON_SCOPE_MISMATCH_CLOSED,
        ] {
            assert_eq!(
                stale_dispatch_queue_exit_kind(Some("cancelled"), Some(reason)),
                Some(QueueExitKind::Cancelled),
                "recognized system cleanup {reason} must drop its stale queued envelope"
            );
        }
    }

    #[test]
    fn redispatch_supersede_drops_the_old_dispatch_before_replacement() {
        let kanban_source = include_str!("../../server/routes/kanban.rs");
        let redispatch_source = kanban_source
            .split_once("pub async fn redispatch_card")
            .expect("redispatch_card production path exists")
            .1
            .split_once("pub async fn defer_dod")
            .expect("redispatch_card is followed by defer_dod")
            .0;
        assert!(
            redispatch_source.contains("Some(crate::dispatch::SUPERSEDE_REASON_REDISPATCH_CARD)"),
            "redispatch_card must persist supersede evidence before creating its replacement"
        );
        assert_eq!(
            stale_dispatch_queue_exit_kind(
                Some("cancelled"),
                Some(crate::dispatch::SUPERSEDE_REASON_REDISPATCH_CARD),
            ),
            Some(QueueExitKind::Superseded)
        );
    }

    #[test]
    fn explicit_user_cancel_is_dropped_without_resurrection() {
        let queue_source = include_str!("../queue.rs");
        assert_eq!(
            queue_source
                .matches("Some(crate::dispatch::USER_CANCEL_REASON_QUEUE_API)")
                .count(),
            3,
            "cancel_dispatch, cancel_all_dispatches, and cancel_turn must persist user-cancel evidence"
        );
        assert_eq!(
            stale_dispatch_queue_exit_kind(
                Some("cancelled"),
                Some(crate::dispatch::USER_CANCEL_REASON_QUEUE_API),
            ),
            Some(QueueExitKind::Cancelled)
        );
    }

    #[test]
    fn superseded_word_outside_a_structured_reason_prefix_is_not_exit_evidence() {
        assert_eq!(
            stale_dispatch_queue_exit_kind(
                Some("cancelled"),
                Some("ordinary payload discussing a superseded prior attempt"),
            ),
            None
        );
        assert_eq!(
            stale_dispatch_queue_exit_kind(
                Some("completed"),
                Some(crate::dispatch::SUPERSEDE_REASON_RETRY_CARD),
            ),
            None
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
