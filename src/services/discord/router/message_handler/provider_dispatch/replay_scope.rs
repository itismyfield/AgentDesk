//! Final driver selection separates non-TUI starts from the existing TUI input owner.
use crate::services::provider::ProviderKind;
use crate::services::provider_hosting::{self, ProviderSessionDriver};

pub(in crate::services::discord::router::message_handler) struct Scope {
    pub driver: ProviderSessionDriver,
    pub runtime: &'static str,
}

pub(in crate::services::discord::router::message_handler) fn selected(
    provider: &ProviderKind,
    channel: u64,
    remote: bool,
    tmux: Option<&str>,
) -> Scope {
    #[cfg(unix)]
    let capable = !remote && tmux.is_some() && crate::services::claude::is_tmux_available();
    #[cfg(not(unix))]
    let capable = false;
    let driver = provider_hosting::resolve_provider_session_selection_with_channel(
        provider,
        capable,
        Some(channel),
    )
    .driver;
    let runtime = if remote {
        "remote"
    } else if tmux.is_none() {
        "owned_direct"
    } else if !capable {
        "process_backend"
    } else if driver == ProviderSessionDriver::TuiHosting {
        "tui"
    } else {
        "legacy_tmux_wrapper"
    };
    Scope { driver, runtime }
}
