//! Off-by-default iMessage admission reserves each GUID before delivery can have an effect.

use std::future::Future;
use std::sync::OnceLock;

use axum::{Json, http::StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::db::imsg_reply::{
    AdmissionIdentity, AdmissionSettlement, Obligation, read_obligation, reserve_admission,
    settle_admission,
};
use crate::services::discord::health::{HumanInputDelivery, HumanInputError};

pub(crate) type AdmissionResponse = (StatusCode, Json<Value>);

pub(crate) struct AdmissionInput<'a> {
    pub origin_id: &'a str,
    pub agent_id: &'a str,
    pub channel_id: u64,
    pub author_id: u64,
    pub source: &'a str,
    pub text: &'a str,
}

pub(crate) fn parse_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on"
        )
    })
}

/// Cache the kill switch once so activation requires a process restart.
pub(crate) fn is_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| parse_enabled(std::env::var("ADK_IMSG_OBLIGATIONS").ok().as_deref()))
}

pub(crate) fn enabled_for_source(enabled: bool, source: &str) -> bool {
    enabled && source.trim() == "imessage"
}

fn request_hash(input: &AdmissionInput<'_>) -> String {
    let normalized = json!([
        input.text.trim(),
        input.source.trim(),
        input.channel_id.to_string(),
        input.author_id.to_string(),
    ]);
    format!("{:x}", Sha256::digest(normalized.to_string().as_bytes()))
}

fn failure(status: StatusCode, error: &str) -> AdmissionResponse {
    (status, Json(json!({"ok": false, "error": error})))
}

fn replay(row: &Obligation, identity: &AdmissionIdentity<'_>) -> AdmissionResponse {
    if row.source != "imessage"
        || row.agent_id != identity.agent_id
        || row.channel_id != identity.channel_id
        || row.person_id != identity.person_id
        || row.request_hash != identity.request_hash
    {
        return failure(StatusCode::CONFLICT, "origin_conflict");
    }
    match row.admission.as_str() {
        "admitting" => failure(StatusCode::ACCEPTED, "admission_pending"),
        "unknown" => failure(StatusCode::ACCEPTED, "admission_unknown"),
        "admitted" | "refused" => {
            let stored = row.admission_result.as_ref();
            let status = stored
                .and_then(|stored| stored["status"].as_u64())
                .and_then(|status| u16::try_from(status).ok())
                .and_then(|status| StatusCode::from_u16(status).ok());
            match (status, stored.and_then(|stored| stored.get("body"))) {
                (Some(status), Some(body)) => (status, Json(body.clone())),
                _ => failure(StatusCode::SERVICE_UNAVAILABLE, "admission_unavailable"),
            }
        }
        _ => failure(StatusCode::SERVICE_UNAVAILABLE, "admission_unavailable"),
    }
}

/// The enabled iMessage route supplies authorization and its unchanged delivery/response functions.
pub(crate) async fn deliver_once<A, AF, D, DF, S>(
    pool: &PgPool,
    input: &AdmissionInput<'_>,
    authorize: A,
    deliver: D,
    serialize: S,
) -> AdmissionResponse
where
    A: FnOnce() -> AF,
    AF: Future<Output = Result<(), HumanInputError>>,
    D: FnOnce() -> DF,
    DF: Future<Output = Result<HumanInputDelivery, HumanInputError>>,
    S: FnOnce(Result<HumanInputDelivery, HumanInputError>) -> AdmissionResponse,
{
    if let Err(error) = authorize().await {
        return serialize(Err(error));
    }
    let Ok(person_id) = i64::try_from(input.author_id) else {
        return failure(StatusCode::UNPROCESSABLE_ENTITY, "invalid_author_id");
    };
    let channel_id = input.channel_id.to_string();
    let request_hash = request_hash(input);
    let identity = AdmissionIdentity {
        origin_id: input.origin_id,
        agent_id: input.agent_id,
        channel_id: &channel_id,
        person_id,
        request_hash: &request_hash,
    };
    match reserve_admission(pool, &identity).await {
        Ok(true) => {}
        Ok(false) => {
            // Read after the INSERT completes: its conflict snapshot may predate the winning row.
            return match read_obligation(pool, input.origin_id).await {
                Ok(Some(row)) => replay(&row, &identity),
                _ => failure(StatusCode::SERVICE_UNAVAILABLE, "admission_unavailable"),
            };
        }
        Err(_) => return failure(StatusCode::SERVICE_UNAVAILABLE, "admission_unavailable"),
    }

    let result = deliver().await;
    let (admission, kind, confirmed, turn_id) = match &result {
        Ok(HumanInputDelivery::Started { turn_id }) => (
            "admitted",
            Some("started"),
            Some(true),
            Some(turn_id.clone()),
        ),
        Ok(HumanInputDelivery::Queued { turn_id, .. }) => (
            "admitted",
            Some("queued"),
            Some(true),
            Some(turn_id.clone()),
        ),
        Ok(HumanInputDelivery::Injected { turn_id }) => {
            ("admitted", Some("injected"), Some(true), turn_id.clone())
        }
        Ok(HumanInputDelivery::Unconfirmed { turn_id, .. }) => (
            "admitted",
            Some("unconfirmed"),
            Some(false),
            turn_id.clone(),
        ),
        Err(HumanInputError::AuthorNotAllowed) => ("refused", None, None, None),
        // Delivery errors can follow pane or handback effects; absence of evidence is not refusal.
        Err(_) => ("unknown", None, None, None),
    };
    let response = serialize(result);
    let stored = json!({"status": response.0.as_u16(), "body": response.1.0});
    let settlement = AdmissionSettlement {
        admission,
        kind,
        result: &stored,
        delivery_confirmed: confirmed,
        turn_id: turn_id.as_deref(),
    };
    if matches!(
        settle_admission(pool, input.origin_id, &settlement).await,
        Ok(true)
    ) {
        response
    } else {
        failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "admission_settlement_failed",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obligations_flag_defaults_off_and_only_imessage_uses_it() {
        for value in [None, Some(""), Some("off"), Some("false"), Some("garbage")] {
            assert!(!parse_enabled(value));
        }
        for value in ["1", "true", " on ", "TRUE"] {
            assert!(parse_enabled(Some(value)));
        }
        assert!(enabled_for_source(true, " imessage "));
        assert!(!enabled_for_source(false, "imessage"));
        assert!(!enabled_for_source(true, "discord"));
        assert!(!enabled_for_source(true, "iMessage"));
    }

    #[test]
    fn hash_tracks_delivered_text_and_canonical_identity() {
        let mut input = AdmissionInput {
            origin_id: "Case-Preserved-GUID",
            agent_id: "agent-a",
            channel_id: 7,
            author_id: 9,
            source: " imessage ",
            text: " text \n",
        };
        let hash = request_hash(&input);
        input.source = "imessage";
        input.text = "text";
        assert_eq!(request_hash(&input), hash);
        input.text = "text changed";
        assert_ne!(request_hash(&input), hash);
        input.text = "text";
        input.author_id = 10;
        assert_ne!(request_hash(&input), hash);
        input.author_id = 9;
        input.channel_id = 8;
        assert_ne!(request_hash(&input), hash);
    }
}
