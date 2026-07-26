use super::*;

pub(super) fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub(super) fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn prepared_matches(
    intent: &StatusPanelTransitionIntent,
    prior_binding: Option<StatusPanelSingletonBinding>,
    identity: Option<&InflightTurnIdentity>,
    operation: StatusPanelTransitionOperation,
) -> bool {
    intent.state == StatusPanelTransitionState::Prepared
        && expected_prior(intent) == prior_binding
        && intent.identity.as_ref() == identity
        && intent.operation == operation
}

fn coalesced_content(
    existing_content: &str,
    requested_content: &str,
    operation: StatusPanelTransitionOperation,
) -> String {
    match operation {
        StatusPanelTransitionOperation::LiveBind => requested_content.to_string(),
        StatusPanelTransitionOperation::CompletionFallback => existing_content.to_string(),
    }
}

fn deterministic_nonce(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    prior_binding: Option<StatusPanelSingletonBinding>,
    identity: Option<&InflightTurnIdentity>,
    operation: StatusPanelTransitionOperation,
    prepared_at_unix_ms: u64,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(provider.as_str());
    hasher.update([0]);
    hasher.update(token_hash);
    hasher.update([0]);
    hasher.update(channel_id.to_le_bytes());
    hasher.update([match operation {
        StatusPanelTransitionOperation::LiveBind => 0,
        StatusPanelTransitionOperation::CompletionFallback => 1,
    }]);
    if let Some(prior) = prior_binding {
        hasher.update(prior.panel_message_id.to_le_bytes());
        hasher.update(prior.generation.to_le_bytes());
    }
    if let Some(identity) = identity {
        hasher.update(identity.user_msg_id.to_le_bytes());
        hasher.update(identity.started_at.as_bytes());
        if let Some(session) = identity.tmux_session_name.as_deref() {
            hasher.update(session.as_bytes());
        }
        if let Some(offset) = identity.turn_start_offset {
            hasher.update(offset.to_le_bytes());
        }
    }
    hasher.update(
        prepared_at_unix_ms
            .checked_div(duration_millis(PREPARED_RETENTION))
            .unwrap_or_default()
            .to_le_bytes(),
    );
    let digest = hex::encode(hasher.finalize());
    format!("adksp{}", &digest[..20])
}

pub(super) fn prepare_candidate_at(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    prior_binding: Option<StatusPanelSingletonBinding>,
    identity: Option<InflightTurnIdentity>,
    operation: StatusPanelTransitionOperation,
    content: &str,
    now_ms: u64,
) -> Result<PreparedStatusPanelTransition, String> {
    if channel_id == 0 || content.is_empty() {
        return Err("status panel transition requires channel and content".to_string());
    }
    let _guard = TRANSITION_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let root = root()?;
    let probe = path_in_root(&root, provider, token_hash, channel_id, "prepare");
    let _file_guard = lock_intent_path(&probe)?;
    let dir = channel_dir_in_root(&root, provider, token_hash, channel_id);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
            fs::read_dir(&dir).map_err(|error| error.to_string())?
        }
        Err(error) => return Err(error.to_string()),
    };
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        if !valid_canonical_name(&path) {
            continue;
        }
        match load_scoped_in_root(&path, provider, token_hash, channel_id) {
            Ok(Some(mut intent))
                if prepared_matches(&intent, prior_binding, identity.as_ref(), operation) =>
            {
                let retained_until = intent
                    .prepared_at_unix_ms
                    .saturating_add(duration_millis(PREPARED_RETENTION));
                if intent.prepared_at_unix_ms != 0 && now_ms >= retained_until {
                    remove_in_root(&path)?;
                    continue;
                }
                let content = coalesced_content(&intent.content, content, operation);
                if intent.content != content {
                    intent.content = content;
                    validate_intent_shape(&intent)?;
                    save_in_root(&path, &intent)?;
                }
                return Ok(PreparedStatusPanelTransition {
                    nonce: intent.nonce,
                    content: intent.content,
                    send_now: now_ms >= intent.next_retry_at_unix_ms,
                });
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "isolating malformed status-panel transition intent"
                );
                quarantine_malformed_strict(&path)?;
            }
        }
    }
    let prepared_at_unix_ms = now_ms;
    let nonce = deterministic_nonce(
        provider,
        token_hash,
        channel_id,
        prior_binding,
        identity.as_ref(),
        operation,
        prepared_at_unix_ms,
    );
    let path = path_in_root(&root, provider, token_hash, channel_id, &nonce);
    let intent = StatusPanelTransitionIntent {
        nonce: nonce.clone(),
        provider: provider.as_str().to_string(),
        token_hash: token_hash.to_string(),
        channel_id,
        candidate_panel_id: None,
        prior_panel_id: prior_binding.map(|binding| binding.panel_message_id),
        prior_generation: prior_binding.map(|binding| binding.generation),
        generation: None,
        identity,
        operation,
        content: content.to_string(),
        state: StatusPanelTransitionState::Prepared,
        prepared_at_unix_ms,
        next_retry_at_unix_ms: prepared_at_unix_ms,
    };
    validate_intent_shape(&intent)?;
    save_in_root(&path, &intent)?;
    Ok(PreparedStatusPanelTransition {
        nonce,
        content: content.to_string(),
        send_now: true,
    })
}

