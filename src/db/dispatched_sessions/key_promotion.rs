use super::*;

const SNAPSHOT_CHANGED: &str = "registration snapshot changed; rediscover before retry";

pub(crate) fn snapshot_changed(error: &HookSessionUpsertError) -> bool {
    matches!(error, HookSessionUpsertError::Conflict(conflict) if conflict.message == SNAPSHOT_CHANGED)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RegistrationSnapshot {
    pub(crate) id: i64,
    pub(crate) primary: String,
    pub(crate) nonce: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) hosted: Option<serde_json::Value>,
}

/// The host reader retains its explicit namespace observation until commit.
pub(crate) struct RegistrationWitness<'a> {
    pub(crate) snapshot: &'a RegistrationSnapshot,
    pub(crate) new_key: &'a str,
    pub(crate) new_nonce: &'a str,
    pub(crate) old_host_absent: &'a (dyn Fn() -> bool + Send + Sync),
}

pub(crate) async fn discover_registration_pg(
    pool: &PgPool,
    provider: &str,
    identity: CanonicalSessionIdentity<'_>,
) -> Result<Option<RegistrationSnapshot>, HookSessionUpsertError> {
    let Some((id, _)) = read_resolved_row(pool, None, Some(provider), Some(identity)).await? else {
        return Ok(None);
    };
    load_snapshot(pool, id).await
}

async fn load_snapshot<'e>(
    executor: impl sqlx::Executor<'e, Database = Postgres>,
    id: i64,
) -> Result<Option<RegistrationSnapshot>, HookSessionUpsertError> {
    sqlx::query("SELECT id, session_key, active_turn_nonce, status, hosted_execution FROM sessions WHERE id = $1")
        .bind(id)
        .fetch_optional(executor)
        .await
        .map_err(database_error)?
        .map(|row| Ok(RegistrationSnapshot {
            id: row.try_get("id").map_err(database_error)?,
            primary: row.try_get("session_key").map_err(database_error)?,
            nonce: row.try_get("active_turn_nonce").map_err(database_error)?,
            status: row.try_get("status").map_err(database_error)?,
            hosted: row.try_get("hosted_execution").map_err(database_error)?,
        }))
        .transpose()
}

pub(crate) async fn register_hook_session_pg(
    pool: &PgPool,
    params: HookSessionUpsert<'_>,
    identity: CanonicalSessionIdentity<'_>,
    witness: &RegistrationWitness<'_>,
) -> Result<HookSessionUpsertOutcome, HookSessionUpsertError> {
    let pin = HookSessionActorPin::Existing {
        id: witness.snapshot.id,
        active_turn_nonce: witness.snapshot.nonce.clone(),
        status: witness.snapshot.status.clone(),
    };
    upsert_hook_session_pg(
        pool,
        params,
        Some(identity),
        Some(&pin),
        None,
        Some(witness),
    )
    .await
    .map(|(outcome, _)| outcome)
}

pub(super) async fn lock_locators(
    tx: &mut Transaction<'_, Postgres>,
    incoming: &str,
    witness: Option<&RegistrationWitness<'_>>,
) -> Result<(), HookSessionUpsertError> {
    let mut keys = vec![incoming];
    if let Some(witness) = witness {
        keys.push(&witness.snapshot.primary);
    }
    keys.sort_unstable();
    keys.dedup();
    for key in keys {
        acquire_locator_lock(tx, key).await?;
    }
    Ok(())
}

fn refused(message: &str) -> HookSessionUpsertError {
    conflict(SessionIdentityConflictKind::OwnershipMismatch, message)
}

pub(super) async fn validate_registration(
    tx: &mut Transaction<'_, Postgres>,
    params: &HookSessionUpsert<'_>,
    identity: Option<CanonicalSessionIdentity<'_>>,
    target: Option<&SessionEvidence>,
    witness: &RegistrationWitness<'_>,
) -> Result<(), HookSessionUpsertError> {
    let snapshot = witness.snapshot;
    let pin = HookSessionActorPin::Existing {
        id: snapshot.id,
        active_turn_nonce: snapshot.nonce.clone(),
        status: snapshot.status.clone(),
    };
    if identity.is_none_or(|id| id.kind != SessionIdentityKind::DiscordChannel)
        || witness.new_key != params.session_key
        || witness.new_key == snapshot.primary
        || witness.new_nonce.trim().is_empty()
        || params.turn_start_nonce != Some(witness.new_nonce)
        || snapshot.hosted.is_some()
        || pin.active_for_other_actor(Some(witness.new_nonce))
        || !(witness.old_host_absent)()
    {
        return Err(refused(
            "registration has no fresh actor and old-host absence witness",
        ));
    }
    if target
        .is_none_or(|target| target.id != snapshot.id || target.session_key != snapshot.primary)
        || load_snapshot(&mut **tx, snapshot.id).await?.as_ref() != Some(snapshot)
    {
        return Err(refused(SNAPSHOT_CHANGED));
    }
    if load_alias_for_update(tx, &snapshot.primary)
        .await?
        .is_some_and(|alias| alias.id != snapshot.id)
    {
        return Err(refused("old primary alias belongs to another row"));
    }
    Ok(())
}

pub(crate) fn same_physical_locator(left: &str, right: &str) -> bool {
    use crate::domain::session_identity::SessionIdentity;
    match (SessionIdentity::parse(left), SessionIdentity::parse(right)) {
        (Some(left), Some(right)) => left.host == right.host && left.tmux_name == right.tmux_name,
        _ => false,
    }
}

pub(super) async fn alias_nonce_fence<'a>(
    tx: &mut Transaction<'_, Postgres>,
    target: &SessionEvidence,
    params: &HookSessionUpsert<'a>,
    is_alias: bool,
    actor_pinned: bool,
    terminal: Option<&'a str>,
) -> Result<Option<&'a str>, HookSessionUpsertError> {
    if is_alias && !actor_pinned && terminal.is_none() {
        if let Some(nonce) = params.turn_start_nonce {
            return Ok(Some(nonce));
        }
        let nonce: Option<String> =
            sqlx::query_scalar("SELECT active_turn_nonce FROM sessions WHERE id = $1")
                .bind(target.id)
                .fetch_one(&mut **tx)
                .await
                .map_err(database_error)?;
        if nonce.is_some() {
            return Err(refused("alias hook requires its expected actor nonce"));
        }
    }
    if !same_physical_locator(&target.session_key, params.session_key)
        && !actor_pinned
        && params.status.eq_ignore_ascii_case("turn_active")
    {
        return Err(refused("new execution requires explicit registration"));
    }
    Ok(terminal)
}

pub(super) async fn promote_primary(
    tx: &mut Transaction<'_, Postgres>,
    target: &SessionEvidence,
    witness: &RegistrationWitness<'_>,
) -> Result<String, HookSessionUpsertError> {
    if !(witness.old_host_absent)() {
        return Err(refused("old host absence witness changed"));
    }
    let updated = sqlx::query("UPDATE sessions SET session_key = $1 WHERE id = $2 AND (active_turn_nonce IS NULL OR active_turn_nonce = $3)")
        .bind(witness.new_key).bind(target.id).bind(witness.new_nonce)
        .execute(&mut **tx).await.map_err(classify_write_error)?;
    if updated.rows_affected() != 1 {
        return Err(refused("promotion actor nonce changed"));
    }
    preserve_alias(tx, &target.session_key, target.id).await?;
    Ok(witness.new_key.to_owned())
}
