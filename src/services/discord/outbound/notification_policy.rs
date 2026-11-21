//! Push policy for `discord.secretary_channels`: suppressed posts are still delivered with the
//! silent flag; progress never pushes, unrequested notices stay silent in KST quiet hours.

use chrono::{DateTime, FixedOffset, Timelike, Utc};
use poise::serenity_prelude::{CreateMessage, MessageFlags};

use crate::services::tui_o::shadow::UnitKind;

/// Korea has no daylight saving time, so a fixed +09:00 offset is exact.
const KST_OFFSET_SECS: i32 = 9 * 60 * 60;
/// Quiet hours run from 23:00 (inclusive) to 06:00 (exclusive) KST.
const QUIET_START_HOUR: u32 = 23;
const QUIET_END_HOUR: u32 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoticeClass {
    /// Tool cards, input notices, recaps, lifecycle and watchdog notices.
    Progress,
    /// Notices nobody asked for, such as operational alerts.
    Unrequested,
    /// Turn replies: direct answers, decision questions and final results.
    Answer,
    /// Scheduled messages the user set up.
    Reminder,
}

pub(crate) fn outbox_class(source: &str, reason_code: Option<&str>) -> NoticeClass {
    match (source.trim(), reason_code.map(str::trim)) {
        ("headless_turn", _) => NoticeClass::Answer,
        ("scheduled_message", _) => NoticeClass::Reminder,
        ("lifecycle_notifier", Some("lifecycle.background_task_complete")) => {
            NoticeClass::Unrequested
        }
        (
            "lifecycle_notifier"
            | "long_turn_watchdog"
            | "stall_watchdog"
            | "relay_signal_rollup"
            | "queue_overflow_notice"
            | "retained_input_notice"
            | "imessage_input_notice"
            | "boot_custody_notice"
            | "adopt_fence_forward_notice"
            | "tui_o_alarm",
            _,
        ) => NoticeClass::Progress,
        _ => NoticeClass::Unrequested,
    }
}

pub(crate) fn unit_class(kind: UnitKind) -> NoticeClass {
    match kind {
        UnitKind::Body => NoticeClass::Answer,
        UnitKind::Tool | UnitKind::ToolResult | UnitKind::InputNotice | UnitKind::Recap => {
            NoticeClass::Progress
        }
    }
}

pub(crate) fn in_quiet_hours(now: DateTime<Utc>) -> bool {
    let offset = FixedOffset::east_opt(KST_OFFSET_SECS).expect("KST offset is valid");
    let hour = now.with_timezone(&offset).hour();
    hour >= QUIET_START_HOUR || hour < QUIET_END_HOUR
}

/// `channels` holds the configured secretary channel ids as written in the config.
pub(crate) fn suppresses_push(
    channels: &[String],
    channel: u64,
    class: NoticeClass,
    now: DateTime<Utc>,
) -> bool {
    if channel == 0
        || !channels
            .iter()
            .any(|id| id.trim().parse::<u64>().ok() == Some(channel))
    {
        return false;
    }
    match class {
        NoticeClass::Progress => true,
        NoticeClass::Unrequested => in_quiet_hours(now),
        NoticeClass::Answer | NoticeClass::Reminder => false,
    }
}

/// Reads the live config; without a configured secretary channel nothing changes.
pub(crate) fn live_suppresses_push(channel: u64, class: NoticeClass) -> bool {
    let Some(config) = crate::config_live_reload::current() else {
        return false;
    };
    let channels = &config.discord.secretary_channels;
    !channels.is_empty() && suppresses_push(channels, channel, class, Utc::now())
}

/// Accepts the outbox `channel:<id>` target form and a bare id.
pub(crate) fn live_suppresses_target_push(target: &str, class: NoticeClass) -> bool {
    let target = target.trim();
    target
        .strip_prefix("channel:")
        .unwrap_or(target)
        .trim()
        .parse::<u64>()
        .is_ok_and(|channel| live_suppresses_push(channel, class))
}

pub(crate) fn silence(message: CreateMessage, suppress_push: bool) -> CreateMessage {
    if suppress_push {
        message.flags(MessageFlags::SUPPRESS_NOTIFICATIONS)
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn kst(hour: u32, minute: u32) -> DateTime<Utc> {
        FixedOffset::east_opt(KST_OFFSET_SECS)
            .unwrap()
            .with_ymd_and_hms(2026, 11, 21, hour, minute, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn secretary_channel_push_follows_class_and_kst_quiet_hours() {
        let channels = vec!["42".to_string()];
        let progress = outbox_class("lifecycle_notifier", Some("lifecycle.context_compacted"));
        let alert = outbox_class("outbox_delivery_alert", Some("outbox_delivery_failed"));
        let answer = outbox_class("headless_turn", Some("headless.delivery"));
        let reminder = outbox_class("scheduled_message", None);
        let (tool, body) = (unit_class(UnitKind::Tool), unit_class(UnitKind::Body));
        // (time, class, suppressed in channel 42)
        for (hour, minute, class, suppressed) in [
            (12, 0, progress, true),
            (12, 0, tool, true),
            (12, 0, body, false),
            (12, 0, answer, false),
            (22, 59, alert, false),
            (23, 0, alert, true),
            (5, 59, alert, true),
            (6, 0, alert, false),
            (23, 0, answer, false),
            (23, 0, body, false),
            (3, 0, reminder, false),
        ] {
            assert_eq!(
                suppresses_push(&channels, 42, class, kst(hour, minute)),
                suppressed,
                "{hour:02}:{minute:02} {class:?}"
            );
            assert!(!suppresses_push(&channels, 43, class, kst(hour, minute)));
            assert!(!suppresses_push(&[], 42, class, kst(hour, minute)));
        }
    }
}
