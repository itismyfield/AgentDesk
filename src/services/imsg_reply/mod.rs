//! Off-by-default iMessage admission reserves each GUID before delivery can have an effect.

mod contract;
pub(crate) mod episode;

use std::future::Future;
use std::sync::OnceLock;

use axum::{Json, http::StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::db::imsg_reply::{
    AdmissionIdentity, AdmissionSettlement, Obligation, bind_episode, bind_queued, hand_back_busy,
    read_obligation, release_episode, reserve_admission, settle_admission,
};
use crate::services::discord::health::{HumanInputDelivery, HumanInputError};

pub(crate) type AdmissionResponse = (StatusCode, Json<Value>);

/// The delivery error when a GUID could not be bound before an effect; nothing was delivered.
pub(crate) const UNBOUND: &str = "reply_unbound";

/// Marks a started turn's metadata as the admitted GUID's own, so its prompt carries the contract.
const STARTED_MARK: &str = "reply_contract";

/// An admitted iMessage being delivered; each delivery binds the GUID before its own effect.
#[derive(Clone)]
pub(crate) struct ReplyObligation {
    pool: PgPool,
    origin_id: String,
}

/// Only one committed row counts; a storage error reads as unbound.
fn committed(result: Result<bool, sqlx::Error>, step: &str) -> bool {
    result.unwrap_or_else(|error| {
        tracing::warn!(%error, step, "iMessage reply binding not committed");
        false
    })
}

impl ReplyObligation {
    pub(crate) fn new(pool: PgPool, origin_id: &str) -> Self {
        let origin_id = origin_id.to_string();
        Self { pool, origin_id }
    }

    /// The text a busy pane receives: the contract, then the person's text unchanged.
    pub(crate) fn frame(&self, text: &str) -> String {
        contract::prepend(&self.origin_id, text)
    }

    pub(crate) fn contract(&self) -> String {
        contract::render(&self.origin_id)
    }

    pub(crate) fn mark_started(&self, metadata: Option<Value>) -> Option<Value> {
        let mut metadata = metadata.unwrap_or_else(|| json!({}));
        metadata["human_input"][STARTED_MARK] = json!("v1");
        Some(metadata)
    }

    pub(crate) async fn bind_episode(&self, episode: &str, nonce: Option<&str>) -> bool {
        let bound = bind_episode(&self.pool, &self.origin_id, episode, nonce).await;
        committed(bound, "episode")
    }

    pub(crate) async fn release_episode(&self, episode: &str) -> bool {
        let released = release_episode(&self.pool, &self.origin_id, episode).await;
        committed(released, "release")
    }

    pub(crate) async fn bind_queued(&self, queued_message_id: u64) -> bool {
        let queued = queued_message_id.to_string();
        committed(
            bind_queued(&self.pool, &self.origin_id, &queued).await,
            "queue",
        )
    }

    pub(crate) async fn hand_back(
        &self,
        episode: &str,
        nonce: &str,
        queued_message_id: u64,
    ) -> bool {
        let queued = queued_message_id.to_string();
        let handed = hand_back_busy(&self.pool, &self.origin_id, (episode, nonce), &queued);
        committed(handed.await, "handback")
    }
}

fn started_contract(source: Option<&str>, metadata: Option<&Value>) -> Option<String> {
    let human = metadata?.get("human_input")?;
    if source != Some("imessage") || human.get(STARTED_MARK) != Some(&json!("v1")) {
        return None;
    }
    human.get("origin_id")?.as_str().map(contract::render)
}

/// A started turn's reply context, led by the contract when its delivery marked the metadata.
pub(crate) fn with_started_contract(
    reply: Option<String>,
    source: Option<&str>,
    metadata: Option<&Value>,
) -> Option<String> {
    let Some(contract) = started_contract(source, metadata) else {
        return reply;
    };
    Some(reply.map_or_else(
        || contract.clone(),
        |reply| format!("{contract}\n\n{reply}"),
    ))
}

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
    fn s1b_started_contract_leads_only_a_marked_imessage_start() {
        let metadata = json!({"human_input": {"source": "imessage", "origin_id": "p:0/G"}});
        let marked = json!({"human_input": {
            "source": "imessage", "origin_id": "p:0/G", STARTED_MARK: "v1",
        }});
        let retry = Some("retry context".to_string());
        let lead = |source, metadata| with_started_contract(retry.clone(), source, metadata);
        assert_eq!(lead(Some("imessage"), Some(&metadata)), retry);
        assert_eq!(lead(Some("external"), Some(&marked)), retry);
        let contract = contract::render("p:0/G");
        assert_eq!(
            lead(Some("imessage"), Some(&marked)),
            Some(format!("{contract}\n\nretry context"))
        );
        let alone = with_started_contract(None, Some("imessage"), Some(&marked));
        assert_eq!(alone, Some(contract));
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
