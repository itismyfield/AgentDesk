//! `server` surface settings that live outside the giant `config.rs`.
use serde::{Deserialize, Serialize};

use super::ServerConfig;

/// `server.browser_guard`: what happens to a request that fails the Host
/// allowlist, the cross-site rule, or the JSON body type rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserGuardMode {
    /// Serve it unchanged and log one WARN line.
    #[default]
    Log,
    /// Reject it (403, or 415 for the body type rule).
    Enforce,
}

impl BrowserGuardMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Enforce => "enforce",
        }
    }

    /// Keeps the default out of saved YAML so a later default change still applies.
    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("port", &self.port)
            .field("host", &self.host)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "allow_insecure_nonloopback_bind",
                &self.allow_insecure_nonloopback_bind,
            )
            .field("peer_filter", &self.peer_filter)
            .field("browser_guard", &self.browser_guard)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::BrowserGuardMode;
    use crate::config::Config;

    #[test]
    fn browser_guard_defaults_to_log_and_saved_config_keeps_an_explicit_enforce() {
        let default_yaml = serde_yaml::to_string(&Config::default()).unwrap();
        assert!(!default_yaml.contains("browser_guard"), "{default_yaml}");
        let missing: Config = serde_yaml::from_str("server:\n  port: 8791\n").unwrap();
        assert_eq!(missing.server.browser_guard, BrowserGuardMode::Log);

        let parsed: Config = serde_yaml::from_str("server:\n  browser_guard: enforce\n").unwrap();
        assert_eq!(parsed.server.browser_guard, BrowserGuardMode::Enforce);
        let saved = serde_yaml::to_string(&parsed).unwrap();
        let reparsed: Config = serde_yaml::from_str(&saved).unwrap();
        assert_eq!(reparsed.server.browser_guard, BrowserGuardMode::Enforce);
        assert!(serde_yaml::from_str::<Config>("server:\n  browser_guard: off\n").is_err());
    }

    #[test]
    fn debug_still_redacts_the_token() {
        let mut config = Config::default();
        config.server.auth_token = Some("secret-token-value".into());
        let rendered = format!("{:?}", config.server);
        assert!(!rendered.contains("secret-token-value"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(rendered.contains("browser_guard: Log"), "{rendered}");
    }
}
