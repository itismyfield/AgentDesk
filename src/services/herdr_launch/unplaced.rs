//! A Pending whose create outcome never came back, resolved under the channel's transition lease:
//! close unrecorded executions, remove the launch's untouched shell, adopt its provider or retire.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use sqlx::PgPool;

use super::{
    EvidenceProbe, HerdrLaunchEndpoint, HerdrLaunchHost, load_row, on_blocking_thread,
    pane_location, retire_own_unplaced, source_ref,
};
use crate::db::dispatched_sessions::hosted_execution::{
    ExpectedExecution, HostedCasOutcome, HostedExecution, HostedLocation, HostedOwner,
    HostedRecord, HostedState, ProcessStamp, record_launch_evidence_pg,
};
use crate::services::discord::UnplacedLease;
use crate::services::session_host::{PaneOccupant, START_SLACK, ServerWitness};
use crate::services::tui_prompt_dedupe::binding_context::{
    BindingContext, context_of_env_path, execution_context,
};

/// How long after a launch's last client work, or this process's start, before it is judged.
pub(crate) const UNPLACED_QUIET: Duration = Duration::from_secs(60);
const ENDED_KEPT: Duration = Duration::from_secs(3_600);
/// Reads after one round of orphan closes and one residue removal; neither is ever repeated.
const PASSES: usize = 3;

/// Each launch this process started, and when its last client work ended (`None` while running).
static IN_FLIGHT: LazyLock<Mutex<HashMap<String, Option<Instant>>>> = LazyLock::new(Mutex::default);
static PROCESS_SEEN: LazyLock<Instant> = LazyLock::new(Instant::now);

fn in_flight() -> MutexGuard<'static, HashMap<String, Option<Instant>>> {
    IN_FLIGHT.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One launch's client work; it ends when the launch and every worker it started drop their clone.
pub(crate) struct LaunchInFlight(String);

impl LaunchInFlight {
    pub(crate) fn begin(nonce: &str) -> Arc<Self> {
        LazyLock::force(&PROCESS_SEEN);
        let mut launches = in_flight();
        launches.retain(|_, ended| ended.is_none_or(|at| at.elapsed() < ENDED_KEPT));
        launches.insert(nonce.to_owned(), None);
        Arc::new(Self(nonce.to_owned()))
    }

    pub(crate) fn nonce(&self) -> &str {
        &self.0
    }
}

impl Drop for LaunchInFlight {
    fn drop(&mut self) {
        in_flight().insert(self.0.clone(), Some(Instant::now()));
    }
}

/// Refuses a launch still running here, or one whose last work or context is too recent.
fn quiet_since(nonce: &str, created: SystemTime, quiet: Duration) -> Result<(), String> {
    let since = match in_flight().get(nonce) {
        Some(None) => return Err("launch in flight".into()),
        Some(Some(ended)) => *ended,
        None => *PROCESS_SEEN,
    };
    let created_age = created.elapsed().unwrap_or(Duration::ZERO);
    if since.elapsed() < quiet || created_age < quiet {
        return Err("launch too recent".into());
    }
    Ok(())
}

/// One pane of a workspace whose label names the channel, and what runs in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChannelPane {
    pub pane_id: String,
    pub label: String,
    /// Its workspace holds this pane alone in one tab.
    pub pristine: bool,
    pub occupant: PaneOccupant,
}

/// The shell a lost create left, read on `witness`'s server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Residue {
    pub pane_id: String,
    pub label: String,
    pub root: ProcessStamp,
    pub witness: ServerWitness,
}

enum Label {
    /// The bare logical key a launch before the marker used.
    Bare,
    Marked(String),
}

fn label_of(logical_key: &str, label: &str) -> Option<Label> {
    if label == logical_key {
        return Some(Label::Bare);
    }
    let nonce = label.strip_prefix(logical_key)?.strip_prefix('#')?;
    let hex = |c: char| c.is_ascii_digit() || ('a'..='f').contains(&c);
    (nonce.len() == 32 && nonce.chars().all(hex)).then(|| Label::Marked(nonce.to_owned()))
}

