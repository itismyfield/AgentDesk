//! The boot owner of AGY custody: every route row and every delivery journal short of Complete,
//! whatever the writer selection is, ends once through the journal; nothing reruns.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use poise::serenity_prelude::Http;

use crate::services::discord::SharedData;
use crate::services::discord::health::boot_hold;
use crate::services::discord::inflight::{self, AgyCustodyRoute, CustodyRow};
use crate::services::discord::outbound::o_writer_io::{ChannelLeases, GatewayPort};
use crate::services::provider::ProviderKind;
use crate::services::tui_o::ownership::{self, OwnershipGate};
use crate::services::tui_o::store::agy_delivery::{
    AgyBinding, AgyJournalRoot, AgyOriginal, Saved, TurnKey, unfinished,
};
use crate::services::tui_o::store::agy_spool::CHECKPOINT;
use crate::services::tui_o::store::agy_spool::replay::{
    AgyLaunch, AgyOutcome, AgyReplay, AgySlot, AgyStarted, AgyTarget, replay, version_dir,
};
use crate::services::tui_o::writer::DeliveryLease;
use crate::services::tui_o::writer::agy_delivery::plan::{Abandon, Seal, Terminal};
use crate::services::tui_o::writer::agy_delivery::{
    Plan, Progress, Projection, Replay, Transport, TurnLease, hash,
};

#[cfg(test)]
#[path = "agy_recovery_tests.rs"]
mod tests;

const PROVIDER: &str = "antigravity";
/// The interval 4b may tune; held keys are judged again until none is left.
const RETRY: Duration = Duration::from_secs(5);

/// The provider's boot hold over every unresolved key: released only when none is left.
#[derive(Default)]
pub(super) struct HoldSet(Mutex<BTreeMap<String, String>>);

impl HoldSet {
    pub(super) fn record(&self, key: &str, reason: Option<String>) {
        let mut held = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        match reason {
            Some(reason) => held.insert(key.to_owned(), reason),
            None => held.remove(key),
        };
        match held.values().next() {
            Some(reason) => boot_hold::hold(PROVIDER, "agy_custody", reason),
            None => boot_hold::release(PROVIDER),
        }
    }
}

enum Job {
    Deliver(Plan, Option<TurnLease>),
    Held(String),
    Done,
}

/// Runs after observe: judgement comes first, posting runs in a spawned task that judges the
/// keys it left open again until none is left. An empty boot makes no Discord call.
pub(super) async fn run(http: &Arc<Http>, shared: &Arc<SharedData>, provider: &ProviderKind) {
    // Once per process: a later pass would judge this process's own live producers as orphans.
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if *provider != ProviderKind::Antigravity
        || STARTED.swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        return;
    }
    let Some(root) = crate::config::runtime_root() else {
        return;
    };
    let judged = tokio::task::spawn_blocking({
        let root = root.clone();
        move || judge(&root, None)
    });
    let Ok(jobs) = judged.await else {
        return;
    };
    if jobs.is_empty() {
        return;
    }
    let (http, shared) = (Arc::clone(http), Arc::clone(shared));
    tokio::spawn(async move {
        let gate = ownership::gate(PROVIDER);
        let leases = ChannelLeases::from_shared(Arc::clone(&shared));
        let identity = move || {
            let http = Arc::clone(&http);
            async move {
                let bot = http.get_current_user().await.ok()?.id.get();
                Some(GatewayPort::new(http, bot))
            }
        };
        drive(&root, &gate, &leases, &shared, jobs, identity, RETRY).await;
    });
}

