use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

pub(crate) const RELAY_REQUEST_ID_HEADER: &str = "x-agentdesk-relay-request-id";
pub(crate) const RELAY_PUBLISHED_AT_HEADER: &str = "x-agentdesk-relay-published-at";
pub(crate) const RELAY_DEADLINE_HEADER: &str = "x-agentdesk-relay-deadline";
pub(crate) const DELIVERY_TTL: Duration = Duration::from_secs(60 * 60);
pub(crate) const LEDGER_RETENTION: Duration = Duration::from_secs(2 * 60 * 60);

#[cfg(test)]
fn behavior_mutation_enabled(name: &str) -> bool {
    std::env::var("AGENTDESK_HOOK_RELAY_TEST_MUTATION").is_ok_and(|value| value == name)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RelayReceiptPin {
    provider: String,
    event: String,
    command_session_id: Option<String>,
    payload_hash: String,
    published_at: Option<String>,
    deadline: Option<String>,
}

impl RelayReceiptPin {
    pub(crate) fn new(
        provider: &str,
        event: &str,
        command_session_id: Option<&str>,
        payload: &Value,
        headers: &HeaderMap,
    ) -> Self {
        let encoded = serde_json::to_vec(payload).unwrap_or_default();
        Self {
            provider: provider.to_string(),
            event: event.to_string(),
            command_session_id: command_session_id.map(str::to_string),
            payload_hash: blake3::hash(&encoded).to_hex().to_string(),
            published_at: header_string(headers, RELAY_PUBLISHED_AT_HEADER),
            deadline: header_string(headers, RELAY_DEADLINE_HEADER),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RelayReceiptTicket {
    request_id: String,
    pin: RelayReceiptPin,
}

#[derive(Clone, Debug)]
pub(crate) struct RelayReceiptResponse {
    pub(crate) status: StatusCode,
    pub(crate) body: Value,
}

#[derive(Clone, Debug)]
pub(crate) enum RelayReceiptBegin {
    Legacy,
    Fresh(RelayReceiptTicket),
    Respond(RelayReceiptResponse),
}

#[derive(Clone, Debug)]
enum RelayReceiptEntry {
    InFlight {
        pin: RelayReceiptPin,
        updated_at: DateTime<Utc>,
    },
    Accepted {
        pin: RelayReceiptPin,
        response: RelayReceiptResponse,
        updated_at: DateTime<Utc>,
    },
    Failed {
        pin: RelayReceiptPin,
        response: RelayReceiptResponse,
        updated_at: DateTime<Utc>,
    },
}

impl RelayReceiptEntry {
    fn pin(&self) -> &RelayReceiptPin {
        match self {
            Self::InFlight { pin, .. } | Self::Accepted { pin, .. } | Self::Failed { pin, .. } => {
                pin
            }
        }
    }

    fn updated_at(&self) -> DateTime<Utc> {
        match self {
            Self::InFlight { updated_at, .. }
            | Self::Accepted { updated_at, .. }
            | Self::Failed { updated_at, .. } => *updated_at,
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct RelayReceiptLedger {
    entries: Arc<Mutex<BTreeMap<String, RelayReceiptEntry>>>,
}

impl RelayReceiptLedger {
    pub(crate) fn begin(
        &self,
        headers: &HeaderMap,
        pin: RelayReceiptPin,
        now: DateTime<Utc>,
    ) -> RelayReceiptBegin {
        let Some(raw_request_id) = header_string(headers, RELAY_REQUEST_ID_HEADER) else {
            if freshness_headers_present(headers) {
                return RelayReceiptBegin::Respond(error_response(
                    StatusCode::BAD_REQUEST,
                    "relay freshness headers require a request id",
                ));
            }
            return RelayReceiptBegin::Legacy;
        };
        let request_id = match uuid::Uuid::parse_str(&raw_request_id) {
            Ok(request_id) => request_id.to_string(),
            Err(_) => {
                return RelayReceiptBegin::Respond(error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid relay request id",
                ));
            }
        };

        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        prune_entries(&mut entries, now);
        #[cfg(test)]
        if behavior_mutation_enabled("receipt-bypass") {
            entries.remove(&request_id);
        }
        if let Some(existing) = entries.get(&request_id) {
            if existing.pin() != &pin {
                return RelayReceiptBegin::Respond(error_response(
                    StatusCode::CONFLICT,
                    "relay request id was reused with different routing inputs or payload",
                ));
            }
            return match existing {
                RelayReceiptEntry::Accepted { response, .. }
                | RelayReceiptEntry::Failed { response, .. } => {
                    RelayReceiptBegin::Respond(response.clone())
                }
                RelayReceiptEntry::InFlight { .. } => RelayReceiptBegin::Respond(error_response(
                    StatusCode::TOO_EARLY,
                    "relay request is already in flight",
                )),
            };
        }

        entries.insert(
            request_id.clone(),
            RelayReceiptEntry::InFlight {
                pin: pin.clone(),
                updated_at: now,
            },
        );
        let ticket = RelayReceiptTicket { request_id, pin };
        match validate_freshness(headers, now) {
            Ok(()) => RelayReceiptBegin::Fresh(ticket),
            Err(response) => {
                entries.insert(
                    ticket.request_id.clone(),
                    RelayReceiptEntry::Failed {
                        pin: ticket.pin.clone(),
                        response: response.clone(),
                        updated_at: now,
                    },
                );
                RelayReceiptBegin::Respond(response)
            }
        }
    }

    pub(crate) fn finish_accepted(
        &self,
        ticket: RelayReceiptTicket,
        status: StatusCode,
        body: Value,
    ) {
        self.finish(ticket, status, body, true);
    }

    pub(crate) fn finish_failed(
        &self,
        ticket: RelayReceiptTicket,
        status: StatusCode,
        body: Value,
    ) {
        self.finish(ticket, status, body, false);
    }

    fn finish(&self, ticket: RelayReceiptTicket, status: StatusCode, body: Value, accepted: bool) {
        let response = RelayReceiptResponse { status, body };
        let entry = if accepted {
            RelayReceiptEntry::Accepted {
                pin: ticket.pin,
                response,
                updated_at: Utc::now(),
            }
        } else {
            RelayReceiptEntry::Failed {
                pin: ticket.pin,
                response,
                updated_at: Utc::now(),
            }
        };
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(ticket.request_id, entry);
    }
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn freshness_headers_present(headers: &HeaderMap) -> bool {
    headers.contains_key(RELAY_PUBLISHED_AT_HEADER) || headers.contains_key(RELAY_DEADLINE_HEADER)
}

fn validate_freshness(headers: &HeaderMap, now: DateTime<Utc>) -> Result<(), RelayReceiptResponse> {
    #[cfg(test)]
    if behavior_mutation_enabled("freshness-removal") {
        return Ok(());
    }
    let published = header_string(headers, RELAY_PUBLISHED_AT_HEADER);
    let deadline = header_string(headers, RELAY_DEADLINE_HEADER);
    let (Some(published), Some(deadline)) = (published, deadline) else {
        if freshness_headers_present(headers) {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "relay published-at and deadline headers must be supplied together",
            ));
        }
        return Ok(());
    };
    let published = DateTime::parse_from_rfc3339(&published)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| error_response(StatusCode::BAD_REQUEST, "invalid relay published-at"))?;
    let deadline = DateTime::parse_from_rfc3339(&deadline)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| error_response(StatusCode::BAD_REQUEST, "invalid relay deadline"))?;
    let max_ttl = chrono::Duration::from_std(DELIVERY_TTL).expect("delivery TTL fits chrono");
    if deadline < published || deadline - published > max_ttl {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "relay deadline exceeds the delivery TTL contract",
        ));
    }
    if deadline <= now {
        return Err(error_response(
            StatusCode::GONE,
            "relay delivery deadline expired",
        ));
    }
    Ok(())
}

fn prune_entries(entries: &mut BTreeMap<String, RelayReceiptEntry>, now: DateTime<Utc>) {
    let retention = chrono::Duration::from_std(LEDGER_RETENTION).expect("retention fits chrono");
    entries.retain(|_, entry| now - entry.updated_at() <= retention);
}

fn error_response(status: StatusCode, error: &str) -> RelayReceiptResponse {
    RelayReceiptResponse {
        status,
        body: json!({"ok": false, "error": error}),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn ledger_retention_covers_every_deliverable_request() {
        assert!(LEDGER_RETENTION >= DELIVERY_TTL);
    }

    #[test]
    fn request_id_pin_mismatch_is_conflict_and_accepted_body_is_fixed() {
        let ledger = RelayReceiptLedger::default();
        let request_id = uuid::Uuid::new_v4().to_string();
        let mut headers = HeaderMap::new();
        headers.insert(
            RELAY_REQUEST_ID_HEADER,
            HeaderValue::from_str(&request_id).unwrap(),
        );
        let first_pin = RelayReceiptPin::new(
            "Claude",
            "Stop",
            Some("session"),
            &json!({"ordinal":1}),
            &headers,
        );
        let ticket = match ledger.begin(&headers, first_pin.clone(), Utc::now()) {
            RelayReceiptBegin::Fresh(ticket) => ticket,
            other => panic!("expected fresh receipt, got {other:?}"),
        };
        let accepted = json!({"ok":true,"fixed":"body"});
        ledger.finish_accepted(ticket, StatusCode::ACCEPTED, accepted.clone());
        match ledger.begin(&headers, first_pin, Utc::now()) {
            RelayReceiptBegin::Respond(response) => {
                assert_eq!(response.status, StatusCode::ACCEPTED);
                assert_eq!(response.body, accepted);
            }
            other => panic!("expected cached receipt, got {other:?}"),
        }

        let mismatched = RelayReceiptPin::new(
            "Claude",
            "Stop",
            Some("session"),
            &json!({"ordinal":2}),
            &headers,
        );
        match ledger.begin(&headers, mismatched, Utc::now()) {
            RelayReceiptBegin::Respond(response) => {
                assert_eq!(response.status, StatusCode::CONFLICT);
            }
            other => panic!("expected pin conflict, got {other:?}"),
        }
    }
}
