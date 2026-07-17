use std::fmt;
use std::str::FromStr;

/// Stable semantic roles for AgentDesk's utility Discord bots.
///
/// The public alias is deliberately defined only in [`Self::alias`]. Renaming a
/// utility bot therefore keeps role-based call sites intact and makes new roles
/// compiler-visible through exhaustive matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UtilityBotRole {
    /// Sends actionable messages that may start an agent turn.
    Announce = 0,
    /// Sends informational messages that agents must not ingest as turns.
    Notify = 1,
}

impl UtilityBotRole {
    pub const ALL: [Self; 2] = [Self::Announce, Self::Notify];

    pub const fn alias(self) -> &'static str {
        match self {
            Self::Announce => "announce",
            Self::Notify => "notify",
        }
    }

    pub fn from_alias(alias: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.alias() == alias)
    }

    pub const fn credential_label(self) -> &'static str {
        match self {
            Self::Announce => "credential/announce_bot_token",
            Self::Notify => "credential/notify_bot_token",
        }
    }

    pub const fn log_emoji(self) -> &'static str {
        match self {
            Self::Announce => "📢",
            Self::Notify => "🔔",
        }
    }

    pub const fn uses_attachment_for_oversize(self) -> bool {
        match self {
            Self::Announce => true,
            Self::Notify => false,
        }
    }
}

impl fmt::Display for UtilityBotRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.alias())
    }
}

impl FromStr for UtilityBotRole {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_alias(value).ok_or(())
    }
}

#[cfg(test)]
mod tests {
    use super::UtilityBotRole;

    #[test]
    fn utility_bot_role_aliases_round_trip_and_are_unique() {
        let aliases = UtilityBotRole::ALL.map(UtilityBotRole::alias);
        assert_eq!(aliases, ["announce", "notify"]);
        assert_ne!(aliases[0], aliases[1]);

        for role in UtilityBotRole::ALL {
            assert_eq!(UtilityBotRole::from_alias(role.alias()), Some(role));
            assert_eq!(role.alias().parse(), Ok(role));
        }
        assert_eq!(UtilityBotRole::from_alias("unknown"), None);
        assert_eq!(UtilityBotRole::from_alias(" notify "), None);
    }

    #[test]
    fn utility_bot_role_metadata_is_exhaustive() {
        let metadata = UtilityBotRole::ALL.map(|role| {
            (
                role.alias(),
                role.credential_label(),
                role.log_emoji(),
                role.uses_attachment_for_oversize(),
            )
        });
        assert_eq!(
            metadata,
            [
                ("announce", "credential/announce_bot_token", "📢", true,),
                ("notify", "credential/notify_bot_token", "🔔", false),
            ]
        );
    }
}
