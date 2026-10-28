//! Busy-turn injection for human input: the kill switch, the vetoes read before any pane call,
//! the mailbox order reservation, and the hand-off of one input to `claude_tui::busy_inject`, a
//! Herdr pane's gate (`herdr`) or, once enabled, `codex_tui::busy_inject`.

mod catch_up_race;
#[cfg(all(test, unix))]
mod first_input;
mod herdr;
mod names;
mod queued_owner;
mod queued_provider;
mod queued_reconcile;
mod queued_support;

use futures::FutureExt;
use std::path::PathBuf;
use std::sync::Arc;

use poise::serenity_prelude::{ChannelId, MessageId};

use super::HumanInputRequest;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude_tui::busy_inject::{self, Outcome, Receipt};
use crate::services::codex_tui::busy_inject as codex_inject;
use crate::services::discord::SharedData;
use crate::services::discord::inflight::{InflightTurnState, TurnSource};
use crate::services::discord::inject_disposition::{self, InjectionOutcome, SourceGuard};
use crate::services::discord::queue_io::queued_head_inject::{
    BindingKey, binding_key, persistence_failed, queued_head_enabled,
};
use crate::services::discord::router::live_intake_inject_covers;
use crate::services::imsg_reply::{self, ReplyObligation, episode};
use crate::services::provider::ProviderKind;
use crate::services::session_host::HerdrTarget;
use crate::services::turn_orchestrator::{
    ActiveTurnKind, ChannelMailboxSnapshot, ExpectedClaim, HANDBACK_NOT_WRITTEN, HeadOutcome,
    HeadSettle, InjectionSettlement, Intervention, QueueIdentity, QueuedHeadClaim, ReserveOutcome,
};
use names::{codex_unconfirmed_name, codex_veto_name, veto_name};
use queued_support::{HEAD_UNKNOWN, queued_target_matches};
use tokio::sync::OwnedMutexGuard;

/// `ADK_BUSY_INJECT` turns on busy-turn injection of human input into Claude TUI sessions, read
/// once per process: `external` or `all` opens it for any turn holder; anything else keeps it off.
pub(crate) const INJECT_ENV: &str = "ADK_BUSY_INJECT";

