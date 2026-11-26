use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;

use crate::db::server_campaigns::StoreError;

pub use super::adk_observe::{DeploymentExpectation, DeploymentObservation};
use super::adk_observe::{is_sha, observe_deployment};
use super::host_gate::{JournalEntry, JournalState};
use super::host_worker::{HostWorker, WorkerError};
use super::verification_run::{Digest, digest_of};
use crate::domain::campaign::model::CampaignState;

/// The deploy initiator binds this watch in its effect payload under the final fence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentWatch {
    pub action_id: String,
    pub not_before: DateTime<Utc>,
    pub deadline: DateTime<Utc>,
    pub targets: Vec<DeploymentExpectation>,
}

impl DeploymentWatch {
    pub fn payload_digest(&self) -> Result<Digest, WorkerError> {
        let mut hosts = BTreeSet::new();
        let first = self.targets.first().ok_or(WorkerError::Binding)?;
        if self.action_id.trim().is_empty()
            || self.not_before >= self.deadline
            || !is_sha(&first.expected_sha)
            || self.targets.iter().any(|target| {
                target.action_id != self.action_id
                    || target.expected_sha != first.expected_sha
                    || target.host.trim().is_empty()
                    || !hosts.insert(&target.host)
                    || target.previous_incarnation.trim().is_empty()
                    || target
                        .expected_binary_digest
                        .as_ref()
                        .is_some_and(|d| d.trim().is_empty())
            })
        {
            return Err(WorkerError::Binding);
        }
        digest_of(self).map_err(|error| WorkerError::Store(StoreError::from(error)))
    }

    fn bind(&self, entry: &JournalEntry, payload: &str) -> Result<String, WorkerError> {
        if entry.identity.payload_digest != payload
            || entry.identity.action_id != self.action_id
            || self.not_before > entry.updated_at
            || entry.attempts == 0
            || !matches!(
                entry.state,
                JournalState::Started { .. }
                    | JournalState::Finished { .. }
                    | JournalState::Indeterminate { .. }
            )
        {
            return Err(WorkerError::Binding);
        }
        Ok(entry.initiation_id())
    }
}

pub type HealthReply<'a> = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

/// Read authenticated detailed health directly from the named host on each call.
pub trait DeploymentHealth: Send + Sync {
    fn detail<'a>(&'a self, host: &'a str) -> HealthReply<'a>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentHostEvidence {
    pub deployment: DeploymentObservation,
    pub observed_at: DateTime<Utc>,
    pub boot_started_at_ms: i64,
    pub build_target: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum DeploymentProgress {
    Waiting {
        missing_hosts: Vec<String>,
    },
    Confirmed {
        witnesses: Vec<DeploymentHostEvidence>,
    },
    Failed {
        missing_hosts: Vec<String>,
    },
}

#[derive(Serialize, Deserialize)]
struct SavedObservation {
    payload: Digest,
    initiation_id: String,
    recorded_at: DateTime<Utc>,
    result: DeploymentProgress,
}

fn verify_sample(
    target: &DeploymentExpectation,
    health: &Value,
    not_before: DateTime<Utc>,
    requested_at: DateTime<Utc>,
    received_at: DateTime<Utc>,
) -> Option<DeploymentHostEvidence> {
    let witness = health.get("running_binary")?;
    let observed_at: DateTime<Utc> = witness.get("observed_at")?.as_str()?.parse().ok()?;
    let boot_started_at_ms = witness.get("boot_started_at_ms")?.as_i64()?;
    let build_target = witness.get("target")?.as_str()?;
    if observed_at <= requested_at
        || observed_at > received_at
        || boot_started_at_ms <= not_before.timestamp_millis()
        || boot_started_at_ms > observed_at.timestamp_millis()
        || build_target.trim().is_empty()
    {
        return None;
    }
    let observation = observe_deployment(target, Ok(health), observed_at);
    let deployment = observation.observed_evidence()?;
    deployment
        .is_expected_running(target.expected_binary_digest.as_deref())
        .then(|| DeploymentHostEvidence {
            deployment: deployment.clone(),
            observed_at,
            boot_started_at_ms,
            build_target: build_target.into(),
        })
}

async fn sample(
    probe: &dyn DeploymentHealth,
    target: &DeploymentExpectation,
    not_before: DateTime<Utc>,
    timeout: Duration,
) -> Option<DeploymentHostEvidence> {
    let started_at = Utc::now();
    let health = tokio::time::timeout(timeout, probe.detail(&target.host))
        .await
        .ok()?
        .ok()?;
    verify_sample(target, &health, not_before, started_at, Utc::now())
}

fn settle(
    watch: &DeploymentWatch,
    samples: Vec<Option<DeploymentHostEvidence>>,
    now: DateTime<Utc>,
) -> DeploymentProgress {
    let missing_hosts = watch
        .targets
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            now >= watch.deadline || samples.get(*index).is_none_or(Option::is_none)
        })
        .map(|(_, target)| target.host.clone())
        .collect::<Vec<_>>();
    if now >= watch.deadline {
        DeploymentProgress::Failed { missing_hosts }
    } else if missing_hosts.is_empty() {
        DeploymentProgress::Confirmed {
            witnesses: samples.into_iter().flatten().collect(),
        }
    } else {
        DeploymentProgress::Waiting { missing_hosts }
    }
}

fn initiation(
    worker: &HostWorker,
    state: &CampaignState,
    watch: &DeploymentWatch,
    payload: &str,
) -> Result<String, WorkerError> {
    watch.bind(worker.deployment_entry(state, &watch.action_id)?, payload)
}

/// Reconcile an attempted deployment on the fixed worker, including after a stop or restart.
pub async fn step_deployment(
    worker: &mut HostWorker,
    pool: &PgPool,
    watch: &DeploymentWatch,
    probe: &dyn DeploymentHealth,
    per_request: Duration,
) -> Result<DeploymentProgress, WorkerError> {
    let payload = watch.payload_digest()?;
    let (tx, state) = worker.locked(pool).await?;
    let initiation_id = initiation(worker, &state, watch, &payload)?;
    drop(tx);

    let journal = worker.deployment_observations()?;
    let name = format!("{payload}.json");
    if let Some(held) = journal.read::<SavedObservation>(&name)? {
        if held.payload != payload || held.initiation_id != initiation_id {
            return Err(WorkerError::Binding);
        }
        // Failure is terminal. A prior success is audit evidence and cannot authorize a new verdict.
        if matches!(held.result, DeploymentProgress::Failed { .. }) {
            return Ok(held.result);
        }
    }

    let now = Utc::now();
    let samples = if let Some(remaining) = (watch.deadline - now)
        .to_std()
        .ok()
        .filter(|d| !d.is_zero())
    {
        join_all(
            watch
                .targets
                .iter()
                .map(|target| sample(probe, target, watch.not_before, per_request.min(remaining))),
        )
        .await
    } else {
        Vec::new()
    };
    let (tx, state) = worker.locked(pool).await?;
    if initiation(worker, &state, watch, &payload)? != initiation_id {
        return Err(WorkerError::Binding);
    }
    let now = Utc::now();
    let result = settle(watch, samples, now);
    if !matches!(result, DeploymentProgress::Waiting { .. }) {
        journal.write(
            &name,
            &SavedObservation {
                payload,
                initiation_id,
                recorded_at: now,
                result: result.clone(),
            },
        )?;
    }
    drop(tx);
    Ok(result)
}

#[cfg(test)]
#[path = "deploy_observe_tests.rs"]
mod tests;
