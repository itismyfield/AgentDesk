use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::db::dispatched_sessions::HookSessionUpsert;

const DISCORD_CHANNEL_KIND: &str = "discord_channel";
const SCHEDULED_SNAPSHOT_KIND: &str = "scheduled_snapshot";
const ADVISORY_LOCK_NAMESPACE: i32 = 4913;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionIdentityKind {
    DiscordChannel,
    ScheduledSnapshot,
}

impl SessionIdentityKind {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            DISCORD_CHANNEL_KIND => Some(Self::DiscordChannel),
            SCHEDULED_SNAPSHOT_KIND => Some(Self::ScheduledSnapshot),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::DiscordChannel => DISCORD_CHANNEL_KIND,
            Self::ScheduledSnapshot => SCHEDULED_SNAPSHOT_KIND,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CanonicalSessionIdentity<'a> {
    pub(crate) kind: SessionIdentityKind,
    pub(crate) discord_token_hash: &'a str,
    pub(crate) channel_id: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionIdentityConflictKind {
    AmbiguousCanonical,
    AmbiguousLegacy,
    EvidenceDivergence,
    LocatorNamespace,
    OwnershipMismatch,
}

impl SessionIdentityConflictKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::AmbiguousCanonical => "ambiguous_canonical",
            Self::AmbiguousLegacy => "ambiguous_legacy",
            Self::EvidenceDivergence => "evidence_divergence",
            Self::LocatorNamespace => "locator_namespace",
            Self::OwnershipMismatch => "ownership_mismatch",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SessionIdentityConflict {
    pub(crate) kind: SessionIdentityConflictKind,
    message: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HookSessionUpsertError {
    Conflict(SessionIdentityConflict),
    Database(String),
}

impl HookSessionUpsertError {
    pub(crate) fn conflict_kind(&self) -> Option<SessionIdentityConflictKind> {
        match self {
            Self::Conflict(conflict) => Some(conflict.kind),
            Self::Database(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_conflict(kind: SessionIdentityConflictKind, message: &str) -> Self {
        conflict(kind, message)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HookSessionUpsertOutcome {
    pub(crate) inserted: bool,
    pub(crate) session_key: String,
}

#[derive(Clone, Debug)]
struct SessionEvidence {
    id: i64,
    session_key: String,
    provider: Option<String>,
    identity_kind: Option<String>,
    discord_token_hash: Option<String>,
    channel_id: Option<String>,
}

pub(crate) async fn upsert_hook_session_with_identity_pg(
    pool: &PgPool,
    params: HookSessionUpsert<'_>,
    identity: Option<CanonicalSessionIdentity<'_>>,
) -> Result<HookSessionUpsertOutcome, HookSessionUpsertError> {
    let mut tx = pool.begin().await.map_err(database_error)?;

    acquire_locator_lock(&mut tx, params.session_key).await?;
    if let Some(identity) = identity {
        acquire_identity_lock(&mut tx, params.provider, identity).await?;
    }

    let exact = load_exact_key_for_update(&mut tx, params.session_key).await?;
    let alias = load_alias_for_update(&mut tx, params.session_key).await?;
    let canonical = match identity {
        Some(identity) if identity.kind == SessionIdentityKind::DiscordChannel => {
            load_canonical_for_update(&mut tx, params.provider, identity).await?
        }
        _ => None,
    };
    let promotion = match (identity, canonical.as_ref()) {
        (Some(identity), None) if identity.kind == SessionIdentityKind::DiscordChannel => {
            load_unique_legacy_candidate_for_update(&mut tx, params.provider, identity).await?
        }
        _ => None,
    };

    let target = resolve_evidence(exact, alias, canonical, promotion)?;
    let outcome = match target {
        Some(target) => {
            validate_target_identity(&target, params.provider, params.channel_id, identity)?;
            update_target(&mut tx, target.id, &params, identity).await?;
            preserve_alias(&mut tx, params.session_key, target.id).await?;
            HookSessionUpsertOutcome {
                inserted: false,
                session_key: target.session_key,
            }
        }
        None => insert_target(&mut tx, &params, identity).await?,
    };

    tx.commit().await.map_err(database_error)?;
    Ok(outcome)
}

pub(crate) async fn resolve_session_key_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<Option<String>, HookSessionUpsertError> {
    resolve_session_key_with_identity_pg(pool, session_key, None, None).await
}

/// Resolve and lock an exact primary-or-alias locator inside the caller's
/// mutation transaction. Returning the durable row id keeps the subsequent
/// write pinned even if the primary locator changes after resolution.
pub(crate) async fn resolve_session_id_for_mutation_pg(
    tx: &mut Transaction<'_, Postgres>,
    session_key: &str,
) -> Result<Option<i64>, HookSessionUpsertError> {
    acquire_locator_lock(tx, session_key).await?;
    let exact = load_exact_key_for_update(tx, session_key).await?;
    let alias = load_alias_for_update(tx, session_key).await?;
    resolve_evidence(exact, alias, None, None).map(|target| target.map(|evidence| evidence.id))
}

pub(crate) async fn upsert_legacy_hook_session_pg(
    pool: &PgPool,
    params: HookSessionUpsert<'_>,
) -> Result<HookSessionUpsertOutcome, HookSessionUpsertError> {
    upsert_hook_session_with_identity_pg(pool, params, None).await
}

/// Resolve a runtime locator in compatibility order: exact current key, exact
/// alias, then the supplied ordinary Discord identity. All evidence must
/// converge on one durable row; precedence never permits conflicting evidence
/// to select a destructive target.
pub(crate) async fn resolve_session_key_with_identity_pg(
    pool: &PgPool,
    session_key: &str,
    provider: Option<&str>,
    identity: Option<CanonicalSessionIdentity<'_>>,
) -> Result<Option<String>, HookSessionUpsertError> {
    let rows = sqlx::query(
        "SELECT id, session_key, source_rank
         FROM (
             SELECT s.id, s.session_key, 1 AS source_rank
             FROM sessions s
             WHERE s.session_key = $1
             UNION ALL
             SELECT s.id, s.session_key, 2 AS source_rank
             FROM session_key_aliases a
             JOIN sessions s ON s.id = a.session_id
             WHERE a.session_key = $1
             UNION ALL
             SELECT s.id, s.session_key, 3 AS source_rank
             FROM sessions s
             WHERE $2::TEXT IS NOT NULL
               AND $3::TEXT IS NOT NULL
               AND $4::TEXT IS NOT NULL
               AND $5::TEXT = 'discord_channel'
               AND s.provider = $2
               AND s.discord_token_hash = $3
               AND s.channel_id = $4
               AND s.identity_kind = 'discord_channel'
         ) evidence
         ORDER BY source_rank",
    )
    .bind(session_key)
    .bind(provider)
    .bind(identity.map(|value| value.discord_token_hash))
    .bind(identity.map(|value| value.channel_id))
    .bind(identity.map(|value| value.kind.as_str()))
    .fetch_all(pool)
    .await
    .map_err(database_error)?;

    let mut resolved: Option<(i64, String)> = None;
    for row in rows {
        let id: i64 = row.try_get("id").map_err(database_error)?;
        let key: String = row.try_get("session_key").map_err(database_error)?;
        if let Some((resolved_id, _)) = resolved.as_ref()
            && *resolved_id != id
        {
            return Err(conflict(
                SessionIdentityConflictKind::EvidenceDivergence,
                "session locator resolves to multiple rows",
            ));
        }
        resolved = Some((id, key));
    }
    Ok(resolved.map(|(_, key)| key))
}

async fn acquire_locator_lock(
    tx: &mut Transaction<'_, Postgres>,
    session_key: &str,
) -> Result<(), HookSessionUpsertError> {
    sqlx::query("SELECT agentdesk_lock_session_locator($1)")
        .bind(session_key)
        .execute(&mut **tx)
        .await
        .map_err(database_error)?;
    Ok(())
}

async fn acquire_identity_lock(
    tx: &mut Transaction<'_, Postgres>,
    provider: &str,
    identity: CanonicalSessionIdentity<'_>,
) -> Result<(), HookSessionUpsertError> {
    let key = format!(
        "identity:{}:{provider}:{}:{}",
        identity.kind.as_str(),
        identity.discord_token_hash,
        identity.channel_id
    );
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(ADVISORY_LOCK_NAMESPACE)
        .bind(key)
        .execute(&mut **tx)
        .await
        .map_err(database_error)?;
    Ok(())
}

async fn load_exact_key_for_update(
    tx: &mut Transaction<'_, Postgres>,
    session_key: &str,
) -> Result<Option<SessionEvidence>, HookSessionUpsertError> {
    load_one_for_update(
        tx,
        "SELECT id, session_key, provider, identity_kind, discord_token_hash, channel_id
         FROM sessions WHERE session_key = $1 FOR UPDATE",
        session_key,
    )
    .await
}

async fn load_alias_for_update(
    tx: &mut Transaction<'_, Postgres>,
    session_key: &str,
) -> Result<Option<SessionEvidence>, HookSessionUpsertError> {
    load_one_for_update(
        tx,
        "SELECT s.id, s.session_key, s.provider, s.identity_kind,
                s.discord_token_hash, s.channel_id
         FROM session_key_aliases a
         JOIN sessions s ON s.id = a.session_id
         WHERE a.session_key = $1
         FOR UPDATE OF a, s",
        session_key,
    )
    .await
}

async fn load_one_for_update(
    tx: &mut Transaction<'_, Postgres>,
    query: &str,
    session_key: &str,
) -> Result<Option<SessionEvidence>, HookSessionUpsertError> {
    sqlx::query(query)
        .bind(session_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(database_error)?
        .map(decode_evidence)
        .transpose()
}

async fn load_canonical_for_update(
    tx: &mut Transaction<'_, Postgres>,
    provider: &str,
    identity: CanonicalSessionIdentity<'_>,
) -> Result<Option<SessionEvidence>, HookSessionUpsertError> {
    let rows = sqlx::query(
        "SELECT id, session_key, provider, identity_kind, discord_token_hash, channel_id
         FROM sessions
         WHERE identity_kind = 'discord_channel'
           AND provider = $1
           AND discord_token_hash = $2
           AND channel_id = $3
         FOR UPDATE",
    )
    .bind(provider)
    .bind(identity.discord_token_hash)
    .bind(identity.channel_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_error)?;

    if rows.len() > 1 {
        return Err(conflict(
            SessionIdentityConflictKind::AmbiguousCanonical,
            "canonical Discord identity is ambiguous",
        ));
    }
    rows.into_iter().next().map(decode_evidence).transpose()
}

async fn load_unique_legacy_candidate_for_update(
    tx: &mut Transaction<'_, Postgres>,
    provider: &str,
    identity: CanonicalSessionIdentity<'_>,
) -> Result<Option<SessionEvidence>, HookSessionUpsertError> {
    let rows = sqlx::query(
        "SELECT id, session_key, provider, identity_kind, discord_token_hash, channel_id
         FROM sessions
         WHERE identity_kind IS NULL
           AND discord_token_hash IS NULL
           AND provider = $1
           AND channel_id = $2
           AND agentdesk_legacy_discord_locator_is_ordinary(session_key, $1, $3)
         FOR UPDATE",
    )
    .bind(provider)
    .bind(identity.channel_id)
    .bind(identity.discord_token_hash)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_error)?;

    match rows.len() {
        0 => Ok(None),
        1 => rows.into_iter().next().map(decode_evidence).transpose(),
        _ => Err(conflict(
            SessionIdentityConflictKind::AmbiguousLegacy,
            "multiple legacy session rows claim the canonical Discord identity",
        )),
    }
}

fn decode_evidence(row: sqlx::postgres::PgRow) -> Result<SessionEvidence, HookSessionUpsertError> {
    Ok(SessionEvidence {
        id: row.try_get("id").map_err(database_error)?,
        session_key: row.try_get("session_key").map_err(database_error)?,
        provider: row.try_get("provider").map_err(database_error)?,
        identity_kind: row.try_get("identity_kind").map_err(database_error)?,
        discord_token_hash: row.try_get("discord_token_hash").map_err(database_error)?,
        channel_id: row.try_get("channel_id").map_err(database_error)?,
    })
}

fn resolve_evidence(
    exact: Option<SessionEvidence>,
    alias: Option<SessionEvidence>,
    canonical: Option<SessionEvidence>,
    promotion: Option<SessionEvidence>,
) -> Result<Option<SessionEvidence>, HookSessionUpsertError> {
    let mut target: Option<SessionEvidence> = None;
    for evidence in [exact, alias, canonical, promotion].into_iter().flatten() {
        if let Some(current) = target.as_ref()
            && current.id != evidence.id
        {
            return Err(conflict(
                SessionIdentityConflictKind::EvidenceDivergence,
                "session identity evidence resolves to multiple rows",
            ));
        }
        target = Some(evidence);
    }
    Ok(target)
}

pub(crate) fn hook_session_upsert_error_to_app_error(
    error: HookSessionUpsertError,
) -> crate::error::AppError {
    match error {
        HookSessionUpsertError::Conflict(conflict) => {
            crate::error::AppError::conflict(conflict.message)
        }
        HookSessionUpsertError::Database(error) => {
            crate::error::AppError::internal(error).with_code(crate::error::ErrorCode::Database)
        }
    }
}

fn validate_target_identity(
    target: &SessionEvidence,
    provider: &str,
    incoming_channel_id: Option<&str>,
    identity: Option<CanonicalSessionIdentity<'_>>,
) -> Result<(), HookSessionUpsertError> {
    let Some(identity) = identity else {
        if target
            .provider
            .as_deref()
            .is_some_and(|owner| owner != provider)
            || match (target.channel_id.as_deref(), incoming_channel_id) {
                (Some(owner), Some(incoming)) => owner != incoming,
                _ => false,
            }
        {
            return Err(conflict(
                SessionIdentityConflictKind::OwnershipMismatch,
                "legacy session locator has conflicting provider or channel ownership",
            ));
        }
        return Ok(());
    };

    if identity.kind == SessionIdentityKind::ScheduledSnapshot {
        if target.identity_kind.as_deref() == Some(DISCORD_CHANNEL_KIND) {
            return Err(conflict(
                SessionIdentityConflictKind::OwnershipMismatch,
                "scheduled snapshot locator belongs to a Discord channel",
            ));
        }
        if target.identity_kind.as_deref() == Some(SCHEDULED_SNAPSHOT_KIND) {
            let matches = target.provider.as_deref() == Some(provider)
                && target.discord_token_hash.as_deref() == Some(identity.discord_token_hash)
                && target.channel_id.as_deref() == Some(identity.channel_id);
            return if matches {
                Ok(())
            } else {
                Err(conflict(
                    SessionIdentityConflictKind::OwnershipMismatch,
                    "scheduled snapshot row has conflicting ownership",
                ))
            };
        }
        let legacy_matches = target.identity_kind.is_none()
            && target.discord_token_hash.is_none()
            && target.provider.as_deref() == Some(provider)
            && target.channel_id.as_deref() == Some(identity.channel_id);
        return if legacy_matches {
            Ok(())
        } else {
            Err(conflict(
                SessionIdentityConflictKind::OwnershipMismatch,
                "scheduled snapshot row has conflicting ownership",
            ))
        };
    }

    let legacy_owner_matches = target.identity_kind.is_none()
        && target.discord_token_hash.is_none()
        && target.provider.as_deref() == Some(provider)
        && target.channel_id.as_deref() == Some(identity.channel_id);
    if legacy_owner_matches {
        return Ok(());
    }

    let matches = target.provider.as_deref() == Some(provider)
        && target.identity_kind.as_deref() == Some(identity.kind.as_str())
        && target.discord_token_hash.as_deref() == Some(identity.discord_token_hash)
        && target.channel_id.as_deref() == Some(identity.channel_id);
    if matches {
        Ok(())
    } else {
        Err(conflict(
            SessionIdentityConflictKind::OwnershipMismatch,
            "legacy session row has conflicting Discord ownership",
        ))
    }
}

async fn preserve_alias(
    tx: &mut Transaction<'_, Postgres>,
    session_key: &str,
    session_id: i64,
) -> Result<(), HookSessionUpsertError> {
    let primary_id = sqlx::query_scalar::<_, i64>("SELECT id FROM sessions WHERE session_key = $1")
        .bind(session_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(database_error)?;
    if let Some(primary_id) = primary_id {
        if primary_id != session_id {
            return Err(conflict(
                SessionIdentityConflictKind::OwnershipMismatch,
                "session locator is owned by another row",
            ));
        }
        return Ok(());
    }

    let alias_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO session_key_aliases (session_key, session_id)
         VALUES ($1, $2)
         ON CONFLICT (session_key) DO UPDATE
         SET session_id = session_key_aliases.session_id
         RETURNING session_id",
    )
    .bind(session_key)
    .bind(session_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(classify_write_error)?;
    if alias_id != session_id {
        return Err(conflict(
            SessionIdentityConflictKind::OwnershipMismatch,
            "session alias is owned by another row",
        ));
    }
    Ok(())
}

async fn update_target(
    tx: &mut Transaction<'_, Postgres>,
    session_id: i64,
    params: &HookSessionUpsert<'_>,
    identity: Option<CanonicalSessionIdentity<'_>>,
) -> Result<(), HookSessionUpsertError> {
    sqlx::query(
        "UPDATE sessions SET
            status = $2,
            instance_id = COALESCE(NULLIF(BTRIM($3), ''), instance_id),
            provider = $4,
            session_info = COALESCE($5, session_info),
            model = COALESCE($6, model),
            tokens = CASE WHEN $7 IS NOT NULL THEN $7 ELSE tokens END,
            tokens_updated_at = CASE WHEN $7 IS NOT NULL THEN NOW() ELSE tokens_updated_at END,
            cwd = COALESCE($8, cwd),
            active_dispatch_id = CASE
              WHEN lower($2) IN ('disconnected', 'aborted') THEN NULL
              WHEN $9 IS NOT NULL THEN $9
              ELSE active_dispatch_id
            END,
            agent_id = COALESCE(NULLIF(BTRIM($10), ''), NULLIF(BTRIM(agent_id), '')),
            thread_channel_id = COALESCE($11, thread_channel_id),
            channel_id = COALESCE($12, channel_id),
            identity_kind = COALESCE($13, identity_kind),
            discord_token_hash = COALESCE($14, discord_token_hash),
            claude_session_id = COALESCE($15, claude_session_id),
            claude_session_id_recorded_at = CASE
              WHEN $15 IS NULL THEN claude_session_id_recorded_at
              WHEN claude_session_id IS DISTINCT FROM $15 THEN NOW()
              ELSE COALESCE(claude_session_id_recorded_at, NOW())
            END,
            raw_provider_session_id = COALESCE($16, raw_provider_session_id),
            active_turn_nonce = COALESCE($17, active_turn_nonce),
            dispatched_origin_turn_nonce = CASE
              WHEN $17 IS NULL THEN dispatched_origin_turn_nonce
              WHEN $18 THEN $17
              ELSE NULL
            END,
            last_heartbeat = NOW()
         WHERE id = $1",
    )
    .bind(session_id)
    .bind(params.status)
    .bind(params.instance_id)
    .bind(params.provider)
    .bind(params.session_info)
    .bind(params.model)
    .bind(params.tokens)
    .bind(params.cwd)
    .bind(params.active_dispatch_id)
    .bind(params.agent_id)
    .bind(params.thread_channel_id)
    .bind(params.channel_id)
    .bind(identity.map(|value| value.kind.as_str()))
    .bind(identity.map(|value| value.discord_token_hash))
    .bind(params.claude_session_id)
    .bind(params.raw_provider_session_id)
    .bind(params.turn_start_nonce)
    .bind(params.dispatched_origin)
    .execute(&mut **tx)
    .await
    .map_err(database_error)?;
    Ok(())
}

async fn insert_target(
    tx: &mut Transaction<'_, Postgres>,
    params: &HookSessionUpsert<'_>,
    identity: Option<CanonicalSessionIdentity<'_>>,
) -> Result<HookSessionUpsertOutcome, HookSessionUpsertError> {
    let inserted = sqlx::query_scalar::<_, i64>(
        "INSERT INTO sessions (
            session_key, instance_id, agent_id, provider, status, session_info,
            model, tokens, tokens_updated_at, cwd, active_dispatch_id,
            thread_channel_id, channel_id, identity_kind, discord_token_hash,
            claude_session_id, raw_provider_session_id, active_turn_nonce,
            dispatched_origin_turn_nonce, claude_session_id_recorded_at, last_heartbeat
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, COALESCE($8, 0),
            CASE WHEN $8 IS NOT NULL THEN NOW() ELSE NULL END,
            $9, $10, $11, $12, $13, $14, $15, $16, $17,
            CASE WHEN $18 THEN $17 ELSE NULL END,
            CASE WHEN NULLIF(BTRIM($15), '') IS NOT NULL THEN NOW() ELSE NULL END,
            NOW()
         )
         RETURNING id",
    )
    .bind(params.session_key)
    .bind(params.instance_id)
    .bind(params.agent_id)
    .bind(params.provider)
    .bind(params.status)
    .bind(params.session_info)
    .bind(params.model)
    .bind(params.tokens)
    .bind(params.cwd)
    .bind(params.active_dispatch_id)
    .bind(params.thread_channel_id)
    .bind(params.channel_id)
    .bind(identity.map(|value| value.kind.as_str()))
    .bind(identity.map(|value| value.discord_token_hash))
    .bind(params.claude_session_id)
    .bind(params.raw_provider_session_id)
    .bind(params.turn_start_nonce)
    .bind(params.dispatched_origin)
    .fetch_one(&mut **tx)
    .await
    .map_err(classify_write_error)?;

    preserve_alias(tx, params.session_key, inserted).await?;
    Ok(HookSessionUpsertOutcome {
        inserted: true,
        session_key: params.session_key.to_string(),
    })
}

fn conflict(
    kind: SessionIdentityConflictKind,
    message: impl Into<String>,
) -> HookSessionUpsertError {
    HookSessionUpsertError::Conflict(SessionIdentityConflict {
        kind,
        message: message.into(),
    })
}

fn classify_write_error(error: sqlx::Error) -> HookSessionUpsertError {
    let locator_namespace_collision = error
        .as_database_error()
        .and_then(|database_error| database_error.constraint())
        == Some("session_locator_namespace");
    if locator_namespace_collision {
        return conflict(
            SessionIdentityConflictKind::LocatorNamespace,
            "session locator is already owned in the primary/alias namespace",
        );
    }
    database_error(error)
}

fn database_error(error: impl std::fmt::Display) -> HookSessionUpsertError {
    HookSessionUpsertError::Database(crate::utils::redact::redact_known_secrets(
        &error.to_string(),
    ))
}

#[cfg(test)]
#[path = "canonical_identity_pg_tests.rs"]
mod pg_tests;

#[cfg(test)]
mod tests {
    use super::{
        HookSessionUpsertError, SessionEvidence, SessionIdentityKind, resolve_evidence,
        validate_target_identity,
    };

    fn evidence(id: i64, key: &str) -> SessionEvidence {
        SessionEvidence {
            id,
            session_key: key.to_string(),
            provider: None,
            identity_kind: None,
            discord_token_hash: None,
            channel_id: None,
        }
    }

    #[test]
    fn resolver_order_accepts_matching_exact_alias_and_canonical_evidence() {
        let resolved = resolve_evidence(
            Some(evidence(7, "current")),
            Some(evidence(7, "current")),
            Some(evidence(7, "current")),
            None,
        );
        assert!(matches!(resolved, Ok(Some(row)) if row.id == 7));
    }

    #[test]
    fn resolver_fails_closed_when_evidence_disagrees() {
        let error = resolve_evidence(
            Some(evidence(7, "current")),
            Some(evidence(8, "other")),
            None,
            None,
        );
        assert!(matches!(error, Err(HookSessionUpsertError::Conflict(_))));
    }

    #[test]
    fn old_body_cannot_reassign_an_existing_locator() {
        let mut target = evidence(7, "legacy");
        target.provider = Some("claude".to_string());
        target.channel_id = Some("123".to_string());

        assert!(validate_target_identity(&target, "claude", Some("123"), None).is_ok());
        assert!(matches!(
            validate_target_identity(&target, "codex", Some("123"), None),
            Err(HookSessionUpsertError::Conflict(_))
        ));
        assert!(matches!(
            validate_target_identity(&target, "claude", Some("999"), None),
            Err(HookSessionUpsertError::Conflict(_))
        ));
    }

    #[test]
    fn safe_legacy_promotion_requires_empty_identity_evidence() {
        let mut target = evidence(7, "legacy");
        target.provider = Some("claude".to_string());
        target.channel_id = Some("123".to_string());
        let identity = super::CanonicalSessionIdentity {
            kind: SessionIdentityKind::DiscordChannel,
            discord_token_hash: "discord_0123456789abcdef",
            channel_id: "123",
        };
        assert!(validate_target_identity(&target, "claude", Some("123"), Some(identity)).is_ok());

        let mut conflicting = target;
        conflicting.channel_id = Some("999".to_string());
        assert!(matches!(
            validate_target_identity(&conflicting, "claude", Some("123"), Some(identity)),
            Err(HookSessionUpsertError::Conflict(_))
        ));
    }
}
