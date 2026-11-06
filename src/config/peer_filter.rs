//! `server.peer_filter`: how the HTTP surface treats socket peers outside
//! loopback and Tailscale ranges.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerFilterMode {
    /// No filter layer at all.
    Off,
    /// Warn about such peers but serve them unchanged.
    Log,
    /// Answer such peers, private LAN included, with 403.
    /// `enforce` is its former name and is still accepted.
    #[default]
    #[serde(alias = "enforce")]
    TailnetOnly,
}

impl PeerFilterMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Log => "log",
            Self::TailnetOnly => "tailnet_only",
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
        let missing: Config = serde_yaml::from_str("server:\n  port: 8791\n").unwrap();
        assert_eq!(missing.server.peer_filter, PeerFilterMode::TailnetOnly);

        // `enforce` is the value deployed configs already spell out; it must stay the strict mode.
        for value in ["tailnet_only", "enforce"] {
            let parsed: Config =
                serde_yaml::from_str(&format!("server:\n  peer_filter: {value}\n")).unwrap();
            assert_eq!(
                parsed.server.peer_filter,
                PeerFilterMode::TailnetOnly,
                "{value}"
            );
            let saved = serde_yaml::to_string(&parsed).unwrap();
            assert!(!saved.contains("peer_filter"), "{value}: {saved}");
        }
        for (value, mode) in [("log", PeerFilterMode::Log), ("off", PeerFilterMode::Off)] {
            let parsed: Config =
                serde_yaml::from_str(&format!("server:\n  peer_filter: {value}\n")).unwrap();
            assert_eq!(parsed.server.peer_filter, mode, "{value}");
            let saved = serde_yaml::to_string(&parsed).unwrap();
            assert!(saved.contains(&format!("peer_filter: {value}")), "{saved}");
            let reparsed: Config = serde_yaml::from_str(&saved).unwrap();
            assert_eq!(reparsed.server.peer_filter, mode, "{value}");
        }
        for value in [
            "Enforce",
            "TAILNET_ONLY",
            "tailnet-only",
            "'enforce '",
            "deny",
            "''",
        ] {
            let parsed =
                serde_yaml::from_str::<Config>(&format!("server:\n  peer_filter: {value}\n"));
            assert!(parsed.is_err(), "{value} must not parse");
        }
    }
}