/// Delivers and retries until no key is open. The bot identity is fetched only for delivery work
/// and again on the next pass after a failure, never fixed to an unusable value.
async fn drive<P, Fut>(
    root: &Path,
    gate: &OwnershipGate,
    leases: &impl DeliveryLease,
    projection: &impl Projection,
    mut jobs: BTreeMap<String, Job>,
    mut identity: impl FnMut() -> Fut,
    retry: Duration,
) where
    P: Transport,
    Fut: std::future::Future<Output = Option<P>>,
{
    let (holds, mut port) = (HoldSet::default(), None);
    loop {
        if port.is_none() && jobs.values().any(|job| matches!(job, Job::Deliver(..))) {
            port = identity().await;
        }
        let open = deliver(root, gate, port.as_ref(), leases, projection, &holds, jobs).await;
        if open.is_empty() {
            return;
        }
        tokio::time::sleep(retry).await;
        let (scan, only) = (root.to_owned(), open);
        let Ok(next) = tokio::task::spawn_blocking(move || judge(&scan, Some(&only))).await else {
            return;
        };
        jobs = next;
    }
}

/// One pass over everything found, or over the keys `only` left open: the keys still unresolved.
pub(super) async fn recover(
    root: &Path,
    gate: &OwnershipGate,
    port: &impl Transport,
    leases: &impl DeliveryLease,
    projection: &impl Projection,
    holds: &HoldSet,
    only: Option<&BTreeSet<String>>,
) -> BTreeSet<String> {
    let jobs = judge(root, only);
    deliver(root, gate, Some(port), leases, projection, holds, jobs).await
}

async fn deliver(
    root: &Path,
    gate: &OwnershipGate,
    port: Option<&impl Transport>,
    leases: &impl DeliveryLease,
    projection: &impl Projection,
    holds: &HoldSet,
    jobs: BTreeMap<String, Job>,
) -> BTreeSet<String> {
    // Every unresolved key is held before any transport, so one key's end never hides another.
    for (name, job) in &jobs {
        if !matches!(job, Job::Done) {
            holds.record(name, Some("agy custody is being ended".into()));
        }
    }
    let mut open = BTreeSet::new();
    for (name, job) in jobs {
        let held = match (job, port) {
            (Job::Done, _) => None,
            (Job::Held(reason), _) => Some(reason),
            (Job::Deliver(..), None) => Some("agy owner bot identity is unavailable".into()),
            (Job::Deliver(plan, lease), Some(port)) => {
                settle(root, gate, port, leases, projection, &plan, lease)
                    .await
                    .err()
            }
        };
        if held.is_some() {
            open.insert(name.clone());
        }
        holds.record(&name, held);
    }
    open
}

/// Drives the journal to Complete: posting, then clearing the row, the lease and the queue.
async fn settle(
    root: &Path,
    gate: &OwnershipGate,
    port: &impl Transport,
    leases: &impl DeliveryLease,
    projection: &impl Projection,
    plan: &Plan,
    lease: Option<TurnLease>,
) -> Result<(), String> {
    let epoch = gate
        .current()
        .epoch()
        .ok_or("agy owner has no ownership epoch")?;
    let mut store =
        AgyJournalRoot::open(root, plan.key.destination).map_err(|e| format!("{e:?}"))?;
    let mut replay = match lease {
        Some(lease) => Replay::open(&mut store, gate, epoch, lease, plan),
        None => Replay::recover(&mut store, gate, epoch, plan),
    }
    .map_err(|error| format!("{error:?}"))?;
    for _ in 0..plan.body.len() * 3 + 8 {
        match replay.advance(port, leases, projection).await {
            Ok(Progress::More) => {}
            Ok(Progress::Complete) => return Ok(()),
            Ok(Progress::Held(reason)) => return Err(reason),
            Err(error) => return Err(format!("{error:?}")),
        }
    }
    Err("agy owner delivery did not finish".into())
}

struct Found {
    key: TurnKey,
    route: Option<AgyCustodyRoute>,
    saved: Option<Saved>,
}

/// The one key a failed discovery holds; a later complete scan takes up the debt it hid.
const DISCOVERY: &str = "discovery";

