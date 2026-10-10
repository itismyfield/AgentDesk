//! Admitting pieces into the shared re-post budget: an uncertain original sent while re-post was on,
//! or an operator-approved rejected piece. Anything older or unproven is reported once, never sent.

use std::collections::BTreeSet;
use std::io;

use sqlx::PgPool;

use super::config::RepostSwitch;
use super::identity::{IDENTITY_VERSION, PieceKey, SPLIT_VERSION, payload_sha256, piece_of};
use super::o_piece_attempts::AttemptResult;
use super::o_piece_delivery::{
    self, AdmitOutcome, AdmittedBy, Failure, LedgerError, NewDelivery, Receipt, ReceiptMethod,
    ReceiptOutcome,
};
use super::provenance::{ProvenanceEntry, ProvenanceLog};
use crate::services::tui_o::shadow::UnitKey;
use crate::services::tui_o::store::ledger::{PieceOutcome, PieceRecord};

/// An original whose result went uncertain while re-post was on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OriginalAttempt {
    pub(crate) serial: u64,
    pub(crate) key: PieceKey,
    pub(crate) payload: String,
    pub(crate) anchor: u64,
}

/// Why an uncertain piece is kept and reported instead of re-posted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BacklogReason {
    BeforeActivation,
    /// No durable intent shows it was sent while re-post was on.
    NoIntent,
    /// The intent names another piece or payload.
    IntentMismatch,
    /// A kind or payload no re-post covers.
    NotRepostable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Candidate {
    Eligible(OriginalAttempt),
    Backlog(BacklogReason),
    /// Posted, rejected, or already in admission: nothing to admit here.
    Settled,
}

