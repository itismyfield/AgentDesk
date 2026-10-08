//! A Claude turn on a Herdr pane behind the default-off `runtime.herdr_turn_enabled` switch: one
//! gated prompt after SessionStart, never resent or relaunched; an unclear send holds later ones.

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
use crate::services::agent_protocol::{
    NativeTerminalKind, RuntimeHandoff, RuntimeHandoffKind, StreamMessage,
};
use crate::services::claude_tui::hook_server::{HookEvent, HookEventKind};
use crate::services::claude_tui::host_input::{InputRun, run_herdr};
use crate::services::claude_tui::input::{PromptReadinessSnapshot, TuiInputAction};
use crate::services::claude_tui::transcript_tail::claude_transcript_path;
use crate::services::herdr_launch::{
    EvidenceProbe, HerdrCreateOutcome, HerdrCreateRequest, HerdrLaunch, HerdrLaunchCommand,
    HerdrLaunchEndpoint, HerdrLaunchHost, HerdrLaunchOutcome, launch_herdr_session,
};
use crate::services::provider::cancel_token_claude_interrupt::herdr_stop_settlement_available;
use crate::services::provider::{
    CancelToken, ProviderKind, ReadOutputResult, cancel_requested, herdr_provider_terminal_only,
    poll_output_file_until_result,
};
use crate::services::session_backend::{
    StreamLineState, process_stream_line, read_output_file_until_result_with_harvest,
};
use crate::services::session_host::{
    EvidenceGap, HerdrTarget, RestoreResume, SocketHerdrLaunchHost, herdr_endpoints,
};
use crate::services::tui_prompt_dedupe::TuiRuntimeBinding;
use crate::services::tui_prompt_dedupe::binding_context::{PreparedIncarnation, execution_context};
use crate::services::tui_prompt_dedupe::binding_events::{
    BindingCause, BindingEvent, BindingTarget, binding_events_since, claude_history,
};

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
    // The turn takes its stop state before launch, so a stop is recorded whatever the switch says.
    if herdr_stop_settlement_available()
        && let Some(token) = turn.cancel.as_deref()
    {
        token.prepare_herdr_interrupt(ProviderKind::Claude, &turn.owner);
    }
    let attached = match turn.row {
        Some(HostedRecord::Known(record)) if record.state == HostedState::Bound => {
            not_held(&record.execution_nonce)?;
            bound_source(&turn, ports, record)?
        }
        Some(HostedRecord::Known(record)) if record.state == HostedState::Pending => {
            not_held(&record.execution_nonce)?;
            pending_source(&turn, &runtime, ports, record)?
        }
        Some(HostedRecord::Unknown(_)) => return Err("herdr turn: unreadable hosted record".into()),
        _ => launched_source(&turn, &runtime, ports)?,
    };
    prompt_and_read(&turn, &runtime, attached, sender)
}

