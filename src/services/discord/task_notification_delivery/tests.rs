use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use poise::serenity_prelude as serenity;

use super::*;

struct TestRuntimeRootGuard {
    previous: Option<std::ffi::OsString>,
}

impl TestRuntimeRootGuard {
    fn set(path: &std::path::Path) -> Self {
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
        Self { previous }
    }
}

impl Drop for TestRuntimeRootGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}

#[test]
fn response_turn_key_is_stable_and_separates_offsets() {
    let first = response_turn_key(4055, "2026-07-11T01:37:00Z", Some(10));
    assert_eq!(first.len(), 64);
    assert_eq!(
        first,
        response_turn_key(4055, "2026-07-11T01:37:00Z", Some(10))
    );
    assert_ne!(
        first,
        response_turn_key(4055, "2026-07-11T01:37:00Z", Some(11))
    );
}

#[test]
fn response_chunk_nonce_is_stable_bounded_and_distinct() {
    let turn = response_turn_key(4055, "2026-07-11T01:37:00Z", Some(10));
    let first = response_chunk_nonce(&turn, 0);
    assert_eq!(first, response_chunk_nonce(&turn, 0));
    assert_ne!(first, response_chunk_nonce(&turn, 1));
    assert_ne!(
        first,
        response_chunk_nonce(
            &response_turn_key(4055, "2026-07-11T01:37:00Z", Some(11)),
            0,
        )
    );
    assert!(first.starts_with("adktr"));
    assert!(first.len() <= 25, "Discord nonce length: {first}");
}