/// Route rows and unfinished journals by full turn key; a retry (`only`) decides nothing about a key
/// it did not leave open or a route this process wrote, and an incomplete scan proves nothing gone.
fn judge(root: &Path, only: Option<&BTreeSet<String>>) -> BTreeMap<String, Job> {
    let mut found = BTreeMap::<String, Found>::new();
    let (mut held_rows, mut failed) = (Vec::new(), Vec::new());
    match inflight::custody_rows(&ProviderKind::Antigravity) {
        Ok(rows) => {
            for row in rows {
                match row {
                    CustodyRow::Owned(route) => {
                        if let Some(key) = route_key(&route) {
                            entry(&mut found, key).route = Some(route);
                        }
                    }
                    CustodyRow::Held(channel) => held_rows.push(format!("row:{channel}")),
                }
            }
        }
        Err(error) => failed.push(error),
    }
    for saved in unfinished(root) {
        match saved {
            Ok(saved) => entry(&mut found, saved.key).saved = Some(saved),
            Err(error) => failed.push(error),
        }
    }
    let adopt = failed.is_empty() && only.is_some_and(|only| only.contains(DISCOVERY));
    let wanted =
        |name: &str, here: bool| only.is_none_or(|only| only.contains(name) || (adopt && !here));
    let mut jobs = BTreeMap::new();
    for name in held_rows.into_iter().filter(|name| wanted(name, false)) {
        jobs.insert(name, Job::Held("agy custody row is held".into()));
    }
    for (name, found) in found {
        let key = found.key;
        let here = inflight::custody_written_here(key.destination.get(), key.id.get(), key.nonce);
        if wanted(&name, here) {
            jobs.insert(name, decide(root, found));
        }
    }
    if !failed.is_empty() {
        jobs.insert(DISCOVERY.into(), Job::Held(failed.join("; ")));
    }
    for name in only.into_iter().flatten() {
        jobs.entry(name.clone())
            .or_insert_with(|| match failed.is_empty() {
                true => Job::Done,
                false => Job::Held("agy custody discovery is incomplete".into()),
            });
    }
    jobs
}

fn entry(found: &mut BTreeMap<String, Found>, key: TurnKey) -> &mut Found {
    let name = format!("{}:{}:{}", key.destination, key.id, key.nonce);
    found.entry(name).or_insert(Found {
        key,
        route: None,
        saved: None,
    })
}

fn route_key(route: &AgyCustodyRoute) -> Option<TurnKey> {
    Some(TurnKey {
        destination: NonZeroU64::new(route.destination)?,
        id: NonZeroU64::new(route.message)?,
        nonce: route.nonce,
    })
}

fn decide(root: &Path, found: Found) -> Job {
    let Found { key, route, saved } = found;
    let binding = route.as_ref().map(|route| AgyBinding {
        logical_channel: route.logical_channel,
        original: route.original.clone(),
    });
    if let Some(saved) = saved {
        // An existing journal reopens with its saved revision, under the plan it recorded.
        let binding = binding.or(saved.binding);
        let mut plans: Vec<Plan> = [Abandon::NoSlot, Abandon::NotStarted, Abandon::ProducerLost]
            .map(|reason| Plan::abandoned(key, reason))
            .into();
        if let Some(binding) = &binding
            && let Ok(Spool::Read(read)) = read_spool(root, key, binding)
            && let Ok(Some(plan)) = sealed_plan(key, &read)
        {
            plans.push(plan);
        }
        return match plans
            .into_iter()
            .find(|plan| plan.fingerprint == saved.fingerprint)
        {
            Some(plan) => Job::Deliver(plan, None),
            None => Job::Held("agy journal plan cannot be rebuilt".into()),
        };
    }
    let Some(binding) = binding else {
        return Job::Done;
    };
    let plan = match disposition(root, key, &binding) {
        Ok(plan) => plan,
        Err(reason) => return Job::Held(reason),
    };
    let lease = TurnLease {
        key,
        revision: NonZeroU64::MIN,
    };
    Job::Deliver(
        Plan {
            binding: Some(binding),
            ..plan
        },
        Some(lease),
    )
}

