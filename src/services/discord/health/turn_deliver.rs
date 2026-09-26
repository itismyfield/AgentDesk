//! Human input entry: start a turn when the mailbox is idle, steer the running
//! hosted-TUI turn when busy, otherwise queue the input on the channel mailbox.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, UserId};

use super::HealthRegistry;
use super::runtime_resolve::resolve_direct_meeting_shared;
use crate::services::discord::{SharedData, router};
use crate::services::provider::ProviderKind;
use crate::services::provider_hosting::ProviderSessionSelection;
use crate::services::tui_steering::{self, SteeringOutcome, SteeringSnapshot};
use crate::services::turn_orchestrator::{
    Intervention, InterventionMode, SourceMessageQueuedGeneration,
};

pub struct HumanInputRequest {
    pub channel_id: ChannelId,
    pub provider: ProviderKind,
    pub text: String,
    pub author_id: u64,
    pub source: String,
    pub metadata: Option<serde_json::Value>,
    pub channel_name_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanInputDelivery {
    Started {
        turn_id: String,
    },
    Injected {
        turn_id: String,
    },
    Queued {
        turn_id: String,
        reason: String,
    },
    /// Steering failed after the pane may already hold the input; never requeued.
    Unconfirmed {
        turn_id: String,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanInputError {
    AuthorNotAllowed,
    RuntimeUnavailable(String),
    QueueRefused(String),
}

/// Stricter than Discord intake auth: an explicit owner is required and
/// `allow_all_users` is never honored for remote input.
pub(crate) fn author_allowed_for_human_input(
    owner_user_id: Option<u64>,
    allowed_user_ids: &[u64],
    author_id: u64,
) -> bool {
    author_id != 0
        && owner_user_id.is_some()
        && (owner_user_id == Some(author_id) || allowed_user_ids.contains(&author_id))
}

enum StartAttempt {
    Started(String),
    Busy,
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveTurn {
    turn_id: String,
    nonce: Option<String>,
    background: bool,
}

struct BusyTarget {
    selection: ProviderSessionSelection,
    tmux_session_name: String,
}

#[async_trait]
trait DeliveryPorts: Send + Sync {
    async fn try_start(&self) -> StartAttempt;
    async fn active_turn(&self) -> Option<ActiveTurn>;
    async fn busy_tui_target(&self) -> Result<BusyTarget, &'static str>;
    async fn inject(&self, target: BusyTarget) -> SteeringOutcome;
    async fn enqueue(&self) -> Result<String, String>;
}

async fn deliver_with_ports<P: DeliveryPorts>(
    ports: &P,
) -> Result<HumanInputDelivery, HumanInputError> {
    let observed = match ports.try_start().await {
        StartAttempt::Started(turn_id) => return Ok(HumanInputDelivery::Started { turn_id }),
        StartAttempt::Unavailable(error) => return Err(HumanInputError::RuntimeUnavailable(error)),
        StartAttempt::Busy => ports.active_turn().await,
    };
    let Some(active) = observed else {
        return retry_start_or_queue(ports, "session_transition").await;
    };
    if active.background {
        return queue(ports, "background_turn").await;
    }
    let target = match ports.busy_tui_target().await {
        Ok(target) => target,
        Err(reason) => return queue(ports, reason).await,
    };
    // The turn may have ended while the pane was probed; steering an idle pane
    // would run the input outside mailbox custody.
    if ports.active_turn().await.as_ref() != Some(&active) {
        return retry_start_or_queue(ports, "turn_changed").await;
    }
    match ports.inject(target).await {
        SteeringOutcome::Injected => Ok(HumanInputDelivery::Injected {
            turn_id: active.turn_id,
        }),
        SteeringOutcome::Unsafe(reason) => queue(ports, &format!("unsafe:{reason}")).await,
        SteeringOutcome::ExistingMailbox => queue(ports, "non_tui_driver").await,
        SteeringOutcome::Failed(_) => Ok(HumanInputDelivery::Unconfirmed {
            turn_id: active.turn_id,
            reason: "injection_unconfirmed".to_string(),
        }),
    }
}

async fn retry_start_or_queue<P: DeliveryPorts>(
    ports: &P,
    reason: &str,
) -> Result<HumanInputDelivery, HumanInputError> {
    match ports.try_start().await {
        StartAttempt::Started(turn_id) => Ok(HumanInputDelivery::Started { turn_id }),
        StartAttempt::Unavailable(error) => Err(HumanInputError::RuntimeUnavailable(error)),
        StartAttempt::Busy => queue(ports, reason).await,
    }
}

async fn queue<P: DeliveryPorts>(
    ports: &P,
    reason: &str,
) -> Result<HumanInputDelivery, HumanInputError> {
    match ports.enqueue().await {
        Ok(turn_id) => Ok(HumanInputDelivery::Queued {
            turn_id,
            reason: reason.to_string(),
        }),
        Err(refusal) => Err(HumanInputError::QueueRefused(refusal)),
    }
}

/// Steers only a pane that still shows a running turn; a Claude pane back at
/// its ready prompt means the turn ended and the input must not bypass the mailbox.
fn steer_running_turn<C, I>(
    selection: &ProviderSessionSelection,
    mut capture: C,
    inject: I,
) -> SteeringOutcome
where
    C: FnMut() -> Option<SteeringSnapshot>,
    I: FnMut() -> Result<(), String>,
{
    if let Some(SteeringSnapshot::Claude(snapshot)) = capture()
        && snapshot.prompt_marker_detected
    {
        return SteeringOutcome::Unsafe("turn not running");
    }
    tui_steering::inject_with_bounded_retry_using(selection, capture, inject)
}

struct LivePorts {
    shared: Arc<SharedData>,
    ctx: serenity::Context,
    token: String,
    channel_id: ChannelId,
    provider: ProviderKind,
    text: String,
    author: UserId,
    author_name: String,
    source: String,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
}

#[async_trait]
impl DeliveryPorts for LivePorts {
    async fn try_start(&self) -> StartAttempt {
        let result = router::start_reserved_headless_turn_for_author(
            &self.ctx,
            self.channel_id,
            &self.text,
            &self.author_name,
            self.author,
            &self.shared,
            &self.token,
            Some(self.source.as_str()),
            self.metadata.clone(),
            self.channel_name_hint.clone(),
            router::reserve_headless_turn(),
        )
        .await;
        match result {
            Ok(outcome) => StartAttempt::Started(outcome.turn_id),
            Err(router::HeadlessTurnStartError::Conflict(_)) => StartAttempt::Busy,
            Err(router::HeadlessTurnStartError::Internal(error)) => {
                StartAttempt::Unavailable(error)
            }
        }
    }

    async fn active_turn(&self) -> Option<ActiveTurn> {
        let snapshot = super::super::mailbox_snapshot(&self.shared, self.channel_id).await;
        snapshot.cancel_token.as_ref()?;
        let message_id = snapshot.active_user_message_id?;
        Some(ActiveTurn {
            turn_id: format!("discord:{}:{}", self.channel_id.get(), message_id.get()),
            nonce: snapshot.active_turn_nonce,
            background: snapshot.active_turn_kind.is_background(),
        })
    }

    #[cfg(unix)]
    async fn busy_tui_target(&self) -> Result<BusyTarget, &'static str> {
        if !matches!(self.provider, ProviderKind::Claude | ProviderKind::Codex) {
            return Err("provider_unsupported");
        }
        let selection =
            crate::services::provider_hosting::resolve_provider_session_selection_with_channel(
                &self.provider,
                crate::services::claude::is_tmux_available(),
                Some(self.channel_id.get()),
            );
        if tui_steering::route_input_by_session_driver(&selection)
            != tui_steering::SteeringRoute::NativeTui
        {
            return Err("non_tui_driver");
        }
        let (channel_name, remote, current_path, session_id) = {
            let data = self.shared.core.lock().await;
            let session = data
                .sessions
                .get(&self.channel_id)
                .ok_or("tui_session_absent")?;
            (
                session.channel_name.clone(),
                session.remote_profile_name.is_some(),
                session.current_path.clone(),
                session.session_id.clone(),
            )
        };
        if remote {
            return Err("remote_session");
        }
        let tmux_session_name = channel_name
            .map(|name| self.provider.build_tmux_session_name(&name))
            .ok_or("tui_session_absent")?;
        if !router::hosted_tui_transcript_busy(
            &self.shared,
            &self.provider,
            self.channel_id,
            &tmux_session_name,
            current_path.as_deref(),
            session_id.as_deref(),
        ) {
            return Err("tui_not_busy");
        }
        Ok(BusyTarget {
            selection,
            tmux_session_name,
        })
    }

    #[cfg(not(unix))]
    async fn busy_tui_target(&self) -> Result<BusyTarget, &'static str> {
        Err("non_tui_driver")
    }

    async fn inject(&self, target: BusyTarget) -> SteeringOutcome {
        let provider = self.provider.clone();
        let prompt = self.text.clone();
        tokio::task::spawn_blocking(move || {
            let session = target.tmux_session_name.as_str();
            steer_running_turn(
                &target.selection,
                || tui_steering::capture_snapshot(&provider, session),
                || tui_steering::inject_once(&provider, session, &prompt),
            )
        })
        .await
        .unwrap_or_else(|error| SteeringOutcome::Failed(error.to_string()))
    }

    async fn enqueue(&self) -> Result<String, String> {
        let reservation = router::reserve_headless_turn();
        let message_id = reservation.user_msg_id();
        let generation = crate::services::discord::runtime_store::process_generation();
        let intervention = Intervention {
            author_id: self.author,
            author_is_bot: false,
            message_id,
            queued_generation: generation,
            source_message_ids: vec![message_id],
            source_message_queued_generations: vec![
                SourceMessageQueuedGeneration::user_instruction(message_id, generation),
            ],
            source_text_segments: Vec::new(),
            text: self.text.clone(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        };
        let outcome = super::super::mailbox_enqueue_intervention(
            &self.shared,
            &self.provider,
            self.channel_id,
            intervention,
        )
        .await;
        if !outcome.enqueued {
            return Err(outcome
                .refusal_reason
                .map(|reason| format!("{reason:?}"))
                .unwrap_or_else(|| "not_enqueued".to_string()));
        }
        Ok(reservation.turn_id(self.channel_id))
    }
}

pub async fn deliver_human_input(
    registry: &HealthRegistry,
    request: HumanInputRequest,
) -> Result<HumanInputDelivery, HumanInputError> {
    let shared = resolve_direct_meeting_shared(registry, request.channel_id, &request.provider)
        .await
        .map_err(HumanInputError::RuntimeUnavailable)?;
    let allowed = {
        let settings = shared.settings.read().await;
        author_allowed_for_human_input(
            settings.owner_user_id,
            &settings.allowed_user_ids,
            request.author_id,
        )
    };
    if !allowed {
        return Err(HumanInputError::AuthorNotAllowed);
    }
    let ctx = shared
        .http
        .cached_serenity_ctx
        .get()
        .cloned()
        .ok_or_else(|| {
            HumanInputError::RuntimeUnavailable("provider runtime is not ready".to_string())
        })?;
    let token = shared
        .http
        .cached_bot_token
        .get()
        .cloned()
        .or_else(|| crate::services::discord::resolve_discord_token_by_hash(&shared.token_hash))
        .ok_or_else(|| {
            HumanInputError::RuntimeUnavailable("provider token unavailable".to_string())
        })?;
    let ports = LivePorts {
        author_name: format!("{}:{}", request.source, request.author_id),
        shared,
        ctx,
        token,
        channel_id: request.channel_id,
        provider: request.provider,
        text: request.text,
        author: UserId::new(request.author_id),
        source: request.source,
        metadata: request.metadata,
        channel_name_hint: request.channel_name_hint,
    };
    deliver_with_ports(&ports).await
}

/// Registers a bot runtime bound to `channel_id` with the given auth settings.
#[cfg(test)]
pub(crate) async fn register_bot_auth_for_tests(
    registry: &HealthRegistry,
    provider: &str,
    channel_id: u64,
    owner_user_id: Option<u64>,
    allowed_user_ids: Vec<u64>,
    allow_all_users: bool,
) {
    let shared = crate::services::discord::make_shared_data_for_tests();
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = owner_user_id;
        settings.allowed_user_ids = allowed_user_ids;
        settings.allow_all_users = allow_all_users;
        settings.allowed_channel_ids = vec![channel_id];
    }
    registry.register(provider.to_string(), shared).await;
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::services::provider_hosting::ProviderSessionDriver;
    use SteeringOutcome::{ExistingMailbox, Failed, Injected, Unsafe};

    fn active(message: u64, background: bool) -> Option<ActiveTurn> {
        let turn_id = format!("discord:7:{message}");
        Some(ActiveTurn {
            turn_id,
            nonce: None,
            background,
        })
    }

    fn tui_selection() -> ProviderSessionSelection {
        ProviderSessionSelection {
            provider_id: "codex".to_string(),
            requested_tui_hosting: true,
            driver: ProviderSessionDriver::TuiHosting,
            fallback_reason: None,
        }
    }

    struct FakePorts {
        starts: Mutex<VecDeque<StartAttempt>>,
        actives: Mutex<VecDeque<Option<ActiveTurn>>>,
        target: Result<(), &'static str>,
        inject: Box<dyn Fn() -> SteeringOutcome + Send + Sync>,
        enqueue: Result<String, String>,
        calls: [AtomicUsize; 2],
    }

    fn ports(starts: Vec<StartAttempt>, actives: Vec<Option<ActiveTurn>>) -> FakePorts {
        FakePorts {
            starts: Mutex::new(starts.into()),
            actives: Mutex::new(actives.into()),
            target: Ok(()),
            inject: Box::new(|| Injected),
            enqueue: Ok("discord:7:900".to_string()),
            calls: Default::default(),
        }
    }

    /// Busy mailbox whose active turn stays the same across both observations.
    fn busy(outcome: SteeringOutcome) -> FakePorts {
        let mut fake = ports(vec![StartAttempt::Busy], vec![active(11, false); 2]);
        fake.inject = Box::new(move || outcome.clone());
        fake
    }

    #[async_trait]
    impl DeliveryPorts for FakePorts {
        async fn try_start(&self) -> StartAttempt {
            let next = self.starts.lock().unwrap().pop_front();
            next.unwrap_or(StartAttempt::Busy)
        }
        async fn active_turn(&self) -> Option<ActiveTurn> {
            self.actives.lock().unwrap().pop_front().flatten()
        }
        async fn busy_tui_target(&self) -> Result<BusyTarget, &'static str> {
            let tmux_session_name = "pane".to_string();
            self.target.map(|()| BusyTarget {
                selection: tui_selection(),
                tmux_session_name,
            })
        }
        async fn inject(&self, _target: BusyTarget) -> SteeringOutcome {
            self.calls[0].fetch_add(1, Ordering::SeqCst);
            (self.inject)()
        }
        async fn enqueue(&self) -> Result<String, String> {
            self.calls[1].fetch_add(1, Ordering::SeqCst);
            self.enqueue.clone()
        }
    }

    /// `<delivery> <turn_id> [reason] inject=N enqueue=N` for compact expectations.
    async fn run(fake: &FakePorts) -> String {
        let outcome = match deliver_with_ports(fake).await {
            Ok(HumanInputDelivery::Started { turn_id }) => format!("started {turn_id}"),
            Ok(HumanInputDelivery::Injected { turn_id }) => format!("injected {turn_id}"),
            Ok(HumanInputDelivery::Queued { turn_id, reason }) => {
                format!("queued {turn_id} {reason}")
            }
            Ok(HumanInputDelivery::Unconfirmed { turn_id, reason }) => {
                format!("unconfirmed {turn_id} {reason}")
            }
            Err(error) => format!("{error:?}"),
        };
        let [injects, enqueues] = &fake.calls;
        let (injects, enqueues) = (
            injects.load(Ordering::SeqCst),
            enqueues.load(Ordering::SeqCst),
        );
        format!("{outcome} inject={injects} enqueue={enqueues}")
    }

    #[tokio::test]
    async fn delivery_table_routes_each_state_to_exactly_one_commit_point() {
        let mut not_busy = busy(Injected);
        not_busy.target = Err("tui_not_busy");
        let mut refused = busy(Unsafe("composer draft"));
        refused.enqueue = Err("LastItemDedup".to_string());
        #[rustfmt::skip]
        let cases = [
            (ports(vec![StartAttempt::Started("discord:7:1".into())], vec![]), "started discord:7:1 inject=0 enqueue=0"),
            (busy(Injected), "injected discord:7:11 inject=1 enqueue=0"),
            (busy(Unsafe("interactive modal")), "queued discord:7:900 unsafe:interactive modal inject=1 enqueue=1"),
            (busy(ExistingMailbox), "queued discord:7:900 non_tui_driver inject=1 enqueue=1"),
            (not_busy, "queued discord:7:900 tui_not_busy inject=0 enqueue=1"),
            (ports(vec![StartAttempt::Busy], vec![active(11, true)]), "queued discord:7:900 background_turn inject=0 enqueue=1"),
            // Failure after typing may have reached the pane, so it is reported, never requeued.
            (busy(Failed("draft stuck".into())), "unconfirmed discord:7:11 injection_unconfirmed inject=1 enqueue=0"),
            (ports(vec![StartAttempt::Unavailable("no ctx".into())], vec![]), "RuntimeUnavailable(\"no ctx\") inject=0 enqueue=0"),
            (refused, "QueueRefused(\"LastItemDedup\") inject=1 enqueue=1"),
        ];
        for (fake, expected) in cases {
            assert_eq!(run(&fake).await, expected);
        }
    }

    #[tokio::test]
    async fn a_turn_that_ends_or_changes_before_steering_is_never_injected() {
        let restarted = [
            StartAttempt::Busy,
            StartAttempt::Started("discord:7:2".into()),
        ];
        #[rustfmt::skip]
        let cases = [
            (ports(restarted.into(), vec![active(11, false), None]), "started discord:7:2 inject=0 enqueue=0"),
            (ports(vec![StartAttempt::Busy], vec![active(11, false), active(12, false)]), "queued discord:7:900 turn_changed inject=0 enqueue=1"),
            (ports(vec![StartAttempt::Busy], vec![None]), "queued discord:7:900 session_transition inject=0 enqueue=1"),
        ];
        for (fake, expected) in cases {
            assert_eq!(run(&fake).await, expected);
        }
    }

    #[tokio::test]
    async fn codex_pane_without_composer_is_queued_with_the_strict_snapshot_reason() {
        use crate::services::codex_tui::input::PromptReadinessSnapshot;
        let mut fake = busy(Injected);
        fake.inject = Box::new(|| {
            let working = SteeringSnapshot::Codex(PromptReadinessSnapshot {
                composer_marker_detected: false,
                prompt_draft_detected: false,
                tmux_pane_alive: true,
                capture_available: true,
                pane_tail: "• Working (12s • esc to interrupt)".to_string(),
            });
            steer_running_turn(
                &tui_selection(),
                || Some(working.clone()),
                || panic!("typed"),
            )
        });
        let expected = "queued discord:7:900 unsafe:composer not present inject=1 enqueue=1";
        assert_eq!(run(&fake).await, expected);
    }

    #[test]
    fn claude_pane_back_at_ready_prompt_is_not_steered() {
        use crate::services::claude_tui::input::PromptReadinessSnapshot;
        let ready = SteeringSnapshot::Claude(PromptReadinessSnapshot {
            prompt_marker_detected: true,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "❯ ".to_string(),
        });
        let outcome =
            steer_running_turn(&tui_selection(), || Some(ready.clone()), || panic!("typed"));
        assert_eq!(outcome, Unsafe("turn not running"));
    }

    #[tokio::test]
    async fn only_listed_authors_of_an_owned_bot_pass_even_when_allow_all_is_on() {
        let channel = 6_245_001_u64;
        let request = |author_id| HumanInputRequest {
            channel_id: ChannelId::new(channel),
            provider: ProviderKind::Claude,
            text: "status?".to_string(),
            author_id,
            source: "imessage".to_string(),
            metadata: None,
            channel_name_hint: None,
        };
        let (open_bot, ownerless) = (HealthRegistry::new(), HealthRegistry::new());
        register_bot_auth_for_tests(&open_bot, "claude", channel, Some(100), vec![200], true).await;
        register_bot_auth_for_tests(&ownerless, "claude", channel, None, vec![200], true).await;
        let not_allowed = Err(HumanInputError::AuthorNotAllowed);
        assert_eq!(
            deliver_human_input(&open_bot, request(300)).await,
            not_allowed
        );
        assert_eq!(
            deliver_human_input(&ownerless, request(200)).await,
            not_allowed
        );
        // Allowed authors get past auth and stop only at the missing gateway context.
        for author in [100, 200] {
            let outcome = deliver_human_input(&open_bot, request(author)).await;
            assert!(matches!(
                outcome,
                Err(HumanInputError::RuntimeUnavailable(_))
            ));
        }
    }
}
