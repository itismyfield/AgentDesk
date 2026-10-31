//! The owner branch: a channel registered with the input owner hands its input effect to the
//! owner's pump instead of a provider driver, and no refusal falls back to that driver.
use std::path::PathBuf;
use std::sync::Mutex;

use super::StreamingTurn;
use crate::services::claude_tui::{busy_inject, owner_input as scope};
use crate::services::discord::queue_io::input_owner::drive::{
    Drive, Front, FrontClaim, Handoff, Settled as Owed, claim_turn,
};
use crate::services::discord::queue_io::input_owner::effect::{
    self, BusyClaude, BusyClaudeHerdr, BusyCodex, BusyCodexHerdr, Clock, CodexInput, Completion,
    PreparedPayload,
};
use crate::services::discord::queue_io::input_owner::origin::{Execution, InputOrigin};
use crate::services::discord::queue_io::input_owner::{pump, target};
use crate::services::platform::tmux::PaneLiveness;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::session_host::{HerdrTarget, herdr_endpoints};
use crate::services::turn_host::{HerdrTurnPlan, TurnHost};
use crate::services::turn_orchestrator::{ChannelMailboxHandle, PendingQueueItem};
use serde_json::{Value, json};

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
    until_done: Option<UntilDone>,
}

/// Reads an idle submit's AI turn to its end on the turn's sender and returns its outcome.
pub(super) type UntilDone = Box<dyn FnOnce() -> Result<(), String> + Send>;

impl Registration {
    /// A front's registration read from what its turn observes: host verdict, tmux spawn marker or
    /// Bound Herdr row, live transcript. Without a live execution it is Absent, whatever `busy`.
    pub(super) fn observed(
        mailbox: ChannelMailboxHandle,
        (provider, origin, busy): (&str, InputOrigin, bool),
        (host, session, live): (&TurnHost, &str, Option<(&str, Option<&str>)>),
        original: &PendingQueueItem,
        until_done: Option<UntilDone>,
    ) -> Result<Self, String> {
        let herdr = match host {
            TurnHost::Herdr(plan) => bound_herdr(plan),
            _ => None,
        };
        // A pane kept after its process exited is no live execution; an unknown answer refuses.
        let alive = match host {
            TurnHost::Tmux => match crate::services::platform::tmux::pane_liveness(session) {
                PaneLiveness::Live => true,
                PaneLiveness::DeadOrAbsent => false,
                PaneLiveness::ProbeError => return Err("NotSent(\"pane_unobserved\")".into()),
            },
            _ => herdr.is_some(),
        };
        let phase = match (alive, busy) {
            (false, _) => target::Phase::Absent,
            (true, true) => target::Phase::Busy,
            (true, false) => target::Phase::Idle,
        };
        // An Absent attempt's target and source are the created execution's, set by its launch.
        let target = match (&herdr, phase) {
            (_, target::Phase::Absent) => Value::Null,
            (Some((_, _, target)), _) => target.clone(),
            (None, _) => scope::created_tmux_target(provider, session)?,
        };
        let source = match live.filter(|_| phase != target::Phase::Absent) {
            Some((path, Some(session_id))) => {
                use std::os::unix::fs::MetadataExt;
                let meta = std::fs::metadata(path).map_err(|error| error.to_string())?;
                json!({"kind":"existing","session_id":session_id,"transcript":path,
                    "file":[meta.dev(), meta.ino()],"offset":meta.len()})
            }
            _ => json!({"kind":"pre_source","expected_session_id":null,
                "expected_transcript":null,"binding_watermark":null}),
        };
        let sources = match original.source_message_ids.as_slice() {
            [] => vec![original.message_id.to_string()],
            ids => ids.iter().map(u64::to_string).collect(),
        };
        Ok(Self {
            mailbox,
            execution: Execution::LocalTui,
            origin,
            phase,
            attempt: json!({"nonce":busy_inject::fresh_nonce(),"provider":provider,
                "target":target,"source":source}),
            sources,
            transcript: live.map_or_else(PathBuf::new, |(path, _)| path.into()),
            program: "tmux".into(),
            herdr: herdr.map(|(pane, writer, _)| (pane, writer)),
            author: original.author_id.to_string(),
            name: format!("discord:{}", original.author_id),
            clock: std::sync::Arc::new(|| chrono::Utc::now().timestamp_millis().max(0) as u64),
            timing: &busy_inject::TIMING,
            until_done,
        })
    }
}