pub(super) const HOLDER_CHANGED: &str = "holder_changed";
pub(super) const INPUT_IN_FLIGHT: &str = "input_in_flight";
const MAILBOX_UNAVAILABLE: &str = "mailbox_unavailable";
const NOT_BUSY: &str = "not_busy";
const EPISODE_UNIDENTIFIED: &str = "episode_unidentified";
/// The message is already queued, held, injected or claimed by another path.
pub(crate) const SOURCE_OWNED: &str = "source_owned";
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
pub(crate) enum InjectAttempt {
    /// Nothing was reserved or sent; the caller's own start or queue follows.
    NotSent(&'static str),
    Injected {
        turn_id: Option<String>,
    },
    Unconfirmed {
        turn_id: Option<String>,
        detail: &'static str,
    },
    /// The pane vetoed after the reservation; the input took the queue front as `turn_id`.
    HandedBack {
        turn_id: String,
        veto: &'static str,
    },
    /// The pane vetoed or an admitted GUID stayed unbound, and the queue front was not written
    /// or its result is unknown.
    HandbackFailed(&'static str),
    /// The owner died outside the pane effect: nothing was recorded and the paste is unknown.
    OwnerFailed {
        turn_id: Option<String>,
    },
}

/// Where the input came from. A Discord message carries its id, the queue entry a vetoed paste
/// becomes, and the claim the owner holds on it until the injection ends.
pub(crate) enum Origin {
    External,
    /// An admitted iMessage: its paste carries the reply contract, and its GUID is bound to the
    /// holder's episode before any pane effect.
    Obligated(ReplyObligation),
    QueuedHead {
        expected: QueueIdentity,
        parent: Option<ChannelId>,
        gate: fn(ChannelId, Option<ChannelId>) -> bool,
    },
    Discord {
        message: MessageId,
        handback: Box<(Intervention, String)>,
        guard: SourceGuard,
        /// The author's name as Discord shows it for this message.
        name: String,
    },
}

impl Origin {
    pub(crate) fn queued_head_ready(provider: &ProviderKind, channel: ChannelId) -> bool {
        let supported = *provider == ProviderKind::Claude;
        #[cfg(test)]
        let supported = supported
            || (test_hook::codex_enabled(channel.get())
                && queued_provider::QueuedProvider::select(provider, channel.get()).is_some());
        !cfg!(windows)
            && supported
            && mode(channel.get()) != InjectMode::Off
            && queued_head_enabled(channel.get())
    }

    pub(crate) fn queued_head_gate_enabled(
        provider: &ProviderKind,
        channel: ChannelId,
        parent: Option<ChannelId>,
        gate: fn(ChannelId, Option<ChannelId>) -> bool,
    ) -> bool {
        Self::queued_head_ready(provider, channel) && gate(channel, parent)
    }
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

/// Codex capability is limited by the existing Discord channel gate below.
pub(super) const CODEX_BUSY_INJECT_ENABLED: bool = true;

/// Require the requested channel itself; intake permission from its parent is insufficient.
fn codex_enabled(channel_id: u64) -> bool {
    #[cfg(test)]
    if test_hook::codex_enabled(channel_id) {
        return true;
    }
    CODEX_BUSY_INJECT_ENABLED && live_intake_inject_covers(ChannelId::new(channel_id), None)
}

/// Who holds the channel at one read: the mailbox claim and the durable row.
pub(super) struct Holder {
    claim: ExpectedClaim,
    row: Option<RowKey>,
    pub(super) turn_id: Option<String>,
}

type RowKey = (TurnSource, u64, String, Option<u64>);

fn row_key(row: &InflightTurnState) -> RowKey {
    let started = row.started_at.clone();
    (
        row.turn_source,
        row.user_msg_id,
        started,
        row.turn_start_offset,
    )
}

/// Any holder takes input, except a claimed input whose own row is not on disk: that input has not
/// reached the pane, so later input waits behind it. `turn_id` names a row's Discord message.
pub(super) fn holder(
    snapshot: &ChannelMailboxSnapshot,
    row: Option<&InflightTurnState>,
    channel_id: u64,
) -> Result<Holder, &'static str> {
    let message = snapshot.active_user_message_id;
    let claim = snapshot
        .cancel_token
        .clone()
        .map(|token| (token, snapshot.active_turn_kind, message));
    let own_row = row
        .zip(message)
        .is_some_and(|(row, message)| row.user_msg_id == message.get());
    if claim.is_some() && snapshot.active_turn_kind == ActiveTurnKind::UserOrAgent && !own_row {
        return Err(INPUT_IN_FLIGHT);
    }
    let turn_id = row
        .filter(|row| row.turn_source != TurnSource::ExternalInput && row.user_msg_id != 0)
        .map(|row| format!("discord:{channel_id}:{}", row.user_msg_id));
    let row = row.map(row_key);
    Ok(Holder {
        claim,
        row,
        turn_id,
    })
}

/// Queued input that is not parked, or a head dequeued but not yet claimed, must run before
/// this one; the owner's reservation decides again on its own state.
pub(super) fn backlog(
    snapshot: &ChannelMailboxSnapshot,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<(), &'static str> {
    use crate::services::discord::busy_followup_retry_store::{input_class, parked_only};
    let queue = snapshot.intervention_queue.iter();
    let classes: Vec<_> = queue
        .map(|item| input_class(provider, channel, item))
        .collect();
    if snapshot.pending_user_dispatch.is_none() && parked_only(&classes) {
        Ok(())
    } else {
        Err(QUEUE_NONEMPTY)
    }
}

struct Target {
    session: String,
    transcript: PathBuf,
    /// The Codex model turn the rollout read as open; None for Claude.
    native_turn: Option<String>,
    turn_id: Option<String>,
    /// The mailbox claim the holder check judged; the reservation requires it unchanged.
    claim: ExpectedClaim,
    /// Held from the first mailbox read until the owner settles.
    transition: OwnedMutexGuard<()>,
    row: Option<RowKey>,
    binding: Option<BindingKey>,
    /// The gated pane of a Herdr-hosted session; the effect then never calls tmux.
    herdr: Option<HerdrTarget>,
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

/// The channel's name, read under the core lock.
async fn channel_name(shared: &SharedData, channel: ChannelId) -> Option<String> {
    let data = shared.core.lock().await;
    let session = data.sessions.get(&channel);
    session.and_then(|session| session.channel_name.clone())
}

/// The channel's `runtime` pane and transcript. A row stamped with another runtime, or a first
/// bound candidate (watcher, row, channel name) of another runtime, means the session is not it.
pub(super) fn tui_session(
    provider: &ProviderKind,
    runtime: RuntimeHandoffKind,
    row: Option<&InflightTurnState>,
    watcher: Option<String>,
    named: Option<String>,
) -> Option<(String, PathBuf)> {
    let kind = row.and_then(|row| row.runtime_kind);
    if kind.is_some_and(|kind| kind != runtime) {
        return None;
    }
    let candidates = [
        watcher,
        row.and_then(|row| row.tmux_session_name.clone()),
        named.map(|name| provider.build_tmux_session_name(&name)),
    ];
    let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session;
    let (session, binding) = candidates
        .into_iter()
        .flatten()
        .find_map(|session| binding(&session).map(|binding| (session, binding)))?;
    let output = match provider {
        ProviderKind::Codex => binding.output_path.as_str(),
        _ => binding.relay_output_path(),
    };
    (binding.runtime_kind == runtime).then(|| (session, PathBuf::from(output)))
}

fn live_tui_session(
    shared: &SharedData,
    request: &HumanInputRequest,
    runtime: RuntimeHandoffKind,
    row: Option<&InflightTurnState>,
    named: Option<String>,
) -> Option<(String, PathBuf)> {
    let watcher = shared.tmux_watchers.channel_binding(&request.channel_id);
    let watcher = watcher.map(|binding| binding.tmux_session_name);
    tui_session(&request.provider, runtime, row, watcher, named)
}

// Only confirmed transcript-output Claude channels on tmux can submit without a busy veto.
fn turn_mode_tmux(channel: u64) -> bool {
    crate::services::tui_o::turn_mode::transcript_turns(channel)
        && crate::config::session_hosts::herdr_endpoint(channel).is_none()
}

/// Whether the transcript shows a turn the input can join: a live Claude turn, or the open Codex
/// model turn, which the paste is then aimed at.
fn busy_turn(
    runtime: RuntimeHandoffKind,
    transcript: &std::path::Path,
) -> Result<Option<String>, &'static str> {
    if runtime == RuntimeHandoffKind::ClaudeTui {
        let turn = crate::services::tui_turn_state::observe_claude_jsonl_turn_state(transcript);
        return if turn.is_busy() {
            Ok(None)
        } else {
            Err(NOT_BUSY)
        };
    }
    match codex_inject::read_turn(transcript) {
        Some(codex_inject::TurnVerdict::KnownModelTurn(turn)) => Ok(Some(turn)),
        Some(codex_inject::TurnVerdict::NotBusy) => Err(NOT_BUSY),
        Some(codex_inject::TurnVerdict::NonSteerable(_)) => Err("not_steerable"),
        Some(codex_inject::TurnVerdict::Unknown) | None => Err("turn_unknown"),
    }
}

impl SharedData {
    /// Checks the holder or bound transcript without preparing uploads or touching the pane.
    pub(crate) async fn has_inject_turn(
        &self,
        provider: &ProviderKind,
        channel: ChannelId,
    ) -> bool {
        if crate::services::discord::mailbox_snapshot(self, channel)
            .await
            .cancel_token
            .is_some()
        {
            return true;
        }
        let runtime = match provider {
            ProviderKind::Claude => RuntimeHandoffKind::ClaudeTui,
            ProviderKind::Codex => RuntimeHandoffKind::CodexTui,
            _ => return false,
        };
        let row = crate::services::discord::inflight::load_inflight_state_read_only(
            provider,
            channel.get(),
        );
        let watcher = self.tmux_watchers.channel_binding(&channel);
        let named = channel_name(self, channel).await;
        tui_session(
            provider,
            runtime,
            row.as_ref(),
            watcher.map(|binding| binding.tmux_session_name),
            named,
        )
        .is_some_and(|(_, transcript)| {
            (runtime == RuntimeHandoffKind::ClaudeTui && turn_mode_tmux(channel.get()))
                || busy_turn(runtime, &transcript).is_ok()
        })
    }
}

/// Every veto that needs no pane call, in the documented order.
async fn resolve(
    shared: &Arc<SharedData>,
    request: &HumanInputRequest,
    queued: bool,
) -> Result<Target, &'static str> {
    let channel = request.channel_id.get();
    let runtime = match request.provider {
        ProviderKind::Claude => RuntimeHandoffKind::ClaudeTui,
        ProviderKind::Codex if codex_enabled(channel) => RuntimeHandoffKind::CodexTui,
        _ => return Err("provider_unsupported"),
    };
    // A session that resolves to no TUI pane of that runtime stops here, before acquiring the
    // session-transition guard or issuing pane I/O.
    let read = crate::services::discord::inflight::load_inflight_state_read_only;
    let row = read(&request.provider, channel);
    let named = channel_name(shared, request.channel_id).await;
    let live = |row: Option<&InflightTurnState>, named| {
        live_tui_session(shared, request, runtime, row, named)
    };
    let pane = live(row.as_ref(), named).ok_or(SESSION_UNRESOLVED)?;
    let binding = binding_key(&pane.0);
    // Transcript-output Claude follows the resulting native turn, whether idle or busy now.
    let native_turn = if runtime == RuntimeHandoffKind::ClaudeTui && turn_mode_tmux(channel) {
        None
    } else {
        busy_turn(runtime, &pane.1)?
    };
    // Held until the pane effect ends. Bounded waiters (claim handback, headless start, /clear,
    // /resume) give up after 3s; intake and kickoff never wait and fall back to their queue.
    let transition = shared
        .session_transition_lock(request.channel_id)
        .try_lock_owned()
        .map_err(|_| TRANSITION_BUSY)?;
    let (snapshot, row) = observe(shared, request).await;
    let first = holder(&snapshot, row.as_ref(), channel)?;
    if !queued {
        backlog(&snapshot, &request.provider, request.channel_id)?;
    }
    // A host the check keeps off tmux takes input only as its Bound Herdr execution's pane.
    let deferred = crate::services::discord::host_defer_gate::channel_session_deferred;
    let herdr = if deferred(shared, &request.provider, channel, &pane.0).await {
        let herdr = herdr::bound_pane(shared, &request.provider, channel, &pane.0).await;
        Some(herdr.ok_or(SESSION_UNRESOLVED)?)
    } else {
        None
    };
    // Paths that skip the transition may queue or claim while these lookups wait; the owner's
    // reservation checks the claim and the queue after them, and the row is read last here.
    #[cfg(test)]
    test_hook::final_name_lookup(channel);
    let named = channel_name(shared, request.channel_id).await;
    let row = read(&request.provider, channel);
    if live(row.as_ref(), named).as_ref() != Some(&pane)
        || (queued && binding_key(&pane.0) != binding)
    {
        return Err(SESSION_UNRESOLVED);
    }
    if row.as_ref().map(row_key) != first.row {
        return Err(HOLDER_CHANGED);
    }
    let (session, transcript) = pane;
    Ok(Target {
        session,
        transcript,
        native_turn,
        turn_id: first.turn_id,
        claim: first.claim,
        transition,
        row: first.row,
        binding,
        herdr,
    })
}

/// Checks the existing effect-free target predicate before origin admission.
pub(super) async fn external_ready(shared: &Arc<SharedData>, request: &HumanInputRequest) -> bool {
    resolve(shared, request, false).await.is_ok()
}

pub(crate) async fn attempt(
    shared: &Arc<SharedData>,
    request: &HumanInputRequest,
    origin: Origin,
) -> InjectAttempt {
    let origin = match catch_up_race::own_queued_head(shared, request, origin).await {
        Ok(attempt) => return attempt,
        Err(origin) => origin,
    };
    if let Origin::QueuedHead {
        expected,
        parent,
        gate,
    } = origin
    {
        if !Origin::queued_head_gate_enabled(&request.provider, request.channel_id, parent, gate) {
            return InjectAttempt::NotSent("queued_gate_off");
        }
        return queued_owner::queued_attempt(shared, request, expected, parent, gate, None).await;
    }
    let target = match resolve(shared, request, false).await {
        Ok(target) => target,
        Err(veto) => return InjectAttempt::NotSent(veto),
    };
    let channel = request.channel_id.get();
    let reply = match &origin {
        Origin::Obligated(reply) => match busy_episode(request, &target) {
            Some(episode) => Some((reply.clone(), episode)),
            None => return InjectAttempt::NotSent(EPISODE_UNIDENTIFIED),
        },
        _ => None,
    };
    let text = match &reply {
        Some((reply, _)) => reply.frame(&request.text),
        None => request.text.clone(),
    };
    // An external input names its author as its idle header does: the source and the id.
    let name = match &origin {
        Origin::Discord { name, .. } => name.clone(),
        _ => format!("{}:{}", request.source, request.author_id),
    };
    let input = Arc::new(Input {
        channel,
        provider: request.provider.as_str().to_string(),
        source: request.source.clone(),
        author: request.author_id.to_string(),
        name,
        text,
        nonce: busy_inject::fresh_nonce(),
    });
    let (session, turn_id) = (target.session.clone(), target.turn_id.clone());
    let (message, handback, guard) = match origin {
        Origin::External => (None, request.queue_entry(None), None),
        Origin::Obligated(reply) => (None, request.queue_entry(Some(&reply)), None),
        Origin::Discord {
            message,
            handback,
            guard,
            ..
        } => (Some(message), *handback, Some(guard)),
        Origin::QueuedHead { .. } => unreachable!("queued origin has its own owner"),
    };
    let owner = Owner {
        shared: shared.clone(),
        provider: request.provider.clone(),
        channel_id: request.channel_id,
        message,
        handback,
        guard,
        reply,
    };
    // The owner outlives a cancelled request: it settles the reservation and any handback itself.
    match tokio::spawn(owner.run(target, input.clone())).await {
        Ok(attempt) => attempt,
        // The owner panicked; its reservation is an orphan and its paste is unknown.
        Err(_) => {
            let _ = unconfirmed(&input, &session, turn_id.clone(), "executor_failed");
            InjectAttempt::OwnerFailed { turn_id }
        }
    }
}

/// The episode of the row the target verified; a holder that names no single turn has none.
fn busy_episode(request: &HumanInputRequest, target: &Target) -> Option<String> {
    let (_, user_msg_id, started_at, start_offset) = target.row.as_ref()?;
    let holder = episode::Holder {
        session: &target.session,
        user_msg_id: *user_msg_id,
        started_at,
        start_offset: *start_offset,
        native_turn: target.native_turn.as_deref(),
    };
    episode::busy(&request.provider, request.channel_id.get(), &holder)
}

struct Owner {
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    /// The Discord message the input is, whose injection is recorded so it never replays.
    message: Option<MessageId>,
    /// The queue entry a vetoed paste becomes, and the turn id that names it.
    handback: (Intervention, String),
    /// Dropped before the result returns, so a caller that falls back to its own intake is not
    /// refused by this claim.
    guard: Option<SourceGuard>,
    /// An admitted iMessage's obligation and the holder episode its paste joins.
    reply: Option<(ReplyObligation, String)>,
}

impl Owner {
    async fn run(self, target: Target, input: Arc<Input>) -> InjectAttempt {
        let Owner {
            shared,
            provider,
            channel_id,
            message,
            handback: (handback, queued_turn),
            guard,
            reply,
        } = self;
        let Target {
            session,
            transcript,
            native_turn,
            turn_id,
            claim,
            transition,
            herdr,
            ..
        } = target;
        let mailbox = shared.mailbox(channel_id);
        let discord = crate::services::discord::queue_persistence_context;
        let persistence = discord(&shared, &provider, channel_id);
        #[cfg(test)]
        test_hook::before_reserve(input.channel).await;
        let reserve = mailbox.reserve_injection(message, claim, persistence.clone());
        let ticket = match reserve.await {
            ReserveOutcome::Reserved(ticket) => ticket,
            ReserveOutcome::Owned | ReserveOutcome::Consumed => {
                return InjectAttempt::NotSent(SOURCE_OWNED);
            }
            ReserveOutcome::HolderChanged => return InjectAttempt::NotSent(HOLDER_CHANGED),
            ReserveOutcome::Backlog => return InjectAttempt::NotSent(QUEUE_NONEMPTY),
            ReserveOutcome::Unavailable => return InjectAttempt::NotSent(MAILBOX_UNAVAILABLE),
        };
        #[cfg(test)]
        test_hook::owner_crash(input.channel);
        // The GUID names the holder's episode, with this paste's nonce, before the pane can take it.
        let bound = match &reply {
            Some((reply, episode)) => reply.bind_episode(episode, Some(&input.nonce)).await,
            None => true,
        };
        let pane = pane(input.channel, &session);
        let effect = tokio::task::spawn_blocking({
            let (input, session, turn_id) = (input.clone(), session.clone(), turn_id.clone());
            move || match native_turn {
                _ if !bound => InjectAttempt::NotSent(imsg_reply::UNBOUND),
                None => run_received(
                    (&pane, herdr.as_ref()),
                    &session,
                    &transcript,
                    turn_id,
                    &input,
                ),
                Some(native) => {
                    let target = (session.as_str(), transcript.as_path(), native.as_str());
                    run_codex(&pane, target, turn_id, message, &input)
                }
            }
        });
        let attempt = match effect.await {
            Ok(attempt) => attempt,
            // The effect panicked outside its catch_unwind or never started, so it recorded nothing.
            Err(_) => unconfirmed(&input, &session, turn_id, "executor_failed"),
        };
        // A refused paste leaves its episode before its queue entry exists; unbound, nothing queues.
        let rebound = match (&attempt, &reply) {
            (InjectAttempt::NotSent(_), Some((reply, episode))) => {
                let queued = handback.message_id.get();
                bound && reply.hand_back(episode, &input.nonce, queued).await
            }
            _ => true,
        };
        let attempt = match attempt {
            // The dropped ticket's reservation counts as gone; nothing reached the pane or queue.
            InjectAttempt::NotSent(_) if !rebound => {
                drop(ticket);
                InjectAttempt::HandbackFailed(imsg_reply::UNBOUND)
            }
            InjectAttempt::NotSent(veto) => {
                let source = [handback.message_id.get()];
                let handed = mailbox.hand_back_injected_input(ticket, handback, persistence);
                #[cfg(test)]
                let handed = test_hook::hand_back(input.channel, handed);
                match handed.await {
                    Ok(events) => {
                        let apply = crate::services::discord::apply_queue_exit_feedback;
                        apply(&shared, channel_id, &events).await;
                        InjectAttempt::HandedBack {
                            turn_id: queued_turn,
                            veto,
                        }
                    }
                    Err(reason) => {
                        if reason == HANDBACK_NOT_WRITTEN {
                            let notice =
                                crate::services::discord::queue_io::input_persistence_refusal_notice;
                            notice(&shared, channel_id, &source).await;
                        }
                        InjectAttempt::HandbackFailed(reason)
                    }
                }
            }
            delivered => {
                let outcome = match delivered {
                    InjectAttempt::Injected { .. } => InjectionOutcome::Observed,
                    _ => InjectionOutcome::Unconfirmed,
                };
                let settle = InjectionSettlement::Delivered(outcome);
                let _ = mailbox
                    .settle_injected_input(ticket, settle, persistence)
                    .await;
                // Recorded whatever the settle answered, so a restart still finds it.
                if let Some(message) = message {
                    if outcome == InjectionOutcome::Observed {
                        catch_up_race::confirmed(
                            &shared,
                            &provider,
                            channel_id,
                            &handback.source_message_queued_generations(),
                        )
                        .await;
                    } else {
                        record_terminal(&provider, channel_id, message, outcome).await;
                    }
                }
                delivered
            }
        };
        // A handed-back message now waits in the queue, so the drain must not see it claimed.
        drop((guard, transition));
        // Ending the reservation may release a drain it withheld.
        let kick = crate::services::discord::queue_io::schedule_post_enqueue_idle_queue_kick;
        kick(shared, provider, channel_id);
        attempt
    }
}

/// Records an injected message in the provider file; a failure only loses replay protection.
async fn record_terminal(
    provider: &ProviderKind,
    channel: ChannelId,
    message: MessageId,
    outcome: InjectionOutcome,
) {
    let (provider, now_ms) = (provider.clone(), chrono::Utc::now().timestamp_millis());
    let record =
        move || inject_disposition::record_terminal(&provider, channel, message, outcome, now_ms);
    match tokio::task::spawn_blocking(record).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "injected input not recorded"),
        Err(error) => tracing::warn!(%error, "injected input record panicked"),
    }
}

