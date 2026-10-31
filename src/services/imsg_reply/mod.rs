//! iMessage reply contracts layered over durable origin admission.

pub(crate) mod admission;
mod contract;
pub(crate) mod episode;

use std::future::Future;
use std::sync::OnceLock;

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::db::imsg_reply::{bind_episode, bind_queued, hand_back_busy, release_episode};
use crate::services::discord::health::{HumanInputDelivery, HumanInputError};

pub(crate) use admission::{AdmissionInput, AdmissionResponse};

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

fn enabled_from_env() -> bool {
    crate::utils::env_flag("ADK_IMSG_OBLIGATIONS", false)
}

#[cfg(test)]
pub(crate) fn parse_enabled(raw: Option<&str>) -> bool {
    crate::utils::parse_env_flag_for_test("ADK_IMSG_OBLIGATIONS", raw, false)
}

/// Cache the kill switch once so activation requires a process restart.
pub(crate) fn is_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(enabled_from_env)
}

pub(crate) fn enabled_for_source(enabled: bool, source: &str) -> bool {
    enabled && source.trim() == "imessage"
}

/// The iMessage route retains its reply binding and original admission source.
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
    admission::deliver_once(pool, input, "imessage", authorize, deliver, serialize).await
}

#[cfg(test)]
mod tests {
    use super::admission::request_hash;
    use super::*;

    #[test]
    fn obligations_flag_defaults_off_and_only_imessage_uses_it() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _restore = crate::config::TestEnvVarGuard::capture_after_shared_test_env_lock(
            "ADK_IMSG_OBLIGATIONS",
        );
        for (value, expected) in [
            (None, false),
            (Some(""), false),
            (Some("off"), false),
            (Some("garbage"), false),
            (Some("1"), true),
            (Some("TRUE"), true),
            (Some(" on "), true),
            (Some("yes"), true),
        ] {
            match value {
                Some(value) => unsafe { std::env::set_var("ADK_IMSG_OBLIGATIONS", value) },
                None => unsafe { std::env::remove_var("ADK_IMSG_OBLIGATIONS") },
            }
            assert_eq!(enabled_from_env(), expected);
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
            provider: "claude",
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
