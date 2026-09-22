//! Browser WebSocket admission uses bounded, expiring, one-use tickets.
//! The shared server token is accepted only in HTTP Authorization headers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, StatusCode, header};
use sha2::{Digest, Sha256};

const TICKET_TTL: Duration = Duration::from_secs(15);
const MAX_PENDING_TICKETS: usize = 1024;

struct Ticket {
    origin: String,
    expires: Instant,
}

#[derive(Clone)]
pub(crate) struct DashboardAccess {
    // Same boot snapshot as REST auth. A token change requires the existing
    // server restart, which also invalidates tickets and existing sockets.
    token: Option<Arc<str>>,
    tickets: Arc<Mutex<HashMap<[u8; 32], Ticket>>>,
}

impl DashboardAccess {
    pub(crate) fn new(config: &crate::config::Config) -> Self {
        Self {
            token: config.server.auth_token.as_deref().map(Arc::from),
            tickets: Arc::default(),
        }
    }

    pub(crate) fn issue(&self, headers: &HeaderMap) -> Result<(String, u64), StatusCode> {
        let origin = request_origin(headers)?;
        let now = Instant::now();
        let mut tickets = self
            .tickets
            .lock()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        tickets.retain(|_, ticket| ticket.expires > now);
        if tickets.len() >= MAX_PENDING_TICKETS {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        let value = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        tickets.insert(
            Sha256::digest(value.as_bytes()).into(),
            Ticket {
                origin,
                expires: now + TICKET_TTL,
            },
        );
        Ok((value, TICKET_TTL.as_secs()))
    }

    pub(crate) fn authorize_ws(&self, headers: &HeaderMap, ticket: Option<&str>) -> bool {
        if let Some(value) = ticket {
            let Ok(origin) = request_origin(headers) else {
                return false;
            };
            let Ok(mut tickets) = self.tickets.lock() else {
                return false;
            };
            let key: [u8; 32] = Sha256::digest(value.as_bytes()).into();
            return tickets
                .remove(&key)
                .is_some_and(|entry| entry.origin == origin && entry.expires > Instant::now());
        }
        // Native clients may supply Bearer on their upgrade. A browser without
        // a ticket cannot supply it, and cross-origin browser upgrades are denied.
        if headers.contains_key(header::ORIGIN) && request_origin(headers).is_err() {
            return false;
        }
        ws_token_authorized(
            self.token.as_deref(),
            super::routes::auth::extract_bearer(headers).unwrap_or(""),
        )
    }
}

pub(super) fn ws_token_authorized(expected: Option<&str>, supplied: &str) -> bool {
    match expected {
        Some(expected) if !expected.is_empty() => {
            crate::utils::auth::constant_time_token_eq(expected, supplied)
        }
        _ => true,
    }
}

// Pin tickets to the actual request Host and full Origin (scheme + port too).
// Forwarded headers are not trusted. A TLS proxy must preserve the public Host.
fn request_origin(headers: &HeaderMap) -> Result<String, StatusCode> {
    let invalid = StatusCode::FORBIDDEN;
    let raw = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .ok_or(invalid)?;
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .ok_or(invalid)?;
    let origin = url::Url::parse(raw).map_err(|_| invalid)?;
    if !matches!(origin.scheme(), "http" | "https")
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return Err(invalid);
    }
    let target = url::Url::parse(&format!("{}://{host}", origin.scheme())).map_err(|_| invalid)?;
    if target.origin() != origin.origin()
        || !target.username().is_empty()
        || target.password().is_some()
        || target.path() != "/"
        || target.query().is_some()
        || target.fragment().is_some()
    {
        return Err(invalid);
    }
    Ok(origin.origin().ascii_serialization())
}

#[cfg(test)]
mod tests;
