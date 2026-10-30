//! The owner branch: a channel registered with the input owner hands its input effect to the
//! owner's pump instead of a provider driver, and no refusal falls back to that driver.
use std::path::PathBuf;
use std::sync::Mutex;

use super::StreamingTurn;
use crate::services::claude_tui::{busy_inject, owner_input as scope};
use crate::services::discord::queue_io::input_owner::drive::FrontClaim;
use crate::services::discord::queue_io::input_owner::effect::{
    self, BusyClaude, BusyClaudeHerdr, BusyCodex, BusyCodexHerdr, Clock, CodexInput, Completion,
    PreparedPayload,
};
use crate::services::discord::queue_io::input_owner::origin::{Execution, InputOrigin};
use crate::services::discord::queue_io::input_owner::{pump, target};
use crate::services::session_host::HerdrTarget;
use crate::services::turn_host::TurnHost;
use crate::services::turn_orchestrator::ChannelMailboxHandle;

/// What the owner verified about a channel's target before its turn reached dispatch.
pub(super) struct Registration {
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
    /// The provider's observer of the AI turn an idle submit starts; returns once that turn ends.
    until_done: Option<Box<dyn FnOnce() + Send>>,
}

/// The drive's answer for one front: its local settlement, never its AI completion.
pub(super) type Settled = tokio::sync::oneshot::Sender<Result<(), String>>;
type Reply = std::sync::Arc<dyn Fn(&Result<(), String>) + Send + Sync>;

fn reply_once(settled: Option<Settled>) -> Reply {
    let slot = Mutex::new(settled);
    std::sync::Arc::new(move |result| {
        if let Some(settled) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = settled.send(result.clone());
        }
    })
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
    take_owned(turn, None, registration, reply_once(None))
}

/// The owner's front handoff, from its arguments alone: the drive hears `settled` once the input
/// settled, while this thread runs on with the AI turn.
pub(super) fn execute_front(
    turn: StreamingTurn<'_>,
    sender: std::sync::mpsc::Sender<crate::services::agent_protocol::StreamMessage>,
    claim: FrontClaim,
    owner: Registration,
    settled: Settled,
) -> Result<(), String> {
    let reply = reply_once(Some(settled));
    let result = match take_owned(&turn, Some(claim), owner, reply.clone()) {
        Some(Taken::Done(result)) => result,
        Some(Taken::Launch(guard)) => guard.finish(super::drive(turn, sender)),
        None => Err("NotSent(\"not_owned\")".into()),
    };
    reply(&result);
    result
}

fn take_owned(
    turn: &StreamingTurn<'_>,
    front: Option<FrontClaim>,
    mut registration: Registration,
    reply: Reply,
) -> Option<Taken> {
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
        let hook = created(registration, turn.prompt.into(), wanted, (front, reply));
        return Some(Taken::Launch(scope::install_launch(hook)));
    }
    // An idle pane runs a new turn, so it holds the node's execution capacity until that AI turn
    // ends; a busy pane writes into the turn that already holds it.
    let until_done = registration.until_done.take();
    let capacity = match route {
        target::Route::ClaudeIdle | target::Route::CodexIdle => {
            let cancel = std::sync::Arc::clone(&turn.cancel);
            let (pool, provider) = (turn.pool, turn.provider.as_str());
            match crate::services::cluster::execution_capacity::acquire(
                pool,
                provider,
                turn.channel_id,
                cancel,
            ) {
                Ok(guard) => guard,
                Err(refused) => return Some(Taken::Done(Err(format!("NotSent({refused:?})")))),
            }
        }
        _ => None,
    };
    let idle = matches!(route, target::Route::ClaudeIdle | target::Route::CodexIdle);
    let result = run(turn, route, registration, front);
    reply(&result);
    if let (true, Ok(()), Some(until_done)) = (idle, &result, until_done) {
        until_done();
    }
    drop(capacity);
    Some(Taken::Done(result))
}

/// Submits on the owner's pump from the driver's blocking thread, for the granted front if any.
fn submit(
    (mailbox, clock): (&ChannelMailboxHandle, Clock),
    front: Option<FrontClaim>,
    prepared: PreparedPayload,
    leaf: effect::Leaf,
) -> Result<(), String> {
    let submitted = pump::submit_front(mailbox, front, prepared, clock, leaf);
    match tokio::runtime::Handle::current().block_on(submitted)? {
        Completion::Submitted => Ok(()),
        other => Err(format!("{other:?}")),
    }
}

