//! Which failed sends of a queued hook request are sent again as the same request file.

use std::time::Duration;

use crate::services::claude_tui::hook_server::HookEventKind;

/// Pause before a failed transport is retried, so a receiver that is down is not spun on.
const TRANSPORT_RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// Whether a queued request is resent unchanged after `error`: a 425 always, and a Claude
/// SessionStart's failed transport or 502/503/504 after a backoff, being transition evidence.
pub(super) fn retries(provider: &str, event: &str, error: &str) -> bool {
    if error.contains("HTTP 425") {
        return true;
    }
    let start = HookEventKind::from_path(event) == HookEventKind::SessionStart;
    let start = provider == "claude" && start;
    let gateway = ["HTTP 502", "HTTP 503", "HTTP 504"];
    let transport =
        error.starts_with("post hook event:") || gateway.iter().any(|s| error.contains(s));
    #[cfg(test)]
    let (start, transport) = {
        use crate::services::claude_tui::source_verify::n2b_mutant;
        let start = (start && !n2b_mutant("drop")) || n2b_mutant("retry-any");
        (
            start,
            transport || (n2b_mutant("retry-500") && error.contains("HTTP 500")),
        )
    };
    if !(start && transport) {
        return false;
    }
    std::thread::sleep(TRANSPORT_RETRY_BACKOFF);
    true
}