/// What the spool proves about a route without a journal; `Err` holds it visibly.
fn disposition(root: &Path, key: TurnKey, binding: &AgyBinding) -> Result<Plan, String> {
    let read = match read_spool(root, key, binding)? {
        Spool::Missing => return Ok(Plan::abandoned(key, Abandon::NoSlot)),
        Spool::NoCheckpoint(dir) => {
            set_aside(&dir)?;
            return Ok(Plan::abandoned(key, Abandon::NotStarted));
        }
        Spool::Read(read) => read,
    };
    if let Some(plan) = sealed_plan(key, &read)? {
        return Ok(plan);
    }
    let AgyLaunch::Started(started) = &read.launch else {
        // No launch, or a launch the gate never let run: the input never ran.
        return Ok(Plan::abandoned(key, Abandon::NotStarted));
    };
    end_orphan(started)?;
    // The child may have sealed before it ended; that seal wins over a loss.
    match read_spool(root, key, binding)? {
        Spool::Read(read) => {
            Ok(sealed_plan(key, &read)?
                .unwrap_or_else(|| Plan::abandoned(key, Abandon::ProducerLost)))
        }
        _ => Err("agy spool changed while its producer ended".into()),
    }
}

/// No recorder survives a restart, so a started child never seals: its group is ended.
fn end_orphan(started: &AgyStarted) -> Result<(), String> {
    let parse = |value: &Option<String>| value.as_deref().and_then(|value| value.parse().ok());
    let identity = crate::services::process::ProcessIdentity::from_persisted(
        parse(&started.starttime),
        parse(&started.macos_lstart_hash),
    );
    crate::services::process::orphan_group::end_recorded_group(started.pid, started.pgid, identity)
}

enum Spool {
    Missing,
    NoCheckpoint(PathBuf),
    Read(AgyReplay),
}

fn read_spool(root: &Path, key: TurnKey, binding: &AgyBinding) -> Result<Spool, String> {
    let slot = match &binding.original {
        AgyOriginal::Discord { .. } => AgySlot::Discord(key.id.get()),
        AgyOriginal::Imessage { digest } => AgySlot::Imessage(digest.clone()),
    };
    let target = AgyTarget {
        destination: key.destination.get(),
        logical_channel: binding.logical_channel,
    };
    let missing = |error: &std::io::Error| error.kind() == std::io::ErrorKind::NotFound;
    let dir = match version_dir(root, target.destination) {
        Ok(version) => version.join(slot.dir_name()),
        Err(error) if missing(&error) => return Ok(Spool::Missing),
        Err(error) => return Err(error.to_string()),
    };
    for (path, absent) in [
        (&dir, Spool::Missing),
        (&dir.join(CHECKPOINT), Spool::NoCheckpoint(dir.clone())),
    ] {
        match std::fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(error) if missing(&error) => return Ok(absent),
            Err(error) => return Err(error.to_string()),
        }
    }
    replay(root, target, &slot, None)
        .map(Spool::Read)
        .map_err(|error| error.to_string())
}

/// A slot that never committed its first record is set aside, so it holds its destination no more.
fn set_aside(dir: &Path) -> Result<(), String> {
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("agy slot has no name")?;
    let aside = dir.with_file_name(format!(".not-started-{name}"));
    std::fs::rename(dir, &aside).map_err(|error| error.to_string())?;
    crate::domain::file_sync::fsync_parent_dir(&aside).map_err(|error| error.to_string())
}

fn sealed_plan(key: TurnKey, read: &AgyReplay) -> Result<Option<Plan>, String> {
    if read.key.id != key.id.get() || read.key.nonce != key.nonce {
        return Err("agy spool names another execution".into());
    }
    let terminal = match &read.outcome {
        AgyOutcome::Pending => return Ok(None),
        AgyOutcome::Failed(_) => Terminal::Failed,
        AgyOutcome::Sealed(seal) => Terminal::Done {
            conversation: seal.conversation_id,
            body: seal.body.clone(),
            sha256: hash(&seal.body),
            split_version: seal.split_version,
        },
    };
    let last_seq = read.frontier.last_seq;
    Plan::derive(Seal {
        key,
        last_seq,
        terminal,
    })
    .map(Some)
    .map_err(str::to_owned)
}
