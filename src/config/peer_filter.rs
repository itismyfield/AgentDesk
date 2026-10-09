//! `server.peer_filter`: how the HTTP surface treats socket peers outside
//! loopback, Tailscale and private-LAN ranges.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerFilterMode {
    /// No filter layer at all.
    Off,
    /// Warn about such peers but serve them unchanged.
    #[default]
    Log,
    /// Answer such peers with 403.
    Enforce,
}

impl PeerFilterMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Log => "log",
            Self::Enforce => "enforce",
        }
    }

    /// Keeps the default out of saved YAML so a later default change still applies.
    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::PeerFilterMode;
    use crate::config::Config;

    #[test]
    fn saved_config_omits_the_default_mode_and_keeps_an_explicit_one() {
        let default_yaml = serde_yaml::to_string(&Config::default()).unwrap();
        assert!(!default_yaml.contains("peer_filter"), "{default_yaml}");

        let parsed: Config = serde_yaml::from_str("server:\n  peer_filter: enforce\n").unwrap();
        assert_eq!(parsed.server.peer_filter, PeerFilterMode::Enforce);
        let saved = serde_yaml::to_string(&parsed).unwrap();
        let reparsed: Config = serde_yaml::from_str(&saved).unwrap();
        assert_eq!(reparsed.server.peer_filter, PeerFilterMode::Enforce);
        let missing: Config = serde_yaml::from_str("server:\n  port: 8791\n").unwrap();
        assert_eq!(missing.server.peer_filter, PeerFilterMode::Log);
    }
}
