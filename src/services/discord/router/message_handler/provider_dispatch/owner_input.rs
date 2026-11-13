//! The owner branch: a channel registered with the input owner hands its input effect to the
//! owner's pump instead of a provider driver, and no refusal falls back to that driver.
use std::path::PathBuf;
use std::sync::Mutex;

use super::StreamingTurn;
use crate::services::claude_tui::busy_inject::Screen;
use crate::services::claude_tui::host_input::BusyOperation;
use crate::services::claude_tui::{busy_inject, owner_input as scope};
use crate::services::discord::queue_io::input_owner::claude_handoff::{self, bound_herdr};
use crate::services::discord::queue_io::input_owner::drive::{
    Drive, Front, FrontClaim, Handoff, Settled as Owed, claim_turn,
};
use crate::services::discord::queue_io::input_owner::effect::{
    self, BusyClaude, BusyCodex, BusyCodexHerdr, Clock, CodexInput, Completion, PreparedPayload,
};
use crate::services::discord::queue_io::input_owner::origin::{Execution, InputOrigin};
use crate::services::discord::queue_io::input_owner::{pump, target};
use crate::services::discord::router::intake_dispatch::WRITER_HELD;
use crate::services::platform::tmux::PaneLiveness;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::session_host::HerdrTarget;
use crate::services::tui_o::cutover::intake_route::{self, IntakeRoute};
use crate::services::turn_host::TurnHost;
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
    /// Only a successfully settled write may attach its native input to this birth episode.
    native_pin_submitted: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

/// Reads an idle submit's AI turn to its end on the turn's sender and returns its outcome.
pub(super) type UntilDone = Box<dyn FnOnce() -> Result<(), String> + Send>;

impl Registration {
    /// A front's registration read from what its turn observes: host verdict, tmux spawn marker or
    /// Bound Herdr row, live transcript. Without a live execution under the channel's account it is
    /// Absent, whatever `busy`.
    pub(super) fn observed(
        mailbox: ChannelMailboxHandle,
        kind: (&str, InputOrigin, bool),
        at: (&TurnHost, &str, Option<(&str, Option<&str>)>),
        original: &PendingQueueItem,
        until_done: Option<UntilDone>,
    ) -> Result<Self, String> {
        Self::read_as(mailbox, kind, at, original, (until_done, None))
    }

    /// As `observed`; a tmux pane whose liveness its caller already read is registered as read.
    fn read_as(
        mailbox: ChannelMailboxHandle,
        (provider, origin, busy): (&str, InputOrigin, bool),
        (host, session, live): (&TurnHost, &str, Option<(&str, Option<&str>)>),
        original: &PendingQueueItem,
        (until_done, read): (Option<UntilDone>, Option<bool>),
    ) -> Result<Self, String> {
        let herdr = match host {
            TurnHost::Herdr(plan) => bound_herdr(plan),
            _ => None,
        };
        // A pane kept after its process exited is no live execution; an unknown answer refuses.
        let alive = match (host, read) {
            (TurnHost::Tmux, Some(alive)) => alive,
            (TurnHost::Tmux, None) => match crate::services::platform::tmux::pane_liveness(session)
            {
                PaneLiveness::Live => !other_account(provider, session),
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
            native_pin_submitted: None,
        })
    }
}

/// A tmux pane under another account than its channel's: no execution of this channel, so its
/// driver replaces it as the live path does, and nothing is written into it.
fn other_account(provider: &str, session: &str) -> bool {
    let Some(kind) = ProviderKind::from_str(provider) else {
        return false;
    };
    let overlay = crate::services::discord::org_schema::overlay_from_tmux_session(kind, session);
    let matches = crate::services::tmux_common::tmux_session_auth_profile_matches;
    overlay.is_ok_and(|overlay| !matches(session, &overlay.profile_id))
}

