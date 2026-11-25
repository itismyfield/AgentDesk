//! Decision requests: what an agent asks its operator to choose. An ask is stored and posted with
//! disabled buttons to its category's Decision forum, whose threads stay out of agent intake.

pub(crate) mod card;
pub(crate) mod ingress;
pub(crate) mod publication;
pub(crate) mod requester;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::decisions::requests::{self, NewRequest, OptionRow, Stored};

#[derive(Debug, Deserialize)]
pub(crate) struct Target {
    pub channel_id: String,
}

#[derive(Debug, Deserialize)]
struct AskOption {
    label: String,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    detail: String,
    #[serde(default)]
    recommendation_percent: Option<i16>,
}

/// The body of an ask. It cannot name a category, a forum, an operator or a result.
#[derive(Debug, Deserialize)]
pub(crate) struct Ask {
    pub agent_id: String,
    pub target: Target,
    #[serde(default)]
    pub provider_hint: Option<String>,
    #[serde(rename = "ref")]
    request_ref: String,
    #[serde(default)]
    idempotency_key: Option<String>,
    title: String,
    #[serde(default)]
    context: String,
    options: Vec<AskOption>,
}

const MAX_OPTIONS: usize = 5;

/// Whether a label or key reads as a bare option number, which a reply reserves for the ordinal.
fn reads_as_ordinal(name: &str) -> bool {
    let digits = name.strip_suffix('번').unwrap_or(name);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

impl Ask {
    /// The options as they are stored, or the first rule the ask breaks.
    fn stored_options(&self) -> Result<Vec<OptionRow>, &'static str> {
        if self.request_ref.trim().is_empty() || self.title.trim().is_empty() {
            return Err("ref_and_title_required");
        }
        if !(2..=MAX_OPTIONS).contains(&self.options.len()) {
            return Err("options_must_be_two_to_five");
        }
        let mut names: Vec<&str> = Vec::new();
        let mut options = Vec::new();
        for option in &self.options {
            let key = option.key.as_deref().map(str::trim);
            for name in std::iter::once(option.label.trim()).chain(key) {
                if name.is_empty() || reads_as_ordinal(name) || names.contains(&name) {
                    return Err("option_names_must_be_distinct_words");
                }
                names.push(name);
            }
            options.push(OptionRow {
                label: option.label.trim().to_string(),
                key: key.map(str::to_string),
                detail: option.detail.clone(),
                recommendation_percent: option.recommendation_percent,
            });
        }
        // The helper gives every option a percent or none; the server checks range and sum only.
        let percents: Vec<i16> = (options.iter())
            .filter_map(|option| option.recommendation_percent)
            .collect();
        let whole = percents.len() == options.len()
            && percents.iter().all(|percent| (0..=100).contains(percent))
            && percents.iter().sum::<i16>() == 100;
        if !percents.is_empty() && !whole {
            return Err("recommendation_must_cover_every_option_and_sum_to_100");
        }
        Ok(options)
    }
}

#[derive(Debug)]
pub(crate) enum AskError {
    Invalid(&'static str),
    /// The idempotency key is held by another payload or another capability.
    Conflict,
    Storage(sqlx::Error),
}

/// Stores an ask for the target the caller resolved, under the hash of its capability. The same
/// key, payload and capability answers the request already stored: `(id, created)`.
pub(crate) async fn store(
    pool: &PgPool,
    ask: &Ask,
    (logical_channel_id, target_channel_id): (u64, u64),
    capability_sha256: &str,
) -> Result<(Uuid, bool), AskError> {
    let options = ask.stored_options().map_err(AskError::Invalid)?;
    let (request_ref, title) = (ask.request_ref.trim(), ask.title.trim());
    let key = (ask.idempotency_key.as_deref().map(str::trim))
        .filter(|key| !key.is_empty())
        .unwrap_or(request_ref);
    let payload = serde_json::json!([
        ask.agent_id,
        target_channel_id,
        request_ref,
        title,
        ask.context,
        options
    ]);
    let payload_sha256 = hex::encode(Sha256::digest(payload.to_string().as_bytes()));
    let new = NewRequest {
        agent_id: &ask.agent_id,
        logical_channel_id,
        target_channel_id,
        request_ref,
        idempotency_key: key,
        payload_sha256: &payload_sha256,
        capability_sha256,
        title,
        context: &ask.context,
        options: &options,
    };
    match requests::create_once(pool, &new).await {
        Ok(Stored::Created(id)) => Ok((id, true)),
        Ok(Stored::Same(id)) => Ok((id, false)),
        Ok(Stored::Conflict) => Err(AskError::Conflict),
        Err(error) => Err(AskError::Storage(error)),
    }
}

#[cfg(test)]
#[path = "ask_tests.rs"]
mod tests;
