//! The owner branch: a channel registered with the input owner hands its input effect to the
//! owner's pump instead of a provider driver, and no refusal falls back to that driver.
use std::path::PathBuf;
use std::sync::Mutex;

use super::StreamingTurn;
use crate::services::claude_tui::busy_inject;
use crate::services::discord::queue_io::input_owner::effect::{
    self, BusyClaude, Clock, Completion, PreparedPayload,
};
use crate::services::discord::queue_io::input_owner::origin::{Execution, InputOrigin};
use crate::services::discord::queue_io::input_owner::{pump, target};
use crate::services::turn_host::TurnHost;
use crate::services::turn_orchestrator::ChannelMailboxHandle;

/// What the owner verified about a channel's target before its turn reached dispatch.
struct Registration {
    mailbox: ChannelMailboxHandle,
    execution: Execution,
    origin: InputOrigin,
    phase: target::Phase,
    attempt: serde_json::Value,
    sources: Vec<String>,
    transcript: PathBuf,
    /// The tmux program the busy pane is written through.
    program: PathBuf,
    author: String,
    name: String,
    clock: Clock,
    timing: &'static busy_inject::Timing,
}

static REGISTERED: Mutex<Vec<(u64, Registration)>> = Mutex::new(Vec::new());

/// The next turn on `channel` goes to the owner.
fn register(channel: u64, registration: Registration) {
    let mut registered = REGISTERED.lock().unwrap_or_else(|e| e.into_inner());
    registered.retain(|(id, _)| *id != channel);
    registered.push((channel, registration));
}

/// `None` leaves the turn to its driver: the channel is not registered, or its execution is not
/// one the owner takes.
pub(super) fn take(turn: &StreamingTurn<'_>) -> Option<Result<(), String>> {
    let registration = {
        let mut registered = REGISTERED.lock().unwrap_or_else(|e| e.into_inner());
        let at = registered
            .iter()
            .position(|(id, _)| *id == turn.channel_id)?;
        registered.remove(at).1
    };
    let host = match turn.host {
        TurnHost::Tmux => "tmux",
        TurnHost::Herdr(_) => "herdr",
        TurnHost::Refused(refusal) => return Some(Err(refusal.to_string())),
    };
    let route = target::route(
        registration.execution,
        registration.origin,
        turn.provider.as_str(),
        host,
        registration.phase,
    )?;
    Some(run(turn, route, registration))
}

fn run(turn: &StreamingTurn<'_>, route: target::Route, r: Registration) -> Result<(), String> {
    let session = turn.tmux_session_name.ok_or("owner input without a pane")?;
    let nonce = r.attempt["nonce"].as_str().unwrap_or_default().to_string();
    let (frame, leaf) = match route {
        // The busy frame carries main's header, so the prepared bytes are the ones pasted.
        target::Route::ClaudeBusy => (
            busy_inject::frame("discord", &r.author, &r.name, &nonce, turn.prompt),
            BusyClaude {
                pane: busy_inject::Pane::with_program(session, r.program),
                session: session.into(),
                transcript: r.transcript,
                channel: turn.channel_id,
                source: "discord".into(),
                author: r.author,
                name: r.name,
                nonce,
                text: turn.prompt.into(),
                timing: r.timing,
            }
            .leaf(),
        ),
        target::Route::ClaudeIdle => (turn.prompt.into(), effect::idle_claude(session.into())),
        // The create belongs to the provider's launch, which does not hand its first prompt over.
        target::Route::Cold => return Err("NotSent(\"cold_launch_unowned\")".into()),
        target::Route::Held(reason) => return Err(format!("NotSent({reason:?})")),
    };
    let prepared = PreparedPayload {
        frame,
        sources: r.sources,
        attempt: r.attempt,
    };
    let submitted = pump::submit(&r.mailbox, prepared, r.clock, leaf);
    match tokio::runtime::Handle::current().block_on(submitted)? {
        Completion::Submitted => Ok(()),
        other => Err(format!("{other:?}")),
    }
}

#[cfg(test)]
#[path = "owner_input_tests.rs"]
mod tests;