pub(in crate::services::discord) fn prepare_candidate(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    prior_binding: Option<StatusPanelSingletonBinding>,
    identity: Option<InflightTurnIdentity>,
    operation: StatusPanelTransitionOperation,
    content: &str,
) -> Result<PreparedStatusPanelTransition, String> {
    prepare_candidate_at(
        provider,
        token_hash,
        channel_id,
        prior_binding,
        identity,
        operation,
        content,
        now_unix_ms(),
    )
}

pub(in crate::services::discord) fn classify_create_failure_status(
    status: Option<u16>,
) -> StatusPanelCreateFailureDisposition {
    if status.is_some_and(|status| matches!(status, 403 | 404 | 410)) {
        StatusPanelCreateFailureDisposition::PermanentRejected
    } else {
        StatusPanelCreateFailureDisposition::RetrySameNonce
    }
}

pub(super) fn record_create_failure_at(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    prepared: &PreparedStatusPanelTransition,
    disposition: StatusPanelCreateFailureDisposition,
    now_ms: u64,
) -> Result<(), String> {
    match disposition {
        StatusPanelCreateFailureDisposition::PermanentRejected => {
            let _ = cancel_prepared_candidate(provider, token_hash, channel_id, prepared)?;
            Ok(())
        }
        StatusPanelCreateFailureDisposition::RetrySameNonce => {
            update_intent_by_nonce(
                provider,
                token_hash,
                channel_id,
                &prepared.nonce,
                |intent| {
                    if intent.state != StatusPanelTransitionState::Prepared {
                        return Ok(());
                    }
                    if intent.prepared_at_unix_ms == 0 {
                        intent.prepared_at_unix_ms = now_ms;
                    }
                    intent.next_retry_at_unix_ms =
                        now_ms.saturating_add(duration_millis(PREPARED_RETRY_INTERVAL));
                    Ok(())
                },
            )?;
            Ok(())
        }
    }
}

pub(in crate::services::discord) fn record_create_failure(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    prepared: &PreparedStatusPanelTransition,
    disposition: StatusPanelCreateFailureDisposition,
) -> Result<(), String> {
    record_create_failure_at(
        provider,
        token_hash,
        channel_id,
        prepared,
        disposition,
        now_unix_ms(),
    )
}

pub(in crate::services::discord) fn record_serenity_create_failure(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    prepared: &PreparedStatusPanelTransition,
    error: &poise::serenity_prelude::Error,
) -> Result<StatusPanelCreateFailureDisposition, String> {
    let status = match error {
        poise::serenity_prelude::Error::Http(
            poise::serenity_prelude::http::HttpError::UnsuccessfulRequest(response),
        ) => Some(response.status_code.as_u16()),
        _ => None,
    };
    let disposition = classify_create_failure_status(status);
    record_create_failure(provider, token_hash, channel_id, prepared, disposition)?;
    Ok(disposition)
}

pub(super) fn prepared_recovery_disposition_at(
    intent: &StatusPanelTransitionIntent,
    now_ms: u64,
) -> PreparedRecoveryDisposition {
    if intent.prepared_at_unix_ms != 0
        && now_ms
            >= intent
                .prepared_at_unix_ms
                .saturating_add(duration_millis(PREPARED_RETENTION))
    {
        PreparedRecoveryDisposition::Retired
    } else if now_ms < intent.next_retry_at_unix_ms {
        PreparedRecoveryDisposition::Deferred
    } else {
        PreparedRecoveryDisposition::RetryNow
    }
}

fn retire_prepared_intent(
    provider: &ProviderKind,
    token_hash: &str,
    intent: &StatusPanelTransitionIntent,
) -> Result<(), String> {
    let prepared = PreparedStatusPanelTransition {
        nonce: intent.nonce.clone(),
        content: intent.content.clone(),
        send_now: false,
    };
    let _ = cancel_prepared_candidate(provider, token_hash, intent.channel_id, &prepared)?;
    Ok(())
}

