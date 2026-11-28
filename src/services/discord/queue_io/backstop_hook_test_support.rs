use super::{Arc, ChannelId};

/// Runs in a slow cycle between the owner check's answer and the cycle acting on it.
pub(in crate::services::discord) type OwnerAnsweredHook = Arc<
    dyn Fn(bool) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;
static OWNER_ANSWERED_HOOKS: std::sync::LazyLock<dashmap::DashMap<ChannelId, OwnerAnsweredHook>> =
    std::sync::LazyLock::new(Default::default);

pub(super) async fn owner_answered_for_tests(channel_id: ChannelId, owned: bool) {
    let hook = OWNER_ANSWERED_HOOKS
        .get(&channel_id)
        .map(|hook| hook.clone());
    if let Some(hook) = hook {
        hook(owned).await;
    }
}

pub(in crate::services::discord) fn set_owner_answered_hook(
    channel: ChannelId,
    hook: Option<OwnerAnsweredHook>,
) {
    if let Some(hook) = hook {
        OWNER_ANSWERED_HOOKS.insert(channel, hook);
    } else {
        OWNER_ANSWERED_HOOKS.remove(&channel);
    }
}