/// A Bound Herdr row's gated pane, its physical writer `socket#pane` and its attempt target.
fn bound_herdr(plan: &HerdrTurnPlan) -> Option<(HerdrTarget, String, Value)> {
    use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
    let HostedRecord::Known(record) = &plan.row.as_ref()?.record else {
        return None;
    };
    let at = (record.location.as_ref()).filter(|_| record.state == HostedState::Bound)?;
    let pane = herdr_endpoints().target(record)?;
    let target = json!({"host":"herdr","owner":record.owner.logical_key,"root":at.socket_addr,
        "execution_nonce":record.execution_nonce});
    Some((pane, format!("{}#{}", at.socket_addr, at.pane_id), target))
}

/// The channel's owner drive: a front goes into the turn that runs, else to `idle`, the start of
/// a turn that claims it.
pub(in crate::services::discord) fn owner_drive(
    (mailbox, pool): (ChannelMailboxHandle, Option<sqlx::PgPool>),
    (provider, channel): (ProviderKind, u64),
    idle: Handoff,
) -> std::sync::Arc<Drive> {
    let (busy, owed) = (
        mailbox.clone(),
        (mailbox.clone(), provider.as_str().to_string()),
    );
    let drains = pool.clone();
    let handoff: Handoff = std::sync::Arc::new(move |front| {
        let (mailbox, pool, provider, idle) =
            (busy.clone(), pool.clone(), provider.clone(), idle.clone());
        Box::pin(async move {
            match mailbox.has_active_turn().await {
                // A routine, scheduled or HTTP start completes only on its own turn.
                Ok(true) if synthetic(&mailbox, &front).await => {
                    Err("NotSent(\"synthetic_waits_idle\")".into())
                }
                Ok(true) => hand_busy((mailbox, pool), provider, (channel, front)).await,
                Ok(false) => idle(front).await,
                Err(unreachable) => Err(format!("{unreachable:?}")),
            }
        })
    });
    let recheck = std::time::Duration::from_secs(5);
    let settled: Option<Owed> = drains.map(|pool| {
        let settled: Owed = std::sync::Arc::new(move || {
            let (pool, (mailbox, provider)) = (pool.clone(), owed.clone());
            Box::pin(async move {
                let drain = crate::services::discord::queue_io::input_owner::notice::drain;
                let drain = drain(&pool, &mailbox, &provider, channel).await;
                if let Err(error) = drain {
                    tracing::warn!(channel_id = channel, %error, "owner notices stay owed");
                }
            })
        });
        settled
    });
    std::sync::Arc::new(Drive {
        mailbox,
        channel,
        handoff,
        recheck,
        settled,
    })
}

/// Whether a front's admission carries a start's context; an unreadable one counts as a start.
async fn synthetic(mailbox: &ChannelMailboxHandle, (ticket, original): &Front) -> bool {
    let Some(ticket) = ticket.clone() else {
        return false;
    };
    let context = mailbox.v2_admission_context(original.clone(), ticket);
    !matches!(context.await, Ok(None))
}

/// A front into the running turn whose inflight row names the pane; the grant is spent only for a
/// live busy pane, and the drive waits for the input's settlement, not the AI turn.
pub(in crate::services::discord) async fn hand_busy(
    (mailbox, pool): (ChannelMailboxHandle, Option<sqlx::PgPool>),
    provider: ProviderKind,
    (channel, front): (u64, Front),
) -> Result<(), String> {
    let unobserved = || "NotSent(\"busy_turn_unobserved\")".to_string();
    let row = crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel)
        .ok_or_else(unobserved)?;
    let session = row.tmux_session_name.clone().ok_or_else(unobserved)?;
    let key = row.session_key.as_deref();
    let host = crate::services::turn_host::for_turn(pool.as_ref(), &provider, channel, key).await;
    let live = (row.output_path.as_deref()).map(|path| (path, row.session_id.as_deref()));
    let at = (&host, session.as_str(), live);
    let kind = (provider.as_str(), InputOrigin::Discord, true);
    let owner = Registration::observed(mailbox.clone(), kind, at, &front.1, None)?;
    if owner.phase != target::Phase::Busy {
        return Err(unobserved());
    }
    let claim = claim_turn(&mailbox, front.1.message_id, &front).await?;
    let claim = claim.ok_or("NotSent(\"front_not_granted\")")?;
    let mut bodies = Vec::new();
    for item in std::iter::once(&front.1).chain(claim.followers()) {
        bodies.push(super::super::input_payload::busy_body(channel, item, pool.as_ref()).await?);
    }
    let payload = bodies.join("\n\n");
    let (settled, answer) = tokio::sync::oneshot::channel();
    let thread = tokio::task::spawn_blocking(move || {
        let turn = StreamingTurn {
            pool: pool.as_ref(),
            shared: None,
            provider: &provider,
            prompt: &payload,
            session_id: None,
            working_dir: "",
            system_prompt: None,
            allowed_tools: &[],
            cancel: std::sync::Arc::new(CancelToken::new()),
            codex_observation: None,
            tmux_session_name: Some(&session),
            teardown: None,
            host: &host,
            channel_id: channel,
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
        execute_front(turn, std::sync::mpsc::channel().0, claim, owner, settled)
    });
    match answer.await {
        Ok(settlement) => settlement,
        Err(_) => thread.await.map_err(|error| error.to_string())?,
    }
}

