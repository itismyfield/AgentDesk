use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use poise::serenity_prelude as serenity;

use super::*;

#[test]
fn task_response_fallback_gate_normalizes_provider_and_clears_exact_scope() {
    let session = "AgentDesk-claude-4055-normalized-gate";
    block_unanchored_task_response_fallback(" CLAUDE ", session, 4_055_801);

    assert!(unanchored_task_response_fallback_blocked(
        "claude", session, 4_055_801,
    ));
    assert!(!unanchored_task_response_fallback_blocked(
        "claude", session, 4_055_802,
    ));

    clear_unanchored_task_response_fallback("Claude", session, 4_055_801);
    assert!(!unanchored_task_response_fallback_blocked(
        "claude", session, 4_055_801,
    ));
}

#[derive(Default)]
struct FakeTransport {
    by_nonce: Mutex<HashMap<String, u64>>,
    next_message_id: AtomicU64,
    physical_posts: AtomicUsize,
    post_calls: AtomicUsize,
    edit_calls: AtomicUsize,
    fail_next_post_after_commit: AtomicBool,
    fail_edits_transiently: AtomicBool,
    report_edit_missing: AtomicBool,
}

impl FakeTransport {
    fn new() -> Self {
        Self {
            next_message_id: AtomicU64::new(9_000),
            ..Default::default()
        }
    }
}

impl TaskCardTransport for FakeTransport {
    async fn post_card(
        &self,
        _bot: &CardBot,
        _channel_id: u64,
        _content: &str,
        nonce: &str,
    ) -> Result<u64, TaskCardTransportError> {
        self.post_calls.fetch_add(1, Ordering::AcqRel);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let message_id = {
            let mut messages = self.by_nonce.lock().expect("fake nonce map");
            *messages.entry(nonce.to_string()).or_insert_with(|| {
                self.physical_posts.fetch_add(1, Ordering::AcqRel);
                self.next_message_id.fetch_add(1, Ordering::AcqRel)
            })
        };
        if self
            .fail_next_post_after_commit
            .swap(false, Ordering::AcqRel)
        {
            return Err(TaskCardTransportError::Transient(
                "ambiguous response after Discord commit".to_string(),
            ));
        }
        Ok(message_id)
    }

    async fn edit_card(
        &self,
        _bot: &CardBot,
        _channel_id: u64,
        _message_id: u64,
        _content: &str,
    ) -> Result<(), TaskCardTransportError> {
        self.edit_calls.fetch_add(1, Ordering::AcqRel);
        if self.report_edit_missing.load(Ordering::Acquire) {
            return Err(TaskCardTransportError::ConfirmedMissing(
                "404 / Discord 10008".to_string(),
            ));
        }
        if self.fail_edits_transiently.load(Ordering::Acquire) {
            return Err(TaskCardTransportError::Transient("429".to_string()));
        }
        Ok(())
    }
}

fn clients() -> CardDeliveryClients {
    CardDeliveryClients::new([CardBot::new(
        "notify",
        Arc::new(serenity::Http::new("test-token")),
    )])
}

fn event(task_id: &str) -> TaskCardEvent {
    TaskCardEvent::from_task_prompt(
        44_055,
        "claude",
        "AgentDesk-claude-4055",
        &format!(
            "<task-notification><task-id>{task_id}</task-id><tool-use-id>toolu-{task_id}</tool-use-id><status>completed</status><summary>Background command \"CI\" completed</summary><result>done</result></task-notification>"
        ),
    )
}

#[test]
fn xml_and_stream_json_share_semantic_key_and_nonce_is_bounded() {
    let event = event("same-task");
    let stream = serde_json::json!({
        "type": "system",
        "subtype": "task_notification",
        "task_id": "same-task",
        "tool_use_id": "toolu-same-task",
        "status": "completed",
        "summary": "Background command \"CI\" completed",
        "task_notification_kind": "background"
    });
    let context = TaskNotificationContext::from_stream_json(
        &stream,
        &crate::services::session_backend::StreamLineState::new(),
    )
    .expect("stream task context");
    let stream_event = context.to_event(44_055, "claude", "AgentDesk-claude-4055");
    assert_eq!(event.scope.event_key, stream_event.scope.event_key);
    let nonce = stable_nonce(&event.scope, 1);
    assert!(nonce.len() <= 25, "Discord nonce length: {nonce}");
    assert_eq!(nonce, stable_nonce(&stream_event.scope, 1));
}

