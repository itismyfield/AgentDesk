use super::owner_input::OwnerTurn;
use super::*;
use crate::services::claude::herdr_turn::retry_probe::CauseRead;
use crate::services::claude_tui::busy_inject::Screen;
use crate::services::discord::inflight::row_removed;
use crate::services::discord::queue_io::input_owner::drive::{Front, FrontClaim};
use crate::services::discord::queue_io::input_owner::held_watch;
use crate::services::turn_orchestrator::{ChannelMailboxHandle, PendingQueueItem, RetryWaitCause};

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum Kind {
    Opaque,
    ProviderUnsupported,
    EndpointNotLocal,
    ExecutorNotWired,
    HostedHerdrUnconfigured,
}
pub(in crate::services::discord) fn confirmed(refusal: &HerdrRefusal) -> Option<Kind> {
    use crate::db::dispatched_sessions::hosted_execution::HostedState;
    match refusal {
        HerdrRefusal::ProviderUnsupported { .. } => Some(Kind::ProviderUnsupported),
        HerdrRefusal::EndpointNotLocal { .. } => Some(Kind::EndpointNotLocal),
        HerdrRefusal::ExecutorNotWired => Some(Kind::ExecutorNotWired),
        HerdrRefusal::HostedHerdrUnconfigured {
            state: Some(HostedState::Pending | HostedState::Bound),
        } => Some(Kind::HostedHerdrUnconfigured),
        _ => None,
    }
}
type Shared = Arc<crate::services::discord::SharedData>;