/// Classifies the latest attempt of a piece. Rejected is never admitted on its own; only an
/// operator's approval re-opens it.
pub(crate) fn classify(
    serial: u64,
    record: &PieceRecord,
    provenance: &super::provenance::Provenance,
) -> Candidate {
    match record.outcome {
        None
        | Some(PieceOutcome::NotFound)
        | Some(PieceOutcome::Ambiguous(_))
        | Some(PieceOutcome::Unresolved(_)) => {}
        Some(PieceOutcome::Posted(_)) | Some(PieceOutcome::Rejected(_)) => {
            return Candidate::Settled;
        }
    }
    if provenance.started(serial) {
        return Candidate::Settled;
    }
    let Some(key) = piece_of(record) else {
        return Candidate::Backlog(BacklogReason::NotRepostable);
    };
    let activated = provenance.activation();
    if activated.is_none_or(|activation| serial < activation.frontier) {
        return Candidate::Backlog(BacklogReason::BeforeActivation);
    }
    let Some(intent) = provenance.intent(serial) else {
        return Candidate::Backlog(BacklogReason::NoIntent);
    };
    if intent.key.as_ref() != Some(&key) || intent.payload_sha256 != payload_sha256(&record.payload)
    {
        return Candidate::Backlog(BacklogReason::IntentMismatch);
    }
    Candidate::Eligible(OriginalAttempt {
        serial,
        key,
        payload: record.payload.clone(),
        anchor: record.anchor_id,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BacklogEntry {
    pub(crate) serial: u64,
    pub(crate) unit_key: UnitKey,
    pub(crate) piece_index: u32,
    pub(crate) reason: BacklogReason,
}

/// Not-yet-reported uncertain pieces that are never re-posted, marked reported before they are
/// returned so a repeated scan reports nothing twice. `latest` holds each piece's latest attempt.
pub(crate) fn report_backlog<'r>(
    log: &mut ProvenanceLog,
    latest: impl IntoIterator<Item = (u64, &'r PieceRecord)>,
) -> io::Result<Vec<BacklogEntry>> {
    let state = log.state();
    let Some(activation) = state.activation() else {
        return Ok(Vec::new());
    };
    let entries: Vec<_> = latest
        .into_iter()
        .filter(|(serial, _)| !state.reported(*serial))
        .filter_map(|(serial, record)| match classify(serial, record, state) {
            Candidate::Backlog(reason) => Some(BacklogEntry {
                serial,
                unit_key: record.unit_key.clone(),
                piece_index: record.piece_index,
                reason,
            }),
            Candidate::Eligible(_) | Candidate::Settled => None,
        })
        .collect();
    if !entries.is_empty() {
        log.append(ProvenanceEntry::BacklogReported {
            generation: activation.generation,
            serials: entries.iter().map(|entry| entry.serial).collect(),
        })?;
    }
    Ok(entries)
}

#[derive(Debug)]
pub(crate) enum AdoptError {
    /// The sidecar did not take the entry; nothing was admitted and nothing is re-sent.
    Sidecar(io::Error),
    /// PostgreSQL did not answer; the admission stays pending and is retried, never replaced by a
    /// local budget.
    Ledger(LedgerError),
    /// A pending serial whose ledger record is gone.
    Missing(u64),
}

impl From<io::Error> for AdoptError {
    fn from(error: io::Error) -> Self {
        Self::Sidecar(error)
    }
}

impl From<LedgerError> for AdoptError {
    fn from(error: LedgerError) -> Self {
        Self::Ledger(error)
    }
}

/// How many counted sends an operator-approved piece made before its approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PriorBudget {
    /// Counting the rejected original.
    Known { counted_posts: u32 },
    /// Unprovable: admitted with no further send allowed.
    Unknown,
}

/// The latest rejected attempt an operator approved re-sending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ApprovedRejected {
    pub(crate) rejected_serial: u64,
    pub(crate) key: PieceKey,
    pub(crate) payload: String,
    pub(crate) anchor: u64,
}

/// Admission keys of a channel, recovered before a holder posts any original.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AdmittedKeys {
    Known(BTreeSet<PieceKey>),
    /// PostgreSQL did not answer; this is not "none admitted".
    Unreadable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OriginalGate {
    Send,
    /// Admitted under any earlier serial or holder; its original is never sent again.
    Admitted,
    /// Admission is unknown; hold the original.
    Unknown,
}

impl AdmittedKeys {
    pub(crate) fn original(&self, unit_key: &UnitKey, piece_index: u32) -> OriginalGate {
        let Some(key) = PieceKey::new(unit_key.clone(), piece_index) else {
            return OriginalGate::Send;
        };
        match self {
            Self::Known(keys) if keys.contains(&key) => OriginalGate::Admitted,
            Self::Known(_) => OriginalGate::Send,
            Self::Unreadable(_) => OriginalGate::Unknown,
        }
    }

    pub(crate) fn insert(&mut self, key: PieceKey) {
        if let Self::Known(keys) = self {
            keys.insert(key);
        }
    }
}

/// Durable `Posted` entries of admitted pieces as `(key, message)`, to record as receipts before
/// any further slot is considered; Discord changing the content does not matter.
pub(crate) fn posted_receipts<'r>(
    records: impl IntoIterator<Item = &'r PieceRecord>,
    admitted: &BTreeSet<PieceKey>,
) -> Vec<(PieceKey, u64)> {
    let posted = records
        .into_iter()
        .filter_map(|record| match record.outcome {
            Some(PieceOutcome::Posted(message)) => Some((piece_of(record)?, message)),
            _ => None,
        });
    posted.filter(|(key, _)| admitted.contains(key)).collect()
}

/// Admission against PostgreSQL. Only [`Admitter::when_on`] builds one, so with the switch off no
/// admission step reaches PostgreSQL or the sidecar.
pub(crate) struct Admitter<'a> {
    pool: &'a PgPool,
    node: &'a str,
    sender_id: u64,
}

impl<'a> Admitter<'a> {
    pub(crate) fn when_on(
        switch: &mut RepostSwitch,
        pool: &'a PgPool,
        node: &'a str,
        sender_id: u64,
    ) -> Option<Self> {
        switch.when_enabled(|_| Self {
            pool,
            node,
            sender_id,
        })
    }