/// Whether a workspace label is one a launch of `logical_key` gives; no other pane is read.
pub(crate) fn candidate_label(logical_key: &str, label: &str) -> bool {
    label_of(logical_key, label).is_some()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    Held(String),
    /// Unrecorded executions of this channel, by nonce and pane.
    Orphans(Vec<(String, String)>),
    Dispose {
        pane_id: String,
        label: String,
        root: ProcessStamp,
    },
    Adopt {
        pane_id: String,
    },
    Retire,
}

/// What to do about Pending `nonce` from every pane its channel's labels hold; nothing is read.
pub(crate) fn judge(
    panes: &[ChannelPane],
    owner: &HostedOwner,
    nonce: &str,
    created: SystemTime,
    context_of: &dyn Fn(&Path) -> Result<Option<BindingContext>, String>,
) -> Plan {
    let held = |pane: &ChannelPane, why: &str| Plan::Held(format!("pane {}: {why}", pane.pane_id));
    let (mut own, mut orphans, mut residues) = (Vec::new(), Vec::new(), Vec::new());
    for pane in panes {
        let Some(label) = label_of(&owner.logical_key, &pane.label) else {
            continue;
        };
        match &pane.occupant {
            PaneOccupant::Unreadable(why) => return held(pane, why),
            PaneOccupant::Provider { context, .. } => {
                let ctx = match context_of(context) {
                    Ok(Some(ctx)) => ctx,
                    Ok(None) => continue,
                    Err(why) => return held(pane, &why),
                };
                let channel = ctx.channel_id.map(|channel| channel.to_string());
                if ctx.provider != owner.provider
                    || channel.as_deref() != Some(owner.channel_id.as_str())
                    || ctx.tmux_session != owner.logical_key
                {
                    continue;
                }
                // Only N may run under the bare label; any other provider must run under its own marker.
                match label {
                    Label::Marked(marked) if marked != ctx.execution_nonce => {
                        return held(pane, "the provider runs under another launch's label");
                    }
                    _ if ctx.execution_nonce == nonce => own.push(pane),
                    Label::Marked(_) => orphans.push((ctx.execution_nonce, pane.pane_id.clone())),
                    Label::Bare => {
                        return held(pane, "an unrecorded provider under the bare label");
                    }
                }
            }
            PaneOccupant::LoneShell { root, root_started } => match label {
                Label::Marked(marked) if marked == nonce && pane.pristine => {
                    residues.push((pane, root));
                }
                Label::Marked(marked) if marked == nonce => return held(pane, "residue_touched"),
                Label::Marked(_) => {}
                // A shell that started before the context did is no one's launch.
                Label::Bare if *root_started + START_SLACK < created => {}
                Label::Bare => return held(pane, "legacy_unattributed"),
            },
        }
    }
    if own.len() > 1 || residues.len() > 1 || (!own.is_empty() && !residues.is_empty()) {
        return Plan::Held("several panes claim the launch".into());
    }
    if !orphans.is_empty() {
        return Plan::Orphans(orphans);
    }
    if let Some((pane, root)) = residues.pop() {
        return Plan::Dispose {
            pane_id: pane.pane_id.clone(),
            label: pane.label.clone(),
            root: root.clone(),
        };
    }
    match own.pop() {
        Some(pane) => Plan::Adopt {
            pane_id: pane.pane_id.clone(),
        },
        None => Plan::Retire,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolution {
    /// The row now records the launch's pane and provider.
    Adopted,
    /// The row is Retired, by this resolution or an earlier one; a fresh launch may follow.
    Retired,
    Held(String),
}

/// One unplaced Pending and where its launch went.
pub(crate) struct Unplaced<'a> {
    pub pool: &'a PgPool,
    pub owner: &'a HostedOwner,
    pub nonce: &'a str,
    pub endpoint: &'a HerdrLaunchEndpoint,
    pub host: Arc<dyn HerdrLaunchHost>,
    pub quiet: Duration,
}

/// Resolves the Pending while `lease` holds its channel; every write is a CAS on what was read.
pub(crate) async fn resolve_unplaced(lease: &UnplacedLease, target: &Unplaced<'_>) -> Resolution {
    let resolution = if lease.covers(target.owner) {
        resolve(target).await
    } else {
        Resolution::Held("the lease is another channel's".into())
    };
    tracing::info!(
        event = "herdr_unplaced_pending",
        channel = %target.owner.channel_id,
        nonce = target.nonce,
        ?resolution,
    );
    resolution
}

async fn resolve(target: &Unplaced<'_>) -> Resolution {
    let (owner, nonce) = (target.owner, target.nonce);
    // Callers read only this node's rows and endpoints; the row must name both and this runtime.
    let root = crate::config::runtime_root().map(|root| root.display().to_string());
    if target.endpoint.execution_node != owner.owner_node
        || root.as_deref() != Some(owner.runtime_root.as_str())
    {
        return Resolution::Held("another node's or runtime's launch".into());
    }
    let mut closed = false;
    for _ in 0..PASSES {
        match unplaced(target).await {
            Ok(true) => {}
            Ok(false) => return Resolution::Retired,
            Err(why) => return Resolution::Held(why),
        }
        let created: SystemTime = match execution_context(&owner.provider, nonce) {
            Ok(ctx) => ctx.created_at.into(),
            Err(why) => return Resolution::Held(why),
        };
        if let Err(why) = quiet_since(nonce, created, target.quiet) {
            return Resolution::Held(why);
        }
        let (endpoint, key) = (target.endpoint.clone(), owner.logical_key.clone());
        let owned = nonce.to_owned();
        let read = on_blocking_thread(&target.host, move |host| {
            let witness = host.unplaced_witness(&endpoint, &owned)?;
            let panes = host.channel_panes(&endpoint, &key, &witness)?;
            Ok::<_, String>((witness, panes))
        });
        let (witness, panes) = match read.await {
            Some(Ok(read)) => read,
            Some(Err(why)) => return Resolution::Held(why),
            None => return Resolution::Held("pane read panicked".into()),
        };
        match judge(&panes, owner, nonce, created, &context_of_env_path) {
            Plan::Held(why) => return Resolution::Held(why),
            Plan::Orphans(orphans) if closed => {
                return Resolution::Held(format!(
                    "unrecorded executions outlived a close {orphans:?}"
                ));
            }
            Plan::Orphans(orphans) => {
                if let Err(why) = close_orphans(target, &witness, orphans).await {
                    return Resolution::Held(why);
                }
                closed = true;
            }
            Plan::Dispose {
                pane_id,
                label,
                root,
            } => {
                let residue = Residue {
                    pane_id,
                    label,
                    root,
                    witness,
                };
                if let Err(why) = dispose(target, residue).await {
                    return Resolution::Held(why);
                }
            }
            Plan::Adopt { pane_id } => return adopt(target, witness, pane_id, created).await,
            Plan::Retire => return retire(target).await,
        }
    }
    Resolution::Held("the channel's panes kept changing".into())
}

/// A turn's resolution of its row's unplaced Pending: `Adopted` or `Retired`, the lease released
/// before the caller launches anew; Held refuses the turn.
pub(crate) fn resolve_for_turn(
    runtime: &tokio::runtime::Handle,
    lease: Result<UnplacedLease, String>,
    target: Unplaced<'_>,
) -> Result<Resolution, String> {
    let held = |why: &str| {
        format!(
            "herdr turn: pending execution {} has no recorded pane and is held: {why}",
            target.nonce
        )
    };
    let lease = lease.map_err(|why| held(&why))?;
    match runtime.block_on(resolve_unplaced(&lease, &target)) {
        Resolution::Held(why) => Err(held(&why)),
        resolved => Ok(resolved),
    }
}

/// The quiet a turn or pass waits out; a test on this thread may shorten it.
pub(crate) fn quiet() -> Duration {
    #[cfg(test)]
    if let Some(quiet) = QUIET.with(std::cell::Cell::get) {
        return quiet;
    }
    UNPLACED_QUIET
}

#[cfg(test)]
thread_local! {
    pub(crate) static QUIET: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

/// `Ok(true)` while the row is still `nonce`'s Pending with no pane; `Ok(false)` once it is retired.
async fn unplaced(target: &Unplaced<'_>) -> Result<bool, String> {
    let current = load_row(target.pool, target.owner)
        .await
        .map_err(|error| format!("{error:?}"))?;
    match &current.record {
        HostedRecord::Known(record) if record.execution_nonce == target.nonce => {
            match record.state {
                HostedState::Retired => Ok(false),
                HostedState::Pending if record.location.is_none() && record.expected.is_none() => {
                    Ok(true)
                }
                _ => Err("the row changed".into()),
            }
        }
        _ => Err("the row changed".into()),
    }
}

async fn dispose(target: &Unplaced<'_>, residue: Residue) -> Result<(), String> {
    if !unplaced(target).await? {
        return Err("the row changed".into());
    }
    let (endpoint, nonce) = (target.endpoint.clone(), target.nonce.to_owned());
    on_blocking_thread(&target.host, move |host| {
        host.dispose_residue(&endpoint, &nonce, &residue)
    })
    .await
    .unwrap_or_else(|| Err("residue removal panicked".into()))
}

/// Where execution `nonce` runs and its provider, proven by the launch's own evidence rule.
async fn probe(
    target: &Unplaced<'_>,
    witness: ServerWitness,
    pane_id: String,
    nonce: &str,
    created: SystemTime,
) -> Result<(HostedLocation, ExpectedExecution), String> {
    let location = pane_location(target.endpoint, pane_id);
    let probe = EvidenceProbe {
        location: location.clone(),
        provider: target.owner.provider.clone(),
        execution_nonce: nonce.to_owned(),
        launched_at: created,
        witness,
    };
    match on_blocking_thread(&target.host, move |host| host.launch_evidence(&probe)).await {
        Some(Ok(expected)) => Ok((location, expected)),
        other => Err(format!("provider of {nonce} not proven: {other:?}")),
    }
}

/// Closes each orphan's pane through the terminate service while the row still holds; any
/// refusal, disabled switch or unconfirmed end holds.
async fn close_orphans(
    target: &Unplaced<'_>,
    witness: &ServerWitness,
    orphans: Vec<(String, String)>,
) -> Result<(), String> {
    for (nonce, pane_id) in orphans {
        let created = execution_context(&target.owner.provider, &nonce)?.created_at;
        let (location, expected) =
            probe(target, witness.clone(), pane_id, &nonce, created.into()).await?;
        let orphan = HostedExecution {
            location: Some(location),
            expected: Some(expected),
            ..HostedExecution::pending(
                target.owner.clone(),
                nonce.clone(),
                source_ref(target.owner, &nonce),
            )
        };
        let holds = async { unplaced(target).await == Ok(true) };
        close_unrecorded(&orphan, holds)
            .await
            .map_err(|why| format!("orphan {nonce} not closed: {why}"))?;
    }
    Ok(())
}

#[cfg(unix)]
async fn close_unrecorded(
    orphan: &HostedExecution,
    holds: impl std::future::Future<Output = bool>,
) -> Result<(), String> {
    let closed = crate::services::discord::herdr_terminate::close_unrecorded(orphan, holds).await;
    closed.map_err(|result| format!("{result:?}"))
}

/// The terminate service is Unix only; elsewhere an orphan is never closed and the row holds.
#[cfg(not(unix))]
async fn close_unrecorded(
    _: &HostedExecution,
    _: impl std::future::Future<Output = bool>,
) -> Result<(), String> {
    Err("unsupported on this platform".into())
}

/// Records the pane and its probed provider in one CAS; a failed probe writes nothing.
async fn adopt(
    target: &Unplaced<'_>,
    witness: ServerWitness,
    pane_id: String,
    created: SystemTime,
) -> Resolution {
    let (location, expected) = match probe(target, witness, pane_id, target.nonce, created).await {
        Ok(found) => found,
        Err(why) => return Resolution::Held(why),
    };
    let (pool, owner, nonce) = (target.pool, target.owner, target.nonce);
    let recorded = match load_row(pool, owner).await {
        Ok(current) => {
            record_launch_evidence_pg(pool, &current, owner, nonce, location, expected).await
        }
        Err(error) => return Resolution::Held(format!("{error:?}")),
    };
    match recorded {
        Ok(HostedCasOutcome::Written) => Resolution::Adopted,
        other => Resolution::Held(format!("adoption not recorded: {other:?}")),
    }
}

async fn retire(target: &Unplaced<'_>) -> Resolution {
    let retired = retire_own_unplaced(target.pool, target.owner, target.nonce).await;
    if retired == Ok(HostedCasOutcome::Written) {
        return Resolution::Retired;
    }
    match unplaced(target).await {
        Ok(false) => Resolution::Retired,
        _ => Resolution::Held(format!("retire not recorded: {retired:?}")),
    }
}

#[cfg(test)]
#[path = "unplaced_tests.rs"]
mod tests;
