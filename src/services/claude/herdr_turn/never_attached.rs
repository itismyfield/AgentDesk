//! A Pending execution no attach ever took: its turn is refused with why, and with the exit the
//! live terminate switch leaves. Only the launch probe may write, filling missing evidence once.

use tokio::runtime::Handle;

use super::{HerdrTurn, HerdrTurnPorts, launch, launched};
use crate::db::dispatched_sessions::hosted_execution::HostedExecution;
use crate::services::herdr_launch::{HerdrLaunchCommand, launch_herdr_session};
use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;

/// Probes the recorded pane of a Pending without evidence again; it never creates a pane.
pub(super) fn probe_again(
    turn: &HerdrTurn<'_>,
    runtime: &Handle,
    ports: &dyn HerdrTurnPorts,
) -> Result<String, String> {
    let host = ports.launch_host().ok_or("herdr turn: no launch host")?;
    let never = |_: &PreparedIncarnation| -> Result<HerdrLaunchCommand, String> {
        Err("a pending pane is never created again".into())
    };
    launched(runtime.block_on(launch_herdr_session(
        turn.pool,
        launch(turn, None, false),
        never,
        host,
    )))
}

/// Why the turn of execution `record`, whose session `session_id` logged no start, is refused.
pub(super) fn refusal(
    turn: &HerdrTurn<'_>,
    runtime: &Handle,
    ports: &dyn HerdrTurnPorts,
    record: &HostedExecution,
    session_id: &str,
) -> String {
    let nonce = &record.execution_nonce;
    let pane = record.location.as_ref().map(|location| &location.pane_id);
    let proven = match (&record.expected, pane) {
        (Some(_), _) => Ok(()),
        (None, Some(_)) => probe_again(turn, runtime, ports).map(drop),
        (None, None) => Err("no pane recorded".to_string()),
    };
    let terminate = crate::services::discord::herdr_terminate::terminate_enabled();
    let context = crate::services::platform::active_provider_context("claude");
    let session_key = context.and_then(|context| context.session_key);
    let key = session_key.as_deref().unwrap_or("<its session key>");
    tracing::warn!(
        event = "herdr_turn_never_attached",
        channel = turn.channel_id,
        session_key = key,
        nonce = nonce.as_str(),
        pane = ?pane,
        evidence = ?proven,
        terminate,
    );
    let exit = match (proven, pane) {
        (Ok(()), _) if terminate => format!(
            "end it with `agentdesk herdr terminate --session-key {key} --execution-nonce {nonce} \
             --reason never-attached` (keeps the queue) or /clear (drops the queue)"
        ),
        (Ok(()), _) => {
            "ending it needs runtime.herdr_terminate_enabled, which is off: termination \
             is disabled here (herdr runbook: never-attached execution)"
                .to_string()
        }
        (Err(gap), Some(pane)) => format!(
            "its provider in pane {pane} is not proven ({gap}); once that pane is closed, \
             `agentdesk herdr retire {}` frees the channel",
            turn.channel_id
        ),
        (Err(_), None) => "no pane was recorded and its create outcome is unknown, so it is \
             kept; nothing here ends it"
            .to_string(),
    };
    format!(
        "herdr turn: pending execution {nonce} never attached (no logged start of {session_id}); \
         {exit}"
    )
}