/// The channel's owner drive: a front goes into the turn that runs, else to `idle`, the start of
/// a turn that claims it.
pub(in crate::services::discord) fn owner_drive(
    (mailbox, pool): (ChannelMailboxHandle, Option<sqlx::PgPool>),
    (provider, channel): (ProviderKind, u64),
    idle: Handoff,
    runtime: Option<Shared>,
) -> std::sync::Arc<Drive> {
    let (busy, owed) = (
        mailbox.clone(),
        (mailbox.clone(), provider.as_str().to_string()),
    );
    let drains = pool.clone();
    let handoff: Handoff = std::sync::Arc::new(move |front| {
        let (mailbox, pool, provider, idle, runtime) = (
            busy.clone(),
            pool.clone(),
            provider.clone(),
            idle.clone(),
            runtime.clone(),
        );
        Box::pin(async move {
            use crate::services::discord::queue_io::input_ingress as ingress;
            let original = front.clone();
            if crate::services::discord::router::intake_dispatch::writer_boot_held(
                &provider, channel,
            )
            .await
            {
                let id = ChannelId::new(channel);
                hold_start(&mailbox, id, &front).await;
                #[cfg(test)]
                crate::services::discord::queue_io::cancel_backstop_test_support::owner_boot_hold_finished(
                    id, front.1.message_id,
                );
                if crate::services::tui_o::writer::host::channel_accepts(channel)
                    && intake_route::for_input_placement(provider.as_str(), channel).await
                        == IntakeRoute::Gateway
                    && let Some(shared) = &runtime
                {
                    wake(shared, id);
                }
                return Err(WRITER_HELD.into());
            }
            let origin = front_origin(&mailbox, &front).await?;
            let synthetic = !origin.is_person();
            if !synthetic
                && claude_handoff::prepare(runtime.as_ref(), &mailbox, &provider, channel, &front)
                    .await?
            {
                return idle(front).await;
            }
            let active = match mailbox.has_active_turn().await {
                // Without a registry this read considers only a durable external holder.
                Ok(false) if !synthetic => Ok(
                    crate::services::discord::health::external_turn_hold_for_start(
                        None, &provider, channel,
                    )
                    .await
                    .is_some(),
                ),
                active => active,
            };
            let result = match active {
                // A routine, scheduled or HTTP start completes only on its own turn.
                Ok(true) if synthetic => Err("NotSent(\"synthetic_waits_idle\")".into()),
                Ok(true) => {
                    let transition = runtime.as_ref().map(|shared| {
                        shared
                            .session_transition_lock(ChannelId::new(channel))
                            .try_lock_owned()
                    });
                    if let Some(hold) = runtime
                        .as_ref()
                        .and_then(|shared| ingress::ingress_hold(shared, false))
                    {
                        let reason = match hold {
                            ingress::Hold::RestartRecovery => "restart_recovery",
                            _ => "upload_materialization_failed",
                        };
                        Err(effect::unsent(reason.into()))
                    } else if matches!(transition, Some(Err(_))) {
                        Err(effect::unsent("session_transition_busy".into()))
                    } else {
                        hand_busy(
                            (mailbox.clone(), pool),
                            provider.clone(),
                            (channel, front),
                            runtime.clone(),
                        )
                        .await
                    }
                }
                Ok(false) => idle(front).await,
                Err(unreachable) => Err(format!("{unreachable:?}")),
            };
            if synthetic
                && origin != InputOrigin::EffortContinue
                && result.as_ref().err().map(String::as_str) == Some(START_WAITS)
            {
                let held = original.clone();
                mailbox
                    .v2_request(move |slot| {
                        slot.as_mut()
                            .ok_or("V2 not adopted")?
                            .prefer_person(&held)
                            .map(drop)
                    })
                    .await?;
            }
            if !synthetic
                && let (Some(shared), Err(error)) = (&runtime, &result)
                && error != "NotSent(\"owner_start_failed\")"
                && let Some(hold) = ingress::Hold::from_refusal(error)
            {
                ingress::handoff_held(shared, &provider, ChannelId::new(channel), &original, hold)
                    .await;
            }
            #[cfg(test)]
            tests::idle::observe_root_recovery_handoff(channel, original.1.message_id, &result);
            result
        })
    });
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
    #[cfg(test)]
    let settled = tests::idle::observe_settlement(channel, settled);
    std::sync::Arc::new(Drive {
        mailbox,
        channel,
        handoff,
        settled,
    })
}

/// Reads the shared person policy from the exact front's kept context; read failures stay unsent.
async fn front_origin(
    mailbox: &ChannelMailboxHandle,
    (ticket, original): &Front,
) -> Result<InputOrigin, String> {
    let context = match ticket {
        Some(ticket) => {
            mailbox
                .v2_admission_context(original.clone(), ticket.clone())
                .await?
        }
        None => None,
    };
    Ok(InputOrigin::from_context(context.as_ref()))
}

/// A front into the running turn whose inflight row names the pane; the grant is spent only for a
/// live busy pane, and the drive waits for the input's settlement, not the AI turn.
pub(in crate::services::discord) async fn hand_busy(
    (mailbox, pool): (ChannelMailboxHandle, Option<sqlx::PgPool>),
    provider: ProviderKind,
    (channel, front): (u64, Front),
    runtime: Option<Shared>,
) -> Result<(), String> {
    let row = crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel)
        .ok_or_else(unobserved)?;
    let session = row.tmux_session_name.clone().ok_or_else(unobserved)?;
    let key = row.session_key.as_deref();
    let host = crate::services::turn_host::for_turn(pool.as_ref(), &provider, channel, key).await;
    // Claude's prelaunch row has no transcript until its observer hands it back. Its live pane
    // already has the binding that the idle observer used before submitting the first input.
    let binding = (provider == ProviderKind::Claude
        && matches!(&host, TurnHost::Tmux)
        && row.output_path.is_none())
    .then(|| crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&session))
    .flatten()
    .filter(|binding| {
        binding.runtime_kind == crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
    });
    let output = row
        .output_path
        .as_deref()
        .or_else(|| binding.as_ref().map(|binding| binding.output_path.as_str()));
    let native = binding
        .as_ref()
        .and_then(|binding| binding.session_id.as_deref())
        .or(row.session_id.as_deref());
    let live = output.map(|path| (path, native));
    let at = (&host, session.as_str(), live);
    let kind = (
        provider.as_str(),
        front_origin(&mailbox, &front).await?,
        true,
    );
    let owner =
        Registration::observed(mailbox.clone(), kind, at, &front.1, None).map_err(|error| {
            if effect::surely_unsent(&error) {
                error
            } else {
                effect::unsent(format!("busy_source_unavailable: {error}"))
            }
        })?;
    if owner.phase != target::Phase::Busy {
        return Err(unobserved());
    }
    let claim = claim_turn(&mailbox, front.1.message_id, &front).await?;
    let claim = claim.ok_or("NotSent(\"front_not_granted\")")?;
    // The running turn's token learns the attempt before its paste can land a row.
    let active = mailbox.cancel_token().await.ok().flatten();
    if let (Some(active), Some(attempt)) = (active, owner.attempt["nonce"].as_str())
        && active
            .turn_nonce()
            .is_some_and(|nonce| row.turn_nonce.as_deref() == Some(nonce))
        && !active.native_stop.is_completed()
    {
        crate::services::discord::inflight::record_native_busy_child(&row, &active, attempt);
    }
    let (settled, answer) = tokio::sync::oneshot::channel();
    let thread = paste_busy(
        (pool, provider, runtime),
        (channel, session, host),
        claim,
        owner,
        settled,
    )
    .await;
    match (answer.await, thread) {
        (Ok(settlement), _) => settlement,
        (Err(_), Some(thread)) => thread.await.map_err(|error| error.to_string())?,
        (Err(_), None) => Err(unobserved()),
    }
}