/// A Bound execution keeps the source attached when it launched, or takes the session its own
/// clear awaits; a restart reattach is not here.
fn bound_source(
    turn: &HerdrTurn<'_>,
    ports: &dyn HerdrTurnPorts,
    record: &HostedExecution,
) -> Result<Attached, String> {
    let target = gate(record)?;
    ports.confirm_bound(&turn.owner, record, &target)?;
    let logical = &turn.owner.logical_key;
    if let Some(awaited) = awaited_clear(turn.channel_id, logical, &record.execution_nonce) {
        return cleared_source(turn, record, target, awaited);
    }
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

/// The clear Pending a Bound execution's next prompt goes to.
pub(crate) struct AwaitedClear {
    pub session_id: String,
    /// The transcript path the Pending's hook named.
    pub transcript: Option<String>,
}

/// Execution `nonce`'s latest own SessionStart(clear) Pending, when every record after the source
/// it logged last is such a Pending taken from that source and the canonical fold awaits it.
pub(crate) fn awaited_clear(channel: u64, logical: &str, nonce: &str) -> Option<AwaitedClear> {
    let events = binding_events_since(channel, 0).unwrap_or_default();
    let moved = |event: &BindingEvent| !matches!(event.new, BindingTarget::Rejected { .. });
    let pane = events
        .into_iter()
        .filter(|event| event.tmux_session == logical);
    let records: Vec<BindingEvent> = pane.filter(moved).collect();
    let own = |event: &BindingEvent| event.execution_nonce.as_deref() == Some(nonce);
    let clear = |event: &&BindingEvent| {
        own(event)
            && matches!(event.new, BindingTarget::Pending { .. })
            && event.cause == BindingCause::Clear
            && event.evidence.hook_event.as_deref() == Some("session_start")
    };
    let waiting = records.iter().rev().take_while(clear).count();
    let (earlier, chain) = records.split_at(records.len() - waiting);
    let from = earlier.last().filter(|event| own(event));
    let from = from.and_then(|event| match &event.new {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => Some(source),
        _ => None,
    })?;
    if chain
        .iter()
        .any(|pending| pending.old.as_ref() != Some(from))
    {
        return None;
    }
    let BindingTarget::Pending {
        payload_session_id,
        payload_transcript_path,
    } = &chain.last()?.new
    else {
        return None;
    };
    // The helper commits a clear only on this same fold's awaited session.
    let (_, history) = claude_history(channel, logical, Some(nonce)).ok()?;
    let awaited = history.awaiting.filter(|_| history.complete)?;
    (awaited.session == *payload_session_id).then(|| AwaitedClear {
        session_id: payload_session_id.clone(),
        transcript: payload_transcript_path.clone(),
    })
}

/// The cleared session of a Bound execution: nothing is attached until its first prompt writes
/// the transcript, whose registration then resolves the Pending.
fn cleared_source(
    turn: &HerdrTurn<'_>,
    record: &HostedExecution,
    target: HerdrTarget,
    awaited: AwaitedClear,
) -> Result<Attached, String> {
    let session_id = awaited.session_id;
    let transcript = claude_transcript_path(Path::new(turn.working_dir), &session_id, None)?;
    if awaited.transcript.as_deref().map(Path::new) != Some(transcript.as_path()) {
        return Err(format!(
            "herdr turn: the clear Pending of {session_id} names another transcript than {}",
            transcript.display()
        ));
    }
    let logical = &turn.owner.logical_key;
    crate::services::tui_prompt_dedupe::register_tmux_channel(logical, turn.channel_id);
    Ok(Attached {
        nonce: record.execution_nonce.clone(),
        session_id,
        transcript,
        target,
        bound: true,
    })
}

/// A Pending execution from an earlier turn, only once the log holds its own record for the
/// session its launch named: only an attach after SessionStart, or the pane's hooks, write one.
fn pending_source(
    turn: &HerdrTurn<'_>,
    runtime: &Handle,
    ports: &dyn HerdrTurnPorts,
    record: &HostedExecution,
) -> Result<Attached, String> {
    let nonce = &record.execution_nonce;
    let session_id = execution_context("claude", nonce)?
        .expected_native_session_id
        .ok_or("herdr turn: the pending launch named no session")?;
    let started = latest_logged(turn).is_some_and(|event| {
        event.execution_nonce.as_deref() == Some(nonce.as_str())
            && logged_session(&event.new) == Some(session_id.as_str())
    });
    if !started {
        return Err(format!("herdr turn: no logged start of {session_id}"));
    }
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
    attach(turn, runtime, ports, nonce, &session_id)
}

/// The pane's latest logged record that moved it; refusal audits do not.
fn latest_logged(turn: &HerdrTurn<'_>) -> Option<BindingEvent> {
    let logical = turn.owner.logical_key.as_str();
    let events = binding_events_since(turn.channel_id, 0).ok()?;
    events
        .into_iter()
        .rev()
        .filter(|event| event.tmux_session == logical)
        .find(|event| !matches!(event.new, BindingTarget::Rejected { .. }))
}

fn logged_session(target: &BindingTarget) -> Option<&str> {
    match target {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
            Some(&source.session_id)
        }
        BindingTarget::Pending {
            payload_session_id, ..
        } => Some(payload_session_id),
        BindingTarget::Rejected { .. } => None,
    }
}

/// Where a prompt that may sit unsubmitted in execution `nonce`'s composer is recorded.
fn hold_path(nonce: &str) -> Result<PathBuf, String> {
    let plain = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    if nonce.is_empty() || !nonce.chars().all(plain) {
        return Err(format!("herdr turn: unusable execution nonce {nonce:?}"));
    }
    Ok(holds_dir()?.join(nonce))
}

