//! Interaction handlers for idle-recap card buttons.
//!
//! The button's `custom_id` looks like `idle_recap:clear:<message_id>`. We
//! resolve the message id back to a `session_key` via the
//! `sessions.idle_recap_message_id` index, route through the same clear path
//! as `/clear`, and delete the recap card.

use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use std::sync::Arc;

use super::{Data, Error, SharedData, check_auth};
use crate::services::discord::idle_recap::{
    IDLE_RECAP_CLEAR_BUTTON_PREFIX, IDLE_RECAP_COMPACT_BUTTON_PREFIX,
    IDLE_RECAP_RELAY_DIAG_BUTTON_PREFIX, IDLE_RECAP_SUGGEST_BUTTON_PREFIX, clear_recap_pointer,
    delete_previous_card,
};
use crate::services::discord::input_runtime::fence;
use crate::services::provider::ProviderKind;

#[derive(Clone, Debug, PartialEq, Eq)]
struct RecapClearTarget {
    session_key: String,
    provider: String,
    owner_instance_id: Option<String>,
    turn_generation: i64,
    channel_matches: bool,
    provider_matches: bool,
    recap_current: bool,
}

mod compact;
mod relay_diag;
mod suggest;

/// The input fence's verdict for a recap action on `channel`; a refusal is a health reason too.
fn admit_recap_effect(
    provider: &ProviderKind,
    channel: u64,
) -> Result<Option<fence::Permit>, String> {
    fence::effect::admit(provider, channel).map_err(|failure| {
        fence::record_failure(provider, channel, &[], failure);
        format!("{failure:?}")
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecapPromptRoute {
    NativeSlashCompact,
    InternalFollowup,
}

fn recap_prompt_route(custom_id: &str) -> Option<RecapPromptRoute> {
    if custom_id.starts_with(IDLE_RECAP_COMPACT_BUTTON_PREFIX) {
        Some(RecapPromptRoute::NativeSlashCompact)
    } else if custom_id.starts_with(IDLE_RECAP_SUGGEST_BUTTON_PREFIX) {
        Some(RecapPromptRoute::InternalFollowup)
    } else {
        None
    }
}

/// True if `custom_id` belongs to the idle-recap clear button.
pub(super) fn is_idle_recap_clear_custom_id(custom_id: &str) -> bool {
    custom_id.starts_with(IDLE_RECAP_CLEAR_BUTTON_PREFIX)
}

pub(super) fn is_idle_recap_custom_id(custom_id: &str) -> bool {
    is_idle_recap_clear_custom_id(custom_id)
        || custom_id.starts_with(IDLE_RECAP_COMPACT_BUTTON_PREFIX)
        || custom_id.starts_with(IDLE_RECAP_RELAY_DIAG_BUTTON_PREFIX)
        || custom_id.starts_with(IDLE_RECAP_SUGGEST_BUTTON_PREFIX)
}

pub(super) async fn handle_idle_recap_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let custom_id = &component.data.custom_id;
    if custom_id.starts_with(IDLE_RECAP_CLEAR_BUTTON_PREFIX) {
        return handle_idle_recap_clear_interaction(ctx, component, data).await;
    }
    if custom_id.starts_with(IDLE_RECAP_RELAY_DIAG_BUTTON_PREFIX) {
        return relay_diag::handle_idle_recap_relay_diag_interaction(ctx, component, data).await;
    }
    match recap_prompt_route(custom_id) {
        Some(RecapPromptRoute::NativeSlashCompact) => {
            return compact::handle_idle_recap_compact_interaction(ctx, component, data).await;
        }
        Some(RecapPromptRoute::InternalFollowup) => {
            return suggest::handle_idle_recap_suggest_interaction(ctx, component, data).await;
        }
        None => {}
    }
    let _ = component
        .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
        .await;
    Ok(())
}

pub(super) async fn handle_idle_recap_clear_interaction(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    // Authorise the click. Without this, anyone who can see the recap
    // card (= anyone with read access to the bound Discord channel) could
    // drop the provider session id. Reuses the same auth gate that the
    // `/clear` slash command goes through (see commands::control::clear).
    let user_id = component.user.id;
    let user_name = &component.user.name;
    if !check_auth(user_id, user_name, &data.shared, &data.token).await {
        let _ = component
            .create_response(
                ctx,
                serenity::CreateInteractionResponse::Message(
                    serenity::CreateInteractionResponseMessage::new()
                        .content("Not authorized for this bot.")
                        .ephemeral(true),
                ),
            )
            .await;
        return Ok(());
    }

    let custom_id = &component.data.custom_id;
    let Some(message_id) = parse_message_id(custom_id, IDLE_RECAP_CLEAR_BUTTON_PREFIX) else {
        // Unknown / sentinel id ("0") — happens during the brief window
        // before post_recap_card rewrites the placeholder button to the
        // real id. Acknowledge so the client doesn't time out.
        let _ = component
            .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
            .await;
        return Ok(());
    };

    let acknowledge = || async {
        let _ = component
            .create_response(ctx, serenity::CreateInteractionResponse::Acknowledge)
            .await;
    };
    let (shared, provider) = (&data.shared, &data.provider);
    let channel = component.channel_id;
    let clear = clear_recap_card(
        &ctx.http,
        shared,
        provider,
        channel,
        message_id,
        acknowledge,
    );
    if let Some(reply) = clear.await? {
        send_ephemeral(ctx, component, &reply).await;
    }
    Ok(())
}

/// The clear click after its authorisation, answered by `acknowledge` or by the reply it
/// returns. A session the host check refuses keeps its card and its pointer, as does a channel
/// the input fence refuses; an admitted clear consumes them only after the clear succeeded.
async fn clear_recap_card<Ack, AckFut>(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: serenity::ChannelId,
    message_id: u64,
    acknowledge: Ack,
) -> Result<Option<String>, Error>
where
    Ack: FnOnce() -> AckFut,
    AckFut: std::future::Future<Output = ()>,
{
    let permit = match admit_recap_effect(provider, channel.get()) {
        Ok(permit) => permit,
        Err(reason) => {
            tracing::warn!(message_id, %reason, "idle_recap clear: input fence refused");
            return Ok(Some(format!(
                "세션 정리 거절: 이 채널의 입력이 전환 중이라 지금은 정리할 수 없습니다. ({reason})"
            )));
        }
    };
    let Some(pool) = shared.pg_pool.as_ref().cloned() else {
        return Ok(Some("세션 정리 실패: DB 연결 없음.".to_string()));
    };

    let target = lookup_recap_clear_target(&pool, message_id, channel.get(), provider.as_str());
    let clear_target = match target.await {
        Ok(Some(target)) => target,
        Ok(None) => {
            // Card already cleared (compare-and-clear path won the race
            // with a fresh-cycle post) — silently acknowledge.
            acknowledge().await;
            return Ok(None);
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                message_id = message_id,
                "idle_recap clear: target lookup failed"
            );
            return Ok(Some("세션 정리 실패. 잠시 후 다시 시도하세요.".to_string()));
        }
    };

    let current =
        clear_target.channel_matches && clear_target.provider_matches && clear_target.recap_current;
    // Judged on the card's own session before its pointer is consumed or the card deleted.
    let key = Some(clear_target.session_key.as_str());
    let refusal = super::admin_host_guard::managed_reset_refusal;
    if current && let Some(reason) = refusal(shared, provider, channel, true, false, key).await {
        tracing::warn!(message_id, %reason, "idle_recap clear: host unsupported");
        return Ok(Some(format!(
            "세션 정리 거절: 이 세션의 호스트에서는 세션 정리를 지원하지 않습니다. ({reason})"
        )));
    }

    acknowledge().await;

    if !current {
        let _ = clear_recap_pointer(&pool, &clear_target.session_key, message_id).await;
        delete_previous_card(http, channel.get(), message_id).await;
        return Ok(None);
    }

    // Compare-and-clear the recap pointer for this session, then delete
    // the card. Order matters: clear the pointer first so the
    // user-message hook (intake_gate) doesn't try to delete the same
    // message at the same time.
    if permit.is_none() {
        let pointer_cleared = clear_recap_pointer(&pool, &clear_target.session_key, message_id)
            .await
            .unwrap_or(false);
        if !pointer_cleared {
            delete_previous_card(http, channel.get(), message_id).await;
            return Ok(None);
        }
    }

    // Reuse `/clear` semantics, not just the provider-session-id drop. TUI
    // providers keep live tmux/process state that must be reset too.
    let clear = crate::services::discord::commands::clear_channel_session_state_with_session_key(
        http,
        shared,
        provider,
        channel,
        "idle_recap_clear",
        crate::services::discord::commands::SoftClearNotifyMode::Enqueue,
        Some(&clear_target.session_key),
    );
    match permit {
        // An admitted clear keeps its card until the clear itself has succeeded.
        Some(permit) => {
            fence::effect::scope(Some(permit), clear).await?;
            let _ = clear_recap_pointer(&pool, &clear_target.session_key, message_id).await;
        }
        None => clear.await?,
    }
    delete_previous_card(http, channel.get(), message_id).await;

    Ok(None)
}

/// The native `/compact` send itself, kept here with the platform split.
fn send_native_compact(request: &compact::NativeCompactRequest) -> Result<(), String> {
    #[cfg(unix)]
    {
        crate::services::claude_tui::composer_lock::with_session_turn_lock(
            &request.tmux_session_name,
            || {
                crate::services::claude_tui::input::send_followup_prompt(
                    &request.tmux_session_name,
                    request.prompt,
                    None,
                )
            },
        )
    }
    #[cfg(not(unix))]
    {
        crate::services::claude_tui::input::send_followup_prompt(
            &request.tmux_session_name,
            request.prompt,
            None,
        )
    }
}

async fn authorize_component(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    data: &Data,
) -> bool {
    let user_id = component.user.id;
    let user_name = &component.user.name;
    if check_auth(user_id, user_name, &data.shared, &data.token).await {
        return true;
    }
    let _ = component
        .create_response(
            ctx,
            serenity::CreateInteractionResponse::Message(
                serenity::CreateInteractionResponseMessage::new()
                    .content("Not authorized for this bot.")
                    .ephemeral(true),
            ),
        )
        .await;
    false
}

async fn send_ephemeral(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    content: &str,
) {
    let _ = component
        .create_response(
            ctx,
            serenity::CreateInteractionResponse::Message(
                serenity::CreateInteractionResponseMessage::new()
                    .content(content)
                    .ephemeral(true),
            ),
        )
        .await;
}

async fn edit_deferred_ephemeral(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    message_id: u64,
    content: &str,
) {
    if let Err(error) = component
        .edit_response(
            ctx,
            serenity::EditInteractionResponse::new().content(content),
        )
        .await
    {
        tracing::warn!(
            error = %error,
            message_id,
            "idle_recap compact: failed to edit deferred interaction response"
        );
    }
}

fn truncate_interaction_body(body: &str) -> String {
    const LIMIT: usize = 1800;
    let mut out = String::new();
    let mut chars = body.chars();
    for ch in chars.by_ref().take(LIMIT) {
        out.push(ch);
    }
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

fn prompt_sent_ephemeral(prompt: &str) -> String {
    let prompt = truncate_interaction_body(prompt);
    format!("다음 프롬프트를 보냈습니다:\n> {prompt}")
}

fn parse_message_id(custom_id: &str, prefix: &str) -> Option<u64> {
    custom_id
        .strip_prefix(prefix)
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|id| *id != 0)
}

async fn lookup_recap_clear_target(
    pool: &PgPool,
    message_id: u64,
    channel_id: u64,
    provider: &str,
) -> Result<Option<RecapClearTarget>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, String, Option<String>, i64, bool, bool, bool)>(
        "SELECT session_key,
                provider,
                instance_id,
                idle_recap_turn_generation,
                idle_recap_channel_id = $2 AS channel_matches,
                provider = $3 AS provider_matches,
                COALESCE(idle_recap_posted_at >= COALESCE(last_heartbeat, created_at), false) AS recap_current
         FROM sessions
         WHERE idle_recap_message_id = $1
         LIMIT 1",
    )
    .bind(message_id as i64)
    .bind(channel_id as i64)
    .bind(provider)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(
            session_key,
            provider,
            owner_instance_id,
            turn_generation,
            channel_matches,
            provider_matches,
            recap_current,
        )| RecapClearTarget {
            session_key,
            provider,
            owner_instance_id,
            turn_generation,
            channel_matches,
            provider_matches,
            recap_current,
        },
    ))
}

