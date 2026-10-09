//! Busy-turn injection for human input: the kill switch, the vetoes read before any pane call,
//! the mailbox order reservation, and the hand-off of one input to `claude_tui::busy_inject` or,
//! once enabled, `codex_tui::busy_inject`.

use std::path::PathBuf;
use std::sync::Arc;

use poise::serenity_prelude::{ChannelId, MessageId};

use super::HumanInputRequest;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude_tui::busy_inject::{self, Outcome, Unconfirmed, Veto};
use crate::services::codex_tui::busy_inject as codex_inject;
use crate::services::discord::SharedData;
use crate::services::discord::inflight::{InflightTurnState, TurnSource};
use crate::services::discord::inject_disposition::{self, InjectionOutcome, SourceGuard};
use crate::services::discord::input_runtime::fence;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{
    ActiveTurnKind, ChannelMailboxSnapshot, ExpectedClaim, HANDBACK_NOT_WRITTEN,
    InjectionSettlement, Intervention, ReserveOutcome,
};
use tokio::sync::OwnedMutexGuard;

/// `ADK_BUSY_INJECT` turns on busy-turn injection of human input into Claude TUI sessions, read
/// once per process: `external` or `all` opens it for any turn holder; anything else keeps it off.
pub(crate) const INJECT_ENV: &str = "ADK_BUSY_INJECT";