fn holds_dir() -> Result<PathBuf, String> {
    let root = crate::config::runtime_root().ok_or("herdr turn: no runtime root")?;
    Ok(root.join("runtime/herdr_input_holds"))
}

/// What releasing an input hold left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HoldRelease {
    /// Removed and synced, or there was none.
    Released,
    /// Removed, but the directory sync failed, so the removal may not survive a crash.
    NotDurable(String),
    /// The hold is still there.
    Kept(String),
}

/// Ends execution `nonce`'s hold; an absent hold is already ended. A confirmed retire calls it,
/// as does a Codex cold start once its launch's own source is bound.
pub(crate) fn release_hold(nonce: &str) -> HoldRelease {
    let path = match hold_path(nonce) {
        Ok(path) => path,
        Err(error) => return HoldRelease::Kept(error),
    };
    match std::fs::remove_file(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => HoldRelease::Released,
        Err(error) => HoldRelease::Kept(error.to_string()),
        Ok(()) => match crate::services::discord::runtime_store::fsync_parent_dir(&path) {
            Ok(()) => HoldRelease::Released,
            Err(error) => HoldRelease::NotDurable(error.to_string()),
        },
    }
}

/// Each held nonce and when its hold was recorded; the file holds only that time.
pub(crate) fn input_holds() -> Result<Vec<(String, Option<String>)>, String> {
    let dir = holds_dir()?;
    let entries = match std::fs::read_dir(&dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        entries => {
            entries.map_err(|error| format!("herdr turn: input holds unreadable: {error}"))?
        }
    };
    let mut holds = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("herdr turn: input holds unreadable: {error}"))?;
        let recorded = std::fs::read_to_string(entry.path()).ok().and_then(|text| {
            chrono::DateTime::parse_from_rfc3339(text.trim())
                .ok()
                .map(|at| at.to_rfc3339())
        });
        holds.push((entry.file_name().to_string_lossy().into_owned(), recorded));
    }
    holds.sort();
    Ok(holds)
}

/// Whether an earlier prompt may sit in execution `nonce`'s composer; `Err` when unreadable.
pub(crate) fn input_held(nonce: &str) -> Result<bool, String> {
    match std::fs::symlink_metadata(hold_path(nonce)?) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Ok(_) => Ok(true),
        Err(error) => Err(format!(
            "herdr turn: input hold of {nonce} unreadable: {error}"
        )),
    }
}

/// Refuses the turn while an earlier prompt may sit in the composer; nothing here clears it.
pub(crate) fn not_held(nonce: &str) -> Result<(), String> {
    match input_held(nonce)? {
        false => Ok(()),
        true => Err(format!(
            "herdr turn: input held after an unclear prompt to {nonce}"
        )),
    }
}

/// Recorded and synced before the first write, so even a crash mid-send leaves it.
pub(crate) fn hold(nonce: &str) -> Result<PathBuf, String> {
    use std::io::Write;
    let path = hold_path(nonce)?;
    let written = std::fs::create_dir_all(path.parent().unwrap_or(&path))
        .and_then(|()| std::fs::File::create(&path))
        .and_then(|mut file| {
            file.write_all(chrono::Utc::now().to_rfc3339().as_bytes())?;
            file.sync_all()
        })
        .and_then(|()| crate::services::discord::runtime_store::fsync_parent_dir(&path));
    written.map_err(|error| format!("herdr turn: input hold not recorded: {error}"))?;
    Ok(path)
}

/// Whether the composer is as before the run: submitted, or never written.
fn composer_settled(run: &InputRun) -> bool {
    matches!(
        run,
        InputRun::Applied | InputRun::Refused(_) | InputRun::Cancelled { confirmed: 0 }
    )
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
        provider_root: None,
    }
}

