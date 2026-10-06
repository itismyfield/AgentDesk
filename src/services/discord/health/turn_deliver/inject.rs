//! Busy-turn injection for human input: the kill switch, the vetoes read before any pane call,
//! and the hand-off of one deliver to `claude_tui::busy_inject`.

use std::path::PathBuf;
use std::sync::Arc;

use super::HumanInputRequest;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude_tui::busy_inject::{self, Outcome, Unconfirmed, Veto};
use crate::services::discord::SharedData;
use crate::services::discord::inflight::{InflightTurnState, TurnSource};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ChannelMailboxSnapshot;
use tokio::sync::OwnedMutexGuard;

/// `ADK_BUSY_INJECT` turns on busy-turn injection for turn/deliver, read once per process: `external`
/// for TUI-direct turns, `all` for Discord turns too; unset or any other value keeps it off.
pub(crate) const INJECT_ENV: &str = "ADK_BUSY_INJECT";

pub(super) const HOLDER_UNSUPPORTED: &str = "holder_unsupported";
pub(super) const QUEUE_NONEMPTY: &str = "queue_nonempty";
const SESSION_UNRESOLVED: &str = "session_unresolved";
pub(super) const TRANSITION_BUSY: &str = "transition_busy";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InjectMode {
    Off,
    External,
    All,
}

impl InjectMode {
    pub(super) fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("external") => Self::External,
            Some("all") => Self::All,
            _ => Self::Off,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum InjectAttempt {
    NotSent(&'static str),
    Injected {
        turn_id: Option<String>,
    },
    Unconfirmed {
        turn_id: Option<String>,
        detail: &'static str,
    },
}

/// Tests never read the process env; a channel injects only when a test forces it.
pub(super) fn mode(channel_id: u64) -> InjectMode {
    #[cfg(test)]
    return test_hook::forced(channel_id).map_or(InjectMode::Off, |(mode, _)| mode);
    #[cfg(not(test))]
    {
        static PROCESS: std::sync::OnceLock<InjectMode> = std::sync::OnceLock::new();
        let _ = channel_id;
        *PROCESS.get_or_init(|| InjectMode::parse(std::env::var(INJECT_ENV).ok().as_deref()))
    }
}

/// Holder vetoes on one mailbox snapshot and the channel's durable row; `Ok` names the busy turn.
pub(super) fn holder(
    mode: InjectMode,
    snapshot: &ChannelMailboxSnapshot,
    row: Option<&InflightTurnState>,
    channel_id: u64,
) -> Result<Option<String>, &'static str> {
    let row = row.ok_or(HOLDER_UNSUPPORTED)?;
    match (&snapshot.cancel_token, row.turn_source) {
        // A TUI-direct turn holds the channel through its row alone.
        (None, TurnSource::ExternalInput) => Ok(None),
        // A Discord turn whose own row is on disk.
        (Some(_), TurnSource::Managed)
            if mode == InjectMode::All
                && !snapshot.active_turn_kind.is_background()
                && snapshot
                    .active_user_message_id
                    .is_some_and(|id| id.get() == row.user_msg_id) =>
        {
            Ok(Some(format!("discord:{channel_id}:{}", row.user_msg_id)))
        }
        // Background, monitor and adopted holders, and a claim taken over a foreign row.
        _ => Err(HOLDER_UNSUPPORTED),
    }
}

/// Queued input, or a head dequeued but not yet claimed, must run before this one.
pub(super) fn backlog(snapshot: &ChannelMailboxSnapshot) -> Result<(), &'static str> {
    if snapshot.intervention_queue.is_empty() && snapshot.pending_user_dispatch.is_none() {
        Ok(())
    } else {
        Err(QUEUE_NONEMPTY)
    }
}

struct Target {
    session: String,
    transcript: PathBuf,
    turn_id: Option<String>,
    /// Held from the first mailbox read until the pane effect ends.
    transition: OwnedMutexGuard<()>,
}

async fn observe(
    shared: &SharedData,
    request: &HumanInputRequest,
) -> (ChannelMailboxSnapshot, Option<InflightTurnState>) {
    let snapshot = crate::services::discord::mailbox_snapshot(shared, request.channel_id).await;
    let row = crate::services::discord::inflight::load_inflight_state_read_only(
        &request.provider,
        request.channel_id.get(),
    );
    (snapshot, row)
}

