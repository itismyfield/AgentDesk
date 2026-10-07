//! Herdr launch preparation for one channel's canonical session row. No launch selects
//! Herdr yet. The order is fixed: Pending commit, `.host_kind` and nonce markers, then one create.
//! A Pending pane left without evidence is only probed again, never recreated.
#![cfg_attr(not(test), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use sqlx::PgPool;

use crate::config::runtime_root;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    ExpectedExecution, HostedCasOutcome, HostedExecution, HostedLocation, HostedLookup,
    HostedLookupKey, HostedObservation, HostedOwner, HostedRecord, HostedState, SourceRef,
    install_pending_pg, load_hosted_execution_pg, record_launch_evidence_pg,
    record_pane_location_pg,
};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude_tui::hook_output_guard::configured_claude_projects_root;
pub(crate) use crate::services::session_host::RESTORE_RESUME_NOT_OFF;
use crate::services::session_host::{EvidenceGap, HostKind, RestoreResume, ServerWitness};
use crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel;
use crate::services::tui_o::store::OStore;
use crate::services::tui_prompt_dedupe::binding_context::{
    BindingContext, PreparedIncarnation, execution_context, stable_host_identity,
};

pub(crate) const ENDPOINT_MISSING: &str = "endpoint_missing";
pub(crate) const HERDR_NOT_ADMITTED: &str = "herdr launch is not admitted";
const HOST_KIND_TEMP_EXT: &str = "host_kind";
/// What Herdr 0.9.3 gives every pane process; a provider that sees them may report to Herdr.
pub(crate) const HERDR_PANE_ENV: [&str; 7] = [
    "HERDR_ENV",
    "HERDR_PANE_ID",
    "HERDR_BIN_PATH",
    "HERDR_SOCKET_PATH",
    "HERDR_TAB_ID",
    "HERDR_WORKSPACE_ID",
    "HERDR_CONFIG_PATH",
];

/// Whether a TUI launch for the channel belongs to Herdr. A configured channel never gets a tmux
/// session, whatever its admission or O readiness; the turn host reports why it is refused.
pub(crate) fn herdr_configured_for_tui_launch(channel_id: Option<u64>) -> bool {
    channel_id.is_some_and(configured)
}

/// Read from the boot `session_hosts` section, never from the live config.
fn configured(channel: u64) -> bool {
    crate::config::session_hosts::herdr_endpoint(channel).is_some()
}

/// Whether the channel's O writer could take a Herdr turn of `kind`'s provider now.
pub(crate) fn o_writer_ready(channel: u64, kind: RuntimeHandoffKind) -> bool {
    o_ready_at(runtime_root().as_deref(), channel, kind)
}

/// O owns the channel's output, its writer accepts work, and its store holds a binding
/// checkpoint, which only a found baseline or an applied binding writes; the store is not opened.
fn o_ready_at(runtime_root: Option<&Path>, channel: u64, kind: RuntimeHandoffKind) -> bool {
    #[cfg(test)]
    READINESS_READS.with(|reads| reads.set(reads.get() + 1));
    let owned = peek_o_owns_tui_output_for_channel(channel, Some(kind));
    let seeded = |root| {
        OStore::existing(root)
            .is_some_and(|store| matches!(store.peek_binding_checkpoint(channel), Ok(Some(_))))
    };
    owned == Ok(true) && writer_accepts(channel) && runtime_root.is_some_and(seeded)
}

fn writer_accepts(channel: u64) -> bool {
    #[cfg(test)]
    if let Some(answer) = WRITER_ACCEPTS.with(std::cell::Cell::get) {
        return answer;
    }
    crate::services::tui_o::writer::host::channel_accepts(channel)
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
    /// The server that read resume-on-restore off just before; only it may get the create.
    pub restore_off_witness: ServerWitness,
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
    /// The pane exists but its command was not confirmed to start (cwd mismatch, E7 lost,
    /// unclear input reply). The pane is recorded; nothing is resent or killed.
    CreatedUnconfirmed {
        pane_id: String,
        detail: String,
    },
}

