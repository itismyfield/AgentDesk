//! Durable `/clear` cutoff for a ledger channel: inputs accepted before the cutoff end as
//! `UserClear`, later ones keep their owner. No production path selects it yet.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use tokio::sync::{OwnedMutexGuard, oneshot};

use crate::db::session_transcripts::{
    NativeClearGeneration, NativeClearRecord, NativeClearResolve,
};
use crate::services::tui_input::ledger::Ledger;
use crate::services::tui_input::rows::{AbandonReason, Entry, RowState};
use crate::services::tui_prompt_dedupe::native_clear::{ClearTicket, InputCutoff};

pub(crate) type Step<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Ticket payload bound; a larger cutoff is refused before anything is written.
pub(crate) const MAX_AFFECTED_KEYS: usize = 256;
pub(crate) const PG_RETRY_NOTICE: &str =
    "PG 복구 후 /clear를 재시도해 주세요. 입력 책임은 보존되며 완료 확인 전 제출은 보류됩니다.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    LedgerUnreadable,
    Unbound,
    TooManyInputs,
    ExecutionUnknown,
    CommitFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unresolved {
    PgUnavailable,
    CommitUncertain,
    Foreign,
    Mismatch,
    Superseded,
    WalUncertain,
    ResetUnconfirmed,
    ResolveUncertain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Cleared,
    /// No input cutoff is pending.
    Idle,
    /// Nothing durable changed; every input keeps its owner.
    Refused(Refusal),
    /// Input admission stays closed until a later resume settles the clear.
    Held(Unresolved),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Execution {
    Current,
    Replaced,
    Retired,
    Unknown,
}

pub(crate) struct Identity {
    pub provider: String,
    pub channel: u64,
    pub tmux: String,
    pub host: Option<String>,
}

/// One channel's clear effects. They run while the clear owns the session transition guard and
/// must not wait for it.
pub(crate) trait ClearHost: Send + 'static {
    fn identity(&self) -> Identity;
    /// The running execution as a ticket without a cutoff; `None` when it is not identified.
    fn capture(&mut self) -> Option<ClearTicket>;
    fn record(&mut self) -> Step<'_, anyhow::Result<Option<NativeClearRecord>>>;
    fn commit<'a>(
        &'a mut self,
        ticket: &'a serde_json::Value,
    ) -> Step<'a, anyhow::Result<NativeClearGeneration>>;
    fn execution(&mut self, ticket: &ClearTicket) -> Execution;
    /// The managed reset, applied only while the ticket's execution is still current.
    fn reset<'a>(&'a mut self, ticket: &'a ClearTicket) -> Step<'a, bool>;
    fn resolve(
        &mut self,
        generation: NativeClearGeneration,
    ) -> Step<'_, anyhow::Result<NativeClearResolve>>;
    fn notice<'a>(&'a mut self, text: &'a str) -> Step<'a, ()>;
}

static UNRESOLVED: Mutex<BTreeMap<u64, (String, Unresolved)>> = Mutex::new(BTreeMap::new());

pub(crate) fn health_reasons() -> Vec<String> {
    let held = UNRESOLVED.lock().unwrap_or_else(|e| e.into_inner());
    held.iter()
        .map(|(channel, (provider, reason))| {
            format!("clear_unresolved provider={provider} channel={channel} reason={reason:?}")
        })
        .collect()
}

/// Runs a user clear after input admission closed and the actor drained: an earlier unresolved
/// cutoff settles first, then the inputs this ledger owns now are cut.
pub(crate) async fn run<H: ClearHost>(
    ledger: &mut Ledger,
    host: &mut H,
    guard: OwnedMutexGuard<()>,
) -> Outcome {
    let outcome = match settle_prior(ledger, host).await {
        Some(held) => held,
        None => match cutoff(ledger, host) {
            Err(refusal) => Outcome::Refused(refusal),
            Ok(ticket) => match commit(host, &ticket).await {
                Err(outcome) => outcome,
                Ok(generation) => finish(ledger, host, &ticket, generation).await,
            },
        },
    };
    report(host, outcome).await;
    drop(guard);
    outcome
}

/// Settles a cutoff a crash left unresolved; it runs before the actor starts or a handback.
pub(crate) async fn resume<H: ClearHost>(
    ledger: &mut Ledger,
    host: &mut H,
    guard: OwnedMutexGuard<()>,
) -> Outcome {
    let outcome = match host.record().await {
        Err(_) => Outcome::Held(Unresolved::PgUnavailable),
        Ok(record) => match pending(&host.identity(), record) {
            Pending::None => Outcome::Idle,
            Pending::Held(reason) => Outcome::Held(reason),
            Pending::Superseded => Outcome::Held(Unresolved::Superseded),
            Pending::Unresolved(ticket, generation) => {
                finish(ledger, host, &ticket, generation).await
            }
        },
    };
    report(host, outcome).await;
    drop(guard);
    outcome
}

