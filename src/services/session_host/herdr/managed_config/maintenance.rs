//! Keeps the booted dedicated Herdr configs canonical: one pass at boot, then one task per process
//! re-ensures and re-reads E7 every 30 seconds. Nothing here restarts, stops or reloads a server.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::path::{PersonalPaths, Target};
use super::{EnsureResult, ManagedHome, publication};
use crate::config::session_hosts::{BootSessionHosts, with_boot};
use crate::services::session_host::herdr::model::HerdrEndpoint;
use crate::services::session_host::herdr::observe::{self, RestoreResume, RestoreUnverified};

pub(crate) const INTERVAL: Duration = Duration::from_secs(30);
const STALE_AFTER: Duration = Duration::from_secs(90);
/// E7 needs a start strictly after the barrier plus one second; Linux reads starts up to 1s early.
const START_WAIT: Duration = Duration::from_millis(2_100);

/// The latest upkeep of one dedicated endpoint's home, as health shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Record {
    pub config: &'static str,
    pub backup: Option<PathBuf>,
    pub last_e7: String,
    pub reason: Option<&'static str>,
    pub observed_at: DateTime<Utc>,
    pub earliest_start_after: Option<DateTime<Utc>>,
}

impl Record {
    /// The fixed health reason; an upkeep that stopped reporting is never read as healthy.
    pub(crate) fn reason_at(&self, now: DateTime<Utc>) -> Option<&'static str> {
        let stale = (now - self.observed_at)
            .to_std()
            .is_ok_and(|age| age > STALE_AFTER);
        if stale {
            Some("herdr_config_observation_stale")
        } else {
            self.reason
        }
    }
}

static RECORDS: Mutex<BTreeMap<String, Record>> = Mutex::new(BTreeMap::new());
static TASK: OnceLock<()> = OnceLock::new();

pub(crate) fn records() -> BTreeMap<String, Record> {
    RECORDS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

enum File {
    Ensured(EnsureResult),
    Busy,
    Refused,
    RecordUnreadable,
    Failed,
}

/// Busy is retried next tick; the other failures name a cause that repair alone cannot clear.
fn classify(home: &ManagedHome, error: &io::Error) -> File {
    if error.kind() == io::ErrorKind::WouldBlock {
        File::Busy
    } else if publication::state(&home.path.join("config.toml")).is_err() {
        File::RecordUnreadable
    } else if error.kind() == io::ErrorKind::PermissionDenied || home.verify_dir().is_err() {
        File::Refused
    } else {
        File::Failed
    }
}

fn reason(file: &File, e7: Option<&RestoreResume>) -> Option<&'static str> {
    use RestoreUnverified as Why;
    match file {
        File::Busy => None,
        File::Refused => Some("herdr_managed_home_unsafe"),
        File::RecordUnreadable => Some("herdr_config_record_unreadable"),
        File::Failed => Some("herdr_config_fix_failed"),
        File::Ensured(_) => match e7 {
            Some(RestoreResume::Unverified(
                Why::ConfigChangedSinceStart
                | Why::ConfigNotCanonical
                | Why::ConfigMissing
                | Why::ConfigChanged,
            )) => Some("herdr_config_restart_required"),
            Some(RestoreResume::Unverified(Why::NotBootstrapped)) => Some("herdr_job_env_mismatch"),
            _ => None,
        },
    }
}

fn e7_label(reading: &RestoreResume) -> String {
    match reading {
        RestoreResume::Off { .. } => "off".into(),
        RestoreResume::On => "on".into(),
        RestoreResume::Unverified(why) => format!("unverified:{why:?}"),
    }
}

/// E7 through the production socket and OS reads, exactly as a launch would read it.
fn read_e7(boot: &BootSessionHosts, key: &str, target: &Target) -> RestoreResume {
    use crate::services::session_host::herdr::transport::{
        HerdrSocketConfig, HerdrSocketTransport,
    };
    use crate::services::session_host::herdr::wire::LineJsonFraming;
    let configured = boot.config().herdr.endpoints.get(key);
    let endpoint = configured.zip(boot.local_node()).and_then(|(e, node)| {
        HerdrEndpoint::new(node, key, &e.socket_path, &e.herdr_session)
            .and_then(|endpoint| endpoint.with_herdr_home(&target.declared))
            .ok()
    });
    let Some(endpoint) = endpoint else {
        return RestoreResume::Unverified(RestoreUnverified::NotBootstrapped);
    };
    let transport =
        HerdrSocketTransport::new(&endpoint, HerdrSocketConfig::default(), LineJsonFraming);
    observe::read_restore_resume(&transport, &endpoint)
}

/// One upkeep pass over the booted dedicated homes. A refused or failing home never blocks
/// another; a busy home keeps its last record until the next tick.
pub(crate) fn pass(boot: &BootSessionHosts, personal: &PersonalPaths, observe: bool) {
    let now = SystemTime::now();
    for target in ManagedHome::local(boot, personal) {
        let file = match &target.home {
            Err(_) => File::Refused,
            Ok(home) => match home.ensure() {
                Ok(result) => File::Ensured(result),
                Err(error) => classify(home, &error),
            },
        };
        let barrier = match (&file, &target.home) {
            (File::Ensured(_), Ok(home)) => match home.inspect() {
                super::Inspection::Canonical { committed } => committed,
                _ => None,
            },
            _ => None,
        };
        for key in &target.keys {
            let e7 =
                (observe && matches!(file, File::Ensured(_))).then(|| read_e7(boot, key, &target));
            let mut records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
            let previous = records.get(key).cloned();
            let record = match (&file, previous) {
                (File::Busy, Some(previous)) => Record {
                    config: "busy",
                    ..previous
                },
                _ => Record {
                    config: match &file {
                        File::Ensured(EnsureResult::Unchanged) => "unchanged",
                        File::Ensured(EnsureResult::Prepared { .. }) => "prepared",
                        File::Ensured(EnsureResult::Recovered) => "recovered",
                        File::Busy => "busy",
                        File::Refused => "refused",
                        File::RecordUnreadable => "record_unreadable",
                        File::Failed => "failed",
                    },
                    backup: match &file {
                        File::Ensured(EnsureResult::Prepared { backup }) => backup.clone(),
                        _ => None,
                    },
                    last_e7: e7.as_ref().map_or_else(|| "never".into(), e7_label),
                    reason: reason(&file, e7.as_ref()),
                    observed_at: now.into(),
                    earliest_start_after: barrier.map(|at| (at + START_WAIT).into()),
                },
            };
            if matches!(file, File::Refused | File::RecordUnreadable | File::Failed) {
                tracing::warn!(endpoint = %key, config = record.config, "[herdr] managed config not ensured");
            }
            records.insert(key.clone(), record);
        }
    }
}

/// The boot pass over the snapshot this process installed, never a re-read config.
pub(crate) fn boot() {
    if let Some(boot) = with_boot(|boot| boot.cloned()) {
        pass(&boot, &PersonalPaths::current(), false);
    }
}

/// Starts the process's one upkeep task when the booted snapshot has a local dedicated home.
pub(crate) fn spawn() -> bool {
    let Some(boot) = with_boot(|boot| boot.cloned()) else {
        return false;
    };
    if ManagedHome::local(&boot, &PersonalPaths::current()).is_empty() || TASK.set(()).is_err() {
        return false;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let boot = boot.clone();
            let _ = tokio::task::spawn_blocking(move || {
                pass(&boot, &PersonalPaths::current(), true);
            })
            .await;
        }
    });
    true
}

#[cfg(test)]
#[path = "maintenance_tests.rs"]
mod tests;
