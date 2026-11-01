//! Shared provider invocation for user intake and explicit headless turns.
//! Keep their provider selection, cancellation and session options identical.

use std::sync::{Arc, mpsc::Sender};
use std::time::Duration;

use crate::services::agent_protocol::StreamMessage;
use crate::services::discord::codex_absence::InflightObs;
use crate::services::discord::inflight::{ManagedSubmission, SubmissionFailure};
use crate::services::discord::replay_policy::permit::EffectTarget;
use crate::services::discord::router::intake_dispatch::replay_admission::ReplayStart;
use crate::services::provider::{CancelToken, LegacyDispatchKind, ProviderKind};
use crate::services::provider_teardown::TeardownClearance;
use crate::services::stream_json_cli::{
    ConfiguredToolPolicy, ProviderTurnRequest, execute_streaming,
};
use crate::services::turn_host::{HerdrRefusal, HerdrTurnPlan, ReturnPlan, TurnHost};
use crate::services::{claude, claude_tui, codex, gemini, opencode, qwen};

#[cfg(unix)]
mod owner_input;
#[cfg(unix)]
pub(in crate::services::discord) use owner_input::{
    OwnerTurn, admit_start as owner_admit_start, recover as owner_recover, wake as owner_wake,
};
pub(super) mod replay_scope;

/// No input owner runs a turn off unix.
#[cfg(not(unix))]
pub(in crate::services::discord) enum OwnerTurn {}
#[cfg(not(unix))]
impl OwnerTurn {
    pub(in crate::services::discord) fn front(
        &self,
    ) -> &crate::services::turn_orchestrator::OwnerFront {
        match *self {}
    }
}

pub(super) struct StreamingTurn<'a> {
    pub pool: Option<&'a sqlx::PgPool>,
    /// The bot whose channel lease a Herdr turn takes to resolve an unplaced launch.
    pub shared: Option<&'a Arc<crate::services::discord::SharedData>>,
    pub provider: &'a ProviderKind,
    pub prompt: &'a str,
    pub session_id: Option<&'a str>,
    pub working_dir: &'a str,
    pub system_prompt: Option<&'a str>,
    pub allowed_tools: &'a [String],
    pub cancel: Arc<CancelToken>,
    pub codex_observation: Option<&'a InflightObs>,
    pub tmux_session_name: Option<&'a str>,
    /// Host-guard verdict for the turn's tmux teardowns, judged before spawn.
    pub teardown: Option<&'a TeardownClearance>,
    /// The turn's host, judged before spawn; only `Tmux` reaches a provider driver.
    pub host: &'a TurnHost,
    pub channel_id: u64,
    pub model: Option<&'a str>,
    pub native_fast_mode: Option<bool>,
    pub codex_goals: Option<bool>,
    pub compact_percent: Option<u64>,
    pub compact_lower_bound_tokens: u64,
    pub compact_token_limit: Option<u64>,
    pub cache_ttl_minutes: Option<u32>,
    pub dispatch_type: Option<&'a str>,
    pub force_fresh: bool,
    /// Pre-payload fence of a managed Claude TUI turn; only the Claude legacy driver reads it.
    pub submission: Option<&'a ManagedSubmission>,
}

pub(super) async fn observe_codex_turn(
    provider: &ProviderKind,
    channel: u64,
    tmux: Option<&str>,
    host: &TurnHost,
    token: &Arc<CancelToken>,
) -> Option<InflightObs> {
    if !matches!(provider, ProviderKind::Codex) || !matches!(host, TurnHost::Tmux) {
        return None;
    }
    crate::services::discord::codex_absence::observe_turn(channel, tmux?, token).await
}

#[cfg(test)]
pub(in crate::services::discord) struct InputEffectProbe {
    pub channel: u64,
    /// Receives the thread the provider ran on, so a caller can tell it left the async task.
    pub entered: tokio::sync::oneshot::Sender<std::thread::ThreadId>,
    pub release: std::sync::mpsc::Receiver<()>,
    pub terminal_before_release: bool,
}
#[cfg(test)]
pub(in crate::services::discord) static INPUT_EFFECT_PROBE: std::sync::Mutex<
    Option<InputEffectProbe>,
> = std::sync::Mutex::new(None);
#[cfg(test)]
pub(super) type InputEffectObserver = Box<dyn FnOnce(String) + Send>;
#[cfg(test)]
pub(super) static INPUT_EFFECT_OBSERVER: std::sync::Mutex<Option<(u64, InputEffectObserver)>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
pub(super) struct SubmittedPrompt {
    pub prompt: String,
    pub session_id: Option<String>,
}
#[cfg(test)]
pub(super) static INPUT_PROMPT_PROBE: std::sync::Mutex<
    Option<(u64, tokio::sync::oneshot::Sender<SubmittedPrompt>)>,
