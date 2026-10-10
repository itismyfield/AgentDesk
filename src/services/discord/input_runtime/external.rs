//! External human input on the ledger: an origin-keyed Source and its receipt under one lease.
//! Dormant: no production path builds or submits one yet.

use std::io;

use serde::Deserialize;
use serde_json::{Value, json};

use super::receipt::{self, Deferred, DurableReceipt, Receipt};
use super::source::Source;
use crate::services::tui_input::input_key::external_key_v1;
use crate::services::tui_input::ledger::LedgerLease;
use crate::services::tui_input::rows::RowState;
use crate::services::tui_input::rows::receipt_identity::{ReceiptIdentity, Responsibility};

const ORIGIN_VERSION: u64 = 1;
const DEFAULT_SOURCE: &str = "external";
const SOURCE_MAX: usize = 64;
const ORIGIN_MAX: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExternalReceipt {
    /// Durable, appended now or found for the same origin; `state` is read under the same lease.
    Received {
        receipt: DurableReceipt,
        duplicate: bool,
        state: RowState,
    },
    /// The key already holds another origin; a collision is never resolved by probing.
    Collision,
    /// The same origin was received for another author or channel.
    AuthorMismatch,
    Deferred(Deferred),
}

#[derive(Deserialize, PartialEq)]
struct Origin {
    version: u64,
    source: String,
    origin_id: String,
}

// A missing, unreadable or other-version origin is unknown, never a match or a mismatch.
fn origin(input: &Value) -> Option<Origin> {
    let origin: Origin = serde_json::from_value(input.get("http_origin")?.clone()).ok()?;
    (origin.version == ORIGIN_VERSION).then_some(origin)
}

/// One external input keyed by its trimmed origin; the caller has already authorized the author.
pub(crate) fn source(
    channel: u64,
    author: u64,
    text: &str,
    source_ns: Option<&str>,
    origin_id: &str,
) -> io::Result<Source> {
    let source_ns = (source_ns.map(str::trim))
        .filter(|ns| !ns.is_empty())
        .unwrap_or(DEFAULT_SOURCE);
    let origin_id = origin_id.trim();
    if source_ns.len() > SOURCE_MAX || origin_id.is_empty() || origin_id.len() > ORIGIN_MAX {
        let reason = "external source or origin out of bounds";
        return Err(io::Error::new(io::ErrorKind::InvalidInput, reason));
    }
    let key = external_key_v1(channel, source_ns, origin_id);
    let identity = ReceiptIdentity::new(key, vec![key], author, channel, channel)?;
    let input = json!({
        "text": text,
        "message_id": key,
        "source_message_ids": [key],
        "author_id": author,
        "channel_id": channel,
        "http_origin": {"version": ORIGIN_VERSION, "source": source_ns, "origin_id": origin_id},
    });
    Source::new(key, identity, input, Vec::new())
}

/// The key's own row answers by origin before the generic receipt lookup; only an absent key may
/// append, and only while `admit_new` holds.
pub(crate) fn submit(lease: &mut LedgerLease, source: Source, admit_new: bool) -> ExternalReceipt {
    let key = source.key();
    let Ok(rows) = lease.get().and_then(|ledger| ledger.rows()) else {
        lease.needs_reopen = true;
        return ExternalReceipt::Deferred(Deferred::Persistence);
    };
    let Some(incoming) = origin(source.input()) else {
        return ExternalReceipt::Deferred(Deferred::Unknown);
    };
    if let Some(row) = rows.row(key) {
        match origin(&row.input) {
            None => return ExternalReceipt::Deferred(Deferred::Unknown),
            Some(saved) if saved != incoming => return ExternalReceipt::Collision,
            Some(_) => {}
        }
    }
    match rows.responsibility(source.identity()) {
        Responsibility::Known {
            key: known,
            received_seq,
        } if known == key => match (rows.receipt_identity(key), rows.row(key)) {
            (Some(identity), Some(row)) => ExternalReceipt::Received {
                receipt: DurableReceipt {
                    key,
                    received_seq,
                    identity,
                },
                duplicate: true,
                state: row.state,
            },
            _ => ExternalReceipt::Deferred(Deferred::Unknown),
        },
        Responsibility::Conflict if rows.row(key).is_some() => ExternalReceipt::AuthorMismatch,
        Responsibility::Absent if admit_new => match receipt::commit(lease, source, true) {
            Receipt::Accepted(receipt) => ExternalReceipt::Received {
                receipt,
                duplicate: false,
                state: RowState::Received,
            },
            Receipt::Deferred(deferred) => ExternalReceipt::Deferred(deferred),
            _ => ExternalReceipt::Deferred(Deferred::Unknown),
        },
        Responsibility::Absent => ExternalReceipt::Deferred(Deferred::Closed),
        // An external row handed to Legacy has no Legacy form to answer for it.
        Responsibility::Legacy { .. } | Responsibility::Conflict => {
            ExternalReceipt::Deferred(Deferred::Conflict)
        }
        Responsibility::Known { .. } | Responsibility::Unknown => {
            ExternalReceipt::Deferred(Deferred::Unknown)
        }
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
#[path = "external_tests.rs"]
mod tests;
