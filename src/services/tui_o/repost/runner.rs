//! The re-post turn. Off, nothing past the switch runs: no PostgreSQL, probe or sidecar step.
//! On, a tick advances admitted probes and a turn grants a slot only on fresh absence or approval.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use sqlx::Row;
use sqlx::postgres::PgPool;
use tokio::time::Instant;

use super::admission::{
    Admitter, AdoptError, ApprovedRejected, Candidate, OriginalGate, PriorBudget, classify,
};
use super::config::RepostSwitch;
use super::dispatch::{self, Admission, DispatchPermit, GuardReason, RepostWriter};
use super::io::probe::evidence::{EvidenceScope, NotFoundEvidence, RunScope};
use super::io::probe::matcher::AttributionSnapshot;
use super::io::probe::{ProbeRead, ProbeSession, Progress};
use super::o_piece_attempts::{self, GrantOutcome, GrantRequest, Intent};
use super::o_piece_delivery::{
    self, AdmitOutcome, Failure, LedgerError, PieceKey, ReceiptOutcome, kind_name, provider_name,
};
use super::provenance::{ProvenanceEntry, ProvenanceLog};
use super::send::BoundedTransport;
use crate::services::tui_o::ownership::GatewayOwnership;
use crate::services::tui_o::store::ledger::{LedgerState, PieceOutcome, PieceRecord};

/// How long a granted slot stays open before another holder may settle it as abandoned.
pub(crate) const GRANT_TTL: Duration = Duration::from_secs(180);

