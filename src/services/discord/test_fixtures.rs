//! Discord runtime test fixtures shared by the crate's tests.

use super::*;

#[cfg(all(test, unix))]
pub(crate) fn claim_cross_channel_tmux_watcher_for_high_risk_test(
    requested_channel_id: ChannelId,
    existing_channel_id: ChannelId,
    thread_parent_channel_id: Option<ChannelId>,
) {
    tmux::claim_cross_channel_tmux_watcher_for_test(
        requested_channel_id,
        existing_channel_id,
        thread_parent_channel_id,
    );
}

#[cfg(all(test, unix))]
pub(crate) fn input_mailbox_for_test(
    shared: &SharedData,
    channel: u64,
) -> crate::services::turn_orchestrator::ChannelMailboxHandle {
    shared.mailbox(ChannelId::new(channel))
}

#[cfg(all(test, unix))]
pub(crate) async fn gateway_context_for_test(
    http: Arc<serenity::Http>,
    gateway: String,
) -> serenity::Context {
    tui_prompt_relay::relay_e2e::discord_mock::serenity_context_over(http, gateway).await
}

#[cfg(all(test, unix))]
pub(crate) fn ready_gateway_for_test(shared: &SharedData, context: serenity::Context) {
    shared.http.cached_serenity_ctx.set(context).unwrap();
    shared
        .http
        .cached_bot_token
        .set("test-token".into())
        .unwrap();
}

#[cfg(all(test, unix))]
pub(crate) struct RestGuardForTest(Option<shared_state::test_rest::Guard>);

#[cfg(all(test, unix))]
impl Drop for RestGuardForTest {
    fn drop(&mut self) {
        drop(self.0.take());
    }
}

#[cfg(all(test, unix))]
pub(crate) fn rest_for_test(http: Arc<serenity::Http>) -> RestGuardForTest {
    RestGuardForTest(Some(shared_state::test_rest::install(http)))
}

#[cfg(all(test, unix))]
pub(crate) fn input_queue_path_for_test(shared: &SharedData, channel: u64) -> std::path::PathBuf {
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(channel);
    let context = queue_dispatch::persistence_context(shared, &provider, channel);
    crate::services::turn_orchestrator::v2_queue(&context, channel)
        .unwrap()
        .0
}

#[cfg(test)]
pub(crate) fn register_resume_watcher_for_tests(
    shared: &SharedData,
    channel_id: ChannelId,
    tmux_session_name: &str,
) {
    shared.tmux_watchers.insert(
        channel_id,
        TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: format!("/runtime/{tmux_session_name}.jsonl"),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(
                std::sync::atomic::AtomicI64::new(tmux_watcher_now_ms()),
            ),
        },
    );
}

#[cfg(test)]
pub(crate) fn resume_owner_channel_for_tests(
    shared: &SharedData,
    tmux_session_name: &str,
) -> Option<ChannelId> {
    shared
        .tmux_watchers
        .owner_channel_for_tmux_session(tmux_session_name)
}

#[cfg(all(test, unix))]
pub(crate) async fn revoke_notice_author_for_test(shared: &SharedData) {
    shared.settings.write().await.allowed_user_ids.clear();
}