fn unobserved() -> String {
    "NotSent(\"busy_turn_unobserved\")".to_string()
}

/// Runs the busy effect for a claimed front on a blocking thread that answers `settled` once the
/// input settled; a body that cannot be built answers there too and starts no thread.
async fn paste_busy(
    (pool, provider, runtime): (Option<sqlx::PgPool>, ProviderKind, Option<Shared>),
    (channel, session, host): (u64, String, TurnHost),
    claim: FrontClaim,
    owner: Registration,
    settled: Settled,
) -> Option<tokio::task::JoinHandle<Result<(), String>>> {
    let mut head = claim.front().1.clone();
    if owner.origin == InputOrigin::Imessage
        && head.reply_context.is_none()
        && let Some(ticket) = claim.front().0.clone()
    {
        let context = owner
            .mailbox
            .v2_admission_context(head.clone(), ticket)
            .await;
        match context {
            Ok(Some(context)) => {
                head.reply_context = crate::services::imsg_reply::with_started_contract(
                    None,
                    context["source"].as_str(),
                    context.get("metadata"),
                );
            }
            Ok(None) => {}
            Err(error) => {
                let _ = settled.send(Err(effect::unsent(error)));
                return None;
            }
        }
    }
    let mut bodies = Vec::new();
    for item in std::iter::once(&head).chain(claim.followers()) {
        match super::super::input_payload::busy_body(channel, item, pool.as_ref()).await {
            Ok(body) => bodies.push(body),
            Err(error) => {
                let _ = settled.send(Err(error));
                return None;
            }
        }
    }
    let payload = bodies.join("\n\n");
    Some(tokio::task::spawn_blocking(move || {
        let turn = StreamingTurn {
            pool: pool.as_ref(),
            shared: runtime.as_ref(),
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
            effort: None,
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
    }))
}

/// A front the drive granted and the owner claimed, carried by value to the turn that starts it,
/// with the drive's answer once that input settled.
pub(in crate::services::discord) struct OwnerTurn {
    mailbox: ChannelMailboxHandle,
    runtime: Option<Shared>,
    claim: FrontClaim,
    settled: Settled,
    /// Where the front came from, read from the provenance its admission kept.
    origin: InputOrigin,
    /// What the start's headless turn knows of its pane for the late native read.
    late: LateRead,
    refusal: std::sync::Arc<Mutex<Option<crate::services::turn_host::HerdrRefusal>>>,
}

/// Whether the start's session id names the pane it writes to, and whether its fresh or snapshot
/// runtime-binding clear waits for that pane to read idle.
#[derive(Clone, Copy, Default)]
pub(in crate::services::discord) struct LateRead {
    own_pane: bool,
    clear_binding: bool,
}

impl OwnerTurn {
    pub(super) fn mailbox(&self) -> &ChannelMailboxHandle {
        &self.mailbox
    }
    pub(super) fn claim(&self) -> &FrontClaim {
        &self.claim
    }
    pub(in crate::services::discord) fn refused(
        &self,
        refusal: crate::services::turn_host::HerdrRefusal,
    ) {
        *self.refusal.lock().unwrap_or_else(|e| e.into_inner()) = Some(refusal);
    }

    pub(in crate::services::discord) fn read_late(&mut self, own_pane: bool, clear_binding: bool) {
        self.late = LateRead {
            own_pane,
            clear_binding,
        };
    }

    pub(in crate::services::discord) fn front(&self) -> &Front {
        self.claim.front()
    }

    /// No effect began: leave the original at the owner's front until another real wake.
    pub(in crate::services::discord) fn defer_before_submit(self, reason: &str) {
        let _ = self.settled.send(Err(effect::unsent(reason.into())));
    }

    /// The input finished locally with no provider turn: it leaves the queue and the drive goes on.
    pub(in crate::services::discord) async fn complete_locally(self) {
        let claimed = (self.claim.front().clone(), self.claim.number());
        let followers = self.claim.followers().len();
        let done = self.mailbox.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("V2 not adopted")?;
            owner.complete_claimed_locally((&claimed.0, claimed.1), followers)
        });
        let _ = self.settled.send(done.await);
    }

    /// A pane running a turn AgentDesk did not start takes the claimed front now, through the busy
    /// entry; the row the input produces decides who answers, and the drive hears its settlement.
    pub(in crate::services::discord) async fn into_busy_pane(
        self,
        (pool, provider): (Option<sqlx::PgPool>, ProviderKind),
        (channel, session, key): (u64, String, Option<String>),
        (transcript, session_id): (String, Option<String>),
    ) {
        let OwnerTurn {
            mailbox,
            runtime,
            claim,
            settled,
            origin,
            ..
        } = self;
        let for_turn = crate::services::turn_host::for_turn;
        let host = for_turn(pool.as_ref(), &provider, channel, key.as_deref()).await;
        let live = Some((transcript.as_str(), session_id.as_deref()));
        let kind = (provider.as_str(), origin, true);
        let at = (&host, session.as_str(), live);
        let observed =
            Registration::observed(mailbox, kind, at, &claim.front().1, None).and_then(|owner| {
                (owner.phase == target::Phase::Busy)
                    .then_some(owner)
                    .ok_or_else(unobserved)
            });
        let owner = match observed {
            Ok(owner) => owner,
            Err(error) => {
                if let Some(shared) = runtime.as_ref() {
                    let originals: Vec<_> = std::iter::once(claim.front().1.clone())
                        .chain(claim.followers().iter().cloned())
                        .collect();
                    super::super::busy_retry::note_busy_result(
                        shared,
                        &provider,
                        ChannelId::new(channel),
                        &originals,
                        &[],
                        &Err(error.clone()),
                    )
                    .await;
                }
                return drop(settled.send(Err(error)));
            }
        };
        let at = (channel, session, host);
        let _ = paste_busy((pool, provider, runtime), at, claim, owner, settled).await;
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
    owner_drive(
        at,
        (shared.provider.clone(), channel.get()),
        idle,
        Some(shared.clone()),
    )
}

