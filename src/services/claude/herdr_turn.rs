//! A Claude turn on a Herdr pane behind the default-off `runtime.herdr_turn_enabled` switch: attach
//! after SessionStart, one gated prompt, and no resend, relaunch or kill after a refused send.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use tokio::runtime::Handle;
use tokio::sync::broadcast::{self, error::RecvError};

use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    ExpectedExecution, HostedCasOutcome, HostedExecution, HostedLookup, HostedLookupKey,
    HostedOwner, HostedRecord, HostedState, bind_pg, load_hosted_execution_pg,
};
use crate::services::agent_protocol::{RuntimeHandoff, RuntimeHandoffKind, StreamMessage};
use crate::services::claude_tui::hook_server::{HookEvent, HookEventKind};
use crate::services::claude_tui::host_input::run_herdr;
use crate::services::claude_tui::input::{PromptReadinessSnapshot, TuiInputAction};
use crate::services::claude_tui::transcript_tail::claude_transcript_path;
use crate::services::herdr_launch::{
    EvidenceProbe, HerdrCreateOutcome, HerdrCreateRequest, HerdrLaunch, HerdrLaunchCommand,
    HerdrLaunchEndpoint, HerdrLaunchHost, HerdrLaunchOutcome, launch_herdr_session,
};
use crate::services::provider::{CancelToken, ProviderKind, cancel_requested};
use crate::services::session_backend::read_output_file_until_result_with_harvest;
use crate::services::session_host::{
    EvidenceGap, HerdrTarget, RestoreResume, SocketHerdrLaunchHost, herdr_endpoints,
};
use crate::services::tui_prompt_dedupe::TuiRuntimeBinding;
use crate::services::tui_prompt_dedupe::binding_context::{PreparedIncarnation, execution_context};
use crate::services::tui_prompt_dedupe::binding_events::{BindingTarget, binding_events_since};

const SESSION_START_WAIT: Duration = Duration::from_secs(30);

/// The boot registry's launch host, on its endpoints only.
pub(crate) fn boot_launch_host() -> Option<Arc<dyn HerdrLaunchHost>> {
    struct Boot(&'static SocketHerdrLaunchHost);
    impl HerdrLaunchHost for Boot {
        fn restore_resume(&self, endpoint: &HerdrLaunchEndpoint) -> RestoreResume {
            self.0.restore_resume(endpoint)
        }
        fn create(&self, request: &HerdrCreateRequest) -> HerdrCreateOutcome {
            self.0.create(request)
        }
        fn launch_evidence(&self, probe: &EvidenceProbe) -> Result<ExpectedExecution, EvidenceGap> {
            self.0.launch_evidence(probe)
        }
    }
    let host = herdr_endpoints().launch_host()?;
    Some(Arc::new(Boot(host)))
}

/// One turn; `row` is the record the turn host read for the channel's sessions row.
pub(crate) struct HerdrTurn<'a> {
    pub pool: &'a PgPool,
    pub owner: HostedOwner,
    pub channel_id: u64,
    pub endpoint: HerdrLaunchEndpoint,
    pub row: Option<&'a HostedRecord>,
    pub prompt: &'a str,
    pub working_dir: &'a str,
    pub system_prompt: Option<&'a str>,
    pub model: Option<&'a str>,
    /// The hook receiver a launch reports to; only a launch needs it.
    pub hook_endpoint: Option<String>,
    pub cancel: Option<Arc<CancelToken>>,
}

/// The stored execution whose launched session the turn asks to attach, and its pane gate.
pub(crate) struct AttachRequest<'a> {
    pub owner: &'a HostedOwner,
    pub record: &'a HostedExecution,
    pub session_id: &'a str,
    pub transcript: &'a Path,
    pub target: &'a HerdrTarget,
}

