//! A finished session transition wakes the owner of any request queued behind it.
use super::super::{ChannelId, SharedData};
use std::sync::Arc;

/// Declared before the transition guard, so every return releases that guard first.
pub(crate) struct SessionTransitionCompletion<'a> {
    shared: &'a Arc<SharedData>,
    channel: ChannelId,
}

impl<'a> SessionTransitionCompletion<'a> {
    pub(crate) fn new(shared: &'a Arc<SharedData>, channel: ChannelId) -> Self {
        Self { shared, channel }
    }
}

impl Drop for SessionTransitionCompletion<'_> {
    fn drop(&mut self) {
        #[cfg(not(unix))]
        let _ = (self.shared, self.channel);
        #[cfg(unix)]
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (shared, channel) = (self.shared.clone(), self.channel);
            runtime.spawn(async move { wake_installed_owner(&shared, channel).await });
        }
    }
}

#[cfg(unix)]
pub(in crate::services::discord) async fn wake_installed_owner(
    shared: &Arc<SharedData>,
    channel: ChannelId,
) {
    let Some(mailbox) = shared.mailbox_peek(channel) else {
        return;
    };
    if mailbox.v2_request(|slot| Ok(slot.is_some())).await == Ok(true) {
        super::message_handler::owner_wake(shared, channel);
    }
}

impl SharedData {
    pub(crate) fn session_transition_completion(
        self: &Arc<Self>,
        channel: ChannelId,
    ) -> SessionTransitionCompletion<'_> {
        SessionTransitionCompletion::new(self, channel)
    }
}
