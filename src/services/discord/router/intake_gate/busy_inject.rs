//! Admitted Discord text, from a person or a bot, offered to a busy Claude TUI pane before intake
//! queues or starts it, behind its own channel gate, the message claimed until the injection ends.

use std::collections::HashSet;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use super::super::intake_queue_transaction::SoftInterventionSpec;
use crate::services::discord::health::{
    HumanInputRequest, InjectAttempt, InjectOrigin, SOURCE_OWNED, inject_human_input,
};
use crate::services::discord::inject_disposition;
use crate::services::discord::turn_view_reconciler::{TurnViewIdentity, TurnViewTarget};
use crate::services::discord::{Data, SharedData};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::HANDBACK_NOT_WRITTEN;

/// `ADK_BUSY_INJECT_DISCORD_CHANNELS` opens busy-turn injection of admitted Discord text, read
/// once per process: `*` or channel ids split by commas or spaces, threads of a listed channel too.
#[cfg(not(test))]
const DISCORD_INJECT_ENV: &str = "ADK_BUSY_INJECT_DISCORD_CHANNELS";

const INJECTED_REACTION: char = '📥';
const UNCONFIRMED_REACTION: char = '❓';
const UNCONFIRMED_NOTICE: &str = "❓ 터미널 입력을 확인하지 못했습니다. 중복이면 무시하세요.";
const QUEUE_UNKNOWN_NOTICE: &str = "⚠️ 메시지 큐 저장 중 오류가 감지되어 접수 표시를 생략했어.";
/// Prefixes intake handles as dispatches or meetings, never as typed input.
const NOT_TYPED: [&str; 2] = ["DISPATCH:", "/meeting "];

#[derive(Debug, PartialEq, Eq)]
enum Gate {
    Off,
    All,
    Channels(HashSet<u64>),
}

/// An unset, empty or unparsable value keeps the gate off.
fn parse(value: Option<&str>) -> Gate {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Gate::Off;
    };
    if value == "*" {
        return Gate::All;
    }
    let ids = value.split([',', ' ']).filter(|id| !id.is_empty());
    match ids
        .map(str::parse::<u64>)
        .collect::<Result<HashSet<_>, _>>()
    {
        Ok(ids) => Gate::Channels(ids),
        Err(_) => Gate::Off,
    }
}

/// Tests never read the process env; a channel is open only while a test opens it.
fn listed(channel: u64) -> bool {
    #[cfg(test)]
    return test_support::gate_open(channel);
    #[cfg(not(test))]
    {
        static PROCESS: std::sync::OnceLock<Gate> = std::sync::OnceLock::new();
        let gate = PROCESS.get_or_init(|| parse(std::env::var(DISCORD_INJECT_ENV).ok().as_deref()));
        match gate {
            Gate::Off => false,
            Gate::All => true,
            Gate::Channels(ids) => ids.contains(&channel),
        }
    }
}

/// Whether the gate opens `channel`, or the parent of thread `channel`. Never on Windows, whose
/// provider file has no flock, nor on a Herdr-hosted channel.
fn covers(channel: ChannelId, parent: Option<ChannelId>) -> bool {
    if cfg!(windows) || !(listed(channel.get()) || parent.is_some_and(|id| listed(id.get()))) {
        return false;
    }
    let herdr = crate::services::herdr_launch::herdr_configured_for_tui_launch;
    !herdr(Some(channel.get())) && !parent.is_some_and(|id| herdr(Some(id.get())))
}

/// Whether a thread may take a message its gated parent passed on; a parent injection that owns
/// or ended it blocks the promotion, and an allowed one records the thread's intake.
pub(super) fn thread_may_take(
    provider: &ProviderKind,
    parent: ChannelId,
    message: MessageId,
) -> bool {
    if !covers(parent, None) {
        return true;
    }
    let allowed = inject_disposition::promote_thread(provider, message, Instant::now());
    #[cfg(test)]
    test_support::note_promotion(message.get(), allowed);
    allowed
}

/// Whether a live arrival on a gated channel was already taken by an injection, so intake ends
/// it without a reaction, checkpoint or notice.
pub(super) fn already_taken(
    provider: &ProviderKind,
    channel: ChannelId,
    parent: Option<ChannelId>,
    message: MessageId,
) -> bool {
    if !covers(channel, parent) {
        return false;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    inject_disposition::taken(provider, message, now_ms, Instant::now())
}

/// One live message as intake read it.
pub(super) struct LiveText<'a> {
    pub(super) channel_id: ChannelId,
    pub(super) parent: Option<ChannelId>,
    pub(super) message_id: MessageId,
    pub(super) author_id: UserId,
    /// The author marks intake's own queue entry would carry, which a handback carries too.
    pub(super) author_is_bot: bool,
    pub(super) author_is_allowed_automation: bool,
    pub(super) text: &'a str,
    pub(super) reply_context: Option<&'a str>,
    pub(super) has_reply_boundary: bool,
    /// Attachments, uploads or a voice transcript ride with the text.
    pub(super) carries_more_than_text: bool,
}

/// Only plain text, whoever sent it, outside startup recovery and restart drain, is offered.
fn offerable(live: &LiveText<'_>, shared: &SharedData) -> bool {
    let restart = &shared.restart;
    !live.carries_more_than_text
        && !live.text.trim().is_empty()
        && !crate::services::discord::catch_up::handled_command::is_text_command(live.text)
        && !NOT_TYPED.iter().any(|prefix| live.text.starts_with(prefix))
        && restart.reconcile_done.load(Relaxed)
        && !restart.restart_pending.load(Relaxed)
        && !restart.shutting_down.load(Relaxed)
}