/// Runs [`run`] on a blocking worker; a dropped receiver neither cancels it nor frees `guard`
/// before the outcome is durable.
pub(crate) fn start<H: ClearHost>(
    ledger: Ledger,
    host: H,
    guard: OwnedMutexGuard<()>,
) -> oneshot::Receiver<(Ledger, Outcome)> {
    on_worker(ledger, host, guard, false)
}

/// Runs [`resume`] behind the same blocking boundary as [`start`].
pub(crate) fn resume_blocking<H: ClearHost>(
    ledger: Ledger,
    host: H,
    guard: OwnedMutexGuard<()>,
) -> oneshot::Receiver<(Ledger, Outcome)> {
    on_worker(ledger, host, guard, true)
}

fn on_worker<H: ClearHost>(
    mut ledger: Ledger,
    mut host: H,
    guard: OwnedMutexGuard<()>,
    replay: bool,
) -> oneshot::Receiver<(Ledger, Outcome)> {
    let (send, recv) = oneshot::channel();
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let outcome = runtime.block_on(async {
            match replay {
                true => resume(&mut ledger, &mut host, guard).await,
                false => run(&mut ledger, &mut host, guard).await,
            }
        });
        let _ = send.send((ledger, outcome));
    });
    recv
}

enum Pending {
    None,
    Held(Unresolved),
    Superseded,
    Unresolved(ClearTicket, NativeClearGeneration),
}

fn pending(identity: &Identity, record: Option<NativeClearRecord>) -> Pending {
    let Some(record) = record.filter(|record| !record.resolved) else {
        return Pending::None;
    };
    let ticket = record.ticket.clone();
    let ticket = ticket.and_then(|value| serde_json::from_value::<ClearTicket>(value).ok());
    let Some(ticket) = ticket.filter(|ticket| ticket.input.is_some()) else {
        // A native correlation is not this runtime's to settle while it is still current.
        return match record.superseded || record.after_frontier {
            true => Pending::None,
            false => Pending::Held(Unresolved::Foreign),
        };
    };
    if !names(identity, &ticket) {
        return Pending::Held(Unresolved::Mismatch);
    }
    match record.superseded {
        true => Pending::Superseded,
        false => Pending::Unresolved(ticket, record.generation),
    }
}

fn names(identity: &Identity, ticket: &ClearTicket) -> bool {
    let context = &ticket.context;
    let host = context.host.as_deref().filter(|h| !h.trim().is_empty());
    context.schema == 1
        && context.provider == identity.provider
        && context.channel_id == Some(identity.channel)
        && context.tmux_session == identity.tmux
        && !context.execution_nonce.is_empty()
        && host.is_some()
        && host == identity.host.as_deref()
}

// A superseded earlier cutoff needs no replay: the new cutoff names whichever of its inputs are
// still open.
async fn settle_prior<H: ClearHost>(ledger: &mut Ledger, host: &mut H) -> Option<Outcome> {
    let record = match host.record().await {
        Ok(record) => record,
        Err(_) => return Some(Outcome::Held(Unresolved::PgUnavailable)),
    };
    match pending(&host.identity(), record) {
        Pending::None | Pending::Superseded => None,
        Pending::Held(reason) => Some(Outcome::Held(reason)),
        Pending::Unresolved(ticket, generation) => {
            match finish(ledger, host, &ticket, generation).await {
                Outcome::Cleared => None,
                outcome => Some(outcome),
            }
        }
    }
}

fn cutoff<H: ClearHost>(ledger: &Ledger, host: &mut H) -> Result<ClearTicket, Refusal> {
    let rows = ledger.rows().map_err(|_| Refusal::LedgerUnreadable)?;
    if !rows.unbound().is_empty() {
        return Err(Refusal::Unbound);
    }
    let affected_keys: Vec<u64> = rows.open_rows().map(|(key, _)| key).collect();
    if affected_keys.len() > MAX_AFFECTED_KEYS {
        return Err(Refusal::TooManyInputs);
    }
    let mut ticket = host.capture().ok_or(Refusal::ExecutionUnknown)?;
    if ticket.input.is_some() || !names(&host.identity(), &ticket) {
        return Err(Refusal::ExecutionUnknown);
    }
    ticket.input = Some(InputCutoff {
        ledger_generation: ledger.snapshot().map_or(0, |s| s.generation),
        ledger_seq: rows.folded_seq(),
        affected_keys,
    });
    Ok(ticket)
}