/// What a turn takes from its caller: launch host, hook events and the source reconcile.
pub(crate) trait HerdrTurnPorts {
    fn launch_host(&self) -> Option<Arc<dyn HerdrLaunchHost>>;
    fn hook_events(&self) -> broadcast::Receiver<HookEvent>;
    /// `Ok(bound)` once the launched source is published; `Err` attached nothing.
    fn attach(&self, request: &AttachRequest<'_>) -> Result<bool, String>;
    /// `Ok` only while the reconcile reads the Bound execution as a match.
    fn confirm_bound(
        &self,
        owner: &HostedOwner,
        record: &HostedExecution,
        target: &HerdrTarget,
    ) -> Result<(), String>;
}

/// The source a prompt goes to.
struct Attached {
    nonce: String,
    session_id: String,
    transcript: PathBuf,
    target: HerdrTarget,
    bound: bool,
}

/// Runs one turn on a blocking thread inside the runtime; every error leaves the pane as it is.
pub(crate) fn execute(
    turn: HerdrTurn<'_>,
    ports: &dyn HerdrTurnPorts,
    sender: Sender<StreamMessage>,
) -> Result<(), String> {
    let runtime = Handle::try_current().map_err(|error| format!("herdr turn: {error}"))?;
    let attached = match turn.row {
        Some(HostedRecord::Known(record)) if record.state == HostedState::Bound => {
            bound_source(&turn, ports, record)?
        }
        Some(HostedRecord::Known(record)) if record.state == HostedState::Pending => {
            pending_source(&turn, &runtime, ports, record)?
        }
        Some(HostedRecord::Unknown(_)) => return Err("herdr turn: unreadable hosted record".into()),
        _ => launched_source(&turn, &runtime, ports)?,
    };
    prompt_and_read(&turn, &runtime, attached, sender)
}

/// A Bound execution keeps the source attached when it launched; a restart reattach is not here.
fn bound_source(
    turn: &HerdrTurn<'_>,
    ports: &dyn HerdrTurnPorts,
    record: &HostedExecution,
) -> Result<Attached, String> {
    let target = gate(record)?;
    ports.confirm_bound(&turn.owner, record, &target)?;
    let logical = &turn.owner.logical_key;
    let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(logical)
        .ok_or_else(|| format!("herdr turn: {logical} has no attached source"))?;
    let session_id = binding
        .session_id
        .ok_or_else(|| format!("herdr turn: {logical} source names no session"))?;
    Ok(Attached {
        nonce: record.execution_nonce.clone(),
        session_id,
        transcript: PathBuf::from(binding.output_path),
        target,
        bound: true,
    })
}

/// A Pending execution from an earlier turn: its pane is probed again when it has no evidence,
/// then attached under the session its launch named. Its SessionStart has already passed.
fn pending_source(
    turn: &HerdrTurn<'_>,
    runtime: &Handle,
    ports: &dyn HerdrTurnPorts,
    record: &HostedExecution,
) -> Result<Attached, String> {
    if record.expected.is_none() {
        let host = ports.launch_host().ok_or("herdr turn: no launch host")?;
        let never = |_: &PreparedIncarnation| -> Result<HerdrLaunchCommand, String> {
            Err("a pending pane is never created again".into())
        };
        launched(runtime.block_on(launch_herdr_session(
            turn.pool,
            launch(turn, None),
            never,
            host,
        )))?;
    }
    let nonce = &record.execution_nonce;
    let session_id = execution_context("claude", nonce)?
        .expected_native_session_id
        .ok_or("herdr turn: the pending launch named no session")?;
    attach(turn, runtime, ports, nonce, &session_id)
}

/// A new execution: create, evidence, SessionStart, then attach; no prompt before the attach.
fn launched_source(
    turn: &HerdrTurn<'_>,
    runtime: &Handle,
    ports: &dyn HerdrTurnPorts,
) -> Result<Attached, String> {
    let host = ports.launch_host().ok_or("herdr turn: no launch host")?;
    let working_dir = Path::new(turn.working_dir);
    let fresh = super::fresh_claude_tui_session_resolution(working_dir, None)?;
    let hook_endpoint = turn
        .hook_endpoint
        .clone()
        .ok_or("herdr turn: no hook endpoint")?;
    let logical = &turn.owner.logical_key;
    let overlay = crate::services::discord::org_schema::overlay_from_tmux_session(
        ProviderKind::Claude,
        logical,
    )?;
    let auth = crate::services::provider_auth_profile::overlay_shell_env_lines(&overlay);
    let prepare = |prepared: &PreparedIncarnation| {
        let (system, model) = (turn.system_prompt, turn.model);
        let (session, hooks) = (fresh.session_id.as_str(), hook_endpoint.clone());
        super::tui_session_launch::prepare_claude_herdr_launch(
            prepared,
            working_dir,
            session,
            system,
            model,
            hooks,
            &auth,
        )
    };
    // Subscribed before the create, so the launch's SessionStart cannot pass unseen.
    let mut hooks = ports.hook_events();
    let launch = launch(turn, Some(fresh.session_id.clone()));
    let outcome = runtime.block_on(launch_herdr_session(turn.pool, launch, prepare, host));
    let nonce = launched(outcome)?;
    session_started(
        runtime,
        &mut hooks,
        &fresh.session_id,
        turn.cancel.as_deref(),
    )?;
    attach(turn, runtime, ports, &nonce, &fresh.session_id)
}

fn launch(turn: &HerdrTurn<'_>, session_id: Option<String>) -> HerdrLaunch {
    HerdrLaunch {
        endpoint: Some(turn.endpoint.clone()),
        owner: turn.owner.clone(),
        channel_id: Some(turn.channel_id),
        expected_native_session_id: session_id,
        resume: false,
    }
}

/// The nonce of a launch whose pane is recorded with its evidence; anything else stops the turn.
fn launched(
    outcome: Result<HerdrLaunchOutcome, crate::services::herdr_launch::HerdrLaunchError>,
) -> Result<String, String> {
    match outcome {
        Ok(HerdrLaunchOutcome::Launched {
            execution_nonce,
            evidence: Ok(()),
            ..
        }) => Ok(execution_nonce),
        other => Err(format!("herdr launch did not complete: {other:?}")),
    }
}

/// Waits for the launched session's SessionStart: its provider and hooks are up.
fn session_started(
    runtime: &Handle,
    hooks: &mut broadcast::Receiver<HookEvent>,
    session_id: &str,
    cancel: Option<&CancelToken>,
) -> Result<(), String> {
    let deadline = Instant::now() + SESSION_START_WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if cancel_requested(cancel) || left.is_zero() {
            return Err(format!("herdr turn: no SessionStart of {session_id}"));
        }
        let poll = left.min(Duration::from_millis(200));
        match runtime.block_on(tokio::time::timeout(poll, hooks.recv())) {
            Ok(Ok(event))
                if event.provider == "claude"
                    && event.kind == HookEventKind::SessionStart
                    && event.session_id == session_id =>
            {
                return Ok(());
            }
            Ok(Err(RecvError::Closed)) => return Err("herdr turn: hook events closed".into()),
            _ => {}
        }
    }
}

