use super::outbox::{
    build_minimal_dispatch_message, format_dispatch_message, prefix_dispatch_message,
    review_submission_hint,
};
use super::resolve_channel_alias;
use super::thread_reuse::{
    clear_thread_for_channel_pg, get_thread_for_channel_pg, set_thread_for_channel_pg,
    try_reuse_thread,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use crate::dispatch::dispatch_destination_provider_override;
pub(crate) use crate::services::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind, DispatchMessagePostOutcome,
    DispatchNotifyDeliveryResult, DispatchTransport, ReviewFollowupKind,
    dispatch_delivery_correlation_id, dispatch_delivery_semantic_event_id,
    send_dispatch_with_delivery_guard,
};
use crate::services::discord_delivery_metadata::{
    CardIssueInfo, DispatchDeliveryMetadata, dispatch_context_value, load_card_issue_info,
    load_dispatch_delivery_metadata, parse_pg_dispatch_context,
    resolve_dispatch_delivery_channel_pg, resolve_review_followup_channel_pg,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use sqlite_test::OptionalExtension;
use sqlx::{PgPool, Row as SqlxRow};
use std::sync::OnceLock;

const SLOT_THREAD_RESET_MESSAGE_LIMIT: u64 = 500;
const SLOT_THREAD_RESET_MAX_AGE_DAYS: i64 = 7;
const SLOT_THREAD_MAX_SLOTS: i64 = 32;
const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

#[derive(Clone, Debug)]
struct SlotThreadBinding {
    agent_id: String,
    slot_index: i64,
    thread_id: Option<String>,
}

pub(crate) fn discord_api_base_url() -> String {
    std::env::var("AGENTDESK_DISCORD_API_BASE_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DISCORD_API_BASE.to_string())
}

pub(crate) fn discord_api_url(base_url: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn shared_discord_http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

fn resolve_dispatch_thread_owner_user_id(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
) -> Option<u64> {
    let config = crate::config::load_graceful();
    crate::server::routes::escalation::effective_owner_user_id_with_backends(db, pg_pool, &config)
}

fn context_slot_index(dispatch_context: Option<&serde_json::Value>) -> Option<i64> {
    dispatch_context
        .and_then(|ctx| ctx.get("slot_index"))
        .and_then(|value| value.as_i64())
}

fn context_reset_slot_thread_before_reuse(dispatch_context: Option<&serde_json::Value>) -> bool {
    dispatch_context
        .and_then(|ctx| ctx.get("reset_slot_thread_before_reuse"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn dispatch_type_requires_independent_slot_thread(dispatch_type: Option<&str>) -> bool {
    matches!(dispatch_type, Some("review-decision"))
}

#[cfg(test)]
mod slot_routing_pure_tests {
    use super::dispatch_type_requires_independent_slot_thread;

    #[test]
    fn review_decision_requires_independent_slot_thread() {
        assert!(dispatch_type_requires_independent_slot_thread(Some(
            "review-decision"
        )));
        assert!(!dispatch_type_requires_independent_slot_thread(Some(
            "implementation"
        )));
        assert!(!dispatch_type_requires_independent_slot_thread(Some(
            "review"
        )));
        assert!(!dispatch_type_requires_independent_slot_thread(None));
    }
}

/// #750: announce-bot reaction sync target. Command bot owns `⏳` (added at
/// turn start, removed at turn end) and adds `✅` on response delivery; the
/// announce-bot sync runs only for (a) failed/cancelled dispatches — to clean
/// stale `✅`/`⏳` and add `❌` — and (b) completions that did not pass
/// through the live command-bot path (api / recovery / supervisor), where
/// the announce-bot `✅` is the only terminal-state signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DispatchMessageTarget {
    channel_id: String,
    message_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DispatchStatusReactionState {
    Succeeded,
    Failed,
}

fn dispatch_reaction_emoji_path(emoji: char) -> Option<&'static str> {
    match emoji {
        '⏳' => Some("%E2%8F%B3"),
        '✅' => Some("%E2%9C%85"),
        '❌' => Some("%E2%9D%8C"),
        _ => None,
    }
}

fn parse_dispatch_message_target(dispatch_context: Option<&str>) -> Option<DispatchMessageTarget> {
    let context = dispatch_context_value(dispatch_context)?;
    let channel_id = context
        .get("discord_message_channel_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let message_id = context
        .get("discord_message_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    Some(DispatchMessageTarget {
        channel_id: channel_id.to_string(),
        message_id: message_id.to_string(),
    })
}

/// #1445 fallback command-bot credential names checked when the loaded
/// `discord.bots.*` config doesn't surface any provider-bound bots (e.g.
/// pre-onboarding bootstraps). Tokens are only consumed when a credential
/// file actually exists on disk.
const COMMAND_BOT_PENDING_FALLBACK_NAMES: &[&str] = &["claude", "codex", "command", "command_2"];

/// #1445: enumerate tokens of currently-launchable command bots — i.e.
/// bots whose `discord.bots.<name>.provider` is set (these are the bots
/// that add the `⏳` pending marker at turn start). `load_discord_bot_launch_configs`
/// already filters by agent-channel mapping and deduplicates by token, so
/// each returned entry represents a distinct command-bot identity. We
/// supplement with on-disk credential files for the canonical fallback
/// names so the cleanup still works when the YAML omits inline tokens.
fn collect_command_bot_pending_tokens() -> Vec<String> {
    let mut tokens: Vec<String> =
        crate::services::discord::settings::load_discord_bot_launch_configs()
            .into_iter()
            .map(|launch| launch.token)
            .collect();
    for name in COMMAND_BOT_PENDING_FALLBACK_NAMES {
        let Some(token) = crate::credential::read_bot_token(name) else {
            continue;
        };
        if !tokens.iter().any(|existing| existing == &token) {
            tokens.push(token);
        }
    }
    tokens
}

async fn update_dispatch_reaction_presence(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    target: &DispatchMessageTarget,
    emoji: char,
    present: bool,
) -> Result<(), String> {
    let encoded_emoji = dispatch_reaction_emoji_path(emoji)
        .ok_or_else(|| format!("unsupported dispatch reaction emoji: {emoji}"))?;
    let url = discord_api_url(
        base_url,
        &format!(
            "/channels/{}/messages/{}/reactions/{}/@me",
            target.channel_id, target.message_id, encoded_emoji
        ),
    );
    let response = if present {
        client
            .put(&url)
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await
            .map_err(|error| {
                format!(
                    "failed to add reaction {emoji} to dispatch message {}: {error}",
                    target.message_id
                )
            })?
    } else {
        client
            .delete(&url)
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await
            .map_err(|error| {
                format!(
                    "failed to remove reaction {emoji} from dispatch message {}: {error}",
                    target.message_id
                )
            })?
    };

    // Discord returns 404 when we try to remove a reaction that isn't present.
    // That's expected when announce bot never added the emoji in the first
    // place (the common case now that command bot owns ⏳), so treat
    // 404-on-remove as success.
    if response.status().is_success() || (!present && response.status().as_u16() == 404) {
        return Ok(());
    }

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let action = if present { "add" } else { "remove" };
    Err(format!(
        "failed to {action} reaction {emoji} for dispatch message {}: {status} {body}",
        target.message_id
    ))
}

async fn apply_dispatch_status_reaction_state(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    target: &DispatchMessageTarget,
    state: DispatchStatusReactionState,
) -> Result<(), String> {
    match state {
        DispatchStatusReactionState::Succeeded => {
            // Clean announce-bot's own ⏳/❌ (404-tolerant) then add ✅.
            // Command bot's separate ✅ (if any) is not touched.
            update_dispatch_reaction_presence(client, token, base_url, target, '⏳', false).await?;
            update_dispatch_reaction_presence(client, token, base_url, target, '❌', false).await?;
            update_dispatch_reaction_presence(client, token, base_url, target, '✅', true).await
        }
        DispatchStatusReactionState::Failed => {
            // Clean announce-bot's own ⏳/✅ (404-tolerant) then add ❌.
            // #1445: also DELETE the command-bot's `⏳` via each launchable
            // command-bot token — repair paths that bypass the live
            // command-bot cleanup (queue/API cancel, orphan recovery) leave
            // the pending marker in place, so without this users see
            // ⏳ + ❌ together and can't tell whether the dispatch is
            // in-progress or failed. The cross-bot cleanup is best-effort:
            // a 401/403 from a stale or revoked token must not block the
            // authoritative ❌ PUT, so failures are logged and swallowed.
            // Command bot's ✅ added on response delivery (turn_bridge:1537)
            // is a separate @user reaction and will still render alongside
            // ❌ — inevitable cross-bot collision on failed turns that
            // returned text. ❌ remains the authoritative failure signal.
            update_dispatch_reaction_presence(client, token, base_url, target, '⏳', false).await?;
            for owner_token in collect_command_bot_pending_tokens() {
                if owner_token == token {
                    // Already cleaned via the announce-bot @me DELETE above.
                    continue;
                }
                if let Err(error) = update_dispatch_reaction_presence(
                    client,
                    &owner_token,
                    base_url,
                    target,
                    '⏳',
                    false,
                )
                .await
                {
                    tracing::debug!(
                        message_id = %target.message_id,
                        %error,
                        "[dispatch] #1445 best-effort command-bot ⏳ cleanup skipped (treating as harmless)"
                    );
                }
            }
            update_dispatch_reaction_presence(client, token, base_url, target, '✅', false).await?;
            update_dispatch_reaction_presence(client, token, base_url, target, '❌', true).await
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HttpDispatchTransport {
    announce_bot_token: Option<String>,
    discord_api_base: String,
    thread_owner_user_id: Option<u64>,
    pg_pool: Option<PgPool>,
}

impl HttpDispatchTransport {
    pub(crate) fn from_runtime(db: &crate::db::Db) -> Self {
        Self::from_runtime_with_pg(Some(db), None)
    }

    pub(crate) fn from_runtime_with_pg(
        db: Option<&crate::db::Db>,
        pg_pool: Option<PgPool>,
    ) -> Self {
        Self {
            announce_bot_token: crate::credential::read_bot_token("announce"),
            discord_api_base: discord_api_base_url(),
            thread_owner_user_id: resolve_dispatch_thread_owner_user_id(db, pg_pool.as_ref()),
            pg_pool,
        }
    }

    fn with_context(
        announce_bot_token: Option<&str>,
        discord_api_base: &str,
        thread_owner_user_id: Option<u64>,
    ) -> Self {
        Self {
            announce_bot_token: announce_bot_token.map(str::to_string),
            discord_api_base: discord_api_base.to_string(),
            thread_owner_user_id,
            pg_pool: None,
        }
    }
}

impl DispatchTransport for HttpDispatchTransport {
    fn pg_pool(&self) -> Option<&PgPool> {
        self.pg_pool.as_ref()
    }

    fn send_dispatch(
        &self,
        db: Option<crate::db::Db>,
        agent_id: String,
        title: String,
        card_id: String,
        dispatch_id: String,
    ) -> impl std::future::Future<Output = Result<DispatchNotifyDeliveryResult, String>> + Send
    {
        let transport = self.clone();
        async move {
            let token = match transport.announce_bot_token.as_deref() {
                Some(token) => token,
                None => {
                    tracing::warn!(
                        "[dispatch] No announce bot token (missing credential/announce_bot_token)"
                    );
                    return Err("no announce bot token".into());
                }
            };
            send_dispatch_to_discord_inner_with_context_pg(
                db.as_ref(),
                &agent_id,
                &title,
                &card_id,
                &dispatch_id,
                token,
                &transport.discord_api_base,
                transport.thread_owner_user_id,
                transport.pg_pool.as_ref(),
            )
            .await
        }
    }

    fn send_review_followup(
        &self,
        db: Option<crate::db::Db>,
        review_dispatch_id: String,
        card_id: String,
        channel_id_num: u64,
        message: String,
        kind: ReviewFollowupKind,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let transport = self.clone();
        async move {
            let token = transport
                .announce_bot_token
                .as_deref()
                .ok_or_else(|| "no announce bot token".to_string())?;
            send_review_result_message_via_http(
                db.as_ref(),
                transport.pg_pool.as_ref(),
                &review_dispatch_id,
                &card_id,
                channel_id_num,
                &message,
                kind,
                token,
                &transport.discord_api_base,
            )
            .await
        }
    }
}

// #750: dispatch_reaction_emoji_path + parse_dispatch_message_target removed.
// They fed the announce-bot lifecycle emoji writer that now no-ops. The
// `discord_message_channel_id` / `discord_message_id` context fields are
// still persisted by persist_dispatch_message_target_on_pg (used by the
// message post path) — reading them is just no longer needed here.

async fn persist_dispatch_message_target_on_pg(
    pool: &PgPool,
    dispatch_id: &str,
    channel_id: &str,
    message_id: &str,
) -> Result<(), String> {
    let existing: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT context FROM task_dispatches WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres dispatch context for {dispatch_id}: {error}"))?
    .flatten();

    let mut context = existing
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    context["discord_message_channel_id"] = serde_json::json!(channel_id);
    context["discord_message_id"] = serde_json::json!(message_id);

    sqlx::query(
        "UPDATE task_dispatches
         SET context = $1,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(context.to_string())
    .bind(dispatch_id)
    .execute(pool)
    .await
    .map_err(|error| {
        format!("persist postgres dispatch message target for {dispatch_id}: {error}")
    })?;
    Ok(())
}

fn is_discord_length_error(status: reqwest::StatusCode, body: &str) -> bool {
    if status != reqwest::StatusCode::BAD_REQUEST {
        return false;
    }
    let lowered = body.to_ascii_lowercase();
    body.contains("BASE_TYPE_MAX_LENGTH")
        || lowered.contains("2000 or fewer in length")
        || lowered.contains("100 or fewer in length")
        || lowered.contains("string value is too long")
        || (body.contains("50035") && lowered.contains("length"))
}

/// Pure POST helper — no pre-truncation. Used by the unified outbound API
/// (see `crate::services::discord::outbound`) which owns the length policy.
pub(crate) async fn post_raw_message_once(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    channel_id: &str,
    message: &str,
) -> Result<String, DispatchMessagePostError> {
    let message_url = discord_api_url(base_url, &format!("/channels/{channel_id}/messages"));
    let response = client
        .post(&message_url)
        .header("Authorization", format!("Bot {}", token))
        .json(&serde_json::json!({"content": message}))
        .send()
        .await
        .map_err(|error| {
            DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                format!("failed to post dispatch message to {channel_id}: {error}"),
            )
        })?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let kind = if is_discord_length_error(status, &body) {
            DispatchMessagePostErrorKind::MessageTooLong
        } else {
            DispatchMessagePostErrorKind::Other
        };
        return Err(DispatchMessagePostError::new(
            kind,
            format!("failed to post dispatch message to {channel_id}: {status} {body}"),
        ));
    }
    let body = response
        .json::<serde_json::Value>()
        .await
        .map_err(|error| {
            DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                format!("failed to parse dispatch message response for {channel_id}: {error}"),
            )
        })?;
    body.get("id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .ok_or_else(|| {
            DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                format!("dispatch message response for {channel_id} omitted message id"),
            )
        })
}

/// Pure PATCH helper used by the unified outbound API for edit operations.
pub(crate) async fn edit_raw_message_once(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    channel_id: &str,
    message_id: &str,
    content: &str,
) -> Result<String, DispatchMessagePostError> {
    let url = discord_api_url(
        base_url,
        &format!("/channels/{channel_id}/messages/{message_id}"),
    );
    let response = client
        .patch(url)
        .header("Authorization", format!("Bot {}", token))
        .json(&serde_json::json!({ "content": content }))
        .send()
        .await
        .map_err(|error| {
            DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                format!("failed to edit dispatch message {message_id}: {error}"),
            )
        })?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let kind = if is_discord_length_error(status, &body) {
            DispatchMessagePostErrorKind::MessageTooLong
        } else {
            DispatchMessagePostErrorKind::Other
        };
        return Err(DispatchMessagePostError::new(
            kind,
            format!("Discord edit failed for message {message_id}: {status} {body}"),
        ));
    }

    let body = response
        .json::<serde_json::Value>()
        .await
        .map_err(|error| {
            DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                format!("failed to parse dispatch edit response for {message_id}: {error}"),
            )
        })?;
    body.get("id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .ok_or_else(|| {
            DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                format!("dispatch edit response for {message_id} omitted message id"),
            )
        })
}

pub(super) async fn post_dispatch_message_to_channel(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    channel_id: &str,
    message: &str,
    minimal_message: &str,
) -> Result<String, DispatchMessagePostError> {
    post_dispatch_message_to_channel_with_delivery(
        client,
        token,
        base_url,
        channel_id,
        message,
        minimal_message,
        None,
    )
    .await
    .map(|outcome| outcome.message_id)
}

