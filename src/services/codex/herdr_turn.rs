//! A Codex turn on a Herdr pane behind the default-off `runtime.herdr_codex_turn_enabled` switch:
//! one prompt to a launch's ready composer, its hold ending only once the hook Source is bound.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use tokio::runtime::Handle;

use super::tui_session_launch::{
    codex_herdr_home, codex_herdr_launch_admissible, prepare_codex_herdr_launch,
};
use super::{CodexLaunchOptions, codex_reasoning_effort_from_env};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLookup, HostedOwner, HostedRecord, HostedState,
};
use crate::services::agent_protocol::{RuntimeHandoff, StreamMessage};
use crate::services::claude::herdr_turn::{
    HoldRelease, gate, hold, launched, load, not_held, release_hold,
};
use crate::services::codex_tui::host_input::{InputRun, PlanRun, legacy_result, run_herdr};
use crate::services::codex_tui::input::{
    PROMPT_READY_CANCELLED_ERROR, pane_looks_ready_for_codex_prompt, plan_prompt_submit,
};
use crate::services::herdr_launch::{
    HerdrLaunch, HerdrLaunchCommand, HerdrLaunchEndpoint, HerdrLaunchHost, launch_herdr_session,
};
use crate::services::provider::{
    CancelToken, ProviderKind, ReadOutputResult, cancel_requested, is_readonly_tool_policy,
};
use crate::services::session_host::HerdrTarget;
use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
use crate::services::tui_prompt_dedupe::binding_events::{
    BindingCause, BindingTarget, SourceId, binding_events_since, subscribe_binding_events,
};

const COMPOSER_READY_WAIT: Duration = Duration::from_secs(15);
#[cfg(not(test))]
const SOURCE_WAIT: Duration = Duration::from_secs(60);
#[cfg(test)]
const SOURCE_WAIT: Duration = Duration::from_secs(3);

/// One turn; `row` is the record the turn host read for the channel's sessions row.
pub(crate) struct CodexHerdrTurn<'a> {
    pub pool: &'a PgPool,
    pub owner: HostedOwner,
    pub channel_id: u64,
    pub endpoint: HerdrLaunchEndpoint,
    pub row: Option<&'a HostedRecord>,
    pub prompt: &'a str,
    pub working_dir: &'a str,
    pub system_prompt: Option<&'a str>,
    pub allowed_tools: &'a [String],
    pub model: Option<&'a str>,
    pub fast_mode: Option<bool>,
    pub goals: Option<bool>,
    pub compact_token_limit: Option<u64>,
    pub cancel: Option<Arc<CancelToken>>,
}

/// What a turn takes from its caller: the launch host and the source attach.
pub(crate) trait CodexHerdrPorts {
    fn launch_host(&self) -> Option<Arc<dyn HerdrLaunchHost>>;
    /// `Ok(bound)` once the launch's own hook source is confirmed; `Err` attached nothing.
    fn attach(
        &self,
        owner: &HostedOwner,
        record: &HostedExecution,
        source: &SourceId,
        target: &HerdrTarget,
    ) -> Result<bool, String>;
}

/// Runs one turn on a blocking thread inside the runtime; every error leaves the pane as it is.
pub(crate) fn execute(
    turn: CodexHerdrTurn<'_>,
    ports: &dyn CodexHerdrPorts,
    sender: Sender<StreamMessage>,
) -> Result<(), String> {
    let runtime = Handle::try_current().map_err(|error| format!("herdr turn: {error}"))?;
    let nonce = match turn.row {
        Some(HostedRecord::Known(record)) if record.state == HostedState::Bound => {
            not_held(&record.execution_nonce)?;
            return Err("herdr turn: a bound codex pane takes no follow-up yet".into());
        }
        Some(HostedRecord::Known(record)) if record.state == HostedState::Pending => {
            not_held(&record.execution_nonce)?;
            pending_launch(&turn, &runtime, ports, record)?
        }
        Some(HostedRecord::Unknown(_)) => return Err("herdr turn: unreadable hosted record".into()),
        _ => fresh_launch(&turn, &runtime, ports)?,
    };
    first_prompt(&turn, &runtime, ports, &nonce, sender)
}

fn launch_options(turn: &CodexHerdrTurn<'_>) -> CodexLaunchOptions {
    let tools = Some(turn.allowed_tools);
    let instructions = super::compose_codex_developer_instructions(turn.system_prompt, tools);
    CodexLaunchOptions::new("")
        .with_developer_instructions(instructions.as_deref())
        .with_model(turn.model)
        .with_reasoning_effort(codex_reasoning_effort_from_env().as_deref())
        .with_compact_token_limit(turn.compact_token_limit)
        .with_readonly_mode(is_readonly_tool_policy(tools))
        .with_fast_mode_enabled(turn.fast_mode)
        .with_goals_enabled(turn.goals)
        .with_cwd(Some(turn.working_dir))
}