#[test]
fn stream_context_recovers_tool_identity_from_task_start_state() {
    let mut state = crate::services::session_backend::StreamLineState::new();
    crate::services::session_backend::observe_stream_context(
        &serde_json::json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": "state-task",
            "tool_use_id": "toolu-from-start",
            "task_type": "local_agent"
        }),
        &mut state,
    );
    let context = TaskNotificationContext::from_stream_json(
        &serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "task_id": "state-task",
            "status": "completed",
            "summary": "Subagent finished"
        }),
        &state,
    )
    .expect("stream task context");
    assert_eq!(context.tool_use_id.as_deref(), Some("toolu-from-start"));
    assert_eq!(context.routing_kind(), TaskNotificationKind::Subagent);
}

#[test]
fn semantic_identity_separates_tasks_sessions_and_channels() {
    let first = event("identity-a");
    let different_task = event("identity-b");
    let different_channel = TaskCardEvent::from_task_prompt(
        44_056,
        "claude",
        "AgentDesk-claude-4055",
        "<task-notification><task-id>identity-a</task-id><status>completed</status><summary>done</summary></task-notification>",
    );
    let different_session = TaskCardEvent::from_task_prompt(
        44_055,
        "claude",
        "AgentDesk-claude-other",
        "<task-notification><task-id>identity-a</task-id><status>completed</status><summary>done</summary></task-notification>",
    );

    assert_ne!(first.scope.event_key, different_task.scope.event_key);
    assert_ne!(
        stable_nonce(&first.scope, 1),
        stable_nonce(&different_task.scope, 1)
    );
    assert_ne!(
        stable_nonce(&first.scope, 1),
        stable_nonce(&different_channel.scope, 1)
    );
    assert_ne!(
        stable_nonce(&first.scope, 1),
        stable_nonce(&different_session.scope, 1)
    );
}

#[test]
fn fully_unkeyed_task_event_cannot_be_deferred_to_footer() {
    let event = TaskCardEvent::from_task_prompt(
        44_055,
        "claude",
        "AgentDesk-claude-4055",
        "<task-notification><status>completed</status><summary>unkeyed work</summary></task-notification>",
    );
    assert!(!event.supports_footer_deferral());
    assert!(event.scope.event_key.starts_with("payload:"));

    let stream = TaskNotificationContext::from_stream_json(
        &serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "status": "completed",
            "summary": "unkeyed work",
            "task_notification_kind": "subagent"
        }),
        &crate::services::session_backend::StreamLineState::new(),
    )
    .expect("unkeyed stream context")
    .to_event(44_055, "claude", "AgentDesk-claude-4055");
    assert_eq!(event.scope.event_key, stream.scope.event_key);
    assert_eq!(
        stable_nonce(&event.scope, 1),
        stable_nonce(&stream.scope, 1)
    );
}

#[test]
fn subagent_agent_path_is_hashed_and_never_enters_identity() {
    let raw = r#"<subagent_notification>{"agent_path":"/private/secret/agent-42","status":{"completed":"done"}}</subagent_notification>"#;
    let event = TaskCardEvent::from_subagent_prompt(1, "codex", "session", raw);
    assert!(!event.scope.event_key.contains("/private/secret"));
    assert!(!event.payload.render(1).contains("/private/secret"));
}

#[test]
fn malformed_subagent_still_has_safe_durable_identity_and_card() {
    let secret = "/private/secret/malformed-agent";
    let raw = format!(
        r#"<subagent_notification>{{"agent_path":"{secret}",broken}}</subagent_notification>"#
    );
    let event = TaskCardEvent::from_subagent_prompt(1, "codex", "session", &raw);
    assert!(!event.scope.event_key.contains(secret));
    assert!(!event.payload.render(1).contains(secret));
    assert!(
        event
            .payload
            .render(1)
            .contains("malformed payload omitted")
    );
}