/// The nonce of a launch whose pane is recorded with its evidence; anything else stops the turn.
pub(crate) fn launched(
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
pub(crate) fn gate(record: &HostedExecution) -> Result<HerdrTarget, String> {
    herdr_endpoints()
        .target(record)
        .ok_or_else(|| "herdr turn: the execution is not on a registered endpoint".into())
}

pub(crate) async fn load(pool: &PgPool, owner: &HostedOwner) -> HostedLookup {
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
    let held = hold(&attached.nonce)?;
    let run = if herdr_stop_settlement_available()
        && let Some(state) = cancel.and_then(CancelToken::herdr_interrupt_state)
    {
        use crate::services::provider::cancel_token_claude_interrupt::{
            HerdrSubmission, HerdrTurnStart,
        };
        let mut submitted = state.submission.lock().unwrap_or_else(|e| e.into_inner());
        let mut start = HerdrTurnStart::at_end_of(&attached.nonce, path, Some(started_at));
        start.offset = before;
        if !state.record_turn_start(start) {
            return Err("herdr turn: the token already began another turn".into());
        }
        let run = run_herdr(&attached.target, &plan, cancel);
        *submitted = match &run {
            InputRun::Applied => HerdrSubmission::Submitted,
            InputRun::Indeterminate {
                confirmed: 1,
                cause: crate::services::claude_tui::host_input::StopCause::Send(_),
            } => HerdrSubmission::Unknown,
            _ => HerdrSubmission::Unsubmitted,
        };
        run
    } else {
        run_herdr(&attached.target, &plan, cancel)
    };
    if composer_settled(&run)
        && let Err(error) = std::fs::remove_file(&held)
    {
        tracing::warn!(logical = %turn.owner.logical_key, %error, "herdr turn: input hold kept");
    }
    run.into_legacy()?;
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
    let (mut terminal, mut unconfirmed) = (None, false);
    if early.is_none() {
        let kind = Some(RuntimeHandoffKind::ClaudeTui);
        let probe =
            super::host_gate::host_poll_probe(None, ProviderKind::Claude, kind, &transcript);
        let cancel = turn.cancel.clone();
        if let Some(token) = cancel.as_ref()
            && herdr_provider_terminal_only(Some(token)).is_some()
        {
            let read =
                read_to_provider_terminal(&transcript, start, &sender, token, probe.is_alive);
            terminal = read?.map(|terminal| (terminal, token.clone(), start));
            unconfirmed = terminal.is_none();
        } else {
            read_output_file_until_result_with_harvest(
                &transcript,
                start,
                sender.clone(),
                cancel,
                probe,
            )
            .map_err(|failure| failure.error)?;
        }
    }
    // A Herdr turn hands off exactly where its terminal record ends, never past it.
    let terminal_end = terminal.as_ref().map(|(read, ..)| read.end);
    let handoff_end = || terminal_end.unwrap_or_else(length);
    // The tmux handoff's launch registration; with the transcript now written it resolves a cold
    // start's Pending source, before any terminal of this turn reaches admission.
    let binding = TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: transcript.clone(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(attached.session_id.clone()),
        last_offset: handoff_end(),
        relay_last_offset: None,
    };
    crate::services::tui_prompt_dedupe::register_launched_tmux_runtime_binding(logical, binding);
    if !attached.bound {
        runtime.block_on(bind_once_logged(turn, &attached.nonce));
    }
    if let Some((read, token, start)) = terminal.take() {
        let _ = sender.send(read.frame(&transcript, logical, start, &token));
    }
    // A read that ended without this turn's terminal hands no runtime to a watcher to finish.
    if unconfirmed {
        return Ok(());
    }
    let _ = sender.send(StreamMessage::RuntimeReady {
        handoff: RuntimeHandoff::ClaudeTui {
            transcript_path: transcript,
            tmux_session_name: logical.to_owned(),
            last_offset: handoff_end(),
        },
    });
    Ok(())
}

/// A Herdr Claude turn's provider terminal and the record end the read stopped at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeTurnTerminal {
    pub(crate) kind: NativeTerminalKind,
    pub(crate) end: u64,
    pub(crate) result: String,
    pub(crate) session_id: Option<String>,
    /// The (dev, ino) of the descriptor the read used.
    pub(crate) file: (u64, u64),
}

impl ClaudeTurnTerminal {
    /// The typed frame only bridge admission turns into a terminal; no tmux generation names it.
    fn frame(
        self,
        transcript: &str,
        logical: &str,
        start: u64,
        token: &Arc<CancelToken>,
    ) -> StreamMessage {
        StreamMessage::ClaudeTuiTerminalDone {
            result: self.result,
            session_id: self.session_id,
            transcript_path: transcript.to_owned(),
            tmux_session_name: logical.to_owned(),
            turn_nonce: token.turn_nonce().unwrap_or_default().to_owned(),
            source_start: start,
            complete_record_end: self.end,
            generation_mtime_ns: 0,
            source_file_dev: self.file.0,
            source_file_ino: self.file.1,
            actor: Arc::downgrade(token),
            kind: self.kind,
        }
    }
}