/// Reserves the first prompt on the target of the execution its driver observed, runs the driver's
/// submit as the effect, and refuses any lineage but `wanted` before reserving.
fn created(
    r: Registration,
    frame: String,
    wanted: scope::Lineage,
    (front, reply): (Option<FrontClaim>, Reply),
) -> scope::LaunchHook {
    Box::new(move |lineage, target, source, writer, submit_created| {
        if lineage != wanted {
            return Err(format!("NotSent(\"lineage_refused: {lineage:?}\")"));
        }
        // A Codex Herdr write types its plan's normalized text, so that text is what is reserved.
        let frame = match (r.attempt["provider"].as_str(), target["host"].as_str()) {
            (Some("codex"), Some("herdr")) => codex_herdr_frame(&frame)
                .map_err(|error| format!("NotSent(\"invalid_prompt: {error}\")"))?,
            _ => frame,
        };
        let mut attempt = r.attempt;
        attempt["target"] = target;
        attempt["source"] = source;
        let prepared = PreparedPayload {
            frame,
            sources: r.sources,
            attempt,
        };
        let leaf = effect::created(submit_created, writer);
        let result = submit((&r.mailbox, r.clock), front, prepared, leaf);
        reply(&result);
        result
    })
}

/// The bytes a Codex write types for `prompt`: the text of its submit plan.
fn codex_herdr_frame(prompt: &str) -> Result<String, String> {
    use crate::services::codex_tui::input::{TuiInputAction, plan_prompt_submit};
    let plan = plan_prompt_submit(prompt)?;
    let typed = plan.iter().filter_map(|action| match action {
        TuiInputAction::Literal(text) | TuiInputAction::PasteBuffer(text) => Some(text.as_str()),
        TuiInputAction::Enter | TuiInputAction::Escape => None,
    });
    Ok(typed.collect())
}

fn run(
    turn: &StreamingTurn<'_>,
    route: target::Route,
    r: Registration,
    front: Option<FrontClaim>,
) -> Result<(), String> {
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
        target::Route::CodexIdle => (
            codex_herdr_frame(turn.prompt)
                .map_err(|error| format!("NotSent(\"invalid_prompt: {error}\")"))?,
            effect::idle_codex(
                busy_inject::Pane::with_program(session()?, r.program),
                session()?.into(),
            ),
        ),
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
        target::Route::CodexBusy | target::Route::CodexHerdrBusy => {
            // Main's busy preparation: the open model turn the paste is aimed at, read up front.
            use crate::services::codex_tui::busy_inject::{TIMING, TurnVerdict, read_turn};
            let target_turn = match read_turn(&r.transcript) {
                Some(TurnVerdict::KnownModelTurn(turn)) => turn,
                other => return Err(format!("NotSent(\"busy_turn: {other:?}\")")),
            };
            let frame = busy_inject::frame("discord", &r.author, &r.name, &nonce, turn.prompt);
            let input = CodexInput {
                session: session()?.into(),
                rollout: r.transcript,
                target_turn,
                channel: turn.channel_id,
                message: r.sources.first().and_then(|id| id.parse().ok()),
                source: "discord".into(),
                author: r.author,
                name: r.name,
                nonce,
                text: turn.prompt.into(),
            };
            let leaf = match (route, r.herdr) {
                (target::Route::CodexBusy, _) => BusyCodex {
                    pane: busy_inject::Pane::with_program(session()?, r.program),
                    timing: &TIMING,
                    input,
                }
                .leaf(),
                (_, Some((target, writer))) => BusyCodexHerdr {
                    target,
                    writer,
                    input,
                }
                .leaf(),
                (_, None) => return Err("owner Herdr input without a pane".into()),
            };
            (frame, leaf)
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
    submit((&r.mailbox, r.clock), front, prepared, leaf)
}

#[cfg(test)]
#[path = "owner_input_codex_tests.rs"]
mod codex_tests;
#[cfg(test)]
#[path = "owner_input_herdr_tests.rs"]
mod herdr_tests;
#[cfg(test)]
#[path = "owner_input_tests.rs"]
mod tests;
