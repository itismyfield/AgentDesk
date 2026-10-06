//! A person's Discord text offered to the turn holding a Claude TUI pane before intake queues or
//! starts it; text the pane does not take keeps the intake path unchanged.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use crate::services::discord::SharedData;
use crate::services::discord::health::{HumanInputDelivery, HumanInputRequest};
use crate::services::discord::turn_view_reconciler::{TurnViewIdentity, TurnViewTarget};
use crate::services::provider::ProviderKind;

const INJECTED_REACTION: char = '📥';
const UNCONFIRMED_REACTION: char = '❓';
const UNCONFIRMED_NOTICE: &str = "❓ 터미널 입력을 확인하지 못했습니다. 중복이면 무시하세요.";
/// Prefixes intake handles as dispatches or commands, never as typed input.
const NOT_TYPED: [&str; 3] = ["DISPATCH:", "!", "/meeting "];

/// One live message as intake read it.
pub(super) struct LiveText<'a> {
    pub(super) channel_id: ChannelId,
    pub(super) message_id: MessageId,
    pub(super) author_id: UserId,
    pub(super) author_is_bot: bool,
    pub(super) text: &'a str,
    pub(super) reply_context: Option<&'a str>,
    /// Attachments, uploads or a voice transcript ride with the text.
    pub(super) carries_more_than_text: bool,
}

impl<'a> LiveText<'a> {
    /// `more` marks uploads, an admitted attachment turn or a voice transcript riding with the text.
    pub(super) fn new(
        message: &serenity::Message,
        text: &'a str,
        reply_context: Option<&'a str>,
        more: bool,
    ) -> Self {
        Self {
            channel_id: message.channel_id,
            message_id: message.id,
            author_id: message.author.id,
            author_is_bot: message.author.bot,
            text,
            reply_context,
            carries_more_than_text: more || !message.attachments.is_empty(),
        }
    }
}

