//! Safe fallback for transient invalid or missing on-disk configuration.
use super::Config;

pub(super) fn graceful_fallback_config(path_display: &str, reason: String) -> Config {
    if let Some(live) = crate::config_live_reload::current() {
        tracing::warn!(
            "  ⚠ {reason} — keeping the last validated live config (source: {path_display})"
        );
        return (*live).clone();
    }

    tracing::warn!("  ⚠ {reason} — using defaults");
    Config::default()
}