// A failed commit is read back first; only a confirmed absence fails the clear.
async fn commit<H: ClearHost>(
    host: &mut H,
    ticket: &ClearTicket,
) -> Result<NativeClearGeneration, Outcome> {
    let value =
        serde_json::to_value(ticket).map_err(|_| Outcome::Refused(Refusal::CommitFailed))?;
    if let Ok(generation) = host.commit(&value).await {
        return Ok(generation);
    }
    match host.record().await {
        Ok(Some(record)) if !record.resolved && !record.superseded => {
            let stored = record.ticket.map(serde_json::from_value::<ClearTicket>);
            match stored {
                Some(Ok(stored)) if &stored == ticket => Ok(record.generation),
                _ => Err(Outcome::Refused(Refusal::CommitFailed)),
            }
        }
        Ok(_) => Err(Outcome::Refused(Refusal::CommitFailed)),
        Err(_) => Err(Outcome::Held(Unresolved::CommitUncertain)),
    }
}

// The completion mark is written only after every cut transition and the reset are confirmed.
async fn finish<H: ClearHost>(
    ledger: &mut Ledger,
    host: &mut H,
    ticket: &ClearTicket,
    generation: NativeClearGeneration,
) -> Outcome {
    let Some(cut) = ticket.input.as_ref() else {
        return Outcome::Held(Unresolved::Foreign);
    };
    if let Err(reason) = cut_inputs(ledger, cut) {
        return Outcome::Held(reason);
    }
    match host.execution(ticket) {
        Execution::Current if !host.reset(ticket).await => {
            return Outcome::Held(Unresolved::ResetUnconfirmed);
        }
        Execution::Unknown => return Outcome::Held(Unresolved::ResetUnconfirmed),
        _ => {}
    }
    if matches!(
        host.execution(ticket),
        Execution::Current | Execution::Unknown
    ) {
        return Outcome::Held(Unresolved::ResetUnconfirmed);
    }
    if let Ok(NativeClearResolve::Resolved) = host.resolve(generation).await {
        return Outcome::Cleared;
    }
    match host.record().await {
        Ok(Some(record)) if record.generation == generation && record.resolved => Outcome::Cleared,
        Ok(_) => Outcome::Held(Unresolved::Superseded),
        Err(_) => Outcome::Held(Unresolved::ResolveUncertain),
    }
}

// Cuts only rows that existed at the cutoff; a later input under a reused key holds the clear.
fn cut_inputs(ledger: &mut Ledger, cut: &InputCutoff) -> Result<(), Unresolved> {
    let rows = ledger.rows().map_err(|_| Unresolved::WalUncertain)?;
    let generation = ledger.snapshot().map_or(0, |s| s.generation);
    if rows.folded_seq() < cut.ledger_seq || generation < cut.ledger_generation {
        return Err(Unresolved::Mismatch);
    }
    let mut open = Vec::new();
    for &key in &cut.affected_keys {
        match rows.row(key) {
            None => return Err(Unresolved::Mismatch),
            Some(row) if row.state.is_terminal() => {}
            Some(row) if row.since_seq > cut.ledger_seq => return Err(Unresolved::Mismatch),
            Some(_) => open.push(key),
        }
    }
    for key in open {
        let entry = Entry::Transition {
            key,
            state: RowState::Abandoned(AbandonReason::UserClear),
            attempt: None,
        };
        ledger
            .append_entry(&entry, &[])
            .map_err(|_| Unresolved::WalUncertain)?;
    }
    Ok(())
}

async fn report<H: ClearHost>(host: &mut H, outcome: Outcome) {
    let identity = host.identity();
    {
        let mut held = UNRESOLVED.lock().unwrap_or_else(|e| e.into_inner());
        match outcome {
            Outcome::Held(reason) => {
                held.insert(identity.channel, (identity.provider, reason));
            }
            Outcome::Cleared | Outcome::Idle => {
                held.remove(&identity.channel);
            }
            Outcome::Refused(_) => {}
        }
    }
    if let Outcome::Held(reason) = outcome {
        let text = match reason {
            Unresolved::PgUnavailable
            | Unresolved::CommitUncertain
            | Unresolved::ResolveUncertain => PG_RETRY_NOTICE.to_owned(),
            reason => format!(
                "/clear를 확정하지 못해 입력 제출을 보류했어요 ({reason:?}). 입력 책임은 보존됩니다."
            ),
        };
        host.notice(&text).await;
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
#[path = "clear_tests.rs"]
mod tests;
