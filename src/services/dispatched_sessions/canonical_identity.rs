use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::services::discord::session_identity::SessionIdentity;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HookIdentityError {
    Partial,
    InvalidKind,
    InvalidTokenHash,
    InvalidChannelId,
    LocatorMismatch,
}

pub(crate) fn parse_hook_identity<'a>(
    session_key: &str,
    provider: &str,
    identity_kind: Option<&'a str>,
    discord_token_hash: Option<&'a str>,
    channel_id: Option<&'a str>,
) -> Result<Option<CanonicalSessionIdentity<'a>>, HookIdentityError> {
    let identity_fields = [identity_kind.is_some(), discord_token_hash.is_some()];
    if identity_fields.iter().all(|value| !value) {
        return Ok(None);
    }
    if !identity_fields.iter().all(|value| *value) || channel_id.is_none() {
        return Err(HookIdentityError::Partial);
    }

    let kind = SessionIdentityKind::parse(identity_kind.unwrap_or_default())
        .ok_or(HookIdentityError::InvalidKind)?;
    let discord_token_hash = discord_token_hash
        .map(str::trim)
        .filter(|value| valid_discord_token_hash(value))
        .ok_or(HookIdentityError::InvalidTokenHash)?;
    let channel_id = channel_id
        .map(str::trim)
        .filter(|value| valid_channel_id(value))
        .ok_or(HookIdentityError::InvalidChannelId)?;

    let locator = SessionIdentity::parse(session_key).ok_or(HookIdentityError::LocatorMismatch)?;
    if locator.provider_from_key.as_deref() != Some(provider)
        || locator.token_hash.as_deref() != Some(discord_token_hash)
    {
        return Err(HookIdentityError::LocatorMismatch);
    }

    Ok(Some(CanonicalSessionIdentity {
        kind,
        discord_token_hash,
        channel_id,
    }))
}

fn valid_discord_token_hash(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("discord_") else {
        return false;
    };
    hex.len() == 16
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_channel_id(value: &str) -> bool {
    value.parse::<u64>().is_ok_and(|id| id != 0)
}

#[cfg(test)]
mod tests {
    use super::{HookIdentityError, parse_hook_identity};
    use crate::db::dispatched_session_canonical_identity::SessionIdentityKind;

    const KEY: &str = "claude/discord_0123456789abcdef/host:AgentDesk-claude-sanitized-channel";

    #[test]
    fn mixed_version_body_without_identity_remains_legacy_compatible() {
        assert!(
            parse_hook_identity(KEY, "claude", None, None, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn partial_identity_fails_closed() {
        assert_eq!(
            parse_hook_identity(
                KEY,
                "claude",
                Some("discord_channel"),
                Some("discord_0123456789abcdef"),
                None,
            ),
            Err(HookIdentityError::Partial)
        );
    }

    #[test]
    fn provider_and_token_namespace_must_match_locator() {
        assert_eq!(
            parse_hook_identity(
                KEY,
                "codex",
                Some("discord_channel"),
                Some("discord_0123456789abcdef"),
                Some("123"),
            ),
            Err(HookIdentityError::LocatorMismatch)
        );
    }

    #[test]
    fn ordinary_channel_and_thread_use_the_exact_supplied_snowflake() {
        let ordinary = parse_hook_identity(
            KEY,
            "claude",
            Some("discord_channel"),
            Some("discord_0123456789abcdef"),
            Some("111"),
        )
        .unwrap()
        .unwrap();
        let thread = parse_hook_identity(
            KEY,
            "claude",
            Some("discord_channel"),
            Some("discord_0123456789abcdef"),
            Some("222"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(ordinary.channel_id, "111");
        assert_eq!(thread.channel_id, "222");
        assert_ne!(ordinary.channel_id, thread.channel_id);
    }

    #[test]
    fn scheduled_snapshot_kind_is_explicit_and_distinct() {
        let identity = parse_hook_identity(
            KEY,
            "claude",
            Some("scheduled_snapshot"),
            Some("discord_0123456789abcdef"),
            Some("111"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(identity.kind, SessionIdentityKind::ScheduledSnapshot);
    }

    #[test]
    fn malformed_hash_and_channel_are_rejected() {
        assert_eq!(
            parse_hook_identity(
                KEY,
                "claude",
                Some("discord_channel"),
                Some("raw-token"),
                Some("111"),
            ),
            Err(HookIdentityError::InvalidTokenHash)
        );
        assert_eq!(
            parse_hook_identity(
                KEY,
                "claude",
                Some("discord_channel"),
                Some("discord_0123456789abcdef"),
                Some("parent/thread"),
            ),
            Err(HookIdentityError::InvalidChannelId)
        );
    }
}