pub(super) const HOLDER_CHANGED: &str = "holder_changed";
pub(super) const INPUT_IN_FLIGHT: &str = "input_in_flight";
const INPUT_RUNTIME_OWNED: &str = "input_runtime_owned";
const MAILBOX_UNAVAILABLE: &str = "mailbox_unavailable";
const NOT_BUSY: &str = "not_busy";
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
    /// The pane vetoed and the queue front was not written, or its result is unknown.
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
    Discord {
        message: MessageId,
        handback: Box<(Intervention, String)>,
        guard: SourceGuard,
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

/// Codex TUI sessions take busy-turn input only once this is on, whichever mode the env sets.
pub(super) const CODEX_BUSY_INJECT_ENABLED: bool = false;

/// Tests read the same constant; a test may also open one channel, as turning it on would.
fn codex_enabled(channel_id: u64) -> bool {
    #[cfg(test)]
    if test_hook::codex_enabled(channel_id) {
        return true;
    }
    let _ = channel_id;
    CODEX_BUSY_INJECT_ENABLED
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
    /// The Codex model turn the rollout read as open; None for Claude.
    native_turn: Option<String>,
    turn_id: Option<String>,
    /// The mailbox claim the holder check judged; the reservation requires it unchanged.
    claim: ExpectedClaim,
    /// Held from the first mailbox read until the owner settles.
    transition: OwnedMutexGuard<()>,
    /// Keeps an input-runtime gate from closing on the channel until the owner settles.
    input: Option<fence::Permit>,
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
    (binding.runtime_kind == runtime).then(|| (session, PathBuf::from(binding.relay_output_path())))
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

/// Every veto that needs no pane call, in the documented order.
async fn resolve(
    shared: &Arc<SharedData>,
    request: &HumanInputRequest,
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
    // An idle transcript keeps the caller's own start; a reservation is only for a live turn.
    let native_turn = busy_turn(runtime, &pane.1)?;
    let input = match fence::lookup(&request.provider, channel) {
        Some(gate) => Some(gate.admit().map_err(|_| INPUT_RUNTIME_OWNED)?),
        None => None,
    };
    // Held until the pane effect ends. Bounded waiters (claim handback, headless start, /clear,
    // /resume) give up after 3s; intake and kickoff never wait and fall back to their queue.
    let transition = shared
        .session_transition_lock(request.channel_id)
        .try_lock_owned()
        .map_err(|_| TRANSITION_BUSY)?;
    let (snapshot, row) = observe(shared, request).await;
    let first = holder(&snapshot, row.as_ref(), channel)?;
    backlog(&snapshot)?;
    let deferred = crate::services::discord::host_defer_gate::channel_session_deferred;
    if deferred(shared, &request.provider, channel, &pane.0).await {
        return Err(SESSION_UNRESOLVED);
    }
    // Paths that skip the transition may queue or claim while these lookups wait; the owner's
    // reservation checks the claim and the queue after them, and the row is read last here.
    #[cfg(test)]
    test_hook::final_name_lookup(channel);
    let named = channel_name(shared, request.channel_id).await;
    let row = read(&request.provider, channel);
    if live(row.as_ref(), named).as_ref() != Some(&pane) {
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
        input,
    })
}

pub(crate) async fn attempt(
    shared: &Arc<SharedData>,
    request: &HumanInputRequest,
    origin: Origin,
) -> InjectAttempt {
    let target = match resolve(shared, request).await {
        Ok(target) => target,
        Err(veto) => return InjectAttempt::NotSent(veto),
    };
    let channel = request.channel_id.get();
    let input = Arc::new(Input {
        channel,
        provider: request.provider.as_str().to_string(),
        source: request.source.clone(),
        author: request.author_id.to_string(),
        text: request.text.clone(),
        nonce: busy_inject::fresh_nonce(),
    });
    let (session, turn_id) = (target.session.clone(), target.turn_id.clone());
    let (message, handback, guard) = match origin {
        Origin::External => (None, request.queue_entry(), None),
        Origin::Discord {
            message,
            handback,
            guard,
        } => (Some(message), *handback, Some(guard)),
    };
    let owner = Owner {
        shared: shared.clone(),
        provider: request.provider.clone(),
        channel_id: request.channel_id,
        message,
        handback,
        guard,
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
        } = self;
        let Target {
            session,
            transcript,
            native_turn,
            turn_id,
            claim,
            transition,
            input: permit,
        } = target;
        let mailbox = shared.mailbox(channel_id);
        let discord = crate::services::discord::queue_persistence_context;
        let persistence = discord(&shared, &provider, channel_id);
        #[cfg(test)]
        test_hook::before_reserve(input.channel).await;
        let reserve =
            mailbox.reserve_injection(message, claim, persistence.clone(), permit.clone());
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
        let pane = pane(input.channel, &session);
        let effect = tokio::task::spawn_blocking({
            let (input, session, turn_id) = (input.clone(), session.clone(), turn_id.clone());
            move || match native_turn {
                None => run(&pane, &session, &transcript, turn_id, &input),
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
        let attempt = match attempt {
            InjectAttempt::NotSent(veto) => {
                let source = [handback.message_id.get()];
                let handed =
                    mailbox.hand_back_injected_input(ticket, handback, persistence, permit.clone());
                #[cfg(test)]
                let handed = test_hook::hand_back(input.channel, handed);
                match handed.await {
                    Ok(events) => {
                        let apply = crate::services::discord::apply_queue_exit_feedback;
                        let feedback = apply(&shared, channel_id, &events);
                        fence::effect::scope(permit.clone(), feedback).await;
                        InjectAttempt::HandedBack {
                            turn_id: queued_turn,
                            veto,
                        }
                    }
                    Err(reason) => {
                        if reason == HANDBACK_NOT_WRITTEN {
                            let failure = fence::Failure::Persistence;
                            fence::record_failure(&provider, input.channel, &source, failure);
                            let notice = crate::services::discord::queue_io::input_refusal_notice;
                            notice(&shared, channel_id, &source, failure).await;
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
                let permit = permit.clone();
                let _ = mailbox
                    .settle_injected_input(ticket, settle, persistence, permit)
                    .await;
                // Recorded whatever the settle answered, so a restart still finds it.
                if let Some(message) = message {
                    record_terminal(&provider, channel_id, message, outcome).await;
                }
                delivered
            }
        };
        // A handed-back message now waits in the queue, so the drain must not see it claimed.
        drop((guard, transition, permit));
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
    text: String,
    nonce: String,
}

fn run(
    pane: &busy_inject::Pane,
    session: &str,
    transcript: &std::path::Path,
    turn_id: Option<String>,
    input: &Input,
) -> InjectAttempt {
    #[cfg(test)]
    test_hook::crash(input.channel);
    let nonce = &input.nonce;
    let request = busy_inject::Request {
        session,
        transcript,
        source: &input.source,
        author: &input.author,
        nonce,
        text: &input.text,
    };
    let inject = || busy_inject::inject(pane, &request, &busy_inject::TIMING);
    // A panic may come after the paste, so it is reported like any later failure.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(inject));
    let alert = |detail| unconfirmed(input, session, turn_id.clone(), detail);
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
        Ok(codex_inject::Outcome::Injected {
            observed_turn,
            joined,
        }) => {
            // A turn other than the target means it ended before the Enter; the ledger settles it.
            tracing::info!(
                channel_id = input.channel,
                tmux_session = %session,
                nonce = %nonce,
                target_turn,
                observed_turn = observed_turn.as_deref().unwrap_or(""),
                joined,
                "busy Codex turn took the human input"
            );
            InjectAttempt::Injected { turn_id }
        }
        Ok(codex_inject::Outcome::Unconfirmed(detail)) => alert(codex_unconfirmed_name(detail)),
        Err(_) => alert("executor_failed"),
    }
}

fn codex_veto_name(veto: codex_inject::Veto) -> &'static str {
    use codex_inject::Veto;
    match veto {
        Veto::InvalidInput => "invalid_input",
        Veto::LockContended => "lock_contended",
        Veto::HumanAttached => "human_attached",
        Veto::AttachUnknown => "attach_unknown",
        Veto::PaneUnavailable => "pane_unavailable",
        Veto::Modal => "modal",
        Veto::NoComposer => "no_composer",
        Veto::Draft => "draft",
        Veto::QueueShown => "queue_shown",
        Veto::NotSteerable => "not_steerable",
        Veto::NotBusy => NOT_BUSY,
        Veto::TurnUnknown => "turn_unknown",
        Veto::TurnChanged => "turn_changed",
        Veto::TranscriptUnavailable => "transcript_unavailable",
        Veto::LoadFailed => "load_failed",
        Veto::UnpredictableRender => "unpredictable_render",
        Veto::LedgerFull => "ledger_full",
    }
}

fn codex_unconfirmed_name(detail: codex_inject::Unconfirmed) -> &'static str {
    use codex_inject::Unconfirmed;
    match detail {
        Unconfirmed::PasteFailed => "paste_failed",
        Unconfirmed::AttachedAfterPaste => "attached_after_paste",
        Unconfirmed::CaptureFailed => "capture_failed",
        Unconfirmed::DraftNotOwned => "draft_not_owned",
        Unconfirmed::EnterFailed => "enter_failed",
        Unconfirmed::NotObserved => "not_observed",
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
        Veto::UnpredictableRender => "unpredictable_render",
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
    use std::sync::{Arc, Mutex};

    use tokio::sync::Notify;

    use super::InjectMode;

    static FORCED: Mutex<Option<HashMap<u64, (InjectMode, PathBuf)>>> = Mutex::new(None);
    static CODEX_ENABLED: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    static CRASHING: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    static FINAL_LOOKUP: Mutex<Option<HashMap<u64, Arc<Notify>>>> = Mutex::new(None);
    type Park = (Arc<Notify>, Arc<Notify>);
    static BEFORE_RESERVE: Mutex<Option<HashMap<u64, Park>>> = Mutex::new(None);
    static GATEWAYLESS: Mutex<Option<HashMap<u64, Arc<Notify>>>> = Mutex::new(None);
    static OWNER_CRASHING: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    static HANDBACK_FAULTS: Mutex<Vec<(u64, HandbackFault)>> = Mutex::new(Vec::new());

    /// How the channel's next handback answer is lost.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum HandbackFault {
        /// The actor wrote the queue front but its answer never came back.
        AnswerLost,
        /// The settle never reached the actor; the ticket is gone with it.
        Dropped,
    }

    pub(crate) fn fault_handback(channel_id: u64, fault: HandbackFault) {
        let mut faults = HANDBACK_FAULTS.lock().unwrap_or_else(|e| e.into_inner());
        faults.push((channel_id, fault));
    }

    /// The handback as the channel's scripted fault leaves it.
    pub(super) async fn hand_back<T>(
        channel_id: u64,
        handed: impl std::future::Future<Output = Result<T, &'static str>>,
    ) -> Result<T, &'static str> {
        let fault = {
            let mut faults = HANDBACK_FAULTS.lock().unwrap_or_else(|e| e.into_inner());
            let at = faults
                .iter()
                .position(|(channel, _)| *channel == channel_id);
            at.map(|at| faults.remove(at).1)
        };
        match fault {
            None => handed.await,
            Some(HandbackFault::AnswerLost) => {
                assert!(handed.await.is_ok(), "the scripted handback lands");
                Err("handback_unknown")
            }
            // Never polled, so the settle and its ticket go nowhere.
            Some(HandbackFault::Dropped) => Err("handback_unknown"),
        }
    }

    /// Makes the channel's owner panic after its reservation, outside the pane effect.
    pub(crate) fn crash_owner(channel_id: u64) {
        let mut crashing = OWNER_CRASHING.lock().unwrap_or_else(|e| e.into_inner());
        crashing.push(channel_id);
    }

    pub(super) fn owner_crash(channel_id: u64) {
        let crashing = OWNER_CRASHING.lock().unwrap_or_else(|e| e.into_inner());
        let crash = crashing.contains(&channel_id);
        drop(crashing);
        assert!(!crash, "scripted owner crash");
    }

    /// Lets the channel's deliver start run without a gateway context: the headless start's own
    /// transition wait and mailbox claim, nothing after. The signal fires as each start begins.
    pub(crate) fn start_without_gateway(channel_id: u64) -> Arc<Notify> {
        let mut starts = GATEWAYLESS.lock().unwrap_or_else(|e| e.into_inner());
        let starts = starts.get_or_insert_with(HashMap::new);
        starts.entry(channel_id).or_default().clone()
    }

    pub(in crate::services::discord::health::turn_deliver) fn start<'a>(
        shared: &'a crate::services::discord::SharedData,
        request: &'a super::HumanInputRequest,
        reservation: &'a crate::services::discord::router::HeadlessTurnReservation,
    ) -> Option<impl std::future::Future<Output = super::super::StartAttempt> + 'a> {
        use super::super::StartAttempt;
        let starts = GATEWAYLESS.lock().unwrap_or_else(|e| e.into_inner());
        let begun = starts.as_ref()?.get(&request.channel_id.get())?.clone();
        Some(async move {
            begun.notify_one();
            let channel = request.channel_id;
            let owner = poise::serenity_prelude::UserId::new(request.author_id);
            let identity = (request.provider.clone(), None);
            let claim = crate::services::discord::router::claim_reserved_headless_turn;
            match claim(shared, channel, owner, reservation, identity).await {
                Ok(_) => StartAttempt::Started(reservation.turn_id(channel)),
                Err(crate::services::discord::router::HeadlessTurnStartError::Conflict(_)) => {
                    StartAttempt::Busy
                }
                Err(error) => StartAttempt::Unavailable(error.to_string()),
            }
        })
    }

    /// Parks the channel's next owner before its reservation: `(reached, resume)`.
    pub(crate) fn park_before_reserve(channel_id: u64) -> Park {
        let park: Park = Default::default();
        let mut parks = BEFORE_RESERVE.lock().unwrap_or_else(|e| e.into_inner());
        parks
            .get_or_insert_with(HashMap::new)
            .insert(channel_id, park.clone());
        park
    }

    pub(super) async fn before_reserve(channel_id: u64) {
        let park = {
            let mut parks = BEFORE_RESERVE.lock().unwrap_or_else(|e| e.into_inner());
            parks.as_mut().and_then(|parks| parks.remove(&channel_id))
        };
        if let Some((reached, resume)) = park {
            reached.notify_one();
            resume.notified().await;
        }
    }

    /// Fires when the channel's resolve is about to take the core lock for its final name lookup.
    pub(crate) fn final_lookup_signal(channel_id: u64) -> Arc<Notify> {
        let mut signals = FINAL_LOOKUP.lock().unwrap_or_else(|e| e.into_inner());
        let signals = signals.get_or_insert_with(HashMap::new);
        signals.entry(channel_id).or_default().clone()
    }

    pub(super) fn final_name_lookup(channel_id: u64) {
        final_lookup_signal(channel_id).notify_one();
    }

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

    /// Opens Codex injection on the channel, as turning the constant on would.
    pub(crate) fn enable_codex(channel_id: u64) {
        let mut enabled = CODEX_ENABLED.lock().unwrap_or_else(|e| e.into_inner());
        enabled.push(channel_id);
    }

    pub(super) fn codex_enabled(channel_id: u64) -> bool {
        let enabled = CODEX_ENABLED.lock().unwrap_or_else(|e| e.into_inner());
        enabled.contains(&channel_id)
    }

    pub(crate) fn clear(channel_id: u64) {
        let mut enabled = CODEX_ENABLED.lock().unwrap_or_else(|e| e.into_inner());
        enabled.retain(|channel| *channel != channel_id);
        let mut forced = FORCED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(map) = forced.as_mut() {
            map.remove(&channel_id);
        }
        let mut crashing = CRASHING.lock().unwrap_or_else(|e| e.into_inner());
        crashing.retain(|channel| *channel != channel_id);
        let mut crashing = OWNER_CRASHING.lock().unwrap_or_else(|e| e.into_inner());
        crashing.retain(|channel| *channel != channel_id);
        let mut faults = HANDBACK_FAULTS.lock().unwrap_or_else(|e| e.into_inner());
        faults.retain(|(channel, _)| *channel != channel_id);
        let mut starts = GATEWAYLESS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(starts) = starts.as_mut() {
            starts.remove(&channel_id);
        }
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