#[test]
fn durable_response_turn_key_uses_shared_recovery_identity_for_degenerate_turns() {
    let recovered = fallback_response_turn_key(
        4_055_902,
        "claude",
        "AgentDesk-claude-4055-recovered",
        20,
        "done",
    );
    assert_eq!(
        durable_response_turn_key(
            4_055_902,
            "CLAUDE",
            "AgentDesk-claude-4055-recovered",
            0,
            "",
            None,
            20,
            "done",
        ),
        recovered,
        "sink and watcher must converge when neither retains a provider turn identity"
    );
    assert_ne!(
        recovered,
        durable_response_turn_key(
            4_055_902,
            "claude",
            "AgentDesk-claude-4055-recovered",
            0,
            "",
            None,
            20,
            "different response",
        ),
        "response content separates same-offset recovered turns"
    );
    assert_eq!(
        durable_response_turn_key(
            4_055_902,
            "claude",
            "AgentDesk-claude-4055-recovered",
            0,
            "2026-07-11T01:37:00Z",
            Some(20),
            30,
            "done",
        ),
        response_turn_key(0, "2026-07-11T01:37:00Z", Some(20)),
        "a zero user id is still durable when timestamp and start offset are present"
    );
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
fn subagent_agent_path_is_ignored_and_never_enters_identity() {
    let raw = r#"<subagent_notification>{"agent_path":"/private/secret/agent-42","status":{"completed":"done"}}</subagent_notification>"#;
    let event = TaskCardEvent::from_subagent_prompt(1, "codex", "session", raw);
    assert!(!event.scope.event_key.contains("/private/secret"));
    assert!(!event.payload.render(1).contains("/private/secret"));
}

#[test]
fn identity_less_subagent_prompt_and_stream_share_one_semantic_event() {
    let prompt = r#"<subagent_notification>{"agent_path":"/private/secret/agent-42","status":{"completed":"done"}}</subagent_notification>"#;
    let prompt_event = TaskCardEvent::from_subagent_prompt(1, "codex", "session", prompt);
    let stream_event = TaskNotificationContext::from_stream_json(
        &serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "status": "completed",
            "summary": "done",
            "task_notification_kind": "subagent"
        }),
        &crate::services::session_backend::StreamLineState::new(),
    )
    .expect("identity-less stream context")
    .to_event(1, "codex", "session");

    assert_eq!(
        prompt_event.scope.event_key, stream_event.scope.event_key,
        "prompt observation and stream delivery must converge on one card",
    );
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
async fn durable_response_fence_is_exact_and_survives_stale_ownership_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_response_fence_4055",
        "task response durable fallback fence",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let transport = FakeTransport::new();
    let clients = clients();
    let delivered_event = event("postgres-response-delivered");
    let unrelated_pending_event = event("postgres-unrelated-pending");
    let unrelated_card = ensure_card(
        Some(&pool),
        &clients,
        &transport,
        &unrelated_pending_event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("confirm unrelated response card");
    let unrelated_turn = response_turn_key(4054, "2026-07-11T01:36:00Z", Some(4054));
    let unrelated_claim = claim_task_response_delivery(
        Some(&pool),
        unrelated_pending_event.scope.channel_id,
        &unrelated_pending_event.scope.provider,
        &unrelated_pending_event.scope.session_key,
        unrelated_pending_event.event_key(),
        &unrelated_turn,
        unrelated_card.message_id,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("claim unrelated response");
    assert!(matches!(
        unrelated_claim,
        ResponseDeliveryClaimOutcome::Owned(_)
    ));
    assert!(matches!(
        claim_task_response_delivery(
            Some(&pool),
            unrelated_pending_event.scope.channel_id,
            &unrelated_pending_event.scope.provider,
            &unrelated_pending_event.scope.session_key,
            unrelated_pending_event.event_key(),
            &unrelated_turn,
            unrelated_card.message_id,
            ResponseDeliveryOwner::Sink,
        )
        .await
        .expect("load unrelated pending response"),
        ResponseDeliveryClaimOutcome::Wait
    ));

    let confirmed = ensure_card(
        Some(&pool),
        &clients,
        &transport,
        &delivered_event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("confirm response card");
    let turn_key = response_turn_key(4055, "2026-07-11T01:37:00Z", Some(4055));
    let sink_claim = claim_task_response_delivery(
        Some(&pool),
        delivered_event.scope.channel_id,
        &delivered_event.scope.provider,
        &delivered_event.scope.session_key,
        delivered_event.event_key(),
        &turn_key,
        confirmed.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("claim exact response turn");
    let ResponseDeliveryClaimOutcome::Owned(sink_claim) = sink_claim else {
        panic!("first exact claimant must own the response")
    };
    let (pending, persisted_card) = claim_existing_task_response_delivery(
        Some(&pool),
        delivered_event.scope.channel_id,
        &delivered_event.scope.provider,
        &delivered_event.scope.session_key,
        &turn_key,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("resume confirmed response without provider context")
    .expect("durable response row");
    assert_eq!(persisted_card, confirmed.message_id);
    assert!(matches!(pending, ResponseDeliveryClaimOutcome::Wait));

    sqlx::query(
        "UPDATE task_notification_response_delivery
         SET lease_expires_at = NOW() - INTERVAL '1 second'
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3
           AND event_key = $4 AND response_turn_key = $5",
    )
    .bind(i64::try_from(delivered_event.scope.channel_id).expect("test channel id"))
    .bind(&delivered_event.scope.provider)
    .bind(&delivered_event.scope.session_key)
    .bind(delivered_event.event_key())
    .bind(&turn_key)
    .execute(&pool)
    .await
    .expect("expire the sink response claim");
    let (watcher_claim, resumed_card) = claim_existing_task_response_delivery(
        Some(&pool),
        delivered_event.scope.channel_id,
        &delivered_event.scope.provider,
        &delivered_event.scope.session_key,
        &turn_key,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("take over response without provider context")
    .expect("durable response row");
    assert_eq!(resumed_card, confirmed.message_id);
    let ResponseDeliveryClaimOutcome::Owned(watcher_claim) = watcher_claim else {
        panic!("watcher must own the expired response claim")
    };
    assert!(
        mark_task_response_delivered(Some(&pool), &sink_claim)
            .await
            .is_err(),
        "the stale sink token must not commit the watcher-owned response"
    );
    mark_task_response_delivered(Some(&pool), &watcher_claim)
        .await
        .expect("commit exact watcher response delivery");

    assert!(matches!(
        claim_task_response_delivery(
            Some(&pool),
            delivered_event.scope.channel_id,
            &delivered_event.scope.provider,
            &delivered_event.scope.session_key,
            delivered_event.event_key(),
            &turn_key,
            confirmed.message_id,
            ResponseDeliveryOwner::Watcher,
        )
        .await
        .expect("delivered event fence"),
        ResponseDeliveryClaimOutcome::Delivered { .. }
    ));
    assert!(matches!(
        claim_task_response_delivery(
            Some(&pool),
            unrelated_pending_event.scope.channel_id,
            &unrelated_pending_event.scope.provider,
            &unrelated_pending_event.scope.session_key,
            unrelated_pending_event.event_key(),
            &unrelated_turn,
            unrelated_card.message_id,
            ResponseDeliveryOwner::Sink,
        )
        .await
        .expect("unrelated pending event remains fenced"),
        ResponseDeliveryClaimOutcome::Wait
    ));
}

#[tokio::test]
async fn sent_response_claim_never_reopens_post_authority_after_lease_expiry_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_response_sent_4055",
        "sent but uncommitted task response fence",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let event = event("postgres-response-sent-uncommitted");
    let turn_key = response_turn_key(4057, "2026-07-11T03:37:00Z", Some(4057));
    let claim = claim_task_response_delivery(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        90_057,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("claim response before Discord accepts the POST");
    assert!(matches!(claim, ResponseDeliveryClaimOutcome::Owned(_)));

    // Model the boundary under review: Discord accepted the required-reference
    // POST, but the final `delivered` CAS did not commit. This durable state must
    // remain a no-POST tombstone even after the old lease expires.
    sqlx::query(
        "UPDATE task_notification_response_delivery
         SET delivery_state = 'sent', sent_at = NOW(),
             lease_expires_at = NOW() - INTERVAL '1 second'
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3
           AND event_key = $4 AND response_turn_key = $5",
    )
    .bind(i64::try_from(event.scope.channel_id).expect("test channel id"))
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .bind(event.event_key())
    .bind(&turn_key)
    .execute(&pool)
    .await
    .expect("persist sent-but-uncommitted response state");

    let second_owner = claim_task_response_delivery(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        90_057,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("inspect sent response after its former lease expires");
    assert!(matches!(
        second_owner,
        ResponseDeliveryClaimOutcome::SentUncommitted {
            card_message_id: 90_057
        }
    ));
}

#[test]
fn response_reply_nonce_reconciles_after_sent_cas_failure_and_lease_takeover_pg() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let tempdir = tempfile::tempdir().expect("temp runtime root");
    let _root = TestRuntimeRootGuard::set(tempdir.path());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    runtime.block_on(async {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_task_response_nonce_takeover_4055",
            "task response nonce takeover reconciliation",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let event = event("postgres-response-reply-nonce-takeover");
        let turn_key = response_turn_key(4062, "2026-07-11T03:42:00Z", Some(4062));
        let card_message_id = 90_062;
        let claim = claim_task_response_delivery(
            Some(&pool),
            event.scope.channel_id,
            &event.scope.provider,
            &event.scope.session_key,
            event.event_key(),
            &turn_key,
            card_message_id,
            ResponseDeliveryOwner::Sink,
        )
        .await
        .expect("sink claims response before reply POST");
        let ResponseDeliveryClaimOutcome::Owned(claim) = claim else {
            panic!("first response claimant must own the reply POST")
        };

        let channel = serenity::ChannelId::new(event.scope.channel_id);
        let card = serenity::MessageId::new(card_message_id);
        let by_nonce = Arc::new(Mutex::new(HashMap::<String, u64>::new()));
        let physical_posts = Arc::new(AtomicUsize::new(0));
        let next_message_id = Arc::new(AtomicU64::new(100_000));
        let observations = Arc::new(Mutex::new(Vec::<(
            Option<String>,
            bool,
            Option<(serenity::ChannelId, serenity::MessageId)>,
        )>::new()));
        let hook_by_nonce = Arc::clone(&by_nonce);
        let hook_physical_posts = Arc::clone(&physical_posts);
        let hook_next_message_id = Arc::clone(&next_message_id);
        let hook_observations = Arc::clone(&observations);
        let _hook =
            crate::services::discord::formatting::rollback_transport_test_hook::install(
                Box::new(move |seen_channel, _content, reference, nonce, enforce_nonce| {
                    if seen_channel != channel {
                        return None;
                    }
                    hook_observations
                        .lock()
                        .expect("reply observations")
                        .push((nonce.map(str::to_string), enforce_nonce, reference));
                    let message_id = match nonce {
                        Some(nonce) if enforce_nonce => {
                            let mut messages = hook_by_nonce.lock().expect("reply nonce map");
                            *messages.entry(nonce.to_string()).or_insert_with(|| {
                                hook_physical_posts.fetch_add(1, Ordering::AcqRel);
                                hook_next_message_id.fetch_add(1, Ordering::AcqRel)
                            })
                        }
                        _ => {
                            hook_physical_posts.fetch_add(1, Ordering::AcqRel);
                            hook_next_message_id.fetch_add(1, Ordering::AcqRel)
                        }
                    };
                    Some(Ok(serenity::MessageId::new(message_id)))
                }),
                Box::new(|_, _| Some(Ok(()))),
            );
        let http = serenity::Http::new("test-token");
        let shared = crate::services::discord::make_shared_data_for_tests();
        let reply_text = "x".repeat(crate::services::discord::DISCORD_MSG_LIMIT + 32);
        let first = crate::services::discord::formatting::long_send_rollback::send_long_message_raw_with_required_reference_rollback(
            &http,
            channel,
            card,
            &reply_text,
            &shared,
            (channel, card),
            &turn_key,
        )
        .await
        .expect("Discord accepts the first required-reference reply POST");
        assert_eq!(first.len(), 2, "test reply must include a continuation");

        let mut lost_sent_cas = claim.clone();
        lost_sent_cas.owner_token = "lost-owner-token".to_string();
        record_task_response_sent_bounded(Some(&pool), &lost_sent_cas)
            .await
            .expect_err("model POST success followed by sent-state CAS failure");
        let expired = sqlx::query(
            "UPDATE task_notification_response_delivery
             SET lease_expires_at = NOW() - INTERVAL '1 second'
             WHERE channel_id = $1 AND provider = $2 AND session_key = $3
               AND event_key = $4 AND response_turn_key = $5
               AND delivery_state = 'claimed'",
        )
        .bind(i64::try_from(event.scope.channel_id).expect("test channel id"))
        .bind(&event.scope.provider)
        .bind(&event.scope.session_key)
        .bind(event.event_key())
        .bind(&turn_key)
        .execute(&pool)
        .await
        .expect("expire the sink lease after its sent-state CAS failure");
        assert_eq!(expired.rows_affected(), 1);

        let (takeover, persisted_card) = claim_existing_task_response_delivery(
            Some(&pool),
            event.scope.channel_id,
            &event.scope.provider,
            &event.scope.session_key,
            &turn_key,
            ResponseDeliveryOwner::Watcher,
        )
        .await
        .expect("watcher inspects expired sink claim")
        .expect("response row survives the failed sent CAS");
        assert_eq!(persisted_card, card_message_id);
        let ResponseDeliveryClaimOutcome::Owned(takeover) = takeover else {
            panic!("watcher must take over the expired claimed reply")
        };
        let replay = crate::services::discord::formatting::long_send_rollback::send_long_message_raw_with_required_reference_rollback(
            &http,
            channel,
            card,
            &reply_text,
            &shared,
            (channel, card),
            &turn_key,
        )
        .await
        .expect("watcher reconciles the first reply by its enforced nonce");
        assert_eq!(replay, first, "takeover must reconcile the same message id");
        assert_eq!(
            physical_posts.load(Ordering::Acquire),
            first.len(),
            "POST-Ok followed by sent-CAS failure must not duplicate reply chunks"
        );
        let observed = observations.lock().expect("reply observations");
        assert_eq!(observed.len(), 4);
        assert_ne!(observed[0].0, observed[1].0, "chunks need distinct nonces");
        assert_eq!(observed[0].0, observed[2].0);
        assert_eq!(observed[1].0, observed[3].0);
        assert!(observed.iter().all(|(_, enforced, _)| *enforced));
        assert_eq!(observed[0].2, Some((channel, card)));
        assert_eq!(observed[1].2, None);
        assert_eq!(observed[2].2, Some((channel, card)));
        assert_eq!(observed[3].2, None);
        drop(observed);

        record_task_response_sent_bounded(Some(&pool), &takeover)
            .await
            .expect("takeover records the reconciled reply as sent");
        assert!(matches!(
            claim_task_response_delivery(
                Some(&pool),
                event.scope.channel_id,
                &event.scope.provider,
                &event.scope.session_key,
                event.event_key(),
                &turn_key,
                card_message_id,
                ResponseDeliveryOwner::Sink,
            )
            .await
            .expect("inspect reconciled sent response"),
            ResponseDeliveryClaimOutcome::SentUncommitted {
                card_message_id: 90_062
            }
        ));
    });
}

#[tokio::test]
async fn response_card_rebind_requires_exact_owner_token_and_old_card_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_response_rebind_4055",
        "exact task response card rebind",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let event = event("postgres-response-card-rebind");
    let turn_key = response_turn_key(4058, "2026-07-11T03:38:00Z", Some(4058));
    let claim = claim_task_response_delivery(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        90_058,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("claim response bound to deleted card C1");
    let ResponseDeliveryClaimOutcome::Owned(claim) = claim else {
        panic!("first response claimant must own C1")
    };

    let mut stale_token = claim.clone();
    stale_token.owner_token = "stale-owner-token".to_string();
    assert!(
        rebind_task_response_card(Some(&pool), &stale_token, 90_059)
            .await
            .is_err(),
        "a stale owner token cannot rebind C1 to C2"
    );
    let rebound = rebind_task_response_card(Some(&pool), &claim, 90_059)
        .await
        .expect("exact C1 owner rebinds to replacement C2");
    assert_eq!(rebound.card_message_id, 90_059);
    assert!(
        rebind_task_response_card(Some(&pool), &claim, 90_060)
            .await
            .is_err(),
        "the old C1 binding cannot be reused after the exact CAS"
    );
    assert!(
        mark_task_response_sent(Some(&pool), &claim).await.is_err(),
        "the old C1-bound claim cannot record a send"
    );
    mark_task_response_sent(Some(&pool), &rebound)
        .await
        .expect("only the C2-bound claim records the required-reference POST");
    assert!(matches!(
        claim_task_response_delivery(
            Some(&pool),
            event.scope.channel_id,
            &event.scope.provider,
            &event.scope.session_key,
            event.event_key(),
            &turn_key,
            90_059,
            ResponseDeliveryOwner::Watcher,
        )
        .await
        .expect("inspect rebound response"),
        ResponseDeliveryClaimOutcome::SentUncommitted {
            card_message_id: 90_059
        }
    ));
}

#[tokio::test]
async fn missing_required_reference_replaces_once_and_exactly_rebinds_response() {
    let transport = FakeTransport::new();
    let clients = clients();
    let event = event("missing-required-reference-rebind");
    let first_card = ensure_card(None, &clients, &transport, &event, EnsureIntent::Promotion)
        .await
        .expect("create original task card");
    let turn_key = response_turn_key(4060, "2026-07-11T03:40:00Z", Some(4060));
    let claim = claim_task_response_delivery(
        None,
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        first_card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("claim response bound to original card");
    let ResponseDeliveryClaimOutcome::Owned(claim) = claim else {
        panic!("first response claimant must own the send")
    };

    let replacement =
        replace_confirmed_missing_card(None, &clients, &transport, &event, first_card.message_id)
            .await
            .expect("replace Discord-confirmed missing card");
    assert_ne!(replacement.message_id, first_card.message_id);
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 2);

    let mut stale_token = claim.clone();
    stale_token.owner_token = "not-the-owner".to_string();
    assert!(
        rebind_task_response_card(None, &stale_token, replacement.message_id)
            .await
            .is_err(),
        "card rebind must require the exact response owner token"
    );
    let rebound = rebind_task_response_card(None, &claim, replacement.message_id)
        .await
        .expect("exact owner rebinds response to replacement card");
    assert_eq!(rebound.card_message_id, replacement.message_id);
    assert!(
        mark_task_response_sent(None, &claim).await.is_err(),
        "the stale C1-bound claim cannot commit after rebind"
    );
    mark_task_response_sent(None, &rebound)
        .await
        .expect("record the C2-bound required-reference send");

    let retry = claim_task_response_delivery(
        None,
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        replacement.message_id,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("inspect sent response after replacement");
    assert!(matches!(
        retry,
        ResponseDeliveryClaimOutcome::SentUncommitted {
            card_message_id
        } if card_message_id == replacement.message_id
    ));

    let converged =
        replace_confirmed_missing_card(None, &clients, &transport, &event, first_card.message_id)
            .await
            .expect("concurrent stale repair converges on installed replacement");
    assert_eq!(converged.message_id, replacement.message_id);
    assert_eq!(
        transport.physical_posts.load(Ordering::Acquire),
        2,
        "a stale repair must not POST a second replacement"
    );
}

#[tokio::test]
async fn successful_send_with_failed_final_cas_surfaces_sent_but_uncommitted() {
    let event = event("sent-final-cas-failure");
    let turn_key = response_turn_key(4061, "2026-07-11T03:41:00Z", Some(4061));
    let claim = claim_task_response_delivery(
        None,
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        90_061,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("claim response before successful Discord POST");
    let ResponseDeliveryClaimOutcome::Owned(claim) = claim else {
        panic!("first response claimant must own the send")
    };
    mark_task_response_sent(None, &claim)
        .await
        .expect("Discord POST success is durably recorded first");
    force_task_response_delivered_failures(&claim, 3);

    let heartbeat = task_response_delivery_heartbeat(None, Some(&claim));
    let outcome = commit_task_response_delivered_bounded(None, &claim).await;
    heartbeat.stop();
    assert!(matches!(
        outcome,
        TaskResponseCommitOutcome::SentButUncommitted { .. }
    ));
    assert!(matches!(
        claim_task_response_delivery(
            None,
            event.scope.channel_id,
            &event.scope.provider,
            &event.scope.session_key,
            event.event_key(),
            &turn_key,
            90_061,
            ResponseDeliveryOwner::Watcher,
        )
        .await
        .expect("second owner inspects sent response"),
        ResponseDeliveryClaimOutcome::SentUncommitted { .. }
    ));

    assert_eq!(
        commit_task_response_delivered_bounded(None, &claim).await,
        TaskResponseCommitOutcome::Delivered,
        "the same exact owner can reconcile the final CAS later without another POST"
    );
}

#[tokio::test]
async fn missing_card_replacement_replays_same_nonce_after_post_commit_ambiguity() {
    let transport = FakeTransport::new();
    let clients = clients();
    let event = event("missing-card-replacement-restart");
    let first_card = ensure_card(None, &clients, &transport, &event, EnsureIntent::Promotion)
        .await
        .expect("create original task card");
    let claim = store::claim_missing_card_replacement(None, &event.scope, first_card.message_id)
        .await
        .expect("claim missing-card replacement");
    let store::MissingCardReplacementClaim::Owned(claim) = claim else {
        panic!("first replacement worker must own the revision")
    };
    let bot = clients.by_key(&claim.bot_key).expect("pinned card bot");
    let discord_replacement_id = transport
        .post_card(
            bot,
            event.scope.channel_id,
            &claim.rendered_content,
            &claim.discord_nonce,
        )
        .await
        .expect("Discord accepts replacement before DB ambiguity");
    store::mark_post_failure(None, &claim, "ambiguous DB response after Discord commit")
        .await
        .expect("release failed worker while retaining posting nonce");

    let recovered =
        replace_confirmed_missing_card(None, &clients, &transport, &event, first_card.message_id)
            .await
            .expect("replacement retry resumes the same revision nonce");
    assert_eq!(recovered.message_id, discord_replacement_id);
    assert_eq!(transport.post_calls.load(Ordering::Acquire), 3);
    assert_eq!(
        transport.physical_posts.load(Ordering::Acquire),
        2,
        "original card plus one physical replacement; retry must reuse the replacement nonce"
    );
}

#[tokio::test]
async fn unclaimed_response_turn_does_not_block_watcher_owned_delivery_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_response_unclaimed_4055",
        "unclaimed task response watcher handoff",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let turn_key = response_turn_key(4055, "2026-07-11T02:37:00Z", Some(8055));

    let claim = claim_task_response_delivery(
        Some(&pool),
        44_055,
        "claude",
        "AgentDesk-claude-4055-unclaimed",
        "task:missing",
        &turn_key,
        90_055,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("claim unbound response turn");
    assert!(
        matches!(claim, ResponseDeliveryClaimOutcome::Owned(_)),
        "a missing row must hand delivery authority to the watcher"
    );
}

#[tokio::test]
async fn concurrent_response_claims_elect_one_physical_delivery_owner_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_response_concurrent_4055",
        "concurrent task response ownership",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let transport = FakeTransport::new();
    let event = event("concurrent-response-claim");
    let card = ensure_card(
        Some(&pool),
        &clients(),
        &transport,
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("confirm concurrent response card");
    let turn = response_turn_key(4055, "2026-07-11T02:39:00Z", Some(30));
    let claim = |owner| {
        claim_task_response_delivery(
            Some(&pool),
            event.scope.channel_id,
            &event.scope.provider,
            &event.scope.session_key,
            event.event_key(),
            &turn,
            card.message_id,
            owner,
        )
    };
    let (sink, watcher) = tokio::join!(
        claim(ResponseDeliveryOwner::Sink),
        claim(ResponseDeliveryOwner::Watcher)
    );
    let outcomes = [sink.expect("sink claim"), watcher.expect("watcher claim")];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, ResponseDeliveryClaimOutcome::Owned(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, ResponseDeliveryClaimOutcome::Wait))
            .count(),
        1
    );
}

#[tokio::test]
async fn delivered_semantic_event_accepts_a_second_response_turn_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_response_cycle_4055",
        "sequential task response turns",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let transport = FakeTransport::new();
    let clients = clients();
    let event = event("sequential-response-cycle");
    let card = ensure_card(
        Some(&pool),
        &clients,
        &transport,
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("confirm card");
    let first = response_turn_key(4055, "2026-07-11T02:37:00Z", Some(10));
    let second = response_turn_key(4056, "2026-07-11T02:38:00Z", Some(20));

    let first_claim = claim_task_response_delivery(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &first,
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("claim first response turn");
    let ResponseDeliveryClaimOutcome::Owned(first_claim) = first_claim else {
        panic!("first response turn must be owned")
    };
    mark_task_response_delivered(Some(&pool), &first_claim)
        .await
        .expect("deliver first response turn");

    let second_claim = claim_task_response_delivery(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &second,
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("a delivered event must open a fresh exact response cycle");
    assert!(matches!(
        second_claim,
        ResponseDeliveryClaimOutcome::Owned(_)
    ));
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_notification_response_delivery WHERE event_key = $1",
    )
    .bind(event.event_key())
    .fetch_one(&pool)
    .await
    .expect("response cycle row count");
    assert_eq!(
        rows, 2,
        "one semantic card must retain one row per response turn"
    );
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