#[tokio::test]
async fn concurrent_ensure_posts_once_and_returns_same_card() {
    let transport = Arc::new(FakeTransport::new());
    let clients = clients();
    let event = event("concurrent");
    let (left, right) = tokio::join!(
        ensure_card(
            None,
            &clients,
            transport.as_ref(),
            &event,
            EnsureIntent::Observation,
        ),
        ensure_card(
            None,
            &clients,
            transport.as_ref(),
            &event,
            EnsureIntent::Observation,
        ),
    );
    let left = left.expect("left ensure");
    let right = right.expect("right ensure");
    assert_eq!(left.message_id, right.message_id);
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn ambiguous_post_retries_same_nonce_without_second_message() {
    let transport = FakeTransport::new();
    transport
        .fail_next_post_after_commit
        .store(true, Ordering::Release);
    let clients = clients();
    let event = event("ambiguous");
    assert!(
        ensure_card(
            None,
            &clients,
            &transport,
            &event,
            EnsureIntent::Observation,
        )
        .await
        .is_err()
    );
    let recovered = ensure_card(None, &clients, &transport, &event, EnsureIntent::Promotion)
        .await
        .expect("same-nonce recovery");
    assert!(recovered.message_id > 0);
    assert_eq!(transport.post_calls.load(Ordering::Acquire), 2);
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn transient_edit_never_falls_back_to_fresh_post() {
    let transport = FakeTransport::new();
    let clients = clients();
    let event = event("edit-transient");
    ensure_card(
        None,
        &clients,
        &transport,
        &event,
        EnsureIntent::Observation,
    )
    .await
    .expect("first card");
    transport
        .fail_edits_transiently
        .store(true, Ordering::Release);
    assert!(
        ensure_card(
            None,
            &clients,
            &transport,
            &event,
            EnsureIntent::Observation,
        )
        .await
        .is_err()
    );
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 1);
    transport
        .fail_edits_transiently
        .store(false, Ordering::Release);
    ensure_card(
        None,
        &clients,
        &transport,
        &event,
        EnsureIntent::Observation,
    )
    .await
    .expect("later edit retry");
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn confirmed_missing_edit_uses_revision_nonce_for_one_replacement() {
    let transport = FakeTransport::new();
    let clients = clients();
    let event = event("confirmed-missing");
    let original = ensure_card(
        None,
        &clients,
        &transport,
        &event,
        EnsureIntent::Observation,
    )
    .await
    .expect("first card");
    transport.report_edit_missing.store(true, Ordering::Release);
    let replacement = ensure_card(
        None,
        &clients,
        &transport,
        &event,
        EnsureIntent::Observation,
    )
    .await
    .expect("confirmed-missing replacement");
    assert_eq!(replacement.disposition, CardDisposition::Replaced);
    assert_ne!(replacement.message_id, original.message_id);
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn footer_only_observation_posts_nothing_until_response_promotion() {
    let transport = FakeTransport::new();
    let clients = clients();
    let event = event("footer-only");
    record_footer_only(None, &event)
        .await
        .expect("persist footer-only authority");
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 0);

    ensure_card(None, &clients, &transport, &event, EnsureIntent::Promotion)
        .await
        .expect("response promotes footer state to a card");
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn unavailable_pinned_bot_releases_lease_for_immediate_retry() {
    let transport = FakeTransport::new();
    let notify_clients = clients();
    let event = event("pinned-bot-retry");
    ensure_card(
        None,
        &notify_clients,
        &transport,
        &event,
        EnsureIntent::Observation,
    )
    .await
    .expect("first card");

    let provider_only = CardDeliveryClients::new([CardBot::new(
        "provider:claude",
        Arc::new(serenity::Http::new("provider-test-token")),
    )]);
    assert!(
        ensure_card(
            None,
            &provider_only,
            &transport,
            &event,
            EnsureIntent::Observation,
        )
        .await
        .is_err()
    );
    ensure_card(
        None,
        &notify_clients,
        &transport,
        &event,
        EnsureIntent::Observation,
    )
    .await
    .expect("pinned bot can retry without waiting for stale lease expiry");
}

#[tokio::test]
async fn promotion_waits_while_an_edit_owns_the_card_lease() {
    let transport = FakeTransport::new();
    let clients = clients();
    let event = event("promotion-edit-race");
    ensure_card(
        None,
        &clients,
        &transport,
        &event,
        EnsureIntent::Observation,
    )
    .await
    .expect("first card");
    let seed = event.payload.render(2);
    let edit_claim = store::claim_card(
        None,
        &event.scope,
        "notify",
        &seed,
        &content_hash(&seed),
        store::StoreIntent::Observation,
    )
    .await
    .expect("edit claim");
    let CardClaim::Owned(claimed) = edit_claim else {
        panic!("repeat observation must own the edit lease");
    };
    let promotion = store::claim_card(
        None,
        &event.scope,
        "notify",
        &seed,
        &content_hash(&seed),
        store::StoreIntent::Promotion,
    )
    .await
    .expect("promotion claim decision");
    assert!(matches!(promotion, CardClaim::Busy { .. }));
    let store::ClaimAction::Edit { message_id } = claimed.action else {
        panic!("repeat observation must edit the existing card");
    };
    store::mark_edit_failure(None, &claimed, message_id, "test release")
        .await
        .expect("release test edit lease");
}

#[tokio::test]
async fn concurrent_ensure_card_unique_winner_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_card_4055",
        "task notification card concurrency",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let transport = Arc::new(FakeTransport::new());
    let clients = clients();
    let event = event("postgres-concurrent");
    let (left, right) = tokio::join!(
        ensure_card(
            Some(&pool),
            &clients,
            transport.as_ref(),
            &event,
            EnsureIntent::Observation,
        ),
        ensure_card(
            Some(&pool),
            &clients,
            transport.as_ref(),
            &event,
            EnsureIntent::Observation,
        ),
    );
    assert_eq!(
        left.expect("left").message_id,
        right.expect("right").message_id
    );
    assert_eq!(
        transport.post_calls.load(Ordering::Acquire),
        1,
        "PG uniqueness must elect one Discord create-attempt owner"
    );
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 1);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_notification_card_state")
        .fetch_one(&pool)
        .await
        .expect("task card row count");
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn bigint_update_count_above_u32_max_remains_claimable_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_card_bigint_4055",
        "task notification card bigint update count",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let transport = FakeTransport::new();
    let clients = clients();
    let event = event("postgres-bigint-update-count");
    ensure_card(
        Some(&pool),
        &clients,
        &transport,
        &event,
        EnsureIntent::Observation,
    )
    .await
    .expect("first card");

    let u32_boundary = i64::from(u32::MAX);
    sqlx::query(
        "UPDATE task_notification_card_state
         SET update_count = $5, lease_owner = NULL, lease_expires_at = NULL
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4",
    )
    .bind(i64::try_from(event.scope.channel_id).expect("test channel id"))
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .bind(&event.scope.event_key)
    .bind(u32_boundary)
    .execute(&pool)
    .await
    .expect("seed BIGINT update count at the u32 boundary");

    let edited = ensure_card(
        Some(&pool),
        &clients,
        &transport,
        &event,
        EnsureIntent::Observation,
    )
    .await
    .expect("BIGINT count above u32::MAX remains claimable");
    assert_eq!(edited.disposition, CardDisposition::Edited);
    assert_eq!(transport.edit_calls.load(Ordering::Acquire), 1);

    let stored_count: i64 = sqlx::query_scalar(
        "SELECT update_count FROM task_notification_card_state
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4",
    )
    .bind(i64::try_from(event.scope.channel_id).expect("test channel id"))
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .bind(&event.scope.event_key)
    .fetch_one(&pool)
    .await
    .expect("load BIGINT update count after edit");
    assert_eq!(stored_count, u32_boundary + 1);
}

#[tokio::test]
async fn restart_after_discord_post_before_db_commit_recovers_by_nonce_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_card_restart_4055",
        "task notification card restart nonce recovery",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let transport = FakeTransport::new();
    let clients = clients();
    let event = event("postgres-restart");
    let seed = event.payload.render(1);
    let claim = store::claim_card(
        Some(&pool),
        &event.scope,
        "notify",
        &seed,
        &content_hash(&seed),
        store::StoreIntent::Observation,
    )
    .await
    .expect("claim before simulated crash");
    let CardClaim::Owned(claimed) = claim else {
        panic!("first worker must own the post lease");
    };
    let bot = clients.by_key("notify").expect("notify test bot");
    let original_id = transport
        .post_card(bot, event.scope.channel_id, &seed, &claimed.discord_nonce)
        .await
        .expect("Discord accepted create before worker crash");
    sqlx::query(
        "UPDATE task_notification_card_state
         SET lease_expires_at = NOW() - INTERVAL '1 second'
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4",
    )
    .bind(i64::try_from(event.scope.channel_id).expect("test channel id"))
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .bind(&event.scope.event_key)
    .execute(&pool)
    .await
    .expect("expire crashed worker lease");

    let recovered = ensure_card(
        Some(&pool),
        &clients,
        &transport,
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("stale lease replay reconciles the same nonce");
    assert_eq!(recovered.message_id, original_id);
    assert_eq!(transport.post_calls.load(Ordering::Acquire), 2);
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 1);
}
