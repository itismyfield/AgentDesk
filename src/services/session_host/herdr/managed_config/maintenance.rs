//! Keeps the booted dedicated Herdr configs canonical: one pass at boot, then one task per process
//! re-ensures, re-reads E7 and moves each home's notice every 30 seconds. Nothing restarts a server.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::super::observe::{RestoreResume, RestoreUnverified};
use super::path::PersonalPaths;
use super::{EnsureResult, ManagedHome, publication};
use crate::config::session_hosts::{BootSessionHosts, with_boot};

pub(crate) const INTERVAL: Duration = Duration::from_secs(30);
const STALE_AFTER: Duration = Duration::from_secs(90);
/// E7 needs a start strictly after the barrier plus one second; Linux reads starts up to 1s early.
const START_WAIT: Duration = Duration::from_millis(2_100);
const RESTART: &str = "herdr_config_restart_required";
const JOB_ENV: &str = "herdr_job_env_mismatch";

/// The latest upkeep of one dedicated endpoint's home, as health shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Record {
    pub config: &'static str,
    pub backup: Option<PathBuf>,
    pub last_e7: String,
    /// The confirmed cause; an unverifiable reading keeps it and only a fresh `Off` clears a
    /// server cause, so uncertainty never reads as recovery.
    pub cause: Option<&'static str>,
    /// Where this home's notice stands: `none`, `pending` (no channel or poster) or the owner's.
    pub notice: &'static str,
    pub observed_at: DateTime<Utc>,
    pub earliest_start_after: Option<DateTime<Utc>>,
}