pub(super) async fn post_dispatch_message_to_channel_with_delivery(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    channel_id: &str,
    message: &str,
    minimal_message: &str,
    dispatch_id: Option<&str>,
) -> Result<DispatchMessagePostOutcome, DispatchMessagePostError> {
    // #1436: dispatch_outbox is the first production callsite using the v3
    // outbound envelope directly. The compatibility re-export remains for
    // older producers while this path exercises message/policy/decision/result.
    use crate::services::discord::outbound::delivery::{deliver_outbound, first_raw_message_id};
    use crate::services::discord::outbound::message::{DiscordOutboundMessage, OutboundTarget};
    use crate::services::discord::outbound::policy::DiscordOutboundPolicy;
    use crate::services::discord::outbound::result::{DeliveryResult, FallbackUsed};
    use crate::services::discord::outbound::{HttpOutboundClient, OutboundDeduper};
    use poise::serenity_prelude::ChannelId;

    let outbound_client =
        HttpOutboundClient::new(client.clone(), token.to_string(), base_url.to_string());
    let dedup = OutboundDeduper::new();
    let correlation_id = dispatch_id.map(dispatch_delivery_correlation_id);
    let semantic_event_id = dispatch_id.map(dispatch_delivery_semantic_event_id);
    let mut policy = DiscordOutboundPolicy::dispatch_outbox();
    let (delivery_correlation_id, delivery_semantic_event_id) =
        match (correlation_id.as_ref(), semantic_event_id.as_ref()) {
            (Some(correlation_id), Some(semantic_event_id)) => {
                (correlation_id.clone(), semantic_event_id.clone())
            }
            _ => {
                policy = policy.without_idempotency();
                (
                    "dispatch:adhoc".to_string(),
                    "dispatch:adhoc:notify".to_string(),
                )
            }
        };
    let target_channel_id = channel_id
        .parse::<u64>()
        .map(ChannelId::new)
        .map_err(|error| {
            DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                format!("invalid dispatch outbound channel id {channel_id}: {error}"),
            )
        })?;
    let outbound_msg = DiscordOutboundMessage::new(
        delivery_correlation_id,
        delivery_semantic_event_id,
        message,
        OutboundTarget::Channel(target_channel_id),
        policy,
    )
    .with_summary(minimal_message.to_string());

    match deliver_outbound(&outbound_client, &dedup, outbound_msg).await {
        DeliveryResult::Sent { messages, .. } => {
            let message_id = first_raw_message_id(&messages).unwrap_or_default();
            Ok(DispatchMessagePostOutcome {
                message_id: message_id.clone(),
                delivery: DispatchNotifyDeliveryResult {
                    status: "success".to_string(),
                    dispatch_id: dispatch_id.unwrap_or("").to_string(),
                    action: "notify".to_string(),
                    correlation_id,
                    semantic_event_id,
                    target_channel_id: Some(channel_id.to_string()),
                    message_id: Some(message_id),
                    fallback_kind: None,
                    detail: None,
                },
            })
        }
        DeliveryResult::Fallback {
            messages,
            fallback_used,
            ..
        } => {
            let message_id = first_raw_message_id(&messages).unwrap_or_default();
            if matches!(fallback_used, FallbackUsed::MinimalFallback) {
                tracing::warn!(
                    "[dispatch] Message too long for channel {channel_id}; retried with minimal fallback"
                );
            }
            Ok(DispatchMessagePostOutcome {
                message_id: message_id.clone(),
                delivery: DispatchNotifyDeliveryResult {
                    status: "fallback".to_string(),
                    dispatch_id: dispatch_id.unwrap_or("").to_string(),
                    action: "notify".to_string(),
                    correlation_id,
                    semantic_event_id,
                    target_channel_id: Some(channel_id.to_string()),
                    message_id: Some(message_id),
                    fallback_kind: Some(match fallback_used {
                        FallbackUsed::MinimalFallback => "MinimalFallback".to_string(),
                        FallbackUsed::LengthCompacted => "Truncated".to_string(),
                        other => format!("{other:?}"),
                    }),
                    detail: Some("shared outbound API used degraded delivery".to_string()),
                },
            })
        }
        DeliveryResult::Duplicate { .. } => Ok(DispatchMessagePostOutcome {
            message_id: String::new(),
            delivery: DispatchNotifyDeliveryResult {
                status: "duplicate".to_string(),
                dispatch_id: dispatch_id.unwrap_or("").to_string(),
                action: "notify".to_string(),
                correlation_id,
                semantic_event_id,
                target_channel_id: Some(channel_id.to_string()),
                message_id: None,
                fallback_kind: None,
                detail: Some("shared outbound API deduplicated delivery".to_string()),
            },
        }),
        DeliveryResult::Skip { .. } => Err(DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            format!("unexpected skip for channel {channel_id}"),
        )),
        DeliveryResult::PermanentFailure { reason } => {
            let kind = if reason.to_ascii_lowercase().contains("base_type_max_length")
                || reason.contains("length")
            {
                DispatchMessagePostErrorKind::MessageTooLong
            } else {
                DispatchMessagePostErrorKind::Other
            };
            Err(DispatchMessagePostError::new(kind, reason))
        }
    }
}

/// #750: persists the posted dispatch message target (channel_id + message_id)
/// so downstream consumers can locate the original dispatch post, but no
/// longer adds the `⏳` pending emoji reaction. The announce bot reaction
/// path was retired; the command bot's turn-lifecycle emojis remain the
/// single source of truth. The helper signature is unchanged so callers
/// don't need to re-thread http client/token availability through.
pub(super) async fn persist_dispatch_message_target_and_add_pending_reaction_with_pg(
    _db: Option<&crate::db::Db>,
    _client: &reqwest::Client,
    _token: &str,
    _base_url: &str,
    dispatch_id: &str,
    channel_id: &str,
    message_id: &str,
    pg_pool: Option<&PgPool>,
) -> Result<(), String> {
    let pool = pg_pool.ok_or_else(|| {
        format!("postgres pool required while saving message target for {dispatch_id}")
    })?;
    persist_dispatch_message_target_on_pg(pool, dispatch_id, channel_id, message_id).await?;
    Ok(())
}

/// #750: narrow-path dispatch-status reaction sync.
///
/// Command bot owns ⏳ (stop control) and ✅ on response delivery for live
/// turns. This function runs only for terminal states where the announce-bot
/// reaction is still meaningful:
///
/// - `completed`: enqueue is gated on the transition source in
///   `set_dispatch_status_on_conn`; only non-live paths (api, recovery,
///   supervisor) reach this function, and they need the terminal ✅ here
///   because command bot was never involved.
/// - `failed` / `cancelled`: always reached. Command bot unconditionally
///   adds ✅ whenever a response was delivered (turn_bridge:1537), so a
///   failing dispatch that returned any text would otherwise show a false
///   green check. The full-state reconcile (404-tolerant cleanup of
///   announce-bot's own stale ⏳/✅ plus a fresh ❌) is the authoritative
///   failure signal.
///
/// `pending` / `dispatched` are never enqueued — command bot's ⏳ is the
/// single ⏳ source.
pub(crate) async fn sync_dispatch_status_reaction(
    db: &crate::db::Db,
    dispatch_id: &str,
) -> Result<(), String> {
    sync_dispatch_status_reaction_with_pg(Some(db), None, dispatch_id).await
}

pub(crate) async fn sync_dispatch_status_reaction_with_pg(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    dispatch_id: &str,
) -> Result<(), String> {
    let Some((status, context)) = load_dispatch_reaction_row(db, pg_pool, dispatch_id).await?
    else {
        return Ok(());
    };
    let target = parse_dispatch_message_target(context.as_deref());

    let state = match status.as_str() {
        "completed" => DispatchStatusReactionState::Succeeded,
        "failed" | "cancelled" => DispatchStatusReactionState::Failed,
        _ => return Ok(()),
    };

    let Some(target) = target else {
        return Ok(());
    };

    let Some(token) = crate::credential::read_bot_token("announce") else {
        return Err("no announce bot token".to_string());
    };
    let base_url = discord_api_base_url();
    apply_dispatch_status_reaction_state(
        shared_discord_http_client(),
        &token,
        &base_url,
        &target,
        state,
    )
    .await
}

async fn load_dispatch_reaction_row(
    _db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    dispatch_id: &str,
) -> Result<Option<(String, Option<String>)>, String> {
    let pool = pg_pool.ok_or_else(|| {
        format!("dispatch reaction sync for {dispatch_id} requires postgres pool")
    })?;
    let row = sqlx::query("SELECT status, context FROM task_dispatches WHERE id = $1")
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            format!("load postgres dispatch reaction target for {dispatch_id}: {error}")
        })?;
    row.map(|row| {
        Ok((
            row.try_get("status").map_err(|error| {
                format!("read postgres dispatch status for {dispatch_id}: {error}")
            })?,
            row.try_get("context").map_err(|error| {
                format!("read postgres dispatch context for {dispatch_id}: {error}")
            })?,
        ))
    })
    .transpose()
}

fn thread_id_from_slot_map(thread_id_map: Option<&str>, channel_id: u64) -> Option<String> {
    thread_id_map
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|map| {
            map.get(&channel_id.to_string())
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
        })
}

async fn persist_dispatch_slot_index_pg(
    pool: &PgPool,
    dispatch_id: &str,
    slot_index: i64,
) -> Result<(), String> {
    let existing: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT context FROM task_dispatches WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres dispatch context for {dispatch_id}: {error}"))?
    .flatten();
    let mut context = existing
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    if context.get("slot_index").and_then(|value| value.as_i64()) == Some(slot_index) {
        return Ok(());
    }
    context["slot_index"] = serde_json::json!(slot_index);
    sqlx::query(
        "UPDATE task_dispatches
         SET context = $1,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(context.to_string())
    .bind(dispatch_id)
    .execute(pool)
    .await
    .map_err(|error| format!("persist postgres slot index for {dispatch_id}: {error}"))?;
    Ok(())
}

async fn persist_dispatch_slot_index_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    slot_index: i64,
) -> Result<(), String> {
    let existing: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT context
         FROM task_dispatches
         WHERE id = $1
         FOR UPDATE",
    )
    .bind(dispatch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("load postgres dispatch context for {dispatch_id}: {error}"))?
    .flatten();
    let mut context = existing
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    if context.get("slot_index").and_then(|value| value.as_i64()) == Some(slot_index) {
        return Ok(());
    }
    context["slot_index"] = serde_json::json!(slot_index);
    sqlx::query(
        "UPDATE task_dispatches
         SET context = $1,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(context.to_string())
    .bind(dispatch_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("persist postgres slot index for {dispatch_id}: {error}"))?;
    Ok(())
}

async fn ensure_agent_slot_pool_rows_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_pool_size: i64,
) -> Result<(), String> {
    for slot_index in 0..slot_pool_size.clamp(1, 32) {
        sqlx::query(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ($1, $2, '{}'::jsonb)
             ON CONFLICT (agent_id, slot_index) DO NOTHING",
        )
        .bind(agent_id)
        .bind(slot_index)
        .execute(pool)
        .await
        .map_err(|error| {
            format!("ensure postgres slot pool row {agent_id}:{slot_index}: {error}")
        })?;
    }
    Ok(())
}

async fn slot_has_active_dispatch_excluding_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent_id: &str,
    slot_index: i64,
    exclude_dispatch_id: Option<&str>,
) -> Result<bool, String> {
    let exclude_id = exclude_dispatch_id.unwrap_or("");
    let auto_queue_active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM auto_queue_entries
         WHERE agent_id = $1
           AND slot_index = $2
           AND status = 'dispatched'
           AND COALESCE(dispatch_id, '') != $3",
    )
    .bind(agent_id)
    .bind(slot_index)
    .bind(exclude_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| {
        format!("load postgres active slot entries for {agent_id}:{slot_index}: {error}")
    })?;
    if auto_queue_active > 0 {
        return Ok(true);
    }

    let rows = sqlx::query(
        "SELECT id, context
         FROM task_dispatches
         WHERE to_agent_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(agent_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| {
        format!("load postgres active dispatches for {agent_id}:{slot_index}: {error}")
    })?;

    for row in rows {
        let dispatch_id: String = row.try_get("id").map_err(|error| {
            format!("read postgres dispatch id for {agent_id}:{slot_index}: {error}")
        })?;
        if dispatch_id == exclude_id {
            continue;
        }
        let context: Option<String> = row.try_get("context").ok().flatten();
        let Some(context) = context else {
            continue;
        };
        let Some(context_json) = serde_json::from_str::<serde_json::Value>(&context).ok() else {
            continue;
        };
        if context_json
            .get("slot_index")
            .and_then(|value| value.as_i64())
            != Some(slot_index)
        {
            continue;
        }
        if context_json
            .get("sidecar_dispatch")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        if context_json.get("phase_gate").is_some() {
            continue;
        }
        return Ok(true);
    }

    Ok(false)
}

async fn read_slot_thread_binding_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
    channel_id: u64,
) -> Result<Option<SlotThreadBinding>, String> {
    ensure_agent_slot_pool_rows_pg(pool, agent_id, slot_index + 1).await?;
    let thread_id_map: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT thread_id_map::text
         FROM auto_queue_slots
         WHERE agent_id = $1 AND slot_index = $2",
    )
    .bind(agent_id)
    .bind(slot_index)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres slot thread map for {agent_id}:{slot_index}: {error}"))?
    .flatten();
    Ok(Some(SlotThreadBinding {
        agent_id: agent_id.to_string(),
        slot_index,
        thread_id: thread_id_from_slot_map(thread_id_map.as_deref(), channel_id),
    }))
}

fn push_unique_thread_candidate(candidates: &mut Vec<String>, thread_id: Option<&str>) {
    let Some(thread_id) = thread_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };
    if !candidates.iter().any(|existing| existing == thread_id) {
        candidates.push(thread_id.to_string());
    }
}

async fn recent_slot_thread_history_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<Vec<String>, String> {
    let rows = sqlx::query(
        "SELECT id, thread_id, context
         FROM task_dispatches
         WHERE to_agent_id = $1
           AND thread_id IS NOT NULL
           AND BTRIM(thread_id) != ''
         ORDER BY COALESCE(updated_at, created_at) DESC",
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load postgres slot history for {agent_id}:{slot_index}: {error}"))?;

    let mut candidates = Vec::new();
    for row in rows {
        let dispatch_id: String = row.try_get("id").map_err(|error| {
            format!("read postgres dispatch id for {agent_id}:{slot_index}: {error}")
        })?;
        let thread_id: Option<String> = match row.try_get("thread_id") {
            Ok(thread_id) => thread_id,
            Err(error) => {
                tracing::warn!(
                    dispatch_id,
                    agent_id,
                    slot_index,
                    %error,
                    "[dispatch] failed to decode postgres thread_id while checking recent slot history"
                );
                continue;
            }
        };
        let context: Option<String> = match row.try_get("context") {
            Ok(context) => context,
            Err(error) => {
                tracing::warn!(
                    dispatch_id,
                    agent_id,
                    slot_index,
                    %error,
                    "[dispatch] failed to decode postgres dispatch context while checking recent slot history"
                );
                continue;
            }
        };
        let matches_slot = parse_pg_dispatch_context(
            &dispatch_id,
            context.as_deref(),
            "recent_slot_thread_history_pg",
        )
        .and_then(|value| value.get("slot_index").and_then(|value| value.as_i64()))
            == Some(slot_index);
        if matches_slot {
            push_unique_thread_candidate(&mut candidates, thread_id.as_deref());
        }
    }
    Ok(candidates)
}

async fn collect_slot_thread_candidates_pg(
    pool: &PgPool,
    agent_id: &str,
    card_id: &str,
    slot_binding: Option<&SlotThreadBinding>,
    channel_id: u64,
    include_card_thread: bool,
    include_recent_slot_history: bool,
) -> Result<Vec<String>, String> {
    let mut candidates = Vec::new();
    push_unique_thread_candidate(
        &mut candidates,
        slot_binding.and_then(|binding| binding.thread_id.as_deref()),
    );
    if include_card_thread {
        push_unique_thread_candidate(
            &mut candidates,
            get_thread_for_channel_pg(pool, card_id, channel_id)
                .await?
                .as_deref(),
        );
    }
    if include_recent_slot_history && let Some(binding) = slot_binding {
        for thread_id in recent_slot_thread_history_pg(pool, agent_id, binding.slot_index).await? {
            push_unique_thread_candidate(&mut candidates, Some(thread_id.as_str()));
        }
    }
    Ok(candidates)
}

async fn allocate_manual_slot_binding_pg(
    pool: &PgPool,
    agent_id: &str,
    dispatch_id: &str,
    channel_id: u64,
) -> Result<Option<SlotThreadBinding>, String> {
    ensure_agent_slot_pool_rows_pg(pool, agent_id, SLOT_THREAD_MAX_SLOTS).await?;
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres manual slot allocation: {error}"))?;

    for slot_index in 0..SLOT_THREAD_MAX_SLOTS {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(format!("agentdesk:slot:{agent_id}:{slot_index}"))
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!("lock postgres slot allocation for {agent_id}:{slot_index}: {error}")
            })?;

        if slot_has_active_dispatch_excluding_pg_tx(
            &mut tx,
            agent_id,
            slot_index,
            Some(dispatch_id),
        )
        .await?
        {
            continue;
        }
        persist_dispatch_slot_index_pg_tx(&mut tx, dispatch_id, slot_index).await?;
        tx.commit()
            .await
            .map_err(|error| format!("commit postgres manual slot allocation: {error}"))?;
        return read_slot_thread_binding_pg(pool, agent_id, slot_index, channel_id).await;
    }
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres manual slot allocation miss: {error}"))?;
    Ok(None)
}

