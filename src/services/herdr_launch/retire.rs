//! An exact pane read and nonce CAS shared by explicit retire and Missing recovery.
use sqlx::PgPool;

use crate::db::dispatched_sessions::hosted_execution::{
    HostedCasOutcome, HostedExecution, HostedObservation, HostedOwner, HostedRecord, HostedState,
    retire_pg,
};
use crate::services::claude::herdr_turn::{HoldRelease, release_hold};
use crate::services::session_host::{PaneProvider, PaneReading, herdr_endpoints};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Ended,
    Missing,
}

/// Why a retire changed nothing: the row and every hold are as they were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetireRefusal {
    NoLocalEndpoint,
    NoRow(&'static str),
    ProviderRunning,
    Unproven(&'static str),
    Changed(String),
}

/// A retired execution and what became of its input hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retired {
    pub nonce: String,
    pub now: bool,
    pub hold: HoldRelease,
}

/// Reads only the registered exact endpoint, then retires only that observed row and nonce.
pub async fn observed(
    pool: &PgPool,
    row: &HostedObservation,
    scope: Scope,
) -> Result<Retired, RetireRefusal> {
    let HostedRecord::Known(record) = &row.record else {
        return Err(RetireRefusal::NoRow("unreadable record"));
    };
    let view = herdr_endpoints()
        .view(record)
        .ok_or(match record.location {
            None => RetireRefusal::Unproven("no recorded pane"),
            Some(_) => RetireRefusal::NoLocalEndpoint,
        })?;
    let proof_record = record.clone();
    let (reading, ended) = tokio::task::spawn_blocking(move || {
        let reading = view.read_execution();
        let ended = view.ended(&proof_record, &reading);
        (reading, ended)
    })
    .await
    .unwrap_or_else(|error| (PaneReading::Unreadable(error.to_string()), false));
    if scope == Scope::Missing && !matches!(reading, PaneReading::Missing) {
        return Err(RetireRefusal::Unproven("pane is not confirmed missing"));
    }
    let retired = on_reading(pool, row, record, &reading, ended).await?;
    if scope == Scope::Missing {
        let mut missing = MISSING.lock().unwrap_or_else(|e| e.into_inner());
        missing.retain(|(owner, _)| owner != &record.owner);
        missing.push((record.owner.clone(), record.execution_nonce.clone()));
    }
    Ok(retired)
}

/// The hold goes only after this observation's CAS wrote Retired.
pub async fn on_reading(
    pool: &PgPool,
    row: &HostedObservation,
    record: &HostedExecution,
    reading: &PaneReading,
    ended: bool,
) -> Result<Retired, RetireRefusal> {
    if !ended {
        return Err(match reading {
            PaneReading::Present {
                provider: PaneProvider::Execution(_),
                ..
            } => RetireRefusal::ProviderRunning,
            PaneReading::Present {
                provider: PaneProvider::Exited,
                ..
            } if record.expected.is_none() => RetireRefusal::Unproven("no recorded root shell"),
            PaneReading::Present {
                provider: PaneProvider::Exited,
                ..
            } => RetireRefusal::Unproven("root shell replaced"),
            PaneReading::Present { .. } => RetireRefusal::Unproven("provider unverified"),
            _ => RetireRefusal::Unproven("pane unreadable or snapshot incomplete"),
        });
    }
    let nonce = &record.execution_nonce;
    match retire_pg(pool, row, &record.owner, nonce).await {
        Ok(HostedCasOutcome::Written) => Ok(Retired {
            nonce: nonce.clone(),
            now: true,
            hold: release_hold(nonce),
        }),
        Ok(HostedCasOutcome::Stale) => Err(RetireRefusal::Changed("row changed since read".into())),
        Err(error) => Err(RetireRefusal::Changed(format!("{error:?}"))),
    }
}

// Preserve the restart repair's scope until the next front can be judged; no input is stored.
type MissingRetirements = Vec<(HostedOwner, String)>;
static MISSING: std::sync::LazyLock<std::sync::Mutex<MissingRetirements>> =
    std::sync::LazyLock::new(Default::default);

pub fn recovered_missing(record: &HostedExecution) -> bool {
    let mut missing = MISSING.lock().unwrap_or_else(|e| e.into_inner());
    if record.state != HostedState::Retired {
        missing.retain(|(owner, _)| owner != &record.owner);
        return false;
    }
    missing
        .iter()
        .any(|(owner, nonce)| owner == &record.owner && nonce == &record.execution_nonce)
}
