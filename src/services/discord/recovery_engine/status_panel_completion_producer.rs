use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn complete_recovery_status_panel_with_sniffer<S, SniffFuture>(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &super::super::inflight::InflightTurnState,
    channel_id: ChannelId,
    status_msg_id: Option<MessageId>,
    started_at_unix: i64,
    background: bool,
    source: &'static str,
    sniff_background_agent_pending: S,
) -> RecoveryCompletionOutcome
where
    S: FnOnce(Option<String>) -> SniffFuture,
    SniffFuture: std::future::Future<Output = bool>,
{
    complete_recovery_status_panel_with_sniffer_and_sink(
        state,
        sniff_background_agent_pending,
        |background_agent_pending| async move {
            let mut last_status_panel_text = String::new();
            super::super::turn_bridge::complete_status_panel_v2_with_http(
                shared,
                http,
                channel_id,
                status_msg_id,
                provider,
                started_at_unix,
                &mut last_status_panel_text,
                background,
                background_agent_pending,
                source,
                (Some(state.user_msg_id), Some(state)),
            )
            .await
        },
    )
    .await
}

pub(super) async fn complete_recovery_status_panel_with_sniffer_and_sink<
    S,
    SniffFuture,
    C,
    CompleteFuture,
>(
    state: &super::super::inflight::InflightTurnState,
    sniff_background_agent_pending: S,
    complete_status_panel: C,
) -> RecoveryCompletionOutcome
where
    S: FnOnce(Option<String>) -> SniffFuture,
    SniffFuture: std::future::Future<Output = bool>,
    C: FnOnce(bool) -> CompleteFuture,
    CompleteFuture: std::future::Future<Output = bool>,
{
    let background_agent_pending =
        sniff_background_agent_pending(state.tmux_session_name.clone()).await;
    let _committed = complete_status_panel(background_agent_pending).await;
    RecoveryCompletionOutcome::Emitted
}