/// Wakes the channel's owner drive.
pub(in crate::services::discord) fn wake(shared: &Shared, channel: ChannelId) {
    #[cfg(test)]
    crate::services::discord::queue_io::cancel_backstop_test_support::record_owner_wake(channel);
    #[cfg(test)]
    if crate::services::discord::queue_io::cancel_backstop_test_support::run_owner_wake_for_tests(
        channel,
    ) {
        return;
    }
    crate::services::discord::queue_io::input_owner::drive::wake(drive(shared, channel));
}

#[cfg(test)]
pub(in crate::services::discord) fn wake_drive_for_test(shared: &Shared, channel: ChannelId) {
    crate::services::discord::queue_io::input_owner::drive::wake(drive(shared, channel));
}

/// A headless start on a target the owner takes: its reservation's original is admitted under
/// that turn's ticket, then the drive wakes; `None` leaves the start to its legacy path.
pub(in crate::services::discord) async fn admit_start(
    shared: &Shared,
    channel: ChannelId,
    (turn_id, text): (&str, &str),
    mut provenance: serde_json::Value,
) -> Option<crate::services::discord::queue_io::input_ingress::Intake> {
    use crate::services::discord::queue_io::input_ingress::{self as ingress, Intake};
    use crate::services::discord::queue_io::input_owner::start;
    let execution = ingress::execution(&shared.provider, channel.get());
    if !execution.owner_takes(InputOrigin::HttpStart) {
        return None;
    }
    match ingress::adopt(shared, &shared.provider, channel).await {
        Ok(true) => {}
        Ok(false) => return None,
        Err(error) => return Some(Intake::Rejected(error)),
    }
    let (original, ticket) = match start::original(
        &shared.mailbox(channel),
        (turn_id, channel.get(), text),
        &mut provenance,
    )
    .await
    {
        Ok(original) => original,
        Err(error) => return Some(Intake::Rejected(error)),
    };
    let at = (execution, InputOrigin::from_context(Some(&provenance)));
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
        Ok(_) => {
            claude_handoff::recover(shared.clone(), channel);
            wake(shared, channel);
        }
        Err(error) => tracing::warn!(channel_id = channel.get(), %error, "owner recovery held"),
    }
}

/// Why a start waits unclaimed: a later event wakes it, or a read of its pane's native turn does.
enum Wait {
    Go,
    Event,
    Watch(crate::services::discord::queue_io::input_owner::held_watch::ScreenProbe),
}

/// Whether a start must wait for its pane's idle: a turn AgentDesk did not start holds the channel,
/// or the pane the start would take runs a native turn, cannot be read or cannot be named yet.
async fn start_waits(shared: &Shared, channel: ChannelId, context: &Value) -> Wait {
    use crate::services::discord::inflight::{TurnSource, load_inflight_state_read_only};
    use crate::services::discord::turn_presence::activity::{Activity, activity_now};
    let (provider, id) = (&shared.provider, channel.get());
    let rest = crate::services::discord::recovery_engine::herdr_row_end::owner::holds_rest;
    let row = load_inflight_state_read_only(provider, id)
        .is_some_and(|row| row.turn_source != TurnSource::Managed);
    if row || rest(provider, id) {
        return Wait::Event;
    }
    if crate::services::tui_o::turn_mode::transcript_turns(id) {
        let busy = activity_now(shared, provider, channel).await.activity != Activity::Idle;
        if busy {
            return Wait::Event;
        }
        // The activity read the channel's own pane; a start bound for another pane reads that one.
        return match start_target(shared, channel, context).await {
            Ok(Some(target)) if !target.own_pane => native_wait(provider, target),
            Ok(_) => Wait::Go,
            Err(_) => named_wait(shared, channel, context),
        };
    }
    match start_target(shared, channel, context).await {
        Ok(Some(target)) => native_wait(provider, target),
        Ok(None) => Wait::Go,
        Err(_) => named_wait(shared, channel, context),
    }
}

/// The pane a start's headless turn writes to and where its native turn is read from; `None` when
/// that turn takes no pane, or for a provider whose native turn is not read here.
async fn start_target(
    shared: &Shared,
    channel: ChannelId,
    context: &Value,
) -> Result<Option<NativeTarget>, super::super::headless_turn::Unnamed> {
    if !matches!(shared.provider, ProviderKind::Claude | ProviderKind::Codex) {
        return Ok(None);
    }
    let pane = super::super::headless_turn::owner_start_pane(shared, channel, context);
    let Some((tmux, own_pane)) = pane.await? else {
        return Ok(None);
    };
    let (dir, session_id) = {
        let core = shared.core.lock().await;
        let session = core.sessions.get(&channel);
        let dir = session.and_then(|session| session.current_path.clone());
        (dir, session.and_then(|session| session.session_id.clone()))
    };
    Ok(Some(NativeTarget {
        tmux,
        dir,
        session_id: session_id.filter(|_| own_pane),
        own_pane,
    }))
}