async fn lookup_current_recap_target(
    pool: &PgPool,
    message_id: u64,
    channel_id: u64,
    provider: &str,
) -> Result<Option<RecapClearTarget>, sqlx::Error> {
    let Some(target) = lookup_recap_clear_target(pool, message_id, channel_id, provider).await?
    else {
        return Ok(None);
    };
    if target.channel_matches && target.provider_matches && target.recap_current {
        Ok(Some(target))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::compact::*;
    use super::*;

    #[test]
    fn idle_recap_component_router_accepts_all_recap_actions() {
        assert!(is_idle_recap_custom_id("idle_recap:clear:1"));
        assert!(is_idle_recap_custom_id("idle_recap:relay_diag:1"));
        assert!(is_idle_recap_custom_id("idle_recap:compact:1"));
        assert!(is_idle_recap_custom_id("idle_recap:suggest:1"));
        assert!(!is_idle_recap_custom_id("foreign:action:1"));
    }

    const LOCAL_NODE: (&str, &str) = ("mac-mini", "mac-mini-release");

    fn no_host_refusal(_: &str) -> std::future::Ready<Option<String>> {
        std::future::ready(None)
    }

    fn recap_target(session_key: &str) -> RecapClearTarget {
        RecapClearTarget {
            session_key: session_key.to_string(),
            provider: "claude".to_string(),
            owner_instance_id: Some("mac-mini-release".to_string()),
            turn_generation: 7,
            channel_matches: true,
            provider_matches: true,
            recap_current: true,
        }
    }

    #[tokio::test]
    async fn compact_uses_claimed_recap_target_and_native_prompt() {
        let injected = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = injected.clone();
        let outcome = run_native_compact_once(
            recap_target("claude/token/mac-mini:old-bound-session"),
            LOCAL_NODE,
            no_host_refusal,
            || async { Ok(true) },
            || async { Ok(true) },
            |target| std::future::ready(target == "old-bound-session"),
            move |request| {
                observed.lock().unwrap().push(request.clone());
                async { Ok(()) }
            },
        )
        .await;

        let expected = NativeCompactRequest {
            tmux_session_name: "old-bound-session".to_string(),
            prompt: "/compact",
        };
        assert_eq!(outcome, RecapCompactOutcome::Started(expected.clone()));
        assert_eq!(*injected.lock().unwrap(), vec![expected]);
    }

    #[tokio::test]
    async fn rebinding_cannot_redirect_compact_to_current_channel_session() {
        let injection_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = injection_calls.clone();
        let outcome = run_native_compact_once(
            recap_target("claude/token/mac-mini:old-bound-session"),
            LOCAL_NODE,
            no_host_refusal,
            || async { Ok(true) },
            || async { Ok(true) },
            |target| std::future::ready(target == "new-current-session"),
            move |_| {
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Ok(()) }
            },
        )
        .await;

        assert_eq!(
            outcome,
            RecapCompactOutcome::TargetNotLive(NativeCompactRequest {
                tmux_session_name: "old-bound-session".to_string(),
                prompt: "/compact",
            })
        );
        assert_eq!(injection_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn concurrent_compact_claims_allow_exactly_one_injection() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let claim_barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let claimed = std::sync::Arc::new(AtomicBool::new(false));
        let claims_in_flight = std::sync::Arc::new(AtomicUsize::new(0));
        let max_claims_in_flight = std::sync::Arc::new(AtomicUsize::new(0));
        let injections = std::sync::Arc::new(AtomicUsize::new(0));
        let run = || {
            let claim_barrier = claim_barrier.clone();
            let claimed = claimed.clone();
            let claims_in_flight = claims_in_flight.clone();
            let max_claims_in_flight = max_claims_in_flight.clone();
            let injections = injections.clone();
            run_native_compact_once(
                recap_target("mac-mini:claimed-session"),
                LOCAL_NODE,
                no_host_refusal,
                move || async move {
                    let in_flight = claims_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_claims_in_flight.fetch_max(in_flight, Ordering::SeqCst);
                    claim_barrier.wait().await;
                    let won = claimed
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok();
                    claims_in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok(won)
                },
                || async { Ok(true) },
                |_| async { true },
                move |_| async move {
                    injections.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
        };

        let (first, second) = tokio::join!(run(), run());
        let outcomes = [first, second];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, RecapCompactOutcome::Started(_)))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, RecapCompactOutcome::AlreadyClaimed))
                .count(),
            1
        );
        assert_eq!(max_claims_in_flight.load(Ordering::SeqCst), 2);
        assert_eq!(injections.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn foreign_session_identity_preserves_pointer_without_claim_or_injection() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let claim_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let pointer_consumed = std::sync::Arc::new(AtomicBool::new(false));
        let live_checks = std::sync::Arc::new(AtomicUsize::new(0));
        let injections = std::sync::Arc::new(AtomicUsize::new(0));
        let outcome = run_native_compact_once(
            recap_target("claude/token/mac-book:foo"),
            LOCAL_NODE,
            no_host_refusal,
            {
                let claim_calls = claim_calls.clone();
                let pointer_consumed = pointer_consumed.clone();
                move || async move {
                    claim_calls.fetch_add(1, Ordering::SeqCst);
                    pointer_consumed.store(true, Ordering::SeqCst);
                    Ok(true)
                }
            },
            || async { Ok(true) },
            {
                let live_checks = live_checks.clone();
                move |_| {
                    live_checks.fetch_add(1, Ordering::SeqCst);
                    async { true }
                }
            },
            {
                let injections = injections.clone();
                move |_| async move {
                    injections.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
        )
        .await;

        assert_eq!(
            outcome,
            RecapCompactOutcome::RoutingUnavailable {
                owner_instance_id: Some("mac-mini-release".to_string()),
                reason: "session_key_host_mismatch".to_string(),
            }
        );
        assert!(outcome.preserves_recap_card());
        assert_eq!(claim_calls.load(Ordering::SeqCst), 0);
        assert!(!pointer_consumed.load(Ordering::SeqCst));
        assert_eq!(live_checks.load(Ordering::SeqCst), 0);
        assert_eq!(injections.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn foreign_or_missing_owner_never_claims_local_host_target() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        for owner_instance_id in [Some("mac-book-release".to_string()), None] {
            let mut target = recap_target("claude/token/mac-mini:foo");
            target.owner_instance_id = owner_instance_id.clone();
            let claim_calls = std::sync::Arc::new(AtomicUsize::new(0));
            let observed_claims = claim_calls.clone();
            let outcome = run_native_compact_once(
                target,
                LOCAL_NODE,
                no_host_refusal,
                move || {
                    observed_claims.fetch_add(1, Ordering::SeqCst);
                    async { Ok(true) }
                },
                || async { Ok(true) },
                |_| async { true },
                |_| async { Ok(()) },
            )
            .await;

            assert!(matches!(
                outcome,
                RecapCompactOutcome::RoutingUnavailable {
                    owner_instance_id: ref observed_owner,
                    ..
                } if observed_owner == &owner_instance_id
            ));
            assert_eq!(claim_calls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn injection_failure_is_terminal_after_claim() {
        let claim_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = claim_calls.clone();
        let first = run_native_compact_once(
            recap_target("mac-mini:claimed-session"),
            LOCAL_NODE,
            no_host_refusal,
            move || {
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Ok(true) }
            },
            || async { Ok(true) },
            |_| async { true },
            |_| async { Err("submit failed".to_string()) },
        )
        .await;
        let retried_injections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_retries = retried_injections.clone();
        let retry = run_native_compact_once(
            recap_target("mac-mini:claimed-session"),
            LOCAL_NODE,
            no_host_refusal,
            || async { Ok(false) },
            || async { Ok(true) },
            |_| async { true },
            move |_| {
                observed_retries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Ok(()) }
            },
        )
        .await;

        assert!(matches!(
            first,
            RecapCompactOutcome::InjectionFailed { ref request, ref error }
                if request.prompt == "/compact" && error == "submit failed"
        ));
        assert_eq!(retry, RecapCompactOutcome::AlreadyClaimed);
        assert_eq!(claim_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            retried_injections.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[tokio::test]
    async fn claim_database_error_fails_closed_without_injection() {
        let injection_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = injection_calls.clone();
        let outcome = run_native_compact_once(
            recap_target("mac-mini:claimed-session"),
            LOCAL_NODE,
            no_host_refusal,
            || async { Err("database unavailable".to_string()) },
            || async { Ok(true) },
            |_| async { true },
            move |_| {
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Ok(()) }
            },
        )
        .await;

        assert_eq!(
            outcome,
            RecapCompactOutcome::ClaimFailed("database unavailable".to_string())
        );
        assert_eq!(injection_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn owner_handoff_after_claim_is_fenced_immediately_before_injection() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let live_checks = std::sync::Arc::new(AtomicUsize::new(0));
        let injections = std::sync::Arc::new(AtomicUsize::new(0));
        let outcome = run_native_compact_once(
            recap_target("mac-mini:claimed-session"),
            LOCAL_NODE,
            no_host_refusal,
            || async { Ok(true) },
            || async { Ok(false) },
            {
                let live_checks = live_checks.clone();
                move |_| async move {
                    live_checks.fetch_add(1, Ordering::SeqCst);
                    true
                }
            },
            {
                let injections = injections.clone();
                move |_| async move {
                    injections.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
        )
        .await;

        assert_eq!(outcome, RecapCompactOutcome::PostClaimOwnerChanged);
        assert!(!outcome.preserves_recap_card());
        assert_eq!(live_checks.load(Ordering::SeqCst), 1);
        assert_eq!(injections.load(Ordering::SeqCst), 0);
    }

    async fn seed_recap_compact_session_pg(
        pool: &PgPool,
        target: &RecapClearTarget,
        message_id: u64,
        channel_id: u64,
    ) {
        sqlx::query(
            "INSERT INTO sessions (
                session_key,
                provider,
                instance_id,
                idle_recap_turn_generation,
                idle_recap_message_id,
                idle_recap_channel_id,
                idle_recap_posted_at,
                last_heartbeat
             ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
        )
        .bind(&target.session_key)
        .bind(&target.provider)
        .bind(target.owner_instance_id.as_deref())
        .bind(target.turn_generation)
        .bind(message_id as i64)
        .bind(channel_id as i64)
        .execute(pool)
        .await
        .expect("seed recap compact session");
    }

    #[tokio::test]
    async fn concurrent_recap_compact_claim_pg_consumes_exactly_once() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "adk_recap_claim",
            "idle recap compact concurrent claim test",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate_with_max_connections(2).await;
        let target = recap_target("claude/token/mac-mini:pg-claimed-session");
        let message_id = 42;
        let channel_id = 84;
        seed_recap_compact_session_pg(&pool, &target, message_id, channel_id).await;

        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let claim = || {
            let pool = pool.clone();
            let target = target.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                claim_recap_compact_pointer(&pool, &target, message_id, channel_id)
                    .await
                    .expect("claim recap compact pointer")
            }
        };
        let (first, second) = tokio::join!(claim(), claim());

        assert_eq!(
            [first, second]
                .into_iter()
                .filter(|claimed| *claimed)
                .count(),
            1
        );
        let pointer = sqlx::query_as::<_, (Option<i64>, Option<i64>)>(
            "SELECT idle_recap_message_id, idle_recap_channel_id
             FROM sessions
             WHERE session_key = $1",
        )
        .bind(&target.session_key)
        .fetch_one(&pool)
        .await
        .expect("load claimed recap compact pointer");
        assert_eq!(pointer, (None, None));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn recap_compact_claim_pg_rejects_owner_handoff_without_consuming_pointer() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "adk_recap_owner",
            "idle recap compact owner handoff fence test",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let target = recap_target("claude/token/mac-mini:pg-owner-session");
        let message_id = 43;
        let channel_id = 85;
        seed_recap_compact_session_pg(&pool, &target, message_id, channel_id).await;
        sqlx::query("UPDATE sessions SET instance_id = 'mac-book-release' WHERE session_key = $1")
            .bind(&target.session_key)
            .execute(&pool)
            .await
            .expect("handoff recap compact owner");

        let injections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let outcome = run_native_compact_once(
            target.clone(),
            LOCAL_NODE,
            no_host_refusal,
            {
                let pool = pool.clone();
                let target = target.clone();
                move || async move {
                    claim_recap_compact_pointer(&pool, &target, message_id, channel_id)
                        .await
                        .map_err(|error| error.to_string())
                }
            },
            || async { Ok(true) },
            |_| async { true },
            {
                let injections = injections.clone();
                move |_| async move {
                    injections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                }
            },
        )
        .await;
        assert_eq!(outcome, RecapCompactOutcome::AlreadyClaimed);
        assert!(outcome.preserves_recap_card());
        assert_eq!(injections.load(std::sync::atomic::Ordering::SeqCst), 0);
        let pointer = sqlx::query_as::<_, (Option<i64>, Option<i64>, Option<String>)>(
            "SELECT idle_recap_message_id, idle_recap_channel_id, instance_id
             FROM sessions
             WHERE session_key = $1",
        )
        .bind(&target.session_key)
        .fetch_one(&pool)
        .await
        .expect("load owner-fenced recap compact pointer");
        assert_eq!(
            pointer,
            (
                Some(message_id as i64),
                Some(channel_id as i64),
                Some("mac-book-release".to_string())
            )
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn recap_prompt_route_sends_suggest_to_internal_followup_handler() {
        assert_eq!(
            recap_prompt_route("idle_recap:suggest:42"),
            Some(RecapPromptRoute::InternalFollowup)
        );
    }

    #[test]
    fn recap_prompt_route_rejects_unrelated_custom_ids() {
        assert_eq!(recap_prompt_route("idle_recap:clear:42"), None);
        assert_eq!(recap_prompt_route("foreign:action:42"), None);
    }

    #[test]
    fn recap_component_message_id_parser_rejects_zero_and_foreign_prefixes() {
        assert_eq!(
            parse_message_id(
                "idle_recap:relay_diag:42",
                IDLE_RECAP_RELAY_DIAG_BUTTON_PREFIX
            ),
            Some(42)
        );
        assert_eq!(
            parse_message_id("idle_recap:compact:42", IDLE_RECAP_COMPACT_BUTTON_PREFIX),
            Some(42)
        );
        assert_eq!(
            parse_message_id(
                "idle_recap:relay_diag:0",
                IDLE_RECAP_RELAY_DIAG_BUTTON_PREFIX
            ),
            None
        );
        assert_eq!(
            parse_message_id("idle_recap:suggest:42", IDLE_RECAP_RELAY_DIAG_BUTTON_PREFIX),
            None
        );
    }

    #[test]
    fn recap_prompt_sent_ephemeral_includes_actual_prompt_text() {
        assert_eq!(
            prompt_sent_ephemeral("테스트 계속 진행해줘"),
            "다음 프롬프트를 보냈습니다:\n> 테스트 계속 진행해줘"
        );
        assert_eq!(
            prompt_sent_ephemeral("/compact"),
            "다음 프롬프트를 보냈습니다:\n> /compact"
        );
    }
}

#[cfg(test)]
#[cfg(unix)]
mod host_guard_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::compact::*;
    use super::*;
    use crate::services::discord::admin_host_guard::tests::{Recorder, api_child, process};
    use crate::services::discord::host_defer_gate::tests::{
        Case, ScriptedTmux, map_channel, postgres,
    };
    use crate::services::discord::host_teardown_gate::test_support::{
        Stored, channel_key, shared_on,
    };

    // A clear click on a session the host guard keeps answers with the refusal and keeps the
    // card, its pointer, the session and its process; a legacy row clears as in main.
    #[tokio::test]
    async fn recap_clear_keeps_the_card_and_its_pointer_on_a_refused_host_pg() {
        let test = "services::discord::idle_recap_interaction::host_guard_tests::recap_clear_keeps_the_card_and_its_pointer_on_a_refused_host_pg";
        if !api_child(test) {
            return;
        }
        let _root = crate::config::TestRuntimeRootGuard::new();
        let tmux = ScriptedTmux::install();
        let (db, pool) = postgres().await;
        let shared = shared_on(&pool).await;
        let api = Recorder::start().await;
        crate::services::discord::internal_api::init(api.port, None);
        let provider = ProviderKind::Claude;
        for (n, case) in Case::ALL.into_iter().enumerate() {
            if !case.has_row() {
                continue;
            }
            let channel = serenity::ChannelId::new(1_479_671_302_387_068_000 + n as u64);
            let channel_name = format!("p4c2-recap-{n}");
            let name = provider.build_tmux_session_name(&channel_name);
            map_channel(&shared, channel, &channel_name).await;
            let mut core = shared.core.lock().await;
            core.sessions.get_mut(&channel).unwrap().session_id = Some("sid".into());
            drop(core);
            let key = channel_key(&shared, &name);
            case.seed(&pool, &key, &name, channel.get()).await;
            let message_id = 1_479_671_302_387_069_000 + n as u64;
            sqlx::query(
                "UPDATE sessions SET idle_recap_message_id = $2, idle_recap_channel_id = $3,
                        idle_recap_posted_at = NOW() + INTERVAL '1 minute'
                  WHERE session_key = $1",
            )
            .bind(&key)
            .bind(message_id as i64)
            .bind(channel.get() as i64)
            .execute(&pool)
            .await
            .expect("recap card pointer");
            let alive = process(&name, n as u32 + 66_000);
            api.take();
            tmux.take_calls();

            let acks = AtomicUsize::new(0);
            let acknowledge = || {
                acks.fetch_add(1, Ordering::SeqCst);
                async {}
            };
            let click = clear_recap_card(
                &api.http,
                &shared,
                &provider,
                channel,
                message_id,
                acknowledge,
            );
            let reply = click.await.expect("clear click");
            let pointer: Option<i64> = sqlx::query_scalar(
                "SELECT idle_recap_message_id FROM sessions WHERE session_key = $1",
            )
            .bind(&key)
            .fetch_one(&pool)
            .await
            .expect("pointer");
            let calls = api.take();
            let acks = acks.load(Ordering::SeqCst);
            if case == Case::Stored(Stored::Legacy) {
                assert_eq!((&reply, acks, pointer), (&None, 1, None), "{calls:?}");
                let card = message_id.to_string();
                // The card delete starts with main's probe of the recorded message.
                let probed = calls
                    .iter()
                    .any(|c| c.starts_with("GET") && c.contains(&card));
                assert!(probed, "main deletes the card: {calls:?}");
                let session_cleared = calls.iter().any(|c| c.contains("clear-session-id"));
                assert!(session_cleared, "main clears the session: {calls:?}");
                assert!(!alive.load(Ordering::SeqCst), "main kills");
                continue;
            }
            let refused = reply
                .as_deref()
                .is_some_and(|r| r.contains("세션 정리 거절"));
            assert!(refused, "{case:?}: {reply:?}");
            let kept = (acks, pointer);
            assert_eq!(kept, (0, Some(message_id as i64)), "{case:?}: pointer kept");
            assert_eq!(
                calls,
                Vec::<String>::new(),
                "{case:?}: no Discord or API call"
            );
            assert!(alive.load(Ordering::SeqCst), "{case:?}: process kept");
            let core = shared.core.lock().await;
            assert!(core.sessions[&channel].session_id.is_some(), "{case:?}");
            drop(core);
            assert_eq!(tmux.take_calls(), Vec::<String>::new(), "{case:?}: no tmux");
            crate::services::session_backend::remove_process_session(&name);
        }

        // A card whose own session is hosted is refused even when the channel's current
        // session, under another name with no row, would clear as in main.
        let channel = serenity::ChannelId::new(1_479_671_302_387_068_100);
        let (card_name, current) = ("AgentDesk-claude-p4c2-recap-card", "p4c2-recap-current");
        let key = channel_key(&shared, card_name);
        let stored = Case::Stored(Stored::Hosted);
        stored.seed(&pool, &key, card_name, channel.get() + 1).await;
        map_channel(&shared, channel, current).await;
        let message_id = 1_479_671_302_387_069_100_u64;
        let card = "UPDATE sessions SET idle_recap_message_id = $2, idle_recap_channel_id = $3,
                    idle_recap_posted_at = NOW() + INTERVAL '1 minute' WHERE session_key = $1";
        let card = sqlx::query(card).bind(&key).bind(message_id as i64);
        card.bind(channel.get() as i64)
            .execute(&pool)
            .await
            .expect("card");
        let current = process(&provider.build_tmux_session_name(current), 66_100);
        let click = clear_recap_card(
            &api.http,
            &shared,
            &provider,
            channel,
            message_id,
            || async {},
        );
        let reply = click.await.expect("clear click");
        assert!(
            reply.is_some_and(|r| r.contains("세션 정리 거절")),
            "the card's own key decides"
        );
        assert!(
            current.load(Ordering::SeqCst),
            "the channel's process is kept"
        );
        assert_eq!(api.take(), Vec::<String>::new(), "no Discord or API call");
        db.drop().await;
    }

    // Compact on a session whose host is not a found legacy row is refused before the claim
    // consumes the recap card; a legacy row claims and injects as in main.
    #[tokio::test]
    async fn compact_is_refused_before_the_claim_on_a_non_legacy_session_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (db, pool) = postgres().await;
        let provider = ProviderKind::Claude;
        for (n, case) in Case::ALL.into_iter().enumerate() {
            let channel = 1_479_671_302_387_064_000 + n as u64;
            let name = provider.build_tmux_session_name(&format!("p4c2-compact-{n}"));
            let key = format!("claude/p4c2/mac-mini:{name}");
            case.seed(&pool, &key, &name, channel).await;
            let (claims, injects) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let (claimed, injected) = (claims.clone(), injects.clone());
            let target = RecapClearTarget {
                session_key: key.clone(),
                provider: "claude".to_string(),
                owner_instance_id: Some("mac-mini-release".to_string()),
                turn_generation: 7,
                channel_matches: true,
                provider_matches: true,
                recap_current: true,
            };
            let refusal = crate::services::discord::admin_host_guard::session_key_refusal;
            let outcome = run_native_compact_once(
                target,
                ("mac-mini", "mac-mini-release"),
                |tmux_name: &str| {
                    let (pool, provider) = (&pool, &provider);
                    let (key, name) = (key.clone(), tmux_name.to_string());
                    async move { refusal(pool, Some(provider), Some(channel), &key, &name).await }
                },
                move || {
                    claimed.fetch_add(1, Ordering::SeqCst);
                    async { Ok(true) }
                },
                || async { Ok(true) },
                |_| async { true },
                move |_| {
                    injected.fetch_add(1, Ordering::SeqCst);
                    async { Ok(()) }
                },
            )
            .await;
            let counts = (
                claims.load(Ordering::SeqCst),
                injects.load(Ordering::SeqCst),
            );
            if case == Case::Stored(Stored::Legacy) {
                assert!(
                    matches!(outcome, RecapCompactOutcome::Started(_)),
                    "{outcome:?}"
                );
                assert_eq!(counts, (1, 1), "{case:?}");
            } else {
                assert!(
                    matches!(outcome, RecapCompactOutcome::HostUnsupported(_)),
                    "{case:?}: {outcome:?}"
                );
                assert!(outcome.preserves_recap_card(), "{case:?}: the card stays");
                assert_eq!(counts, (0, 0), "{case:?}: no claim or injection");
            }
        }
        db.drop().await;
    }
}

#[cfg(test)]
mod g2e_tests;

#[cfg(test)]
#[cfg(unix)]
mod g2e_pg_tests {
    //! Admitted recap actions against a real sessions row: the card's exact pointer survives every
    //! action that did not happen, serializes double clicks and is consumed only after success.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use poise::serenity_prelude as serenity;
    use sqlx::PgPool;

    use super::compact::*;
    use super::suggest::*;
    use super::{RecapClearTarget, clear_recap_card};
    use crate::services::discord::admin_host_guard::tests::{Recorder, api_child, process};
    use crate::services::discord::host_defer_gate::tests::{
        Case, ScriptedTmux, map_channel, postgres,
    };
    use crate::services::discord::host_teardown_gate::test_support::{
        Stored, channel_key, shared_on,
    };
    use crate::services::discord::input_runtime::fence::{self, Gate};
    use crate::services::provider::ProviderKind;

    const LOCAL_NODE: (&str, &str) = ("mac-mini", "mac-mini-release");

    fn card(channel: u64) -> (RecapClearTarget, u64, u64) {
        let target = RecapClearTarget {
            session_key: format!("claude/token/mac-mini:g2e-pg-{channel}"),
            provider: "claude".to_string(),
            owner_instance_id: Some("mac-mini-release".to_string()),
            turn_generation: 7,
            channel_matches: true,
            provider_matches: true,
            recap_current: true,
        };
        (target, channel + 1, channel)
    }

    /// Posts the card as the session's current recap.
    async fn post(pool: &PgPool, (target, message_id, channel): &(RecapClearTarget, u64, u64)) {
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, instance_id, idle_recap_turn_generation,
                                   idle_recap_message_id, idle_recap_channel_id,
                                   idle_recap_posted_at, last_heartbeat)
             VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())
             ON CONFLICT (session_key) DO UPDATE
                SET idle_recap_turn_generation = EXCLUDED.idle_recap_turn_generation,
                    idle_recap_message_id = EXCLUDED.idle_recap_message_id,
                    idle_recap_channel_id = EXCLUDED.idle_recap_channel_id,
                    idle_recap_posted_at = NOW(), last_heartbeat = NOW()",
        )
        .bind(&target.session_key)
        .bind(&target.provider)
        .bind(target.owner_instance_id.as_deref())
        .bind(target.turn_generation)
        .bind(*message_id as i64)
        .bind(*channel as i64)
        .execute(pool)
        .await
        .expect("post recap card");
    }

    async fn pointer(pool: &PgPool, session_key: &str) -> Option<i64> {
        let query = "SELECT idle_recap_message_id FROM sessions WHERE session_key = $1";
        let row = sqlx::query_scalar(query).bind(session_key);
        row.fetch_one(pool).await.expect("recap pointer")
    }

    fn admitting_gate(channel: u64) -> (Arc<Gate>, fence::test_health::Clear) {
        let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
        let health = fence::test_health::Clear::new(&gate);
        (gate, health)
    }

    type Host = Box<dyn FnOnce(&str) -> std::future::Ready<Option<String>> + Send>;
    type Check = Box<dyn FnOnce() -> std::future::Ready<Result<bool, String>> + Send>;
    type Live = Box<dyn FnOnce(&str) -> std::future::Ready<bool> + Send>;
    type SendFn = Box<dyn FnOnce(&NativeCompactRequest) -> Result<(), String> + Send>;
    type Seams = CompactSeams<Host, Check, Check, Live, SendFn>;

    /// The pane checks this run passes and its send; main's destructive claim must never run.
    fn seams(live: bool, owner: Result<bool, String>, send: SendFn) -> Seams {
        CompactSeams {
            host_refusal: Box::new(|_| std::future::ready(None)),
            legacy_claim: Box::new(|| {
                panic!("an admitted compact never runs main's destructive claim")
            }),
            pre_inject_fence: Box::new(move || std::future::ready(owner)),
            is_live: Box::new(move |_| std::future::ready(live)),
            send,
        }
    }

    fn counted(sends: &Arc<AtomicUsize>, result: Result<(), String>) -> SendFn {
        let sends = sends.clone();
        Box::new(move |_| {
            sends.fetch_add(1, Ordering::SeqCst);
            result
        })
    }

    /// Runs an admitted compact whose send blocks until `during` has looked at the row.
    async fn compact_holding_send<F: std::future::Future<Output = ()>>(
        pool: &PgPool,
        card: (RecapClearTarget, u64, u64),
        during: F,
    ) -> RecapCompactRun {
        let (entered, sending) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let send: SendFn = Box::new(move |_| {
            entered.send(()).unwrap();
            released.recv().unwrap();
            Ok(())
        });
        let pool = pool.clone();
        let run = tokio::spawn(async move {
            let seams = seams(true, Ok(true), send);
            run_recap_compact(&ProviderKind::Claude, &pool, card, LOCAL_NODE, seams).await
        });
        sending.await.unwrap();
        during.await;
        release.send(()).unwrap();
        run.await.unwrap()
    }

    // An admitted compact keeps the card's exact pointer while it sends and through every failure,
    // including an uncertain send it never repeats; success consumes that pointer and no successor.
    #[tokio::test]
    async fn g2e_admitted_compact_consumes_the_card_only_after_its_send_pg() {
        let (db, pool) = postgres().await;
        let provider = ProviderKind::Claude;
        let channel = 6_325_961_000_000_000;
        let _gate = admitting_gate(channel);
        let card = card(channel);
        let (key, message_id) = (card.0.session_key.clone(), card.1 as i64);
        post(&pool, &card).await;

        let sends = Arc::new(AtomicUsize::new(0));
        let lost: SendFn = Box::new(|_| panic!("the send's reply was lost"));
        let failures: [(bool, Result<bool, String>, SendFn); 5] = [
            (false, Ok(true), counted(&sends, Ok(()))),
            (true, Ok(false), counted(&sends, Ok(()))),
            (
                true,
                Err("owner unreadable".into()),
                counted(&sends, Ok(())),
            ),
            (
                true,
                Ok(true),
                counted(&sends, Err("paste sent, Enter failed".into())),
            ),
            (true, Ok(true), lost),
        ];
        for (n, (live, owner, send)) in failures.into_iter().enumerate() {
            let before = sends.load(Ordering::SeqCst);
            let seams = seams(live, owner, send);
            let run = run_recap_compact(&provider, &pool, card.clone(), LOCAL_NODE, seams).await;
            assert!(run.protected && run.keeps_card(), "{n}: {run:?}");
            let sent = sends.load(Ordering::SeqCst) - before;
            assert_eq!(sent, usize::from(n == 3), "{n}: sent at most once");
            assert_eq!(
                pointer(&pool, &key).await,
                Some(message_id),
                "{n}: card kept"
            );
            let failed = matches!(run.outcome, RecapCompactOutcome::InjectionFailed { .. });
            assert_eq!(
                failed,
                n >= 3,
                "{n}: an uncertain send is not success: {run:?}"
            );
        }

        let observed = Arc::new(std::sync::Mutex::new(None));
        let seen = observed.clone();
        let during = async { *seen.lock().unwrap() = pointer(&pool, &key).await };
        let run = compact_holding_send(&pool, card.clone(), during).await;
        assert!(
            matches!(run.outcome, RecapCompactOutcome::Started(_)),
            "{run:?}"
        );
        assert!(!run.keeps_card(), "the sent card is dropped");
        assert_eq!(
            *observed.lock().unwrap(),
            Some(message_id),
            "kept while sending"
        );
        assert_eq!(pointer(&pool, &key).await, None, "consumed after the send");

        // A successor card posted while the first one sends keeps its own pointer.
        post(&pool, &card).await;
        let successor = (card.0.clone(), card.1 + 100, card.2);
        let run = compact_holding_send(&pool, card.clone(), post(&pool, &successor)).await;
        assert!(
            matches!(run.outcome, RecapCompactOutcome::Started(_)),
            "{run:?}"
        );
        assert_eq!(pointer(&pool, &key).await, Some(successor.1 as i64));

        pool.close().await;
        db.drop().await;
    }

    // Two admitted clicks on one card: the second finds the card reserved while the first sends,
    // so the native /compact goes out exactly once.
    #[tokio::test]
    async fn g2e_concurrent_admitted_compacts_send_once_pg() {
        let (db, pool) = postgres().await;
        let channel = 6_325_961_000_000_100;
        let _gate = admitting_gate(channel);
        let card = card(channel);
        let key = card.0.session_key.clone();
        post(&pool, &card).await;

        let sends = Arc::new(AtomicUsize::new(0));
        let second = {
            let (pool, card, sends) = (pool.clone(), card.clone(), sends.clone());
            async move {
                let seams = seams(true, Ok(true), counted(&sends, Ok(())));
                let provider = ProviderKind::Claude;
                run_recap_compact(&provider, &pool, card, LOCAL_NODE, seams).await
            }
        };
        let second_run = Arc::new(std::sync::Mutex::new(None));
        let held = second_run.clone();
        let during = async move { *held.lock().unwrap() = Some(second.await) };
        let first = compact_holding_send(&pool, card, during).await;
        let second = second_run.lock().unwrap().take().unwrap();
        assert!(
            matches!(first.outcome, RecapCompactOutcome::Started(_)),
            "{first:?}"
        );
        assert_eq!(second.outcome, RecapCompactOutcome::AlreadyClaimed);
        assert!(second.keeps_card());
        assert_eq!(
            sends.load(Ordering::SeqCst),
            0,
            "the second click never sent"
        );
        assert_eq!(pointer(&pool, &key).await, None);

        pool.close().await;
        db.drop().await;
    }

    // An admitted suggest keeps the card when the legacy queue does not take the reply, consumes it
    // once the queue persisted the reply, and a later click on the consumed card enqueues nothing.
    #[tokio::test]
    async fn g2e_admitted_suggest_consumes_only_an_enqueued_card_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (db, pool) = postgres().await;
        let provider = ProviderKind::Claude;
        let channel = 6_325_961_000_000_200;
        let _gate = admitting_gate(channel);
        let card = card(channel);
        let (key, message_id) = (card.0.session_key.clone(), card.1);
        post(&pool, &card).await;

        let during = Arc::new(std::sync::Mutex::new(None));
        let seen = during.clone();
        let refused = || async {
            *seen.lock().unwrap() = pointer(&pool, &key).await;
            false
        };
        let outcome = run_recap_suggest(&provider, &pool, card.clone(), refused).await;
        assert_eq!(outcome, RecapSuggestOutcome::NotEnqueued);
        assert_eq!(*during.lock().unwrap(), Some(message_id as i64));
        assert_eq!(pointer(&pool, &key).await, Some(message_id as i64));

        let shared = shared_on(&pool).await;
        let (channel_id, source) = (
            serenity::ChannelId::new(channel),
            serenity::MessageId::new(message_id),
        );
        let enqueue = || {
            let reply = "g2e suggested reply";
            let enqueue = crate::services::discord::enqueue_internal_followup;
            enqueue(&shared, &provider, channel_id, source, reply, "g2e")
        };
        let outcome = run_recap_suggest(&provider, &pool, card.clone(), enqueue).await;
        assert_eq!(outcome, RecapSuggestOutcome::Enqueued { consumed: true });
        assert_eq!(pointer(&pool, &key).await, None);
        let queued = shared
            .mailbox(channel_id)
            .snapshot()
            .await
            .intervention_queue;
        let queued: Vec<_> = queued
            .iter()
            .map(|i| (i.message_id, i.text.as_str()))
            .collect();
        assert_eq!(queued, [(source, "g2e suggested reply")]);

        let again = AtomicUsize::new(0);
        let enqueue = || {
            again.fetch_add(1, Ordering::SeqCst);
            std::future::ready(true)
        };
        let outcome = run_recap_suggest(&provider, &pool, card, enqueue).await;
        assert_eq!(outcome, RecapSuggestOutcome::Stale);
        assert_eq!(again.load(Ordering::SeqCst), 0);

        pool.close().await;
        db.drop().await;
    }

    // A clear click on a fenced channel changes nothing; on an admitted one a failed clear keeps the
    // card and its pointer, and only a clear that succeeded consumes them.
    #[tokio::test]
    async fn g2e_admitted_recap_clear_keeps_the_card_until_the_clear_succeeds_pg() {
        let test = "services::discord::idle_recap_interaction::g2e_pg_tests::g2e_admitted_recap_clear_keeps_the_card_until_the_clear_succeeds_pg";
        if !api_child(test) {
            return;
        }
        let _root = crate::config::TestRuntimeRootGuard::new();
        let tmux = ScriptedTmux::install();
        let (db, pool) = postgres().await;
        let shared = shared_on(&pool).await;
        let api = Recorder::start().await;
        crate::services::discord::internal_api::init(api.port, None);
        let provider = ProviderKind::Claude;
        for (n, step) in ["fenced", "failed", "cleared"].into_iter().enumerate() {
            let channel = serenity::ChannelId::new(6_325_961_000_000_300 + n as u64);
            let channel_name = format!("g2e-recap-clear-{n}");
            let name = provider.build_tmux_session_name(&channel_name);
            map_channel(&shared, channel, &channel_name).await;
            shared
                .core
                .lock()
                .await
                .sessions
                .get_mut(&channel)
                .unwrap()
                .session_id = Some("sid".into());
            let key = channel_key(&shared, &name);
            Case::Stored(Stored::Legacy)
                .seed(&pool, &key, &name, channel.get())
                .await;
            let message_id = 6_325_961_000_000_400 + n as u64;
            sqlx::query(
                "UPDATE sessions SET idle_recap_message_id = $2, idle_recap_channel_id = $3,
                        idle_recap_posted_at = NOW() + INTERVAL '1 minute'
                  WHERE session_key = $1",
            )
            .bind(&key)
            .bind(message_id as i64)
            .bind(channel.get() as i64)
            .execute(&pool)
            .await
            .expect("recap card pointer");
            let alive = process(&name, 67_000 + n as u32);
            let gate = Gate::protect(provider.clone(), channel.get()).unwrap();
            let _health = fence::test_health::Clear::new(&gate);
            let _closing = (step == "fenced").then(|| gate.close().unwrap());
            let busy = shared.session_transition_lock(channel);
            let _busy = match step {
                "failed" => Some(busy.lock_owned().await),
                _ => None,
            };
            api.take();
            tmux.take_calls();

            let acks = AtomicUsize::new(0);
            let acknowledge = || {
                acks.fetch_add(1, Ordering::SeqCst);
                async {}
            };
            let click = clear_recap_card(
                &api.http,
                &shared,
                &provider,
                channel,
                message_id,
                acknowledge,
            );
            let result = click.await;
            let card_calls = api
                .take()
                .into_iter()
                .filter(|call| call.contains(&message_id.to_string()))
                .count();
            let kept = pointer(&pool, &key).await == Some(message_id as i64);
            let alive = alive.load(Ordering::SeqCst);
            match step {
                "fenced" => {
                    let reply = result.expect("fenced click").unwrap_or_default();
                    assert!(reply.contains("입력이 전환 중"), "{reply}");
                    assert_eq!(acks.load(Ordering::SeqCst), 0);
                    assert!(kept && alive && card_calls == 0, "fenced: nothing changes");
                    assert_eq!(tmux.take_calls(), Vec::<String>::new());
                }
                "failed" => {
                    assert!(result.is_err(), "the clear itself failed");
                    assert!(
                        kept && alive && card_calls == 0,
                        "failed: card and pointer kept"
                    );
                }
                _ => {
                    assert_eq!(result.expect("cleared click"), None);
                    assert!(!kept && !alive && card_calls > 0, "cleared: card consumed");
                }
            }
            crate::services::session_backend::remove_process_session(&name);
        }
        db.drop().await;
    }
}
