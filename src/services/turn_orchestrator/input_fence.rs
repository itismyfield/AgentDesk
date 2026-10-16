//! Actor-side freeze barrier for protected channels.
use super::*;
use crate::services::discord::input_runtime::fence::{self, Closing, Failure};
use std::io;
pub(crate) struct FreezeAck {
    channel: u64,
    provider: ProviderKind,
    closing: Arc<Closing>,
}
impl FreezeAck {
    pub(crate) fn channel(&self) -> u64 {
        self.channel
    }
    pub(crate) fn matches(&self, closing: &Closing) -> bool {
        std::ptr::eq(self.closing.as_ref(), closing)
    }
    pub(crate) fn provider(&self) -> &ProviderKind {
        &self.provider
    }
}
impl ChannelMailboxHandle {
    pub(crate) async fn freeze_input(
        &self,
        closing: Arc<Closing>,
        persistence: QueuePersistenceContext,
    ) -> Result<FreezeAck, Failure> {
        self.request(|reply| ChannelMailboxMsg::FreezeInput {
            closing,
            persistence,
            reply,
        })
        .await
        .map_err(|_| Failure::ActorUnreachable)?
    }
}
pub(super) fn freeze(
    state: &mut ChannelMailboxState,
    channel: ChannelId,
    closing: &Arc<Closing>,
    persistence: &QueuePersistenceContext,
) -> Result<FreezeAck, Failure> {
    if closing.channel() != channel.get()
        || closing.provider() != &persistence.provider
        || state.closed
        || state.cancel_token.is_some()
        || state.pending_user_dispatch.is_some()
        || state.pending_user_dispatch_lease.is_some()
    {
        return Err(Failure::Busy);
    }
    fence::require_worker()?;
    let root = fence::population_root().ok_or(Failure::Persistence)?;
    let guard = closing.population(&root)?;
    let tokens = root
        .join("discord_pending_queue")
        .join(persistence.provider.as_str());
    match std::fs::read_dir(&tokens) {
        Ok(entries) => {
            for entry in entries {
                let token = entry.map_err(|_| Failure::Persistence)?.path();
                if token.file_name().and_then(|n| n.to_str()) == Some(&persistence.token_hash) {
                    continue;
                }
                for extension in ["json", "dispatch"] {
                    match std::fs::symlink_metadata(token.join(format!(
                        "{}.{}",
                        channel.get(),
                        extension
                    ))) {
                        Ok(_) => return Err(Failure::Busy),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(_) => return Err(Failure::Persistence),
                    }
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(Failure::Persistence),
    }
    let _scope = fence::PopulationScope::hold(guard);
    super::pending_queue_persistence::load_channel_pending_queue_checked(
        &persistence.provider,
        &persistence.token_hash,
        channel,
    )
    .map_err(|_| Failure::Persistence)?;
    let marker = root
        .join("discord_pending_queue")
        .join(persistence.provider.as_str())
        .join(&persistence.token_hash)
        .join(format!("{}.dispatch", channel.get()));
    match std::fs::read(marker) {
        Ok(bytes) => {
            serde_json::from_slice::<super::pending_queue_persistence::PendingQueueItem>(&bytes)
                .map_err(|_| Failure::Persistence)?;
            return Err(Failure::Busy);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(Failure::Persistence),
    }
    let result = hydrate_pending_queue_from_disk_if_present(state, channel, persistence)
        .persistence_error
        .map_or_else(|| persist_queue(state, channel, persistence), Err)
        .map_err(|_| Failure::Persistence);
    if state.pending_user_dispatch.is_some() || state.pending_user_dispatch_lease.is_some() {
        return Err(Failure::Busy);
    }
    result.map(|_| FreezeAck {
        channel: channel.get(),
        provider: persistence.provider.clone(),
        closing: closing.clone(),
    })
}
/// Protected and not LegacyOpen: restart drains and markers leave the channel as it is.
pub(crate) fn held(provider: &ProviderKind, channel: u64) -> bool {
    fence::lookup(provider, channel).is_some_and(|gate| gate.mode() != fence::Mode::LegacyOpen)
}
#[cfg(test)]
#[path = "input_fence_tests.rs"]
mod tests;