/// A turn prompt joins the rendered reply context to the text with a blank line; so does the paste.
fn body(live: &LiveText<'_>) -> String {
    match live.reply_context {
        Some(reply) => format!("{reply}\n\n{}", live.text),
        None => live.text.to_string(),
    }
}

/// The queue entry intake would have made, which a vetoed paste becomes at the queue front.
fn handback(live: &LiveText<'_>) -> (crate::services::turn_orchestrator::Intervention, String) {
    let spec = SoftInterventionSpec {
        channel_id: live.channel_id,
        author_id: live.author_id,
        author_is_bot: live.author_is_bot,
        author_is_allowed_automation: live.author_is_allowed_automation,
        message_id: live.message_id,
        text: live.text.to_string(),
        reply_context: live.reply_context.map(str::to_string),
        has_reply_boundary: live.has_reply_boundary,
        merge_consecutive: false,
        pending_uploads: Default::default(),
        voice_announcement: None,
    };
    let turn = format!("discord:{}:{}", live.channel_id, live.message_id);
    (spec.into_intervention(), turn)
}

/// True when intake must stop: the pane took the text or may have, the message was handed back
/// or is owned elsewhere, or a handback failed. False keeps the intake path.
pub(super) async fn offered(ctx: &serenity::Context, data: &Data, live: &LiveText<'_>) -> bool {
    if !covers(live.channel_id, live.parent) {
        return false;
    }
    #[cfg(test)]
    test_support::note_offer(live.message_id.get()).await;
    if !offerable(live, &data.shared) {
        return false;
    }
    let message = live.message_id;
    let Some(guard) = inject_disposition::claim_source(&data.provider, message, Instant::now())
    else {
        return false;
    };
    let request = HumanInputRequest {
        channel_id: live.channel_id,
        provider: data.provider.clone(),
        text: body(live),
        author_id: live.author_id.get(),
        source: "discord".to_string(),
        metadata: None,
        channel_name_hint: None,
    };
    let handback = Box::new(handback(live));
    let origin = InjectOrigin::Discord {
        message,
        handback,
        guard,
    };
    let attempt = inject_human_input(&data.shared, &request, origin).await;
    #[cfg(test)]
    test_support::note_outcome(message.get(), &attempt);
    settle(ctx, data, live, attempt).await
}

async fn settle(
    ctx: &serenity::Context,
    data: &Data,
    live: &LiveText<'_>,
    attempt: InjectAttempt,
) -> bool {
    let shared = &data.shared;
    let (channel, message) = (live.channel_id, live.message_id);
    let reaction = match attempt {
        InjectAttempt::NotSent(veto) if veto == SOURCE_OWNED => return true,
        InjectAttempt::NotSent(veto) => {
            tracing::debug!(
                channel_id = channel.get(),
                veto,
                "discord input kept for intake"
            );
            return false;
        }
        InjectAttempt::Injected { .. } => INJECTED_REACTION,
        InjectAttempt::Unconfirmed { .. } => UNCONFIRMED_REACTION,
        // The message waits at the queue front, marked as queued input is; its catch-up is the queue's.
        InjectAttempt::HandedBack { .. } => {
            let queued =
                crate::services::discord::queue_reactions::QUEUE_STANDALONE_PENDING_REACTION;
            let mark = super::queue_effects::add_queue_pending_reaction_self_healing;
            mark(ctx, data, channel, message, queued).await;
            if super::super::queue_status_presentation::queue_status_card_enabled() {
                let ack = super::queue_effects::render_visible_queued_ack;
                ack(ctx, data, channel, message, live.text, false).await;
            }
            return true;
        }
        // The owner already answered a failed write; an unknown one may have landed.
        InjectAttempt::HandbackFailed(reason) => {
            if reason != HANDBACK_NOT_WRITTEN {
                say(ctx, shared, channel, QUEUE_UNKNOWN_NOTICE).await;
            }
            return true;
        }
        // Nothing was recorded; the checkpoint stays so catch-up can judge the message.
        InjectAttempt::OwnerFailed { .. } => {
            say(ctx, shared, channel, UNCONFIRMED_NOTICE).await;
            return true;
        }
    };
    // The live checkpoint moves past the message as for any handled one.
    let advance = crate::services::discord::advance_last_message_checkpoint;
    advance(shared, &data.provider, channel, message);
    let target = TurnViewTarget::intake_user_message(channel, message);
    let identity = TurnViewIdentity::IntakeHttp(ctx.http.clone());
    let reconciler = &shared.turn_view_reconciler;
    let source = "busy_inject_reaction";
    reconciler
        .note_untracked_reaction_added(shared, target, identity, reaction, source)
        .await;
    if reaction == UNCONFIRMED_REACTION {
        say(ctx, shared, channel, UNCONFIRMED_NOTICE).await;
    }
    true
}

async fn say(
    ctx: &serenity::Context,
    shared: &std::sync::Arc<SharedData>,
    channel: ChannelId,
    text: &str,
) {
    crate::services::discord::discord_io::rate_limit_wait(shared, channel).await;
    let _ = channel.say(&ctx.http, text).await;
}

#[cfg(test)]
#[path = "busy_inject_test_support.rs"]
pub(crate) mod test_support;

#[cfg(test)]
mod tests {
    use super::*;

    /// Only `*` or a list of channel ids opens the gate; anything unreadable keeps it shut.
    #[test]
    fn the_gate_opens_only_on_all_or_a_readable_channel_list() {
        let parsed = [
            None,
            Some(""),
            Some(" * "),
            Some("7, 8 9"),
            Some("7,x"),
            Some("all"),
        ]
        .map(parse);
        let listed = Gate::Channels([7, 8, 9].into_iter().collect());
        let expected = [
            Gate::Off,
            Gate::Off,
            Gate::All,
            listed,
            Gate::Off,
            Gate::Off,
        ];
        assert_eq!(parsed, expected);
    }
}