/// Only plain text a person typed, outside startup recovery and restart drain, is offered.
fn offerable(live: &LiveText<'_>, shared: &SharedData) -> bool {
    let restart = &shared.restart;
    !live.author_is_bot
        && !live.carries_more_than_text
        && !live.text.trim().is_empty()
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

/// True when the pane took the text or may have, so intake stops; false keeps the intake path.
pub(super) async fn offered(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    live: &LiveText<'_>,
) -> bool {
    if !offerable(live, shared) {
        return false;
    }
    let request = HumanInputRequest {
        channel_id: live.channel_id,
        provider: provider.clone(),
        text: body(live),
        author_id: live.author_id.get(),
        source: "discord".to_string(),
        metadata: None,
        channel_name_hint: None,
    };
    let reaction = match request.offer_to_busy_turn(shared).await {
        Ok(HumanInputDelivery::Injected { .. }) => INJECTED_REACTION,
        Ok(_) => UNCONFIRMED_REACTION,
        Err(veto) => {
            tracing::debug!(
                channel_id = live.channel_id.get(),
                veto,
                "discord input kept for intake"
            );
            return false;
        }
    };
    // A restart's catch-up must not replay a message the pane already holds.
    let advance = crate::services::discord::advance_last_message_checkpoint;
    advance(shared, provider, live.channel_id, live.message_id);
    let target = TurnViewTarget::intake_user_message(live.channel_id, live.message_id);
    let identity = TurnViewIdentity::IntakeHttp(http.clone());
    let reconciler = &shared.turn_view_reconciler;
    let source = "busy_inject_reaction";
    reconciler
        .note_untracked_reaction_added(shared, target, identity, reaction, source)
        .await;
    if reaction == UNCONFIRMED_REACTION {
        notice(http, shared, live.channel_id).await;
    }
    true
}

async fn notice(http: &Arc<serenity::Http>, shared: &Arc<SharedData>, channel_id: ChannelId) {
    #[cfg(test)]
    {
        let _ = (http, shared);
        NOTICES.with(|notices| notices.borrow_mut().push((channel_id, UNCONFIRMED_NOTICE)));
    }
    #[cfg(not(test))]
    {
        crate::services::discord::discord_io::rate_limit_wait(shared, channel_id).await;
        let _ = channel_id.say(http, UNCONFIRMED_NOTICE).await;
    }
}

#[cfg(test)]
thread_local! {
    static NOTICES: std::cell::RefCell<Vec<(ChannelId, &'static str)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::services::discord::health::{
        HealthRegistry, InjectPane, queue_texts, register_inject_runtime,
    };
    use crate::services::provider::CancelToken;
    use crate::services::turn_orchestrator::ActiveTurnKind;

    fn typed(channel: u64, text: &str) -> LiveText<'_> {
        LiveText {
            channel_id: ChannelId::new(channel),
            message_id: MessageId::new(channel + 50),
            author_id: UserId::new(200),
            author_is_bot: false,
            text,
            reply_context: None,
            carries_more_than_text: false,
        }
    }

    /// Typed text goes into a busy pane whoever holds it and stops intake, with the reply context
    /// in front; bot, upload and command text, the switch off and a vetoed pane keep intake.
    #[tokio::test(flavor = "current_thread")]
    async fn typed_text_goes_into_a_busy_pane_and_anything_else_keeps_intake_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let channels = [
            6_245_901, 6_245_902, 6_245_903, 6_245_904, 6_245_905, 6_245_906, 6_245_907,
        ];
        let registry = HealthRegistry::new();
        let shared = register_inject_runtime(&registry, &channels, Some(pool)).await;
        let (held, off) = (channels[0], channels[6]);
        let panes = channels.map(|ch| InjectPane::new(ch, if ch == off { "off" } else { "all" }));
        let start = crate::services::discord::mailbox_try_start_turn_kinded;
        let (token, owner) = (Arc::new(CancelToken::new()), UserId::new(7));
        let background = ActiveTurnKind::Background;
        let message = MessageId::new(held + 10);
        assert!(
            start(
                &shared,
                ChannelId::new(held),
                token,
                owner,
                message,
                background
            )
            .await
        );
        // The reply context adds two line breaks to the folded paste.
        panes[0].fold_paste(3);
        panes[4].set("attach", "1");
        panes[5].set("fail_paste", "");
        let mut inputs = channels.map(|ch| typed(ch, "status?"));
        inputs[0].reply_context = Some("> earlier answer");
        inputs[1].author_is_bot = true;
        inputs[2].carries_more_than_text = true;
        inputs[3].text = "!stop";
        let http = Arc::new(serenity::Http::new(""));
        let mut observed = Vec::new();
        for (pane, live) in panes.iter().zip(&inputs) {
            let ch = live.channel_id;
            let taken = offered(&http, &shared, &ProviderKind::Claude, live).await;
            let checkpoint = shared.last_message_ids.get(&ch).map(|id| *id);
            let advanced = checkpoint == Some(live.message_id.get());
            let queue = queue_texts(&shared, ch.get()).await.len();
            let noticed =
                NOTICES.with(|notices| notices.borrow().contains(&(ch, UNCONFIRMED_NOTICE)));
            let (calls, keys) = (pane.tmux_calls() > 0, pane.keys().join("+"));
            observed.push(format!(
                "{} taken={taken} advanced={advanced} queue={queue} notice={noticed} tmux={calls} keys={keys}",
                ch.get() - 6_245_900
            ));
        }
        assert_eq!(
            panes[0].pasted().lines().skip(1).collect::<Vec<_>>(),
            ["> earlier answer", "", "status?"]
        );
        assert_eq!(
            observed,
            [
                "1 taken=true advanced=true queue=0 notice=false tmux=true keys=paste-buffer+send-keys",
                "2 taken=false advanced=false queue=0 notice=false tmux=false keys=",
                "3 taken=false advanced=false queue=0 notice=false tmux=false keys=",
                "4 taken=false advanced=false queue=0 notice=false tmux=false keys=",
                "5 taken=false advanced=false queue=0 notice=false tmux=true keys=",
                "6 taken=true advanced=true queue=0 notice=true tmux=true keys=",
                "7 taken=false advanced=false queue=0 notice=false tmux=false keys=",
            ]
        );
    }

    /// Intake offers typed text before it queues behind a held channel; no runtime harness reaches
    /// the gateway handler, so the order is read from its source.
    #[test]
    fn intake_offers_typed_text_before_its_busy_queue() {
        let source = include_str!("../intake_gate.rs");
        let offer = source
            .find("busy_inject::offered(")
            .expect("intake offers typed text");
        let busy = source
            .find("IntakeQueueCommitSource::BusyActiveTurn")
            .expect("busy queue");
        assert!(offer < busy);
    }
}