/// A front the drive granted and the owner claimed, carried by value to the turn that starts it,
/// with the drive's answer once that input settled.
pub(in crate::services::discord) struct OwnerTurn {
    mailbox: ChannelMailboxHandle,
    claim: FrontClaim,
    settled: Settled,
}

impl OwnerTurn {
    pub(in crate::services::discord) fn front(&self) -> &Front {
        self.claim.front()
    }
}

type Shared = std::sync::Arc<crate::services::discord::SharedData>;
type ChannelId = poise::serenity_prelude::ChannelId;

/// The channel's owner drive: a front goes into the running turn, else starts its own.
fn drive(shared: &Shared, channel: ChannelId) -> std::sync::Arc<Drive> {
    let starts = std::sync::Arc::clone(shared);
    let idle: Handoff = std::sync::Arc::new(move |front| {
        Box::pin(start_idle(std::sync::Arc::clone(&starts), channel, front))
    });
    let at = (shared.mailbox(channel), shared.pg_pool.clone());
    owner_drive(at, (shared.provider.clone(), channel.get()), idle)
}

/// Wakes the channel's owner drive.
pub(in crate::services::discord) fn wake(shared: &Shared, channel: ChannelId) {
    crate::services::discord::queue_io::input_owner::drive::wake(drive(shared, channel));
}

/// A headless start on a target the owner takes: its reservation's original is admitted under
/// that turn's ticket, then the drive wakes; `None` leaves the start to its legacy path.
pub(in crate::services::discord) async fn admit_start(
    shared: &Shared,
    channel: ChannelId,
    (turn_id, text): (&str, &str),
    provenance: serde_json::Value,
) -> Option<crate::services::discord::queue_io::input_ingress::Intake> {
    use crate::services::discord::queue_io::input_ingress::{self as ingress, Intake};
    use crate::services::discord::queue_io::input_owner::{routine, start};
    let execution = ingress::execution(&shared.provider, channel.get());
    if !execution.owner_takes(InputOrigin::HttpStart) {
        return None;
    }
    match ingress::adopt(shared, &shared.provider, channel).await {
        Ok(true) => {}
        Ok(false) => return None,
        Err(error) => return Some(Intake::Rejected(error)),
    }
    let (original, ticket) = match routine::synthetic_original(turn_id, channel.get(), text, 1) {
        Ok(original) => original,
        Err(error) => return Some(Intake::Rejected(error)),
    };
    let at = (execution, InputOrigin::HttpStart);
    let ready = std::future::ready(Ok(()));
    start::admit_synthetic(
        drive(shared, channel),
        at,
        (&original, ticket),
        provenance,
        ready,
    )
    .await
}