> = std::sync::Mutex::new(None);
/// Provider dispatches a test counts on one channel; a counted dispatch with no effect probe left
/// is refused, so a duplicate start never reaches the real provider.
#[cfg(test)]
pub(in crate::services::discord) static COUNTED_DISPATCHES: std::sync::Mutex<Option<(u64, usize)>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
fn count_dispatch(channel: u64) -> Result<(), String> {
    let mut counted = COUNTED_DISPATCHES.lock().unwrap();
    let Some((_, count)) = counted.as_mut().filter(|(counted, _)| *counted == channel) else {
        return Ok(());
    };
    *count += 1;
    let probed = INPUT_EFFECT_PROBE
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|probe| probe.channel == channel);
    if probed {
        Ok(())
    } else {
        Err("counted test dispatch has no provider probe".into())
    }
}
#[cfg(test)]
fn input_effect_probe(
    turn: &StreamingTurn<'_>,
    sender: &Sender<StreamMessage>,
) -> Option<Result<(), String>> {
    let probe = {
        let mut slot = INPUT_EFFECT_PROBE.lock().unwrap();
        if slot
            .as_ref()
            .is_some_and(|probe| probe.channel == turn.channel_id)
        {
            slot.take()
        } else {
            None
        }
    }?;
    let observer = {
        let mut slot = INPUT_EFFECT_OBSERVER.lock().unwrap();
        if slot
            .as_ref()
            .is_some_and(|(channel, _)| *channel == turn.channel_id)
        {
            slot.take().map(|(_, observer)| observer)
        } else {
            None
        }
    };
    if let Some(observer) = observer {
        observer(turn.provider.as_str().to_owned());
    }
    let _ = probe.entered.send(std::thread::current().id());
    let terminal = || {
        sender
            .send(StreamMessage::Done {
                result: tests::take_result(turn.channel_id),
                session_id: Some("input-effect-provider".into()),
            })
            .map_err(|error| error.to_string())
    };
    if probe.terminal_before_release {
        let result = terminal();
        // Sender disconnect also releases the fixture during assertion unwinding.
        let _ = probe.release.recv_timeout(Duration::from_secs(30));
        Some(result)
    } else {
        let _ = probe.release.recv_timeout(Duration::from_secs(30));
        Some(terminal())
    }
}

/// Stands in for the Claude driver inside the submission scope, on the real provider thread.
#[cfg(test)]
pub(in crate::services::discord) type ClaudeLegacyProbe =
    Box<dyn FnOnce() -> Result<(), String> + Send>;
#[cfg(test)]
pub(in crate::services::discord) static CLAUDE_LEGACY_PROBE: std::sync::Mutex<
    Option<(u64, ClaudeLegacyProbe)>,
> = std::sync::Mutex::new(None);
#[cfg(test)]
fn claude_legacy_probe(channel: u64) -> Option<Result<(), String>> {
    let mut slot = CLAUDE_LEGACY_PROBE.lock().unwrap();
    if !slot.as_ref().is_some_and(|(probed, _)| *probed == channel) {
        return None;
    }
    slot.take().map(|(_, probe)| probe())
}

/// User and headless workers report panic failures through the same bridge message.
pub(super) fn producer_panic(scope: &str, info: Box<dyn std::any::Any + Send>) -> StreamMessage {
    let message = if let Some(value) = info.downcast_ref::<String>() {
        value.clone()
    } else if let Some(value) = info.downcast_ref::<&str>() {
        value.to_string()
    } else {
        "unknown panic".to_string()
    };
    tracing::error!("  [{}] PANIC: {}", scope, message);
    StreamMessage::Error {
        message: format!("Internal error (panic): {}", message),
        stdout: String::new(),
        stderr: String::new(),
        exit_code: None,
    }
}

/// The bridge message for a provider `Err`. A prompt held before submit sends none, so the
/// bridge keeps the Waiting row instead of finishing the turn as a provider failure.
pub(super) fn producer_error(
    submission: Option<&ManagedSubmission>,
    channel_id: u64,
    error: String,
) -> Option<StreamMessage> {
    if submission.is_some_and(|attempt| attempt.preparation_hold().is_some()) {
        return None;
    }
    match submission.map(ManagedSubmission::classify_failure) {
        Some(SubmissionFailure::GuardedStopped(run)) => {
            tracing::warn!(channel_id, ?run, "guarded Claude submission remains held");
            return None;
        }
        Some(SubmissionFailure::GuardedRunning) => return None,
        Some(SubmissionFailure::PreSubmitPersistenceRefused(refused)) => {
            tracing::warn!(channel_id, %refused, "provider held the prompt before submit");
            return None;
        }
        Some(SubmissionFailure::IdleStashObserved) => {
            tracing::warn!(
                channel_id,
                "provider failed after an observed idle-stash delivery"
            );
        }
        Some(SubmissionFailure::SubmissionIndeterminate) => {
            tracing::warn!(channel_id, "provider failed after the submission fence");
        }
        Some(SubmissionFailure::ProviderExecutionFailed) | None => {
            // The owner's NotSent outcome is a held input, not a failed provider response.
            if error.starts_with("NotSent(") {
                tracing::info!(channel_id, %error, "input not submitted; provider error withheld");
                return None;
            }
        }
    }
    Some(StreamMessage::Error {
        message: error,
        stdout: String::new(),
        stderr: String::new(),
        exit_code: None,
    })
}

