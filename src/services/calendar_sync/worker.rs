use super::{error_code, model::EventContent};
use crate::db::calendar_sync as db;
use crate::services::kakao::{KakaoError, account};
use sqlx::PgPool;
use std::sync::Arc;

pub(crate) async fn calendar_loop(pool: Arc<PgPool>) {
    loop {
        if let Err(_error) = tick(&pool).await {
            // SQL errors can include row content. Keep calendar diagnostics content-free.
            tracing::warn!("calendar worker storage operation failed");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

pub(crate) async fn tick(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Invalid configuration disables new claims, not database lease recovery.
    let accounts = account::calendar_accounts().unwrap_or_default();
    // Nodes lacking a usable, exclusively owned credential store must not consume work.
    let mut available = Vec::new();
    for id in accounts {
        if let Ok(client) = account::shared_client(&id, true).await {
            if client.require_durable_credentials().await.is_ok() {
                available.push(id);
            }
        }
    }
    // Claim also recovers expired leases. An empty allowlist prevents new claims,
    // but must still expose interrupted writes for operator reconciliation.
    let Some(claim) = db::claim(pool, &available).await? else {
        return Ok(());
    };
    let client = match account::shared_client(&claim.account_id, true).await {
        Ok(client) => client,
        Err(error) => {
            return db::fail(pool, &claim, "blocked", error_code(&error)).await;
        }
    };
    let identity = match client.calendar_identity().await {
        Ok(identity) => identity,
        Err(error) => {
            let retry = matches!(
                error,
                KakaoError::TransientAuth
                    | KakaoError::DeliveryUnknown
                    | KakaoError::ProviderRejected(429)
            ) && claim.attempts < 5;
            return db::fail(
                pool,
                &claim,
                if retry { "queued" } else { "blocked" },
                error_code(&error),
            )
            .await;
        }
    };
    if identity.app_id != claim.app_id || identity.user_id != claim.user_id {
        return db::fail(pool, &claim, "blocked", "account_binding_changed").await;
    }
    if let Err(error) = account::authorize_calendar(&claim.account_id) {
        return db::fail(pool, &claim, "blocked", error_code(&error)).await;
    }
    if !db::dispatch(pool, &claim).await? {
        return Ok(());
    }
    execute(pool, &claim, &client).await
}

pub(crate) async fn execute(
    pool: &PgPool,
    claim: &db::Claim,
    client: &crate::services::kakao::KakaoClient,
) -> Result<(), sqlx::Error> {
    let content: EventContent = match serde_json::from_value(claim.snapshot.clone()) {
        Ok(content) => content,
        Err(_) => return db::fail(pool, claim, "rejected", "invalid_stored_intent").await,
    };
    let provider_event = match content.provider_json() {
        Ok(event) => event,
        Err(_) => return db::fail(pool, claim, "rejected", "invalid_stored_intent").await,
    };
    let result = if claim.action == "delete" {
        match claim.remote_id.as_deref() {
            Some(id) => client.calendar_delete(id).await.map(|()| None),
            None => Ok(None), // Earlier undispatched create was canceled; unknown creates remain barriers.
        }
    } else {
        match claim.remote_id.as_deref() {
            Some(id) => client
                .calendar_update(id, &provider_event)
                .await
                .map(|()| Some(id.to_string())),
            None => client.calendar_create(&provider_event).await.map(Some),
        }
    };
    match result {
        Ok(remote) => {
            db::complete(pool, claim, remote.as_deref()).await?;
        }
        Err(error) => {
            let status = match error {
                KakaoError::DeliveryUnknown
                | KakaoError::TransientAuth
                | KakaoError::CredentialPersistence => "needs_reconcile",
                KakaoError::ConsentRequired
                | KakaoError::ReauthorizationRequired
                | KakaoError::BindingChanged => "blocked",
                _ => "rejected",
            };
            db::fail(pool, claim, status, error_code(&error)).await?;
        }
    }
    Ok(())
}
