//! Read-only local row witnesses let historical Legacy locators reach inventory absence checks.
use super::*;
use crate::services::discord::session_identity::SessionIdentity;
use sqlx::{PgPool, Row};

#[derive(Clone, Debug, PartialEq, Eq)]
struct RowWitness {
    id: i64,
    key: String,
    provider: Option<String>,
    kind: Option<String>,
    token: Option<String>,
    channel: Option<String>,
    status: Option<String>,
    nonce: Option<String>,
    dispatch: Option<String>,
    thread: Option<String>,
    hosted: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Proof {
    pub(super) name: String,
    exact: RowWitness,
    canonical: RowWitness,
}

pub(super) async fn collect(
    pool: &PgPool,
    provider: &ProviderKind,
    token: &str,
    channel: u64,
    name: &str,
    exact: &HostedLookup,
    canonical: &HostedLookup,
) -> Result<Proof, String> {
    let (HostedLookup::Found(exact), HostedLookup::Found(canonical)) = (exact, canonical) else {
        return Err("divergence requires two rows".into());
    };
    if exact.session_id() == canonical.session_id()
        || exact.record != HostedRecord::Legacy
        || canonical.record != HostedRecord::Legacy
    {
        return Err("divergence is not historical Legacy rows".into());
    }
    let rows = sqlx::query(
        "SELECT id, session_key, provider, identity_kind, discord_token_hash, channel_id,
                status, active_turn_nonce, active_dispatch_id, thread_channel_id, hosted_execution
           FROM sessions WHERE id = ANY($1)",
    )
    .bind([exact.session_id(), canonical.session_id()].as_slice())
    .fetch_all(pool)
    .await
    .map_err(|error| error.to_string())?;
    let rows: Result<Vec<_>, sqlx::Error> = rows
        .iter()
        .map(|row| {
            Ok(RowWitness {
                id: row.try_get("id")?,
                key: row.try_get("session_key")?,
                provider: row.try_get("provider")?,
                kind: row.try_get("identity_kind")?,
                token: row.try_get("discord_token_hash")?,
                channel: row.try_get("channel_id")?,
                status: row.try_get("status")?,
                nonce: row.try_get("active_turn_nonce")?,
                dispatch: row.try_get("active_dispatch_id")?,
                thread: row.try_get("thread_channel_id")?,
                hosted: row.try_get("hosted_execution")?,
            })
        })
        .collect();
    let rows = rows.map_err(|error| error.to_string())?;
    let find = |id| {
        rows.iter()
            .find(|row| row.id == id)
            .cloned()
            .ok_or("Legacy row disappeared")
    };
    let (exact, canonical) = (find(exact.session_id())?, find(canonical.session_id())?);
    let channel_text = channel.to_string();
    for row in [&exact, &canonical] {
        if row.provider.as_deref() != Some(provider.as_str())
            || row.channel.as_deref() != Some(channel_text.as_str())
            || row.hosted.is_some()
            || row.thread.is_some()
            || !matches!(
                row.status.as_deref(),
                Some("idle" | "disconnected" | "aborted")
            )
        {
            return Err("Legacy row is not a terminal channel row".into());
        }
    }
    if exact.kind.is_some()
        || exact.token.is_some()
        || canonical.kind.as_deref() != Some("discord_channel")
        || canonical.token.as_deref() != Some(token)
    {
        return Err("Legacy row identity changed".into());
    }
    let local_name = |key: &str| -> Result<String, String> {
        let parsed = SessionIdentity::parse(key).ok_or("Legacy locator malformed")?;
        if !parsed.namespaced
            || parsed.provider_from_key.as_deref() != Some(provider.as_str())
            || parsed.token_hash.as_deref() != Some(token)
            || parsed.host != crate::services::platform::hostname_short()
            || key != adk_session::build_namespaced_session_key(token, provider, &parsed.tmux_name)
        {
            return Err("Legacy locator is not the exact local namespace".into());
        }
        Ok(parsed.tmux_name)
    };
    if local_name(&exact.key)? != name {
        return Err("Legacy exact locator changed".into());
    }
    Ok(Proof {
        name: local_name(&canonical.key)?,
        exact,
        canonical,
    })
}