pub(super) fn execute(
    turn: StreamingTurn<'_>,
    start: ReplayStart,
    sender: Sender<StreamMessage>,
    owner: Option<OwnerTurn>,
) -> Result<(), String> {
    let start = if start.receipt_id().is_some() {
        let scope = replay_scope::selected(turn.provider, turn.channel_id, turn.tmux_session_name);
        start
            .in_scope(scope.driver.as_str(), scope.runtime)
            .map_err(|error| format!("replay driver scope changed: {error:?}"))?
    } else {
        start
    };
    // Spent before any driver, Herdr included, so a protected start's first input needs its permit.
    let target = EffectTarget::ProviderStart {
        provider: turn.provider.as_str().to_string(),
        channel: turn.channel_id.to_string(),
    };
    start
        .spend(&target, turn.prompt, &turn.cancel)
        .map_err(|refusal| format!("replay start refused: {refusal:?}"))?;
    // The session comes back from Herdr first, then the owner or the driver runs it on tmux under
    // a clearance that tears nothing down.
    let (tmux, returned, mut turn) = (TurnHost::Tmux, TeardownClearance::Returned, turn);
    if let TurnHost::Return(plan) = turn.host {
        return_to_tmux(&turn, plan)?;
        (turn.host, turn.teardown) = (&tmux, Some(&returned));
    }
    #[cfg(unix)]
    if let Some(owner) = owner {
        return owner_input::execute_owned(turn, sender, owner);
    }
    #[cfg(not(unix))]
    if let Some(owner) = owner {
        match owner {}
    }
    #[cfg(all(test, unix))]
    match owner_input::take(&turn) {
        Some(owner_input::Taken::Done(result)) => return result,
        Some(owner_input::Taken::Launch(guard)) => return guard.finish(drive(turn, sender)),
        None => {}
    }
    drive(turn, sender)
}

