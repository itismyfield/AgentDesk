use super::{Arc, ChannelId, IdleQueueKickoffChannelOutcome, ProviderKind, SharedData};

pub(in crate::services::discord) type IdleQueueKickHookForTests = std::sync::Arc<
    dyn Fn(
            Arc<SharedData>,
            ProviderKind,
            ChannelId,
            &'static str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Option<IdleQueueKickoffChannelOutcome>> + Send>,
        > + Send
        + Sync,
>;

static IDLE_QUEUE_KICK_HOOK_FOR_TESTS: std::sync::Mutex<Option<IdleQueueKickHookForTests>> =
    std::sync::Mutex::new(None);

pub(in crate::services::discord) struct IdleQueueKickHookResetForTests;

impl Drop for IdleQueueKickHookResetForTests {
    fn drop(&mut self) {
        *IDLE_QUEUE_KICK_HOOK_FOR_TESTS
            .lock()
            .expect("idle queue kick hook lock") = None;
    }
}

pub(in crate::services::discord) fn set_idle_queue_kick_hook_for_tests(
    hook: IdleQueueKickHookForTests,
) -> IdleQueueKickHookResetForTests {
    *IDLE_QUEUE_KICK_HOOK_FOR_TESTS
        .lock()
        .expect("idle queue kick hook lock") = Some(hook);
    IdleQueueKickHookResetForTests
}

pub(super) async fn idle_queue_kick_hook_outcome_for_tests(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
) -> Option<IdleQueueKickoffChannelOutcome> {
    let hook = IDLE_QUEUE_KICK_HOOK_FOR_TESTS
        .lock()
        .expect("idle queue kick hook lock")
        .clone();
    hook?(shared, provider, channel_id, reason).await
}