#[derive(Clone, Default)]
pub(super) struct MissingRecovery(Arc<std::sync::atomic::AtomicBool>);
impl MissingRecovery {
    pub(super) fn repaired(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
    pub(super) fn mark(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// A restart's greeting starts only under its permit; a refusal retains it with its notice.
pub(super) async fn greeting_permit(
    shared: &Shared,
    channel: poise::serenity_prelude::ChannelId,
    kept: Option<&serde_json::Value>,
    failed: Failure,
) -> Result<Failure, String> {
    use crate::services::discord::queue_io::input_owner::origin::InputOrigin;
    let Some(context) =
        kept.filter(|kept| InputOrigin::from_context(Some(kept)) == InputOrigin::RestartSeed)
    else {
        return Ok(failed);
    };
    let permit = crate::services::discord::commands::herdr_seed_permit;
    let Err(reason) = permit(shared, channel, context).await else {
        return Ok(failed);
    };
    let detail = format!("restart greeting refused: {reason}");
    let (kind, shared) = (Kind::ExecutorNotWired, Some(shared.clone()));
    failed
        .commit(kind, detail, Ok(None), shared, channel, None)
        .await?;
    Err("NotSent(\"owner_start_failed\")".into())
}

#[derive(Clone)]
pub(super) struct Failure {
    mailbox: ChannelMailboxHandle,
    front: Front,
    number: u64,
    members: Vec<PendingQueueItem>,
    recovery: MissingRecovery,
    notice: Arc<tokio::sync::Mutex<Option<String>>>,
}
impl Failure {
    pub(super) fn claimed(mailbox: &ChannelMailboxHandle, claim: &FrontClaim) -> Self {
        Self {
            recovery: MissingRecovery::default(),
            notice: Arc::default(),
            mailbox: mailbox.clone(),
            front: claim.front().clone(),
            number: claim.number(),
            members: std::iter::once(claim.front().1.clone())
                .chain(claim.followers().iter().cloned())
                .collect(),
        }
    }
    pub(super) fn owner(owner: &OwnerTurn) -> Self {
        Self::claimed(owner.mailbox(), owner.claim()).with_recovery(owner.recovery.clone())
    }
    pub(super) fn with_recovery(mut self, recovery: MissingRecovery) -> Self {
        self.recovery = recovery;
        self
    }

    pub(super) async fn discard_recovered(
        &self,
        shared: &Shared,
        channel: poise::serenity_prelude::ChannelId,
        started: &Result<(), String>,
    ) -> Result<bool, String> {
        if self.recovery.repaired()
            && let Err(error) = started
        {
            self.clone()
                .discard(Some(shared.clone()), channel, error.clone())
                .await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Only the still unreserved exact claim is disposed; a reserve owns its own settlement.
    pub(super) async fn discard(
        self,
        shared: Option<Shared>,
        channel: poise::serenity_prelude::ChannelId,
        detail: String,
    ) -> Result<Option<bool>, String> {
        let notice = self.notice.clone();
        let mut notice = notice.lock().await;
        let anchor = poise::serenity_prelude::MessageId::new(self.front.1.message_id);
        let number = self.number;
        let done = self
            .mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().ok_or("missing owner")?;
                if owner.claim_number() != self.number
                    || !owner.claimed(self.front.1.message_id, &self.front)
                {
                    return Ok(None);
                }
                owner
                    .complete_claimed_locally((&self.front, self.number), self.members.len() - 1)?;
                Ok(Some(true))
            })
            .await?;
        if done.is_some() {
            *notice = Some(detail);
        }
        if let Some(detail) = notice.as_deref() {
            let shared = shared.ok_or("recovery refusal notice runtime unavailable")?;
            let message = crate::services::discord::session_notice::subtext(
                "⚠️",
                &format!("실행을 복구하지 못해 이 입력을 폐기했습니다. 사유: {detail}"),
            );
            let delivered = match shared.serenity_http_or_token_fallback() {
                Some(http) => crate::services::discord::outbound::serenity_reference::send_referenced_lifecycle_notice(
                    http, shared.clone(), channel, anchor, &message,
                    "hosted-recovery-refusal".into(), format!("claim-{number}"),
                ).await.is_ok_and(|message| message.is_some()),
                None => false,
            };
            if !delivered {
                let pool = shared
                    .pg_pool
                    .as_ref()
                    .ok_or("recovery refusal notice outbox unavailable")?;
                let id = crate::services::discord::outbound::recovery_notice::enqueue(
                    pool,
                    shared.provider.as_str(),
                    channel,
                    anchor,
                    number,
                    &message,
                )
                .await?;
                tracing::info!(outbox_id = id, "recovery refusal reason handed to outbox");
            }
            *notice = None;
        }
        Ok(done)
    }

    pub(super) fn guard_reply(
        &self,
        reply: super::owner_input::Reply,
        token: Arc<CancelToken>,
    ) -> super::owner_input::Reply {
        let (mailbox, front, number) = (self.mailbox.clone(), self.front.clone(), self.number);
        Arc::new(move |result| {
            let front = front.clone();
            // An unreserved exact claim waits for the failure commit, and a refusal no key followed
            // for its row's rewind even after a reserve spent the claim; EffectOwned settles now.
            let unsent = result
                .as_ref()
                .is_err_and(|error| error.starts_with("NotSent("));
            let untouched =
                unsent && crate::services::provider::herdr_before_start::ended_untouched(&token);
            let before_input = result.is_err()
                && (untouched
                    || tokio::runtime::Handle::current()
                        .block_on(mailbox.v2_request(move |slot| {
                            Ok(slot.as_ref().is_some_and(|owner| {
                                owner.claim_number() == number
                                    && owner.claimed(front.1.message_id, &front)
                            }))
                        }))
                        .unwrap_or(true));
            if !before_input {
                reply(result);
            }
        })
    }
    pub(super) async fn commit(
        self,
        kind: Kind,
        detail: String,
        watch: Result<Option<row_removed::Watch>, String>,
        shared: Option<Shared>,
        channel: poise::serenity_prelude::ChannelId,
        token: Option<Arc<CancelToken>>,
    ) -> Result<Option<bool>, String> {
        if self.recovery.repaired() {
            return self.discard(shared, channel, detail).await;
        }
        let opaque = matches!(kind, Kind::Opaque);
        let (watch, unreadable) = match watch {
            Ok(watch) => (watch, None),
            Err(error) => (None, Some(error)),
        };
        let pin = watch.as_ref().map(row_removed::Watch::key);
        let provider = shared
            .as_ref()
            .map(|shared| shared.provider.as_str().to_string());
        let proof_token = token.clone();
        let mailbox = self.mailbox.clone();
        let members = self.members.clone();
        let decision = mailbox
            .v2_request(move |slot| {
                let kind =
                    serde_json::from_value(serde_json::to_value(kind).map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?;
                let owner = slot.as_mut().ok_or("missing owner")?;
                // A reserve owns every result, even when no failed-wrapper row proof can be read.
                if owner.claim_number() != self.number
                    || !owner.claimed(self.front.1.message_id, &self.front)
                {
                    return Ok(None);
                }
                tracing::warn!(channel_id=channel.get(), front_id=self.front.1.message_id,
                    stage="provider_start", error=%detail, "owner launch failed before reserve");
                if let Some(error) = unreadable {
                    return Err(format!("failure row cannot be captured: {error}"));
                }
                if proof_token.is_some() && pin.is_none() {
                    return Err("wrapper failure lacks its fresh row identity".into());
                }
                if let Some(pin) = &pin {
                    let token = proof_token
                        .as_ref()
                        .ok_or("wrapper failure lacks its token")?;
                    if pin["anchor"] != self.front.1.message_id
                        || pin["channel"] != channel.get()
                        || provider
                            .as_deref()
                            .is_some_and(|provider| pin["provider"] != provider)
                        || pin["nonce"].as_str() != token.turn_nonce()
                    {
                        return Err("failure row does not name the producer episode".into());
                    }
                }
                if opaque && owner.handoff_start_failed(&self.front, self.number, pin.clone())? {
                    return Ok(Some(false));
                }
                owner.start_failed(&self.front, self.number, &self.members, pin, kind, &detail)
            })
            .await?;
        if decision == Some(true)
            && let Some(shared) = shared.as_ref()
        {
            use crate::services::discord::turn_view_reconciler::{
                TurnViewIdentity, TurnViewTarget, turn_view_owner_for_message,
            };
            let now = (std::time::SystemTime::now(), std::time::Instant::now());
            for item in members {
                let input = crate::services::turn_orchestrator::pending_queue_item_to_intervention(
                    item, now.0, now.1,
                );
                for source in input.source_message_queued_generations() {
                    shared
                        .turn_view_reconciler
                        .note_queue_marker_removed(
                            shared,
                            TurnViewTarget::intake_user_message(channel, source.message_id),
                            turn_view_owner_for_message(
                                channel,
                                source.message_id,
                                source.queued_generation,
                            ),
                            TurnViewIdentity::IntakeShared,
                            crate::services::discord::queue_reactions::QUEUE_STANDALONE_PENDING_REACTION,
                            "start_failure_retained",
                        )
                        .await;
                }
            }
        }
        if decision.is_some() {
            if let Some(shared) = shared {
                tokio::spawn(async move {
                    let wake = if let Some(watch) = watch {
                        let pin = watch.key();
                        #[cfg(test)]
                        let nonce = token
                            .as_ref()
                            .and_then(|token| token.turn_nonce())
                            .unwrap_or_default()
                            .to_string();
                        let row = async {
                            watch.removed().await;
                            #[cfg(test)]
                            fact(channel, &nonce, "row_removed");
                        };
                        let released = async {
                            let result = token_released(
                                &shared,
                                channel,
                                &mailbox,
                                token.as_ref().expect("captured wrapper token"),
                            )
                            .await;
                            #[cfg(test)]
                            if result.is_ok() {
                                fact(channel, &nonce, "token_released");
                            }
                            result
                        };
                        let ((), released) = tokio::join!(row, released);
                        if let Err(error) = released {
                            tracing::warn!(channel_id=channel.get(),%error,"failed episode remains closing");
                            return;
                        }
                        mailbox
                            .v2_request(move |slot| {
                                Ok(slot
                                    .as_mut()
                                    .is_some_and(|owner| owner.start_cleanup_completed(&pin)))
                            })
                            .await
                            .unwrap_or(false)
                    } else {
                        true
                    };
                    if wake {
                        match crate::services::discord::queue_io::input_owner::claude_handoff::retry_start(&shared, channel).await {
                            Ok(true) => return,
                            Ok(false) => (),
                            Err(error) => {
                                tracing::warn!(channel_id=channel.get(), %error, "handoff retry remains held");
                                return;
                            }
                        }
                        super::owner_input::wake(&shared, channel);
                    }
                });
            }
        }
        #[cfg(test)]
        if decision.is_some() {
            let gate = {
                let mut slot = COMMIT_GATE.lock().unwrap();
                if slot.as_ref().is_some_and(|(id, _, _)| *id == channel.get()) {
                    slot.take()
                } else {
                    None
                }
            };
            if let Some((_, entered, release)) = gate {
                let _ = entered.send(());
                release.await.expect("release committed start failure");
            }
        }
        Ok(decision)
    }
    pub(super) fn finish(
        self,
        turn: &StreamingTurn<'_>,
        watch: Result<Option<row_removed::Watch>, String>,
        result: Result<(), String>,
    ) -> Result<(), String> {
        let read = crate::services::claude::herdr_turn::take_retry_probe();
        let Err(error) = result else { return Ok(()) };
        let bound = crate::services::claude::herdr_turn::take_owner_bound_failure();
        let cancelled = turn
            .cancel
            .cancelled
            .load(std::sync::atomic::Ordering::Acquire);
        let before_key = turn.cancel.native_stop.receipt_sink().stopped_before_key();
        let stopped = cancelled || before_key;
        let unsent = !self.recovery.repaired() && !bound && error.starts_with("NotSent(");
        if self.herdr_seed_stopped(&turn.cancel) {
            return Err(error);
        }
        // A refusal typed by a cause a later look may clear waits; it counts no failed start.
        if unsent
            && !stopped
            && let Some(cause) = RetryWaitCause::from_refusal(&error)
        {
            self.wait_for_retry(turn.channel_id, cause, read);
        }
        if stopped || unsent {
            // A cleanup cancel (the producer's own pre-key unwind) publishes no cause; a stop does,
            // and keeps the row for its own settlement.
            let cleanup =
                turn.cancel.is_completion_cleanup() && turn.cancel.cancel_source_kind().is_none();
            if unsent && !before_key && (!cancelled || cleanup) {
                herdr_unsent_unwind::unwind(turn, self.front.1.message_id);
            }
            return Err(error);
        }
        let kind = match turn.host {
            _ if self.recovery.repaired() => Kind::Opaque,
            TurnHost::Refused(refusal) => match confirmed(refusal) {
                Some(kind) => kind,
                None => return Err(error),
            },
            _ => Kind::Opaque,
        };
        let shared = turn.shared.cloned();
        match tokio::runtime::Handle::current().block_on(self.commit(
            kind,
            error.clone(),
            watch,
            shared,
            poise::serenity_prelude::ChannelId::new(turn.channel_id),
            Some(turn.cancel.clone()),
        )) {
            Ok(None) => Err(error),
            Ok(Some(_)) | Err(_) => Err("NotSent(\"owner_start_failed\")".into()),
        }
    }
}
impl Failure {
    /// A Herdr user stop before the reserve cancels the claimed restart greeting instead of its
    /// retry or failure; another front, or no stop, keeps its failure path.
    fn herdr_seed_stopped(&self, cancel: &crate::services::provider::CancelToken) -> bool {
        let stop = cancel.herdr_interrupt_state();
        if !stop.is_some_and(|s| s.user_stop.load(std::sync::atomic::Ordering::Acquire)) {
            return false;
        }
        let (front, number) = (self.front.clone(), self.number);
        let authority =
            crate::services::turn_orchestrator::SeedStopAuthority::Claimed { front, number };
        let cancelled = self.mailbox.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("V2 not adopted")?;
            owner.cancel_seed_not_sent(authority)
        });
        tokio::runtime::Handle::current()
            .block_on(cancelled)
            .is_ok()
    }

    /// The exact claim waits as `cause`; `read` then watches that cause for the drive.
    fn wait_for_retry(&self, channel: u64, cause: RetryWaitCause, read: Option<CauseRead>) {
        let (front, number) = (self.front.clone(), self.number);
        let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
        let entered = self.mailbox.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("V2 not adopted")?;
            owner.enter_retry_wait_claimed((&front, number), cause, now)
        });
        match tokio::runtime::Handle::current().block_on(entered) {
            Ok(Some(wait_seq)) => {
                if let Some(read) = read {
                    let (mailbox, message) = (self.mailbox.clone(), self.front.1.message_id);
                    held_watch::arm(channel, released(mailbox, message, wait_seq, read));
                }
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(channel_id = channel, %error, "retry wait not recorded"),
        }
    }
}

/// Ends wait `wait_seq` of front `message` once `read` sees its cause cleared; a wait that moved
/// on since ends the watch instead.
fn released(
    mailbox: ChannelMailboxHandle,
    message: u64,
    wait_seq: u64,
    mut read: CauseRead,
) -> held_watch::ScreenProbe {
    Box::new(move || match read() {
        Screen::Ready => {
            let release = mailbox.v2_request(move |slot| {
                let owner = slot.as_mut().ok_or("V2 not adopted")?;
                owner.release_retry_wait(message, wait_seq)
            });
            match tokio::runtime::Handle::current().block_on(release) {
                Ok(true) => Screen::Ready,
                Ok(false) => Screen::Gone,
                Err(_) => Screen::Unknown,
            }
        }
        other => other,
    })
}

pub(super) fn run(
    turn: StreamingTurn<'_>,
    start: ReplayStart,
    sender: Sender<StreamMessage>,
    owner: Option<OwnerTurn>,
) -> Result<(), String> {
    let failure = owner.as_ref().map(Failure::owner);
    if failure.is_none() {
        return super::execute_inner(turn, start, sender, owner);
    }
    crate::services::claude::herdr_turn::take_owner_bound_failure();
    let watch = row_removed::watch(turn.provider, turn.channel_id);
    let context = turn.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        super::execute_inner(turn, start, sender, owner)
    }));
    let result = match result {
        Ok(result) => result,
        Err(info) => match producer_panic("owner streaming", info) {
            StreamMessage::Error { message, .. } => Err(message),
            _ => unreachable!(),
        },
    };
    #[cfg(test)]
    {
        let gate = {
            let mut slot = BRIDGE_GATE.lock().unwrap();
            if slot
                .as_ref()
                .is_some_and(|(id, _, _)| *id == context.channel_id)
            {
                slot.take()
            } else {
                None
            }
        };
        if let Some((_, entered, release)) = gate {
            let _ = entered.send(());
            tokio::runtime::Handle::current()
                .block_on(release)
                .expect("release producer bridge frame");
        }
    }
    failure
        .expect("owner failure context")
        .finish(&context, watch, result)
}

