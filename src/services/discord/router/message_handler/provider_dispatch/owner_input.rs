//! The owner branch: a channel registered with the input owner hands its input effect to the
//! owner's pump instead of a provider driver, and no refusal falls back to that driver.
use std::path::PathBuf;
use std::sync::Mutex;

use super::StreamingTurn;
use crate::services::claude_tui::{busy_inject, owner_input as scope};
use crate::services::discord::queue_io::input_owner::effect::{
    self, BusyClaude, BusyClaudeHerdr, Clock, Completion, PreparedPayload,
};
use crate::services::discord::queue_io::input_owner::origin::{Execution, InputOrigin};
use crate::services::discord::queue_io::input_owner::{pump, target};
use crate::services::session_host::HerdrTarget;
use crate::services::turn_host::TurnHost;
use crate::services::turn_orchestrator::ChannelMailboxHandle;

/// What the owner verified about a channel's target before its turn reached dispatch.
struct Registration {
    mailbox: ChannelMailboxHandle,
    execution: Execution,
    origin: InputOrigin,
    phase: target::Phase,
    /// The attempt as prepared; a created execution's own target and source replace these.
    attempt: serde_json::Value,
    sources: Vec<String>,
    transcript: PathBuf,
    /// The tmux program the busy pane is written through.
    program: PathBuf,
    /// A Herdr pane's gate and its physical writer `socket_path#pane_id`.
    herdr: Option<(HerdrTarget, String)>,
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

/// How dispatch goes on after the owner looked at the turn.
pub(super) enum Taken {
    /// The owner ran or refused the input; no driver runs.
    Done(Result<(), String>),
    /// The driver runs its launch, and the owner writes the created execution's first prompt.
    Launch(scope::LaunchGuard),
}

/// `None` leaves the turn to its driver: the channel is not registered, or its execution is not
/// one the owner takes.
pub(super) fn take(turn: &StreamingTurn<'_>) -> Option<Taken> {
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
        TurnHost::Refused(refusal) => return Some(Taken::Done(Err(refusal.to_string()))),
    };
    let route = target::route(
        registration.execution,
        registration.origin,
        turn.provider.as_str(),
        host,
        registration.phase,
    )?;
    // A driver that hands its observed execution over writes the prompt as the owner's effect.
    let wanted = match (route, turn.provider.as_str()) {
        (target::Route::Cold, "claude" | "codex") => Some(scope::Lineage::Created),
        (target::Route::HerdrIdle, "claude" | "codex") => Some(scope::Lineage::Existing),
        _ => None,
    };
    if let Some(wanted) = wanted {
        let hook = created(registration, turn.prompt.into(), wanted);
        return Some(Taken::Launch(scope::install_launch(hook)));
    }
    Some(Taken::Done(run(turn, route, registration)))
}

/// Reserves the first prompt of the execution its driver observed with `wanted` lineage on that
/// execution's own target, never the prepared one, and runs the driver's submit as the effect;
/// another lineage is refused before any reservation.
fn created(r: Registration, frame: String, wanted: scope::Lineage) -> scope::LaunchHook {
    Box::new(move |lineage, target, source, writer, submit| {
        if lineage != wanted {
            return Err(format!("NotSent(\"lineage_refused: {lineage:?}\")"));
        }
        let mut attempt = r.attempt;
        attempt["target"] = target;
        attempt["source"] = source;
        let prepared = PreparedPayload {
            frame,
            sources: r.sources,
            attempt,
        };
        let leaf = effect::created(submit, writer);
        let submitted = pump::submit(&r.mailbox, prepared, r.clock, leaf);
        match tokio::runtime::Handle::current().block_on(submitted)? {
            Completion::Submitted => Ok(()),
            other => Err(format!("{other:?}")),
        }
    })
}

fn run(turn: &StreamingTurn<'_>, route: target::Route, r: Registration) -> Result<(), String> {
    let nonce = r.attempt["nonce"].as_str().unwrap_or_default().to_string();
    let session = || turn.tmux_session_name.ok_or("owner input without a pane");
    let (frame, leaf) = match route {
        // The busy frame carries main's header, so the prepared bytes are the ones pasted.
        target::Route::ClaudeBusy => (
            busy_inject::frame("discord", &r.author, &r.name, &nonce, turn.prompt),
            BusyClaude {
                pane: busy_inject::Pane::with_program(session()?, r.program),
                session: session()?.into(),
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
        target::Route::ClaudeIdle => (turn.prompt.into(), effect::idle_claude(session()?.into())),
        target::Route::ClaudeHerdrBusy => {
            let (target, writer) = r.herdr.ok_or("owner Herdr input without a pane")?;
            (
                busy_inject::frame("discord", &r.author, &r.name, &nonce, turn.prompt),
                BusyClaudeHerdr {
                    target,
                    writer,
                    session: session()?.into(),
                    transcript: r.transcript,
                    channel: turn.channel_id,
                    source: "discord".into(),
                    author: r.author,
                    name: r.name,
                    nonce,
                    text: turn.prompt.into(),
                }
                .leaf(),
            )
        }
        // Every other create or idle pane belongs to a driver that does not hand it over yet.
        target::Route::Cold => return Err("NotSent(\"cold_launch_unowned\")".into()),
        target::Route::HerdrIdle => return Err("NotSent(\"idle_unowned\")".into()),
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
#[path = "owner_input_herdr_tests.rs"]
mod herdr_tests;
#[cfg(test)]
#[path = "owner_input_tests.rs"]
mod tests;