    fn delivery(&self, key: &PieceKey, payload: &str, anchor: u64, serial: u64) -> NewDelivery {
        NewDelivery {
            key: key.clone(),
            payload: payload.to_owned(),
            payload_sha256: payload_sha256(payload),
            identity_version: IDENTITY_VERSION,
            split_version: SPLIT_VERSION,
            original_anchor: anchor,
            sender_id: self.sender_id,
            admitted_by: AdmittedBy::Uncertain,
            origin_serial: serial,
            origin_node: self.node.to_owned(),
            original: AttemptResult::Uncertain,
            prior_retries: 0,
            failure: None,
        }
    }

    /// Admits an uncertain on original with slot 0 spent: sidecar pending, PostgreSQL, admitted. A
    /// failure leaves it pending for [`Self::resume_pending`]; the original is never sent again.
    pub(crate) async fn adopt_uncertain(
        &self,
        log: &mut ProvenanceLog,
        original: &OriginalAttempt,
    ) -> Result<AdmitOutcome, AdoptError> {
        let serial = original.serial;
        if !log.state().started(serial) {
            log.append(ProvenanceEntry::AdmissionPending { serial })?;
        }
        let new = self.delivery(&original.key, &original.payload, original.anchor, serial);
        let outcome = o_piece_delivery::admit(self.pool, &new).await?;
        log.append(ProvenanceEntry::Admitted { serial })?;
        Ok(outcome)
    }

    /// Finishes admissions a crash or an outage left pending; `record` finds a serial's ledger
    /// record. Each serial's result is returned.
    pub(crate) async fn resume_pending<'r>(
        &self,
        log: &mut ProvenanceLog,
        record: impl Fn(u64) -> Option<&'r PieceRecord>,
    ) -> Vec<(u64, Result<AdmitOutcome, AdoptError>)> {
        let pending: Vec<u64> = log.state().pending().collect();
        let mut results = Vec::with_capacity(pending.len());
        for serial in pending {
            let original = record(serial).and_then(|record| {
                Some(OriginalAttempt {
                    serial,
                    key: piece_of(record)?,
                    payload: record.payload.clone(),
                    anchor: record.anchor_id,
                })
            });
            let result = match original {
                Some(original) => self.adopt_uncertain(log, &original).await,
                None => Err(AdoptError::Missing(serial)),
            };
            results.push((serial, result));
        }
        results
    }

    /// Admits an operator-approved rejected piece with its earlier counted sends already spent, so
    /// its retries and any automatic re-post share the one budget. An admitted piece keeps its own.
    pub(crate) async fn adopt_operator(
        &self,
        approved: &ApprovedRejected,
        prior: PriorBudget,
    ) -> Result<AdmitOutcome, LedgerError> {
        let mut new = self.delivery(
            &approved.key,
            &approved.payload,
            approved.anchor,
            approved.rejected_serial,
        );
        new.admitted_by = AdmittedBy::Operator;
        new.original = AttemptResult::Rejected;
        match prior {
            PriorBudget::Known { counted_posts } => {
                new.prior_retries = counted_posts.saturating_sub(1);
            }
            PriorBudget::Unknown => new.failure = Some(Failure::CapUnknown),
        }
        o_piece_delivery::admit(self.pool, &new).await
    }

    pub(crate) async fn admitted_keys(&self, channel_id: u64) -> AdmittedKeys {
        match o_piece_delivery::admitted_keys(self.pool, channel_id).await {
            Ok(keys) => AdmittedKeys::Known(keys.into_iter().collect()),
            Err(error) => AdmittedKeys::Unreadable(error.to_string()),
        }
    }

    /// Records a durable local `Posted` of an admitted piece as its receipt.
    pub(crate) async fn promote_posted(
        &self,
        key: &PieceKey,
        message_id: u64,
    ) -> Result<ReceiptOutcome, LedgerError> {
        let receipt = Receipt {
            key: key.clone(),
            message_id,
            author_id: self.sender_id,
            slot: None,
            method: ReceiptMethod::LocalPosted,
        };
        o_piece_delivery::record_receipt(self.pool, &receipt).await
    }
}
