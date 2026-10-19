use super::*;

/// Marks the watcher's parsed terminal for the observation snapshot.
pub(in crate::services::discord) async fn terminal_checkpoint(
    provider: &ProviderKind,
    channel: u64,
) {
    record_event(provider, channel, "parsed_terminal");
}