async fn resolve_slot_thread_binding_pg(
    pool: &PgPool,
    agent_id: &str,
    card_id: &str,
    dispatch_id: &str,
    dispatch_context: Option<&serde_json::Value>,
    dispatch_type: Option<&str>,
    channel_id: u64,
) -> Result<Option<SlotThreadBinding>, String> {
    if let Some(slot_index) = context_slot_index(dispatch_context) {
        return read_slot_thread_binding_pg(pool, agent_id, slot_index, channel_id).await;
    }

    let auto_queue_slot: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT slot_index
         FROM auto_queue_entries
         WHERE dispatch_id = $1
           AND agent_id = $2
           AND slot_index IS NOT NULL
         LIMIT 1",
    )
    .bind(dispatch_id)
    .bind(agent_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres dispatch slot for {dispatch_id}: {error}"))?
    .flatten();

    if let Some(slot_index) = auto_queue_slot {
        let binding = read_slot_thread_binding_pg(pool, agent_id, slot_index, channel_id).await?;
        persist_dispatch_slot_index_pg(pool, dispatch_id, slot_index).await?;
        return Ok(binding);
    }

    if dispatch_type_requires_independent_slot_thread(dispatch_type) {
        let binding =
            allocate_manual_slot_binding_pg(pool, agent_id, dispatch_id, channel_id).await?;
        if binding.is_none() {
            return Err(format!(
                "no free slot available for independent {dispatch_type:?} dispatch {dispatch_id}"
            ));
        }
        return Ok(binding);
    }

    let same_card_slot: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT slot_index
             FROM auto_queue_entries
             WHERE kanban_card_id = $1
               AND agent_id = $2
               AND status IN ('pending', 'dispatched')
               AND slot_index IS NOT NULL
             ORDER BY CASE status WHEN 'dispatched' THEN 0 ELSE 1 END,
                      priority_rank ASC
             LIMIT 1",
    )
    .bind(card_id)
    .bind(agent_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres card slot for {card_id}: {error}"))?
    .flatten();

    if let Some(slot_index) = same_card_slot {
        let binding = read_slot_thread_binding_pg(pool, agent_id, slot_index, channel_id).await?;
        persist_dispatch_slot_index_pg(pool, dispatch_id, slot_index).await?;
        return Ok(binding);
    }

    allocate_manual_slot_binding_pg(pool, agent_id, dispatch_id, channel_id).await
}

async fn upsert_slot_thread_id_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
    channel_id: u64,
    thread_id: &str,
) -> Result<(), String> {
    let existing: String = sqlx::query_scalar::<_, Option<String>>(
        "SELECT COALESCE(thread_id_map::text, '{}')
         FROM auto_queue_slots
         WHERE agent_id = $1 AND slot_index = $2",
    )
    .bind(agent_id)
    .bind(slot_index)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres slot map for {agent_id}:{slot_index}: {error}"))?
    .flatten()
    .unwrap_or_else(|| "{}".to_string());
    let mut map: serde_json::Value = serde_json::from_str::<serde_json::Value>(&existing)
        .ok()
        .filter(|value| value.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    map[channel_id.to_string()] = serde_json::json!(thread_id);
    sqlx::query(
        "UPDATE auto_queue_slots
         SET thread_id_map = $1::jsonb,
             updated_at = NOW()
         WHERE agent_id = $2 AND slot_index = $3",
    )
    .bind(map.to_string())
    .bind(agent_id)
    .bind(slot_index)
    .execute(pool)
    .await
    .map_err(|error| format!("save postgres slot map for {agent_id}:{slot_index}: {error}"))?;
    Ok(())
}

async fn clear_slot_thread_id_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
    channel_id: u64,
) -> Result<(), String> {
    let existing: String = sqlx::query_scalar::<_, Option<String>>(
        "SELECT COALESCE(thread_id_map::text, '{}')
         FROM auto_queue_slots
         WHERE agent_id = $1 AND slot_index = $2",
    )
    .bind(agent_id)
    .bind(slot_index)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres slot map for {agent_id}:{slot_index}: {error}"))?
    .flatten()
    .unwrap_or_else(|| "{}".to_string());
    if let Ok(mut map) = serde_json::from_str::<serde_json::Value>(&existing)
        && let Some(obj) = map.as_object_mut()
    {
        obj.remove(&channel_id.to_string());
        sqlx::query(
            "UPDATE auto_queue_slots
             SET thread_id_map = $1::jsonb,
                 updated_at = NOW()
             WHERE agent_id = $2 AND slot_index = $3",
        )
        .bind(map.to_string())
        .bind(agent_id)
        .bind(slot_index)
        .execute(pool)
        .await
        .map_err(|error| format!("clear postgres slot map for {agent_id}:{slot_index}: {error}"))?;
    }
    Ok(())
}

fn discord_thread_created_at(
    thread_id: &str,
    thread_info: &serde_json::Value,
) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Some(timestamp) = thread_info
        .get("thread_metadata")
        .and_then(|metadata| metadata.get("create_timestamp"))
        .and_then(|value| value.as_str())
    {
        if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(timestamp) {
            return Some(parsed.with_timezone(&chrono::Utc));
        }
    }

    let raw_id = thread_id.parse::<u64>().ok()?;
    let timestamp_ms = (raw_id >> 22) + 1_420_070_400_000;
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(timestamp_ms as i64)
}

async fn reset_stale_slot_thread_if_needed(
    _db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    client: &reqwest::Client,
    token: &str,
    discord_api_base: &str,
    dispatch_id: &str,
    slot_binding: &SlotThreadBinding,
) -> Result<bool, String> {
    let Some(thread_id) = slot_binding.thread_id.as_deref() else {
        return Ok(false);
    };

    let thread_info_url = discord_api_url(discord_api_base, &format!("/channels/{thread_id}"));
    let response = client
        .get(&thread_info_url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .map_err(|err| format!("failed to inspect slot thread {thread_id}: {err}"))?;

    if !response.status().is_success() {
        return Ok(false);
    }

    let thread_info = response
        .json::<serde_json::Value>()
        .await
        .map_err(|err| format!("failed to parse slot thread {thread_id}: {err}"))?;
    let total_message_sent = thread_info
        .get("total_message_sent")
        .and_then(|value| value.as_u64())
        .or_else(|| {
            thread_info
                .get("message_count")
                .and_then(|value| value.as_u64())
        })
        .unwrap_or(0);
    let message_limit_hit = total_message_sent > SLOT_THREAD_RESET_MESSAGE_LIMIT;
    let age_limit_hit = discord_thread_created_at(thread_id, &thread_info)
        .map(|created_at| {
            chrono::Utc::now().signed_duration_since(created_at)
                > chrono::Duration::days(SLOT_THREAD_RESET_MAX_AGE_DAYS)
        })
        .unwrap_or(false);

    if !message_limit_hit && !age_limit_hit {
        return Ok(false);
    }

    tracing::info!(
        "[dispatch] resetting stale slot thread before dispatch {}: agent={} slot={} messages={} age_limit_hit={}",
        dispatch_id,
        slot_binding.agent_id,
        slot_binding.slot_index,
        total_message_sent,
        age_limit_hit,
    );
    let pool = pg_pool.ok_or_else(|| {
        format!("postgres pool required while resetting stale slot thread for {dispatch_id}")
    })?;
    crate::services::auto_queue::runtime::reset_slot_thread_bindings_excluding_pg(
        pool,
        &slot_binding.agent_id,
        slot_binding.slot_index,
        Some(dispatch_id),
    )
    .await?;
    Ok(true)
}

async fn archive_duplicate_slot_threads(
    client: &reqwest::Client,
    token: &str,
    discord_api_base: &str,
    pg_pool: Option<&PgPool>,
    expected_parent: u64,
    keep_thread_id: &str,
    candidate_thread_ids: &[String],
) {
    for thread_id in candidate_thread_ids {
        if thread_id == keep_thread_id {
            continue;
        }

        let thread_info_url = discord_api_url(discord_api_base, &format!("/channels/{thread_id}"));
        let response = match client
            .get(&thread_info_url)
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!(
                    "[dispatch] Failed to inspect duplicate slot thread {thread_id}: {err}"
                );
                continue;
            }
        };

        if !response.status().is_success() {
            continue;
        }

        let thread_info = match response.json::<serde_json::Value>().await {
            Ok(thread_info) => thread_info,
            Err(err) => {
                tracing::warn!(
                    "[dispatch] Failed to parse duplicate slot thread {thread_id}: {err}"
                );
                continue;
            }
        };

        let parent_id = thread_info
            .get("parent_id")
            .and_then(|value| value.as_str())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default();
        if parent_id != expected_parent {
            continue;
        }

        let already_archived = thread_info
            .get("thread_metadata")
            .and_then(|metadata| metadata.get("archived"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if already_archived {
            continue;
        }

        match super::thread_reuse::should_defer_thread_archive_pg(pg_pool, thread_id).await {
            Ok(true) => {
                tracing::warn!(
                    "[dispatch] Skipping duplicate slot thread archive for {thread_id}: active turn or fresh inflight still present"
                );
                continue;
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(
                    "[dispatch] Skipping duplicate slot thread archive for {thread_id}: active-check failed: {err}"
                );
                continue;
            }
        }

        match client
            .patch(&thread_info_url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"archived": true}))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("[dispatch] Archived duplicate slot thread {thread_id}");
            }
            Ok(resp) => {
                tracing::warn!(
                    "[dispatch] Failed to archive duplicate slot thread {thread_id}: {}",
                    resp.status()
                );
            }
            Err(err) => {
                tracing::warn!(
                    "[dispatch] Failed to archive duplicate slot thread {thread_id}: {err}"
                );
            }
        }
    }
}

