//! HTTP adaptation for scheduled external-provider targets.

use axum::http::StatusCode;
use serde_json::{Map, Value as JsonValue};

use crate::db::scheduled_messages::{ScheduledMessagePatch, ScheduledMessageRow};
use crate::error::AppError;
use crate::services::scheduled_messages::provider_targets::{
    ProviderTargetError, ScheduledProviderTargetsBody, ValidatedProviderTargets, decode_stored,
    validate_for_process,
};

use super::app_error;

pub(super) fn prepare_create(
    body: Option<&JsonValue>,
    content: &str,
    delivery_kind: &str,
) -> Result<Option<ValidatedProviderTargets>, AppError> {
    let Some(value) = body else {
        return Ok(None);
    };
    if delivery_kind != crate::db::scheduled_messages::KIND_PUSH {
        return Err(app_error(
            StatusCode::BAD_REQUEST,
            "providerTargets is only valid for push delivery",
        ));
    }
    let body: ScheduledProviderTargetsBody =
        serde_json::from_value(value.clone()).map_err(|error| {
            app_error(
                StatusCode::BAD_REQUEST,
                format!("providerTargets must be a valid object: {error}"),
            )
        })?;
    validate_for_process(&body, content)
        .map(Some)
        .map_err(map_provider_error)
}

pub(super) fn apply_patch(
    body: &Map<String, JsonValue>,
    patch: &mut ScheduledMessagePatch,
    existing: &ScheduledMessageRow,
) -> Result<bool, AppError> {
    let effective_content = patch
        .content
        .clone()
        .unwrap_or_else(|| existing.content.clone());
    match body.get("providerTargets") {
        Some(JsonValue::Null) => {
            patch.provider_targets = Some(None);
            patch.provider_target_summary = Some(None);
        }
        Some(value) => {
            if existing.delivery_kind != crate::db::scheduled_messages::KIND_PUSH {
                return Err(app_error(
                    StatusCode::BAD_REQUEST,
                    "providerTargets is only valid for push delivery",
                ));
            }
            let parsed: ScheduledProviderTargetsBody = serde_json::from_value(value.clone())
                .map_err(|error| {
                    app_error(
                        StatusCode::BAD_REQUEST,
                        format!("providerTargets must be a valid object or null: {error}"),
                    )
                })?;
            let validated =
                validate_for_process(&parsed, &effective_content).map_err(map_provider_error)?;
            patch.provider_targets = Some(Some(validated.stored));
            patch.provider_target_summary = Some(Some(validated.summary));
        }
        None => {
            if let Some(stored) = existing.provider_targets.as_ref() {
                decode_stored(stored, &effective_content).map_err(|_| {
                    app_error(
                        StatusCode::BAD_REQUEST,
                        "content must contain 1 to 200 characters while providerTargets is enabled",
                    )
                })?;
            }
        }
    }
    Ok(patch
        .provider_targets
        .as_ref()
        .map_or(existing.provider_targets.is_some(), Option::is_some))
}

fn map_provider_error(error: ProviderTargetError) -> AppError {
    if error.is_unavailable() {
        app_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Kakao delivery must be enabled with valid account credentials before scheduling",
        )
    } else if matches!(&error, ProviderTargetError::Serialization) {
        app_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to prepare scheduled provider targets",
        )
    } else {
        app_error(StatusCode::BAD_REQUEST, error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_or_unknown_provider_target_fields_are_rejected() {
        let value = serde_json::json!({
            "kakao": {
                "friendUuids": ["friend"],
                "confirmed": true,
                "unknown": "field"
            }
        });
        assert!(serde_json::from_value::<ScheduledProviderTargetsBody>(value).is_err());
    }
}