/// The turn's driver, Herdr or tmux, after its start was spent.
fn drive(turn: StreamingTurn<'_>, sender: Sender<StreamMessage>) -> Result<(), String> {
    match turn.host {
        TurnHost::Tmux => {}
        TurnHost::Refused(refusal) => return Err(refusal.to_string()),
        // A Herdr turn never falls back to another driver.
        TurnHost::Herdr(plan) => {
            return crate::services::tui_o::exact_submission::dispatch(|| {
                herdr_turn(&turn, plan, sender)
            });
        }
        // `execute` brings a returning session back before any driver runs.
        TurnHost::Return(_) => return Err("herdr return: not returned before the turn".into()),
    }
    #[cfg(test)]
    count_dispatch(turn.channel_id)?;
    #[cfg(test)]
    {
        let mut probe = INPUT_PROMPT_PROBE.lock().unwrap();
        if probe
            .as_ref()
            .is_some_and(|(channel, _)| *channel == turn.channel_id)
        {
            let (_, capture) = probe.take().unwrap();
            let _ = capture.send(SubmittedPrompt {
                prompt: turn.prompt.to_owned(),
                session_id: turn.session_id.map(str::to_owned),
            });
        }
    }
    #[cfg(test)]
    if let Some(result) = input_effect_probe(&turn, &sender) {
        return result;
    }
    let _execution_guard = crate::services::cluster::execution_capacity::acquire(
        turn.pool,
        turn.provider.as_str(),
        turn.channel_id,
        Arc::clone(&turn.cancel),
    )?;
    let turn = StreamingTurn {
        system_prompt: turn.system_prompt.and_then(|prompt| {
            crate::services::provider::system_prompt_for_provider_turn(
                turn.provider,
                turn.session_id,
                prompt,
            )
        }),
        ..turn
    };
    #[cfg(test)]
    let _driver_setup =
        crate::services::discord::codex_absence::test_setup::enter(turn.channel_id, turn.prompt);
    match turn.provider.legacy_streaming_dispatch_kind() {
        LegacyDispatchKind::Claude => claude_tui::guarded_outcome::with_owner(
            turn.submission.map(ManagedSubmission::observer),
            || {
                claude_tui::submission_fence::with_scope(
                    turn.submission.map(ManagedSubmission::fence),
                    || {
                        #[cfg(all(test, unix))]
                        submission_tests::observe_owner(
                            turn.channel_id,
                            turn.submission,
                            turn.prompt,
                        );
                        #[cfg(test)]
                        if let Some(result) = claude_legacy_probe(turn.channel_id) {
                            return result;
                        }
                        claude::execute_command_streaming(
                            turn.prompt,
                            turn.session_id,
                            turn.working_dir,
                            sender,
                            turn.system_prompt,
                            Some(turn.allowed_tools),
                            Some(turn.cancel),
                            turn.tmux_session_name,
                            turn.teardown,
                            Some(turn.channel_id),
                            Some(turn.provider.clone()),
                            turn.model,
                            turn.native_fast_mode,
                            turn.compact_percent,
                            turn.compact_lower_bound_tokens,
                            turn.cache_ttl_minutes,
                            turn.dispatch_type,
                        )
                    },
                )
            },
        ),
        LegacyDispatchKind::Codex => codex::execute_command_streaming_observed(
            codex::CommandStreamingRequest {
                prompt: turn.prompt,
                session_id: turn.session_id,
                working_dir: turn.working_dir,
                sender,
                system_prompt: turn.system_prompt,
                allowed_tools: Some(turn.allowed_tools),
                cancel_token: Some(turn.cancel),
                remote_profile: None,
                tmux_session_name: turn.tmux_session_name,
                teardown: turn.teardown,
                report_channel_id: Some(turn.channel_id),
                report_provider: Some(turn.provider.clone()),
                model: turn.model,
                fast_mode_enabled: turn.native_fast_mode,
                goals_enabled: turn.codex_goals,
                compact_token_limit: turn.compact_token_limit,
                force_fresh_provider_session: turn.force_fresh,
            },
            turn.codex_observation,
        ),
        LegacyDispatchKind::Gemini => gemini::execute_command_streaming(
            turn.prompt,
            turn.session_id,
            turn.working_dir,
            sender,
            turn.system_prompt,
            Some(turn.allowed_tools),
            Some(turn.cancel),
            turn.tmux_session_name,
            Some(turn.channel_id),
            Some(turn.provider.clone()),
            turn.model,
            None,
        ),
        LegacyDispatchKind::Qwen => qwen::execute_command_streaming(
            turn.prompt,
            turn.session_id,
            turn.working_dir,
            sender,
            turn.system_prompt,
            Some(turn.allowed_tools),
            Some(turn.cancel),
            turn.tmux_session_name,
            Some(turn.channel_id),
            Some(turn.provider.clone()),
            turn.model,
            None,
            turn.force_fresh,
        ),
        LegacyDispatchKind::OpenCode => opencode::execute_command_streaming(
            turn.prompt,
            turn.session_id,
            turn.working_dir,
            sender,
            turn.system_prompt,
            Some(turn.allowed_tools),
            Some(turn.cancel),
            turn.tmux_session_name,
            Some(turn.channel_id),
            Some(turn.provider.clone()),
            turn.model,
            None,
        ),
        LegacyDispatchKind::StreamJsonCli(dialect) => {
            execute_streaming(dialect, stream_json_request(&turn)?, sender)
        }
        LegacyDispatchKind::Unsupported(name) => {
            let _ = sender.send(StreamMessage::Error {
                message: format!("Provider '{}' is not installed", name),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
            });
            Ok(())
        }
    }
}

/// Hands the channel's Herdr execution back to tmux before its tmux turn runs.
#[cfg(unix)]
fn return_to_tmux(turn: &StreamingTurn<'_>, plan: &ReturnPlan) -> Result<(), String> {
    use crate::services::claude::herdr_return::{ProductionPorts, ReturnTurn, execute};
    let pool = turn.pool.ok_or("herdr return: no database")?;
    let back = ReturnTurn {
        pool,
        channel_id: turn.channel_id,
        working_dir: turn.working_dir,
        session_id: turn.session_id,
        force_fresh: turn.force_fresh,
    };
    execute(
        &back,
        plan,
        &tokio::runtime::Handle::current(),
        &ProductionPorts,
    )
}

#[cfg(not(unix))]
fn return_to_tmux(_turn: &StreamingTurn<'_>, plan: &ReturnPlan) -> Result<(), String> {
    let _ = (&plan.row, &plan.record);
    Err("herdr return: unix only".into())
}

/// The switch is read first; off, the turn is refused before any I/O.
#[cfg_attr(not(unix), allow(unused_variables))]
fn herdr_turn(
    turn: &StreamingTurn<'_>,
    plan: &HerdrTurnPlan,
    sender: Sender<StreamMessage>,
) -> Result<(), String> {
    #[cfg(unix)]
    if crate::services::turn_host::herdr_turn_switched_on_for(turn.provider) {
        return herdr::execute(turn, plan, sender);
    }
    let refusal = HerdrRefusal::ExecutorNotWired;
    tracing::warn!(endpoint = %plan.endpoint.config_key, "{refusal}");
    Err(refusal.to_string())
}

