use crate::services::provider::ProviderKind;

pub(in crate::services::discord) const DISCORD_CHANNEL_IDENTITY_KIND: &str = "discord_channel";
pub(in crate::services::discord) const SCHEDULED_SNAPSHOT_IDENTITY_KIND: &str =
    "scheduled_snapshot";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct HookCanonicalIdentity<'a> {
    pub(in crate::services::discord) identity_kind: &'static str,
    pub(in crate::services::discord) discord_token_hash: &'a str,
}

pub(in crate::services::discord) fn identity_for_session_key<'a>(
    session_key: &str,
    provider: &ProviderKind,
    expected_token_hash: &'a str,
    scheduled_snapshot: bool,
) -> Option<HookCanonicalIdentity<'a>> {
    let parsed = super::session_identity::SessionIdentity::parse(session_key)?;
    if parsed.provider_from_key.as_deref() != Some(provider.as_str())
        || parsed.token_hash.as_deref() != Some(expected_token_hash)
    {
        return None;
    }
    Some(HookCanonicalIdentity {
        identity_kind: if scheduled_snapshot {
            SCHEDULED_SNAPSHOT_IDENTITY_KIND
        } else {
            DISCORD_CHANNEL_IDENTITY_KIND
        },
        discord_token_hash: expected_token_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        DISCORD_CHANNEL_IDENTITY_KIND, SCHEDULED_SNAPSHOT_IDENTITY_KIND, identity_for_session_key,
    };
    use crate::services::provider::ProviderKind;

    #[test]
    fn sanitize_and_truncate_collisions_do_not_change_canonical_dimensions() {
        let sanitized_a = ProviderKind::Claude.build_tmux_session_name("team/a");
        let sanitized_b = ProviderKind::Claude.build_tmux_session_name("team?a");
        assert_eq!(
            sanitized_a, sanitized_b,
            "fixture must collide after sanitizing"
        );

        let prefix = "x".repeat(44);
        let truncated_a = ProviderKind::Claude.build_tmux_session_name(&format!("{prefix}-one"));
        let truncated_b = ProviderKind::Claude.build_tmux_session_name(&format!("{prefix}-two"));
        assert_eq!(
            truncated_a, truncated_b,
            "fixture must collide after truncating"
        );

        let key = format!("claude/discord_0123456789abcdef/host:{sanitized_a}");
        let identity = identity_for_session_key(
            &key,
            &ProviderKind::Claude,
            "discord_0123456789abcdef",
            false,
        )
        .expect("namespaced locator matches");
        assert_eq!(identity.identity_kind, DISCORD_CHANNEL_IDENTITY_KIND);
        assert_eq!(identity.discord_token_hash, "discord_0123456789abcdef");

        // The exact channel/thread snowflake is a separate HookSessionBody field,
        // so both name collisions remain distinguishable by the canonical tuple.
        let first_channel_id = "111111111111111111";
        let second_channel_id = "222222222222222222";
        assert_ne!(first_channel_id, second_channel_id);
    }

    #[test]
    fn provider_and_token_are_independent_canonical_dimensions() {
        let key = "claude/discord_0123456789abcdef/host:AgentDesk-claude-general";
        assert!(
            identity_for_session_key(key, &ProviderKind::Codex, "discord_0123456789abcdef", false,)
                .is_none()
        );
        assert!(
            identity_for_session_key(
                key,
                &ProviderKind::Claude,
                "discord_fedcba9876543210",
                false,
            )
            .is_none()
        );
    }

    #[test]
    fn scheduled_snapshot_kind_is_explicit() {
        let key = "claude/discord_0123456789abcdef/host:AgentDesk-claude-scheduled";
        let identity =
            identity_for_session_key(key, &ProviderKind::Claude, "discord_0123456789abcdef", true)
                .expect("namespaced locator matches");
        assert_eq!(identity.identity_kind, SCHEDULED_SNAPSHOT_IDENTITY_KIND);
    }
}