fn launch_of(turn: &CodexHerdrTurn<'_>, provider_root: Option<&Path>) -> HerdrLaunch {
    HerdrLaunch {
        endpoint: Some(turn.endpoint.clone()),
        owner: turn.owner.clone(),
        channel_id: Some(turn.channel_id),
        expected_native_session_id: None,
        resume: false,
        provider_root: provider_root.map(|home| home.join("sessions")),
    }
}

/// A new execution; hooks and daemon isolation are judged before its Pending row is written.
fn fresh_launch(
    turn: &CodexHerdrTurn<'_>,
    runtime: &Handle,
    ports: &dyn CodexHerdrPorts,
) -> Result<String, String> {
    let launchable = codex_herdr_launch_admissible()
        .map_err(|refused| format!("codex herdr launch refused: {refused:?}"))?;
    let host = ports.launch_host().ok_or("herdr turn: no launch host")?;
    let logical = &turn.owner.logical_key;
    let overlay = crate::services::discord::org_schema::overlay_from_tmux_session(
        ProviderKind::Codex,
        logical,
    )?;
    let home = codex_herdr_home(&overlay).ok_or("codex herdr launch: no Codex home")?;
    let options = launch_options(turn);
    let prepare = |prepared: &PreparedIncarnation| {
        prepare_codex_herdr_launch(prepared, &launchable, &options, &overlay, &home)
    };
    let launch = launch_of(turn, Some(&home));
    launched(runtime.block_on(launch_herdr_session(turn.pool, launch, prepare, host)))
}

/// A Pending execution whose first prompt never reached its composer: no hook of it is logged and
/// no hold is left. A pane without evidence is only probed again, never created.
fn pending_launch(
    turn: &CodexHerdrTurn<'_>,
    runtime: &Handle,
    ports: &dyn CodexHerdrPorts,
    record: &HostedExecution,
) -> Result<String, String> {
    let nonce = &record.execution_nonce;
    if hook_logged(turn, nonce)? {
        return Err(format!(
            "herdr turn: pending codex execution {nonce} already took its first prompt"
        ));
    }
    if record.expected.is_none() {
        let host = ports.launch_host().ok_or("herdr turn: no launch host")?;
        let never = |_: &PreparedIncarnation| -> Result<HerdrLaunchCommand, String> {
            Err("a pending pane is never created again".into())
        };
        let launch = launch_of(turn, None);
        launched(runtime.block_on(launch_herdr_session(turn.pool, launch, never, host)))?;
    }
    Ok(nonce.clone())
}

/// Whether any hook of execution `nonce` reached the pane's log; Codex sends its first hook only
/// with the first prompt. An unreadable log counts as one.
fn hook_logged(turn: &CodexHerdrTurn<'_>, nonce: &str) -> Result<bool, String> {
    let events = binding_events_since(turn.channel_id, 0)
        .map_err(|error| format!("herdr turn: binding log unreadable: {error}"))?;
    let logical = turn.owner.logical_key.as_str();
    Ok(events.iter().any(|event| {
        event.tmux_session == logical && event.execution_nonce.as_deref() == Some(nonce)
    }))
}

/// The source execution `nonce`'s own SessionStart(startup) hook recorded, while it is the pane's
/// latest record; refusal audits do not move the pane.
fn logged_source(turn: &CodexHerdrTurn<'_>, nonce: &str) -> Option<SourceId> {
    let logical = turn.owner.logical_key.as_str();
    let events = binding_events_since(turn.channel_id, 0).ok()?;
    let moved = |event: &_| !matches!(event, BindingTarget::Rejected { .. });
    let latest = events
        .into_iter()
        .rev()
        .filter(|event| event.tmux_session == logical)
        .find(|event| moved(&event.new))?;
    let own = latest.provider == "codex"
        && latest.execution_nonce.as_deref() == Some(nonce)
        && latest.cause == BindingCause::Startup;
    match latest.new {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } if own => {
            Some(source)
        }
        _ => None,
    }
}

/// The row's execution while it is still `nonce`'s.
fn current_record(
    turn: &CodexHerdrTurn<'_>,
    runtime: &Handle,
    nonce: &str,
) -> Result<HostedExecution, String> {
    match runtime.block_on(load(turn.pool, &turn.owner)) {
        HostedLookup::Found(found) => match found.record {
            HostedRecord::Known(record) if record.execution_nonce == nonce => Ok(record),
            other => Err(format!("herdr turn: row moved on: {other:?}")),
        },
        other => Err(format!("herdr turn: row unreadable: {other:?}")),
    }
}

