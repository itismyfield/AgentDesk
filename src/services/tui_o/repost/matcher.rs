//! Attributing observed messages to one piece: its receipt, then its marker, then an observed
//! nonce, then an exact payload match nobody else may claim. Anything unclear is never absence.

use std::collections::{BTreeMap, BTreeSet};

use super::evidence::{EvidenceScope, MATCHER_VERSION};
use crate::services::tui_o::repost::identity::{marker, payload_sha256};
use crate::services::tui_o::repost::o_piece_delivery::{PieceKey, Receipt, ReceiptMethod};
use crate::services::tui_o::repost::send::RepostIds;

/// A message as Discord returned it. `nonce` is only what came back, never what was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObservedMessage {
    pub(crate) id: u64,
    pub(crate) channel_id: u64,
    pub(crate) author_id: u64,
    pub(crate) content: String,
    /// Embeds of the kind a send attaches, with or without a footer.
    pub(crate) rich_embeds: usize,
    pub(crate) footers: Vec<String>,
    pub(crate) nonce: Option<String>,
}

/// Who else may own a message. `Unknown` is a failed lookup, never an empty index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AttributionSnapshot {
    Known {
        /// Recorded receipts of the channel, by message id.
        receipts: BTreeMap<u64, PieceKey>,
        /// Other unsettled pieces with this piece's payload.
        same_payload: Vec<PieceKey>,
    },
    Unknown(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryKind {
    /// The observed nonce came back while only the original had been sent.
    OriginalRecovered,
    /// A re-post carrying the piece's marker.
    Reposted,
    /// The piece posted, but which of its sends did is not known.
    OriginUnknown,
}

/// A message of this piece and how it was told apart; recording `receipt` is the caller's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValidatedReceipt {
    pub(crate) receipt: Receipt,
    pub(crate) channel_id: u64,
    pub(crate) recovery: RecoveryKind,
    pub(crate) observed_nonce: Option<String>,
    pub(crate) observed_marker: Option<String>,
    pub(crate) content_sha256: String,
    pub(crate) matcher_version: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Attribution {
    /// New messages of this piece. Two or more distinct ids are a duplicate, not a resend reason.
    pub(crate) found: BTreeMap<u64, ValidatedReceipt>,
    /// Messages already recorded as this piece's receipts.
    pub(crate) recorded: BTreeSet<u64>,
    /// Exact matches another piece, or a lookup that failed, could also claim.
    pub(crate) unattributed: BTreeSet<u64>,
    /// The payload under a footer that is not this piece's intact marker.
    pub(crate) damaged: BTreeSet<u64>,
    /// A marker-like footer this build cannot read, such as an older format: never absence.
    pub(crate) unreadable: BTreeSet<u64>,
}

impl Attribution {
    /// Nothing seen could be this piece.
    pub(crate) fn is_clear(&self) -> bool {
        self.found.is_empty()
            && self.recorded.is_empty()
            && self.unattributed.is_empty()
            && self.damaged.is_empty()
            && self.unreadable.is_empty()
    }

    /// Distinct ids known to be this piece's messages.
    pub(crate) fn distinct_ids(&self) -> usize {
        self.found
            .keys()
            .chain(&self.recorded)
            .collect::<BTreeSet<_>>()
            .len()
    }
}

/// The re-post footer ends with a space and the marker, so a longer marker never matches.
fn carries(footer: &str, marker: &str) -> bool {
    footer
        .strip_suffix(marker)
        .is_some_and(|rest| rest.ends_with(' '))
}

/// `Some(true)` when the footer carries a well-formed marker of `channel`, `Some(false)` when it
/// carries something marker-like that is not, `None` when it claims no marker.
fn marker_claim(footer: &str, channel: u64) -> Option<bool> {
    let at = match footer.find(" o:") {
        Some(space) => space + 1,
        None => footer.starts_with("o:").then_some(0)?,
    };
    let rest = footer[at..].strip_prefix(&format!("o:{channel}:"));
    let readable = rest.and_then(|rest| {
        let (fields, index) = rest.rsplit_once('#')?;
        let mut fields = fields.splitn(3, ':');
        let (provider, kind, native) = (fields.next()?, fields.next()?, fields.next()?);
        let digits = !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit());
        let named =
            ["claude", "codex"].contains(&provider) && ["body", "tool_result"].contains(&kind);
        (digits && named && !native.is_empty()).then_some(())
    });
    Some(readable.is_some())
}