#[cfg(test)]
type CommitGate = (
    u64,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
#[cfg(test)]
pub(in crate::services::discord) static COMMIT_GATE: std::sync::Mutex<Option<CommitGate>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(in crate::services::discord) static BRIDGE_GATE: std::sync::Mutex<Option<CommitGate>> =
    std::sync::Mutex::new(None);

async fn token_released(
    shared: &Shared,
    channel: poise::serenity_prelude::ChannelId,
    mailbox: &ChannelMailboxHandle,
    expected: &Arc<CancelToken>,
) -> Result<(), String> {
    let finished = shared.mailboxes.turn_finished(channel);
    let wait = finished.wait();
    tokio::pin!(wait);
    let ready = futures::poll!(&mut wait).is_ready();
    let active = mailbox.cancel_token().await.map_err(|e| format!("{e:?}"))?;
    if active
        .as_ref()
        .is_some_and(|active| Arc::ptr_eq(active, expected))
    {
        if !ready {
            wait.await;
        }
        let active = mailbox.cancel_token().await.map_err(|e| format!("{e:?}"))?;
        if active
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, expected))
        {
            return Err("turn-finished signal did not release the failed token".into());
        }
    }
    Ok(())
}
mod herdr_unsent_unwind;

#[cfg(test)]
type FactSender = (
    u64,
    tokio::sync::mpsc::UnboundedSender<(String, &'static str)>,
);
#[cfg(test)]
pub(in crate::services::discord) static FACTS: std::sync::Mutex<Option<FactSender>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
fn fact(channel: poise::serenity_prelude::ChannelId, nonce: &str, event: &'static str) {
    if let Some((_, sender)) = FACTS
        .lock()
        .unwrap()
        .as_ref()
        .filter(|(id, _)| *id == channel.get())
    {
        let _ = sender.send((nonce.to_string(), event));
    }
}
#[cfg(all(test, unix))]
#[path = "start_failure_state_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "start_failure_missing_tests.rs"]
mod missing_tests;

#[cfg(all(test, unix))]
#[path = "start_failure_unwind_tests.rs"]
mod unwind_tests;

#[cfg(all(test, unix))]
#[path = "start_failure_seed_tests.rs"]
mod seed_tests;