/// Every veto that needs no pane call, in the documented order.
async fn resolve(
    shared: &Arc<SharedData>,
    request: &HumanInputRequest,
    mode: InjectMode,
) -> Result<Target, &'static str> {
    let channel = request.channel_id.get();
    if request.provider != ProviderKind::Claude {
        return Err("provider_unsupported");
    }
    // Held until the pane effect ends. Bounded waiters (claim handback, headless start, /clear,
    // /resume) give up after 3s; intake and kickoff never wait and fall back to their queue.
    let transition = shared
        .session_transition_lock(request.channel_id)
        .try_lock_owned()
        .map_err(|_| TRANSITION_BUSY)?;
    let (snapshot, row) = observe(shared, request).await;
    let turn_id = holder(mode, &snapshot, row.as_ref(), channel)?;
    if crate::services::tui_o::turn_mode::transcript_turns(channel) {
        return Err("turn_mode_unsupported");
    }
    backlog(&snapshot)?;
    let session = row
        .and_then(|row| row.tmux_session_name)
        .filter(|name| !name.trim().is_empty())
        .ok_or(SESSION_UNRESOLVED)?;
    let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&session)
        .filter(|binding| binding.runtime_kind == RuntimeHandoffKind::ClaudeTui)
        .ok_or(SESSION_UNRESOLVED)?;
    let deferred = crate::services::discord::host_defer_gate::channel_session_deferred;
    if deferred(shared, &request.provider, channel, &session).await {
        return Err(SESSION_UNRESOLVED);
    }
    // Paths that skip the transition may have queued or claimed while the lookup awaited.
    let (snapshot, row) = observe(shared, request).await;
    if holder(mode, &snapshot, row.as_ref(), channel)? != turn_id {
        return Err(HOLDER_UNSUPPORTED);
    }
    backlog(&snapshot)?;
    if row.and_then(|row| row.tmux_session_name).as_deref() != Some(session.as_str()) {
        return Err(SESSION_UNRESOLVED);
    }
    Ok(Target {
        transcript: PathBuf::from(binding.relay_output_path()),
        session,
        turn_id,
        transition,
    })
}

pub(super) async fn attempt(
    shared: &Arc<SharedData>,
    request: &HumanInputRequest,
    mode: InjectMode,
) -> InjectAttempt {
    let target = match resolve(shared, request, mode).await {
        Ok(target) => target,
        Err(veto) => return InjectAttempt::NotSent(veto),
    };
    let channel = request.channel_id.get();
    let pane = pane(channel, &target.session);
    let input = Arc::new(Input {
        channel,
        provider: request.provider.as_str().to_string(),
        source: request.source.clone(),
        author: request.author_id.to_string(),
        text: request.text.clone(),
        nonce: busy_inject::fresh_nonce(),
    });
    let (session, turn_id) = (target.session.clone(), target.turn_id.clone());
    // The effect outlives a cancelled request, so it keeps the transition and records its own alert.
    let effect = tokio::task::spawn_blocking({
        let input = input.clone();
        move || run(&pane, target, &input)
    });
    match effect.await {
        Ok(attempt) => attempt,
        // The effect panicked outside its catch_unwind or never started, so it recorded nothing.
        Err(_) => unconfirmed(&input, &session, turn_id, "executor_failed"),
    }
}

struct Input {
    channel: u64,
    provider: String,
    source: String,
    author: String,
    text: String,
    nonce: String,
}

fn run(pane: &busy_inject::Pane, target: Target, input: &Input) -> InjectAttempt {
    #[cfg(test)]
    test_hook::crash(input.channel);
    let Target {
        session,
        transcript,
        turn_id,
        transition,
    } = target;
    let nonce = &input.nonce;
    let request = busy_inject::Request {
        session: &session,
        transcript: &transcript,
        source: &input.source,
        author: &input.author,
        nonce,
        text: &input.text,
    };
    let inject = || busy_inject::inject(pane, &request, &busy_inject::TIMING);
    // A panic may come after the paste, so it is reported like any later failure.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(inject));
    let alert = |detail| unconfirmed(input, &session, turn_id.clone(), detail);
    let result = match outcome {
        Ok(Outcome::NotSent(veto)) => InjectAttempt::NotSent(veto_name(veto)),
        Ok(Outcome::Injected) => {
            tracing::info!(
                channel_id = input.channel,
                tmux_session = %session,
                nonce = %nonce,
                "busy turn took the human input"
            );
            InjectAttempt::Injected {
                turn_id: turn_id.clone(),
            }
        }
        Ok(Outcome::Unconfirmed(detail)) => alert(unconfirmed_name(detail)),
        Err(_) => alert("executor_failed"),
    };
    drop(transition);
    result
}