#[cfg(unix)]
mod herdr {
    use std::sync::{Arc, mpsc::Sender};

    use sqlx::PgPool;
    use tokio::runtime::Handle;

    use super::StreamingTurn;
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedExecution, HostedLookupKey, HostedOwner,
    };
    use crate::services::agent_protocol::StreamMessage;
    use crate::services::claude::herdr_turn::{self, AttachRequest, HerdrTurn, HerdrTurnPorts};
    use crate::services::claude_tui::hook_server::{HookEvent, subscribe_hook_events};
    use crate::services::codex::herdr_turn::{CodexHerdrPorts, CodexHerdrTurn};
    use crate::services::discord::UnplacedLease;
    use crate::services::discord::recovery_engine::host_reconcile::{
        HerdrEndpointId, HerdrExecutionReader, HerdrPaneEvidence, HerdrPaneReading,
        reconcile_hosted_session_pg,
    };
    use crate::services::discord::tui_prompt_relay::herdr_source::{
        HerdrSourceAttach, attach_launched_codex_herdr_source, attach_launched_herdr_source,
    };
    use crate::services::herdr_launch::HerdrLaunchHost;
    use crate::services::provider::ProviderKind;
    use crate::services::session_host::HerdrTarget;
    use crate::services::tui_prompt_dedupe::binding_events::SourceId;
    use crate::services::turn_host::HerdrTurnPlan;

    pub(super) fn execute(
        turn: &StreamingTurn<'_>,
        plan: &HerdrTurnPlan,
        sender: Sender<StreamMessage>,
    ) -> Result<(), String> {
        let pool = turn.pool.ok_or("herdr turn: no database")?;
        let _execution_guard = crate::services::cluster::execution_capacity::acquire(
            turn.pool,
            turn.provider.as_str(),
            turn.channel_id,
            Arc::clone(&turn.cancel),
        )?;
        let ports = BootPorts {
            pool,
            channel_id: turn.channel_id,
            shared: turn.shared,
        };
        if *turn.provider == ProviderKind::Codex {
            let codex = CodexHerdrTurn {
                pool,
                owner: owner(turn.provider, turn.channel_id)?,
                channel_id: turn.channel_id,
                endpoint: plan.endpoint.clone(),
                row: plan.row.as_ref().map(|observed| &observed.record),
                prompt: turn.prompt,
                working_dir: turn.working_dir,
                system_prompt: turn.system_prompt,
                allowed_tools: turn.allowed_tools,
                model: turn.model,
                fast_mode: turn.native_fast_mode,
                goals: turn.codex_goals,
                compact_token_limit: turn.compact_token_limit,
                cancel: Some(Arc::clone(&turn.cancel)),
            };
            return crate::services::codex::herdr_turn::execute(codex, &ports, sender);
        }
        let herdr = HerdrTurn {
            pool,
            owner: owner(turn.provider, turn.channel_id)?,
            channel_id: turn.channel_id,
            endpoint: plan.endpoint.clone(),
            row: plan.row.as_ref().map(|observed| &observed.record),
            prompt: turn.prompt,
            working_dir: turn.working_dir,
            session_id: turn.session_id,
            force_fresh: turn.force_fresh,
            system_prompt: turn.system_prompt.filter(|prompt| !prompt.is_empty()),
            model: turn.model,
            hook_endpoint: crate::services::claude_tui::hook_server::current_hook_endpoint(),
            cancel: Some(Arc::clone(&turn.cancel)),
        };
        herdr_turn::execute(herdr, &ports, sender)
    }

    /// The channel's canonical row owner, named by the session key the turn runs under.
    fn owner(provider: &ProviderKind, channel_id: u64) -> Result<HostedOwner, String> {
        let context = crate::services::platform::active_provider_context(provider.as_str());
        let key = context.and_then(|context| context.session_key);
        let identity = key
            .as_deref()
            .and_then(crate::services::discord::session_identity::SessionIdentity::parse);
        let Some((token_hash, logical_key)) =
            identity.and_then(|identity| Some((identity.token_hash?, identity.tmux_name)))
        else {
            return Err(format!("herdr turn: no namespaced session key ({key:?})"));
        };
        let node = crate::config::session_hosts::local_node();
        let root = crate::config::runtime_root().map(|root| root.display().to_string());
        Ok(HostedOwner {
            provider: provider.as_str().into(),
            discord_token_hash: token_hash,
            channel_id: channel_id.to_string(),
            logical_key,
            owner_node: node.ok_or("herdr turn: no cluster.instance_id")?,
            runtime_root: root.ok_or("herdr turn: no runtime root")?,
        })
    }

    pub(super) struct BootPorts<'a> {
        pub(super) pool: &'a PgPool,
        pub(super) channel_id: u64,
        pub(super) shared: Option<&'a Arc<crate::services::discord::SharedData>>,
    }

    impl CodexHerdrPorts for BootPorts<'_> {
        fn launch_host(&self) -> Option<Arc<dyn HerdrLaunchHost>> {
            herdr_turn::boot_launch_host()
        }

        fn attach(
            &self,
            owner: &HostedOwner,
            record: &HostedExecution,
            source: &SourceId,
            target: &HerdrTarget,
        ) -> Result<bool, String> {
            let reader = GateReader::of(record, target)?;
            let attached = Handle::current().block_on(attach_launched_codex_herdr_source(
                self.pool,
                owner,
                self.channel_id,
                &record.execution_nonce,
                source,
                &reader,
            ));
            match attached {
                HerdrSourceAttach::Published { bound, .. } => Ok(bound),
                other => Err(format!("herdr turn: codex source not attached: {other:?}")),
            }
        }

        fn confirm_bound(
            &self,
            owner: &HostedOwner,
            record: &HostedExecution,
            target: &HerdrTarget,
        ) -> Result<(), String> {
            self.confirmed(owner, record, target)
        }

        fn unplaced_lease(&self, _: &HostedOwner) -> Result<UnplacedLease, String> {
            self.lease()
        }
    }

    impl HerdrTurnPorts for BootPorts<'_> {
        fn launch_host(&self) -> Option<Arc<dyn HerdrLaunchHost>> {
            herdr_turn::boot_launch_host()
        }

        fn hook_events(&self) -> tokio::sync::broadcast::Receiver<HookEvent> {
            subscribe_hook_events()
        }

        fn attach(&self, request: &AttachRequest<'_>) -> Result<bool, String> {
            let reader = GateReader::of(request.record, request.target)?;
            let attached = Handle::current().block_on(attach_launched_herdr_source(
                self.pool,
                request.owner,
                self.channel_id,
                &request.record.execution_nonce,
                request.session_id,
                request.transcript,
                &reader,
            ));
            match attached {
                HerdrSourceAttach::Published { bound, .. } => Ok(bound),
                other => Err(format!("herdr turn: source not attached: {other:?}")),
            }
        }

        fn confirm_bound(
            &self,
            owner: &HostedOwner,
            record: &HostedExecution,
            target: &HerdrTarget,
        ) -> Result<(), String> {
            self.confirmed(owner, record, target)
        }

        fn unplaced_lease(&self, _: &HostedOwner) -> Result<UnplacedLease, String> {
            self.lease()
        }
    }

    impl BootPorts<'_> {
        /// Waits on the channel's transition lease as any transition claim does.
        fn lease(&self) -> Result<UnplacedLease, String> {
            let shared = self.shared.ok_or("no channel lease")?;
            let channel = crate::services::discord::ChannelId::new(self.channel_id);
            Handle::current()
                .block_on(UnplacedLease::acquire(shared, channel))
                .map_err(|_| "transition_busy".to_string())
        }

        /// `Ok` only while the reconcile reads the Bound execution as a match.
        fn confirmed(
            &self,
            owner: &HostedOwner,
            record: &HostedExecution,
            target: &HerdrTarget,
        ) -> Result<(), String> {
            let reader = GateReader::of(record, target)?;
            let identity = crate::db::dispatched_session_canonical_identity::CanonicalSessionIdentity {
                kind: crate::db::dispatched_session_canonical_identity::SessionIdentityKind::DiscordChannel,
                discord_token_hash: &owner.discord_token_hash,
                channel_id: &owner.channel_id,
            };
            let provider = &owner.provider;
            let key = HostedLookupKey::Canonical { provider, identity };
            let verdict =
                Handle::current().block_on(reconcile_hosted_session_pg(self.pool, key, &reader));
            if verdict.admits_reconnect() {
                return Ok(());
            }
            Err(format!(
                "herdr turn: bound execution not confirmed: {verdict:?}"
            ))
        }
    }

    /// The pane as its own gate reads it: the stored execution only when the gate's probe confirms
    /// root, provider and nonce on one server; never a guess.
    struct GateReader<'a> {
        endpoint: HerdrEndpointId,
        pane: &'a str,
        evidence: HerdrPaneEvidence,
        target: &'a HerdrTarget,
    }

    impl<'a> GateReader<'a> {
        fn of(record: &'a HostedExecution, target: &'a HerdrTarget) -> Result<Self, String> {
            let (Some(location), Some(expected)) = (&record.location, &record.expected) else {
                return Err("herdr turn: the execution has no launch evidence".into());
            };
            let evidence = HerdrPaneEvidence {
                binding_nonce: Some(expected.binding_nonce.clone()),
                root: Some(expected.root.clone()),
                provider_process: Some(expected.provider_process.clone()),
                agent_session_id: None,
            };
            Ok(Self {
                endpoint: HerdrEndpointId::of(location),
                pane: &location.pane_id,
                evidence,
                target,
            })
        }
    }

    impl HerdrExecutionReader for GateReader<'_> {
        fn endpoint(&self) -> Option<&HerdrEndpointId> {
            Some(&self.endpoint)
        }

        fn read_pane(&self, pane_id: &str) -> HerdrPaneReading {
            if pane_id == self.pane && self.target.execution_confirmed() {
                return HerdrPaneReading::Present(self.evidence.clone());
            }
            HerdrPaneReading::Unreadable("the pane gate did not confirm the execution".into())
        }
    }
}

