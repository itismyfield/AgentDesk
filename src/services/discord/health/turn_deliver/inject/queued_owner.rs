//! The queued-head owner: a head already proven taken is consumed first, otherwise the first ready
//! input is reserved, its paste recorded before it can start, typed once and settled by the actor.
use super::queued_reconcile::transcript_session;
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
    // A head already proven taken is consumed before the busy check, so an idle pane is no veto.
    if let Some(mended) = super::queued_reconcile::mend(shared, request, parent, gate).await {
        return mended;
    }
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
            let session = bound.and_then(|bound| transcript_session(&target.transcript, bound));
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
                move || {
                    run(
                        (&pane(input.channel, &session), herdr.as_ref()),
                        &session,
                        &transcript,
                        turn_id,
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
            if matches!(attempt, InjectAttempt::Injected { .. }) {
                super::catch_up_race::confirmed(
                    &shared,
                    &request.provider,
                    request.channel_id,
                    &source_generations,
                )
                .await;
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