impl Record {
    /// An upkeep that stopped reporting is never read as healthy; its confirmed cause still stands.
    pub(crate) fn stale_at(&self, now: DateTime<Utc>) -> bool {
        (now - self.observed_at)
            .to_std()
            .is_ok_and(|age| age > STALE_AFTER)
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

fn cause(
    file: &File,
    e7: Option<&RestoreResume>,
    was: Option<&'static str>,
) -> Option<&'static str> {
    use RestoreUnverified as Why;
    match file {
        File::Busy => was,
        File::Refused => Some("herdr_managed_home_unsafe"),
        File::RecordUnreadable => Some("herdr_config_record_unreadable"),
        File::Failed => Some("herdr_config_fix_failed"),
        File::Ensured(_) => match e7 {
            Some(RestoreResume::Off { .. }) => None,
            Some(
                RestoreResume::On
                | RestoreResume::Unverified(
                    Why::ConfigChangedSinceStart
                    | Why::ConfigNotCanonical
                    | Why::ConfigMissing
                    | Why::ConfigChanged,
                ),
            ) => Some(RESTART),
            Some(RestoreResume::Unverified(Why::NotBootstrapped)) => Some(JOB_ENV),
            // An unread or unverifiable server keeps the server cause it already confirmed.
            _ => was.filter(|was| [RESTART, JOB_ENV].contains(was)),
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

/// E7 through the boot registry's launch host, the reading a launch itself checks; `None` when
/// this process registered no launch host.
fn read_e7(boot: &BootSessionHosts, key: &str) -> Option<RestoreResume> {
    use crate::services::herdr_launch::{HerdrLaunchEndpoint, HerdrLaunchHost};
    let configured = boot.config().herdr.endpoints.get(key)?;
    let endpoint = HerdrLaunchEndpoint {
        execution_node: boot.local_node()?.to_owned(),
        config_key: key.to_owned(),
        socket_addr: configured.socket_path.to_string_lossy().into_owned(),
        herdr_session: configured.herdr_session.clone(),
    };
    let registry = super::super::super::herdr_endpoints();
    Some(registry.launch_host()?.restore_resume(&endpoint))
}

/// A home's notice target: a stable fault identity and the one channel that hears it. Delivery
/// lives beside the Unknown notice owner it reuses, in `turn_presence/operator_notice.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OperatorNotice {
    /// Where its receipt is kept; `None` is the runtime's own root.
    pub(crate) root: Option<PathBuf>,
    pub(crate) fault: String,
    pub(crate) channel: u64,
}

impl OperatorNotice {
    /// A managed herdr home's fault on `node`; PIDs, ticks, mtimes and error text are not identity.
    pub(crate) fn herdr_home(node: &str, home: &std::path::Path, channel: u64) -> Self {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        digest.update(node.as_bytes());
        digest.update([0]);
        digest.update(home.as_os_str().as_encoded_bytes());
        Self {
            root: None,
            fault: hex::encode(&digest.finalize()[..8]),
            channel,
        }
    }
}

/// A home's confirmed cause after a pass, and the notice that cause moves.
pub(crate) struct Fault {
    keys: Vec<String>,
    cause: Option<&'static str>,
    /// Every endpoint read a fresh `Off`: only then does a pass without a cause end the episode.
    cleared: bool,
    notice: Option<OperatorNotice>,
}

/// One upkeep pass over the booted dedicated homes. A refused or failing home never blocks
/// another; a busy home keeps its last record until the next tick.
pub(crate) fn pass(boot: &BootSessionHosts, personal: &PersonalPaths, observe: bool) -> Vec<Fault> {
    pass_with(boot, personal, &|key| {
        observe.then(|| read_e7(boot, key)).flatten()
    })
}

fn pass_with(
    boot: &BootSessionHosts,
    personal: &PersonalPaths,
    read: &dyn Fn(&str) -> Option<RestoreResume>,
) -> Vec<Fault> {
    let now = SystemTime::now();
    let mut faults = Vec::new();
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
        let mut home_cause = None;
        let mut cleared = true;
        for key in &target.keys {
            let e7 = matches!(file, File::Ensured(_))
                .then(|| read(key))
                .flatten();
            cleared &= matches!(e7, Some(RestoreResume::Off { .. }));
            let mut records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
            let previous = records.get(key).cloned();
            let was = previous.as_ref().and_then(|record| record.cause);
            let notice = previous.as_ref().map_or("none", |record| record.notice);
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
                    cause: cause(&file, e7.as_ref(), was),
                    notice,
                    observed_at: now.into(),
                    earliest_start_after: barrier.map(|at| (at + START_WAIT).into()),
                },
            };
            // Logged once per cause change, not every tick the cause persists.
            if record.cause != was {
                tracing::warn!(endpoint = %key, config = record.config, cause = ?record.cause, last_e7 = %record.last_e7, "[herdr] managed config upkeep cause changed");
            }
            home_cause = home_cause.or(record.cause);
            records.insert(key.clone(), record);
        }
        // The home's notice goes to the lowest BOOT channel mapped to one of its endpoints.
        let channel = boot
            .channels()
            .iter()
            .find(|(_, endpoint)| target.keys.contains(&endpoint.key))
            .map(|(channel, _)| *channel);
        faults.push(Fault {
            notice: boot
                .local_node()
                .zip(channel)
                .map(|(node, channel)| OperatorNotice::herdr_home(node, &target.declared, channel)),
            keys: target.keys,
            cause: home_cause,
            cleared,
        });
    }
    faults
}

/// Moves each home's notice episode to its confirmed cause; without a channel or a poster the
/// notice stays pending and nothing counts as sent.
pub(crate) async fn notify(faults: Vec<Fault>) {
    for fault in faults {
        let state = match (&fault.notice, fault.cause) {
            // A restart's first unverifiable reading has no cause yet and must not end the episode.
            (Some(_), None) if !fault.cleared => continue,
            (Some(notice), cause) => notice.observe(cause).await,
            (None, Some(_)) => "pending",
            (None, None) => "none",
        };
        let mut records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
        for key in &fault.keys {
            if let Some(record) = records.get_mut(key) {
                record.notice = state;
            }
        }
    }
}

/// The boot pass over the snapshot this process installed, never a re-read config.
pub(crate) fn boot() {
    if let Some(boot) = with_boot(|boot| boot.cloned()) {
        let _ = pass(&boot, &PersonalPaths::current(), false);
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
            let upkept =
                tokio::task::spawn_blocking(move || pass(&boot, &PersonalPaths::current(), true));
            if let Ok(faults) = upkept.await {
                notify(faults).await;
            }
        }
    });
    true
}

#[cfg(test)]
#[path = "maintenance_tests.rs"]
pub(crate) mod tests;