/// Adds what `seen` shows about the piece of `scope` to `into`; repeated ids count once.
pub(crate) fn match_observations<'m>(
    scope: &EvidenceScope,
    snapshot: &AttributionSnapshot,
    seen: impl IntoIterator<Item = &'m ObservedMessage>,
    into: &mut Attribution,
) {
    let own = marker(&scope.key);
    let nonce = RepostIds::for_piece(&own).map(|ids| ids.nonce().to_owned());
    let (receipts, rivals) = match snapshot {
        AttributionSnapshot::Known {
            receipts,
            same_payload,
        } => (Some(receipts), same_payload.as_slice()),
        AttributionSnapshot::Unknown(_) => (None, &[][..]),
    };
    let channel = scope.key.unit().channel_id;
    for message in seen {
        if message.channel_id != channel || message.author_id != scope.sender_id {
            continue;
        }
        match receipts.and_then(|receipts| receipts.get(&message.id)) {
            Some(key) if *key == scope.key => {
                into.recorded.insert(message.id);
                continue;
            }
            Some(_) => continue,
            None => {}
        }
        let content_sha256 = payload_sha256(&message.content);
        let exact = content_sha256 == scope.payload_sha256;
        let own_nonce = message.nonce.is_some() && message.nonce == nonce;
        let found = |method, recovery, slot| ValidatedReceipt {
            receipt: Receipt {
                key: scope.key.clone(),
                message_id: message.id,
                author_id: message.author_id,
                slot,
                method,
            },
            channel_id: message.channel_id,
            recovery,
            observed_nonce: message.nonce.clone(),
            observed_marker: (method == ReceiptMethod::Marker).then(|| own.clone()),
            content_sha256: content_sha256.clone(),
            matcher_version: MATCHER_VERSION,
        };
        // Both re-post slots share the marker and nonce, so neither names the slot that posted.
        let attributed = if message.footers.iter().any(|footer| carries(footer, &own)) {
            Some(found(ReceiptMethod::Marker, RecoveryKind::Reposted, None))
        } else if message.rich_embeds > 0 || !message.footers.is_empty() {
            // A re-post embed without this piece's intact marker is never a success, and when the
            // payload or this piece's nonce came with it, it is no absence either.
            let elsewhere = rivals.iter().any(|rival| {
                let theirs = marker(rival);
                message
                    .footers
                    .iter()
                    .any(|footer| carries(footer, &theirs))
            });
            let unreadable = message
                .footers
                .iter()
                .any(|footer| marker_claim(footer, channel) == Some(false));
            if (exact || own_nonce) && !elsewhere {
                into.damaged.insert(message.id);
            } else if unreadable && !elsewhere {
                into.unreadable.insert(message.id);
            }
            None
        } else if message.nonce.is_some() {
            let (recovery, slot) = if scope.additional_sent() {
                (RecoveryKind::OriginUnknown, None)
            } else {
                (RecoveryKind::OriginalRecovered, Some(0))
            };
            // A nonce of no known piece on the payload may be an older format of this one's.
            let rival_nonce = rivals.iter().any(|rival| {
                let theirs = RepostIds::for_piece(&marker(rival));
                theirs.is_some_and(|ids| message.nonce.as_deref() == Some(ids.nonce()))
            });
            if exact && !own_nonce && !rival_nonce {
                into.unattributed.insert(message.id);
            }
            own_nonce.then(|| found(ReceiptMethod::Nonce, recovery, slot))
        } else if exact && receipts.is_some() && rivals.is_empty() {
            Some(found(
                ReceiptMethod::ExactMatch,
                RecoveryKind::OriginUnknown,
                None,
            ))
        } else {
            if exact {
                into.unattributed.insert(message.id);
            }
            None
        };
        if let Some(receipt) = attributed {
            into.found.entry(message.id).or_insert(receipt);
        }
    }
}