/// Re-post uses the gateway gate; delegated or unavailable homes need their own admission path.
pub(crate) fn scope_allowed(channel: u64) -> bool {
    crate::services::cluster::channel_home::registered_channel(channel).is_none()
        && crate::services::cluster::home_availability::refusal(channel).is_none()
}

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

    /// Whether the effective switch is on now; off reads nothing else.
    pub(crate) fn enabled(&mut self) -> bool {
        self.switch.when_enabled(|_| ()).is_some()
    }

    fn admitter(&mut self) -> Option<Admitter<'_>> {
        let (pool, holder) = (&self.pool, self.run.holder.as_str());
        Admitter::when_on(&mut self.switch, pool, holder, self.sender_id)
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
        if !scope_allowed(channel) {
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
        // Row and attempt reads yield; current source policy must still permit consuming a slot.
        if let Err(reason) = writer.guard() {
            return Ok(Admission::GuardRejected(GuardReason::Writer(reason)));
        }
        let auto = matches!(intent, Intent::AutoReconfirm);
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
            // Auto evidence whose row went before the lock proves nothing: observe, never legacy.
            GrantOutcome::NotAdmitted if auto => Admission::ObserveOnly,
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

/// What a channel writer lends its re-post turn beyond one dispatch.
pub(crate) trait TurnWriter: RepostWriter + Send {
    fn channel(&self) -> u64;
    fn ledger(&mut self) -> &LedgerState;
    /// Whether a re-post request may still leave; read by every dispatch's wire guard.
    fn set_live(&self, live: bool);
    fn bind_switch(&self, _effective: Arc<dyn Fn() -> bool + Send + Sync>) {}
}

pub(crate) type Turned<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The re-post side of one channel's writer, run only from that writer's actor turn, so it never
/// sends beside the writer's own posts.
pub(crate) trait RepostHook<W>: Send {
    /// One turn; off, it returns before PostgreSQL, the sidecar, the transport or Discord.
    fn turn<'a>(&'a mut self, writer: &'a mut W) -> Turned<'a, ()>;
    /// Whether a fresh original at `serial` may go: off, always, reading nothing; on, never for an
    /// admitted key, not while admission is unknown, and only once its intent is written.
    fn original(&mut self, serial: u64, record: PieceRecord) -> Turned<'_, OriginalGate>;
}

/// Builds a channel's re-post transport; asked only once re-post is on.
pub(crate) type OpenTransport<T> = Box<dyn FnMut() -> Option<Arc<T>> + Send>;

/// One channel's re-post turn: admits uncertain on originals, probes them and sends at most one
/// granted re-post per turn.
pub(crate) struct ChannelTurn<T> {
    runner: Runner,
    open: OpenTransport<T>,
    io: Option<Arc<T>>,
    channel: u64,
    sidecar: PathBuf,
    log: Option<ProvenanceLog>,
    /// Whether the open sidecar recorded the current on period's activation.
    activated: bool,
}

impl<T: BoundedTransport + ProbeRead + Send + Sync + 'static> ChannelTurn<T> {
    pub(crate) fn new(
        runner: Runner,
        open: OpenTransport<T>,
        channel: u64,
        sidecar: PathBuf,
    ) -> Self {
        Self {
            runner,
            open,
            io: None,
            channel,
            sidecar,
            log: None,
            activated: false,
        }
    }

    /// The sidecar, opened and activated once per on period. A failed write drops it, so the next
    /// open cuts any torn tail before another entry lands.
    fn log(&mut self, frontier: u64) -> Option<&mut ProvenanceLog> {
        if self.log.is_none() {
            if let Some(dir) = self.sidecar.parent() {
                std::fs::create_dir_all(dir).ok()?;
            }
            self.log = ProvenanceLog::open(&self.sidecar).ok();
            self.activated = false;
        }
        if !self.activated {
            if self.log.as_mut()?.activate(frontier).is_err() {
                self.log = None;
                return None;
            }
            self.activated = true;
        }
        self.log.as_mut()
    }

    async fn run<W: TurnWriter>(&mut self, writer: &mut W) {
        let on = self.runner.enabled();
        writer.set_live(on);
        if !on {
            // The next on period is a new activation.
            self.activated = false;
            return;
        }
        writer.bind_switch(self.runner.switch.live());
        if !scope_allowed(self.channel) || writer.guard().is_err() || !self.admit(writer).await {
            writer.set_live(false);
            return;
        }
        if self.io.is_none() {
            self.io = (self.open)();
        }
        let Some(io) = self.io.clone() else {
            return;
        };
        let proof_ids = proof_ids(writer.ledger());
        let ticked = self
            .runner
            .tick(self.channel, &*io, &proof_ids, Instant::now());
        let Ok(Tick::Ran(report)) = ticked.await else {
            return;
        };
        for evidence in report.ready {
            // An excluded unit is never sent again, by the writer or by a re-post.
            if writer
                .ledger()
                .excluded(evidence.scope().key.unit())
                .is_some()
            {
                continue;
            }
            let intent = DispatchIntent::Auto(Box::new(evidence));
            let admitted = self
                .runner
                .try_next_dispatch(writer, OriginalGate::Admitted, intent);
            let Ok(admission) = admitted.await else {
                return;
            };
            match (turn_step(&admission), admission) {
                (TurnStep::Dispatch, Admission::Granted { permit, lease }) => {
                    let started = dispatch::start(writer, permit, lease, &*io).await;
                    // Awaited here, so the actor sends nothing else of this channel meanwhile.
                    if let Err(error) = started.finish(writer, &self.runner.pool).await {
                        tracing::warn!(channel = self.channel, %error, "[tui_o] re-post settle failed");
                    }
                    return;
                }
                (TurnStep::Stopped, _) => return,
                _ => {}
            }
        }
    }

    /// Finishes admissions a crash or an outage left pending, then admits each piece whose latest
    /// attempt is an uncertain original sent while on.
    async fn admit<W: TurnWriter>(&mut self, writer: &mut W) -> bool {
        let ledger = writer.ledger();
        let frontier = ledger.next_serial();
        let latest: BTreeMap<u64, PieceRecord> = (0..frontier)
            .filter_map(|serial| Some((serial, ledger.piece(serial)?)))
            .filter(|(serial, piece)| {
                let uncertain = !matches!(
                    piece.outcome,
                    Some(
                        PieceOutcome::Posted(_) | PieceOutcome::Rejected(_) | PieceOutcome::NotSent
                    )
                );
                let latest = ledger.latest_piece(&piece.unit_key, piece.piece_index);
                uncertain && latest.is_some_and(|(at, _)| at == *serial)
            })
            .map(|(serial, piece)| (serial, piece.clone()))
            .collect();
        if self.log(frontier).is_none() {
            return false;
        }
        let (Some(log), Some(admitter)) = (self.log.as_mut(), self.runner.admitter()) else {
            return false;
        };
        let backlog = match super::admission::report_backlog(
            log,
            latest.iter().map(|(&serial, record)| (serial, record)),
        ) {
            Ok(backlog) => backlog,
            Err(error) => {
                tracing::warn!(channel = self.channel, %error, "[tui_o] re-post backlog report failed");
                self.log = None;
                return false;
            }
        };
        if !backlog.is_empty() {
            tracing::warn!(channel = self.channel, count = backlog.len(), pieces = ?backlog,
                "[tui_o] re-post backlog retained");
        }
        let resumed = admitter
            .resume_pending(log, |serial| latest.get(&serial))
            .await;
        let sidecar =
            |result: &Result<_, AdoptError>| matches!(result, Err(AdoptError::Sidecar(_)));
        let mut torn = resumed.iter().any(|(_, result)| sidecar(result));
        let accepted = |result: &Result<_, AdoptError>| {
            matches!(
                result,
                Ok(AdmitOutcome::Admitted(_) | AdmitOutcome::Existing(_))
            )
        };
        if resumed.iter().any(|(_, result)| !accepted(result)) {
            if torn {
                self.log = None;
            }
            return false;
        }
        for (serial, record) in &latest {
            let Candidate::Eligible(original) = classify(*serial, record, log.state()) else {
                continue;
            };
            let adopted = admitter.adopt_uncertain(log, &original).await;
            torn |= sidecar(&adopted);
            // PostgreSQL left it pending; the next turn resumes it.
            if !accepted(&adopted) {
                if torn {
                    self.log = None;
                }
                return false;
            }
        }
        true
    }

    async fn original_gate(&mut self, serial: u64, record: PieceRecord) -> OriginalGate {
        let channel = self.channel;
        let Some(admitter) = self.runner.admitter() else {
            return OriginalGate::Send;
        };
        let keys = admitter.admitted_keys(channel).await;
        let gate = keys.original(&record.unit_key, record.piece_index);
        if gate != OriginalGate::Send {
            return gate;
        }
        if !scope_allowed(channel) || ProvenanceEntry::intent(serial, &record, 0).is_none() {
            return gate;
        }
        // Supported on originals wait until the intent is durable; failures are not opt-outs.
        let Some(log) = self.log(serial) else {
            return OriginalGate::Unknown;
        };
        let generation = log
            .state()
            .activation()
            .map(|activation| activation.generation);
        let Some(intent) =
            generation.and_then(|generation| ProvenanceEntry::intent(serial, &record, generation))
        else {
            return OriginalGate::Unknown;
        };
        if log.append(intent).is_err() {
            self.log = None;
            return OriginalGate::Unknown;
        }
        gate
    }
}

/// The newest messages the writer posted, read back to prove the probe may read the channel.
fn proof_ids(ledger: &LedgerState) -> Vec<u64> {
    let mut posted: Vec<u64> = (0..ledger.next_serial())
        .filter_map(|serial| match ledger.piece(serial)?.outcome {
            Some(PieceOutcome::Posted(id)) => Some(id),
            _ => None,
        })
        .collect();
    posted.sort_unstable_by(|a, b| b.cmp(a));
    posted.truncate(3);
    posted
}

impl<T, W> RepostHook<W> for ChannelTurn<T>
where
    T: BoundedTransport + ProbeRead + Send + Sync + 'static,
    W: TurnWriter,
{
    fn turn<'a>(&'a mut self, writer: &'a mut W) -> Turned<'a, ()> {
        Box::pin(self.run(writer))
    }

    fn original(&mut self, serial: u64, record: PieceRecord) -> Turned<'_, OriginalGate> {
        Box::pin(self.original_gate(serial, record))
    }
}

#[cfg(test)]
#[path = "runner_activation_tests.rs"]
mod activation_tests;