/// Polls the pane until Codex's composer is ready; a modal keeps it unready. A cancel or the
/// deadline stops the turn with nothing written.
fn composer_ready(target: &HerdrTarget, cancel: Option<&CancelToken>) -> Result<(), String> {
    let deadline = Instant::now() + COMPOSER_READY_WAIT;
    loop {
        if cancel_requested(cancel) {
            return Err(PROMPT_READY_CANCELLED_ERROR.to_string());
        }
        let screen = target.capture(-80);
        if screen.is_some_and(|screen| pane_looks_ready_for_codex_prompt(&screen)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("herdr turn: the codex composer did not become ready".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Whether a run that submitted nothing left the composer as it was: only a refusal or a cancel
/// with no write attempted. An unclear send may have landed, whatever its flags say.
fn composer_untouched(run: &PlanRun) -> bool {
    matches!(run.run, InputRun::Refused(_) | InputRun::Cancelled { .. })
        && !run.enter_attempted
        && !run.composer_mutated
}

fn warn_release(logical: &str, released: HoldRelease) {
    match released {
        HoldRelease::Released => {}
        HoldRelease::NotDurable(error) => tracing::warn!(
            logical, %error, "herdr turn: input hold removed; the removal may not survive a crash"
        ),
        HoldRelease::Kept(error) => {
            tracing::warn!(logical, %error, "herdr turn: input hold kept; later prompts stay held")
        }
    }
}

/// The first prompt of execution `nonce`: one gated write on a ready composer, then its own hook
/// Source, the attach, the hold's end and the new rollout from its start.
fn first_prompt(
    turn: &CodexHerdrTurn<'_>,
    runtime: &Handle,
    ports: &dyn CodexHerdrPorts,
    nonce: &str,
    sender: Sender<StreamMessage>,
) -> Result<(), String> {
    let logical = turn.owner.logical_key.as_str();
    let plan = plan_prompt_submit(turn.prompt)?;
    // Opened before the prompt, so a fast hook cannot land unseen.
    let mut logged = subscribe_binding_events(turn.channel_id)
        .map_err(|error| format!("herdr turn: binding log unavailable: {error}"))?;
    let target = gate(&current_record(turn, runtime, nonce)?)?;
    composer_ready(&target, turn.cancel.as_deref())?;
    crate::services::tui_prompt_dedupe::register_provider_session("codex", logical, logical);
    crate::services::tui_prompt_dedupe::register_codex_herdr_placeholder(logical, turn.channel_id);
    hold(nonce)?;
    let run = run_herdr(&target, &plan, turn.cancel.as_deref());
    if run.run != InputRun::Applied {
        if composer_untouched(&run) {
            warn_release(logical, release_hold(nonce));
        }
        return legacy_result(run.run);
    }
    // Submitted: a cancel from here on is remembered on its token, never a reason to stop waiting.
    let deadline = Instant::now() + SOURCE_WAIT;
    let source = loop {
        if let Some(source) = logged_source(turn, nonce) {
            break source;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!(
                "herdr turn: no session start of codex execution {nonce}; later prompts stay held"
            ));
        }
        let poll = left.min(Duration::from_millis(200));
        let _ = runtime.block_on(tokio::time::timeout(poll, logged.changed()));
    };
    let record = current_record(turn, runtime, nonce)?;
    if !ports.attach(&turn.owner, &record, &source, &target)? {
        return Err(format!("herdr turn: codex execution {nonce} is not bound"));
    }
    warn_release(logical, release_hold(nonce));
    let session_id = source.session_id.clone();
    let _ = sender.send(StreamMessage::Init {
        session_id: session_id.clone(),
        raw_session_id: Some(session_id.clone()),
    });
    let read = crate::services::codex_tui::rollout_tail::tail_rollout_file_from_offset(
        &source.path,
        0,
        Some(&session_id),
        sender.clone(),
        turn.cancel.clone(),
        || target.execution_alive(),
    )?;
    let ReadOutputResult::Completed { offset } = read else {
        return Ok(());
    };
    let _ = sender.send(StreamMessage::RuntimeReady {
        handoff: RuntimeHandoff::CodexTui {
            rollout_path: source.path.display().to_string(),
            thread_id: Some(session_id),
            tmux_session_name: logical.to_owned(),
            last_offset: offset,
        },
    });
    Ok(())
}