/// One Claude turn's records as the Herdr reader and bridge admission judge them alike: a
/// turn-end record counts only after this turn's own prompt, so an earlier turn's tail ends nothing.
#[derive(Default)]
pub(crate) struct HerdrTurnLines {
    stream: StreamLineState,
    prompt_seen: bool,
    /// Output before the prompt or a second prompt before the terminal: another turn's records.
    mixed: bool,
}

impl HerdrTurnLines {
    pub(crate) fn process(&mut self, line: &str, sender: &Sender<StreamMessage>) -> bool {
        if is_turn_prompt(line) {
            if self.prompt_seen {
                self.mixed |= self.terminal().is_none();
            } else {
                self.prompt_seen = true;
                self.stream.turn_ended = false;
                self.stream.interrupted = false;
                self.stream.final_result = None;
            }
        } else if !self.prompt_seen {
            self.mixed |= is_turn_output(line);
        }
        process_stream_line(line, sender, &mut self.stream)
    }

    /// The turn's terminal: its interrupt marker is an abort, a result or turn end a completion.
    pub(crate) fn terminal(&self) -> Option<NativeTerminalKind> {
        let stream = &self.stream;
        if !self.prompt_seen || self.mixed || (stream.final_result.is_none() && !stream.turn_ended)
        {
            return None;
        }
        Some(match stream.interrupted && stream.final_result.is_none() {
            true => NativeTerminalKind::Aborted,
            false => NativeTerminalKind::Completed,
        })
    }

    pub(crate) fn result(&self) -> (String, Option<String>) {
        let result = self.stream.final_result.clone().unwrap_or_default();
        (result, self.stream.last_session_id.clone())
    }
}

/// The typed frame the Herdr reader would send for `transcript` read from `start`, for admission tests.
#[cfg(test)]
pub(crate) fn herdr_terminal_frame(
    transcript: &str,
    start: u64,
    logical: &str,
    token: &Arc<CancelToken>,
) -> Option<StreamMessage> {
    let (tx, _rx) = std::sync::mpsc::channel();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let alive = move || std::time::Instant::now() < deadline;
    let read = read_to_provider_terminal(transcript, start, &tx, token, alive).ok()??;
    Some(read.frame(transcript, logical, start, token))
}

/// Admission's re-read of a Claude turn from its descriptor: from the first record stamped at or
/// after the input, as the reader starts, to the first terminal `HerdrTurnLines` accepts.
pub(crate) fn replay_herdr_turn(
    file: &mut std::fs::File,
    submitted_at: chrono::DateTime<chrono::Utc>,
    len: u64,
) -> Option<(u64, NativeTerminalKind, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.take(len).read_to_end(&mut bytes).ok()?;
    let records = complete_records(&bytes);
    let stamped = |line: &str| {
        let json = serde_json::from_str::<serde_json::Value>(line).ok()?;
        let stamp = json.get("timestamp")?.as_str()?;
        chrono::DateTime::parse_from_rfc3339(stamp).ok()
    };
    let (start, at) = records.iter().enumerate().find_map(|(at, (start, line))| {
        stamped(line)
            .filter(|stamp| *stamp >= submitted_at)
            .map(|_| (*start, at))
    })?;
    let (local, _rx) = std::sync::mpsc::channel();
    let mut lines = HerdrTurnLines::default();
    for (record_start, line) in &records[at..] {
        lines.process(line, &local);
        if let Some(kind) = lines.terminal() {
            let end = record_start + line.len() as u64 + 1;
            return Some((start, kind, end));
        }
    }
    None
}

/// Each newline-terminated record's start offset and trimmed text; a torn tail is not a record.
fn complete_records(bytes: &[u8]) -> Vec<(u64, &str)> {
    let mut records = Vec::new();
    let mut offset = 0u64;
    for raw in bytes.split_inclusive(|byte| *byte == b'\n') {
        if raw.last() != Some(&b'\n') {
            break;
        }
        let line = &raw[..raw.len() - 1];
        if let Ok(text) = std::str::from_utf8(line)
            && !text.trim().is_empty()
        {
            records.push((offset, text));
        }
        offset += raw.len() as u64;
    }
    records
}