/// A start whose pane is not named yet waits unclaimed; the watch names it again, and once named
/// wakes the drive, whose next look reads that pane.
fn named_wait(shared: &Shared, channel: ChannelId, context: &Value) -> Wait {
    let (shared, context) = (shared.clone(), context.clone());
    Wait::Watch(Box::new(move || {
        let pane = super::super::headless_turn::owner_start_pane(&shared, channel, &context);
        match tokio::runtime::Handle::current().block_on(pane) {
            Ok(_) => Screen::Ready,
            Err(_) => Screen::Unknown,
        }
    }))
}

/// A start's read of its own pane: a running or unreadable native turn holds it with a watch of
/// that pane; an idle or gone pane lets it go on.
fn native_wait(provider: &ProviderKind, target: NativeTarget) -> Wait {
    let provider = provider.clone();
    let read = move || native_read(&provider, &target);
    match read() {
        Screen::Blocked | Screen::Unknown => Wait::Watch(Box::new(read)),
        Screen::Ready | Screen::Gone => Wait::Go,
    }
}

/// The pane a start writes to; its session id is kept only when that pane is the channel's own.
struct NativeTarget {
    tmux: String,
    dir: Option<String>,
    session_id: Option<String>,
    own_pane: bool,
}

/// One read of a start's pane: gone (dead, absent or another account's), a running native turn, an
/// idle one, or unknown when the pane or its transcript cannot be read.
fn native_read(provider: &ProviderKind, target: &NativeTarget) -> Screen {
    use crate::services::tui_turn_state::{TuiTurnState, observe_claude_jsonl_turn_state};
    match crate::services::platform::tmux::pane_liveness(&target.tmux) {
        PaneLiveness::Live => {}
        PaneLiveness::DeadOrAbsent => return Screen::Gone,
        // A pane that could not be read is not gone; the watch reads it again.
        PaneLiveness::ProbeError => return Screen::Unknown,
    }
    if other_account(provider.as_str(), &target.tmux) {
        return Screen::Gone;
    }
    if *provider == ProviderKind::Codex {
        return codex_read(target);
    }
    let resolve = super::super::tui_followup::claude_followup_transcript;
    let (dir, session_id) = (target.dir.as_deref(), target.session_id.as_deref());
    let Some(path) = resolve(dir, session_id, Some(&target.tmux)) else {
        return Screen::Unknown;
    };
    match observe_claude_jsonl_turn_state(&path) {
        TuiTurnState::Idle => Screen::Ready,
        TuiTurnState::Streaming | TuiTurnState::UserSubmitted => Screen::Blocked,
        // Session markers with no tool waiting read idle once the composer would take a send.
        TuiTurnState::Unknown => {
            use crate::services::claude_tui::input::{
                final_prompt_ready, prompt_readiness_snapshot,
            };
            match final_prompt_ready(&prompt_readiness_snapshot(&target.tmux)) {
                true => Screen::Ready,
                false => Screen::Unknown,
            }
        }
    }
}

