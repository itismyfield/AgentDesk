//! Herdr launch preparation for one channel's canonical session row. No launch selects
//! Herdr yet. The order is fixed: Pending commit, `.host_kind` marker, then one create call.
#![cfg_attr(not(test), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sqlx::PgPool;

use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    ExpectedExecution, HostedCasOutcome, HostedExecution, HostedLocation, HostedLookup,
    HostedLookupKey, HostedObservation, HostedOwner, HostedRecord, HostedState, ProcessStamp,
    SourceRef, install_pending_pg, load_hosted_execution_pg, record_launch_evidence_pg,
    record_pane_location_pg,
};
use crate::services::claude_tui::hook_output_guard::configured_claude_projects_root;
pub(crate) use crate::services::session_host::RESTORE_RESUME_NOT_OFF;
use crate::services::session_host::{HostKind, RestoreResume};
use crate::services::tui_prompt_dedupe::binding_context::{
    BindingContext, PreparedIncarnation, stable_host_identity,
};

pub(crate) const ENDPOINT_MISSING: &str = "endpoint_missing";
pub(crate) const HERDR_NOT_ADMITTED: &str = "herdr launch is not admitted";
const HOST_KIND_TEMP_EXT: &str = "host_kind";
const EVIDENCE_PROVENANCE: &str = "herdr_launch";
/// What Herdr gives every pane process; a provider that sees them may report to Herdr.
pub(crate) const HERDR_PANE_ENV: [&str; 4] = [
    "HERDR_ENV",
    "HERDR_PANE_ID",
    "HERDR_BIN_PATH",
    "HERDR_SOCKET_PATH",
];

/// Whether a Claude TUI launch goes to Herdr, decided before any launch I/O. Nothing
/// admits Herdr yet, so every launch stays on tmux.
pub(crate) const fn herdr_admitted_for_claude_launch(_channel_id: Option<u64>) -> bool {
    false
}

