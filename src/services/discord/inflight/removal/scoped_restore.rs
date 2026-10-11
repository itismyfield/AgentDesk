use super::*;

#[cfg(test)]
pub(in crate::services::discord) async fn invalidate_stale_generation_scoped(
    pool: Option<&sqlx::PgPool>,
    provider: &ProviderKind,
    generation: u64,
    expected: &InflightTurnState,
) -> Result<bool, RestoreFailure> {
    let protected = |row: &InflightTurnState| {
        row.restart_mode.is_some()
            || row.rebind_origin
            || (!scoped_restore_mutant("replay-front")
                && (row.replay_rerun_blocked() || row.replay_receipt_id.is_some()))
    };
    if protected(expected)
        || expected
            .restart_generation
            .is_none_or(|old| old == generation)
    {
        return Ok(false);
    }
    let channel = expected.channel_id;
    let input_held =
        || crate::services::discord::input_runtime::fence::lookup(provider, channel).is_some();
    if channel == 0 || input_held() || expected.provider_kind().as_ref() != Some(provider) {
        return Err("generation restore scope/input mismatch".into());
    }
    let root = inflight_runtime_root()
        .ok_or_else(|| RestoreFailure::retryable("inflight root unavailable"))?;
    let path = inflight_state_path(&root, provider, channel);
    let _guard = try_lock_inflight_state_path(&path).map_err(RestoreFailure::retryable)?;
    let bytes = fs::read_to_string(&path).map_err(RestoreFailure::retryable)?;
    let fresh = parse_inflight_state_content(&bytes).map_err(|error| error.to_string())?;
    if !scoped_restore_mutant("snapshot")
        && serde_json::to_value(&fresh).map_err(|error| error.to_string())?
            != serde_json::to_value(expected).map_err(|error| error.to_string())?
    {
        return Err("generation restore snapshot changed".into());
    }
    if protected(&fresh) {
        return Ok(false);
    }
    let pool =
        pool.ok_or_else(|| RestoreFailure::retryable("generation replay receipt unavailable"))?;
    let sources: Vec<String> = fresh
        .source_message_ids
        .iter()
        .chain([&fresh.user_msg_id])
        .filter(|&&id| id != 0)
        .map(u64::to_string)
        .collect();
    // Canonical source holds without a local projection also fence this cleanup.
    if crate::db::replay_disposition::blocked_receipt_for_sources(
        pool,
        provider.as_str(),
        &channel.to_string(),
        &sources,
    )
    .await
    .map_err(RestoreFailure::retryable)?
    .is_some()
        && !scoped_restore_mutant("canonical-replay")
    {
        return Ok(false);
    }
    crate::services::discord::input_runtime::fence::write(provider, channel, || {
        if input_held() {
            return Err("input-protected generation restore".into());
        }
        log_inflight_remove(
            provider,
            channel,
            fresh.user_msg_id,
            "invalidate_stale_generation_scoped",
            &path,
        );
        fs::remove_file(&path).map_err(|error| String::from(RestoreFailure::retryable(error)))?;
        crate::services::discord::runtime_store::fsync_parent_dir(&path)
            .map_err(|error| String::from(RestoreFailure::retryable(error)))
    })?;
    Ok(true)
}

pub(super) fn backfill_scoped_restore(
    root: &Path,
    path: &Path,
    provider: &ProviderKind,
) -> Result<InflightTurnState, RestoreFailure> {
    let _guard = try_lock_inflight_state_path(path).map_err(RestoreFailure::retryable)?;
    let bytes = fs::read_to_string(path).map_err(RestoreFailure::retryable)?;
    let (state, needs_backfill) = parse_inflight_state_content_with_finalizer_backfill(&bytes)
        .map_err(|error| RestoreFailure::Rejected(error.to_string()))?;
    if state.provider_kind().as_ref() != Some(provider)
        || state.channel_id != channel_id_from_path(path)
        || state.runtime_kind_unknown_on_disk
        || state.version > inflight_state_version()
    {
        return Err(RestoreFailure::Rejected(
            "scoped backfill format/binding refused".into(),
        ));
    }
    if needs_backfill {
        let outcome = super::store::persist_under_lock_with_snapshot(
            root,
            path,
            &state,
            "scoped_restore_backfill",
        );
        #[cfg(test)]
        if scoped_restore_mutant("backfill-ack") && outcome.is_err() {
            return Ok(state);
        }
        let saved = outcome.map_err(RestoreFailure::retryable)?.ok_or_else(|| {
            RestoreFailure::Rejected("scoped backfill persistence refused".into())
        })?;
        #[cfg(test)]
        if scoped_restore_mutant("fsync-failure") {
            return Err(RestoreFailure::retryable("parent fsync failed"));
        }
        crate::services::discord::runtime_store::fsync_parent_dir(path)
            .map_err(RestoreFailure::retryable)?;
        Ok(saved)
    } else {
        Ok(state)
    }
}
