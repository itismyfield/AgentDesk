//! The re-post turn. Off, nothing past the switch runs: no PostgreSQL, probe or sidecar step.
//! On, a tick finds admitted pieces and advances their probes, and a turn grants the next slot
//! only on fresh absence evidence or a current approval.

use std::collections::BTreeMap;
use std::time::Duration;

use sqlx::Row;
use sqlx::postgres::PgPool;
use tokio::time::Instant;

use super::admission::{Admitter, ApprovedRejected, OriginalGate, PriorBudget};
use super::config::RepostSwitch;
use super::dispatch::{Admission, DispatchPermit, GuardReason, RepostWriter};
use super::io::probe::evidence::{EvidenceScope, NotFoundEvidence, RunScope};
use super::io::probe::matcher::AttributionSnapshot;
use super::io::probe::{ProbeRead, ProbeSession, Progress};
use super::o_piece_attempts::{self, GrantOutcome, GrantRequest, Intent};
use super::o_piece_delivery::{
    self, AdmitOutcome, Failure, LedgerError, PieceKey, ReceiptOutcome, kind_name, provider_name,
};
use crate::services::tui_o::ownership::GatewayOwnership;

/// How long a granted slot stays open before another holder may settle it as abandoned.
pub(crate) const GRANT_TTL: Duration = Duration::from_secs(180);

/// The O side's proof that an approval is current: the key's latest disposition is this
/// rejection's Authorized, the id matches and is unconsumed, and key and payload are the same.
pub(crate) trait ApprovalCheck {
    fn current(&self, approval_id: &str, approved: &ApprovedRejected) -> Result<(), String>;
}

pub(crate) enum DispatchIntent<'a> {
    Auto(Box<NotFoundEvidence>),
    OperatorResume {
        approval_id: String,
        approved: ApprovedRejected,
        prior: PriorBudget,
        check: &'a (dyn ApprovalCheck + Sync),
    },
}

/// What the O consumer does with an admission; each variant has exactly one step.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TurnStep {
    Dispatch,
    Legacy,
    Wait,
    Done,
    Stopped,
}