struct Input {
    channel: u64,
    provider: String,
    source: String,
    author: String,
    name: String,
    text: String,
    nonce: String,
}

/// The queued-head effect: its transaction is terminal once Enter is sent.
fn run(
    pane: (&busy_inject::Pane, Option<&HerdrTarget>),
    session: &str,
    transcript: &std::path::Path,
    turn_id: Option<String>,
    input: &Input,
) -> InjectAttempt {
    run_with(pane, (session, transcript), turn_id, input, false)
}

/// The live owner's effect: a paste the transcript never received stays Unconfirmed.
fn run_received(
    pane: (&busy_inject::Pane, Option<&HerdrTarget>),
    session: &str,
    transcript: &std::path::Path,
    turn_id: Option<String>,
    input: &Input,
) -> InjectAttempt {
    run_with(pane, (session, transcript), turn_id, input, true)
}

fn run_with(
    (pane, herdr): (&busy_inject::Pane, Option<&HerdrTarget>),
    (session, transcript): (&str, &std::path::Path),
    turn_id: Option<String>,
    input: &Input,
    received: bool,
) -> InjectAttempt {
    #[cfg(test)]
    test_hook::crash(input.channel);
    if let Some(herdr) = herdr {
        return herdr::run(herdr, session, transcript, turn_id, input);
    }
    let nonce = &input.nonce;
    let request = busy_inject::Request {
        session,
        transcript,
        source: &input.source,
        author: &input.author,
        name: &input.name,
        nonce,
        text: &input.text,
    };
    let inject = || {
        let report = if turn_mode_tmux(input.channel) {
            busy_inject::inject_report_any_turn
        } else {
            busy_inject::inject_report
        };
        let report = report(pane, &request, Some(input.channel), &busy_inject::TIMING);
        let confirm = || busy_inject::confirm(pane, &request, &report, &busy_inject::TIMING);
        (report.outcome, received.then(confirm).flatten())
    };
    // A panic may come after the paste, so it is reported like any later failure.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(inject));
    let alert = |detail| unconfirmed(input, session, turn_id.clone(), detail);
    let result = match outcome {
        Ok((Outcome::NotSent(veto), _)) => InjectAttempt::NotSent(veto_name(veto)),
        // Neither window held a receipt, even after any finishing Enter; nothing is resent.
        Ok((Outcome::Injected, Some(Receipt::Missing { .. }))) => alert("not_observed"),
        Ok((Outcome::Injected, _)) => {
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
        Ok((Outcome::Failed(detail), _)) => alert(match detail {
            busy_inject::WriteFailure::Stash => "stash_failed",
            busy_inject::WriteFailure::Paste => "paste_failed",
            busy_inject::WriteFailure::Enter => "enter_failed",
        }),
        Err(_) => alert("executor_failed"),
    };
    result
}

/// Tells the dedupe observers the nonce is this Discord message, aimed at the target turn.
struct CodexLedger<'a> {
    session: &'a str,
    nonce: &'a str,
    target_turn: &'a str,
    message: Option<MessageId>,
}

