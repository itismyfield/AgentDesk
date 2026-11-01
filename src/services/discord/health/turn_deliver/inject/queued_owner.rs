//! The queued-head owner: a head already proven taken is consumed first, otherwise the first ready
//! input is reserved, its paste recorded before it can start, typed once and settled by the actor.
use super::queued_provider::QueuedProvider;
use super::queued_support::ATTEMPT_UNNAMED;
use super::*;

pub(super) async fn queued_attempt(
    shared: &Arc<SharedData>,
    request: &HumanInputRequest,
    expected: QueueIdentity,
    parent: Option<ChannelId>,
    gate: fn(ChannelId, Option<ChannelId>) -> bool,
    source: Option<(String, SourceGuard)>,
) -> InjectAttempt {
    #[cfg(test)]
    if shared
        .mailbox(request.channel_id)
        .v2_request(|slot| Ok(slot.is_some()))
        .await
        .unwrap_or(false)
    {
        return v2_queued_attempt(shared, request, expected, source).await;
    }
    // A head already proven taken is consumed before the busy check, so an idle pane is no veto.
    if let Some(mended) = super::queued_reconcile::mend(shared, request, parent, gate).await {
        return mended;
    }
    let Some(provider) = QueuedProvider::select(&request.provider, request.channel_id.get()) else {
        return InjectAttempt::NotSent("provider_unsupported");
    };
    // Named before the session transition is held, which nothing then waits on.
    let primary = expected.primary;
    let named = match source.as_ref() {
        Some((name, _)) => Ok(name.clone()),
        None => author_name(shared, request.channel_id, primary, request.author_id).await,
    };
    let name = match named {
        Ok(name) => name,
        Err(veto) => return InjectAttempt::NotSent(veto),
    };
    let target = match resolve(shared, request, true).await {
        Ok(target) => target,
        Err(veto) => return InjectAttempt::NotSent(veto),
    };
    let shared = shared.clone();
    let request = HumanInputRequest {
        channel_id: request.channel_id,
        provider: request.provider.clone(),
        text: request.text.clone(),
        author_id: request.author_id,
        source: request.source.clone(),
        metadata: request.metadata.clone(),
        channel_name_hint: request.channel_name_hint.clone(),
    };
    // The detached owner outlives a cancelled request through claim, pane I/O and durable settle.
    let owner = async move {
        let _source = source;
        let mailbox = shared.mailbox(request.channel_id);
        let persistence = crate::services::discord::queue_persistence_context(
            &shared,
            &request.provider,
            request.channel_id,
        );
        let held = mailbox
            .claim_queued_head_if_enabled(
                expected,
                persistence.clone(),
                request.channel_id,
                parent,
                gate,
            )
            .await;
        let held = match held {
            QueuedHeadClaim::Reserved(ticket) => ticket,
            QueuedHeadClaim::NotEligible => return InjectAttempt::NotSent("queued_not_eligible"),
            QueuedHeadClaim::PersistenceFailed(error) => return persistence_failed(error),
            QueuedHeadClaim::BlockedUnknown => return InjectAttempt::HandbackFailed(HEAD_UNKNOWN),
            _ => return InjectAttempt::HandbackFailed("queued_head_blocked"),
        };
        let item = held.item();
        let source_generations = item.source_message_queued_generations();
        let turn_id = Some(format!(
            "discord:{}:{}",
            request.channel_id, item.message_id
        ));
        let input = Arc::new(Input {
            channel: request.channel_id.get(),
            provider: request.provider.as_str().to_string(),
            source: turn_id.clone().unwrap(),
            author: item.author_id.to_string(),
            name,
            text: item.text.clone(),
            nonce: busy_inject::fresh_nonce(),
        });
        let named = item.author_id.get() == request.author_id && item.message_id.get() == primary;
        let mut ticket = Some(held);
        let work = async {
            // The name was found for another input than the one claimed.
            if !named {
                return InjectAttempt::NotSent(AUTHOR_MISMATCH);
            }
            #[cfg(test)]
            test_hook::before_reserve(input.channel).await;
            if !queued_target_matches(&shared, &request, &target, true).await {
                return InjectAttempt::NotSent(HOLDER_CHANGED);
            }
            #[cfg(test)]
            test_hook::owner_crash(input.channel);
            #[cfg(test)]
            test_hook::before_queued(input.channel, "name_paste").await;
            // The paste is named before it can start, so a later Unknown can still be proven.
            let bound = target.binding.as_ref().map(|binding| binding.4.as_deref());
            let session = bound.and_then(|bound| provider.session(&target.transcript, bound));
            let held = ticket.as_mut().unwrap();
            if let Err(error) = held.name_paste(&input.nonce, &target.transcript, session) {
                let channel_id = input.channel;
                tracing::warn!(channel_id, %error, "queued paste not named; not typed");
                return InjectAttempt::NotSent(ATTEMPT_UNNAMED);
            }
            let held = ticket.take().unwrap();
            let marked = mailbox
                .settle_queued_head(held, HeadOutcome::MayHaveSent, persistence.clone())
                .await;
            match marked {
                HeadSettle::Committed(Some(kept)) => ticket = Some(kept),
                HeadSettle::NotCommitted(kept, error) => {
                    persistence_failed(error);
                    ticket = Some(kept);
                    return InjectAttempt::Unconfirmed {
                        turn_id: turn_id.clone(),
                        detail: "queued_phase_failed",
                    };
                }
                _ => {
                    return InjectAttempt::Unconfirmed {
                        turn_id: turn_id.clone(),
                        detail: "queued_phase_failed",
                    };
                }
            }
            #[cfg(test)]
            test_hook::before_queued(input.channel, "may_have_sent").await;
            if !queued_target_matches(&shared, &request, &target, true).await {
                return InjectAttempt::NotSent(HOLDER_CHANGED);
            }
            let effect = tokio::task::spawn_blocking({
                let (input, session, transcript, turn_id) = (
                    input.clone(),
                    target.session.clone(),
                    target.transcript.clone(),
                    turn_id.clone(),
                );
                let herdr = target.herdr.clone();
                let native_turn = target.native_turn.clone();
                move || {
                    provider.run(
                        (&pane(input.channel, &session), herdr.as_ref()),
                        (&session, &transcript, native_turn.as_deref()),
                        turn_id,
                        MessageId::new(primary),
                        &input,
                    )
                }
            });
            let attempt = effect.await.unwrap_or(InjectAttempt::OwnerFailed {
                turn_id: turn_id.clone(),
            });
            #[cfg(test)]
            test_hook::before_queued(input.channel, "after_effect").await;
            if !matches!(attempt, InjectAttempt::NotSent(_))
                && !queued_target_matches(&shared, &request, &target, false).await
            {
                return InjectAttempt::Unconfirmed {
                    turn_id: turn_id.clone(),
                    detail: HOLDER_CHANGED,
                };
            }
            attempt
        };
        let attempt = std::panic::AssertUnwindSafe(work)
            .catch_unwind()
            .await
            .unwrap_or(InjectAttempt::OwnerFailed {
                turn_id: turn_id.clone(),
            });
        if let Some(ticket) = ticket {
            // Claude tmux is terminal once Enter is sent; a pre-paste menu returns NotSent.
            let outcome = match attempt {
                InjectAttempt::Injected { .. } => HeadOutcome::Injected,
                InjectAttempt::NotSent(_) => HeadOutcome::NotSent,
                _ => HeadOutcome::Unconfirmed,
            };
            if !matches!(
                mailbox
                    .settle_queued_head(ticket, outcome, persistence)
                    .await,
                HeadSettle::Committed(None)
            ) {
                return InjectAttempt::Unconfirmed {
                    turn_id,
                    detail: "queued_settle_unknown",
                };
            }
            // Pane and actor settlement are complete; reaction I/O must not hold the next input.
            drop(target);
            if matches!(attempt, InjectAttempt::Injected { .. }) {
                super::catch_up_race::confirmed(
                    &shared,
                    &request.provider,
                    request.channel_id,
                    &source_generations,
                )
                .await;
                // A live source is marked by its intake caller after this owner returns.
                if _source.is_none() {
                    super::catch_up_race::mark_confirmed(
                        &shared,
                        &request.provider,
                        request.channel_id,
                        &source_generations,
                    )
                    .await;
                }
            }
            // A committed NotSent is the actor's proof that the head is back at the queue front.
            if let (InjectAttempt::NotSent(veto), Some(turn_id)) = (&attempt, turn_id) {
                let veto = *veto;
                return InjectAttempt::HandedBack { turn_id, veto };
            }
        }
        attempt
    };
    match tokio::spawn(owner).await {
        Ok(attempt) => attempt,
        Err(_) => InjectAttempt::OwnerFailed { turn_id: None },
    }
}