/// A configured Herdr endpoint; no field falls back to a default socket, session or pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HerdrLaunchEndpoint {
    pub execution_node: String,
    pub config_key: String,
    pub socket_addr: String,
    pub herdr_session: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HerdrCreateRequest {
    pub endpoint: HerdrLaunchEndpoint,
    /// Name of the managed workspace/tab; the pane comes back in the reply.
    pub label: String,
    pub cwd: PathBuf,
    pub command: String,
    /// The connection that read resume-on-restore off just before; only it may carry the create.
    pub restore_off_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HerdrCreateOutcome {
    Created {
        pane_id: String,
    },
    /// Nothing reached the server.
    NotSent(String),
    /// The server may have acted without a usable reply: lost ACK, wrong id, remote error.
    Indeterminate(String),
}

/// Herdr calls of one launch. They run on a blocking thread with no DB transaction open.
pub(crate) trait HerdrLaunchHost: Send + Sync {
    /// A fresh read of the endpoint server's effective resume-on-restore; never cached.
    fn restore_resume(&self, endpoint: &HerdrLaunchEndpoint) -> RestoreResume;
    fn create(&self, request: &HerdrCreateRequest) -> HerdrCreateOutcome;
    /// Root shell and provider process of the new pane, when both can be read.
    fn launch_evidence(&self, location: &HostedLocation) -> Option<(ProcessStamp, ProcessStamp)>;
}

pub(crate) struct HerdrLaunch {
    pub endpoint: Option<HerdrLaunchEndpoint>,
    pub owner: HostedOwner,
    pub channel_id: Option<u64>,
    pub expected_native_session_id: Option<String>,
    pub resume: bool,
}

pub(crate) struct HerdrLaunchCommand {
    pub cwd: PathBuf,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HerdrLaunchOutcome {
    /// The pane location is stored; `evidence` tells whether its process stamps are too.
    Launched {
        execution_nonce: String,
        location: HostedLocation,
        evidence: bool,
    },
    /// The pane may exist unrecorded. Pending stays; nothing is resent, adopted or killed.
    Indeterminate {
        execution_nonce: String,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HerdrLaunchError {
    /// Refused before any I/O.
    Unsupported(&'static str),
    /// The canonical row is missing, unreadable or conflicting.
    Row(String),
    /// The row holds a Pending, Bound or unreadable record (`None`).
    Occupied(Option<HostedState>),
    Prepare(String),
    /// Pending did not commit; nothing was created.
    Pending(String),
    /// Pending committed without its marker; nothing was created.
    Marker(String),
    /// The create request never left; Pending stays for reconcile.
    NotSent(String),
}

#[cfg(test)]
thread_local! {
    static ADMISSIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn admissions_on_this_thread() -> usize {
    ADMISSIONS.with(std::cell::Cell::get)
}

/// First step of every Herdr launch; it reads nothing and refuses an incomplete endpoint.
fn admit(endpoint: Option<&HerdrLaunchEndpoint>) -> Result<&HerdrLaunchEndpoint, HerdrLaunchError> {
    #[cfg(test)]
    ADMISSIONS.with(|count| count.set(count.get() + 1));
    let filled = |values: [&str; 3]| values.iter().all(|value| !value.trim().is_empty());
    endpoint
        .filter(|e| filled([&e.execution_node, &e.config_key, &e.herdr_session]))
        .filter(|e| Path::new(&e.socket_addr).is_absolute())
        .ok_or(HerdrLaunchError::Unsupported(ENDPOINT_MISSING))
}

/// E7: a server that resumes agents on restore could relaunch behind the stored execution.
async fn restore_off_generation(
    host: &Arc<dyn HerdrLaunchHost>,
    endpoint: &HerdrLaunchEndpoint,
) -> Option<u64> {
    let endpoint = endpoint.clone();
    on_blocking_thread(host, move |host| host.restore_resume(&endpoint))
        .await?
        .admitted_generation()
}

/// Removes the Herdr pane variables on the line before the script's provider `exec`, after
/// every export; the first `exec` line, since its arguments may hold any text.
pub(crate) fn unset_herdr_env_before_exec(script: &str) -> Result<String, String> {
    let exec = script
        .find("\nexec ")
        .ok_or("launch script has no provider exec")?
        + 1;
    let unset = format!("unset {}\n", HERDR_PANE_ENV.join(" "));
    Ok(format!("{}{unset}{}", &script[..exec], &script[exec..]))
}

/// Prepares and starts one Herdr execution for `launch.owner`'s canonical row.
pub(crate) async fn launch_herdr_session(
    pool: &PgPool,
    launch: HerdrLaunch,
    prepare: impl FnOnce(&PreparedIncarnation) -> Result<HerdrLaunchCommand, String>,
    host: Arc<dyn HerdrLaunchHost>,
) -> Result<HerdrLaunchOutcome, HerdrLaunchError> {
    let endpoint = admit(launch.endpoint.as_ref())?.clone();
    if restore_off_generation(&host, &endpoint).await.is_none() {
        return Err(HerdrLaunchError::Unsupported(RESTORE_RESUME_NOT_OFF));
    }
    let owner = launch.owner;
    let observed = load_row(pool, &owner).await?;
    match &observed.record {
        HostedRecord::Legacy => {}
        HostedRecord::Known(record) if record.state == HostedState::Retired => {}
        HostedRecord::Known(record) => return Err(HerdrLaunchError::Occupied(Some(record.state))),
        HostedRecord::Unknown(_) => return Err(HerdrLaunchError::Occupied(None)),
    }
    let expected_native = launch.expected_native_session_id.as_deref();
    let incarnation = herdr_incarnation(&owner, launch.channel_id, expected_native, launch.resume)
        .map_err(HerdrLaunchError::Prepare)?;
    let command = prepare(&incarnation).map_err(HerdrLaunchError::Prepare)?;
    let nonce = incarnation.context.execution_nonce.clone();
    let pending =
        HostedExecution::pending(owner.clone(), nonce.clone(), source_ref(&owner, &nonce));
    match install_pending_pg(pool, &observed, pending).await {
        Ok(HostedCasOutcome::Written) => {}
        Ok(HostedCasOutcome::Stale) => {
            return Err(HerdrLaunchError::Pending(
                "row changed since it was read".into(),
            ));
        }
        Err(error) => return Err(HerdrLaunchError::Pending(format!("{error:?}"))),
    }
    record_herdr_host_marker(&owner.logical_key).map_err(HerdrLaunchError::Marker)?;

    // Read again right before create: the first reading may predate a reconnect or reload.
    let Some(restore_off_generation) = restore_off_generation(&host, &endpoint).await else {
        return Err(HerdrLaunchError::NotSent(RESTORE_RESUME_NOT_OFF.into()));
    };
    let request = HerdrCreateRequest {
        endpoint: endpoint.clone(),
        label: owner.logical_key.clone(),
        cwd: command.cwd,
        command: command.command,
        restore_off_generation,
    };
    let created = on_blocking_thread(&host, move |host| host.create(&request)).await;
    let indeterminate = |detail: String| HerdrLaunchOutcome::Indeterminate {
        execution_nonce: nonce.clone(),
        detail,
    };
    let pane_id = match created {
        Some(HerdrCreateOutcome::Created { pane_id }) if !pane_id.trim().is_empty() => pane_id,
        Some(HerdrCreateOutcome::Created { .. }) => {
            return Ok(indeterminate("create reply named no pane".into()));
        }
        Some(HerdrCreateOutcome::NotSent(detail)) => return Err(HerdrLaunchError::NotSent(detail)),
        Some(HerdrCreateOutcome::Indeterminate(detail)) => return Ok(indeterminate(detail)),
        None => return Ok(indeterminate("create call did not return".into())),
    };
    let location = HostedLocation {
        host: HostKind::Herdr.as_str().to_string(),
        execution_node: endpoint.execution_node,
        endpoint_config_key: endpoint.config_key,
        socket_addr: endpoint.socket_addr,
        named_session: endpoint.herdr_session,
        pane_id,
    };
    // The row is read again after the socket call: only this nonce's Pending takes the pane.
    let recorded = match load_row(pool, &owner).await {
        Ok(current) => {
            record_pane_location_pg(pool, &current, &owner, &nonce, location.clone()).await
        }
        Err(error) => return Ok(indeterminate(format!("{error:?}"))),
    };
    if recorded != Ok(HostedCasOutcome::Written) {
        return Ok(indeterminate(format!(
            "pane {} not recorded: {recorded:?}",
            location.pane_id
        )));
    }
    let evidence = record_evidence(pool, &owner, &nonce, &location, &host).await;
    Ok(HerdrLaunchOutcome::Launched {
        execution_nonce: nonce,
        location,
        evidence,
    })
}

/// Stores the first root/provider stamps; a stored value that differs is left in place.
async fn record_evidence(
    pool: &PgPool,
    owner: &HostedOwner,
    nonce: &str,
    location: &HostedLocation,
    host: &Arc<dyn HerdrLaunchHost>,
) -> bool {
    let probed = location.clone();
    let evidence = on_blocking_thread(host, move |host| host.launch_evidence(&probed)).await;
    let (Some(Some((root, provider_process))), Ok(current)) =
        (evidence, load_row(pool, owner).await)
    else {
        return false;
    };
    let expected = ExpectedExecution {
        binding_provider: owner.provider.clone(),
        binding_nonce: nonce.to_string(),
        root,
        provider_process,
        provenance: EVIDENCE_PROVENANCE.to_string(),
    };
    let written =
        record_launch_evidence_pg(pool, &current, owner, nonce, location.clone(), expected).await;
    written == Ok(HostedCasOutcome::Written)
}

/// `None` when the call panicked: the request may already have reached the server.
async fn on_blocking_thread<R: Send + 'static>(
    host: &Arc<dyn HerdrLaunchHost>,
    call: impl FnOnce(&dyn HerdrLaunchHost) -> R + Send + 'static,
) -> Option<R> {
    let host = Arc::clone(host);
    tokio::task::spawn_blocking(move || call(host.as_ref()))
        .await
        .ok()
}

async fn load_row(
    pool: &PgPool,
    owner: &HostedOwner,
) -> Result<HostedObservation, HerdrLaunchError> {
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: &owner.discord_token_hash,
        channel_id: &owner.channel_id,
    };
    let key = HostedLookupKey::Canonical {
        provider: &owner.provider,
        identity,
    };
    match load_hosted_execution_pg(pool, key).await {
        HostedLookup::Found(observed) => Ok(observed),
        other => Err(HerdrLaunchError::Row(format!("{other:?}"))),
    }
}

/// Immutable launch evidence; unlike `PreparedIncarnation::prepare` it runs no tmux-probing sweep.
fn herdr_incarnation(
    owner: &HostedOwner,
    channel_id: Option<u64>,
    expected_native_session_id: Option<&str>,
    resume: bool,
) -> Result<PreparedIncarnation, String> {
    let context = BindingContext {
        schema: 1,
        provider: owner.provider.clone(),
        created_at: chrono::Utc::now(),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: owner.logical_key.clone(),
        channel_id,
        owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
        host: stable_host_identity(),
        expected_native_session_id: expected_native_session_id.map(str::to_owned),
        launch_mode: if resume { "resume" } else { "fresh" }.to_owned(),
        provider_root: (owner.provider == "claude")
            .then(configured_claude_projects_root)
            .flatten(),
    };
    PreparedIncarnation::create(context).map_err(|error| format!("create binding context: {error}"))
}

fn source_ref(owner: &HostedOwner, nonce: &str) -> SourceRef {
    SourceRef {
        runtime_root: owner.runtime_root.clone(),
        channel: owner.channel_id.clone(),
        provider: owner.provider.clone(),
        logical_key: owner.logical_key.clone(),
        execution_nonce: nonce.to_string(),
        initial_source: None,
        baseline_event_seq: None,
    }
}

/// Written where session cleanup reads `.host_kind`, and only after Pending committed.
fn record_herdr_host_marker(logical_key: &str) -> Result<(), String> {
    let path = crate::services::tmux_common::session_temp_path(logical_key, HOST_KIND_TEMP_EXT);
    std::fs::write(&path, HostKind::Herdr.as_str()).map_err(|error| format!("{path}: {error}"))
}

#[cfg(test)]
#[path = "herdr_launch_tests.rs"]
mod tests;