impl codex_inject::Ledger for CodexLedger<'_> {
    fn register(&self) -> bool {
        let message = self.message.map(MessageId::get);
        let register = crate::services::tui_prompt_dedupe::register_injected_steer;
        register("codex", self.session, self.nonce, self.target_turn, message)
    }

    fn withdraw(&self) {
        let withdraw = crate::services::tui_prompt_dedupe::withdraw_injected_steer;
        withdraw("codex", self.session, self.nonce);
    }
}

/// The Codex pane effect: `target` is the session, its rollout and the model turn read as open.
fn run_codex(
    pane: &busy_inject::Pane,
    (session, rollout, target_turn): (&str, &std::path::Path, &str),
    turn_id: Option<String>,
    message: Option<MessageId>,
    input: &Input,
) -> InjectAttempt {
    #[cfg(test)]
    test_hook::crash(input.channel);
    let nonce = &input.nonce;
    let request = codex_inject::Request {
        session,
        rollout,
        source: &input.source,
        author: &input.author,
        name: &input.name,
        nonce,
        text: &input.text,
        target_turn,
    };
    let ledger = CodexLedger {
        session,
        nonce,
        target_turn,
        message,
    };
    let inject = || codex_inject::inject(pane, &request, &ledger, &codex_inject::TIMING);
    // A panic may come after the paste, so it is reported like any later failure.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(inject));
    let alert = |detail| unconfirmed(input, session, turn_id.clone(), detail);
    match outcome {
        Ok(codex_inject::Outcome::NotSent(veto)) => InjectAttempt::NotSent(codex_veto_name(veto)),
        Ok(codex_inject::Outcome::Injected) => {
            tracing::info!(
                channel_id = input.channel,
                tmux_session = %session,
                nonce = %nonce,
                target_turn,
                "busy Codex paste and Enter sent"
            );
            InjectAttempt::Injected { turn_id }
        }
        Ok(codex_inject::Outcome::Unconfirmed(detail)) => alert(codex_unconfirmed_name(detail)),
        Err(_) => alert("executor_failed"),
    }
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

/// Per-channel mode and tmux program for tests, so parallel tests never share a switch.
#[cfg(test)]
pub(crate) mod test_hook;

#[cfg(all(test, unix))]
mod catch_up_race_tests;