/// An assistant or result record: a turn's own output, never metadata.
fn is_turn_output(line: &str) -> bool {
    let json = serde_json::from_str::<serde_json::Value>(line).ok();
    let kind = json.as_ref().and_then(|json| json.get("type")?.as_str());
    matches!(kind, Some("assistant" | "result"))
}

/// A user record that is a prompt: text, not a tool result, a meta note or an interrupt marker.
fn is_turn_prompt(line: &str) -> bool {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    if json.get("type").and_then(|kind| kind.as_str()) != Some("user")
        || json.get("isMeta").and_then(|meta| meta.as_bool()) == Some(true)
        || crate::services::tui_turn_state::envelope_is_turn_end_terminator(
            &ProviderKind::Claude,
            &json,
        )
    {
        return false;
    }
    match json.pointer("/message/content") {
        Some(serde_json::Value::String(text)) => !text.trim().is_empty(),
        Some(serde_json::Value::Array(blocks)) => {
            let kind = |block: &serde_json::Value, named: &str| {
                block.get("type").and_then(serde_json::Value::as_str) == Some(named)
            };
            blocks.iter().any(|block| kind(block, "text"))
                && !blocks.iter().any(|block| kind(block, "tool_result"))
        }
        _ => false,
    }
}

/// Under settlement the transcript is read to this turn's own turn-end record, returned unsent;
/// offsets advance only over processed records. Idleness, EOF or a dead pane end no turn.
fn read_to_provider_terminal(
    transcript: &str,
    start: u64,
    sender: &Sender<StreamMessage>,
    token: &Arc<CancelToken>,
    is_alive: impl FnMut() -> bool,
) -> Result<Option<ClaudeTurnTerminal>, String> {
    use crate::services::cluster::stream_relay::SourceFileIdentity;
    let mut lines = HerdrTurnLines::default();
    let mut opened = SourceFileIdentity::Unavailable;
    // A buffer's raw end is sent only once a later read shows all its records were processed.
    let pending = std::cell::Cell::new(None::<u64>);
    let (offsets, relay) = (sender.clone(), sender.clone());
    let (local, local_rx) = std::sync::mpsc::channel();
    let read = poll_output_file_until_result(
        transcript,
        start,
        Some(token.clone()),
        &mut lines,
        is_alive,
        || false,
        |raw_end| {
            if let Some(processed) = pending.replace(Some(raw_end)) {
                let _ = offsets.send(StreamMessage::OutputOffset { offset: processed });
            }
        },
        move |line, lines: &mut HerdrTurnLines| {
            let processed = lines.process(line, &local);
            // The turn's Done is the typed frame sent after the read, never the stream's own.
            for message in local_rx.try_iter() {
                if !matches!(message, StreamMessage::Done { .. }) {
                    let _ = relay.send(message);
                }
            }
            processed
        },
        |lines| lines.terminal().is_some(),
        |_| false,
        |_| {},
        |file| opened = SourceFileIdentity::from_open_file(file),
    )
    .map_err(|failure| failure.error)?;
    let end = match read {
        ReadOutputResult::Completed { offset }
        | ReadOutputResult::Cancelled { offset }
        | ReadOutputResult::SessionDied { offset } => offset,
    };
    if let Some(raw_end) = pending.get() {
        let _ = sender.send(StreamMessage::OutputOffset {
            offset: raw_end.min(end),
        });
    }
    let (ReadOutputResult::Completed { .. }, Some(kind)) = (read, lines.terminal()) else {
        return Ok(None);
    };
    let (result, session_id) = lines.result();
    let file = crate::services::provider::cancel_token_claude_interrupt::opened_file_identity(
        Some(&opened),
    );
    Ok(Some(ClaudeTurnTerminal {
        kind,
        end,
        result,
        session_id,
        file,
    }))
}

/// The row turns Bound once the pane's latest logged source is this execution's; until then it
/// stays Pending and the next turn attaches it again.
async fn bind_once_logged(turn: &HerdrTurn<'_>, nonce: &str) {
    let logical = turn.owner.logical_key.as_str();
    let logged = latest_logged(turn).is_some_and(|event| {
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

#[cfg(test)]
mod provider_terminal_tests;
