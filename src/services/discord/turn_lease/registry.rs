use super::*;
use crate::services::discord::health::HealthRegistry;

async fn runtime(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<Arc<SharedData>, String> {
    registry
        .shared_for_provider_on_channel(provider, channel)
        .await
        .ok_or_else(|| "unique provider runtime unavailable".into())
}

pub(crate) async fn inspect(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<Option<LeaseIdentity>, String> {
    let shared = runtime(registry, provider, channel).await?;
    let lease = identity(&shared, provider, channel).await?;
    if let Some(expected) = lease.as_ref() {
        let row = inflight::load_inflight_state(provider, channel.get())
            .ok_or("lease cannot be released: matching inflight identity is missing")?;
        if row.effective_finalizer_turn_id() != expected.user_message_id
            || row.turn_nonce.as_deref() != Some(expected.turn_nonce.as_str())
            || row.restart_mode.is_some()
            || row.rebind_origin
        {
            return Err(
                "lease cannot be released: inflight identity differs or is protected".into(),
            );
        }
    }
    Ok(lease)
}

pub(crate) async fn release(
    registry: &HealthRegistry,
    request: ReleaseRequest,
) -> Result<serde_json::Value, String> {
    let provider = ProviderKind::from_str(&request.expected.provider).ok_or("unknown provider")?;
    let channel = ChannelId::new(request.expected.channel_id);
    let shared = runtime(registry, &provider, channel).await?;
    release_on(&shared, &provider, channel, request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::make_shared_data_for_tests_with_storage;
    use crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root;
    use crate::services::provider::CancelToken;
    use serenity::model::id::UserId;

    #[tokio::test]
    async fn turn_lease_registry_selects_owner_and_refuses_ambiguity() {
        let registry = HealthRegistry::new();
        let first = make_shared_data_for_tests_with_storage(None);
        let second = make_shared_data_for_tests_with_storage(None);
        first.settings.write().await.allowed_channel_ids = vec![57541];
        second.settings.write().await.allowed_channel_ids = vec![57542];
        registry.register("codex".into(), first.clone()).await;
        registry.register("codex".into(), second.clone()).await;
        let channel = ChannelId::new(57542);
        let selected = runtime(&registry, &ProviderKind::Codex, channel)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&selected, &second));
        first
            .settings
            .write()
            .await
            .allowed_channel_ids
            .push(channel.get());
        assert!(
            runtime(&registry, &ProviderKind::Codex, channel)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn turn_lease_registry_inspect_rejects_missing_inflight() {
        with_isolated_runtime_root(|| async {
            let registry = HealthRegistry::new();
            let shared = make_shared_data_for_tests_with_storage(None);
            let channel = ChannelId::new(57543);
            let token = Arc::new(CancelToken::new());
            shared
                .mailbox(channel)
                .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(123))
                .await;
            registry.register("codex".into(), shared.clone()).await;
            let error = inspect(&registry, &ProviderKind::Codex, channel)
                .await
                .unwrap_err();
            assert!(error.contains("matching inflight identity is missing"));
            assert!(
                shared
                    .mailbox(channel)
                    .snapshot()
                    .await
                    .cancel_token
                    .is_some()
            );
            assert!(!token.is_completion_cleanup());
        })
        .await;
    }
}
