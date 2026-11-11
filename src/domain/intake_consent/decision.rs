use super::{IntentRef, RowProjection};
use serde::{Deserialize, Serialize};

pub(crate) const PREFIX: &str = "herdr-consent-custody:v1:";
pub(crate) const REARM_PREFIX: &str = "herdr-consent-rearm:v1:";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RearmMarker {
    pub row: i64,
    pub payload: String,
    pub owner: Option<String>,
    pub generation: Option<i64>,
    pub count: u8,
    pub rearmed_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Marker {
    pub nonce: String,
    pub consent: String,
    pub row: i64,
    pub payload: String,
    pub claim: Option<String>,
    pub owner: Option<String>,
    pub generation: Option<i64>,
    pub released: bool,
}

pub(crate) fn marker(error: Option<&str>) -> Result<Marker, String> {
    let encoded = error.and_then(|error| error.strip_prefix(PREFIX)).ok_or(
        "outbox has no durable before-effects consent custody marker; preserve its original row",
    )?;
    serde_json::from_str(encoded)
        .map_err(|error| format!("invalid consent custody marker: {error}"))
}

pub(crate) fn permits_custody(
    error: Option<&str>,
    row: &RowProjection<'_>,
) -> Result<bool, String> {
    let Some(error) = error else {
        return Ok(true);
    };
    let Some(encoded) = error.strip_prefix(REARM_PREFIX) else {
        return Ok(false);
    };
    let Ok(marker) = serde_json::from_str::<RearmMarker>(encoded) else {
        return Ok(false);
    };
    Ok(marker.row == row.id
        && marker.payload == row.payload.digest()?
        && marker.owner.as_deref() == row.owner_instance_id
        && marker.generation == row.owner_generation
        && marker.count == 1
        && marker.rearmed_at > 0)
}

pub(crate) enum IntentResolution {
    Returned,
    Adopted,
    Settled,
}

pub(crate) fn is_unresolved(resolution: Option<IntentResolution>) -> bool {
    matches!(
        resolution,
        None | Some(IntentResolution::Returned | IntentResolution::Adopted)
    )
}

/// A checked source binding accompanies custody authority but cannot issue it.
pub(crate) struct HoldInput {
    consent: String,
    row: i64,
    payload: String,
    claim: Option<String>,
    owner: Option<String>,
    generation: Option<i64>,
}

impl HoldInput {
    pub(crate) fn checked(
        row: &RowProjection<'_>,
        intent: &IntentRef,
        consent: String,
    ) -> Result<Self, String> {
        if intent.store != "intake_outbox"
            || intent.id != row.id.to_string()
            || intent.provider != row.provider
            || intent.channel.to_string() != row.channel_id
            || intent.source_message.to_string() != row.user_msg_id
            || intent.fifo != row.attempt_no as u64
            || !row.spawned
            || row.claim_owner.is_none()
        {
            return Err(
                "before-effects consent custody does not identify this exact worker payload".into(),
            );
        }
        Ok(Self {
            consent,
            row: row.id,
            payload: row.payload.digest()?,
            claim: row.claim_owner.map(str::to_owned),
            owner: row.owner_instance_id.map(str::to_owned),
            generation: row.owner_generation,
        })
    }

    pub(crate) fn consent(&self) -> &str {
        &self.consent
    }

    pub(crate) fn matches(&self, row: &RowProjection<'_>) -> Result<bool, String> {
        Ok(row.spawned
            && self.row == row.id
            && self.payload == row.payload.digest()?
            && self.claim.as_deref() == row.claim_owner
            && self.owner.as_deref() == row.owner_instance_id
            && self.generation == row.owner_generation)
    }
}
