use poise::serenity_prelude::ChannelId;

use crate::services::discord::settings;
use crate::services::memory::{
    AutoRememberExecutionResult, AutoRememberTurnRequest, CaptureRequest, ReflectRequest,
    TokenUsage, build_resolved_memory_backend, run_auto_remember,
};

#[derive(Debug)]
pub(super) enum TurnEndMemoryJob {
    Capture(CaptureRequest),
    Reflect(ReflectRequest),
    AutoRemember(AutoRememberTurnRequest),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct MemoryPostprocessResult {
    pub(super) token_usage: TokenUsage,
}

pub(super) fn spawn_memory_postprocess_task(
    channel_id: ChannelId,
    memory_settings: settings::ResolvedMemorySettings,
    jobs: Vec<TurnEndMemoryJob>,
) -> tokio::task::JoinHandle<MemoryPostprocessResult> {
    tokio::spawn(async move {
        let backend = build_resolved_memory_backend(&memory_settings);
        let mut result = MemoryPostprocessResult::default();

        for job in jobs {
            match job {
                TurnEndMemoryJob::Capture(capture_request) => {
                    let capture_result = backend.capture(capture_request).await;
                    for warning in &capture_result.warnings {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] [memory] capture warning for channel {}: {}",
                            channel_id.get(),
                            warning
                        );
                    }
                    result
                        .token_usage
                        .saturating_add_assign(capture_result.token_usage);
                }
                TurnEndMemoryJob::Reflect(reflect_request) => {
                    let reason = reflect_request.reason.as_str().to_string();
                    let reflect_result = backend.reflect(reflect_request).await;
                    for warning in &reflect_result.warnings {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] [memory] reflect warning for channel {} ({}): {}",
                            channel_id.get(),
                            reason,
                            warning
                        );
                    }
                    result
                        .token_usage
                        .saturating_add_assign(reflect_result.token_usage);
                }
                TurnEndMemoryJob::AutoRemember(auto_request) => {
                    let auto_result: AutoRememberExecutionResult =
                        run_auto_remember(&memory_settings, auto_request).await;
                    for warning in &auto_result.warnings {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] [memory] auto-remember warning for channel {}: {}",
                            channel_id.get(),
                            warning
                        );
                    }
                    result
                        .token_usage
                        .saturating_add_assign(auto_result.token_usage);
                }
            }
        }

        result
    })
}