fn unconfirmed(
    input: &Input,
    session: &str,
    turn_id: Option<String>,
    detail: &'static str,
) -> InjectAttempt {
    let payload = serde_json::json!({
        "detail": detail,
        "nonce": input.nonce,
        "source": input.source,
        "tmux_session": session,
        "turn_id": turn_id,
    });
    tracing::warn!(
        channel_id = input.channel,
        %payload,
        "busy turn injection unconfirmed; nothing was queued"
    );
    crate::services::observability::events::record_simple(
        "busy_inject_unconfirmed",
        Some(input.channel),
        Some(&input.provider),
        payload,
    );
    InjectAttempt::Unconfirmed { turn_id, detail }
}

/// Tests reach only the scripted tmux their channel forced, never the real server.
fn pane(channel_id: u64, session: &str) -> busy_inject::Pane {
    #[cfg(test)]
    let program = test_hook::forced(channel_id).map_or_else(|| "agentdesk-no-tmux".into(), |f| f.1);
    #[cfg(not(test))]
    let program = {
        let _ = channel_id;
        PathBuf::from("tmux")
    };
    busy_inject::Pane::with_program(session, program)
}

fn veto_name(veto: Veto) -> &'static str {
    match veto {
        Veto::InvalidInput => "invalid_input",
        Veto::LockContended => "lock_contended",
        Veto::HumanAttached => "human_attached",
        Veto::AttachUnknown => "attach_unknown",
        Veto::PaneUnavailable => "pane_unavailable",
        Veto::Modal => "modal",
        Veto::Draft => "draft",
        Veto::NotBusy => "not_busy",
        Veto::TranscriptUnavailable => "transcript_unavailable",
        Veto::LoadFailed => "load_failed",
    }
}

fn unconfirmed_name(detail: Unconfirmed) -> &'static str {
    match detail {
        Unconfirmed::PasteFailed => "paste_failed",
        Unconfirmed::AttachedAfterPaste => "attached_after_paste",
        Unconfirmed::CaptureFailed => "capture_failed",
        Unconfirmed::ModalAfterPaste => "modal_after_paste",
        Unconfirmed::DraftNotOwned => "draft_not_owned",
        Unconfirmed::EnterFailed => "enter_failed",
        Unconfirmed::NotObserved => "not_observed",
    }
}

/// Per-channel mode and tmux program for tests, so parallel tests never share a switch.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::InjectMode;

    static FORCED: Mutex<Option<HashMap<u64, (InjectMode, PathBuf)>>> = Mutex::new(None);
    static CRASHING: Mutex<Vec<u64>> = Mutex::new(Vec::new());

    pub(crate) fn forced(channel_id: u64) -> Option<(InjectMode, PathBuf)> {
        let forced = FORCED.lock().unwrap_or_else(|e| e.into_inner());
        forced.as_ref()?.get(&channel_id).cloned()
    }

    pub(crate) fn set(channel_id: u64, mode: InjectMode, program: PathBuf) {
        let mut forced = FORCED.lock().unwrap_or_else(|e| e.into_inner());
        forced
            .get_or_insert_with(HashMap::new)
            .insert(channel_id, (mode, program));
    }

    pub(crate) fn clear(channel_id: u64) {
        let mut forced = FORCED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(map) = forced.as_mut() {
            map.remove(&channel_id);
        }
        let mut crashing = CRASHING.lock().unwrap_or_else(|e| e.into_inner());
        crashing.retain(|channel| *channel != channel_id);
    }

    /// Makes the channel's effect panic outside its own `catch_unwind`, as a dying executor would.
    pub(crate) fn crash_effect(channel_id: u64) {
        let mut crashing = CRASHING.lock().unwrap_or_else(|e| e.into_inner());
        crashing.push(channel_id);
    }

    pub(super) fn crash(channel_id: u64) {
        let crashing = CRASHING.lock().unwrap_or_else(|e| e.into_inner());
        let crash = crashing.contains(&channel_id);
        drop(crashing);
        assert!(!crash, "scripted executor crash");
    }
}