async fn build_slot_thread_name_pg(
    pool: &PgPool,
    dispatch_id: &str,
    card_id: &str,
    slot_index: i64,
    issue_number: Option<i64>,
    title: &str,
) -> Result<String, String> {
    let mut batch_phase_for_label = 0i64;
    let group_info = sqlx::query(
        "SELECT run_id, COALESCE(thread_group, 0)::BIGINT AS thread_group, COALESCE(batch_phase, 0)::BIGINT AS batch_phase
         FROM auto_queue_entries
         WHERE dispatch_id = $1
         LIMIT 1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres slot group for {dispatch_id}: {error}"))?
    .map(|row| {
        Ok::<_, String>((
            row.try_get::<String, _>("run_id")
                .map_err(|error| format!("read postgres run_id for {dispatch_id}: {error}"))?,
            row.try_get::<i64, _>("thread_group").map_err(|error| {
                format!("read postgres thread_group for {dispatch_id}: {error}")
            })?,
            row.try_get::<i64, _>("batch_phase").map_err(|error| {
                format!("read postgres batch_phase for {dispatch_id}: {error}")
            })?,
        ))
    })
    .transpose()?
    .or(
        sqlx::query(
            "SELECT run_id, COALESCE(thread_group, 0)::BIGINT AS thread_group, COALESCE(batch_phase, 0)::BIGINT AS batch_phase
             FROM auto_queue_entries
             WHERE kanban_card_id = $1
               AND status IN ('pending', 'dispatched')
             ORDER BY CASE status WHEN 'dispatched' THEN 0 ELSE 1 END,
                      priority_rank ASC
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load postgres card slot group for {card_id}: {error}"))?
        .map(|row| {
            Ok::<_, String>((
                row.try_get::<String, _>("run_id")
                    .map_err(|error| format!("read postgres run_id for {card_id}: {error}"))?,
                row.try_get::<i64, _>("thread_group").map_err(|error| {
                    format!("read postgres thread_group for {card_id}: {error}")
                })?,
                row.try_get::<i64, _>("batch_phase").map_err(|error| {
                    format!("read postgres batch_phase for {card_id}: {error}")
                })?,
            ))
        })
        .transpose()?,
    );

    let grouped_issue_label = if let Some((run_id, thread_group, batch_phase)) = group_info {
        batch_phase_for_label = batch_phase;
        let rows = sqlx::query(
            "SELECT kc.github_issue_number, e.kanban_card_id
             FROM auto_queue_entries e
             JOIN kanban_cards kc ON kc.id = e.kanban_card_id
             WHERE e.run_id = $1
               AND COALESCE(e.thread_group, 0) = $2
               AND COALESCE(e.batch_phase, 0) = (
                   SELECT COALESCE(e2.batch_phase, 0)
                   FROM auto_queue_entries e2
                   WHERE e2.kanban_card_id = $3
                     AND e2.run_id = $1
                   LIMIT 1
               )
               AND kc.github_issue_number IS NOT NULL
             ORDER BY e.priority_rank ASC",
        )
        .bind(&run_id)
        .bind(thread_group)
        .bind(card_id)
        .fetch_all(pool)
        .await
        .map_err(|error| format!("load postgres grouped issues for {card_id}: {error}"))?;
        let issues: Vec<(i64, String)> = rows
            .into_iter()
            .filter_map(|row| {
                Some((
                    row.try_get::<i64, _>("github_issue_number").ok()?,
                    row.try_get::<String, _>("kanban_card_id").ok()?,
                ))
            })
            .collect();
        if issues.len() > 1 {
            Some(
                issues
                    .into_iter()
                    .map(|(issue_number, issue_card_id)| {
                        if issue_card_id == card_id {
                            format!("▸{}", issue_number)
                        } else {
                            format!("#{}", issue_number)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        } else {
            None
        }
    } else {
        None
    };

    let base = if let Some(grouped) = grouped_issue_label {
        grouped
    } else if let Some(number) = issue_number {
        let short_title: String = title.chars().take(80).collect();
        format!("#{} {}", number, short_title)
    } else {
        title.chars().take(90).collect()
    };
    let phase_prefix = if batch_phase_for_label > 0 {
        format!("P{} ", batch_phase_for_label)
    } else {
        String::new()
    };
    Ok(format!("[slot {}] {}{}", slot_index, phase_prefix, base)
        .chars()
        .take(100)
        .collect())
}

async fn latest_work_dispatch_thread_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<String>, String> {
    let rows = sqlx::query(
        "SELECT id, thread_id, context
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type IN ('implementation', 'rework')
         ORDER BY
           CASE status
             WHEN 'dispatched' THEN 0
             WHEN 'pending' THEN 1
             WHEN 'completed' THEN 2
             ELSE 3
           END,
           COALESCE(completed_at, updated_at, created_at) DESC",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load postgres work dispatch thread for {card_id}: {error}"))?;

    for row in rows {
        let dispatch_id: String = row
            .try_get("id")
            .map_err(|error| format!("read postgres work dispatch id for {card_id}: {error}"))?;
        let thread_id: Option<String> = match row.try_get("thread_id") {
            Ok(thread_id) => thread_id,
            Err(error) => {
                tracing::warn!(
                    dispatch_id,
                    card_id,
                    %error,
                    "[dispatch] failed to decode postgres work thread_id while loading reusable thread"
                );
                continue;
            }
        };
        if let Some(thread_id) = thread_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            return Ok(Some(thread_id));
        }
        let context: Option<String> = match row.try_get("context") {
            Ok(context) => context,
            Err(error) => {
                tracing::warn!(
                    dispatch_id,
                    card_id,
                    %error,
                    "[dispatch] failed to decode postgres work context while loading reusable thread"
                );
                continue;
            }
        };
        if let Some(thread_id) = parse_pg_dispatch_context(
            &dispatch_id,
            context.as_deref(),
            "latest_work_dispatch_thread_pg",
        )
        .and_then(|value| {
            value
                .get("thread_id")
                .and_then(|value| value.as_str())
                .map(std::string::ToString::to_string)
        })
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        {
            return Ok(Some(thread_id));
        }
    }

    Ok(None)
}

#[cfg(not(feature = "legacy-sqlite-tests"))]
pub(super) fn resolve_dispatch_delivery_channel_on_conn<T>(
    _conn: &T,
    _agent_id: &str,
    _card_id: &str,
    _dispatch_type: Option<&str>,
    _dispatch_context: Option<&str>,
) -> Result<Option<String>, String> {
    Ok(None)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn resolve_dispatch_delivery_channel_on_conn(
    conn: &sqlite_test::Connection,
    agent_id: &str,
    card_id: &str,
    dispatch_type: Option<&str>,
    dispatch_context: Option<&str>,
) -> Result<Option<String>, String> {
    let provider_override = if dispatch_type == Some("review") {
        dispatch_destination_provider_override(dispatch_type, dispatch_context)
    } else if dispatch_type == Some("review-decision") {
        match dispatch_destination_provider_override(dispatch_type, dispatch_context) {
            Some(provider) => Some(provider),
            None => latest_completed_review_provider_on_conn(conn, card_id)?,
        }
    } else {
        None
    };

    if let Some(provider) = provider_override.filter(|provider| !provider.trim().is_empty()) {
        if let Some(channel) = crate::db::agents::resolve_agent_channel_for_provider_on_conn(
            conn,
            agent_id,
            Some(&provider),
        )
        .map_err(|error| {
            format!("resolve sqlite provider channel for {agent_id} ({provider}): {error}")
        })? {
            return Ok(Some(channel));
        }
    }

    crate::db::agents::resolve_agent_dispatch_channel_on_conn(conn, agent_id, dispatch_type)
        .map_err(|error| format!("resolve sqlite dispatch channel for {agent_id}: {error}"))
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn latest_completed_review_provider_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> Result<Option<String>, String> {
    let context: Option<String> = conn
        .query_row(
            "SELECT context
             FROM task_dispatches
             WHERE kanban_card_id = ?1
               AND dispatch_type = 'review'
               AND status = 'completed'
             ORDER BY COALESCE(completed_at, updated_at) DESC, updated_at DESC
             LIMIT 1",
            [card_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("load sqlite review provider for {card_id}: {error}"))?;

    Ok(dispatch_context_value(context.as_deref()).and_then(|ctx| {
        ctx.get("from_provider")
            .and_then(|value| value.as_str())
            .map(str::to_string)
    }))
}

async fn add_thread_member_to_dispatch_thread(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    thread_id: &str,
    user_id: u64,
) -> Result<(), String> {
    let thread_info_url = discord_api_url(base_url, &format!("/channels/{thread_id}"));
    let response = client
        .get(&thread_info_url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .map_err(|err| format!("failed to inspect thread {thread_id}: {err}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "failed to inspect thread {thread_id}: {status} {body}"
        ));
    }

    let thread_info = response
        .json::<serde_json::Value>()
        .await
        .map_err(|err| format!("failed to parse thread {thread_id}: {err}"))?;
    let is_archived = thread_info
        .get("thread_metadata")
        .and_then(|metadata| metadata.get("archived"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    if is_archived {
        let response = client
            .patch(&thread_info_url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"archived": false}))
            .send()
            .await
            .map_err(|err| format!("failed to unarchive thread {thread_id}: {err}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "failed to unarchive thread {thread_id}: {status} {body}"
            ));
        }
    }

    let member_url = discord_api_url(
        base_url,
        &format!("/channels/{thread_id}/thread-members/{user_id}"),
    );
    let response = client
        .put(&member_url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .map_err(|err| format!("failed to add user {user_id} to thread {thread_id}: {err}"))?;

    if response.status().is_success() {
        Ok(())
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!(
            "failed to add user {user_id} to thread {thread_id}: {status} {body}"
        ))
    }
}

async fn maybe_add_owner_to_dispatch_thread(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    thread_id: &str,
    dispatch_id: &str,
    owner_user_id: Option<u64>,
) {
    let Some(owner_user_id) = owner_user_id else {
        return;
    };

    if let Err(err) =
        add_thread_member_to_dispatch_thread(client, token, base_url, thread_id, owner_user_id)
            .await
    {
        tracing::warn!(
            "[dispatch] Failed to add owner {} to thread {} for dispatch {}: {}",
            owner_user_id,
            thread_id,
            dispatch_id,
            err
        );
    }
}

/// Send a dispatch notification to the target agent's Discord channel.
/// Message format: `DISPATCH:<dispatch_id> - <title>\n<issue_url>`
/// The `DISPATCH:<uuid>` prefix is required for the dcserver to link the
/// resulting Claude session back to the kanban card (via `parse_dispatch_id`).
pub(crate) async fn send_dispatch_to_discord(
    db: &crate::db::Db,
    agent_id: &str,
    title: &str,
    card_id: &str,
    dispatch_id: &str,
) -> Result<(), String> {
    let transport = HttpDispatchTransport::from_runtime(db);
    send_dispatch_with_delivery_guard(
        Some(db),
        None,
        agent_id,
        title,
        card_id,
        dispatch_id,
        &transport,
    )
    .await
    .map(|_| ())
}

pub(crate) async fn send_dispatch_to_discord_with_pg(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    agent_id: &str,
    title: &str,
    card_id: &str,
    dispatch_id: &str,
) -> Result<(), String> {
    let transport = HttpDispatchTransport::from_runtime_with_pg(db, pg_pool.cloned());
    send_dispatch_with_delivery_guard(
        db,
        pg_pool,
        agent_id,
        title,
        card_id,
        dispatch_id,
        &transport,
    )
    .await
    .map(|_| ())
}

pub(crate) async fn send_dispatch_to_discord_with_pg_result(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    agent_id: &str,
    title: &str,
    card_id: &str,
    dispatch_id: &str,
) -> Result<DispatchNotifyDeliveryResult, String> {
    let transport = HttpDispatchTransport::from_runtime_with_pg(db, pg_pool.cloned());
    send_dispatch_with_delivery_guard(
        db,
        pg_pool,
        agent_id,
        title,
        card_id,
        dispatch_id,
        &transport,
    )
    .await
}

pub(super) async fn send_dispatch_to_discord_with_transport<T: DispatchTransport>(
    db: &crate::db::Db,
    agent_id: &str,
    title: &str,
    card_id: &str,
    dispatch_id: &str,
    transport: &T,
) -> Result<(), String> {
    send_dispatch_with_delivery_guard(
        Some(db),
        transport.pg_pool(),
        agent_id,
        title,
        card_id,
        dispatch_id,
        transport,
    )
    .await
    .map(|_| ())
}

async fn send_dispatch_to_discord_inner_with_context(
    db: &crate::db::Db,
    agent_id: &str,
    title: &str,
    card_id: &str,
    dispatch_id: &str,
    token: &str,
    discord_api_base: &str,
    thread_owner_user_id: Option<u64>,
) -> Result<DispatchNotifyDeliveryResult, String> {
    send_dispatch_to_discord_inner_with_context_pg(
        Some(db),
        agent_id,
        title,
        card_id,
        dispatch_id,
        token,
        discord_api_base,
        thread_owner_user_id,
        None,
    )
    .await
}

async fn send_dispatch_to_discord_inner_with_context_pg(
    db: Option<&crate::db::Db>,
    agent_id: &str,
    title: &str,
    card_id: &str,
    dispatch_id: &str,
    token: &str,
    discord_api_base: &str,
    thread_owner_user_id: Option<u64>,
    pg_pool: Option<&PgPool>,
) -> Result<DispatchNotifyDeliveryResult, String> {
    // Determine dispatch type + status before attempting Discord delivery.
    let DispatchDeliveryMetadata {
        dispatch_type,
        status: dispatch_status,
        context: dispatch_context,
    } = load_dispatch_delivery_metadata(db, pg_pool, dispatch_id).await?;

    if !matches!(
        dispatch_status.as_deref(),
        Some("pending") | Some("dispatched")
    ) {
        tracing::info!(
            "[dispatch] Skipping Discord send for dispatch {} with non-deliverable status {:?}",
            dispatch_id,
            dispatch_status
        );
        return Ok(DispatchNotifyDeliveryResult::success(
            dispatch_id,
            "notify",
            format!("skipped non-deliverable status {:?}", dispatch_status),
        ));
    }

    // Look up agent's discord channel
    let pool = pg_pool.ok_or_else(|| "postgres pool required for channel lookup".to_string())?;
    let channel_id = resolve_dispatch_delivery_channel_pg(
        pool,
        agent_id,
        card_id,
        dispatch_type.as_deref(),
        dispatch_context.as_deref(),
    )
    .await?;

    let channel_id = match channel_id {
        Some(id) if !id.is_empty() => id,
        _ => {
            tracing::warn!(
                "[dispatch] No discord_channel_id for agent {agent_id}, skipping message"
            );
            return Err(format!("no discord channel for agent {agent_id}"));
        }
    };

    // Parse channel ID as u64, or resolve alias via role_map.json
    let channel_id_num: u64 = match channel_id.parse() {
        Ok(n) => n,
        Err(_) => {
            // Try resolving channel name alias from role_map.json
            match resolve_channel_alias(&channel_id) {
                Some(n) => n,
                None => {
                    tracing::warn!(
                        "[dispatch] Cannot resolve channel '{channel_id}' for agent {agent_id}"
                    );
                    return Err(format!(
                        "cannot resolve channel '{channel_id}' for agent {agent_id}"
                    ));
                }
            }
        }
    };

    // Look up the issue URL and number for context
    let CardIssueInfo {
        issue_url,
        issue_number,
    } = load_card_issue_info(db, pg_pool, card_id).await?;

    let dispatch_context_json = dispatch_context_value(dispatch_context.as_deref());

    // For review dispatches, look up reviewed commit SHA, branch, and target provider from context
    let message = format_dispatch_message(
        dispatch_id,
        title,
        issue_url.as_deref(),
        issue_number,
        dispatch_type.as_deref(),
        dispatch_context.as_deref(),
    );
    let minimal_message = build_minimal_dispatch_message(
        dispatch_id,
        title,
        issue_url.as_deref(),
        issue_number,
        dispatch_type.as_deref(),
        dispatch_context.as_deref(),
    );

    // ── Thread reuse: every dispatch now resolves into a slot thread ──
    let client = reqwest::Client::new();
    if !crate::dispatch::dispatch_type_uses_thread_routing(dispatch_type.as_deref()) {
        let channel_id_text = channel_id_num.to_string();
        let outcome = post_dispatch_message_to_channel_with_delivery(
            &client,
            token,
            &discord_api_base,
            &channel_id_text,
            &message,
            &minimal_message,
            Some(dispatch_id),
        )
        .await
        .map_err(|error| error.to_string())?;
        persist_dispatch_message_target_and_add_pending_reaction_with_pg(
            db,
            &client,
            token,
            &discord_api_base,
            dispatch_id,
            &channel_id_text,
            &outcome.message_id,
            Some(pool),
        )
        .await?;
        tracing::info!(
            "[dispatch] Sent primary-channel dispatch {dispatch_id} to {agent_id} (channel {channel_id_text})"
        );
        return Ok(outcome.delivery);
    }
    let independent_slot_thread =
        dispatch_type_requires_independent_slot_thread(dispatch_type.as_deref());
    let mut slot_binding = resolve_slot_thread_binding_pg(
        pool,
        agent_id,
        card_id,
        dispatch_id,
        dispatch_context_json.as_ref(),
        dispatch_type.as_deref(),
        channel_id_num,
    )
    .await?;
    let reset_slot_thread_before_reuse =
        context_reset_slot_thread_before_reuse(dispatch_context_json.as_ref());
    if reset_slot_thread_before_reuse
        && let Some(binding) = slot_binding.clone()
        && binding.thread_id.is_some()
    {
        crate::services::auto_queue::runtime::reset_slot_thread_bindings_excluding_pg(
            pool,
            &binding.agent_id,
            binding.slot_index,
            Some(dispatch_id),
        )
        .await?;
        slot_binding = read_slot_thread_binding_pg(
            pool,
            &binding.agent_id,
            binding.slot_index,
            channel_id_num,
        )
        .await?;
    }
    if let Some(binding) = slot_binding.clone() {
        if reset_stale_slot_thread_if_needed(
            db,
            Some(pool),
            &client,
            token,
            discord_api_base,
            dispatch_id,
            &binding,
        )
        .await?
        {
            slot_binding = read_slot_thread_binding_pg(
                pool,
                &binding.agent_id,
                binding.slot_index,
                channel_id_num,
            )
            .await?;
        }
    }

    let slot_index = slot_binding
        .as_ref()
        .map(|binding| binding.slot_index)
        .or_else(|| context_slot_index(dispatch_context_json.as_ref()))
        .unwrap_or(0);
    let thread_name =
        build_slot_thread_name_pg(pool, dispatch_id, card_id, slot_index, issue_number, title)
            .await?;
    let existing_thread_ids = collect_slot_thread_candidates_pg(
        pool,
        agent_id,
        card_id,
        slot_binding.as_ref(),
        channel_id_num,
        !independent_slot_thread,
        !reset_slot_thread_before_reuse,
    )
    .await?;

    for existing_tid in &existing_thread_ids {
        match try_reuse_thread(
            &client,
            token,
            discord_api_base,
            existing_tid,
            channel_id_num,
            &thread_name,
            &message,
            &minimal_message,
            dispatch_id,
            card_id,
            db,
            Some(pool),
        )
        .await
        {
            Ok(Some((reused, delivery_outcome))) => {
                if reused {
                    if !independent_slot_thread {
                        set_thread_for_channel_pg(pool, card_id, channel_id_num, existing_tid)
                            .await?;
                    }
                    if let Some(binding) = slot_binding.as_ref() {
                        upsert_slot_thread_id_pg(
                            pool,
                            &binding.agent_id,
                            binding.slot_index,
                            channel_id_num,
                            existing_tid,
                        )
                        .await?;
                    }
                    archive_duplicate_slot_threads(
                        &client,
                        token,
                        discord_api_base,
                        Some(pool),
                        channel_id_num,
                        existing_tid,
                        &existing_thread_ids,
                    )
                    .await;
                    maybe_add_owner_to_dispatch_thread(
                        &client,
                        token,
                        &discord_api_base,
                        existing_tid,
                        dispatch_id,
                        thread_owner_user_id,
                    )
                    .await;
                    return Ok(delivery_outcome
                        .map(|outcome| outcome.delivery)
                        .unwrap_or_else(|| {
                            DispatchNotifyDeliveryResult::success(
                                dispatch_id,
                                "notify",
                                format!("reused thread {existing_tid}"),
                            )
                        }));
                }
            }
            Ok(None) => {}
            Err(error) if error.is_length_error() => return Err(error.to_string()),
            Err(error) => {
                tracing::warn!(
                    "[dispatch] Reusable thread probe failed for {existing_tid}: {error}; falling back to new thread"
                );
            }
        }
    }

    if let Some(binding) = slot_binding.as_ref() {
        clear_slot_thread_id_pg(pool, &binding.agent_id, binding.slot_index, channel_id_num)
            .await?;
    }

    let thread_url = discord_api_url(
        &discord_api_base,
        &format!("/channels/{channel_id_num}/threads"),
    );
    let thread_resp = client
        .post(&thread_url)
        .header("Authorization", format!("Bot {}", token))
        .json(&serde_json::json!({
            "name": thread_name,
            "type": 11, // PUBLIC_THREAD
            "auto_archive_duration": 1440, // 24h
        }))
        .send()
        .await;

    match thread_resp {
        Ok(tr) if tr.status().is_success() => {
            if let Ok(thread_body) = tr.json::<serde_json::Value>().await {
                let thread_id = thread_body.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if !thread_id.is_empty() {
                    // Send dispatch message into the thread BEFORE persisting thread_id.
                    // If the POST fails, we don't save thread_id so that
                    // [I-0] recovery sends to the channel and future dispatches won't
                    // reuse an empty thread.
                    match post_dispatch_message_to_channel_with_delivery(
                        &client,
                        token,
                        &discord_api_base,
                        thread_id,
                        &message,
                        &minimal_message,
                        Some(dispatch_id),
                    )
                    .await
                    {
                        Ok(outcome) => {
                            // Persist thread_id on success
                            sqlx::query(
                                "UPDATE task_dispatches
                                 SET thread_id = $1,
                                     updated_at = NOW()
                                 WHERE id = $2",
                            )
                            .bind(thread_id)
                            .bind(dispatch_id)
                            .execute(pool)
                            .await
                            .map_err(|error| {
                                format!("persist postgres thread_id for {dispatch_id}: {error}")
                            })?;
                            if !independent_slot_thread {
                                set_thread_for_channel_pg(pool, card_id, channel_id_num, thread_id)
                                    .await?;
                            }
                            if let Some(binding) = slot_binding.as_ref() {
                                upsert_slot_thread_id_pg(
                                    pool,
                                    &binding.agent_id,
                                    binding.slot_index,
                                    channel_id_num,
                                    thread_id,
                                )
                                .await?;
                            }
                            persist_dispatch_message_target_and_add_pending_reaction_with_pg(
                                db,
                                &client,
                                token,
                                &discord_api_base,
                                dispatch_id,
                                thread_id,
                                &outcome.message_id,
                                Some(pool),
                            )
                            .await?;
                            archive_duplicate_slot_threads(
                                &client,
                                token,
                                discord_api_base,
                                Some(pool),
                                channel_id_num,
                                thread_id,
                                &existing_thread_ids,
                            )
                            .await;
                            maybe_add_owner_to_dispatch_thread(
                                &client,
                                token,
                                &discord_api_base,
                                thread_id,
                                dispatch_id,
                                thread_owner_user_id,
                            )
                            .await;
                            tracing::info!(
                                "[dispatch] Created thread {thread_id} and sent dispatch {dispatch_id} to {agent_id}"
                            );
                            return Ok(outcome.delivery);
                        }
                        Err(error) => {
                            tracing::warn!(
                                "[dispatch] Thread message POST failed for dispatch {dispatch_id}: {}",
                                error
                            );
                            return Err(error.to_string());
                        }
                    }
                }
            }
            // thread_body parse failed or thread_id empty
            return Err("thread created but response parsing failed".into());
        }
        Ok(tr) => {
            // Thread creation failed — fall back to sending directly to the channel
            let status = tr.status();
            let body = tr.text().await.unwrap_or_default();
            if is_discord_length_error(status, &body) {
                return Err(format!(
                    "thread creation rejected for dispatch {dispatch_id} due to Discord length limits: {status} {body}"
                ));
            }
            tracing::warn!(
                "[dispatch] Thread creation failed ({status}), falling back to channel message"
            );
            let channel_id_text = channel_id_num.to_string();
            match post_dispatch_message_to_channel_with_delivery(
                &client,
                token,
                &discord_api_base,
                &channel_id_text,
                &message,
                &minimal_message,
                Some(dispatch_id),
            )
            .await
            {
                Ok(outcome) => {
                    persist_dispatch_message_target_and_add_pending_reaction_with_pg(
                        db,
                        &client,
                        token,
                        &discord_api_base,
                        dispatch_id,
                        &channel_id_text,
                        &outcome.message_id,
                        Some(pool),
                    )
                    .await?;
                    tracing::info!(
                        "[dispatch] Sent fallback message to {agent_id} (channel {channel_id})"
                    );
                    return Ok(outcome.delivery.with_thread_creation_fallback(format!(
                        "thread creation failed with {status}; delivered to parent channel {channel_id_text}"
                    )));
                }
                Err(e) => {
                    tracing::warn!("[dispatch] Fallback dispatch message failed: {e}");
                    return Err(e.to_string());
                }
            }
        }
        Err(e) => {
            tracing::warn!("[dispatch] Thread creation request failed: {e}");
            return Err(format!("thread creation request failed: {e}"));
        }
    }
}

async fn resolve_review_followup_target_channel(
    _db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    client: &reqwest::Client,
    token: &str,
    discord_api_base: &str,
    card_id: &str,
    channel_id_num: u64,
) -> Result<String, String> {
    let pool = pg_pool
        .ok_or_else(|| "review followup thread lookup requires postgres pool".to_string())?;
    let active_thread_id: Option<String> =
        match get_thread_for_channel_pg(pool, card_id, channel_id_num).await? {
            Some(thread_id) => Some(thread_id),
            None => latest_work_dispatch_thread_pg(pool, card_id).await?,
        };
    let channel_id = channel_id_num.to_string();

    let Some(thread_id) = active_thread_id else {
        return Ok(channel_id);
    };

    let info_url = discord_api_url(discord_api_base, &format!("/channels/{thread_id}"));
    let response = match client
        .get(&info_url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(
                "[review] Failed to inspect thread {thread_id} for review followup: {err}"
            );
            return Ok(channel_id);
        }
    };

    if !response.status().is_success() {
        tracing::warn!(
            "[review] Thread {thread_id} unavailable for review followup: HTTP {}",
            response.status()
        );
        if let Err(error) = clear_thread_for_channel_pg(pool, card_id, channel_id_num).await {
            tracing::warn!(
                "[review] failed to clear postgres thread mapping for {card_id}/{channel_id_num}: {error}"
            );
        }
        return Ok(channel_id);
    }

    let body = match response.json::<serde_json::Value>().await {
        Ok(body) => body,
        Err(err) => {
            tracing::warn!(
                "[review] Failed to parse thread {thread_id} for review followup: {err}"
            );
            return Ok(channel_id);
        }
    };

    let metadata = body.get("thread_metadata");
    let locked = metadata
        .and_then(|value| value.get("locked"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if locked {
        tracing::warn!("[review] Thread {thread_id} is locked, falling back to channel");
        if let Err(error) = clear_thread_for_channel_pg(pool, card_id, channel_id_num).await {
            tracing::warn!(
                "[review] failed to clear locked postgres thread mapping for {card_id}/{channel_id_num}: {error}"
            );
        }
        return Ok(channel_id);
    }

    let archived = metadata
        .and_then(|value| value.get("archived"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if !archived {
        return Ok(thread_id);
    }

    let mut last_error = None;
    for attempt in 1..=2 {
        match client
            .patch(&info_url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"archived": false}))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => return Ok(thread_id),
            Ok(response) => {
                let err = format!("HTTP {}", response.status());
                tracing::warn!(
                    "[review] Failed to unarchive thread {thread_id} (attempt {attempt}/2): {err}"
                );
                last_error = Some(err);
            }
            Err(err) => {
                tracing::warn!(
                    "[review] Failed to unarchive thread {thread_id} (attempt {attempt}/2): {err}"
                );
                last_error = Some(err.to_string());
            }
        }
    }

    Err(format!(
        "failed to unarchive review followup thread {thread_id}: {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    ))
}

/// Handle primary-channel followup after a counter-model review completes.
/// pass/unknown verdicts send an immediate message; improve/rework/reject
/// create a review-decision dispatch whose notify row is delivered by outbox.
pub(super) async fn send_review_result_to_primary(
    db: &crate::db::Db,
    card_id: &str,
    review_dispatch_id: &str,
    verdict: &str,
) -> Result<(), String> {
    let discord_api_base = discord_api_base_url();
    let token = crate::credential::read_bot_token("announce");
    let transport = HttpDispatchTransport::with_context(token.as_deref(), &discord_api_base, None);
    send_review_result_to_primary_with_transport(
        Some(db),
        card_id,
        review_dispatch_id,
        verdict,
        &transport,
    )
    .await
}

pub(super) async fn send_review_result_to_primary_with_transport<T: DispatchTransport>(
    db: Option<&crate::db::Db>,
    card_id: &str,
    review_dispatch_id: &str,
    verdict: &str,
    transport: &T,
) -> Result<(), String> {
    send_review_result_to_primary_with_context_and_transport(
        db,
        card_id,
        review_dispatch_id,
        verdict,
        transport,
    )
    .await
}

async fn send_review_result_to_primary_with_context(
    db: &crate::db::Db,
    card_id: &str,
    review_dispatch_id: &str,
    verdict: &str,
    token: Option<&str>,
    discord_api_base: &str,
) -> Result<(), String> {
    let transport = HttpDispatchTransport::with_context(token, discord_api_base, None);
    send_review_result_to_primary_with_context_and_transport(
        Some(db),
        card_id,
        review_dispatch_id,
        verdict,
        &transport,
    )
    .await
}

async fn send_review_result_to_primary_with_context_and_transport<T: DispatchTransport>(
    db: Option<&crate::db::Db>,
    card_id: &str,
    review_dispatch_id: &str,
    verdict: &str,
    transport: &T,
) -> Result<(), String> {
    let pool = transport
        .pg_pool()
        .ok_or_else(|| "review followup requires postgres pool".to_string())?;

    // Look up card info
    let (agent_id, title, issue_url): (String, String, Option<String>) = {
        let row = sqlx::query(
            "SELECT assigned_agent_id, title, github_issue_url
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load postgres card {card_id} for review followup: {error}"))?;
        let Some(row) = row else {
            return Err(format!("card {card_id} not found or missing agent"));
        };

        let agent_id: Option<String> = row
            .try_get("assigned_agent_id")
            .map_err(|error| format!("read postgres assigned_agent_id for {card_id}: {error}"))?;
        let title: String = row
            .try_get("title")
            .map_err(|error| format!("read postgres title for {card_id}: {error}"))?;
        let issue_url: Option<String> = row
            .try_get("github_issue_url")
            .map_err(|error| format!("read postgres github_issue_url for {card_id}: {error}"))?;

        (
            agent_id
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| format!("card {card_id} not found or missing agent"))?,
            title,
            issue_url,
        )
    };
    let review_dispatch_context: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT context FROM task_dispatches WHERE id = $1",
    )
    .bind(review_dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        format!("load postgres review dispatch context for {review_dispatch_id}: {error}")
    })?
    .flatten();
    let review_context_json = review_dispatch_context
        .as_deref()
        .and_then(|ctx| serde_json::from_str::<serde_json::Value>(ctx).ok());

    // For improve/rework/reject: create a review-decision dispatch via the
    // authoritative path and let the outbox worker deliver the message.
    if verdict != "pass" && verdict != "approved" && verdict != "unknown" {
        // #118/#420: If review automation already converged on a concrete
        // follow-up state, don't enqueue a generic review-decision dispatch on
        // top of it.
        {
            let skip = sqlx::query_scalar::<_, Option<String>>(
                "SELECT review_status FROM kanban_cards WHERE id = $1",
            )
            .bind(card_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .flatten()
            .map(|s| s == "rework_pending" || s == "dilemma_pending")
            .unwrap_or(false);
            if skip {
                tracing::info!(
                    "[review-followup] skipping review-decision for {card_id} — review automation already resolved the follow-up state"
                );
                return Ok(());
            }
        }

        let mut decision_context = serde_json::Map::new();
        decision_context.insert("verdict".to_string(), serde_json::json!(verdict));
        if let Some(provider) = review_context_json
            .as_ref()
            .and_then(|ctx| ctx.get("from_provider"))
            .and_then(|value| value.as_str())
        {
            decision_context.insert("from_provider".to_string(), serde_json::json!(provider));
        }
        if let Some(provider) = review_context_json
            .as_ref()
            .and_then(|ctx| ctx.get("target_provider"))
            .and_then(|value| value.as_str())
        {
            decision_context.insert("target_provider".to_string(), serde_json::json!(provider));
        }
        if let Some(reviewed_commit) = review_context_json
            .as_ref()
            .and_then(|ctx| ctx.get("reviewed_commit"))
            .and_then(|value| value.as_str())
        {
            decision_context.insert(
                "reviewed_commit".to_string(),
                serde_json::json!(reviewed_commit),
            );
        }

        return match create_review_decision_followup_dispatch(
            db,
            Some(pool),
            card_id,
            &agent_id,
            &format!("[리뷰 검토] {title}"),
            &serde_json::Value::Object(decision_context),
        ) {
            Ok((id, _old_status, _reused)) => {
                let payload = serde_json::json!({
                    "card_id": card_id,
                    "state": "suggestion_pending",
                    "pending_dispatch_id": id,
                    "last_verdict": verdict,
                })
                .to_string();
                let _ =
                    crate::engine::ops::review_state_sync_with_backends(db, Some(pool), &payload);
                tracing::info!(
                    "[review-followup] enqueued review-decision dispatch {} for card {}",
                    id,
                    card_id
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    "[review-followup] skipping review-decision dispatch for card {card_id}: {e}"
                );
                Err(format!(
                    "dispatch-core create failed for review-decision: {e}"
                ))
            }
        };
    }

    let channel_id = resolve_review_followup_channel_pg(pool, &agent_id)
        .await?
        .ok_or_else(|| {
            format!("agent {agent_id} missing primary discord channel for review followup")
        })?;

    let channel_id_num: u64 = match channel_id.parse() {
        Ok(n) => n,
        Err(_) => match resolve_channel_alias(&channel_id) {
            Some(n) => n,
            None => return Err(format!("cannot resolve channel alias '{channel_id}'")),
        },
    };

    let (kind, message) = if verdict == "pass" || verdict == "approved" {
        let url_line = issue_url.map(|u| format!("\n{u}")).unwrap_or_default();
        (
            ReviewFollowupKind::Pass,
            format!("✅ [리뷰 통과] {title} — done으로 이동{url_line}"),
        )
    } else {
        let url_line = issue_url.map(|u| format!("\n{u}")).unwrap_or_default();
        let review_locator = review_context_json
            .as_ref()
            .and_then(|ctx| super::outbox::review_target_hint(None, ctx))
            .map(|value| format!("\n대상: {value}"))
            .unwrap_or_default();
        let submission_hint = review_context_json
            .as_ref()
            .and_then(|ctx| review_submission_hint(Some("review"), review_dispatch_id, ctx))
            .map(|value| format!("\n누락된 verdict 제출 경로 참고: {value}"))
            .unwrap_or_default();
        (
            ReviewFollowupKind::Unknown,
            prefix_dispatch_message(
                "review-decision",
                &format!(
                    "⚠️ [리뷰 verdict 미제출] {title}\n\
                     ⛔ 코드 리뷰 금지 — 이것은 리뷰 결과 확인 요청입니다\n\
                     카운터모델이 verdict를 제출하지 않고 세션이 종료됐습니다.\n\
                     GitHub 이슈 코멘트를 확인하고 리뷰 내용이 있으면 반영해주세요.{review_locator}{submission_hint}{url_line}"
                ),
            ),
        )
    };

    transport
        .send_review_followup(
            db.cloned(),
            review_dispatch_id.to_string(),
            card_id.to_string(),
            channel_id_num,
            message,
            kind,
        )
        .await
}

fn create_review_decision_followup_dispatch(
    _db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    card_id: &str,
    agent_id: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<(String, String, bool), String> {
    let pool = pg_pool.ok_or_else(|| {
        "Postgres pool required for review-decision follow-up dispatch".to_string()
    })?;
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        {
            let card_id = card_id.to_string();
            let agent_id = agent_id.to_string();
            let title = title.to_string();
            let context = context.clone();
            move |bridge_pool| async move {
                crate::dispatch::create_dispatch_core(
                    &bridge_pool,
                    &card_id,
                    &agent_id,
                    "review-decision",
                    &title,
                    &context,
                )
                .await
                .map_err(|error| error.to_string())
            }
        },
        |error| error,
    )
}

async fn send_review_result_message_via_http(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    review_dispatch_id: &str,
    card_id: &str,
    channel_id_num: u64,
    message: &str,
    kind: ReviewFollowupKind,
    token: &str,
    discord_api_base: &str,
) -> Result<(), String> {
    // #1457: review followups use the v3 envelope directly so final
    // notification delivery shares the same message/policy/result contract
    // as dispatch outbox.
    use crate::services::discord::outbound::HttpOutboundClient;
    use crate::services::discord::outbound::delivery::deliver_outbound;
    use crate::services::discord::outbound::message::{DiscordOutboundMessage, OutboundTarget};
    use crate::services::discord::outbound::policy::DiscordOutboundPolicy;
    use crate::services::discord::outbound::result::DeliveryResult;
    use poise::serenity_prelude::ChannelId;

    let client = reqwest::Client::new();
    let target_channel = resolve_review_followup_target_channel(
        db,
        pg_pool,
        &client,
        token,
        discord_api_base,
        card_id,
        channel_id_num,
    )
    .await?;

    let outbound_client =
        HttpOutboundClient::new(client, token.to_string(), discord_api_base.to_string());
    let dedup = review_followup_deduper();
    let event_kind = match kind {
        ReviewFollowupKind::Pass => "pass",
        ReviewFollowupKind::Unknown => "unknown",
    };
    let target_channel_id = target_channel
        .parse::<u64>()
        .map(ChannelId::new)
        .map_err(|error| {
            format!("invalid review followup target channel {target_channel}: {error}")
        })?;
    let outbound_msg = DiscordOutboundMessage::new(
        format!("review:{card_id}"),
        format!("review:{review_dispatch_id}:{event_kind}:{discord_api_base}"),
        message,
        OutboundTarget::Channel(target_channel_id),
        DiscordOutboundPolicy::review_notification(),
    );

    match deliver_outbound(&outbound_client, dedup, outbound_msg).await {
        DeliveryResult::Sent { .. } | DeliveryResult::Fallback { .. } => Ok(()),
        DeliveryResult::Duplicate { .. } => {
            // Duplicate suppression is a success for the caller.
            Ok(())
        }
        DeliveryResult::Skip { .. } => Ok(()),
        DeliveryResult::PermanentFailure { reason } => match kind {
            ReviewFollowupKind::Pass => Err(format!(
                "discord request failed for pass notification: {reason}"
            )),
            ReviewFollowupKind::Unknown => Err(format!(
                "discord request failed for unknown-verdict notification: {reason}"
            )),
        },
    }
}

fn review_followup_deduper() -> &'static crate::services::discord::outbound::OutboundDeduper {
    static DEDUPER: OnceLock<crate::services::discord::outbound::OutboundDeduper> = OnceLock::new();
    DEDUPER.get_or_init(crate::services::discord::outbound::OutboundDeduper::new)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Path, State},
        http::Uri,
        response::IntoResponse,
        routing::{get, post, put},
    };
    use std::{
        collections::HashMap,
        ffi::OsString,
        sync::{Arc, Mutex},
    };

    fn test_db() -> crate::db::Db {
        crate::db::test_db()
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::services::discord::runtime_store::lock_test_env()
    }

    #[test]
    fn review_delivery_channel_uses_target_provider_from_context() {
        let db = test_db();
        let conn = db.lock().expect("sqlite conn");
        conn.execute(
            "INSERT INTO agents (
                id, name, provider, discord_channel_id, discord_channel_alt,
                discord_channel_cc, discord_channel_cdx
             ) VALUES ('agent-review-route', 'Agent', 'codex', '111', '222', '111', '222')",
            [],
        )
        .expect("seed agent");

        let channel = resolve_dispatch_delivery_channel_on_conn(
            &conn,
            "agent-review-route",
            "card-review-route",
            Some("review"),
            Some(r#"{"target_provider":"codex"}"#),
        )
        .expect("resolve delivery channel");

        assert_eq!(channel.as_deref(), Some("222"));
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }

        fn set_path(key: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn write_announce_token(root: &std::path::Path) {
        let credential_dir = crate::runtime_layout::credential_dir(root);
        std::fs::create_dir_all(&credential_dir).unwrap();
        std::fs::write(
            crate::runtime_layout::credential_token_path(root, "announce"),
            "announce-token\n",
        )
        .unwrap();
    }

    fn write_command_bot_token(root: &std::path::Path, name: &str, value: &str) {
        let credential_dir = crate::runtime_layout::credential_dir(root);
        std::fs::create_dir_all(&credential_dir).unwrap();
        std::fs::write(
            crate::runtime_layout::credential_token_path(root, name),
            format!("{value}\n"),
        )
        .unwrap();
    }

    struct TestPostgresDb {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let admin_url = postgres_admin_database_url();
            let database_name = format!(
                "agentdesk_dispatch_reaction_{}",
                uuid::Uuid::new_v4().simple()
            );
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "discord delivery tests",
            )
            .await
            .unwrap();

            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "discord delivery tests",
            )
            .await
            .unwrap()
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "discord delivery tests",
            )
            .await
            .unwrap();
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    #[derive(Clone, Debug, Default)]
    struct MockDiscordState {
        archived: bool,
        unarchive_failures_remaining: usize,
        message_length_failures_remaining: usize,
        message_length_failure_min_chars: Option<usize>,
        thread_create_status: Option<axum::http::StatusCode>,
        calls: Vec<String>,
        posted_messages: Vec<(String, String)>,
        thread_names: HashMap<String, String>,
        thread_parents: HashMap<String, String>,
    }

    async fn spawn_mock_discord_server(
        initial_archived: bool,
    ) -> (
        String,
        Arc<Mutex<MockDiscordState>>,
        tokio::task::JoinHandle<()>,
    ) {
        spawn_mock_discord_server_with_config(initial_archived, 0, 0, None, None).await
    }

    async fn spawn_mock_discord_server_with_failures(
        initial_archived: bool,
        unarchive_failures_remaining: usize,
    ) -> (
        String,
        Arc<Mutex<MockDiscordState>>,
        tokio::task::JoinHandle<()>,
    ) {
        spawn_mock_discord_server_with_config(
            initial_archived,
            unarchive_failures_remaining,
            0,
            None,
            None,
        )
        .await
    }

    async fn spawn_mock_discord_server_with_message_length_failures(
        initial_archived: bool,
        message_length_failures_remaining: usize,
        message_length_failure_min_chars: usize,
    ) -> (
        String,
        Arc<Mutex<MockDiscordState>>,
        tokio::task::JoinHandle<()>,
    ) {
        spawn_mock_discord_server_with_config(
            initial_archived,
            0,
            message_length_failures_remaining,
            Some(message_length_failure_min_chars),
            None,
        )
        .await
    }

    async fn spawn_mock_discord_server_with_thread_creation_failure(
        status: axum::http::StatusCode,
    ) -> (
        String,
        Arc<Mutex<MockDiscordState>>,
        tokio::task::JoinHandle<()>,
    ) {
        spawn_mock_discord_server_with_config(false, 0, 0, None, Some(status)).await
    }

    async fn spawn_mock_discord_server_with_config(
        initial_archived: bool,
        unarchive_failures_remaining: usize,
        message_length_failures_remaining: usize,
        message_length_failure_min_chars: Option<usize>,
        thread_create_status: Option<axum::http::StatusCode>,
    ) -> (
        String,
        Arc<Mutex<MockDiscordState>>,
        tokio::task::JoinHandle<()>,
    ) {
        async fn get_channel(
            State(state): State<Arc<Mutex<MockDiscordState>>>,
            Path(thread_id): Path<String>,
        ) -> axum::response::Response {
            if thread_id == "thread-invalid-json" {
                let mut state = state.lock().unwrap();
                state.calls.push(format!("GET /channels/{thread_id}"));
                return (
                    axum::http::StatusCode::OK,
                    [("content-type", "application/json")],
                    "not-json",
                )
                    .into_response();
            }

            let (archived, thread_name, parent_id, total_message_sent) = {
                let mut state = state.lock().unwrap();
                state.calls.push(format!("GET /channels/{thread_id}"));
                let total_message_sent = if thread_id == "thread-stale" { 501 } else { 0 };
                (
                    state.archived,
                    state
                        .thread_names
                        .get(&thread_id)
                        .cloned()
                        .unwrap_or_else(|| format!("seed-{thread_id}")),
                    state
                        .thread_parents
                        .get(&thread_id)
                        .cloned()
                        .unwrap_or_else(|| "123".to_string()),
                    total_message_sent,
                )
            };
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({
                    "id": thread_id,
                    "name": thread_name,
                    "parent_id": parent_id,
                    "total_message_sent": total_message_sent,
                    "thread_metadata": {
                        "archived": archived,
                    }
                })),
            )
                .into_response()
        }

        async fn patch_channel(
            State(state): State<Arc<Mutex<MockDiscordState>>>,
            Path(thread_id): Path<String>,
            Json(body): Json<serde_json::Value>,
        ) -> impl IntoResponse {
            let mut state = state.lock().unwrap();
            state.calls.push(format!("PATCH /channels/{thread_id}"));
            if body.get("archived").and_then(|value| value.as_bool()) == Some(false)
                && state.unarchive_failures_remaining > 0
            {
                state.unarchive_failures_remaining -= 1;
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"id": thread_id, "ok": false})),
                );
            }
            if let Some(name) = body.get("name").and_then(|value| value.as_str()) {
                state
                    .thread_names
                    .insert(thread_id.clone(), name.to_string());
            }
            if let Some(archived) = body.get("archived").and_then(|value| value.as_bool()) {
                state.archived = archived;
            }
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({"id": thread_id, "ok": true})),
            )
        }

        async fn create_thread(
            State(state): State<Arc<Mutex<MockDiscordState>>>,
            Path(channel_id): Path<String>,
            Json(body): Json<serde_json::Value>,
        ) -> impl IntoResponse {
            let mut state = state.lock().unwrap();
            state
                .calls
                .push(format!("POST /channels/{channel_id}/threads"));
            if let Some(status) = state.thread_create_status {
                return (
                    status,
                    Json(serde_json::json!({
                        "message": "mock thread creation failure"
                    })),
                );
            }
            let thread_id = "thread-created".to_string();
            state
                .thread_parents
                .insert(thread_id.clone(), channel_id.clone());
            if let Some(name) = body.get("name").and_then(|value| value.as_str()) {
                state
                    .thread_names
                    .insert(thread_id.clone(), name.to_string());
            }
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({"id": thread_id})),
            )
        }

        async fn post_message(
            State(state): State<Arc<Mutex<MockDiscordState>>>,
            Path(channel_id): Path<String>,
            Json(body): Json<serde_json::Value>,
        ) -> impl IntoResponse {
            let mut state = state.lock().unwrap();
            state
                .calls
                .push(format!("POST /channels/{channel_id}/messages"));
            let content = body
                .get("content")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            state
                .posted_messages
                .push((channel_id.clone(), content.clone()));
            if state.message_length_failures_remaining > 0
                && state
                    .message_length_failure_min_chars
                    .map(|limit| content.chars().count() >= limit)
                    .unwrap_or(false)
            {
                state.message_length_failures_remaining -= 1;
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "code": 50035,
                        "message": "Invalid Form Body",
                        "errors": {
                            "content": {
                                "_errors": [
                                    {
                                        "code": "BASE_TYPE_MAX_LENGTH",
                                        "message": "Must be 2000 or fewer in length."
                                    }
                                ]
                            }
                        }
                    })),
                );
            }
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({"id": format!("message-{channel_id}")})),
            )
        }

        async fn add_thread_member(
            State(state): State<Arc<Mutex<MockDiscordState>>>,
            Path((thread_id, user_id)): Path<(String, String)>,
        ) -> impl IntoResponse {
            let mut state = state.lock().unwrap();
            state.calls.push(format!(
                "PUT /channels/{thread_id}/thread-members/{user_id}"
            ));
            axum::http::StatusCode::NO_CONTENT
        }

        async fn add_reaction(
            State(state): State<Arc<Mutex<MockDiscordState>>>,
            Path((_channel_id, _message_id, _emoji)): Path<(String, String, String)>,
            uri: Uri,
        ) -> impl IntoResponse {
            let mut state = state.lock().unwrap();
            state.calls.push(format!("PUT {}", uri.path()));
            axum::http::StatusCode::NO_CONTENT
        }

        async fn remove_reaction(
            State(state): State<Arc<Mutex<MockDiscordState>>>,
            Path((_channel_id, _message_id, _emoji)): Path<(String, String, String)>,
            uri: Uri,
        ) -> impl IntoResponse {
            let mut state = state.lock().unwrap();
            state.calls.push(format!("DELETE {}", uri.path()));
            axum::http::StatusCode::NO_CONTENT
        }

        let state = Arc::new(Mutex::new(MockDiscordState {
            archived: initial_archived,
            unarchive_failures_remaining,
            message_length_failures_remaining,
            message_length_failure_min_chars,
            thread_create_status,
            calls: Vec::new(),
            posted_messages: Vec::new(),
            thread_names: HashMap::new(),
            thread_parents: HashMap::new(),
        }));
        let app = Router::new()
            .route(
                "/channels/{thread_id}",
                get(get_channel).patch(patch_channel),
            )
            .route("/channels/{channel_id}/threads", post(create_thread))
            .route("/channels/{channel_id}/messages", post(post_message))
            .route(
                "/channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me",
                put(add_reaction).delete(remove_reaction),
            )
            .route(
                "/channels/{thread_id}/thread-members/{user_id}",
                put(add_thread_member),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), state, handle)
    }

    #[tokio::test]
    async fn add_thread_member_unarchives_archived_thread_before_put() {
        let (base_url, state, server_handle) = spawn_mock_discord_server(true).await;
        let client = reqwest::Client::new();

        add_thread_member_to_dispatch_thread(&client, "announce-token", &base_url, "thread-1", 42)
            .await
            .unwrap();

        server_handle.abort();
        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            vec![
                "GET /channels/thread-1",
                "PATCH /channels/thread-1",
                "PUT /channels/thread-1/thread-members/42",
            ]
        );
    }

    #[tokio::test]
    async fn dispatch_outbox_direct_v3_envelope_posts_success_metadata() {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let client = reqwest::Client::new();

        let outcome = post_dispatch_message_to_channel_with_delivery(
            &client,
            "announce-token",
            &base_url,
            "123",
            "direct v3 dispatch message",
            "minimal fallback message",
            Some("dispatch-v3-outbox"),
        )
        .await
        .unwrap();

        server_handle.abort();
        assert_eq!(outcome.message_id, "message-123");
        assert_eq!(outcome.delivery.status, "success");
        assert_eq!(
            outcome.delivery.correlation_id.as_deref(),
            Some("dispatch:dispatch-v3-outbox")
        );
        assert_eq!(
            outcome.delivery.semantic_event_id.as_deref(),
            Some("dispatch:dispatch-v3-outbox:notify")
        );
        assert_eq!(outcome.delivery.target_channel_id.as_deref(), Some("123"));
        assert_eq!(outcome.delivery.message_id.as_deref(), Some("message-123"));
        assert_eq!(outcome.delivery.fallback_kind, None);

        let state = state.lock().unwrap();
        assert_eq!(state.calls, vec!["POST /channels/123/messages"]);
        assert_eq!(
            state.posted_messages,
            vec![("123".to_string(), "direct v3 dispatch message".to_string())]
        );
    }

    #[tokio::test]
    async fn post_dispatch_message_retries_with_minimal_fallback_after_length_error() {
        let (base_url, state, server_handle) =
            spawn_mock_discord_server_with_message_length_failures(false, 1, 120).await;
        let client = reqwest::Client::new();
        let primary_message = "A".repeat(180);
        let minimal_message = "minimal fallback message";

        let outcome = post_dispatch_message_to_channel_with_delivery(
            &client,
            "announce-token",
            &base_url,
            "123",
            &primary_message,
            minimal_message,
            Some("dispatch-length-fallback"),
        )
        .await
        .unwrap();

        server_handle.abort();
        assert_eq!(outcome.message_id, "message-123");
        assert_eq!(outcome.delivery.status, "fallback");
        assert_eq!(
            outcome.delivery.semantic_event_id.as_deref(),
            Some("dispatch:dispatch-length-fallback:notify")
        );
        assert_eq!(outcome.delivery.target_channel_id.as_deref(), Some("123"));

        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            vec!["POST /channels/123/messages", "POST /channels/123/messages",]
        );
        assert_eq!(
            state.posted_messages,
            vec![
                ("123".to_string(), primary_message),
                ("123".to_string(), minimal_message.to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn reused_thread_length_error_does_not_fall_back_to_creating_new_thread() {
        let (base_url, state, server_handle) =
            spawn_mock_discord_server_with_message_length_failures(false, 2, 10).await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, channel_thread_map, active_thread_id,
                    created_at, updated_at
                ) VALUES (
                    'card-length', 'Length card', 'requested', 'agent-1', 'dispatch-length',
                    '{\"123\":\"thread-existing\"}', 'thread-existing',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-length', 'card-length', 'agent-1', 'implementation', 'pending', 'Length card',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        }

        let error = send_dispatch_to_discord_inner_with_context(
            &db,
            "agent-1",
            "Length card",
            "card-length",
            "dispatch-length",
            "announce-token",
            &base_url,
            None,
        )
        .await
        .expect_err("length error after minimal retry should fail closed");

        server_handle.abort();
        assert!(error.contains("BASE_TYPE_MAX_LENGTH"));

        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            vec![
                "GET /channels/thread-existing",
                "PATCH /channels/thread-existing",
                "POST /channels/thread-existing/messages",
                "POST /channels/thread-existing/messages",
            ]
        );
        assert!(
            !state
                .calls
                .contains(&"POST /channels/123/threads".to_string()),
            "length errors on a reused thread must not trigger new thread fallback"
        );

        let conn = db.lock().unwrap();
        let thread_id: Option<String> = conn
            .query_row(
                "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-length'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(thread_id, None);
    }

    #[tokio::test]
    async fn reused_thread_probe_error_falls_back_to_creating_new_thread_after_phase_gate_dispatch()
    {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, channel_thread_map, active_thread_id,
                    created_at, updated_at
                ) VALUES (
                    'card-probe-error', 'Probe error card', 'requested', 'agent-1', 'dispatch-probe-error',
                    '{\"123\":\"thread-invalid-json\"}', 'thread-invalid-json',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-probe-error', 'card-probe-error', 'agent-1', 'implementation', 'pending', 'Probe error card',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        }

        send_dispatch_to_discord_inner_with_context(
            &db,
            "agent-1",
            "Probe error card",
            "card-probe-error",
            "dispatch-probe-error",
            "announce-token",
            &base_url,
            None,
        )
        .await
        .expect("non-length reuse probe errors should fall back to new thread creation");

        server_handle.abort();

        let state = state.lock().unwrap();
        assert_eq!(
            state.calls.first().map(String::as_str),
            Some("GET /channels/thread-invalid-json")
        );
        assert!(
            state
                .calls
                .contains(&"POST /channels/123/threads".to_string())
        );
        assert!(
            state
                .calls
                .contains(&"POST /channels/thread-created/messages".to_string())
        );
        // #750: announce bot no longer writes dispatch-lifecycle emoji
        // reactions — no PUT/DELETE reaction calls should have been issued.
        assert!(
            !state.calls.iter().any(|call| call.contains("/reactions/")),
            "#750: expected no emoji reaction HTTP calls, got {:?}",
            state
                .calls
                .iter()
                .filter(|c| c.contains("/reactions/"))
                .collect::<Vec<_>>()
        );

        let conn = db.lock().unwrap();
        let thread_id: Option<String> = conn
            .query_row(
                "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-probe-error'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(thread_id.as_deref(), Some("thread-created"));
    }

    #[tokio::test]
    async fn thread_creation_failure_records_parent_channel_send_as_fallback_delivery() {
        let (base_url, state, server_handle) =
            spawn_mock_discord_server_with_thread_creation_failure(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            )
            .await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-thread-fallback', 'Thread fallback', 'requested', 'agent-1',
                    'dispatch-thread-fallback', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
                ) VALUES (
                    'dispatch-thread-fallback', 'card-thread-fallback', 'agent-1', 'implementation',
                    'pending', 'Thread fallback', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        }

        let delivery = send_dispatch_to_discord_inner_with_context(
            &db,
            "agent-1",
            "Thread fallback",
            "card-thread-fallback",
            "dispatch-thread-fallback",
            "announce-token",
            &base_url,
            None,
        )
        .await
        .expect("parent-channel fallback delivery succeeds");

        server_handle.abort();

        assert_eq!(delivery.status, "fallback");
        assert_eq!(
            delivery.fallback_kind.as_deref(),
            Some("ThreadCreationParentChannel")
        );
        assert!(
            delivery
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("thread creation failed")
        );
        assert_eq!(delivery.target_channel_id.as_deref(), Some("123"));
        let state = state.lock().unwrap();
        assert!(
            state
                .calls
                .contains(&"POST /channels/123/threads".to_string())
        );
        assert!(
            state
                .calls
                .contains(&"POST /channels/123/messages".to_string())
        );
    }

    #[tokio::test]
    async fn send_dispatch_to_discord_adds_configured_owner_to_created_thread() {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-1', 'Test card', 'requested', 'agent-1', 'dispatch-1', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
                ) VALUES (
                    'dispatch-1', 'card-1', 'agent-1', 'implementation', 'pending', 'Test card', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        }

        send_dispatch_to_discord_inner_with_context(
            &db,
            "agent-1",
            "Test card",
            "card-1",
            "dispatch-1",
            "announce-token",
            &base_url,
            Some(343742347365974026),
        )
        .await
        .unwrap();

        server_handle.abort();

        let state = state.lock().unwrap();
        assert!(
            state
                .calls
                .contains(&"POST /channels/123/threads".to_string())
        );
        assert!(
            state
                .calls
                .contains(&"POST /channels/thread-created/messages".to_string())
        );
        // #750: no emoji reaction calls — see other tests in this file for rationale.
        assert!(
            !state.calls.iter().any(|call| call.contains("/reactions/")),
            "#750: no emoji reaction HTTP calls expected"
        );
        assert!(
            state
                .calls
                .contains(&"GET /channels/thread-created".to_string())
        );
        assert!(state.calls.contains(
            &"PUT /channels/thread-created/thread-members/343742347365974026".to_string()
        ));

        let conn = db.lock().unwrap();
        let thread_id: Option<String> = conn
            .query_row(
                "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(thread_id.as_deref(), Some("thread-created"));
        let context: Option<String> = conn
            .query_row(
                "SELECT context FROM task_dispatches WHERE id = 'dispatch-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let context = serde_json::from_str::<serde_json::Value>(&context.unwrap()).unwrap();
        assert_eq!(context["discord_message_channel_id"], "thread-created");
        assert_eq!(context["discord_message_id"], "message-thread-created");
    }

    #[tokio::test]
    async fn send_phase_gate_dispatch_to_discord_posts_to_primary_channel_without_thread() {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, active_thread_id,
                    created_at, updated_at
                ) VALUES (
                    'card-phase', 'Phase gate', 'review', 'agent-1', 'dispatch-phase', 'thread-existing',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-phase', 'card-phase', 'agent-1', 'phase-gate', 'pending', '[phase-gate P2] Final',
                    '{\"phase_gate\":{\"run_id\":\"run-1\",\"batch_phase\":1}}',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        }

        send_dispatch_to_discord_inner_with_context(
            &db,
            "agent-1",
            "[phase-gate P2] Final",
            "card-phase",
            "dispatch-phase",
            "announce-token",
            &base_url,
            Some(343742347365974026),
        )
        .await
        .unwrap();

        server_handle.abort();

        let state = state.lock().unwrap();
        // #750: phase-gate post → no emoji reaction calls (announce bot
        // writer retired). Only the message POST should have hit Discord.
        assert_eq!(state.calls, vec!["POST /channels/123/messages".to_string()]);
        assert!(
            !state.calls.iter().any(|call| call.contains("/threads")),
            "phase-gate dispatch must not create or reuse a Discord thread"
        );

        let conn = db.lock().unwrap();
        let thread_id: Option<String> = conn
            .query_row(
                "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-phase'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(thread_id, None);
        let context: Option<String> = conn
            .query_row(
                "SELECT context FROM task_dispatches WHERE id = 'dispatch-phase'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let context = serde_json::from_str::<serde_json::Value>(&context.unwrap()).unwrap();
        assert_eq!(context["discord_message_channel_id"], "123");
        assert_eq!(context["discord_message_id"], "message-123");
    }

    #[tokio::test]
    async fn reused_thread_probe_error_falls_back_to_creating_new_thread() {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, channel_thread_map, active_thread_id,
                    created_at, updated_at
                ) VALUES (
                    'card-probe-error', 'Probe error card', 'requested', 'agent-1', 'dispatch-probe-error',
                    '{\"123\":\"thread-invalid-json\"}', 'thread-invalid-json',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-probe-error', 'card-probe-error', 'agent-1', 'implementation', 'pending', 'Probe error card',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        }

        send_dispatch_to_discord_inner_with_context(
            &db,
            "agent-1",
            "Probe error card",
            "card-probe-error",
            "dispatch-probe-error",
            "announce-token",
            &base_url,
            None,
        )
        .await
        .expect("reused thread probe errors should fall back to new thread creation");

        server_handle.abort();

        let state = state.lock().unwrap();
        assert_eq!(
            state.calls.first().map(String::as_str),
            Some("GET /channels/thread-invalid-json")
        );
        assert!(
            !state
                .calls
                .contains(&"POST /channels/thread-invalid-json/messages".to_string())
        );
        assert!(
            state
                .calls
                .contains(&"POST /channels/123/threads".to_string())
        );
        assert!(
            state
                .calls
                .contains(&"POST /channels/thread-created/messages".to_string())
        );
        assert!(
            !state.calls.iter().any(|call| call.contains("/reactions/")),
            "#750: announce bot must not write dispatch-lifecycle emoji reactions, got {:?}",
            state.calls
        );

        let conn = db.lock().unwrap();
        let thread_id: Option<String> = conn
            .query_row(
                "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-probe-error'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(thread_id.as_deref(), Some("thread-created"));
    }

    #[tokio::test]
    async fn send_dispatch_reuses_recent_slot_thread_history_when_slot_map_is_empty() {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, github_issue_number,
                    created_at, updated_at
                ) VALUES (
                    'card-current', 'Reuse card', 'requested', 'agent-1', 'dispatch-current', 506,
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-old', 'Old card', 'done', 'agent-1', 'dispatch-old',
                    datetime('now', '-1 day'), datetime('now', '-1 day')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-current', 'card-current', 'agent-1', 'implementation', 'pending',
                    'Reuse card', '{\"slot_index\":1}', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, thread_id,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-old', 'card-old', 'agent-1', 'implementation', 'completed',
                    'Old card', '{\"slot_index\":1}', 'thread-history',
                    datetime('now', '-1 day'), datetime('now', '-1 day')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
                 VALUES ('agent-1', 1, '{}')",
                [],
            )
            .unwrap();
        }

        send_dispatch_to_discord_inner_with_context(
            &db,
            "agent-1",
            "Reuse card",
            "card-current",
            "dispatch-current",
            "announce-token",
            &base_url,
            None,
        )
        .await
        .unwrap();

        server_handle.abort();

        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            vec![
                "GET /channels/thread-history",
                "PATCH /channels/thread-history",
                "POST /channels/thread-history/messages",
            ]
        );
        assert!(
            !state.calls.iter().any(|call| call.contains("/reactions/")),
            "#750: announce bot must not write dispatch-lifecycle emoji reactions, got {:?}",
            state.calls
        );
        assert_eq!(
            state.thread_names.get("thread-history").map(String::as_str),
            Some("[slot 1] #506 Reuse card")
        );

        let conn = db.lock().unwrap();
        let reused_thread_id: Option<String> = conn
            .query_row(
                "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-current'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reused_thread_id.as_deref(), Some("thread-history"));

        let (active_thread_id, channel_thread_map): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT active_thread_id, channel_thread_map
                 FROM kanban_cards
                 WHERE id = 'card-current'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(active_thread_id.as_deref(), Some("thread-history"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(channel_thread_map.as_deref().unwrap())
                .unwrap()["123"],
            "thread-history"
        );

        let slot_map: String = conn
            .query_row(
                "SELECT thread_id_map
                 FROM auto_queue_slots
                 WHERE agent_id = 'agent-1' AND slot_index = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&slot_map).unwrap()["123"],
            "thread-history"
        );

        let dispatch_context: Option<String> = conn
            .query_row(
                "SELECT context FROM task_dispatches WHERE id = 'dispatch-current'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let dispatch_context =
            serde_json::from_str::<serde_json::Value>(&dispatch_context.unwrap()).unwrap();
        assert_eq!(
            dispatch_context["discord_message_channel_id"],
            "thread-history"
        );
        assert_eq!(
            dispatch_context["discord_message_id"],
            "message-thread-history"
        );
    }

    #[tokio::test]
    async fn send_dispatch_skips_recent_slot_thread_history_when_context_requests_reset() {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, github_issue_number,
                    created_at, updated_at
                ) VALUES (
                    'card-current', 'Reset card', 'requested', 'agent-1', 'dispatch-current', 507,
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-old', 'Old card', 'done', 'agent-1', 'dispatch-old',
                    datetime('now', '-1 day'), datetime('now', '-1 day')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-current', 'card-current', 'agent-1', 'implementation', 'pending',
                    'Reset card', '{\"slot_index\":1,\"reset_slot_thread_before_reuse\":true}', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, thread_id,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-old', 'card-old', 'agent-1', 'implementation', 'completed',
                    'Old card', '{\"slot_index\":1}', 'thread-history',
                    datetime('now', '-1 day'), datetime('now', '-1 day')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
                 VALUES ('agent-1', 1, '{}')",
                [],
            )
            .unwrap();
        }

        send_dispatch_to_discord_inner_with_context(
            &db,
            "agent-1",
            "Reset card",
            "card-current",
            "dispatch-current",
            "announce-token",
            &base_url,
            None,
        )
        .await
        .unwrap();

        server_handle.abort();

        let state = state.lock().unwrap();
        assert!(
            !state
                .calls
                .contains(&"GET /channels/thread-history".to_string()),
            "reset context must not probe old slot-thread history"
        );
        assert!(
            state
                .calls
                .contains(&"POST /channels/123/threads".to_string())
        );
        assert!(
            state
                .calls
                .contains(&"POST /channels/thread-created/messages".to_string())
        );

        let conn = db.lock().unwrap();
        let thread_id: Option<String> = conn
            .query_row(
                "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-current'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(thread_id.as_deref(), Some("thread-created"));
    }

    #[tokio::test]
    async fn stale_slot_thread_reset_failure_fails_closed() {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-current', 'Stale reset card', 'requested', 'agent-1', 'dispatch-current',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-other', 'Conflicting card', 'in_progress', 'agent-1', 'dispatch-other',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-current', 'card-current', 'agent-1', 'implementation', 'pending',
                    'Stale reset card', '{\"slot_index\":1}', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-other', 'card-other', 'agent-1', 'implementation', 'dispatched',
                    'Conflicting card', '{\"slot_index\":1}', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
                 VALUES ('agent-1', 1, '{\"123\":\"thread-stale\"}')",
                [],
            )
            .unwrap();
        }

        let error = send_dispatch_to_discord_inner_with_context(
            &db,
            "agent-1",
            "Stale reset card",
            "card-current",
            "dispatch-current",
            "announce-token",
            &base_url,
            None,
        )
        .await
        .expect_err("stale slot thread reset failures must fail closed");

        server_handle.abort();

        assert!(error.contains("has active dispatch"));

        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            vec!["GET /channels/thread-stale".to_string()],
            "reset failure must not continue into new-thread creation or reuse writes"
        );
    }

    /// #750: completed dispatches reach sync_dispatch_status_reaction only
    /// for non-live completion paths (api/recovery/supervisor — gated by
    /// `transition_source_is_live_command_bot` in set_dispatch_status_on_conn).
    /// For those, the announce bot's ✅ is the only terminal signal, so the
    /// sync runs the full reconcile: DELETE ⏳/❌ (@me, 404-tolerant), PUT ✅.
    #[tokio::test]
    #[ignore = "obsolete SQLite-only reaction sync fixture; PG coverage lives in sync_dispatch_status_reaction_with_pg_marks_completed_dispatch_success"]
    async fn sync_dispatch_status_reaction_writes_success_cycle_for_completed_dispatch() {
        let _env_lock = env_lock();
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let _api_base = EnvVarGuard::set("AGENTDESK_DISCORD_API_BASE_URL", &base_url);
        let temp = tempfile::tempdir().unwrap();
        write_announce_token(temp.path());
        let _root = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-1', 'Complete card', 'in_progress', 'agent-1', 'dispatch-complete',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches
                 (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
                 VALUES (
                    'dispatch-complete', 'card-1', 'agent-1', 'implementation', 'completed', 'Complete me',
                    '{\"discord_message_channel_id\":\"123\",\"discord_message_id\":\"message-123\"}',
                    datetime('now'), datetime('now')
                 )",
                [],
            )
            .unwrap();
        }

        sync_dispatch_status_reaction(&db, "dispatch-complete")
            .await
            .unwrap();

        server_handle.abort();
        let state = state.lock().unwrap();
        let reaction_calls: Vec<String> = state
            .calls
            .iter()
            .filter(|call| call.contains("/reactions/"))
            .cloned()
            .collect();
        assert_eq!(
            reaction_calls,
            vec![
                "DELETE /channels/123/messages/message-123/reactions/%E2%8F%B3/@me".to_string(),
                "DELETE /channels/123/messages/message-123/reactions/%E2%9D%8C/@me".to_string(),
                "PUT /channels/123/messages/message-123/reactions/%E2%9C%85/@me".to_string(),
            ],
            "#750: completed dispatch (non-live source) must DELETE announce-bot's own ⏳/❌ then PUT ✅"
        );
    }

    /// #750: failed dispatches get the full failure reconcile — DELETE
    /// announce-bot's own ⏳/✅ (404-tolerant) then PUT ❌. Command bot's
    /// own ✅ (if added via turn_bridge:1537) is untouched (@me-scoped
    /// deletes), but ❌ is the authoritative failure signal.
    #[tokio::test]
    #[ignore = "obsolete SQLite-only reaction sync fixture; dispatch reaction sync is PG-only after #868"]
    async fn sync_dispatch_status_reaction_writes_failure_cycle_for_failed_dispatch() {
        let _env_lock = env_lock();
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let _api_base = EnvVarGuard::set("AGENTDESK_DISCORD_API_BASE_URL", &base_url);
        let temp = tempfile::tempdir().unwrap();
        write_announce_token(temp.path());
        let _root = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-1', 'Failed card', 'in_progress', 'agent-1', 'dispatch-failed',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches
                 (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
                 VALUES (
                    'dispatch-failed', 'card-1', 'agent-1', 'implementation', 'failed', 'Fail me',
                    '{\"discord_message_channel_id\":\"123\",\"discord_message_id\":\"message-123\"}',
                    datetime('now'), datetime('now')
                 )",
                [],
            )
            .unwrap();
        }

        sync_dispatch_status_reaction(&db, "dispatch-failed")
            .await
            .unwrap();

        server_handle.abort();
        let state = state.lock().unwrap();
        let reaction_calls: Vec<String> = state
            .calls
            .iter()
            .filter(|call| call.contains("/reactions/"))
            .cloned()
            .collect();
        assert_eq!(
            reaction_calls,
            vec![
                "DELETE /channels/123/messages/message-123/reactions/%E2%8F%B3/@me".to_string(),
                "DELETE /channels/123/messages/message-123/reactions/%E2%9C%85/@me".to_string(),
                "PUT /channels/123/messages/message-123/reactions/%E2%9D%8C/@me".to_string(),
            ],
            "#750: failed dispatch must DELETE announce-bot's own ⏳/✅ then PUT ❌ (clean signal, not mixed state)"
        );
    }

    #[tokio::test]
    async fn sync_dispatch_status_reaction_with_pg_marks_completed_dispatch_success() {
        let _env_lock = env_lock();
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let _api_base = EnvVarGuard::set("AGENTDESK_DISCORD_API_BASE_URL", &base_url);
        let temp = tempfile::tempdir().unwrap();
        write_announce_token(temp.path());
        let _root = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

        let sqlite = test_db();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id)
             VALUES ($1, $2, $3)",
        )
        .bind("agent-1")
        .bind("Agent 1")
        .bind("123")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, NOW(), NOW())",
        )
        .bind("card-1")
        .bind("Complete card")
        .bind("in_progress")
        .bind("agent-1")
        .bind("dispatch-complete")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW())",
        )
        .bind("dispatch-complete")
        .bind("card-1")
        .bind("agent-1")
        .bind("implementation")
        .bind("completed")
        .bind("Complete me")
        .bind("{\"discord_message_channel_id\":\"123\",\"discord_message_id\":\"message-123\"}")
        .execute(&pool)
        .await
        .unwrap();

        sync_dispatch_status_reaction_with_pg(Some(&sqlite), Some(&pool), "dispatch-complete")
            .await
            .unwrap();

        server_handle.abort();
        let state = state.lock().unwrap();
        let reaction_calls: Vec<String> = state
            .calls
            .iter()
            .filter(|call| call.contains("/channels/123/messages/message-123/reactions/"))
            .cloned()
            .collect();
        assert_eq!(
            reaction_calls,
            vec![
                "DELETE /channels/123/messages/message-123/reactions/%E2%8F%B3/@me",
                "DELETE /channels/123/messages/message-123/reactions/%E2%9D%8C/@me",
                "PUT /channels/123/messages/message-123/reactions/%E2%9C%85/@me",
            ]
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// #1445: simulates the canonical bug — command-bot has added `⏳` at
    /// turn start, then a repair path (queue/API cancel) drives the dispatch
    /// to `failed` and announce-bot runs `apply_dispatch_status_reaction_state`.
    /// Before the fix the announce-bot's `/@me` DELETE skipped command-bot's
    /// `⏳`, leaving the message rendered as `⏳ + ❌` (in-progress vs failed
    /// ambiguity). The fix issues a 404-tolerant `/@me` DELETE on each
    /// provider's command-bot token so whichever provider owns `⏳` cleans up.
    #[tokio::test]
    async fn apply_dispatch_status_reaction_state_failed_clears_command_bot_pending() {
        let _env_lock = env_lock();
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let _api_base = EnvVarGuard::set("AGENTDESK_DISCORD_API_BASE_URL", &base_url);
        let temp = tempfile::tempdir().unwrap();
        write_announce_token(temp.path());
        write_command_bot_token(temp.path(), "claude", "claude-token");
        write_command_bot_token(temp.path(), "codex", "codex-token");
        let _root = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

        let target = DispatchMessageTarget {
            channel_id: "123".to_string(),
            message_id: "message-123".to_string(),
        };
        let token = crate::credential::read_bot_token("announce").unwrap();
        apply_dispatch_status_reaction_state(
            shared_discord_http_client(),
            &token,
            &base_url,
            &target,
            DispatchStatusReactionState::Failed,
        )
        .await
        .unwrap();

        server_handle.abort();
        let state = state.lock().unwrap();
        let reaction_calls: Vec<String> = state
            .calls
            .iter()
            .filter(|call| call.contains("/channels/123/messages/message-123/reactions/"))
            .cloned()
            .collect();
        assert_eq!(
            reaction_calls,
            vec![
                // announce-bot drops its own stale ⏳ (404-tolerant).
                "DELETE /channels/123/messages/message-123/reactions/%E2%8F%B3/@me",
                // #1445: each provider command-bot also drops its own ⏳
                // (the 404 case for whichever bot didn't own it is fine).
                "DELETE /channels/123/messages/message-123/reactions/%E2%8F%B3/@me",
                "DELETE /channels/123/messages/message-123/reactions/%E2%8F%B3/@me",
                "DELETE /channels/123/messages/message-123/reactions/%E2%9C%85/@me",
                "PUT /channels/123/messages/message-123/reactions/%E2%9D%8C/@me",
            ],
            "#1445: failed dispatch must DELETE command-bot ⏳ via each provider token before announce-bot adds ❌ — final reaction state is ❌ only, never ⏳ + ❌"
        );
        assert!(
            !reaction_calls
                .iter()
                .any(|call| call.starts_with("PUT") && call.contains("%E2%8F%B3")),
            "#1445: must never re-add ⏳ on the failure path"
        );
    }

    fn insert_review_followup_fixture(db: &crate::db::Db) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, latest_dispatch_id, channel_thread_map,
                created_at, updated_at
            ) VALUES (
                'card-review', 'Review Card', 'review', 'agent-1', 'dispatch-review',
                '{\"123\":\"thread-primary\"}', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                created_at, updated_at
            ) VALUES (
                'dispatch-review', 'card-review', 'agent-1', 'review', 'completed',
                '[Review R1] card-review', '{\"from_provider\":\"claude\"}',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    async fn insert_review_followup_fixture_pg(pool: &PgPool) {
        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id)
             VALUES ($1, $2, $3)",
        )
        .bind("agent-1")
        .bind("Agent 1")
        .bind("123")
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, latest_dispatch_id, channel_thread_map,
                created_at, updated_at
             ) VALUES (
                $1, $2, $3, $4, $5, $6::jsonb, NOW(), NOW()
             )",
        )
        .bind("card-review")
        .bind("Review Card")
        .bind("review")
        .bind("agent-1")
        .bind("dispatch-review")
        .bind(r#"{"123":"thread-primary"}"#)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                created_at, updated_at
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7,
                NOW(), NOW()
             )",
        )
        .bind("dispatch-review")
        .bind("card-review")
        .bind("agent-1")
        .bind("review")
        .bind("completed")
        .bind("[Review R1] card-review")
        .bind(r#"{"from_provider":"claude"}"#)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn review_pass_notification_unarchives_and_posts_to_thread() {
        let (base_url, state, server_handle) = spawn_mock_discord_server(true).await;
        let db = test_db();
        insert_review_followup_fixture(&db);

        send_review_result_to_primary_with_context(
            &db,
            "card-review",
            "dispatch-review",
            "pass",
            Some("announce-token"),
            &base_url,
        )
        .await
        .unwrap();

        server_handle.abort();
        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            vec![
                "GET /channels/thread-primary",
                "PATCH /channels/thread-primary",
                "POST /channels/thread-primary/messages",
            ]
        );
    }

    #[tokio::test]
    async fn review_notification_dedupes_same_dispatch_event() {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        insert_review_followup_fixture(&db);

        for _ in 0..2 {
            send_review_result_message_via_http(
                Some(&db),
                None,
                "review-1166-dedup",
                "card-review",
                123,
                "✅ [리뷰 통과] Review Card — done으로 이동",
                ReviewFollowupKind::Pass,
                "announce-token",
                &base_url,
            )
            .await
            .unwrap();
        }

        server_handle.abort();
        let state = state.lock().unwrap();
        let post_count = state
            .calls
            .iter()
            .filter(|call| call.as_str() == "POST /channels/thread-primary/messages")
            .count();
        assert_eq!(post_count, 1, "duplicate review event must not post twice");
    }

    #[tokio::test]
    async fn review_notification_truncates_over_2000_chars() {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        insert_review_followup_fixture(&db);
        let message = format!("{}{}", "✅ [리뷰 통과] ", "A".repeat(2_100));

        send_review_result_message_via_http(
            Some(&db),
            None,
            "review-1166-overflow",
            "card-review",
            123,
            &message,
            ReviewFollowupKind::Pass,
            "announce-token",
            &base_url,
        )
        .await
        .unwrap();

        server_handle.abort();
        let state = state.lock().unwrap();
        assert_eq!(state.posted_messages.len(), 1);
        let posted = &state.posted_messages[0].1;
        assert!(posted.chars().count() <= 2_000);
        assert!(posted.contains("[… truncated]"));
    }

    #[tokio::test]
    async fn review_pass_notification_uses_primary_thread_even_when_review_context_points_to_alt_channel()
     {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (
                    id, name, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
                 ) VALUES ('agent-1', 'Agent 1', 'claude', '123', '456', '123', '456')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, channel_thread_map,
                    created_at, updated_at
                ) VALUES (
                    'card-review-alt', 'Review Card', 'review', 'agent-1', 'dispatch-review-alt',
                    '{\"123\":\"thread-impl\"}', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-review-alt', 'card-review-alt', 'agent-1', 'review', 'completed',
                    '[Review R1] card-review-alt', '{\"from_provider\":\"codex\",\"target_provider\":\"claude\"}',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        }

        send_review_result_to_primary_with_context(
            &db,
            "card-review-alt",
            "dispatch-review-alt",
            "pass",
            Some("announce-token"),
            &base_url,
        )
        .await
        .unwrap();

        server_handle.abort();
        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            vec![
                "GET /channels/thread-impl",
                "POST /channels/thread-impl/messages",
            ]
        );
        assert!(
            !state
                .calls
                .contains(&"POST /channels/456/messages".to_string()),
            "review followup must not fall back to the review channel"
        );
    }

    #[tokio::test]
    async fn review_pass_notification_falls_back_to_primary_channel_when_no_implementation_thread_exists()
     {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (
                    id, name, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
                 ) VALUES ('agent-1', 'Agent 1', 'claude', '123', '456', '123', '456')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-review-fallback', 'Review Card', 'review', 'agent-1', 'dispatch-review-fallback',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-review-fallback', 'card-review-fallback', 'agent-1', 'review', 'completed',
                    '[Review R1] card-review-fallback', '{\"from_provider\":\"codex\",\"target_provider\":\"claude\"}',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        }

        send_review_result_to_primary_with_context(
            &db,
            "card-review-fallback",
            "dispatch-review-fallback",
            "pass",
            Some("announce-token"),
            &base_url,
        )
        .await
        .unwrap();

        server_handle.abort();
        let state = state.lock().unwrap();
        assert_eq!(state.calls, vec!["POST /channels/123/messages"]);
        assert!(
            !state
                .calls
                .contains(&"POST /channels/456/messages".to_string()),
            "review followup fallback must use the implementation channel"
        );
    }

    #[tokio::test]
    async fn review_pass_notification_reuses_latest_work_dispatch_thread_when_channel_map_is_missing()
     {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (
                    id, name, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
                 ) VALUES ('agent-1', 'Agent 1', 'claude', '123', '456', '123', '456')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-review-history', 'Review Card', 'review', 'agent-1', 'dispatch-review-history',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, thread_id,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-impl-history', 'card-review-history', 'agent-1', 'implementation', 'completed',
                    'Implementation', 'thread-history', datetime('now', '-1 minute'), datetime('now', '-1 minute')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-review-history', 'card-review-history', 'agent-1', 'review', 'completed',
                    '[Review R1] card-review-history', '{\"from_provider\":\"codex\",\"target_provider\":\"claude\"}',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        }

        send_review_result_to_primary_with_context(
            &db,
            "card-review-history",
            "dispatch-review-history",
            "pass",
            Some("announce-token"),
            &base_url,
        )
        .await
        .unwrap();

        server_handle.abort();
        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            vec![
                "GET /channels/thread-history",
                "POST /channels/thread-history/messages",
            ]
        );
        assert!(
            !state
                .calls
                .contains(&"POST /channels/456/messages".to_string()),
            "latest work thread must win over the review channel"
        );
    }

    #[tokio::test]
    async fn review_pass_notification_does_not_fallback_to_parent_when_unarchive_fails() {
        let (base_url, state, server_handle) =
            spawn_mock_discord_server_with_failures(true, 2).await;
        let db = test_db();
        insert_review_followup_fixture(&db);

        let err = send_review_result_to_primary_with_context(
            &db,
            "card-review",
            "dispatch-review",
            "pass",
            Some("announce-token"),
            &base_url,
        )
        .await
        .expect_err("review pass should fail closed when thread unarchive keeps failing");

        server_handle.abort();
        assert!(err.contains("failed to unarchive review followup thread thread-primary"));

        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            vec![
                "GET /channels/thread-primary",
                "PATCH /channels/thread-primary",
                "PATCH /channels/thread-primary",
            ]
        );
        assert!(
            !state
                .calls
                .contains(&"POST /channels/123/messages".to_string()),
            "main channel fallback must not happen when the mapped thread still exists"
        );
    }

    #[tokio::test]
    async fn review_pass_notification_uses_postgres_thread_map_and_channel_resolution() {
        let (base_url, state, server_handle) = spawn_mock_discord_server(true).await;
        let db = test_db();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        insert_review_followup_fixture_pg(&pool).await;

        let transport = HttpDispatchTransport {
            announce_bot_token: Some("announce-token".to_string()),
            discord_api_base: base_url.clone(),
            thread_owner_user_id: None,
            pg_pool: Some(pool.clone()),
        };
        send_review_result_to_primary_with_context_and_transport(
            Some(&db),
            "card-review",
            "dispatch-review",
            "pass",
            &transport,
        )
        .await
        .unwrap();

        server_handle.abort();
        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            vec![
                "GET /channels/thread-primary",
                "PATCH /channels/thread-primary",
                "POST /channels/thread-primary/messages",
            ]
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn send_dispatch_to_discord_with_pg_creates_thread_and_persists_context() {
        let _env_lock = env_lock();
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let _api_base = EnvVarGuard::set("AGENTDESK_DISCORD_API_BASE_URL", &base_url);
        let temp = tempfile::tempdir().unwrap();
        write_announce_token(temp.path());
        let _root = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());

        let sqlite = test_db();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id)
             VALUES ($1, $2, $3)",
        )
        .bind("agent-1")
        .bind("Agent 1")
        .bind("123")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, latest_dispatch_id, github_issue_number, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
        )
        .bind("card-1")
        .bind("PG card")
        .bind("requested")
        .bind("agent-1")
        .bind("dispatch-1")
        .bind(701_i64)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
        )
        .bind("dispatch-1")
        .bind("card-1")
        .bind("agent-1")
        .bind("implementation")
        .bind("pending")
        .bind("PG card")
        .execute(&pool)
        .await
        .unwrap();

        send_dispatch_to_discord_with_pg(
            Some(&sqlite),
            Some(&pool),
            "agent-1",
            "PG card",
            "card-1",
            "dispatch-1",
        )
        .await
        .unwrap();

        server_handle.abort();

        let state = state.lock().unwrap();
        assert!(
            state
                .calls
                .contains(&"POST /channels/123/threads".to_string()),
            "pg delivery should create a dispatch thread"
        );
        assert!(
            state
                .calls
                .contains(&"POST /channels/thread-created/messages".to_string()),
            "pg delivery should post the dispatch message into the created thread"
        );
        assert!(
            !state.calls.iter().any(|call| call.contains("/reactions/")),
            "#750: announce bot must not write dispatch-lifecycle emoji reactions, got {:?}",
            state.calls
        );

        let thread_id: Option<String> =
            sqlx::query_scalar("SELECT thread_id FROM task_dispatches WHERE id = $1")
                .bind("dispatch-1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(thread_id.as_deref(), Some("thread-created"));

        let context: Option<String> =
            sqlx::query_scalar("SELECT context FROM task_dispatches WHERE id = $1")
                .bind("dispatch-1")
                .fetch_one(&pool)
                .await
                .unwrap();
        let context = serde_json::from_str::<serde_json::Value>(&context.unwrap()).unwrap();
        assert_eq!(context["discord_message_channel_id"], "thread-created");
        assert_eq!(context["discord_message_id"], "message-thread-created");
        assert_eq!(context["slot_index"], 0);

        let channel_thread_map: Option<String> =
            sqlx::query_scalar("SELECT channel_thread_map::text FROM kanban_cards WHERE id = $1")
                .bind("card-1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&channel_thread_map.unwrap()).unwrap()["123"],
            "thread-created"
        );

        let slot_map: Option<String> = sqlx::query_scalar(
            "SELECT thread_id_map::text
             FROM auto_queue_slots
             WHERE agent_id = $1 AND slot_index = 0",
        )
        .bind("agent-1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&slot_map.unwrap()).unwrap()["123"],
            "thread-created"
        );
    }

    #[tokio::test]
    async fn review_decision_resolves_free_slot_and_skips_card_thread_candidate_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id)
             VALUES ($1, $2, $3)",
        )
        .bind("agent-1")
        .bind("Agent 1")
        .bind("123")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, latest_dispatch_id, channel_thread_map,
                created_at, updated_at
             ) VALUES (
                $1, $2, $3, $4, $5, $6::jsonb, NOW(), NOW()
             )",
        )
        .bind("card-1")
        .bind("Review decision card")
        .bind("in_progress")
        .bind("agent-1")
        .bind("dispatch-work")
        .bind(r#"{"123":"thread-work"}"#)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                created_at, updated_at
             ) VALUES
                ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW()),
                ($8, $2, $3, $9, $10, $11, $12, NOW(), NOW())",
        )
        .bind("dispatch-work")
        .bind("card-1")
        .bind("agent-1")
        .bind("implementation")
        .bind("dispatched")
        .bind("Implementation")
        .bind(r#"{"slot_index":0}"#)
        .bind("dispatch-review-decision")
        .bind("review-decision")
        .bind("pending")
        .bind("Review decision")
        .bind("{}")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ($1, 0, $2::jsonb), ($1, 1, '{}'::jsonb)",
        )
        .bind("agent-1")
        .bind(r#"{"123":"thread-work"}"#)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, agent_id, status)
             VALUES ($1, $2, $3)",
        )
        .bind("run-1")
        .bind("agent-1")
        .bind("active")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group
             ) VALUES ($1, $2, $3, $4, $5, $6, 0, 0)",
        )
        .bind("entry-work")
        .bind("run-1")
        .bind("card-1")
        .bind("agent-1")
        .bind("dispatched")
        .bind("dispatch-work")
        .execute(&pool)
        .await
        .unwrap();

        let dispatch_context = serde_json::json!({});
        let binding = resolve_slot_thread_binding_pg(
            &pool,
            "agent-1",
            "card-1",
            "dispatch-review-decision",
            Some(&dispatch_context),
            Some("review-decision"),
            123,
        )
        .await
        .unwrap()
        .expect("review-decision should claim a free slot");

        assert_eq!(binding.slot_index, 1);
        assert!(binding.thread_id.is_none());

        let candidates = collect_slot_thread_candidates_pg(
            &pool,
            "agent-1",
            "card-1",
            Some(&binding),
            123,
            false,
            true,
        )
        .await
        .unwrap();
        assert!(candidates.is_empty());
        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate == "thread-work"),
            "review-decision must not reuse the active work card thread"
        );
        let card_candidates = collect_slot_thread_candidates_pg(
            &pool,
            "agent-1",
            "card-1",
            Some(&binding),
            123,
            true,
            true,
        )
        .await
        .unwrap();
        assert!(
            card_candidates
                .iter()
                .any(|candidate| candidate == "thread-work"),
            "test fixture must prove the card-thread candidate would be reused without the independent-slot guard"
        );

        let persisted_context: String =
            sqlx::query_scalar("SELECT context FROM task_dispatches WHERE id = $1")
                .bind("dispatch-review-decision")
                .fetch_one(&pool)
                .await
                .unwrap();
        let persisted_context =
            serde_json::from_str::<serde_json::Value>(&persisted_context).unwrap();
        assert_eq!(persisted_context["slot_index"], 1);

        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW())",
        )
        .bind("dispatch-full")
        .bind("card-1")
        .bind("agent-1")
        .bind("review-decision")
        .bind("pending")
        .bind("Review decision with full pool")
        .bind("{}")
        .execute(&pool)
        .await
        .unwrap();
        for slot_index in 2..SLOT_THREAD_MAX_SLOTS {
            sqlx::query(
                "INSERT INTO auto_queue_entries (
                    id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group
                 ) VALUES ($1, $2, $3, $4, $5, $6, $7, 0)",
            )
            .bind(format!("entry-active-{slot_index}"))
            .bind("run-1")
            .bind("card-1")
            .bind("agent-1")
            .bind("dispatched")
            .bind(format!("dispatch-active-{slot_index}"))
            .bind(slot_index)
            .execute(&pool)
            .await
            .unwrap();
        }

        let err = resolve_slot_thread_binding_pg(
            &pool,
            "agent-1",
            "card-1",
            "dispatch-full",
            Some(&dispatch_context),
            Some("review-decision"),
            123,
        )
        .await
        .expect_err("review-decision should fail closed when every slot is active");
        assert!(err.contains("no free slot available"));

        pool.close().await;
        pg_db.drop().await;
    }
}