/// How long the author lookup may take; an input it cannot name in time is not typed.
const NAME_LOOKUP: std::time::Duration = std::time::Duration::from_secs(3);

#[cfg(test)]
async fn v2_queued_attempt(
    shared: &Arc<SharedData>,
    request: &HumanInputRequest,
    expected: QueueIdentity,
    source: Option<(String, SourceGuard)>,
) -> InjectAttempt {
    let mailbox = shared.mailbox(request.channel_id);
    let (key, original_text, author) = match mailbox
        .v2_request(|slot| {
            let owner = slot.as_ref().ok_or("V2 not adopted")?;
            let head = owner.snapshot.head_injection.as_ref().ok_or("no head")?;
            Ok((
                owner.active_attempt()?,
                head.original.text.clone(),
                head.original.author_id,
            ))
        })
        .await
    {
        Ok((key, text, author)) if key.identity == expected => (key, text, author),
        _ => return InjectAttempt::NotSent("v2_not_prepared"),
    };
    if author != request.author_id {
        return InjectAttempt::NotSent(AUTHOR_MISMATCH);
    }
    let Some(provider) = QueuedProvider::select(&request.provider, request.channel_id.get()) else {
        return InjectAttempt::NotSent("provider_unsupported");
    };
    let name = match source.as_ref() {
        Some((name, _)) => Ok(name.clone()),
        None => {
            author_name(
                shared,
                request.channel_id,
                expected.primary,
                request.author_id,
            )
            .await
        }
    };
    let name = match name {
        Ok(name) => name,
        Err(veto) => return InjectAttempt::NotSent(veto),
    };
    let target = match resolve(shared, request, true).await {
        Ok(target) => target,
        Err(veto) => return InjectAttempt::NotSent(veto),
    };
    let recorded = serde_json::to_value(&key.attempt).unwrap_or_default();
    if !queued_target_matches(shared, request, &target, true).await
        || !super::queued_reconcile::v2_record_matches(
            &recorded,
            &target,
            provider,
            &request.provider,
        )
    {
        return InjectAttempt::NotSent(HOLDER_CHANGED);
    }
    let turn_id = Some(format!(
        "discord:{}:{}",
        request.channel_id, expected.primary
    ));
    let input = Input {
        channel: request.channel_id.get(),
        provider: request.provider.as_str().into(),
        source: turn_id.clone().unwrap_or_default(),
        author: request.author_id.to_string(),
        name,
        text: original_text,
        nonce: key.attempt.nonce,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64;
    let effect_turn = turn_id.clone();
    let kind = request.provider.clone();
    match mailbox
        .v2_effect(now, move |_frame| {
            let _source = source;
            let target = target;
            let delivered = matches!(
                provider.run(
                    (&pane(input.channel, &target.session), target.herdr.as_ref()),
                    (
                        &target.session,
                        &target.transcript,
                        target.native_turn.as_deref()
                    ),
                    effect_turn,
                    MessageId::new(expected.primary),
                    &input
                ),
                InjectAttempt::Injected { .. }
            );
            Ok(delivered
                && super::queued_reconcile::v2_record_matches(&recorded, &target, provider, &kind))
        })
        .await
    {
        Ok(true) => InjectAttempt::Injected { turn_id },
        _ => InjectAttempt::Unconfirmed {
            turn_id,
            detail: "v2_unconfirmed",
        },
    }
}

#[cfg(test)]
struct EffectCompletion<K> {
    attempt: K,
    outcome: Result<bool, String>,
}

#[cfg(test)]
impl crate::services::turn_orchestrator::ChannelMailboxHandle {
    /// Uses the real mailbox and durable permit; production admission is intentionally absent.
    pub(crate) async fn v2_effect(
        &self,
        now: u64,
        effect: impl FnOnce(String) -> Result<bool, String> + Send + 'static,
    ) -> Result<bool, String> {
        self.v2_effect_clock(
            now,
            std::sync::Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(u64::MAX as u128) as u64
            }),
            effect,
        )
        .await
    }
    pub(crate) async fn v2_effect_clock(
        &self,
        now: u64,
        clock: std::sync::Arc<dyn Fn() -> u64 + Send + Sync>,
        effect: impl FnOnce(String) -> Result<bool, String> + Send + 'static,
    ) -> Result<bool, String> {
        let mailbox = self.clone();
        tokio::spawn(async move {
            let (key, permit, frame) = mailbox
                .v2_request(move |slot| {
                    let owner = slot.as_mut().ok_or("V2 not adopted")?;
                    let key = owner.active_attempt()?;
                    if key.attempt.target.host != "tmux"
                        || !matches!(key.attempt.provider.as_str(), "claude" | "codex")
                    {
                        return Err("provider_unsupported".into());
                    }
                    let frame = owner
                        .snapshot
                        .head_injection
                        .as_ref()
                        .and_then(|h| h.payload.as_ref())
                        .ok_or("unprepared payload")?
                        .frame
                        .clone();
                    let permit = owner.arm(&key, now)?;
                    Ok((key, permit, frame))
                })
                .await?;
            let fallback = key.clone();
            let completion = tokio::task::spawn_blocking(move || {
                let outcome = permit.consume(&key).and_then(|_writer| effect(frame));
                EffectCompletion {
                    attempt: key,
                    outcome,
                }
            })
            .await
            .unwrap_or_else(|e| EffectCompletion {
                attempt: fallback,
                outcome: Err(e.to_string()),
            });
            let identity = serde_json::to_value(&completion.attempt).map_err(|e| e.to_string())?;
            mailbox
                .v2_settle_exact(
                    identity,
                    completion.outcome.as_ref().copied().unwrap_or(false),
                    clock(),
                )
                .await?;
            completion.outcome
        })
        .await
        .map_err(|e| e.to_string())?
    }
}
/// No name was found for the head's author: the input stays queued.
pub(super) const NAME_UNRESOLVED: &str = "queued_name_unresolved";
/// The head's message, or the head claimed, is not the author's: the input stays queued.
pub(super) const AUTHOR_MISMATCH: &str = "queued_author_mismatch";

