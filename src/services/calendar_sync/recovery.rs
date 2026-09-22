//! Operator recovery policy belongs to the calendar service, independent of HTTP transport.
use super::{CalendarError, account, authorize_event, check_account, db, model::EventContent};
use db::{CalendarDbError, RecoveryResolution};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Recovery {
    resolution: RecoveryResolution,
    remote_event_id: Option<String>,
    note: String,
    credential_owner_restarted: bool,
}

impl Recovery {
    fn validate(&self) -> Result<(), CalendarError> {
        if self.note.trim().len() < 10 || self.note.len() > 1000 {
            return Err(CalendarError::Invalid(
                "recovery requires a 10 to 1000 byte non-secret audit note",
            ));
        }
        if self.resolution == RecoveryResolution::Adopt {
            if self
                .remote_event_id
                .as_deref()
                .is_none_or(|id| id.is_empty() || id.len() > 512)
            {
                return Err(CalendarError::Invalid(
                    "remoteEventId required for adoption",
                ));
            }
        } else if self.remote_event_id.is_some() {
            return Err(CalendarError::Invalid(
                "remoteEventId is only valid for adoption",
            ));
        }
        if self.resolution != RecoveryResolution::Retry && !self.credential_owner_restarted {
            return Err(CalendarError::Invalid(
                "stop and restart the old credential owner before resolving uncertainty",
            ));
        }
        Ok(())
    }
}

pub(crate) async fn recover(
    pool: &PgPool,
    event: Uuid,
    operation: Uuid,
    request: Recovery,
) -> Result<RecoveryResolution, CalendarError> {
    authorize_event(pool, event).await?;
    request.validate()?;
    let claim = db::recovery_target(pool, event, operation).await?;
    let binding = check_account(pool, &claim.account_id).await?;
    if binding.app_id != claim.app_id || binding.user_id != claim.user_id {
        return Err(CalendarDbError::Binding.into());
    }
    if request.resolution == RecoveryResolution::Adopt {
        let id = request
            .remote_event_id
            .as_deref()
            .ok_or(CalendarError::Invalid(
                "remoteEventId required for adoption",
            ))?;
        if claim.action == "delete" || claim.remote_id.as_deref().is_some_and(|known| known != id) {
            return Err(CalendarDbError::Conflict.into());
        }
        let expected: EventContent = serde_json::from_value(claim.snapshot.clone())
            .map_err(|_| CalendarError::Invalid("invalid stored intent"))?;
        let desired = expected.provider_json().map_err(CalendarError::Invalid)?;
        let client = account::shared_client(&claim.account_id, true).await?;
        let detail = client.calendar_detail(id).await?;
        if !crate::services::kakao::calendar::matches_adoption(&detail, id, &desired) {
            return Err(CalendarDbError::Conflict.into());
        }
    }
    db::recover(
        pool,
        &claim,
        request.resolution,
        request.remote_event_id.as_deref(),
        &request.note,
    )
    .await?;
    Ok(request.resolution)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recovery_requires_explicit_resolution_and_evidence() {
        let request = json!({"resolution":"retry","note":"Consent repaired and verified","credentialOwnerRestarted":false});
        let parsed: Recovery = serde_json::from_value(request.clone()).unwrap();
        assert!(parsed.validate().is_ok());
        for (field, value) in [("resolution", json!("guess")), ("confirmed", json!(true))] {
            let mut invalid = request.clone();
            invalid[field] = value;
            assert!(serde_json::from_value::<Recovery>(invalid).is_err());
        }
        for patch in [
            json!({"note":"short"}),
            json!({"remoteEventId":"unexpected"}),
            json!({"resolution":"adopt"}),
            json!({"resolution":"confirm_not_applied"}),
        ] {
            let mut invalid = request.clone();
            invalid
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            assert!(
                serde_json::from_value::<Recovery>(invalid)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        let valid: Recovery = serde_json::from_value(json!({"resolution":"adopt","remoteEventId":"known-id","note":"Verified service-created event after restart","credentialOwnerRestarted":true})).unwrap();
        assert!(valid.validate().is_ok());
    }
}
