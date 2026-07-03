use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::provider::ProviderKind;

/// Readiness inferred from scraping visible tmux pane chrome.
///
/// This is only constructible for providers/runtimes that do not have
/// structured on-disk JSONL turn state. Structured TUI sessions must use the
/// transcript-derived readiness/completion signal instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FallbackPaneReadiness {
    ready: bool,
}

impl FallbackPaneReadiness {
    pub(crate) fn from_pane_scrape(
        provider: &ProviderKind,
        runtime_kind: Option<RuntimeHandoffKind>,
        scrape: impl FnOnce() -> bool,
    ) -> Option<Self> {
        if !crate::services::tui_turn_state::pane_ready_fallback_allowed(provider, runtime_kind) {
            return None;
        }
        Some(Self { ready: scrape() })
    }

    pub(crate) fn is_ready(self) -> bool {
        self.ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_runtime_cannot_construct_fallback_pane_readiness() {
        assert_eq!(
            FallbackPaneReadiness::from_pane_scrape(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                || true,
            ),
            None
        );
        assert_eq!(
            FallbackPaneReadiness::from_pane_scrape(
                &ProviderKind::Codex,
                Some(RuntimeHandoffKind::CodexTui),
                || true,
            ),
            None
        );
    }

    #[test]
    fn non_structured_runtime_preserves_scraped_readiness_value() {
        let ready = FallbackPaneReadiness::from_pane_scrape(
            &ProviderKind::Claude,
            Some(RuntimeHandoffKind::LegacyTmuxWrapper),
            || true,
        );
        assert_eq!(ready.map(FallbackPaneReadiness::is_ready), Some(true));

        let busy = FallbackPaneReadiness::from_pane_scrape(&ProviderKind::Qwen, None, || false);
        assert_eq!(busy.map(FallbackPaneReadiness::is_ready), Some(false));
    }
}