pub(in crate::services::discord) async fn recover_with_transport<S, SendFuture, D, DeleteFuture>(
    provider: &ProviderKind,
    token_hash: &str,
    mut send_candidate: S,
    mut delete_candidate: D,
) -> usize
where
    S: FnMut(u64, String, String) -> SendFuture,
    SendFuture: std::future::Future<Output = Result<u64, StatusPanelCreateFailureDisposition>>,
    D: FnMut(u64, u64) -> DeleteFuture,
    DeleteFuture: std::future::Future<Output = StatusPanelRetirementOutcome>,
{
    let intents = match load_unreconciled(provider, token_hash) {
        Ok(intents) => intents,
        Err(error) => {
            tracing::warn!(error = %error, "failed to load status-panel transition intents");
            return 0;
        }
    };
    let mut resolved = 0;
    for mut intent in intents {
        if intent.state == StatusPanelTransitionState::Settled {
            let Some(candidate) = intent.candidate_panel_id else {
                continue;
            };
            // Settled is written only after singleton ownership commits. It no
            // longer participates in owner selection, so a newer generation may
            // safely replace the candidate while this bounded residue is removed.
            if status_panel_orphan_store::remove_pending_bind_checked(
                provider,
                token_hash,
                intent.channel_id,
                candidate,
            )
            .is_ok()
                && remove_intent_for_candidate(provider, token_hash, intent.channel_id, candidate)
                    .is_ok()
            {
                resolved += 1;
            }
            continue;
        }
        if intent.state == StatusPanelTransitionState::Prepared {
            match prepared_recovery_disposition_at(&intent, now_unix_ms()) {
                PreparedRecoveryDisposition::Deferred => continue,
                PreparedRecoveryDisposition::Retired => {
                    if retire_prepared_intent(provider, token_hash, &intent).is_ok() {
                        resolved += 1;
                    }
                    continue;
                }
                PreparedRecoveryDisposition::RetryNow => {}
            }
            let sent = send_candidate(
                intent.channel_id,
                intent.content.clone(),
                intent.nonce.clone(),
            )
            .await;
            let candidate = match sent {
                Ok(candidate) => candidate,
                Err(disposition) => {
                    let prepared = PreparedStatusPanelTransition {
                        nonce: intent.nonce.clone(),
                        content: intent.content.clone(),
                        send_now: false,
                    };
                    if record_create_failure(
                        provider,
                        token_hash,
                        intent.channel_id,
                        &prepared,
                        disposition,
                    )
                    .is_ok()
                        && disposition == StatusPanelCreateFailureDisposition::PermanentRejected
                    {
                        resolved += 1;
                    }
                    continue;
                }
            };
            let action = acknowledge_candidate(
                provider,
                token_hash,
                intent.channel_id,
                &PreparedStatusPanelTransition {
                    nonce: intent.nonce.clone(),
                    content: intent.content.clone(),
                    send_now: true,
                },
                candidate,
            );
            if matches!(action, StatusPanelTransitionAction::DeferDurability { .. }) {
                continue;
            }
            intent.candidate_panel_id = Some(candidate);
            intent.state = StatusPanelTransitionState::PendingBindDurable;
        }
        let Some(candidate) = intent.candidate_panel_id else {
            continue;
        };
        let action = reconcile_acknowledged(&intent);
        match action {
            StatusPanelTransitionAction::KeepCurrent { .. } => resolved += 1,
            StatusPanelTransitionAction::RetireCandidate => {
                let retirement = delete_candidate(intent.channel_id, candidate).await;
                if finalize_retirement(
                    provider,
                    token_hash,
                    intent.channel_id,
                    candidate,
                    retirement,
                )
                .unwrap_or(false)
                {
                    resolved += 1;
                }
            }
            StatusPanelTransitionAction::DeferDurability { .. }
            | StatusPanelTransitionAction::RecoverUnreconciled => {}
        }
    }
    resolved
}

pub(in crate::services::discord) async fn recover_unreconciled_with_delete<D, DeleteFuture>(
    provider: &ProviderKind,
    token_hash: &str,
    delete_candidate: D,
) -> usize
where
    D: FnMut(u64, u64) -> DeleteFuture,
    DeleteFuture: std::future::Future<Output = StatusPanelRetirementOutcome>,
{
    recover_with_transport(
        provider,
        token_hash,
        |_, _, _| async { Err(StatusPanelCreateFailureDisposition::RetrySameNonce) },
        delete_candidate,
    )
    .await
}