/// A restarted owner re-adopts the channel's V2 queue, settles a head whose window passed,
/// projects history and posts owed notices, then wakes its drive.
pub(in crate::services::discord) async fn recover(shared: &Shared, channel: ChannelId) {
    use crate::services::discord::queue_io::input_owner::recovery;
    use crate::services::tui_input::human_frame::history::v2::Registry;
    let persistence = crate::services::discord::queue_dispatch::persistence_context;
    let queue = crate::services::turn_orchestrator::v2_queue;
    let queue = queue(&persistence(shared, &shared.provider, channel), channel);
    let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let recovered = match (queue, Registry::runtime()) {
        (Ok(queue), Ok(history)) => {
            let at = (shared.provider.as_str(), channel.get(), now);
            let pool = shared.pg_pool.as_ref();
            recovery::recover(&shared.mailbox(channel), queue, history, pool, at).await
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
    };
    match recovered {
        Ok(_) => wake(shared, channel),
        Err(error) => tracing::warn!(channel_id = channel.get(), %error, "owner recovery held"),
    }
}

/// An idle channel's front: the owner claims it, then a queued start presenting that claim runs
/// it; the drive hears the input's settlement, never the AI turn's.
async fn start_idle(
    shared: std::sync::Arc<crate::services::discord::SharedData>,
    channel: poise::serenity_prelude::ChannelId,
    front: Front,
) -> Result<(), String> {
    use crate::services::discord::queue_io::transport::QueueTransport;
    let transport = QueueTransport::from_runtime(&shared);
    let deps = transport
        .as_ref()
        .map(|transport| transport.intake_deps(&shared));
    let deps = deps.ok_or("NotSent(\"discord_unavailable\")")?;
    let mailbox = shared.mailbox(channel);
    let claim = claim_turn(&mailbox, front.1.message_id, &front).await?;
    let claim = claim.ok_or("NotSent(\"front_not_granted\")")?;
    let (settled, answer) = tokio::sync::oneshot::channel();
    let owner = OwnerTurn {
        mailbox,
        claim,
        settled,
    };
    let now = (std::time::SystemTime::now(), std::time::Instant::now());
    let restore = crate::services::turn_orchestrator::pending_queue_item_to_intervention;
    let mut intervention = restore(front.1, now.0, now.1);
    // The merged originals start as one input, joined as the legacy queue joins them.
    for item in owner.claim.followers() {
        intervention.text = format!("{}\n{}", intervention.text, item.text);
        intervention
            .pending_uploads
            .extend(item.pending_uploads.iter().cloned());
    }
    let owner_name = crate::services::discord::queue_dispatch::kickoff::queued_owner_name;
    let name = owner_name(deps.http, intervention.author_id).await;
    let at = (shared.provider.clone(), channel);
    let start = crate::services::discord::router::start_owner_front;
    let started = start(&deps, at, &intervention, name, owner).await;
    match (answer.await, started) {
        (Ok(settlement), _) => settlement,
        (Err(_), Err(error)) => Err(format!("NotSent({:?})", error.to_string())),
        (Err(_), Ok(_)) => Err("NotSent(\"turn_not_started\")".into()),
    }
}

/// An owner front's turn: its registration is read from this turn's own host and pane, and an
/// idle pane's AI turn is read on this turn's sender until it ends.
pub(super) fn execute_owned(
    turn: StreamingTurn<'_>,
    sender: std::sync::mpsc::Sender<crate::services::agent_protocol::StreamMessage>,
    owner: OwnerTurn,
) -> Result<(), String> {
    use crate::services::platform::tmux::pane_liveness;
    let OwnerTurn {
        mailbox,
        claim,
        settled,
    } = owner;
    let session = turn.tmux_session_name.unwrap_or_default();
    let live = matches!(turn.host, TurnHost::Tmux) && pane_liveness(session) == PaneLiveness::Live;
    let observer = live.then(|| observer(&turn, &sender)).flatten();
    let kind = (turn.provider.as_str(), InputOrigin::Discord, false);
    let at = (turn.host, session, None);
    match Registration::observed(mailbox, kind, at, &claim.front().1, observer) {
        Ok(owner) => execute_front(turn, sender, claim, owner, settled),
        Err(error) => {
            let _ = settled.send(Err(error.clone()));
            Err(error)
        }
    }
}

/// The provider's own observer of the AI turn an idle submit starts, set up before that submit.
fn observer(
    turn: &StreamingTurn<'_>,
    sender: &std::sync::mpsc::Sender<crate::services::agent_protocol::StreamMessage>,
) -> Option<UntilDone> {
    let at = (turn.tmux_session_name?, turn.prompt, turn.channel_id);
    let session = (turn.session_id, std::path::Path::new(turn.working_dir));
    let (sender, cancel) = (sender.clone(), std::sync::Arc::clone(&turn.cancel));
    match turn.provider {
        ProviderKind::Claude => {
            use crate::services::claude_tui::hosting::owner_observer;
            owner_observer(at, session, sender, cancel)
        }
        ProviderKind::Codex => {
            use crate::services::codex_tui::warm_followup::owner_observer;
            owner_observer(at, session, sender, cancel)
        }
        _ => None,
    }
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

#[cfg(test)]
static REGISTERED: Mutex<Vec<(u64, Registration)>> = Mutex::new(Vec::new());

/// The next turn on `channel` goes to the owner.
#[cfg(test)]
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
#[cfg(test)]
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
    let idle = matches!(route, target::Route::ClaudeIdle | target::Route::CodexIdle);
    // An idle submit starts an AI turn only its provider's observer reads, so none is sent without it.
    let until_done = match (idle, registration.until_done.take()) {
        (true, None) => return Some(Taken::Done(Err("NotSent(\"idle_unobserved\")".into()))),
        (_, until_done) => until_done,
    };
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
    let result = run(turn, route, registration, front);
    reply(&result);
    let result = match (result, until_done) {
        (Ok(()), Some(until_done)) => until_done(),
        (result, _) => result,
    };
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