/// What launch evidence must tie together: the pane, the execution, and the server the
/// launch was sent to, after which the provider started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EvidenceProbe {
    pub location: HostedLocation,
    pub provider: String,
    pub execution_nonce: String,
    pub launched_at: SystemTime,
    pub witness: ServerWitness,
}

/// Herdr calls of one launch. They run on a blocking thread with no DB transaction open.
pub(crate) trait HerdrLaunchHost: Send + Sync {
    /// A fresh read of the endpoint server's effective resume-on-restore; never cached.
    fn restore_resume(&self, endpoint: &HerdrLaunchEndpoint) -> RestoreResume;
    fn create(&self, request: &HerdrCreateRequest) -> HerdrCreateOutcome;
    /// Root shell and provider of the pane as launch evidence, from reads only, or why not.
    fn launch_evidence(&self, probe: &EvidenceProbe) -> Result<ExpectedExecution, EvidenceGap>;
}

pub(crate) struct HerdrLaunch {
    pub endpoint: Option<HerdrLaunchEndpoint>,
    pub owner: HostedOwner,
    pub channel_id: Option<u64>,
    pub expected_native_session_id: Option<String>,
    pub resume: bool,
    /// Where the provider's hooks may name a transcript; a Claude launch reads its own setting.
    pub provider_root: Option<PathBuf>,
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
        evidence: Result<(), EvidenceGap>,
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
    /// The prepared cwd or command cannot run in a pane; refused before Pending.
    Ineligible(String),
    /// The canonical row is missing, unreadable or conflicting.
    Row(String),
    /// The row holds a Pending, Bound or unreadable record (`None`).
    Occupied(Option<HostedState>),
    Prepare(String),
    /// Pending did not commit; nothing was created.
    Pending(String),
    /// Pending committed without its markers; nothing was created.
    Marker(String),
    /// The create request never left; Pending stays for reconcile.
    NotSent(String),
    /// The Pending pane still has no launch evidence; nothing was created, typed or removed.
    Evidence(EvidenceGap),
}

