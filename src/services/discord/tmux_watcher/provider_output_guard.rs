//! Provider-output guard for the watcher raw rollover edit seam.

use super::StreamingStatusTickContext;
use crate::services::provider_output_guard::{
    ProviderOutputVerdict, inspect_provider_streaming_rollover, safe_blocked_body,
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
}