fn stream_json_request(turn: &StreamingTurn<'_>) -> Result<ProviderTurnRequest, String> {
    ProviderTurnRequest::for_discord_turn(
        turn.provider.clone(),
        turn.prompt.to_string(),
        turn.system_prompt.map(str::to_string),
        ConfiguredToolPolicy::from_legacy_allowed_tools(turn.allowed_tools),
        turn.model.map(str::to_string),
        None,
        turn.working_dir.into(),
        turn.session_id,
        turn.force_fresh,
        Duration::from_secs(300),
        Some(Arc::clone(&turn.cancel)),
        Some(turn.channel_id),
    )
}

#[cfg(test)]
#[path = "provider_dispatch_herdr_tests.rs"]
mod herdr_tests;

#[cfg(test)]
#[path = "provider_dispatch_codex_herdr_tests.rs"]
mod codex_herdr_tests;

#[cfg(test)]
#[path = "provider_dispatch_submission_tests.rs"]
mod submission_tests;

#[cfg(all(test, unix))]
#[path = "provider_dispatch_idle_stash_tests.rs"]
mod idle_stash_tests;

#[cfg(test)]
pub(super) mod producer_error_test_support;

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    static INPUT_RESULT: std::sync::Mutex<Option<(u64, String)>> = std::sync::Mutex::new(None);

    pub(in crate::services::discord::router::message_handler) struct ResultReset(u64);

    impl Drop for ResultReset {
        fn drop(&mut self) {
            let mut result = INPUT_RESULT.lock().unwrap_or_else(|p| p.into_inner());
            if result
                .as_ref()
                .is_some_and(|(channel, _)| *channel == self.0)
            {
                result.take();
            }
        }
    }

    pub(in crate::services::discord::router::message_handler) fn set_result(
        channel: u64,
        result: String,
    ) -> ResultReset {
        assert!(
            INPUT_RESULT
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .replace((channel, result))
                .is_none()
        );
        ResultReset(channel)
    }

    pub(super) fn take_result(channel: u64) -> String {
        let mut result = INPUT_RESULT.lock().unwrap_or_else(|p| p.into_inner());
        if result
            .as_ref()
            .is_some_and(|(probed, _)| *probed == channel)
        {
            result.take().unwrap().1
        } else {
            "NO_REPLY".into()
        }
    }

    #[test]
    fn agy_discord_dispatch_preserves_turn_identity_policy_and_cancellation() {
        let cancel = Arc::new(CancelToken::new());
        let provider = ProviderKind::from_str("agy").unwrap();
        let tools = vec!["Read".to_string()];
        let mut turn = StreamingTurn {
            pool: None,
            shared: None,
            provider: &provider,
            prompt: "question",
            session_id: Some("conversation"),
            working_dir: "workspace",
            system_prompt: Some("role"),
            allowed_tools: &tools,
            cancel: Arc::clone(&cancel),
            codex_observation: None,
            tmux_session_name: None,
            teardown: None,
            host: &TurnHost::Tmux,
            channel_id: 42,
            model: Some("configured-model"),
            native_fast_mode: None,
            codex_goals: None,
            compact_percent: None,
            compact_lower_bound_tokens: 0,
            compact_token_limit: None,
            cache_ttl_minutes: None,
            dispatch_type: None,
            force_fresh: false,
            submission: None,
        };
        assert!(matches!(
            provider.legacy_streaming_dispatch_kind(),
            LegacyDispatchKind::StreamJsonCli(crate::services::provider::StreamJsonDialectId::Agy)
        ));
        let request = stream_json_request(&turn).expect("stream json request");
        assert_eq!(request.provider, ProviderKind::Antigravity);
        assert_eq!(request.prompt, "question");
        assert_eq!(request.system_prompt.as_deref(), Some("role"));
        assert_eq!(request.model.as_deref(), Some("configured-model"));
        assert_eq!(
            request.working_directory,
            std::path::PathBuf::from("workspace")
        );
        assert_eq!(
            request.session.as_ref().map(|session| session.as_str()),
            Some("conversation")
        );
        assert_eq!(
            request.tool_policy,
            ConfiguredToolPolicy::from_legacy_allowed_tools(&tools)
        );
        assert!(Arc::ptr_eq(request.cancel.as_ref().unwrap(), &cancel));
        turn.force_fresh = true;
        assert!(
            stream_json_request(&turn)
                .expect("fresh stream json request")
                .session
                .is_none()
        );
    }

    // A configured Claude channel is refused at the dispatch entry, before the capacity slot and any
    // driver: admission off, then O unready, then, once both pass, the unwired Herdr executor.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn configured_herdr_turn_reaches_no_tmux_or_process_driver_until_an_executor_is_wired_pg()
    {
        use crate::config::TestEnvVarGuard as Guard;
        use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
        use crate::services::herdr_admission::{Admission, force_for_test as force_admission};
        use crate::services::herdr_launch::{force_writer_accepts, o_store_for_test};
        use crate::services::tui_prompt_dedupe::binding_context::tests;
        use std::os::unix::fs::PermissionsExt;
        const CHANNEL: u64 = 1_490_141_479_707_086_938;
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let (root, _env) = tests::fixture_after_shared_test_env_lock();
        let _tmux = tests::fake_tmux(root.path());
        let claude = root.path().join("claude");
        let stub = "#!/bin/bash\nprintf '%s\\n' \"$*\" >> \"$AGENTDESK_ROOT_DIR/claude.calls\"\n";
        std::fs::write(&claude, stub).unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _bin = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CLAUDE_PATH", &claude);
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let _hosts = crate::config::session_hosts::force_for_test(
            Some("mac-mini"),
            &[(CHANNEL, "mac-mini")],
        );
        let provider = ProviderKind::Claude;
        let session_key = "claude/discord_0123456789abcdef/mac-mini:AgentDesk-claude-dash";
        let run = || async {
            let host = crate::services::turn_host::for_turn(
                Some(&pool),
                &provider,
                CHANNEL,
                Some(session_key),
            )
            .await;
            let (sender, _receiver) = std::sync::mpsc::channel();
            let turn = StreamingTurn {
                pool: Some(&pool),
                shared: None,
                provider: &provider,
                prompt: "question",
                session_id: None,
                working_dir: root.path().to_str().unwrap(),
                system_prompt: None,
                allowed_tools: &[],
                cancel: Arc::new(CancelToken::new()),
                codex_observation: None,
                tmux_session_name: Some("AgentDesk-claude-dash"),
                teardown: None,
                host: &host,
                channel_id: CHANNEL,
                model: None,
                native_fast_mode: None,
                codex_goals: None,
                compact_percent: None,
                compact_lower_bound_tokens: 0,
                compact_token_limit: None,
                cache_ttl_minutes: None,
                dispatch_type: None,
                force_fresh: false,
                submission: None,
            };
            tokio::task::block_in_place(|| {
                execute(turn, crate::services::discord::router::intake_dispatch::replay_admission::dormant_start("test").unwrap(), sender, None)
            })
        };

        let off = force_admission(Admission::new(false, None));
        assert_eq!(
            run().await.unwrap_err(),
            "herdr turn refused: admission_stopped(env)"
        );
        drop(off);
        let absent = root.path().join("no-stop-file");
        let _open = force_admission(Admission::new(true, Some(absent)));
        assert_eq!(
            run().await.unwrap_err(),
            "herdr turn refused: o_writer_not_ready"
        );
        let (store, era) = o_store_for_test(root.path(), &[CHANNEL]);
        let mut seeded = store.open_channel(&era, CHANNEL).unwrap().unwrap();
        seeded.set_binding_checkpoint(3).unwrap();
        let _owned =
            crate::services::tui_o::cutover::test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
        let _writer = force_writer_accepts(Some(true));
        assert_eq!(
            run().await.unwrap_err(),
            "herdr turn refused: executor_not_wired"
        );
        // The intake and headless entries' first judgements: only the intake one follows the switch.
        let judge = |intake: bool, on: Option<bool>| {
            let (pool, provider) = (&pool, &provider);
            async move {
                let _switch = crate::services::turn_host::force_switch_for_test(on);
                let key = || async { Some(session_key.to_owned()) };
                let pool = Some(pool);
                match intake {
                    true => {
                        use crate::services::turn_host::intake_refusal_before_turn as entry;
                        entry(pool, provider, CHANNEL, key).await
                    }
                    false => {
                        use crate::services::turn_host::refusal_before_turn as entry;
                        entry(pool, provider, CHANNEL, key).await
                    }
                }
            }
        };
        let not_wired = Some(HerdrRefusal::ExecutorNotWired);
        for off in [None, Some(false)] {
            assert_eq!(judge(true, off).await, not_wired, "intake {off:?}");
            assert_eq!(judge(false, off).await, not_wired, "headless {off:?}");
        }
        assert_eq!(judge(true, Some(true)).await, None, "intake on");
        assert_eq!(judge(false, Some(true)).await, not_wired, "headless on");
        for driver in ["tmux.calls", "claude.calls"] {
            assert!(!root.path().join(driver).exists(), "{driver} ran");
        }
        pool.close().await;
        db.drop().await;
    }
}
