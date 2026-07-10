//! Provider-output guard for the watcher raw rollover edit seam.

use super::StreamingStatusTickContext;
use crate::services::provider_output_guard::{
    ProviderOutputVerdict, inspect_provider_streaming_output, inspect_provider_streaming_rollover,
    safe_blocked_body,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WatcherRolloverAction<'a> {
    SendRaw(&'a str),
    Hold,
    SendSafe(&'static str),
}

fn watcher_rollover_action<'a>(
    provider: &crate::services::provider::ProviderKind,
    unsent_response: &str,
    frozen_chunk: &'a str,
) -> WatcherRolloverAction<'a> {
    match inspect_provider_streaming_rollover(provider, unsent_response, frozen_chunk) {
        ProviderOutputVerdict::Clean => WatcherRolloverAction::SendRaw(frozen_chunk),
        ProviderOutputVerdict::Hold { kind } => {
            tracing::warn!(
                provider = provider.as_str(),
                verdict = "hold",
                kind = kind.as_str(),
                output_bytes = frozen_chunk.len(),
                output_chars = frozen_chunk.chars().count(),
                "held watcher streaming rollover frame"
            );
            WatcherRolloverAction::Hold
        }
        ProviderOutputVerdict::Blocked { kind } => {
            tracing::warn!(
                provider = provider.as_str(),
                verdict = "blocked",
                kind = kind.as_str(),
                output_bytes = frozen_chunk.len(),
                output_chars = frozen_chunk.chars().count(),
                "blocked watcher streaming rollover frame"
            );
            WatcherRolloverAction::SendSafe(safe_blocked_body(kind))
        }
    }
}

/// Select the only payload that the common (non-rollover) watcher tick may
/// hand to either its edit-existing or send-initial branch. `false` means hold
/// the partial frame without touching Discord or advancing local display state.
pub(super) fn guard_streaming_frame(
    provider: &crate::services::provider::ProviderKind,
    unsent_response: &str,
    display_text: &mut String,
) -> bool {
    match inspect_provider_streaming_output(provider, unsent_response) {
        ProviderOutputVerdict::Clean => true,
        ProviderOutputVerdict::Hold { kind } => {
            tracing::warn!(
                provider = provider.as_str(),
                verdict = "hold",
                kind = kind.as_str(),
                output_bytes = unsent_response.len(),
                output_chars = unsent_response.chars().count(),
                "held watcher common streaming frame"
            );
            false
        }
        ProviderOutputVerdict::Blocked { kind } => {
            tracing::warn!(
                provider = provider.as_str(),
                verdict = "blocked",
                kind = kind.as_str(),
                output_bytes = unsent_response.len(),
                output_chars = unsent_response.chars().count(),
                "blocked watcher common streaming frame"
            );
            display_text.clear();
            display_text.push_str(safe_blocked_body(kind));
            true
        }
    }
}

pub(super) async fn guard_rollover(
    ctx: &StreamingStatusTickContext<'_>,
    message_id: serenity::all::MessageId,
    unsent_response: &str,
    frozen_chunk: &str,
) -> bool {
    match watcher_rollover_action(ctx.watcher_provider, unsent_response, frozen_chunk) {
        WatcherRolloverAction::SendRaw(selected) => {
            debug_assert_eq!(selected, frozen_chunk);
            true
        }
        WatcherRolloverAction::Hold => false,
        WatcherRolloverAction::SendSafe(body) => {
            super::rate_limit_wait(ctx.shared, ctx.channel_id).await;
            let _ = crate::services::discord::http::edit_channel_message(
                ctx.http,
                ctx.channel_id,
                message_id,
                body,
            )
            .await;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider::ProviderKind;

    #[test]
    fn invariant_4371_watcher_rollover_never_selects_raw_control_data() {
        let blocked =
            "prefix [SYSTEM NOTIFICATION - NOT USER INPUT] <output-file>/private/x</output-file>";
        let action = watcher_rollover_action(&ProviderKind::Claude, blocked, blocked);
        assert_eq!(
            action,
            WatcherRolloverAction::SendSafe(
                crate::services::provider_output_guard::BLOCKED_PROVIDER_OUTPUT_BODY
            )
        );
        let WatcherRolloverAction::SendSafe(body) = action else {
            panic!("blocked frame selected raw delivery");
        };
        assert!(!body.contains("<output-file>"));

        let partial = "safe prefix [SYSTEM NOTIF";
        assert_eq!(
            watcher_rollover_action(&ProviderKind::Claude, partial, partial),
            WatcherRolloverAction::Hold
        );
    }

    #[test]
    fn invariant_4371_watcher_common_frame_selects_one_safe_payload() {
        let blocked =
            "prefix [SYSTEM NOTIFICATION - NOT USER INPUT] <output-file>/private/x</output-file>";
        let mut selected = "RAW + footer".to_string();
        assert!(guard_streaming_frame(
            &ProviderKind::Claude,
            blocked,
            &mut selected
        ));
        assert_eq!(
            selected,
            crate::services::provider_output_guard::BLOCKED_PROVIDER_OUTPUT_BODY
        );
        assert!(!selected.contains("<output-file>"));

        assert_eq!(
            guard_streaming_frame(
                &ProviderKind::Claude,
                "safe prefix [SYSTEM NOTIF",
                &mut "RAW".to_string()
            ),
            false,
            "partial control markers must not touch edit or initial-send paths"
        );
        let mut clean = "normal + footer".to_string();
        assert!(guard_streaming_frame(
            &ProviderKind::Claude,
            "normal answer",
            &mut clean
        ));
        assert_eq!(clean, "normal + footer");
        let mut codex = "codex body".to_string();
        assert!(guard_streaming_frame(
            &ProviderKind::Codex,
            blocked,
            &mut codex
        ));
        assert_eq!(codex, "codex body");
    }

    #[test]
    fn invariant_4371_common_edit_and_initial_send_share_guarded_payload() {
        let source = include_str!("streaming_status_tick.rs");
        let common_tick = source
            .split("let mut display_text = build_watcher_streaming_edit_text")
            .nth(1)
            .expect("common display-text seam");
        assert!(
            common_tick.contains("guard_streaming_frame"),
            "common tick must classify before either Discord write"
        );
        assert!(
            common_tick.contains("edit_channel_message(&http, channel_id, msg_id, &display_text)"),
            "existing-message edit must use the guarded selection"
        );
        assert!(
            common_tick.contains("send_channel_message(&http, channel_id, &display_text)"),
            "initial send must use the same guarded selection"
        );
    }
}