/// The name Discord shows for the head's author, checked against the head's own message when it
/// is a Discord message.
async fn author_name(
    shared: &SharedData,
    channel: ChannelId,
    primary: u64,
    author: u64,
) -> Result<String, &'static str> {
    use poise::serenity_prelude::UserId;
    if author <= 1 {
        return Ok("system".to_string());
    }
    let http = shared
        .serenity_http_or_token_fallback()
        .ok_or(NAME_UNRESOLVED)?;
    let user = UserId::new(author);
    let lookup = async {
        if !crate::services::discord::is_synthetic_headless_message_id_raw(primary) {
            let message = channel.message(&http, MessageId::new(primary)).await;
            let message = message.map_err(|_| NAME_UNRESOLVED)?;
            if message.author.id != user {
                return Err(AUTHOR_MISMATCH);
            }
        }
        let place = channel
            .to_channel(&http)
            .await
            .map_err(|_| NAME_UNRESOLVED)?;
        let (nick, user) = match place.guild() {
            Some(place) => {
                let member = place.guild_id.member(&http, user).await;
                let member = member.map_err(|_| NAME_UNRESOLVED)?;
                (member.nick, member.user)
            }
            None => (
                None,
                user.to_user(&http).await.map_err(|_| NAME_UNRESOLVED)?,
            ),
        };
        let display = crate::services::tui_input::human_frame::display_name;
        Ok(display(nick.as_deref(), user.global_name.as_deref(), &user.name).to_string())
    };
    tokio::time::timeout(NAME_LOOKUP, lookup)
        .await
        .unwrap_or(Err(NAME_UNRESOLVED))
}