/// The gated pane of a stored execution on this node's registered endpoint.
fn gate(record: &HostedExecution) -> Result<HerdrTarget, String> {
    herdr_endpoints()
        .target(record)
        .ok_or_else(|| "herdr turn: the execution is not on a registered endpoint".into())
}

async fn load(pool: &PgPool, owner: &HostedOwner) -> HostedLookup {
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: &owner.discord_token_hash,
        channel_id: &owner.channel_id,
    };
    let provider = &owner.provider;
    load_hosted_execution_pg(pool, HostedLookupKey::Canonical { provider, identity }).await
}

/// Attaches `nonce`'s session; a refused attach sends nothing.
fn attach(
    turn: &HerdrTurn<'_>,
    runtime: &Handle,
    ports: &dyn HerdrTurnPorts,
    nonce: &str,
    session_id: &str,
) -> Result<Attached, String> {
    let record = match runtime.block_on(load(turn.pool, &turn.owner)) {
        HostedLookup::Found(found) => match found.record {
            HostedRecord::Known(record) if record.execution_nonce == nonce => record,
            other => return Err(format!("herdr turn: row moved on: {other:?}")),
        },
        other => return Err(format!("herdr turn: row unreadable: {other:?}")),
    };
    let target = gate(&record)?;
    let transcript = claude_transcript_path(Path::new(turn.working_dir), session_id, None)?;
    let request = AttachRequest {
        owner: &turn.owner,
        record: &record,
        session_id,
        transcript: &transcript,
        target: &target,
    };
    let bound = ports.attach(&request)?;
    Ok(Attached {
        nonce: nonce.to_owned(),
        session_id: session_id.to_owned(),
        transcript,
        target,
        bound,
    })
}

