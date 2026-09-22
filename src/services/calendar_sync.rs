//! Managed calendar orchestration. One target executor handles every consenting account.
pub(crate) mod model;
mod recovery;
mod worker;
use crate::db::calendar_sync::{self as db, Binding, CalendarDbError, Receipt};
use crate::services::kakao::{KakaoError, account};
use model::EventContent;
pub(crate) use recovery::{Recovery, recover};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
pub(crate) use worker::calendar_loop;
#[cfg(test)]
pub(crate) use worker::execute as execute_for_test;

#[derive(Debug, thiserror::Error)]
pub enum CalendarError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Storage(#[from] CalendarDbError),
    #[error(transparent)]
    Provider(#[from] KakaoError),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateEvent {
    pub account_ids: Vec<String>,
    pub title: String,
    pub time: model::EventTime,
    pub description: Option<String>,
    pub location: Option<model::Location>,
    pub reminders: Option<Vec<i32>>,
}

pub(crate) fn fingerprint(method: &str, path: &str, body: &Value) -> Result<String, CalendarError> {
    Ok(crate::db::idempotency::fingerprint_request(
        method,
        path,
        &serde_json::to_vec(body).map_err(|_| CalendarError::Invalid("invalid request"))?,
    ))
}

pub(crate) async fn check_account(
    pool: &PgPool,
    account_id: &str,
) -> Result<Binding, CalendarError> {
    let client = account::shared_client(account_id, true).await?;
    client.require_durable_credentials().await?;
    let identity = client.calendar_identity().await?;
    Ok(db::bind_account(pool, account_id, identity.app_id, identity.user_id).await?)
}

pub(crate) async fn authorize_event(pool: &PgPool, event: Uuid) -> Result<(), CalendarError> {
    let accounts = db::event_accounts(pool, event).await?;
    if accounts.is_empty() {
        return Err(CalendarDbError::NotFound.into());
    }
    for id in accounts {
        account::authorize_calendar(&id)?;
    }
    Ok(())
}

pub(crate) async fn create(
    pool: &PgPool,
    key: &str,
    mut input: CreateEvent,
) -> Result<Receipt, CalendarError> {
    input.account_ids.sort();
    if input.account_ids.is_empty()
        || input.account_ids.len() > 16
        || input.account_ids.windows(2).any(|pair| pair[0] == pair[1])
    {
        return Err(CalendarError::Invalid(
            "one to sixteen distinct accountIds required",
        ));
    }
    for id in &input.account_ids {
        account::authorize_calendar(id)?;
    }
    let event = EventContent {
        title: input.title,
        time: input.time,
        description: input.description,
        location: input.location,
        reminders: input.reminders,
    };
    event.validate().map_err(CalendarError::Invalid)?;
    let content =
        serde_json::to_value(&event).map_err(|_| CalendarError::Invalid("invalid event"))?;
    let fp = fingerprint(
        "POST",
        "/api/kakao/calendar/events",
        &json!({"accountIds":input.account_ids,"content":content}),
    )?;
    if let Some(receipt) = db::replay(pool, key, &fp).await? {
        authorize_event(pool, receipt.event_id).await?;
        return Ok(receipt);
    }
    let mut bindings = Vec::new();
    for id in &input.account_ids {
        bindings.push(check_account(pool, id).await?);
    }
    if bindings
        .iter()
        .any(|binding| binding.app_id != bindings[0].app_id)
    {
        return Err(CalendarError::Invalid(
            "calendar accounts must belong to the same Kakao app",
        ));
    }
    Ok(db::create(pool, key, &fp, &content, &bindings).await?)
}

pub(crate) async fn mutate(
    pool: &PgPool,
    event: Uuid,
    key: &str,
    input: Value,
    delete: bool,
) -> Result<Receipt, CalendarError> {
    authorize_event(pool, event).await?;
    let fp = fingerprint(
        if delete { "DELETE" } else { "PATCH" },
        &format!("/api/kakao/calendar/events/{event}"),
        &input,
    )?;
    if let Some(receipt) = db::replay(pool, key, &fp).await? {
        return Ok(receipt);
    }
    let expected = input
        .get("expectedRevision")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .ok_or(CalendarError::Invalid("positive expectedRevision required"))?;
    let current = db::get(pool, event).await?;
    let value = patched_snapshot(&current, &input, expected, delete)?;
    Ok(db::mutate(
        pool,
        db::Mutation {
            event,
            key,
            fingerprint: &fp,
            expected_revision: expected,
            content: &value,
            delete,
        },
    )
    .await?)
}

fn patched_snapshot(
    current: &Value,
    input: &Value,
    expected: i64,
    delete: bool,
) -> Result<Value, CalendarError> {
    // The patch must be based on the exact revision checked under the DB lock.
    // Otherwise a request for a future revision could apply stale content after
    // waiting for another writer to commit that revision.
    if current["deleted"] == true || current["revision"].as_i64() != Some(expected) {
        return Err(db::CalendarDbError::Conflict.into());
    }
    let content: EventContent = serde_json::from_value(current["content"].clone())
        .map_err(|_| CalendarError::Invalid("stored event invalid"))?;
    let updated = if delete {
        if input.as_object().is_none_or(|obj| obj.len() != 1) {
            return Err(CalendarError::Invalid(
                "delete accepts expectedRevision only",
            ));
        }
        content
    } else {
        content.patched(input).map_err(CalendarError::Invalid)?
    };
    let value =
        serde_json::to_value(updated).map_err(|_| CalendarError::Invalid("invalid event"))?;
    Ok(value)
}

pub(crate) fn error_code(error: &KakaoError) -> &'static str {
    match error {
        KakaoError::Disabled => "calendar_disabled",
        KakaoError::UnknownAccount => "account_disabled",
        KakaoError::MissingCredentials => "credentials_missing",
        KakaoError::ConsentRequired => "calendar_consent_required",
        KakaoError::ReauthorizationRequired => "reauthorization_required",
        KakaoError::CredentialPersistence => "credential_persistence_failed",
        KakaoError::TransientAuth => "auth_temporarily_unavailable",
        KakaoError::BindingChanged => "account_binding_changed",
        KakaoError::DeliveryUnknown => "needs_reconcile",
        KakaoError::ProviderRejected(429) => "rate_limited",
        KakaoError::ProviderRejected(_) | KakaoError::ProviderResult(_) => "provider_rejected",
        _ => "configuration_invalid",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_requires_its_observed_revision_and_preserves_other_fields() {
        let mut current = json!({"revision": 1, "deleted": false, "content": {
            "title": "original", "description": "original description",
            "time": {"startAt": "2026-09-30T10:00:00+09:00", "endAt": "2026-09-30T11:00:00+09:00", "timeZone": "Asia/Seoul"}
        }});
        let patch = json!({"expectedRevision": 2, "title": "second writer"});
        assert!(matches!(
            patched_snapshot(&current, &patch, 2, false),
            Err(CalendarError::Storage(db::CalendarDbError::Conflict))
        ));
        current["revision"] = json!(2);
        current["content"]["description"] = json!("first writer");
        let updated = patched_snapshot(&current, &patch, 2, false).unwrap();
        assert_eq!(updated["title"], "second writer");
        assert_eq!(updated["description"], "first writer");
        assert!(matches!(
            patched_snapshot(&current, &patch, 1, false),
            Err(CalendarError::Storage(db::CalendarDbError::Conflict))
        ));
    }
}
