//! Provider-neutral scheduled target validation.
//!
//! HTTP handlers use [`ScheduledProviderTargetsBody`] while the scheduler
//! persists a smaller confirmed plan. Raw Kakao friend UUIDs are deliberately
//! absent from Debug output and API summaries.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use thiserror::Error;

use crate::services::kakao::{KakaoClient, KakaoError, validate_recipients};
use crate::services::kakao_message::{KakaoMessage, validate_message};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ScheduledProviderTargetsBody {
    pub kakao: ScheduledKakaoTargetBody,
}

impl fmt::Debug for ScheduledProviderTargetsBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScheduledProviderTargetsBody")
            .field("kakao_account_id", &self.kakao.account_id)
            .field("kakao_friend_count", &self.kakao.friend_uuids.len())
            .field("kakao_send_to_self", &self.kakao.send_to_self)
            .field("kakao_image_enabled", &self.kakao.image_url.is_some())
            .field("confirmed", &self.kakao.confirmed)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ScheduledKakaoTargetBody {
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub friend_uuids: Vec<String>,
    #[serde(default)]
    pub send_to_self: bool,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub confirmed: bool,
}

#[derive(Debug, Error)]
pub(crate) enum ProviderTargetError {
    #[error("provider target delivery is not confirmed")]
    NotConfirmed,
    #[error("at least one Kakao friend or sendToSelf target is required")]
    NoAudience,
    #[error("Kakao target configuration is invalid")]
    InvalidTarget,
    #[error("Kakao delivery is not available")]
    KakaoUnavailable(#[source] KakaoError),
    #[error("provider target serialization failed")]
    Serialization,
}

impl ProviderTargetError {
    pub(crate) fn is_unavailable(&self) -> bool {
        matches!(self, Self::KakaoUnavailable(_))
    }
}

#[derive(Clone)]
pub(crate) struct ValidatedProviderTargets {
    pub stored: JsonValue,
    pub summary: JsonValue,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoredProviderTargets {
    pub kakao: StoredKakaoTarget,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoredKakaoTarget {
    pub account_id: String,
    pub friend_uuids: Vec<String>,
    pub send_to_self: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
}

pub(crate) fn validate_for_process(
    body: &ScheduledProviderTargetsBody,
    content: &str,
) -> Result<ValidatedProviderTargets, ProviderTargetError> {
    let requested_account = body
        .kakao
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let client = KakaoClient::from_process(requested_account)
        .map_err(ProviderTargetError::KakaoUnavailable)?;
    validate_resolved(body, content, client.account_id())
}

fn validate_resolved(
    body: &ScheduledProviderTargetsBody,
    content: &str,
    account_id: &str,
) -> Result<ValidatedProviderTargets, ProviderTargetError> {
    if !body.kakao.confirmed {
        return Err(ProviderTargetError::NotConfirmed);
    }
    let friend_count = body.kakao.friend_uuids.len();
    if friend_count == 0 && !body.kakao.send_to_self {
        return Err(ProviderTargetError::NoAudience);
    }
    if friend_count > 0 {
        validate_recipients(&body.kakao.friend_uuids)
            .map_err(|_| ProviderTargetError::InvalidTarget)?;
    }
    validate_message(&KakaoMessage {
        text: content.to_string(),
        image_url: body.kakao.image_url.clone(),
    })
    .map_err(|_| ProviderTargetError::InvalidTarget)?;

    let stored = StoredProviderTargets {
        kakao: StoredKakaoTarget {
            account_id: account_id.to_string(),
            friend_uuids: body.kakao.friend_uuids.clone(),
            send_to_self: body.kakao.send_to_self,
            image_url: body.kakao.image_url.clone(),
        },
    };
    let stored = serde_json::to_value(stored).map_err(|_| ProviderTargetError::Serialization)?;
    let summary = json!({
        "kakao": {
            "enabled": true,
            "accountId": account_id,
            "friendCount": friend_count,
            "sendToSelf": body.kakao.send_to_self,
            "contentMode": if body.kakao.image_url.is_some() { "feed" } else { "text" }
        }
    });
    Ok(ValidatedProviderTargets { stored, summary })
}

pub(crate) fn decode_stored(
    value: &JsonValue,
    content: &str,
) -> Result<(StoredProviderTargets, KakaoMessage), ProviderTargetError> {
    let stored: StoredProviderTargets =
        serde_json::from_value(value.clone()).map_err(|_| ProviderTargetError::InvalidTarget)?;
    if stored.kakao.friend_uuids.is_empty() && !stored.kakao.send_to_self {
        return Err(ProviderTargetError::NoAudience);
    }
    if !stored.kakao.friend_uuids.is_empty() {
        validate_recipients(&stored.kakao.friend_uuids)
            .map_err(|_| ProviderTargetError::InvalidTarget)?;
    }
    let message = KakaoMessage {
        text: content.to_string(),
        image_url: stored.kakao.image_url.clone(),
    };
    validate_message(&message).map_err(|_| ProviderTargetError::InvalidTarget)?;
    Ok((stored, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(friends: &[&str], send_to_self: bool, confirmed: bool) -> ScheduledProviderTargetsBody {
        ScheduledProviderTargetsBody {
            kakao: ScheduledKakaoTargetBody {
                account_id: Some("default".to_string()),
                friend_uuids: friends.iter().map(|value| (*value).to_string()).collect(),
                send_to_self,
                image_url: None,
                confirmed,
            },
        }
    }

    #[test]
    fn debug_and_summary_do_not_expose_friend_uuids() {
        let private_uuid = "private-friend-uuid";
        let body = body(&[private_uuid], true, true);
        assert!(!format!("{body:?}").contains(private_uuid));
        let validated = validate_resolved(&body, "예약 알림", "default").unwrap();
        assert!(!validated.summary.to_string().contains(private_uuid));
        assert_eq!(validated.summary["kakao"]["friendCount"], 1);
    }

    #[test]
    fn target_requires_confirmation_and_an_audience() {
        assert!(matches!(
            validate_resolved(&body(&["friend"], false, false), "hello", "default"),
            Err(ProviderTargetError::NotConfirmed)
        ));
        assert!(matches!(
            validate_resolved(&body(&[], false, true), "hello", "default"),
            Err(ProviderTargetError::NoAudience)
        ));
        assert!(validate_resolved(&body(&[], true, true), "hello", "default").is_ok());
    }

    #[test]
    fn schema_is_closed_and_kakao_text_limit_is_reused() {
        let unknown = serde_json::json!({
            "kakao": {
                "friendUuids": ["friend"],
                "confirmed": true,
                "unexpected": true
            }
        });
        assert!(serde_json::from_value::<ScheduledProviderTargetsBody>(unknown).is_err());
        assert!(
            validate_resolved(&body(&["friend"], false, true), &"가".repeat(201), "default")
                .is_err()
        );
    }

    #[test]
    fn stored_plan_round_trip_revalidates_without_confirmation_field() {
        let validated =
            validate_resolved(&body(&["friend"], true, true), "hello", "default").unwrap();
        assert!(validated.stored["kakao"].get("confirmed").is_none());
        let (stored, message) = decode_stored(&validated.stored, "hello").unwrap();
        assert_eq!(stored.kakao.account_id, "default");
        assert_eq!(message.text, "hello");
    }
}
