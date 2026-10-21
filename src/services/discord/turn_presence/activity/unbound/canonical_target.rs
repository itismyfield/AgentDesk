//! Read-only canonical locator evidence for a binding-only local target.
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind, resolve_session_row_pg,
};
use crate::services::discord::{adk_session, session_identity::SessionIdentity};
use crate::services::provider::ProviderKind;
use sqlx::{PgPool, Row};

#[derive(Clone, Debug, PartialEq, Eq)]
struct RowWitness {
    id: i64,
    session_key: Option<String>,
    provider: Option<String>,
    identity_kind: Option<String>,
    discord_token_hash: Option<String>,
    channel_id: Option<String>,
    status: Option<String>,
    active_turn_nonce: Option<String>,
    hosted_execution: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CanonicalTarget {
    pub(super) name: String,
    canonical: (i64, String),
    exact: (i64, String),
    row: RowWitness,
}

pub(super) async fn collect(
    pool: &PgPool,
    provider: &ProviderKind,
    token_hash: &str,
    channel: u64,
) -> Result<CanonicalTarget, String> {
    let channel_text = channel.to_string();
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: token_hash,
        channel_id: &channel_text,
    };
    let canonical = resolve_session_row_pg(pool, None, Some(provider.as_str()), Some(identity))
        .await
        .map_err(|_| "canonical locator lookup failed")?
        .ok_or("canonical locator missing")?;
    let parsed = SessionIdentity::parse(&canonical.1).ok_or("canonical primary malformed")?;
    if !parsed.namespaced
        || parsed.provider_from_key.as_deref() != Some(provider.as_str())
        || parsed.token_hash.as_deref() != Some(token_hash)
        || parsed.host != crate::services::platform::hostname_short()
    {
        return Err("canonical primary namespace mismatch".into());
    }
    let key = adk_session::build_namespaced_session_key(token_hash, provider, &parsed.tmux_name);
    if key != canonical.1 {
        return Err("canonical primary is not the exact local key".into());
    }
    let row = sqlx::query(
        "SELECT id, session_key, provider, identity_kind, discord_token_hash, channel_id,
                status, active_turn_nonce, hosted_execution
           FROM sessions WHERE id = $1",
    )
    .bind(canonical.0)
    .fetch_optional(pool)
    .await
    .map_err(|_| "canonical row read failed")?
    .ok_or("canonical row disappeared")?;
    let row = RowWitness {
        id: row
            .try_get("id")
            .map_err(|_| "canonical row id unreadable")?,
        session_key: row
            .try_get("session_key")
            .map_err(|_| "canonical row primary unreadable")?,
        provider: row
            .try_get("provider")
            .map_err(|_| "canonical row provider unreadable")?,
        identity_kind: row
            .try_get("identity_kind")
            .map_err(|_| "canonical row kind unreadable")?,
        discord_token_hash: row
            .try_get("discord_token_hash")
            .map_err(|_| "canonical row token unreadable")?,
        channel_id: row
            .try_get("channel_id")
            .map_err(|_| "canonical row channel unreadable")?,
        status: row
            .try_get("status")
            .map_err(|_| "canonical row status unreadable")?,
        active_turn_nonce: row
            .try_get("active_turn_nonce")
            .map_err(|_| "canonical row nonce unreadable")?,
        hosted_execution: row
            .try_get("hosted_execution")
            .map_err(|_| "canonical hosted execution unreadable")?,
    };
    if row.id != canonical.0
        || row.session_key.as_deref() != Some(canonical.1.as_str())
        || row.provider.as_deref() != Some(provider.as_str())
        || row.identity_kind.as_deref() != Some(identity.kind.as_str())
        || row.discord_token_hash.as_deref() != Some(token_hash)
        || row.channel_id.as_deref() != Some(channel_text.as_str())
    {
        return Err("canonical row identity changed".into());
    }
    // Only SQL NULL has Legacy meaning; JSON null and every hosted record stay unresolved.
    if row.hosted_execution.is_some() {
        return Err("canonical row is not legacy local evidence".into());
    }
    let exact = resolve_session_row_pg(pool, Some(&key), None, None)
        .await
        .map_err(|_| "canonical exact locator lookup failed")?
        .ok_or("canonical exact locator missing")?;
    if exact != canonical {
        return Err("canonical exact locator diverged".into());
    }
    Ok(CanonicalTarget {
        name: parsed.tmux_name,
        canonical,
        exact,
        row,
    })
}