/// One gated paste and Enter, then the transcript until the turn ends, then the watcher handoff.
/// Before its first prompt a cold start has no transcript; the prompt makes it.
fn prompt_and_read(
    turn: &HerdrTurn<'_>,
    runtime: &Handle,
    attached: Attached,
    sender: Sender<StreamMessage>,
) -> Result<(), String> {
    let path = attached.transcript.as_path();
    let transcript = path.display().to_string();
    let length = || std::fs::metadata(path).map_or(0, |meta| meta.len());
    let session_id = attached.session_id.clone();
    let _ = sender.send(StreamMessage::Init {
        session_id: session_id.clone(),
        raw_session_id: Some(session_id),
    });
    let (before, started_at) = (length(), chrono::Utc::now());
    let plan = [
        TuiInputAction::PasteBuffer(turn.prompt.to_owned()),
        TuiInputAction::Enter,
    ];
    let cancel = turn.cancel.as_deref();
    run_herdr(&attached.target, &plan, cancel).into_legacy()?;
    let start = super::claude_tui_turn_start_offset_after_timestamp(path, started_at, before);
    let logical = turn.owner.logical_key.as_str();
    let unread = || PromptReadinessSnapshot {
        prompt_marker_detected: false,
        prompt_draft_detected: false,
        tmux_pane_alive: false,
        capture_available: false,
        pane_tail: String::new(),
    };
    let early = super::wait_for_claude_tui_transcript_file_inner(
        &transcript,
        start,
        cancel,
        logical,
        super::CLAUDE_TUI_TRANSCRIPT_INITIAL_WAIT_TIMEOUT,
        || path.exists(),
        || true,
        unread,
    )?;
    if early.is_none() {
        let kind = Some(RuntimeHandoffKind::ClaudeTui);
        let probe =
            super::host_gate::host_poll_probe(None, ProviderKind::Claude, kind, &transcript);
        let cancel = turn.cancel.clone();
        read_output_file_until_result_with_harvest(
            &transcript,
            start,
            sender.clone(),
            cancel,
            probe,
        )
        .map_err(|failure| failure.error)?;
    }
    // The tmux handoff's launch registration; with the transcript now written it resolves a cold
    // start's Pending source.
    let binding = TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: transcript.clone(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(attached.session_id.clone()),
        last_offset: length(),
        relay_last_offset: None,
    };
    crate::services::tui_prompt_dedupe::register_launched_tmux_runtime_binding(logical, binding);
    if !attached.bound {
        runtime.block_on(bind_once_logged(turn, &attached.nonce));
    }
    let _ = sender.send(StreamMessage::RuntimeReady {
        handoff: RuntimeHandoff::ClaudeTui {
            transcript_path: transcript,
            tmux_session_name: logical.to_owned(),
            last_offset: length(),
        },
    });
    Ok(())
}

/// The row turns Bound once the pane's latest logged source is this execution's; until then it
/// stays Pending and the next turn attaches it again.
async fn bind_once_logged(turn: &HerdrTurn<'_>, nonce: &str) {
    let logical = turn.owner.logical_key.as_str();
    let events = binding_events_since(turn.channel_id, 0).unwrap_or_default();
    let latest = events
        .iter()
        .rev()
        .filter(|event| event.tmux_session == logical)
        .find(|event| !matches!(event.new, BindingTarget::Rejected { .. }));
    let logged = latest.is_some_and(|event| {
        event.execution_nonce.as_deref() == Some(nonce)
            && matches!(
                event.new,
                BindingTarget::Source(_) | BindingTarget::Resolved { .. }
            )
    });
    if !logged {
        return;
    }
    if let HostedLookup::Found(observed) = load(turn.pool, &turn.owner).await {
        let bound = bind_pg(turn.pool, &observed, &turn.owner, nonce).await;
        if bound != Ok(HostedCasOutcome::Written) {
            tracing::warn!(logical, ?bound, "herdr turn: row not bound");
        }
    }
}