pub(crate) fn turn_step<L>(admission: &Admission<L>) -> TurnStep {
    match admission {
        Admission::Granted { .. } => TurnStep::Dispatch,
        Admission::LegacyOff => TurnStep::Legacy,
        Admission::WaitForOpen | Admission::ObserveOnly => TurnStep::Wait,
        Admission::AlreadyResolved | Admission::CapReached | Admission::CapUnknown => {
            TurnStep::Done
        }
        Admission::Parked => TurnStep::Stopped,
        Admission::GuardRejected(GuardReason::LeaseBusy | GuardReason::NoGateway) => TurnStep::Wait,
        Admission::GuardRejected(GuardReason::Writer(_) | GuardReason::Approval(_)) => {
            TurnStep::Stopped
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct TickReport {
    /// Fresh absence evidence, one per piece, for a turn to judge.
    pub(crate) ready: Vec<NotFoundEvidence>,
    /// What recording each probed success returned, by message id.
    pub(crate) recorded: Vec<(u64, ReceiptOutcome)>,
    /// Pieces left as they were, with why.
    pub(crate) waiting: Vec<(PieceKey, String)>,
}

#[derive(Debug)]
pub(crate) enum Tick {
    Off,
    Ran(TickReport),
}

pub(crate) struct Runner {
    switch: RepostSwitch,
    pool: PgPool,
    sender_id: u64,
    run: RunScope,
    probes: BTreeMap<PieceKey, (EvidenceScope, ProbeSession)>,
}

impl Runner {
    pub(crate) fn new(switch: RepostSwitch, pool: PgPool, sender_id: u64, run: RunScope) -> Self {
        Self {
            switch,
            pool,
            sender_id,
            run,
            probes: BTreeMap::new(),
        }
    }

    /// The channel's recorded receipts by message id; `None` when any cannot be read.
    async fn receipts(&self, channel: u64, keys: &[PieceKey]) -> Option<BTreeMap<u64, PieceKey>> {
        let sql = "SELECT message_id, provider, native_key, kind, piece_index \
                   FROM o_piece_receipts WHERE channel_id = $1";
        let rows = sqlx::query(sql)
            .bind(channel.to_string())
            .fetch_all(&self.pool)
            .await
            .ok()?;
        let mut receipts = BTreeMap::new();
        for row in rows {
            let id = u64::try_from(row.try_get::<i64, _>("message_id").ok()?).ok()?;
            let (provider, kind): (String, String) =
                (row.try_get("provider").ok()?, row.try_get("kind").ok()?);
            let (native, index): (String, i64) = (
                row.try_get("native_key").ok()?,
                row.try_get("piece_index").ok()?,
            );
            let owner = keys.iter().find(|key| {
                let unit = key.unit();
                provider_name(unit.provider) == provider
                    && kind_name(unit.kind) == kind
                    && unit.native_key == native
                    && i64::from(key.piece_index()) == index
            })?;
            receipts.insert(id, owner.clone());
        }
        Some(receipts)
    }

    /// One pass over the channel's admitted pieces: expired slots settled, probes advanced, found
    /// messages recorded as receipts. Off returns before PostgreSQL.
    pub(crate) async fn tick(
        &mut self,
        channel: u64,
        reader: &impl ProbeRead,
        proof_ids: &[u64],
        settled_at: Instant,
    ) -> Result<Tick, LedgerError> {
        if self.switch.when_enabled(|_| ()).is_none() {
            return Ok(Tick::Off);
        }
        let mut report = TickReport::default();
        let keys = o_piece_delivery::admitted_keys(&self.pool, channel).await?;
        let mut rows = Vec::new();
        for key in &keys {
            o_piece_attempts::settle_expired(&self.pool, key).await?;
            if let Some(row) = o_piece_delivery::load(&self.pool, key).await? {
                rows.push((row, o_piece_attempts::attempts(&self.pool, key).await?));
            }
        }
        let receipts = self.receipts(channel, &keys).await;
        let resolved = |key: &PieceKey| {
            receipts
                .as_ref()
                .is_some_and(|receipts| receipts.values().any(|owner| owner == key))
        };
        for (row, attempts) in &rows {
            let key = &row.key;
            if resolved(key) {
                self.probes.remove(key);
                continue;
            }
            let scope = match EvidenceScope::of(row, attempts, &self.run) {
                Ok(scope) => scope,
                Err(slot) => {
                    self.probes.remove(key);
                    report
                        .waiting
                        .push((key.clone(), format!("slot {slot} is open")));
                    continue;
                }
            };
            let snapshot = match &receipts {
                Some(receipts) => AttributionSnapshot::Known {
                    receipts: receipts.clone(),
                    same_payload: rows
                        .iter()
                        .filter(|(other, _)| other.key != *key && !resolved(&other.key))
                        .filter(|(other, _)| other.payload_sha256 == row.payload_sha256)
                        .map(|(other, _)| other.key.clone())
                        .collect(),
                },
                None => AttributionSnapshot::Unknown("receipts unreadable".into()),
            };
            let entry = self.probes.entry(key.clone()).or_insert_with(|| {
                let session = ProbeSession::new(scope.clone(), proof_ids.to_vec(), settled_at);
                (scope.clone(), session)
            });
            // A changed row, slot set or run starts over: older passes describe something else.
            if entry.0 != scope {
                let session = ProbeSession::new(scope.clone(), proof_ids.to_vec(), settled_at);
                *entry = (scope, session);
            }
            let progress = entry.1.advance(reader, &self.run, &snapshot).await;
            for found in entry.1.attribution().found.values() {
                let outcome = o_piece_delivery::record_receipt(&self.pool, &found.receipt).await?;
                report.recorded.push((found.receipt.message_id, outcome));
            }
            match progress {
                Progress::Absent(evidence) => report.ready.push(*evidence),
                other => report.waiting.push((key.clone(), format!("{other:?}"))),
            }
        }
        Ok(Tick::Ran(report))
    }

    /// The pre-grant checks in order, then the grant. Only a committed grant becomes a permit;
    /// every other answer sends nothing, and a refusal after the grant still keeps the slot.
    pub(crate) async fn try_next_dispatch<W: RepostWriter>(
        &mut self,
        writer: &mut W,
        membership: OriginalGate,
        intent: DispatchIntent<'_>,
    ) -> Result<Admission<W::Lease>, LedgerError> {
        let asked = Instant::now();
        if self.switch.when_enabled(|_| ()).is_none() {
            // Off: the recovered admission index decides; an unknown one holds the piece.
            return Ok(match membership {
                OriginalGate::Admitted => Admission::Parked,
                OriginalGate::Send => Admission::LegacyOff,
                OriginalGate::Unknown => Admission::ObserveOnly,
            });
        }
        if !writer.settle_open().await {
            return Ok(Admission::WaitForOpen);
        }
        if let Err(reason) = writer.guard() {
            return Ok(Admission::GuardRejected(GuardReason::Writer(reason)));
        }
        let Some(lease) = writer.try_lease() else {
            return Ok(Admission::GuardRejected(GuardReason::LeaseBusy));
        };
        if !matches!(writer.gate().current(), GatewayOwnership::Owned { .. }) {
            return Ok(Admission::GuardRejected(GuardReason::NoGateway));
        }
        let (row, expected_revision, intent) = match intent {
            DispatchIntent::Auto(evidence) => {
                let key = &evidence.scope().key;
                let Some(row) = o_piece_delivery::load(&self.pool, key).await? else {
                    // A row gone since the evidence proves nothing; never the legacy path.
                    return Ok(Admission::ObserveOnly);
                };
                let attempts = o_piece_attempts::attempts(&self.pool, key).await?;
                // The evidence keeps its own revision; the grant's CAS refuses any later row.
                if !evidence.validate_current(&row, &attempts, &self.run) {
                    return Ok(Admission::ObserveOnly);
                }
                (row, evidence.scope().row_revision, Intent::AutoReconfirm)
            }
            DispatchIntent::OperatorResume {
                approval_id,
                approved,
                prior,
                check,
            } => {
                if let Err(reason) = check.current(&approval_id, &approved) {
                    return Ok(Admission::GuardRejected(GuardReason::Approval(reason)));
                }
                let (pool, holder) = (&self.pool, self.run.holder.as_str());
                let Some(admitter) =
                    Admitter::when_on(&mut self.switch, pool, holder, self.sender_id)
                else {
                    return Ok(Admission::Parked);
                };
                let row = match admitter.adopt_operator(&approved, prior).await? {
                    AdmitOutcome::Admitted(row) | AdmitOutcome::Existing(row) => row,
                    AdmitOutcome::IdentityConflict(_) => {
                        let reason = "another payload holds the key".to_owned();
                        return Ok(Admission::GuardRejected(GuardReason::Approval(reason)));
                    }
                };
                let revision = row.revision;
                (row, revision, Intent::OperatorResume { approval_id })
            }
        };
        let request = GrantRequest {
            key: &row.key,
            expected_revision,
            intent,
            owner: &self.run.holder,
            run_id: &self.run.run,
            ttl: GRANT_TTL,
        };
        Ok(match o_piece_attempts::grant(&self.pool, request).await? {
            GrantOutcome::Granted(grant) => {
                let permit = DispatchPermit::new(grant, row.payload, asked + GRANT_TTL);
                Admission::Granted { permit, lease }
            }
            GrantOutcome::NotAdmitted => Admission::LegacyOff,
            GrantOutcome::Resolved => Admission::AlreadyResolved,
            GrantOutcome::Failed(Failure::CapUnknown) => Admission::CapUnknown,
            GrantOutcome::Failed(
                Failure::NotFound | Failure::UnknownAtCap | Failure::Rejected | Failure::Cap,
            ) => Admission::CapReached,
            GrantOutcome::Stale { .. } => Admission::ObserveOnly,
            GrantOutcome::Open { .. } => Admission::WaitForOpen,
            GrantOutcome::CapReached => Admission::CapReached,
        })
    }
}