/// A live Codex pane is idle only once its rollout shows its latest turn's own end; a rollout with
/// no start or with any end it cannot attribute proves no idle.
fn codex_read(target: &NativeTarget) -> Screen {
    use crate::services::codex_tui::busy_inject::NativeEnd;
    let end = super::super::tui_followup::codex_native_end;
    match end(&target.tmux, target.session_id.as_deref()) {
        NativeEnd::Ended => Screen::Ready,
        NativeEnd::Open => Screen::Blocked,
        NativeEnd::NoStart | NativeEnd::Unknown => Screen::Unknown,
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
    // A routine, scheduled or HTTP start keeps its provenance; a user's input keeps none.
    let kept = match front.0.clone() {
        Some(ticket) => mailbox.v2_admission_context(front.1.clone(), ticket).await,
        None => Ok(None),
    };
    let kept = kept.map_err(effect::unsent)?;
    crate::services::discord::herdr_terminate::root_replaced::preclaim::before_idle(
        &shared,
        &shared.provider,
        channel,
    )
    .await?;
    // A start waits unclaimed behind a turn it did not start; that turn's idle wakes the owner.
    let wait = match &kept {
        Some(context) if !InputOrigin::from_context(Some(context)).is_person() => {
            start_waits(&shared, channel, context).await
        }
        Some(_) => Wait::Go,
        None => Wait::Go,
    };
    if let Wait::Watch(read) = wait {
        crate::services::discord::queue_io::input_owner::held_watch::arm(channel.get(), read);
        hold_start(&mailbox, channel, &front).await;
        return Err(START_WAITS.into());
    }
    if let Wait::Event = wait {
        return Err(START_WAITS.into());
    }
    let (held, holder) = (front.clone(), mailbox.clone());
    let claim = claim_turn(&mailbox, front.1.message_id, &front).await?;
    let claim = claim.ok_or("NotSent(\"front_not_granted\")")?;
    let (settled, answer) = tokio::sync::oneshot::channel();
    let origin = InputOrigin::from_context(kept.as_ref());
    let failed = super::start_failure::Failure::claimed(&mailbox, &claim);
    let refusal = std::sync::Arc::new(Mutex::new(None));
    let owner = OwnerTurn {
        mailbox,
        runtime: Some(shared.clone()),
        claim,
        settled,
        origin,
        late: LateRead::default(),
        refusal: refusal.clone(),
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
    #[cfg(test)]
    let owner = match tests::idle::root_recovery_launch(channel, owner, &intervention.text) {
        Ok(owner) => owner,
        Err(result) => return result,
    };
    let owner_name = crate::services::discord::queue_dispatch::kickoff::queued_owner_name;
    let name = owner_name(deps.http, intervention.author_id).await;
    let at = (shared.provider.clone(), channel);
    let started = match (kept, deps.ctx_for_chained_dispatch) {
        // Its session key, role, workspace and fresh boundary resolve as its legacy start's do.
        (Some(context), Some(ctx)) => {
            let start = super::super::headless_turn::start_owner_headless;
            let at = (ctx, &shared, deps.token);
            let started = start(at, channel, (&intervention.text, &context), owner).await;
            started.map(drop).map_err(|error| error.to_string())
        }
        (Some(_), None) => {
            drop(owner);
            Err("discord_unavailable".to_string())
        }
        (None, _) => {
            let start = crate::services::discord::router::start_owner_front;
            let started = start(&deps, at, &intervention, name, owner).await;
            started.map(drop).map_err(|error| error.to_string())
        }
    };
    let typed = refusal.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(refusal) = typed
        && let Some(kind) = super::start_failure::confirmed(&refusal)
    {
        if failed
            .commit(
                kind,
                refusal.to_string(),
                Ok(None),
                Some(shared.clone()),
                channel,
                None,
            )
            .await?
            .is_some()
        {
            return Err("NotSent(\"owner_start_failed\")".into());
        }
    }
    let turn_mode = crate::services::discord::router::intake_dispatch::TURN_MODE_AWAITED;
    match (answer.await, started) {
        (_, Err(error)) if error == WRITER_HELD => {
            hold_start(&holder, channel, &held).await;
            if crate::services::tui_o::writer::host::channel_accepts(channel.get())
                && intake_route::for_input_placement(shared.provider.as_str(), channel.get()).await
                    == IntakeRoute::Gateway
            {
                wake(&shared, channel);
            }
            Err(error)
        }
        // Held only by an awaited turn mode: watched by the confirmation that releases it.
        (_, Err(error)) if error == turn_mode => {
            let watch = crate::services::discord::router::intake_dispatch::turn_mode_watch;
            if let Some(read) = watch(channel.get()) {
                crate::services::discord::queue_io::input_owner::held_watch::arm(
                    channel.get(),
                    read,
                );
                hold_start(&holder, channel, &held).await;
            }
            Err(error)
        }
        // The late read held the claimed start: it waits as an unclaimed start would, watched.
        (Ok(Err(error)), _) if error == START_WAITS => {
            hold_start(&holder, channel, &held).await;
            Err(error)
        }
        (Ok(settlement), _) => settlement,
        (Err(_), Err(error)) => Err(format!("NotSent({error:?})")),
        (Err(_), Ok(_)) => Err("NotSent(\"turn_not_started\")".into()),
    }
}

/// A start's settlement while its pane's native turn runs or cannot be read: nothing was sent.
const START_WAITS: &str = "NotSent(\"synthetic_waits_idle\")";

/// Marks the start's front held so the drive watches the read a hold armed.
async fn hold_start(mailbox: &ChannelMailboxHandle, channel: ChannelId, front: &Front) {
    let held = front.clone();
    let held =
        mailbox.v2_request(move |slot| slot.as_mut().ok_or("V2 not adopted")?.hold_front(&held));
    if let Err(error) = held.await {
        tracing::warn!(channel_id = channel.get(), %error, "start hold unwatched");
    }
}

#[cfg(test)]
pub(in crate::services::discord) async fn start_idle_for_test(
    shared: Shared,
    channel: ChannelId,
    front: Front,
) -> Result<(), String> {
    start_idle(shared, channel, front).await
}

#[cfg(test)]
pub(in crate::services::discord) use tests::idle::{
    RootRecoveryDriveEvent, claimed_root_recovery_launch, install_root_recovery_drive_probe,
    install_root_recovery_launch,
};

/// An owner front's turn, registered from this turn's own host and pane: a running Claude pane
/// takes it through the busy entry, an idle pane's AI turn is read on this turn's sender.
pub(super) fn execute_owned(
    turn: StreamingTurn<'_>,
    sender: std::sync::mpsc::Sender<crate::services::agent_protocol::StreamMessage>,
    owner: OwnerTurn,
) -> Result<(), String> {
    use crate::services::platform::tmux::pane_liveness;
    let OwnerTurn {
        mailbox,
        runtime,
        claim,
        settled,
        origin,
        late,
        ..
    } = owner;
    let turn = StreamingTurn {
        shared: turn.shared.or(runtime.as_ref()),
        ..turn
    };
    let session = turn.tmux_session_name.unwrap_or_default();
    // A start never joins a running or unreadable native turn: read once before it registers, it
    // waits unsent with a watch of that pane.
    let synthetic = !origin.is_person()
        && matches!(turn.host, TurnHost::Tmux)
        && matches!(turn.provider, ProviderKind::Claude | ProviderKind::Codex);
    // Its idle or gone read is the pane it registers; no later read can turn it unobserved.
    let mut read = None;
    if synthetic {
        let target = NativeTarget {
            tmux: session.to_string(),
            dir: Some(turn.working_dir.to_string()).filter(|dir| !dir.is_empty()),
            session_id: (turn.session_id.filter(|_| late.own_pane)).map(str::to_string),
            own_pane: late.own_pane,
        };
        let screen = native_read(turn.provider, &target);
        read = Some(screen == Screen::Ready);
        if let Screen::Blocked | Screen::Unknown = screen {
            let nonce = turn.cancel.turn_nonce().map(str::to_string);
            let at = (turn.channel_id, nonce);
            let probe = late_probe(turn.provider.clone(), target, mailbox.clone(), at);
            crate::services::discord::queue_io::input_owner::held_watch::arm(
                turn.channel_id,
                probe,
            );
            let _ = settled.send(Err(START_WAITS.into()));
            return Err(START_WAITS.into());
        }
    }
    if late.clear_binding
        && let Some(tmux) = turn.tmux_session_name
    {
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(tmux);
    }
    let live = read.unwrap_or_else(|| {
        matches!(turn.host, TurnHost::Tmux)
            && pane_liveness(session) == PaneLiveness::Live
            && !other_account(turn.provider.as_str(), session)
    });
    let running = (live && !synthetic)
        .then(|| running_transcript(&turn, session))
        .flatten();
    let registered = match &running {
        Some(transcript) => {
            let kind = (turn.provider.as_str(), origin, true);
            let at = (
                turn.host,
                session,
                Some((transcript.as_str(), turn.session_id)),
            );
            Registration::observed(mailbox, kind, at, &claim.front().1, None).map(|mut owner| {
                owner.until_done = busy_observer(&turn, &sender, &mut owner);
                owner
            })
        }
        None => {
            let observer = live.then(|| observer(&turn, &sender)).flatten();
            let kind = (turn.provider.as_str(), origin, false);
            let at = (turn.host, session, None);
            let front = &claim.front().1;
            Registration::read_as(mailbox, kind, at, front, (observer, read))
        }
    };
    match registered {
        Ok(owner) => execute_front(turn, sender, claim, owner, settled),
        Err(error) => {
            let _ = settled.send(Err(error.clone()));
            Err(error)
        }
    }
}

/// The late hold's read of its pane: ready only once the held attempt's own unwind ended (its
/// mailbox turn released, its inflight row gone), so the one wake is not spent on its leftovers.
fn late_probe(
    provider: ProviderKind,
    target: NativeTarget,
    mailbox: ChannelMailboxHandle,
    (channel, nonce): (u64, Option<String>),
) -> crate::services::discord::queue_io::input_owner::held_watch::ScreenProbe {
    use crate::services::discord::inflight::load_inflight_state_read_only_result as row_now;
    Box::new(move || match native_read(&provider, &target) {
        Screen::Ready => {
            let active = futures::executor::block_on(mailbox.has_active_turn());
            let own = |row: &crate::services::discord::inflight::InflightTurnState| {
                nonce.is_some() && row.turn_nonce == nonce
            };
            match (active, row_now(&provider, channel)) {
                (Ok(false), Ok(row)) if !row.as_ref().is_some_and(own) => Screen::Ready,
                _ => Screen::Unknown,
            }
        }
        read => read,
    })
}

/// The transcript of a Claude tmux pane whose turn is running now, resolved by the pane's identity.
fn running_transcript(turn: &StreamingTurn<'_>, session: &str) -> Option<String> {
    (*turn.provider == ProviderKind::Claude).then_some(())?;
    let dir = Some(turn.working_dir).filter(|dir| !dir.is_empty());
    let resolve = super::super::tui_followup::claude_followup_transcript;
    let path = resolve(dir, turn.session_id, Some(session))?;
    let state = crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&path);
    state.is_busy().then(|| path.display().to_string())
}

/// The reader of a busy paste's answer, from the row its attempt takes to that turn's end.
fn busy_observer(
    turn: &StreamingTurn<'_>,
    sender: &std::sync::mpsc::Sender<crate::services::agent_protocol::StreamMessage>,
    owner: &mut Registration,
) -> Option<UntilDone> {
    let nonce = owner.attempt["nonce"].as_str()?;
    let frame = busy_inject::frame("discord", &owner.author, &owner.name, nonce, turn.prompt);
    let at = (
        turn.tmux_session_name?,
        nonce,
        frame.as_str(),
        turn.channel_id,
    );
    let cancel = std::sync::Arc::clone(&turn.cancel);
    let submitted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    owner.native_pin_submitted = Some(submitted.clone());
    let proof = crate::services::discord::inflight::BusyNativeInput::capture(
        at.0,
        nonce,
        &frame,
        &owner.transcript,
        &owner.attempt["source"],
        submitted,
        cancel.clone(),
    );
    let on_native_user = Box::new(move |offset| {
        if let Some(proof) = proof {
            proof.adopt(offset);
        }
    });
    let observe = crate::services::claude_tui::hosting::busy_owner_observer;
    observe(
        at,
        &owner.transcript,
        sender.clone(),
        cancel,
        on_native_user,
    )
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
pub(super) type Reply = std::sync::Arc<dyn Fn(&Result<(), String>) + Send + Sync>;

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
    let failure = super::start_failure::Failure::claimed(&owner.mailbox, &claim);
    let watch =
        crate::services::discord::inflight::row_removed::watch(turn.provider, turn.channel_id);
    let context = turn.clone();
    let inside = failure.guard_reply(reply.clone());
    let result = match take_owned(&turn, Some(claim), owner, inside) {
        Some(Taken::Done(result)) => result,
        Some(Taken::Launch(guard)) => guard.finish(super::drive(turn, sender)),
        None => Err("NotSent(\"not_owned\")".into()),
    };
    let result = failure.finish(&context, watch, result);
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
        // `execute` brings a returning session back first; one still returning is never written.
        TurnHost::Return(_) => return Some(Taken::Done(Err("NotSent(\"host_returning\")".into()))),
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
    let declared = until_done.is_some();
    // Only adapters that consume the first-key gate can supply its stopped-before-key proof.
    let gate = matches!(route, target::Route::ClaudeIdle | target::Route::ClaudeBusy)
        .then(|| turn.cancel.native_stop.receipt_sink());
    let native_pin_submitted = registration.native_pin_submitted.clone();
    let busy = matches!(
        route,
        target::Route::ClaudeBusy | target::Route::CodexBusy | target::Route::CodexHerdrBusy
    );
    let originals: Vec<_> = front
        .as_ref()
        .map(|claim| {
            std::iter::once(claim.front().1.clone())
                .chain(claim.followers().iter().cloned())
                .collect()
        })
        .unwrap_or_default();
    let fallback = registration.sources.clone();
    let result = run(turn, (route, declared), registration, front);
    if busy && let Some(shared) = turn.shared {
        let note = super::super::busy_retry::note_busy_result;
        tokio::runtime::Handle::current().block_on(note(
            shared,
            turn.provider,
            ChannelId::new(turn.channel_id),
            &originals,
            &fallback,
            &result,
        ));
    }
    if result.is_ok()
        && let Some(submitted) = native_pin_submitted
    {
        submitted.store(true, std::sync::atomic::Ordering::Release);
    }
    reply(&result);
    let result = match (result, until_done) {
        (Ok(()), Some(until_done)) => until_done(),
        // A write that may have landed starts an AI turn, so the capacity is held to its end too;
        // a stop that took the Claude writer's gate before its first key started none.
        (Err(error), Some(until_done))
            if !effect::surely_unsent(&error)
                && !gate.as_ref().is_some_and(|gate| gate.stopped_before_key()) =>
        {
            until_done().and(Err(error))
        }
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

/// Writes the prepared input on `route`; `declared` says a reader set up before the write answers it.
fn run(
    turn: &StreamingTurn<'_>,
    (route, declared): (target::Route, bool),
    r: Registration,
    front: Option<FrontClaim>,
) -> Result<(), String> {
    let nonce = r.attempt["nonce"].as_str().unwrap_or_default().to_string();
    let session = || {
        turn.tmux_session_name
            .ok_or("NotSent(\"owner input without a pane\")")
    };
    let (frame, leaf) = match route {
        // The busy frame carries main's header, so the prepared bytes are the ones pasted.
        target::Route::ClaudeBusy => (
            busy_inject::frame("discord", &r.author, &r.name, &nonce, turn.prompt),
            BusyClaude {
                operation: BusyOperation::for_session(
                    session()?,
                    r.herdr,
                    r.program,
                    r.timing,
                    declared,
                    turn.cancel.clone(),
                )?,
                session: session()?.into(),
                transcript: r.transcript,
                channel: turn.channel_id,
                source: "discord".into(),
                author: r.author,
                name: r.name,
                nonce,
                text: turn.prompt.into(),
                draft_notice: draft_notice(turn, &r.sources),
            }
            .leaf(),
        ),
        target::Route::ClaudeIdle => (
            turn.prompt.into(),
            effect::idle_claude(
                busy_inject::Pane::with_program(session()?, r.program)?,
                session()?.into(),
                turn.channel_id,
                turn.cancel.clone(),
            ),
        ),
        target::Route::CodexIdle => (
            codex_herdr_frame(turn.prompt)
                .map_err(|error| format!("NotSent(\"invalid_prompt: {error}\")"))?,
            effect::idle_codex(
                busy_inject::Pane::with_program(session()?, r.program)?,
                session()?.into(),
                turn.channel_id,
            ),
        ),
        target::Route::CodexBusy | target::Route::CodexHerdrBusy => {
            // A readable rollout supplies a diagnostic turn, not permission to deliver.
            use crate::services::codex_tui::busy_inject::{TIMING, TurnVerdict, read_turn};
            let target_turn = match read_turn(&r.transcript) {
                Some(TurnVerdict::KnownModelTurn(turn)) => turn,
                Some(_) => String::new(),
                None => return Err("NotSent(\"busy_turn: transcript unavailable\")".into()),
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
                    pane: busy_inject::Pane::with_program(session()?, r.program)?,
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
                (_, None) => return Err("NotSent(\"owner Herdr input without a pane\")".into()),
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

/// Posts the original's one notice that a draft beside its paste may not be kept; the outbox key
/// is the original's, so a later attempt of it posts nothing.
fn draft_notice(turn: &StreamingTurn<'_>, sources: &[String]) -> Option<effect::DraftNotice> {
    let pool = turn.pool?.clone();
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    let (provider, channel) = (turn.provider.as_str().to_string(), turn.channel_id);
    let sources = sources.to_vec();
    Some(Box::new(move || {
        runtime.spawn(async move {
            let post = crate::services::discord::queue_io::retained_notice::draft_unproven;
            if let Err(error) = post(&pool, &provider, channel, &sources).await {
                tracing::warn!(channel_id = channel, %error, "draft notice not enqueued");
            }
        });
    }))
}

#[cfg(test)]
#[path = "owner_input_codex_tests.rs"]
mod codex_tests;
#[cfg(test)]
#[path = "owner_input_herdr_tests.rs"]
mod herdr_tests;
#[cfg(test)]
#[path = "owner_input_local_control_tests.rs"]
mod local_control_tests;
#[cfg(test)]
#[path = "owner_input_tests.rs"]
pub(super) mod tests;