#[cfg(test)]
thread_local! {
    static ADMISSIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static WRITER_ACCEPTS: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static READINESS_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Stands in for the writer's readiness on this thread until dropped.
#[cfg(test)]
pub(crate) struct WriterAcceptsGuard(Option<bool>);

#[cfg(test)]
pub(crate) fn force_writer_accepts(writer_accepts: Option<bool>) -> WriterAcceptsGuard {
    WriterAcceptsGuard(WRITER_ACCEPTS.with(|cell| cell.replace(writer_accepts)))
}

#[cfg(test)]
impl Drop for WriterAcceptsGuard {
    fn drop(&mut self) {
        WRITER_ACCEPTS.with(|cell| cell.set(self.0));
    }
}

#[cfg(test)]
pub(crate) fn readiness_reads_on_this_thread() -> usize {
    READINESS_READS.with(std::cell::Cell::get)
}

/// An O store sealed over `channels` with no checkpoint yet, as a writer's first start leaves it.
#[cfg(test)]
pub(crate) fn o_store_for_test(
    runtime_root: &Path,
    channels: &[u64],
) -> (OStore, crate::services::tui_o::store::OEra) {
    use crate::services::tui_o::store::{Initialized, StoreConfig};
    let config = StoreConfig { enabled: true };
    let store = OStore::open_if_enabled(&config, runtime_root)
        .unwrap()
        .unwrap();
    let init = |channel| {
        Ok(Initialized {
            channel,
            sources: Vec::new(),
            initial_anchor: 1,
            build_digest: "test".into(),
            at: chrono::Utc::now(),
        })
    };
    let era = store.begin_era(channels, chrono::Utc::now(), init).unwrap();
    (store, era)
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
async fn restore_off_witness(
    host: &Arc<dyn HerdrLaunchHost>,
    endpoint: &HerdrLaunchEndpoint,
) -> Option<ServerWitness> {
    let endpoint = endpoint.clone();
    on_blocking_thread(host, move |host| host.restore_resume(&endpoint))
        .await?
        .admitted_witness()
}

/// An existing absolute cwd and one line of command, since the command is typed into the
/// pane followed by Enter. Herdr opens a missing cwd in HOME without an error.
pub(crate) fn launch_command_eligible(cwd: &Path, command: &str) -> Result<(), String> {
    if !cwd.is_absolute() || cwd.to_str().is_none() {
        return Err(format!("cwd {} is not an absolute path", cwd.display()));
    }
    if !cwd.is_dir() {
        return Err(format!("cwd {} is not a directory", cwd.display()));
    }
    if command.trim().is_empty() || command.chars().any(char::is_control) {
        return Err("command is not one line of text".to_string());
    }
    Ok(())
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
    let Some(admitted) = restore_off_witness(&host, &endpoint).await else {
        return Err(HerdrLaunchError::Unsupported(RESTORE_RESUME_NOT_OFF));
    };
    let owner = launch.owner;
    let observed = load_row(pool, &owner).await?;
    if let HostedRecord::Known(record) = &observed.record
        && let Some(location) = awaiting_evidence(record, &endpoint)
    {
        return probe_again(pool, &owner, record, location, admitted, &host).await;
    }
    match &observed.record {
        HostedRecord::Legacy => {}
        HostedRecord::Known(record) if record.state == HostedState::Retired => {}
        HostedRecord::Known(record) => return Err(HerdrLaunchError::Occupied(Some(record.state))),
        HostedRecord::Unknown(_) => return Err(HerdrLaunchError::Occupied(None)),
    }
    let expected_native = launch.expected_native_session_id.as_deref();
    let incarnation = herdr_incarnation(
        &owner,
        launch.channel_id,
        expected_native,
        launch.resume,
        launch.provider_root,
    )
    .map_err(HerdrLaunchError::Prepare)?;
    let command = prepare(&incarnation).map_err(HerdrLaunchError::Prepare)?;
    launch_command_eligible(&command.cwd, &command.command)
        .map_err(HerdrLaunchError::Ineligible)?;
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
    // The new execution is held until a reconcile admits it; an earlier admission ends here.
    crate::services::tui_prompt_dedupe::install_herdr_execution(&owner.logical_key, &nonce);
    record_herdr_host_marker(&owner.logical_key).map_err(HerdrLaunchError::Marker)?;
    // Hooks and binding events name an execution by its spawn nonce marker, as on tmux.
    #[cfg(unix)]
    crate::services::discord::stamp_spawn_markers(&owner.logical_key, Some(&incarnation))
        .map_err(|error| HerdrLaunchError::Marker(error.to_string()))?;

    // Read again right before create: the first reading may predate a reconnect or reload.
    let Some(restore_off_witness) = restore_off_witness(&host, &endpoint).await else {
        return Err(HerdrLaunchError::NotSent(RESTORE_RESUME_NOT_OFF.into()));
    };
    let witness = restore_off_witness.clone();
    let request = HerdrCreateRequest {
        endpoint: endpoint.clone(),
        label: owner.logical_key.clone(),
        cwd: command.cwd,
        command: command.command,
        restore_off_witness,
    };
    let sent_at = SystemTime::now();
    let created = on_blocking_thread(&host, move |host| host.create(&request)).await;
    let indeterminate = |detail: String| HerdrLaunchOutcome::Indeterminate {
        execution_nonce: nonce.clone(),
        detail,
    };
    let (pane_id, unconfirmed) = match created {
        Some(HerdrCreateOutcome::Created { pane_id }) if !pane_id.trim().is_empty() => {
            (pane_id, None)
        }
        Some(HerdrCreateOutcome::CreatedUnconfirmed { pane_id, detail })
            if !pane_id.trim().is_empty() =>
        {
            (pane_id, Some(detail))
        }
        Some(
            HerdrCreateOutcome::Created { .. } | HerdrCreateOutcome::CreatedUnconfirmed { .. },
        ) => {
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
    // The pane is recorded where reconcile finds it; its unconfirmed command is never retried.
    if let Some(detail) = unconfirmed {
        return Ok(indeterminate(format!(
            "pane {}: {detail}",
            location.pane_id
        )));
    }
    let probe = EvidenceProbe {
        location: location.clone(),
        provider: owner.provider.clone(),
        execution_nonce: nonce.clone(),
        launched_at: sent_at,
        witness,
    };
    let evidence = record_evidence(pool, &owner, probe, &host).await;
    Ok(HerdrLaunchOutcome::Launched {
        execution_nonce: nonce,
        location,
        evidence,
    })
}

/// The pane of a Pending that has one on this endpoint but no evidence: its provider was
/// not confirmed when it was launched.
fn awaiting_evidence<'a>(
    record: &'a HostedExecution,
    endpoint: &HerdrLaunchEndpoint,
) -> Option<&'a HostedLocation> {
    let location = record.location.as_ref()?;
    let here = (&location.execution_node, &location.endpoint_config_key)
        == (&endpoint.execution_node, &endpoint.config_key)
        && (&location.socket_addr, &location.named_session)
            == (&endpoint.socket_addr, &endpoint.herdr_session);
    (record.state == HostedState::Pending && record.expected.is_none() && here).then_some(location)
}

/// Probes the recorded pane again under the server E7 admitted now. No process that started
/// before the execution's binding context existed can carry its nonce.
async fn probe_again(
    pool: &PgPool,
    owner: &HostedOwner,
    record: &HostedExecution,
    location: &HostedLocation,
    witness: ServerWitness,
    host: &Arc<dyn HerdrLaunchHost>,
) -> Result<HerdrLaunchOutcome, HerdrLaunchError> {
    let nonce = record.execution_nonce.clone();
    let context = execution_context(&owner.provider, &nonce)
        .map_err(|detail| HerdrLaunchError::Evidence(EvidenceGap::OtherNonce(detail)))?;
    let probe = EvidenceProbe {
        location: location.clone(),
        provider: owner.provider.clone(),
        execution_nonce: nonce.clone(),
        launched_at: context.created_at.into(),
        witness,
    };
    record_evidence(pool, owner, probe, host)
        .await
        .map_err(HerdrLaunchError::Evidence)?;
    Ok(HerdrLaunchOutcome::Launched {
        execution_nonce: nonce,
        location: location.clone(),
        evidence: Ok(()),
    })
}

/// Stores the probed evidence on this nonce's Pending; a stored value that differs is left
/// in place.
async fn record_evidence(
    pool: &PgPool,
    owner: &HostedOwner,
    probe: EvidenceProbe,
    host: &Arc<dyn HerdrLaunchHost>,
) -> Result<(), EvidenceGap> {
    let (nonce, location) = (probe.execution_nonce.clone(), probe.location.clone());
    let probed = on_blocking_thread(host, move |host| host.launch_evidence(&probe)).await;
    let expected =
        probed.unwrap_or_else(|| Err(EvidenceGap::ReadFailed("probe panicked".into())))?;
    let not_recorded = |detail: String| EvidenceGap::NotRecorded(detail);
    let current = load_row(pool, owner)
        .await
        .map_err(|error| not_recorded(format!("{error:?}")))?;
    match record_launch_evidence_pg(pool, &current, owner, &nonce, location, expected).await {
        Ok(HostedCasOutcome::Written) => Ok(()),
        other => Err(not_recorded(format!("{other:?}"))),
    }
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
    provider_root: Option<PathBuf>,
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
        provider_root: match owner.provider.as_str() {
            "claude" => configured_claude_projects_root(),
            _ => provider_root,
        },
        first_prompt_digest: None,
        source_policy: None,
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
