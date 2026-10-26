//! Process configuration selects new producers; durable consumers do not depend on it.
use crate::services::{provider::ProviderKind, provider_hosting::ProviderSessionDriver};
use std::{collections::BTreeSet, sync::LazyLock};

#[derive(Debug, Default)]
pub(crate) struct Settings {
    channels: BTreeSet<u64>,
}
impl Settings {
    pub(crate) fn parse(flag: Option<&str>, channels: Option<&str>) -> Self {
        if !flag.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true")) {
            return Self::default();
        }
        let Some(channels) = channels.filter(|v| !v.trim().is_empty()) else {
            return Self::default();
        };
        let parsed: Option<BTreeSet<u64>> = channels
            .split(',')
            .map(|s| {
                let s = s.trim();
                if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                s.parse().ok().filter(|id| *id != 0)
            })
            .collect();
        Self {
            channels: parsed.unwrap_or_default(),
        }
    }
    pub(crate) fn selected(&self, channel: &str) -> bool {
        channel
            .parse::<u64>()
            .is_ok_and(|id| self.channels.contains(&id))
    }
}
static PROCESS: LazyLock<Settings> = LazyLock::new(|| {
    Settings::parse(
        std::env::var("AGENTDESK_STALE_REPLAY_V1").ok().as_deref(),
        std::env::var("AGENTDESK_STALE_REPLAY_CHANNELS")
            .ok()
            .as_deref(),
    )
});

pub(crate) fn requested(channel: &str) -> bool {
    #[cfg(test)]
    if crate::services::discord::router::replay_test_activation::active(channel) {
        return true;
    }
    production_requested(channel)
}

pub(crate) fn production_requested(channel: &str) -> bool {
    PROCESS.selected(channel)
}

pub(crate) fn supported(provider: &str, driver: &str, runtime: &str) -> bool {
    runtime == "owned_direct"
        && driver == "legacy-prompt"
        && ProviderKind::from_str(provider).is_some_and(|p| {
            super::live::can_activate(p, ProviderSessionDriver::LegacyPrompt, None)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn p5_activation_parser_is_closed_and_channel_scoped() {
        for flag in [None, Some("0"), Some("false"), Some("invalid"), Some("")] {
            assert!(!Settings::parse(flag, Some("42")).selected("42"));
        }
        for channels in [
            None,
            Some(""),
            Some("42,"),
            Some("0"),
            Some("42,wrong"),
            Some("-1"),
            Some("18446744073709551616"),
        ] {
            assert!(!Settings::parse(Some("1"), channels).selected("42"));
        }
        let s = Settings::parse(Some("true"), Some("42, 43,42"));
        assert!(s.selected("42") && s.selected("43"));
        assert!(!s.selected("44"));
    }
    #[test]
    fn p5_activation_runtime_matrix_is_closed() {
        for p in ["claude", "codex", "qwen", "gemini", "unknown"] {
            for d in ["legacy-prompt", "tui-hosting", ""] {
                for r in [
                    "owned_direct",
                    "process_backend",
                    "legacy_tmux_wrapper",
                    "remote",
                    "tui",
                    "",
                ] {
                    assert_eq!(
                        supported(p, d, r),
                        matches!(p, "claude" | "codex")
                            && d == "legacy-prompt"
                            && r == "owned_direct",
                        "{p}/{d}/{r}"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
static READS: LazyLock<std::sync::Mutex<std::collections::HashMap<String, u64>>> =
    LazyLock::new(Default::default);
#[cfg(test)]
pub(crate) fn record_read(key: &str) {
    *READS.lock().unwrap().entry(key.into()).or_default() += 1;
}
#[cfg(test)]
pub(crate) fn reads(key: &str) -> u64 {
    *READS.lock().unwrap().get(key).unwrap_or(&0)
}
