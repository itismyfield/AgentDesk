//! Account-specific provider usage synchronization and cache writes.

use sqlx::PgPool;

use super::{fetch_codex_oauth_usage, rate_limit_sync};

pub(super) async fn upsert_rate_limit_cache_entry(
    pg_pool: &PgPool,
    provider: &str,
    profile_id: &str,
    data: &str,
    fetched_at: i64,
) {
    let profile_id = if profile_id.trim().is_empty() {
        "default"
    } else {
        profile_id
    };
    if let Err(error) = sqlx::query(
        "INSERT INTO rate_limit_cache (provider, profile_id, data, fetched_at)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (provider, profile_id)
         DO UPDATE SET data = EXCLUDED.data, fetched_at = EXCLUDED.fetched_at",
    )
    .bind(provider)
    .bind(profile_id)
    .bind(data)
    .bind(fetched_at)
    .execute(pg_pool)
    .await
    {
        tracing::warn!(
            "[rate-limit-sync] failed to upsert rate_limit_cache row for {provider}: {error}"
        );
    }
}

fn rate_limit_upsert_conflict_target() -> &'static str {
    "(provider, profile_id)"
}

pub(super) async fn sync_named_profile_rate_limits(pg_pool: &PgPool) {
    let catalog = crate::services::discord::org_schema::provider_auth_catalog();
    let now = chrono::Utc::now().timestamp();
    for (profile_id, def) in catalog {
        let Ok(provider) = crate::services::provider_auth_profile::intern_provider(&def.provider)
        else {
            continue;
        };
        if profile_id == crate::services::provider_auth_profile::DEFAULT_PROFILE_ID {
            continue;
        }
        let Ok(overlay) = crate::services::provider_auth_profile::resolve(
            provider.clone(),
            Some(&profile_id),
            None,
            &crate::services::discord::org_schema::provider_auth_catalog(),
        ) else {
            continue;
        };
        let Some(home) = overlay.home.as_ref() else {
            continue;
        };
        let result = match provider {
            crate::services::provider::ProviderKind::Claude => {
                crate::services::provider_auth::claude_oauth_token_from_home(home)
                    .ok_or_else(|| anyhow::anyhow!("no claude overlay token"))
            }
            crate::services::provider::ProviderKind::Codex => {
                crate::services::provider_auth::codex_access_token_from_home(home)
                    .ok_or_else(|| anyhow::anyhow!("no codex overlay token"))
            }
            crate::services::provider::ProviderKind::Grok => {
                crate::services::provider_auth::grok_token_from_home(home)
                    .ok_or_else(|| anyhow::anyhow!("no grok overlay token"))
            }
            _ => continue,
        };
        let token = match result {
            Ok(token) => token,
            Err(error) => {
                tracing::debug!("[rate-limit-sync] skip profile {profile_id}: {error}");
                continue;
            }
        };
        let buckets = match provider {
            crate::services::provider::ProviderKind::Claude => {
                rate_limit_sync::fetch_claude_oauth_usage(&token).await
            }
            crate::services::provider::ProviderKind::Codex => fetch_codex_oauth_usage(&token).await,
            crate::services::provider::ProviderKind::Grok => {
                rate_limit_sync::fetch_grok_billing_usage(&token).await
            }
            _ => continue,
        };
        match buckets {
            Ok(buckets) => {
                let data = serde_json::json!({ "buckets": buckets }).to_string();
                upsert_rate_limit_cache_entry(pg_pool, provider.as_str(), &profile_id, &data, now)
                    .await;
            }
            Err(error) => {
                tracing::warn!(
                    "[rate-limit-sync] {provider} profile {profile_id} fetch failed: {error}",
                    provider = provider.as_str()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_011_upsert_conflict_is_provider_and_profile() {
        assert_eq!(
            super::rate_limit_upsert_conflict_target(),
            "(provider, profile_id)"
        );
    }
}
