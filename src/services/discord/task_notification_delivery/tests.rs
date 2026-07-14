use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use poise::serenity_prelude as serenity;

use super::response_chunks::{
    ResponseChunkHistoryError, ResponseChunkHistoryMessage, ResponseChunkPostError,
    ResponseChunkTransport,
};
use super::*;

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
    content_hash_by_nonce: Mutex<HashMap<String, String>>,
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
        content: &str,
        nonce: &str,
    ) -> Result<u64, TaskCardTransportError> {
        self.post_calls.fetch_add(1, Ordering::AcqRel);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let (message_id, inserted) = {
            let mut messages = self.by_nonce.lock().expect("fake nonce map");
            match messages.entry(nonce.to_string()) {
                std::collections::hash_map::Entry::Occupied(entry) => (*entry.get(), false),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    self.physical_posts.fetch_add(1, Ordering::AcqRel);
                    let message_id = self.next_message_id.fetch_add(1, Ordering::AcqRel);
                    entry.insert(message_id);
                    (message_id, true)
                }
            }
        };
        if inserted {
            self.content_hash_by_nonce
                .lock()
                .expect("fake card content hashes")
                .insert(nonce.to_string(), content_hash(content));
        }
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

    async fn reconcile_card_post(
        &self,
        _bot: &CardBot,
        _channel_id: u64,
        nonce: &str,
        expected_content_hash: &str,
        _post_started_at: chrono::DateTime<chrono::Utc>,
    ) -> CardPostReconcile {
        let message_id = self
            .by_nonce
            .lock()
            .expect("fake nonce map")
            .get(nonce)
            .copied();
        let actual_hash = self
            .content_hash_by_nonce
            .lock()
            .expect("fake card content hashes")
            .get(nonce)
            .cloned();
        match (message_id, actual_hash) {
            (Some(message_id), Some(actual_hash)) if actual_hash == expected_content_hash => {
                CardPostReconcile::Found(message_id)
            }
            (Some(_), Some(_)) => CardPostReconcile::Ambiguous(
                "fake history found the card nonce with different content".into(),
            ),
            _ => CardPostReconcile::Ambiguous(
                "fake history cannot disprove card POST-then-delete".into(),
            ),
        }
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

struct FakeResponseChunkTransport {
    bot_user_id: u64,
    clock: Mutex<chrono::DateTime<chrono::Utc>>,
    messages: Mutex<Vec<ResponseChunkHistoryMessage>>,
    next_message_id: AtomicU64,
    post_calls: AtomicUsize,
    physical_posts: AtomicUsize,
    nonce_window_seconds: AtomicI64,
    fail_after_commit_once: AtomicBool,
    unknown_reference_once: AtomicBool,
    unknown_reference_failures: AtomicUsize,
    fail_before_post_call: Mutex<Option<usize>>,
    delay_after_post_call: Mutex<Option<(usize, u64)>>,
    history_authoritative: AtomicBool,
    history_error: AtomicBool,
    history_permanent_error: AtomicBool,
}

impl FakeResponseChunkTransport {
    fn new() -> Self {
        Self {
            bot_user_id: 77_4055,
            clock: Mutex::new(chrono::Utc::now()),
            messages: Mutex::new(Vec::new()),
            next_message_id: AtomicU64::new(200_000),
            post_calls: AtomicUsize::new(0),
            physical_posts: AtomicUsize::new(0),
            nonce_window_seconds: AtomicI64::new(120),
            fail_after_commit_once: AtomicBool::new(false),
            unknown_reference_once: AtomicBool::new(false),
            unknown_reference_failures: AtomicUsize::new(0),
            fail_before_post_call: Mutex::new(None),
            delay_after_post_call: Mutex::new(None),
            history_authoritative: AtomicBool::new(false),
            history_error: AtomicBool::new(false),
            history_permanent_error: AtomicBool::new(false),
        }
    }

    fn advance(&self, seconds: i64) {
        *self.clock.lock().expect("fake response clock") += chrono::Duration::seconds(seconds);
    }

    fn expire_nonce_cache(&self) {
        self.nonce_window_seconds.store(0, Ordering::Release);
    }

    fn fail_before_post_call(&self, call: usize) {
        *self.fail_before_post_call.lock().expect("fake fail call") = Some(call);
    }

    fn delay_after_post_call(&self, call: usize, millis: u64) {
        *self.delay_after_post_call.lock().expect("fake delay call") = Some((call, millis));
    }

    fn fail_unknown_references(&self, attempts: usize) {
        self.unknown_reference_failures
            .store(attempts, Ordering::Release);
    }
}

impl ResponseChunkTransport for FakeResponseChunkTransport {
    async fn bot_user_id(&self) -> Result<u64, String> {
        Ok(self.bot_user_id)
    }

    async fn post_chunk(
        &self,
        channel_id: u64,
        content: &str,
        reference_message_id: Option<u64>,
        nonce: &str,
    ) -> Result<u64, ResponseChunkPostError> {
        let call = self.post_calls.fetch_add(1, Ordering::AcqRel) + 1;
        let counted_unknown = reference_message_id.is_some()
            && self
                .unknown_reference_failures
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok();
        if reference_message_id.is_some()
            && (self.unknown_reference_once.swap(false, Ordering::AcqRel) || counted_unknown)
        {
            return Err(ResponseChunkPostError::UnknownReference(
                "injected Discord unknown required reference".to_string(),
            ));
        }
        if self
            .fail_before_post_call
            .lock()
            .expect("fake fail call")
            .take_if(|expected| *expected == call)
            .is_some()
        {
            return Err(ResponseChunkPostError::Transient(format!(
                "injected pre-commit failure on call {call}"
            )));
        }
        let now = *self.clock.lock().expect("fake response clock");
        let window = self.nonce_window_seconds.load(Ordering::Acquire);
        if let Some(existing) = self
            .messages
            .lock()
            .expect("fake response history")
            .iter()
            .find(|message| {
                message.nonce.as_deref() == Some(nonce)
                    && now.signed_duration_since(message.created_at)
                        < chrono::Duration::seconds(window)
            })
            .map(|message| message.message_id)
        {
            return Ok(existing);
        }
        let message_id = self.next_message_id.fetch_add(1, Ordering::AcqRel);
        self.physical_posts.fetch_add(1, Ordering::AcqRel);
        self.messages
            .lock()
            .expect("fake response history")
            .push(ResponseChunkHistoryMessage {
                channel_id,
                message_id,
                author_id: self.bot_user_id,
                nonce: Some(nonce.to_string()),
                content_hash: content_hash(content),
                referenced_message_id: reference_message_id,
                created_at: now,
            });
        if self.fail_after_commit_once.swap(false, Ordering::AcqRel) {
            return Err(ResponseChunkPostError::Transient(
                "injected POST commit followed by lost response".to_string(),
            ));
        }
        let delay = self
            .delay_after_post_call
            .lock()
            .expect("fake delay call")
            .take_if(|(expected, _)| *expected == call)
            .map(|(_, millis)| millis);
        if let Some(millis) = delay {
            tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
        }
        Ok(message_id)
    }

    async fn history_page(
        &self,
        channel_id: u64,
        before_message_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<ResponseChunkHistoryMessage>, ResponseChunkHistoryError> {
        if self.history_permanent_error.load(Ordering::Acquire) {
            return Err(ResponseChunkHistoryError::Permanent(
                "injected Discord history 403".to_string(),
            ));
        }
        if self.history_error.load(Ordering::Acquire) {
            return Err(ResponseChunkHistoryError::Transient(
                "injected Discord history transport failure".to_string(),
            ));
        }
        let mut messages = self
            .messages
            .lock()
            .expect("fake response history")
            .iter()
            .filter(|message| {
                message.channel_id == channel_id
                    && before_message_id.is_none_or(|before| message.message_id < before)
            })
            .cloned()
            .collect::<Vec<_>>();
        messages.sort_by_key(|message| std::cmp::Reverse(message.message_id));
        messages.truncate(limit);
        Ok(messages)
    }

    fn history_proves_deletions(&self) -> bool {
        self.history_authoritative.load(Ordering::Acquire)
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

fn sourced_event(
    session_key: &str,
    source_event_id: &str,
    task_id: &str,
    result: &str,
) -> TaskCardEvent {
    TaskCardEvent::from_task_prompt_with_source_event_id(
        44_295,
        "claude",
        session_key,
        &format!(
            "<task-notification><task-id>{task_id}</task-id><status>completed</status><summary>Agent completed</summary><result>{result}</result></task-notification>"
        ),
        Some(source_event_id),
    )
}

fn promotion_event(session_key: &str, task_id: &str) -> TaskCardEvent {
    let context = TaskNotificationContext::from_stream_json(
        &serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "task_id": task_id,
            "status": "completed",
            "summary": "Agent completed",
            "task_notification_kind": "background"
        }),
        &crate::services::session_backend::StreamLineState::new(),
    )
    .expect("stream task context");
    context.to_event(44_295, "claude", session_key)
}

#[tokio::test]
async fn sourced_idle_observation_and_stream_promotion_post_one_card() {
    let transport = FakeTransport::new();
    let clients = clients();
    let session_key = "AgentDesk-claude-4295-lane-convergence";
    let observed = sourced_event(
        session_key,
        "lane-convergence-source-entry",
        "lane-convergence-task",
        "single terminal payload",
    );
    let promoted = promotion_event(session_key, "lane-convergence-task");

    let first = ensure_card(
        None,
        &clients,
        &transport,
        &observed,
        EnsureIntent::Observation,
    )
    .await
    .expect("idle scanner observation card");
    let promotion = ensure_card(
        None,
        &clients,
        &transport,
        &promoted,
        EnsureIntent::Promotion,
    )
    .await
    .expect("same-turn stream promotion");

    assert_eq!(
        transport.physical_posts.load(Ordering::Acquire),
        1,
        "source identity must supplement the shared semantic claim key"
    );
    assert_eq!(promotion.disposition, CardDisposition::Existing);
    assert_eq!(promotion.message_id, first.message_id);
}

#[tokio::test]
async fn footer_observation_promotion_and_exact_replay_post_one_card_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_card_footer_replay_4295",
        "footer task card promotion and replay convergence",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let clients = clients();
    let source_event_id = "footer-source-entry";
    let task_id = "footer-replay-task";
    let session_key = "AgentDesk-claude-4295-footer";
    let observed = sourced_event(
        session_key,
        source_event_id,
        task_id,
        "footer terminal payload",
    );
    record_footer_only(Some(&pool), &observed)
        .await
        .expect("footer-only observation");

    let transport = FakeTransport::new();
    let promoted = promotion_event(session_key, task_id);
    let card = ensure_card(
        Some(&pool),
        &clients,
        &transport,
        &promoted,
        EnsureIntent::Promotion,
    )
    .await
    .expect("footer promotion card");
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 1);

    sqlx::query(
        "UPDATE task_notification_card_state
         SET created_at = NOW() - INTERVAL '12 hours',
             updated_at = NOW() - INTERVAL '12 hours'
         WHERE discord_message_id = $1",
    )
    .bind(i64::try_from(card.message_id).expect("message id"))
    .execute(&pool)
    .await
    .expect("age footer-promoted card");
    sqlx::query(
        "UPDATE task_notification_terminal_delivery
         SET delivered_at = NOW() - INTERVAL '12 hours'
         WHERE discord_message_id = $1",
    )
    .bind(i64::try_from(card.message_id).expect("message id"))
    .execute(&pool)
    .await
    .expect("age footer terminal delivery");
    pool.close().await;

    let restarted_pool = pg_db.connect_and_migrate().await;
    let replay_transport = FakeTransport::new();
    let replay = sourced_event(
        "AgentDesk-claude-4295-footer-after-restart",
        source_event_id,
        task_id,
        "footer terminal payload",
    );
    let replay_outcome = ensure_card(
        Some(&restarted_pool),
        &clients,
        &replay_transport,
        &replay,
        EnsureIntent::Observation,
    )
    .await
    .expect("footer exact replay lookup");

    assert_eq!(replay_outcome.disposition, CardDisposition::Existing);
    assert_eq!(replay_outcome.message_id, card.message_id);
    assert_eq!(
        replay_transport.physical_posts.load(Ordering::Acquire),
        0,
        "footer deferral, promotion, and replay must share one durable identity"
    );
    pg_db.drop().await;
}

#[tokio::test]
async fn pane_first_then_uuid_replay_backfills_semantic_delivery_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_card_pane_uuid_replay_4295",
        "pane-first task card replay fingerprint backfill",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let clients = clients();
    let task_id = "pane-first-task";
    let first_event = TaskCardEvent::from_task_prompt(
        44_295,
        "claude",
        "AgentDesk-claude-4295-pane-first",
        &format!(
            "<task-notification><task-id>{task_id}</task-id><status>completed</status><summary>Agent completed</summary><result>pane terminal payload</result></task-notification>"
        ),
    );
    let first_transport = FakeTransport::new();
    let first = ensure_card(
        Some(&pool),
        &clients,
        &first_transport,
        &first_event,
        EnsureIntent::Observation,
    )
    .await
    .expect("pane observation card without entry id");
    assert_eq!(first_transport.physical_posts.load(Ordering::Acquire), 1);

    sqlx::query(
        "UPDATE task_notification_card_state
         SET created_at = NOW() - INTERVAL '12 hours',
             updated_at = NOW() - INTERVAL '12 hours'
         WHERE discord_message_id = $1",
    )
    .bind(i64::try_from(first.message_id).expect("message id"))
    .execute(&pool)
    .await
    .expect("age pane-delivered card");
    sqlx::query(
        "UPDATE task_notification_terminal_delivery
         SET delivered_at = NOW() - INTERVAL '12 hours'
         WHERE discord_message_id = $1",
    )
    .bind(i64::try_from(first.message_id).expect("message id"))
    .execute(&pool)
    .await
    .expect("age pane terminal delivery");
    pool.close().await;

    let restarted_pool = pg_db.connect_and_migrate().await;
    let replay_transport = FakeTransport::new();
    let replay = sourced_event(
        "AgentDesk-claude-4295-pane-after-restart",
        "pane-entry-uuid-after-offset-reset",
        task_id,
        "pane terminal payload",
    );
    let replay_outcome = ensure_card(
        Some(&restarted_pool),
        &clients,
        &replay_transport,
        &replay,
        EnsureIntent::Observation,
    )
    .await
    .expect("uuid replay resolves fp-less semantic delivery");

    assert_eq!(
        replay_transport.physical_posts.load(Ordering::Acquire),
        0,
        "semantic delivery fallback must suppress pane-first replay"
    );
    assert_eq!(replay_outcome.disposition, CardDisposition::Existing);
    assert_eq!(replay_outcome.message_id, first.message_id);
    let stored_fingerprint: Option<String> = sqlx::query_scalar(
        "SELECT terminal_delivery_fingerprint
         FROM task_notification_card_state
         WHERE discord_message_id = $1",
    )
    .bind(i64::try_from(first.message_id).expect("message id"))
    .fetch_one(&restarted_pool)
    .await
    .expect("load backfilled semantic fingerprint");
    assert_eq!(
        stored_fingerprint,
        replay.scope.terminal_delivery_fingerprint
    );
    pg_db.drop().await;
}

#[tokio::test]
async fn promotion_first_edit_then_new_completion_preserves_original_replay_hash_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_card_promotion_edit_replay_4295",
        "promotion-first edited task card replay hash",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let clients = clients();
    let transport = FakeTransport::new();
    let session_key = "AgentDesk-claude-4295-promotion-edit-replay";
    let task_id = "promotion-edit-replay-task";
    let promoted = promotion_event(session_key, task_id);
    let original_content = promoted.payload.render(1);
    let first = ensure_card(
        Some(&pool),
        &clients,
        &transport,
        &promoted,
        EnsureIntent::Promotion,
    )
    .await
    .expect("promotion-first card without source fingerprint");

    let edited = TaskCardEvent::from_task_prompt(
        44_295,
        "claude",
        session_key,
        &format!(
            "<task-notification><task-id>{task_id}</task-id><status>completed</status><summary>Agent completed</summary><result>later pane detail</result></task-notification>"
        ),
    );
    ensure_card(
        Some(&pool),
        &clients,
        &transport,
        &edited,
        EnsureIntent::Observation,
    )
    .await
    .expect("fingerprint-less duplicate edits promoted card");
    assert_ne!(edited.payload.render(2), original_content);

    let followup = sourced_event(
        session_key,
        "promotion-edit-followup-entry",
        task_id,
        "genuine later completion",
    );
    ensure_card(
        Some(&pool),
        &clients,
        &transport,
        &followup,
        EnsureIntent::Observation,
    )
    .await
    .expect("new source-bound completion");
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 2);
    pool.close().await;

    let restarted_pool = pg_db.connect_and_migrate().await;
    let replay = TaskCardEvent::from_task_prompt_with_source_event_id(
        44_295,
        "claude",
        "AgentDesk-claude-4295-promotion-edit-restarted",
        &format!(
            "<task-notification><task-id>{task_id}</task-id><status>completed</status><summary>Agent completed</summary></task-notification>"
        ),
        Some("promotion-edit-original-entry-replay"),
    );
    assert_eq!(replay.payload.render(1), original_content);
    let replay_transport = FakeTransport::new();
    let replay_outcome = ensure_card(
        Some(&restarted_pool),
        &clients,
        &replay_transport,
        &replay,
        EnsureIntent::Observation,
    )
    .await
    .expect("original source entry replay after restart");

    assert_eq!(replay_outcome.disposition, CardDisposition::Existing);
    assert_eq!(replay_outcome.message_id, first.message_id);
    assert_eq!(
        replay_transport.physical_posts.load(Ordering::Acquire),
        0,
        "the first-delivered hash must survive edits and later completions"
    );
    pg_db.drop().await;
}

#[tokio::test]
async fn durable_terminal_identity_suppresses_restart_compaction_exact_replay_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_card_exact_replay_4295",
        "task card restart compaction exact replay guard",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let clients = clients();
    let source_event_id = "af4848f5-source-entry";
    let first_event = sourced_event(
        "AgentDesk-claude-4295-before-restart",
        source_event_id,
        "af4848f5",
        "original terminal payload",
    );
    let first_transport = FakeTransport::new();
    let first = ensure_card(
        Some(&pool),
        &clients,
        &first_transport,
        &first_event,
        EnsureIntent::Observation,
    )
    .await
    .expect("first terminal card");
    assert_eq!(first_transport.physical_posts.load(Ordering::Acquire), 1);

    sqlx::query(
        "UPDATE task_notification_card_state
         SET created_at = NOW() - INTERVAL '12 hours',
             updated_at = NOW() - INTERVAL '12 hours'
         WHERE discord_message_id = $1",
    )
    .bind(i64::try_from(first.message_id).expect("message id"))
    .execute(&pool)
    .await
    .expect("age delivered card past the former one-hour dedupe window");
    pool.close().await;

    // A process restart loses every in-memory nonce/message cache. Compaction
    // also shifts the transcript byte boundary and can mint a new synthetic
    // anchor/session, but the provider entry id remains byte-for-byte stable.
    let restarted_pool = pg_db.connect_and_migrate().await;
    let replay_event = sourced_event(
        "AgentDesk-claude-4295-after-compact-offset-reset",
        source_event_id,
        "af4848f5",
        "original terminal payload",
    );
    let restarted_transport = FakeTransport::new();
    let replay = ensure_card(
        Some(&restarted_pool),
        &clients,
        &restarted_transport,
        &replay_event,
        EnsureIntent::Observation,
    )
    .await
    .expect("durable replay lookup");

    assert_eq!(replay.disposition, CardDisposition::Existing);
    assert_eq!(replay.message_id, first.message_id);
    assert_eq!(
        restarted_transport.physical_posts.load(Ordering::Acquire),
        0,
        "restart + 12h + compact-offset-reset exact replay must issue zero POSTs"
    );
    pg_db.drop().await;
}

#[tokio::test]
async fn same_task_new_source_event_and_payload_posts_followup_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_card_followup_4295",
        "same task new terminal follow-up remains deliverable",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let clients = clients();
    let transport = FakeTransport::new();
    let original = sourced_event(
        "AgentDesk-claude-4295-followup",
        "source-boundary-first",
        "af4848f5",
        "first terminal payload",
    );
    let followup = sourced_event(
        "AgentDesk-claude-4295-followup",
        "source-boundary-second",
        "af4848f5",
        "new follow-up terminal payload",
    );

    let first = ensure_card(
        Some(&pool),
        &clients,
        &transport,
        &original,
        EnsureIntent::Observation,
    )
    .await
    .expect("original terminal card");
    let second = ensure_card(
        Some(&pool),
        &clients,
        &transport,
        &followup,
        EnsureIntent::Observation,
    )
    .await
    .expect("new follow-up terminal card");

    assert_eq!(first.disposition, CardDisposition::Created);
    assert_eq!(second.disposition, CardDisposition::Created);
    assert_ne!(first.message_id, second.message_id);
    assert_eq!(
        transport.physical_posts.load(Ordering::Acquire),
        2,
        "a different source boundary/payload for the same task id must POST"
    );
    pg_db.drop().await;
}

#[tokio::test]
async fn concurrent_new_completions_advance_distinct_revisions_and_nonces_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_card_concurrent_followup_4295",
        "concurrent new terminal completions use distinct nonces",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let clients = clients();
    let transport = Arc::new(FakeTransport::new());
    let session_key = "AgentDesk-claude-4295-concurrent-followup";
    let task_id = "concurrent-followup-task";
    let original = sourced_event(
        session_key,
        "concurrent-followup-original",
        task_id,
        "original completion",
    );
    ensure_card(
        Some(&pool),
        &clients,
        transport.as_ref(),
        &original,
        EnsureIntent::Observation,
    )
    .await
    .expect("original completion card");

    let left_event = sourced_event(
        session_key,
        "concurrent-followup-left",
        task_id,
        "left completion",
    );
    let right_event = sourced_event(
        session_key,
        "concurrent-followup-right",
        task_id,
        "right completion",
    );
    store::install_new_terminal_claim_race_hook(left_event.event_key());
    let (left, right) = tokio::join!(
        ensure_card(
            Some(&pool),
            &clients,
            transport.as_ref(),
            &left_event,
            EnsureIntent::Observation,
        ),
        ensure_card(
            Some(&pool),
            &clients,
            transport.as_ref(),
            &right_event,
            EnsureIntent::Observation,
        ),
    );
    let left = left.expect("left concurrent completion");
    let right = right.expect("right concurrent completion");

    assert_ne!(left.message_id, right.message_id);
    assert_eq!(
        transport.physical_posts.load(Ordering::Acquire),
        3,
        "each distinct completion must cross Discord with a distinct nonce"
    );
    let revisions: Vec<i32> = sqlx::query_scalar(
        "SELECT revision FROM task_notification_card_state
         WHERE channel_id = $1 AND provider = $2 AND event_key = $3",
    )
    .bind(i64::try_from(original.scope.channel_id).expect("channel id"))
    .bind(&original.scope.provider)
    .bind(original.event_key())
    .fetch_all(&pool)
    .await
    .expect("load final task card revision");
    assert_eq!(revisions, vec![3]);
    assert_eq!(
        transport.by_nonce.lock().expect("fake nonce map").len(),
        3,
        "revision CAS must force the stale writer to derive a fresh nonce"
    );
    pg_db.drop().await;
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
async fn resumed_card_nonce_keeps_the_payload_that_crossed_the_post_boundary_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_card_payload_drift_4446",
        "task card nonce payload must stay immutable after POST begins",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let transport = FakeTransport::new();
    transport
        .fail_next_post_after_commit
        .store(true, Ordering::Release);
    let clients = clients();
    let original = event("payload-drift");
    let changed = TaskCardEvent::from_task_prompt(
        original.scope.channel_id,
        &original.scope.provider,
        &original.scope.session_key,
        "<task-notification><task-id>payload-drift</task-id><tool-use-id>toolu-payload-drift</tool-use-id><status>completed</status><summary>changed summary</summary><result>changed result</result></task-notification>",
    );
    assert_eq!(original.scope.event_key, changed.scope.event_key);
    let original_content = original.payload.render(1);
    assert!(
        ensure_card(
            Some(&pool),
            &clients,
            &transport,
            &original,
            EnsureIntent::Observation,
        )
        .await
        .is_err()
    );

    let recovered = ensure_card(
        Some(&pool),
        &clients,
        &transport,
        &changed,
        EnsureIntent::Observation,
    )
    .await
    .expect("same nonce resumes the original payload");
    assert!(recovered.message_id > 0);
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 1);
    let persisted: String = sqlx::query_scalar(
        "SELECT rendered_content FROM task_notification_card_state
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4",
    )
    .bind(i64::try_from(original.scope.channel_id).expect("channel id"))
    .bind(&original.scope.provider)
    .bind(&original.scope.session_key)
    .bind(&original.scope.event_key)
    .fetch_one(&pool)
    .await
    .expect("load persisted card payload");
    assert_eq!(persisted, original_content);
    let nonce = stable_nonce(&original.scope, 1);
    assert_eq!(
        transport
            .content_hash_by_nonce
            .lock()
            .expect("fake card content hashes")
            .get(&nonce)
            .cloned(),
        Some(content_hash(&original_content)),
        "Discord's original nonce payload must agree with durable state"
    );
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
    let pending = claim_existing_task_response_delivery(
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
    assert_eq!(pending.card_message_id, confirmed.message_id);
    assert_eq!(pending.event_key, delivered_event.event_key());
    assert!(matches!(
        pending.outcome,
        ResponseDeliveryClaimOutcome::Wait
    ));

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
    let watcher_existing = claim_existing_task_response_delivery(
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
    assert_eq!(watcher_existing.card_message_id, confirmed.message_id);
    assert_eq!(watcher_existing.event_key, delivered_event.event_key());
    assert_eq!(watcher_existing.card_bot_key, "notify");
    let recovered_event = TaskCardEvent::from_recovered_terminal(
        delivered_event.scope.channel_id,
        &delivered_event.scope.provider,
        &delivered_event.scope.session_key,
        TaskNotificationKind::Background,
        &turn_key,
    );
    assert_ne!(recovered_event.event_key(), delivered_event.event_key());
    let recovered_event = recovered_event.with_persisted_event_key(&watcher_existing.event_key);
    let ResponseDeliveryClaimOutcome::Owned(watcher_claim) = watcher_existing.outcome else {
        panic!("watcher must own the expired response claim")
    };
    assert!(
        mark_task_response_delivered(Some(&pool), &sink_claim)
            .await
            .is_err(),
        "the stale sink token must not commit the watcher-owned response"
    );
    // Model the production UnknownReference branch after restart: repair must
    // address the rich sink event row and use its persisted notify-bot pin,
    // never the synthetic `turn:<response>` scope.
    let replacement = replace_confirmed_missing_card(
        Some(&pool),
        &clients,
        &transport,
        &recovered_event,
        confirmed.message_id,
    )
    .await
    .expect("watcher repairs Discord-confirmed deleted notify card");
    let watcher_claim =
        rebind_task_response_card(Some(&pool), &watcher_claim, replacement.message_id)
            .await
            .expect("watcher rebinds recovered response to repaired rich event card");
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
            replacement.message_id,
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
async fn watcher_fallback_turn_key_resumes_sink_primary_key_without_second_row_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_response_alias_4055",
        "sink primary to watcher fallback response identity",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let transport = FakeTransport::new();
    let clients = clients();
    let event = event("primary-fallback-alias");
    let card = ensure_card(
        Some(&pool),
        &clients,
        &transport,
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("notify-pinned task card");
    let primary = response_turn_key(4_446, "2026-07-11T10:37:00Z", Some(44_460));
    let fallback = fallback_response_turn_key(
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        55_500,
        "terminal answer after restart",
    );
    assert_ne!(primary, fallback);
    let sink = claim_task_response_delivery_with_recovery_key(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &primary,
        Some(&fallback),
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("sink persists canonical and recovery identities");
    let ResponseDeliveryClaimOutcome::Owned(sink) = sink else {
        panic!("sink must own new response")
    };
    sqlx::query(
        "UPDATE task_notification_response_delivery
         SET lease_expires_at = NOW() - INTERVAL '1 second'
         WHERE response_turn_key = $1",
    )
    .bind(&primary)
    .execute(&pool)
    .await
    .expect("model sink crash");

    let resumed = claim_existing_task_response_delivery(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        &fallback,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("watcher resolves recovery alias")
    .expect("persisted sink response");
    assert_eq!(resumed.response_turn_key, primary);
    assert_eq!(resumed.event_key, event.event_key());
    assert_eq!(resumed.card_bot_key, "notify");
    let ResponseDeliveryClaimOutcome::Owned(watcher) = resumed.outcome else {
        panic!("watcher must take over expired sink response")
    };
    assert_eq!(watcher.response_turn_key(), primary);
    assert!(
        mark_task_response_sent(Some(&pool), &sink).await.is_err(),
        "stale sink cannot commit after alias takeover"
    );
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_notification_response_delivery
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3",
    )
    .bind(i64::try_from(event.scope.channel_id).expect("channel id"))
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .fetch_one(&pool)
    .await
    .expect("count response rows");
    assert_eq!(rows, 1, "primary/fallback actors must share one fence");
}

#[tokio::test]
async fn watcher_primary_key_blocks_late_sink_frame_alias_without_second_row_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_response_reverse_alias_4055",
        "watcher fallback primary to late sink frame alias",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let transport = FakeTransport::new();
    let event = event("watcher-primary-frame-alias");
    let card = ensure_card(
        Some(&pool),
        &clients(),
        &transport,
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("notify-pinned task card");
    let frame_key = response_turn_key(4_446, "2026-07-11T10:38:00Z", Some(44_461));
    let fallback_key = fallback_response_turn_key(
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        55_501,
        "watcher delivered before the frame sink resumed",
    );
    let sink_fallback_key = fallback_response_turn_key(
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        55_777,
        "sink normalized a different terminal body",
    );
    assert_ne!(frame_key, fallback_key);
    assert_ne!(sink_fallback_key, fallback_key);

    let watcher = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &fallback_key,
        Some(&fallback_key),
        Some("2026-07-11 19:38:00"),
        Some(44_461),
        Some(55_501),
        card.message_id,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("watcher persists fallback-primary response");
    assert!(matches!(watcher, ResponseDeliveryClaimOutcome::Owned(_)));

    let sink = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &frame_key,
        Some(&sink_fallback_key),
        Some("2026-07-11 19:38:00"),
        Some(44_461),
        Some(55_501),
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("late sink resolves watcher fallback-primary authority");
    assert!(
        matches!(sink, ResponseDeliveryClaimOutcome::Wait),
        "late sink must observe the active watcher lease, not own a second row"
    );

    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_notification_response_delivery
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3",
    )
    .bind(i64::try_from(event.scope.channel_id).expect("channel id"))
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .fetch_one(&pool)
    .await
    .expect("count reverse-alias response rows");
    assert_eq!(rows, 1, "watcher/sink aliases must share one fence row");
    let persisted_alias: Option<String> = sqlx::query_scalar(
        "SELECT recovery_turn_key FROM task_notification_response_delivery
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3",
    )
    .bind(i64::try_from(event.scope.channel_id).expect("channel id"))
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .fetch_one(&pool)
    .await
    .expect("load reverse-alias response row");
    assert_eq!(persisted_alias.as_deref(), Some(fallback_key.as_str()));
}

#[tokio::test]
async fn recently_delivered_watcher_blocks_a_divergent_late_sink_alias_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_response_delivered_cross_actor_4446",
        "delayed counter-actor convergence after delivery",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let event = event("delivered-cross-actor");
    let card = ensure_card(
        Some(&pool),
        &clients(),
        &FakeTransport::new(),
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("confirm card");
    let watcher_key = fallback_response_turn_key(
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        61_001,
        "watcher terminal body",
    );
    let watcher = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &watcher_key,
        Some(&watcher_key),
        None,
        None,
        Some(61_001),
        card.message_id,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("watcher response claim");
    let ResponseDeliveryClaimOutcome::Owned(watcher) = watcher else {
        panic!("watcher owns the first response")
    };
    mark_task_response_delivered(Some(&pool), &watcher)
        .await
        .expect("watcher commits delivery");

    let sink = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &response_turn_key(44_460, "2026-07-12T04:00:00Z", Some(61_000)),
        Some(&fallback_response_turn_key(
            event.scope.channel_id,
            &event.scope.provider,
            &event.scope.session_key,
            61_999,
            "sink body diverged from watcher",
        )),
        Some("2000-01-01T00:00:00Z"),
        None,
        Some(61_001),
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("late sink is recognized as the delivered counter-actor");
    assert!(matches!(
        sink,
        ResponseDeliveryClaimOutcome::Delivered { .. }
    ));
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_notification_response_delivery WHERE event_key = $1",
    )
    .bind(event.event_key())
    .fetch_one(&pool)
    .await
    .expect("count converged response rows");
    assert_eq!(rows, 1, "delayed counter-actor must not open a second row");
}

#[tokio::test]
async fn response_chunk_journal_preserves_confirmed_prefix_and_rejects_payload_drift_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_response_chunk_prefix_4446",
        "durable response chunk confirmed prefix",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let card_transport = FakeTransport::new();
    let event = event("chunk-confirmed-prefix");
    let card = ensure_card(
        Some(&pool),
        &clients(),
        &card_transport,
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("response card");
    let turn_key = response_turn_key(44_461, "2026-07-11T11:01:00Z", Some(1));
    let claim = claim_task_response_delivery(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("response claim");
    let ResponseDeliveryClaimOutcome::Owned(claim) = claim else {
        panic!("new response must be owned")
    };
    let response_transport = FakeResponseChunkTransport::new();
    response_transport.fail_before_post_call(2);
    let text = "x".repeat(crate::services::discord::DISCORD_MSG_LIMIT + 32);
    let first = send_task_response_chunks(Some(&pool), &response_transport, &claim, &text).await;
    assert!(
        matches!(first, Err(ResponseChunkDeliveryError::Transient(_))),
        "unexpected first partial result: {first:?}"
    );
    assert_eq!(response_transport.physical_posts.load(Ordering::Acquire), 1);

    let resumed = send_task_response_chunks(Some(&pool), &response_transport, &claim, &text)
        .await
        .expect("resume at first unconfirmed chunk");
    assert_eq!(resumed.len(), 2);
    assert_eq!(
        response_transport.physical_posts.load(Ordering::Acquire),
        2,
        "confirmed prefix must not be deleted or replayed"
    );
    let calls_before_drift = response_transport.post_calls.load(Ordering::Acquire);
    assert!(matches!(
        send_task_response_chunks(
            Some(&pool),
            &response_transport,
            &claim,
            &format!("{text}different tail"),
        )
        .await,
        Err(ResponseChunkDeliveryError::Permanent(_))
    ));
    assert_eq!(
        response_transport.post_calls.load(Ordering::Acquire),
        calls_before_drift,
        "immutable chunk count/hash must reject actor text drift before POST"
    );
}

#[tokio::test]
async fn recent_post_ack_loss_retries_same_nonce_without_second_physical_message() {
    let event = event("recent-expiring-nonce");
    let turn_key = response_turn_key(44_467, "2026-07-11T11:07:00Z", Some(7));
    let claim = claim_task_response_delivery(
        None,
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        90_467,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("response claim");
    let ResponseDeliveryClaimOutcome::Owned(claim) = claim else {
        panic!("new response must be owned")
    };
    let transport = FakeResponseChunkTransport::new();
    transport
        .fail_after_commit_once
        .store(true, Ordering::Release);
    assert!(matches!(
        send_task_response_chunks(None, &transport, &claim, "answer").await,
        Err(ResponseChunkDeliveryError::Transient(_))
    ));
    let reconciled = send_task_response_chunks(None, &transport, &claim, "answer")
        .await
        .expect("same recent nonce reconciles Discord message id");
    assert_eq!(reconciled.len(), 1);
    assert_eq!(transport.post_calls.load(Ordering::Acquire), 2);
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn delayed_first_chunk_does_not_age_a_later_unattempted_chunk_outside_nonce_authority_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_response_chunk_delayed_prefix_4446",
        "delayed response prefix must not age later chunks",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let event = event("delayed-prefix-clock");
    let card = ensure_card(
        Some(&pool),
        &clients(),
        &FakeTransport::new(),
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("response card");
    let turn_key = response_turn_key(44_467, "2026-07-11T11:07:00Z", Some(7));
    let claim = claim_task_response_delivery(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("response claim");
    let ResponseDeliveryClaimOutcome::Owned(claim) = claim else {
        panic!("new response must be owned")
    };
    let transport = FakeResponseChunkTransport::new();
    transport.delay_after_post_call(1, 6_100);
    let text = "d".repeat(crate::services::discord::DISCORD_MSG_LIMIT + 32);
    let delivered = send_task_response_chunks(Some(&pool), &transport, &claim, &text)
        .await
        .expect("later unattempted chunk keeps fresh POST authority");
    assert_eq!(delivered.len(), 2);
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn expiring_nonce_takeover_reconciles_history_instead_of_reposting_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_response_chunk_expiring_nonce_4446",
        "expiring nonce response takeover",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let card_transport = FakeTransport::new();
    let event = event("expiring-nonce-takeover");
    let card = ensure_card(
        Some(&pool),
        &clients(),
        &card_transport,
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("response card");
    let turn_key = response_turn_key(44_462, "2026-07-11T11:02:00Z", Some(2));
    let claim = claim_task_response_delivery(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("response claim");
    let ResponseDeliveryClaimOutcome::Owned(_claim) = claim else {
        panic!("new response must be owned")
    };
    let response_transport = FakeResponseChunkTransport::new();
    response_transport
        .fail_after_commit_once
        .store(true, Ordering::Release);
    assert!(matches!(
        send_task_response_chunks(Some(&pool), &response_transport, &_claim, "answer").await,
        Err(ResponseChunkDeliveryError::Transient(_))
    ));
    assert_eq!(response_transport.physical_posts.load(Ordering::Acquire), 1);
    sqlx::query(
        "UPDATE task_notification_response_chunk
         SET attempt_started_at = NOW() - INTERVAL '10 minutes',
             post_started_at = NOW() - INTERVAL '10 minutes'
         WHERE response_delivery_id = (
             SELECT id FROM task_notification_response_delivery WHERE response_turn_key = $1
         )",
    )
    .bind(&turn_key)
    .execute(&pool)
    .await
    .expect("age attempt beyond nonce window");
    sqlx::query(
        "UPDATE task_notification_response_delivery
         SET lease_expires_at = NOW() - INTERVAL '1 second'
         WHERE response_turn_key = $1",
    )
    .bind(&turn_key)
    .execute(&pool)
    .await
    .expect("model sink crash and watcher takeover");
    response_transport.advance(600);
    response_transport.expire_nonce_cache();
    let existing = claim_existing_task_response_delivery(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        &turn_key,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("watcher lookup")
    .expect("persisted response");
    let ResponseDeliveryClaimOutcome::Owned(watcher) = existing.outcome else {
        panic!("watcher owns expired response")
    };
    let reconciled =
        send_task_response_chunks(Some(&pool), &response_transport, &watcher, "answer")
            .await
            .expect("history nonce/message reconciles after cache expiry");
    assert_eq!(reconciled.len(), 1);
    assert_eq!(response_transport.post_calls.load(Ordering::Acquire), 1);
    assert_eq!(response_transport.physical_posts.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn old_never_attempted_prepared_chunk_resumes_without_history_proof_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_response_chunk_old_unattempted_4446",
        "old prepared response chunk has not crossed the network boundary",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let event = event("old-unattempted-response");
    let card = ensure_card(
        Some(&pool),
        &clients(),
        &FakeTransport::new(),
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("response card");
    let turn_key = response_turn_key(44_468, "2026-07-11T11:08:00Z", Some(8));
    let claim = claim_task_response_delivery(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("response claim");
    let ResponseDeliveryClaimOutcome::Owned(claim) = claim else {
        panic!("new response must be owned")
    };
    let transport = FakeResponseChunkTransport::new();
    store::prepare_response_chunk(
        Some(&pool),
        &claim,
        0,
        1,
        &content_hash("never attempted"),
        &response_chunk_nonce_for_generation(&turn_key, 1, 0),
        transport.bot_user_id,
        Some(card.message_id),
    )
    .await
    .expect("durable pre-network intent");
    sqlx::query(
        "UPDATE task_notification_response_chunk
         SET attempt_started_at = NOW() - INTERVAL '10 minutes'
         WHERE response_delivery_id = (
             SELECT id FROM task_notification_response_delivery WHERE response_turn_key = $1
         )",
    )
    .bind(&turn_key)
    .execute(&pool)
    .await
    .expect("age durable intent without attempting POST");

    let delivered = send_task_response_chunks(Some(&pool), &transport, &claim, "never attempted")
        .await
        .expect("pre-network intent remains safe to POST at any age");
    assert_eq!(delivered.len(), 1);
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn equal_second_history_page_boundary_reconciles_by_snowflake_order_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_response_chunk_equal_second_page_4446",
        "equal-second Discord response history pagination",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let event = event("equal-second-history-page");
    let card = ensure_card(
        Some(&pool),
        &clients(),
        &FakeTransport::new(),
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("response card");
    let turn_key = response_turn_key(44_469, "2026-07-11T11:09:00Z", Some(9));
    let claim = claim_task_response_delivery(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("response claim");
    let ResponseDeliveryClaimOutcome::Owned(claim) = claim else {
        panic!("new response must be owned")
    };
    let transport = FakeResponseChunkTransport::new();
    let nonce = response_chunk_nonce_for_generation(&turn_key, 1, 0);
    store::prepare_response_chunk(
        Some(&pool),
        &claim,
        0,
        1,
        &content_hash("reconcile equal second"),
        &nonce,
        transport.bot_user_id,
        Some(card.message_id),
    )
    .await
    .expect("prepare response journal");
    sqlx::query(
        "UPDATE task_notification_response_chunk
         SET delivery_state = 'posting',
             attempt_started_at = NOW() - INTERVAL '10 minutes',
             post_started_at = NOW() - INTERVAL '10 minutes'
         WHERE response_delivery_id = (
             SELECT id FROM task_notification_response_delivery WHERE response_turn_key = $1
         )",
    )
    .bind(&turn_key)
    .execute(&pool)
    .await
    .expect("age prepared response");
    let same_second = chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp(), 0)
        .expect("whole-second Discord timestamp");
    let mut history = transport.messages.lock().expect("fake response history");
    for offset in 0..=100_u64 {
        let is_target = offset == 0;
        history.push(ResponseChunkHistoryMessage {
            channel_id: event.scope.channel_id,
            message_id: 400_000 + offset,
            author_id: transport.bot_user_id,
            nonce: Some(if is_target {
                nonce.clone()
            } else {
                format!("unrelated-{offset}")
            }),
            content_hash: content_hash(if is_target {
                "reconcile equal second"
            } else {
                "unrelated"
            }),
            referenced_message_id: is_target.then_some(card.message_id),
            created_at: same_second,
        });
    }
    drop(history);
    let reconciled =
        send_task_response_chunks(Some(&pool), &transport, &claim, "reconcile equal second")
            .await
            .expect("exclusive snowflake cursor permits equal-second next page");
    assert_eq!(reconciled, vec![serenity::MessageId::new(400_000)]);
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn unknown_reference_repair_advances_generation_and_never_reuses_old_nonce_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_response_chunk_generation_4446",
        "response chunk generation after card repair",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let card_transport = FakeTransport::new();
    let card_clients = clients();
    let event = event("unknown-reference-generation");
    let card = ensure_card(
        Some(&pool),
        &card_clients,
        &card_transport,
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("response card");
    let turn_key = response_turn_key(44_465, "2026-07-11T11:05:00Z", Some(5));
    let claim = claim_task_response_delivery(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        card.message_id,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("response claim");
    let ResponseDeliveryClaimOutcome::Owned(claim) = claim else {
        panic!("new response must be owned")
    };
    let response_transport = FakeResponseChunkTransport::new();
    response_transport
        .unknown_reference_once
        .store(true, Ordering::Release);
    assert!(matches!(
        send_task_response_chunks(Some(&pool), &response_transport, &claim, "answer").await,
        Err(ResponseChunkDeliveryError::UnknownReference { .. })
    ));
    assert_eq!(response_transport.physical_posts.load(Ordering::Acquire), 0);

    let replacement = replace_confirmed_missing_card(
        Some(&pool),
        &card_clients,
        &card_transport,
        &event,
        card.message_id,
    )
    .await
    .expect("repair missing card");
    let rebound = rebind_task_response_card(Some(&pool), &claim, replacement.message_id)
        .await
        .expect("advance response generation during exact card rebind");
    assert_eq!(claim.response_generation(), 1);
    assert_eq!(rebound.response_generation(), 2);
    assert_ne!(
        response_chunk_nonce_for_generation(&turn_key, 1, 0),
        response_chunk_nonce_for_generation(&turn_key, 2, 0),
        "a nonce tied to a rejected/deleted reference must never be reused"
    );
    send_task_response_chunks(Some(&pool), &response_transport, &rebound, "answer")
        .await
        .expect("new generation posts against replacement card");
    assert_eq!(response_transport.physical_posts.load(Ordering::Acquire), 1);
    let generations: Vec<i32> = sqlx::query_scalar(
        "SELECT response_generation FROM task_notification_response_chunk
         WHERE response_delivery_id = (
             SELECT id FROM task_notification_response_delivery WHERE response_turn_key = $1
         ) ORDER BY response_generation",
    )
    .bind(&turn_key)
    .fetch_all(&pool)
    .await
    .expect("load response chunk generations");
    assert_eq!(generations, vec![1, 2]);
}

#[tokio::test]
async fn repeated_replacement_deletion_reaches_generation_three_with_bounded_repair_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_response_chunk_repeated_card_delete_4446",
        "bounded repeated task card replacement",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let card_transport = FakeTransport::new();
    let card_clients = clients();
    let event = event("repeated-card-delete");
    let card = ensure_card(
        Some(&pool),
        &card_clients,
        &card_transport,
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("response card");
    let turn_key = response_turn_key(44_471, "2026-07-11T11:11:00Z", Some(11));
    let claim = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        Some(&turn_key),
        Some("2026-07-11T11:11:00Z"),
        Some(11),
        Some(11_100),
        card.message_id,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("response claim");
    let ResponseDeliveryClaimOutcome::Owned(claim) = claim else {
        panic!("new response must be owned")
    };
    let response_transport = FakeResponseChunkTransport::new();
    response_transport.fail_unknown_references(2);
    let (messages, rebound) = send_task_response_chunks_with_card_repair(
        Some(&pool),
        &card_clients,
        &card_transport,
        &response_transport,
        &event,
        claim,
        "generation-three answer",
    )
    .await
    .expect("two consecutive missing-card races remain recoverable");
    assert_eq!(messages.len(), 1);
    assert_eq!(rebound.response_generation(), 3);
    assert_eq!(response_transport.post_calls.load(Ordering::Acquire), 3);
    assert_eq!(response_transport.physical_posts.load(Ordering::Acquire), 1);
    let generations: Vec<i32> = sqlx::query_scalar(
        "SELECT response_generation FROM task_notification_response_chunk
         WHERE response_delivery_id = (
             SELECT id FROM task_notification_response_delivery WHERE response_turn_key = $1
         ) ORDER BY response_generation",
    )
    .bind(&turn_key)
    .fetch_all(&pool)
    .await
    .expect("load response chunk generations");
    assert_eq!(generations, vec![1, 2, 3]);

    mark_task_response_sent(Some(&pool), &rebound)
        .await
        .expect("mark first repaired response sent");
    mark_task_response_delivered(Some(&pool), &rebound)
        .await
        .expect("finish first repaired response");
    let capped_turn_key = response_turn_key(44_472, "2026-07-11T11:12:00Z", Some(12));
    let capped_claim = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &capped_turn_key,
        Some(&capped_turn_key),
        Some("2026-07-11T11:12:00Z"),
        Some(12),
        Some(12_100),
        rebound.card_message_id(),
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("bounded response claim");
    let ResponseDeliveryClaimOutcome::Owned(capped_claim) = capped_claim else {
        panic!("bounded response must be owned")
    };
    let capped_transport = FakeResponseChunkTransport::new();
    capped_transport.fail_unknown_references(3);
    let capped = send_task_response_chunks_with_card_repair(
        Some(&pool),
        &card_clients,
        &card_transport,
        &capped_transport,
        &event,
        capped_claim,
        "preserve after repair cap",
    )
    .await;
    assert!(
        matches!(capped, Err(ResponseChunkDeliveryError::Transient(ref reason)) if reason.contains("bounded repairs")),
        "repair-budget exhaustion must preserve terminal retry authority: {capped:?}"
    );
    assert_eq!(capped_transport.post_calls.load(Ordering::Acquire), 3);
    assert_eq!(capped_transport.physical_posts.load(Ordering::Acquire), 0);
    let capped_generation: i32 = sqlx::query_scalar(
        "SELECT response_generation FROM task_notification_response_delivery
         WHERE response_turn_key = $1",
    )
    .bind(&capped_turn_key)
    .fetch_one(&pool)
    .await
    .expect("load capped response generation");
    assert_eq!(capped_generation, 3);
}

#[tokio::test]
async fn old_posting_chunk_with_bounded_history_ambiguity_fails_closed_and_backs_off_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_response_chunk_ambiguous_4446",
        "bounded ambiguous response history",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let card_transport = FakeTransport::new();
    let event = event("old-posting-ambiguity");
    let card = ensure_card(
        Some(&pool),
        &clients(),
        &card_transport,
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("response card");
    let turn_key = response_turn_key(44_463, "2026-07-11T11:03:00Z", Some(3));
    let claim = claim_task_response_delivery(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("response claim");
    let ResponseDeliveryClaimOutcome::Owned(claim) = claim else {
        panic!("new response must be owned")
    };
    let response_transport = FakeResponseChunkTransport::new();
    let nonce = response_chunk_nonce_for_generation(&turn_key, 1, 0);
    store::prepare_response_chunk(
        Some(&pool),
        &claim,
        0,
        1,
        &content_hash("ambiguous POST boundary"),
        &nonce,
        response_transport.bot_user_id,
        Some(card.message_id),
    )
    .await
    .expect("durable intent exists before network POST");
    sqlx::query(
        "UPDATE task_notification_response_chunk
         SET delivery_state = 'posting',
             attempt_started_at = NOW() - INTERVAL '10 minutes',
             post_started_at = NOW() - INTERVAL '10 minutes'
         WHERE response_delivery_id = (
             SELECT id FROM task_notification_response_delivery WHERE response_turn_key = $1
         )",
    )
    .bind(&turn_key)
    .execute(&pool)
    .await
    .expect("age ambiguous POST-boundary phase");

    assert!(matches!(
        send_task_response_chunks(
            Some(&pool),
            &response_transport,
            &claim,
            "ambiguous POST boundary",
        )
        .await,
        Err(ResponseChunkDeliveryError::Ambiguous { .. })
    ));
    assert_eq!(response_transport.post_calls.load(Ordering::Acquire), 0);
    let first_alerts: i64 = sqlx::query_scalar(
        "SELECT alert_count FROM task_notification_response_chunk
         WHERE response_delivery_id = (
             SELECT id FROM task_notification_response_delivery WHERE response_turn_key = $1
         ) AND response_generation = 1 AND chunk_index = 0",
    )
    .bind(&turn_key)
    .fetch_one(&pool)
    .await
    .expect("ambiguous alert count");
    assert_eq!(first_alerts, 1);
    assert!(matches!(
        send_task_response_chunks(
            Some(&pool),
            &response_transport,
            &claim,
            "ambiguous POST boundary",
        )
        .await,
        Err(ResponseChunkDeliveryError::Ambiguous { .. })
    ));
    let second_alerts: i64 = sqlx::query_scalar(
        "SELECT alert_count FROM task_notification_response_chunk
         WHERE response_delivery_id = (
             SELECT id FROM task_notification_response_delivery WHERE response_turn_key = $1
         ) AND response_generation = 1 AND chunk_index = 0",
    )
    .bind(&turn_key)
    .fetch_one(&pool)
    .await
    .expect("backoff alert count");
    assert_eq!(
        second_alerts, 1,
        "quarantine prevents a hot alert/retry loop"
    );

    sqlx::query(
        "UPDATE task_notification_response_chunk SET next_reconcile_at = NOW() - INTERVAL '1 second'
         WHERE response_delivery_id = (
             SELECT id FROM task_notification_response_delivery WHERE response_turn_key = $1
         )",
    )
    .bind(&turn_key)
    .execute(&pool)
    .await
    .expect("release quarantine for history-error case");
    response_transport
        .history_error
        .store(true, Ordering::Release);
    assert!(matches!(
        send_task_response_chunks(
            Some(&pool),
            &response_transport,
            &claim,
            "ambiguous POST boundary",
        )
        .await,
        Err(ResponseChunkDeliveryError::Transient(ref reason))
            if reason.contains("transport failure")
    ));
    response_transport
        .history_error
        .store(false, Ordering::Release);
    response_transport
        .history_permanent_error
        .store(true, Ordering::Release);
    assert!(matches!(
        send_task_response_chunks(
            Some(&pool),
            &response_transport,
            &claim,
            "ambiguous POST boundary",
        )
        .await,
        Err(ResponseChunkDeliveryError::Ambiguous { ref reason })
            if reason.contains("403")
    ));
    assert_eq!(
        response_transport.post_calls.load(Ordering::Acquire),
        0,
        "permanent history authorization failure never opens POST authority"
    );
    let permanent_alerts: i64 = sqlx::query_scalar(
        "SELECT alert_count FROM task_notification_response_chunk
         WHERE response_delivery_id = (
             SELECT id FROM task_notification_response_delivery WHERE response_turn_key = $1
         ) AND response_generation = 1 AND chunk_index = 0",
    )
    .bind(&turn_key)
    .fetch_one(&pool)
    .await
    .expect("permanent history rejection remains operator-visible");
    assert_eq!(
        permanent_alerts, 2,
        "permanent history rejection must pass through fail-closed quarantine"
    );
    response_transport
        .history_permanent_error
        .store(false, Ordering::Release);

    sqlx::query(
        "UPDATE task_notification_response_chunk SET next_reconcile_at = NOW() - INTERVAL '1 second'
         WHERE response_delivery_id = (
             SELECT id FROM task_notification_response_delivery WHERE response_turn_key = $1
         )",
    )
    .bind(&turn_key)
    .execute(&pool)
    .await
    .expect("release quarantine for missing-nonce case");
    response_transport
        .messages
        .lock()
        .expect("fake response history")
        .push(ResponseChunkHistoryMessage {
            channel_id: event.scope.channel_id,
            message_id: 299_999,
            author_id: response_transport.bot_user_id,
            nonce: None,
            content_hash: content_hash("ambiguous POST boundary"),
            referenced_message_id: Some(card.message_id),
            created_at: chrono::Utc::now(),
        });
    assert!(matches!(
        send_task_response_chunks(
            Some(&pool),
            &response_transport,
            &claim,
            "ambiguous POST boundary",
        )
        .await,
        Err(ResponseChunkDeliveryError::Ambiguous { .. })
    ));

    sqlx::query(
        "UPDATE task_notification_response_chunk SET next_reconcile_at = NOW() - INTERVAL '1 second'
         WHERE response_delivery_id = (
             SELECT id FROM task_notification_response_delivery WHERE response_turn_key = $1
         )",
    )
    .bind(&turn_key)
    .execute(&pool)
    .await
    .expect("release quarantine for history page-cap case");
    let base = chrono::Utc::now();
    let mut history = response_transport
        .messages
        .lock()
        .expect("fake response history");
    history.clear();
    for index in 0..1_000_u64 {
        history.push(ResponseChunkHistoryMessage {
            channel_id: event.scope.channel_id,
            message_id: 300_000 + index,
            author_id: response_transport.bot_user_id,
            nonce: Some(format!("unrelated-{index}")),
            content_hash: content_hash("unrelated"),
            referenced_message_id: None,
            created_at: base
                - chrono::Duration::milliseconds(i64::try_from(999 - index).expect("millis") * 50),
        });
    }
    drop(history);
    assert!(matches!(
        send_task_response_chunks(
            Some(&pool),
            &response_transport,
            &claim,
            "ambiguous POST boundary",
        )
        .await,
        Err(ResponseChunkDeliveryError::Ambiguous { ref reason })
            if reason.contains("page cap")
    ));

    sqlx::query(
        "UPDATE task_notification_response_chunk SET next_reconcile_at = NOW() - INTERVAL '1 second'
         WHERE response_delivery_id = (
             SELECT id FROM task_notification_response_delivery WHERE response_turn_key = $1
         )",
    )
    .bind(&turn_key)
    .execute(&pool)
    .await
    .expect("release test quarantine");
    response_transport
        .messages
        .lock()
        .expect("fake response history")
        .clear();
    response_transport
        .history_authoritative
        .store(true, Ordering::Release);
    let delivered = send_task_response_chunks(
        Some(&pool),
        &response_transport,
        &claim,
        "ambiguous POST boundary",
    )
    .await
    .expect("complete no-deletion history proves crash happened before POST");
    assert_eq!(delivered.len(), 1);
    assert_eq!(response_transport.physical_posts.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn response_chunk_store_outage_remains_transient_and_never_posts_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_response_chunk_store_outage_4446",
        "response chunk store outage classification",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let card_transport = FakeTransport::new();
    let event = event("chunk-store-outage");
    let card = ensure_card(
        Some(&pool),
        &clients(),
        &card_transport,
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("response card");
    let turn_key = response_turn_key(44_464, "2026-07-11T11:04:00Z", Some(4));
    let claim = claim_task_response_delivery(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("response claim");
    let ResponseDeliveryClaimOutcome::Owned(claim) = claim else {
        panic!("new response must be owned")
    };
    pool.close().await;
    let response_transport = FakeResponseChunkTransport::new();
    let result = send_task_response_chunks(
        Some(&pool),
        &response_transport,
        &claim,
        "must wait for PostgreSQL",
    )
    .await;
    assert!(
        matches!(result, Err(ResponseChunkDeliveryError::Transient(_))),
        "store unavailability must preserve retry authority: {result:?}"
    );
    assert_eq!(response_transport.post_calls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn response_chunk_journal_is_cascade_pruned_with_bounded_response_retention_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_response_chunk_retention_4446",
        "bounded response chunk retention",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let event = event("chunk-retention");
    let card = ensure_card(
        Some(&pool),
        &clients(),
        &FakeTransport::new(),
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("response card");
    let turn_key = response_turn_key(44_466, "2026-07-11T11:06:00Z", Some(6));
    let claim = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &turn_key,
        Some(&turn_key),
        Some("2026-07-11T11:06:00Z"),
        Some(6),
        Some(6_100),
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("response claim");
    let ResponseDeliveryClaimOutcome::Owned(claim) = claim else {
        panic!("new response must be owned")
    };
    send_task_response_chunks(
        Some(&pool),
        &FakeResponseChunkTransport::new(),
        &claim,
        "retained answer",
    )
    .await
    .expect("confirm response chunk");
    mark_task_response_sent(Some(&pool), &claim)
        .await
        .expect("mark sent");
    mark_task_response_delivered(Some(&pool), &claim)
        .await
        .expect("mark delivered");
    let incomplete_turn_key = response_turn_key(44_470, "2026-07-11T11:10:00Z", Some(10));
    let incomplete = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &incomplete_turn_key,
        Some(&incomplete_turn_key),
        Some("2026-07-11T11:10:00Z"),
        Some(10),
        Some(10_100),
        card.message_id,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("incomplete response claim");
    let ResponseDeliveryClaimOutcome::Owned(incomplete) = incomplete else {
        panic!("incomplete response must be owned")
    };
    let incomplete_transport = FakeResponseChunkTransport::new();
    incomplete_transport.fail_before_post_call(2);
    let incomplete_text = "p".repeat(crate::services::discord::DISCORD_MSG_LIMIT + 32);
    assert!(matches!(
        send_task_response_chunks(
            Some(&pool),
            &incomplete_transport,
            &incomplete,
            &incomplete_text,
        )
        .await,
        Err(ResponseChunkDeliveryError::Transient(_))
    ));
    assert_eq!(
        incomplete_transport.physical_posts.load(Ordering::Acquire),
        1,
        "fixture keeps one confirmed prefix plus one unconfirmed tail"
    );
    sqlx::query(
        "UPDATE task_notification_response_delivery
         SET updated_at = NOW() - INTERVAL '8 days'
         WHERE response_turn_key IN ($1, $2)",
    )
    .bind(&turn_key)
    .bind(&incomplete_turn_key)
    .execute(&pool)
    .await
    .expect("age delivered and incomplete responses");
    sqlx::query(
        "UPDATE task_notification_response_delivery
         SET lease_expires_at = NOW() - INTERVAL '1 day'
         WHERE response_turn_key = $1",
    )
    .bind(&incomplete_turn_key)
    .execute(&pool)
    .await
    .expect("expire incomplete response lease");
    sqlx::query(
        "UPDATE task_notification_card_state
         SET updated_at = NOW() - INTERVAL '8 days'
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4",
    )
    .bind(i64::try_from(event.scope.channel_id).expect("channel id"))
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .bind(event.event_key())
    .execute(&pool)
    .await
    .expect("age card state beside incomplete response");
    store::cleanup_old_rows_pg_checked(&pool)
        .await
        .expect("bounded response/chunk cleanup");
    let responses: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_notification_response_delivery WHERE response_turn_key = $1",
    )
    .bind(&turn_key)
    .fetch_one(&pool)
    .await
    .expect("count retained response rows");
    let chunks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_notification_response_chunk")
        .fetch_one(&pool)
        .await
        .expect("count retained chunk rows");
    assert_eq!(responses, 0);
    let incomplete_responses: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_notification_response_delivery WHERE response_turn_key = $1",
    )
    .bind(&incomplete_turn_key)
    .fetch_one(&pool)
    .await
    .expect("count incomplete response rows");
    assert_eq!(
        incomplete_responses, 1,
        "incomplete parent is not a tombstone"
    );
    assert_eq!(
        chunks, 2,
        "confirmed prefix and prepared tail survive expired-lease retention cleanup"
    );
    let cards: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_notification_card_state
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4",
    )
    .bind(i64::try_from(event.scope.channel_id).expect("channel id"))
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .bind(event.event_key())
    .fetch_one(&pool)
    .await
    .expect("count card state protected by incomplete response");
    assert_eq!(
        cards, 1,
        "recovery metadata stays joined to incomplete response"
    );
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
    // remain a no-POST tombstone and become a bounded delivered record after
    // the old owner lease expires.
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
        ResponseDeliveryClaimOutcome::Delivered {
            card_message_id: 90_057
        }
    ));
    let state: String = sqlx::query_scalar(
        "SELECT delivery_state FROM task_notification_response_delivery
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3
           AND event_key = $4 AND response_turn_key = $5",
    )
    .bind(i64::try_from(event.scope.channel_id).expect("test channel id"))
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .bind(event.event_key())
    .bind(&turn_key)
    .fetch_one(&pool)
    .await
    .expect("expired sent response finalized durably");
    assert_eq!(state, "delivered");
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
async fn replacement_card_cannot_partition_active_response_authority_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_response_replacement_partition_4446",
        "replacement card shares the active event fence",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let event = event("replacement-card-active-response");
    let first_key = response_turn_key(4_446, "2026-07-12 04:30:00", Some(90_000));
    let first = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &first_key,
        Some(&first_key),
        Some("2026-07-12 04:30:00"),
        Some(90_000),
        Some(90_050),
        90_058,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("claim response on original card");
    let ResponseDeliveryClaimOutcome::Owned(first) = first else {
        panic!("first response owns original-card authority")
    };

    let second_key = response_turn_key(4_447, "2026-07-12 04:30:01", Some(90_100));
    let second = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &second_key,
        Some(&second_key),
        Some("2026-07-12 04:30:01"),
        Some(90_100),
        Some(90_150),
        90_059,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("replacement-card claimant observes the event fence");
    assert!(matches!(second, ResponseDeliveryClaimOutcome::Wait));
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_notification_response_delivery WHERE event_key = $1",
    )
    .bind(event.event_key())
    .fetch_one(&pool)
    .await
    .expect("count active response authorities");
    assert_eq!(
        rows, 1,
        "card replacement must not open a second active row"
    );

    let rebound = rebind_task_response_card(Some(&pool), &first, 90_059)
        .await
        .expect("original exact owner still rebinds to replacement card");
    assert_eq!(rebound.card_message_id, 90_059);
}

#[tokio::test]
async fn expired_claim_rebinds_during_same_turn_event_fallback_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_response_expired_replacement_4446",
        "expired same-turn claim moves to the confirmed replacement card",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let event = event("expired-replacement-card-response");
    let first_key = response_turn_key(44_460, "2026-07-12 06:00:00", Some(120_000));
    let first = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &first_key,
        Some(&first_key),
        Some("2026-07-12 06:00:00"),
        Some(120_000),
        Some(120_050),
        90_060,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("claim response on deleted card");
    let ResponseDeliveryClaimOutcome::Owned(first) = first else {
        panic!("first owner must claim the deleted card")
    };
    sqlx::query(
        "UPDATE task_notification_response_delivery
         SET lease_expires_at = NOW() - INTERVAL '1 second'
         WHERE response_turn_key = $1",
    )
    .bind(&first_key)
    .execute(&pool)
    .await
    .expect("expire crashed original owner");

    let fallback_key = response_turn_key(44_461, "2026-07-12 06:00:01", Some(120_001));
    let replacement = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &fallback_key,
        Some(&fallback_key),
        Some("2026-07-12 06:00:01"),
        Some(120_000),
        Some(120_050),
        90_061,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("take over expired same-turn authority on replacement card");
    let ResponseDeliveryClaimOutcome::Owned(replacement) = replacement else {
        panic!("same turn must recover instead of waiting forever")
    };
    assert_eq!(replacement.card_message_id, 90_061);
    assert_eq!(replacement.response_generation(), 2);
    assert!(
        mark_task_response_sent(Some(&pool), &first).await.is_err(),
        "crashed owner's stale card/token cannot commit after takeover"
    );
    mark_task_response_sent(Some(&pool), &replacement)
        .await
        .expect("replacement-card owner records the accepted response");
    mark_task_response_delivered(Some(&pool), &replacement)
        .await
        .expect("replacement-card owner finalizes the exact response");
    let persisted_card: i64 = sqlx::query_scalar(
        "SELECT referenced_card_message_id
         FROM task_notification_response_delivery WHERE response_turn_key = $1",
    )
    .bind(&first_key)
    .fetch_one(&pool)
    .await
    .expect("load recovered response authority");
    assert_eq!(persisted_card, 90_061);
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

    let first_claim = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &first,
        Some(&first),
        Some("2026-07-11T02:37:00Z"),
        Some(10),
        Some(10_100),
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

    let second_claim = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &second,
        Some(&second),
        Some("2026-07-11T02:38:00Z"),
        Some(20),
        Some(20_100),
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
async fn active_first_turn_cannot_consume_a_distinct_later_sink_turn_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_response_active_sequential_4446",
        "active first turn does not consume a later turn",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let event = event("active-sequential-response-cycle");
    let card = ensure_card(
        Some(&pool),
        &clients(),
        &FakeTransport::new(),
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("confirm card");
    let first_key = response_turn_key(4_446, "2026-07-12 04:20:00", Some(70_000));
    let second_key = response_turn_key(4_447, "2026-07-12 04:20:00", Some(70_100));
    let first = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &first_key,
        Some(&first_key),
        Some("2026-07-12 04:20:00"),
        Some(70_000),
        Some(70_050),
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("production local timestamp is accepted");
    let ResponseDeliveryClaimOutcome::Owned(first) = first else {
        panic!("first turn owns its response fence")
    };
    mark_task_response_sent(Some(&pool), &first)
        .await
        .expect("record first response POST");

    let second_while_first_active = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &second_key,
        Some(&second_key),
        Some("2026-07-12 04:20:00"),
        Some(70_100),
        Some(70_150),
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("later turn observes the active event fence");
    assert!(
        matches!(
            second_while_first_active,
            ResponseDeliveryClaimOutcome::Wait
        ),
        "a distinct offset must wait, not inherit the first turn's sent state"
    );

    sqlx::query(
        "UPDATE task_notification_response_delivery
         SET lease_expires_at = NOW() - INTERVAL '1 second'
         WHERE response_turn_key = $1",
    )
    .bind(&first_key)
    .execute(&pool)
    .await
    .expect("expire first sent lease");
    let second_finalizes_expired_first =
        claim_task_response_delivery_with_recovery_key_and_started_at(
            Some(&pool),
            event.scope.channel_id,
            &event.scope.provider,
            &event.scope.session_key,
            event.event_key(),
            &second_key,
            Some(&second_key),
            Some("2026-07-12 04:20:00"),
            Some(70_100),
            Some(70_150),
            card.message_id,
            ResponseDeliveryOwner::Sink,
        )
        .await
        .expect("later turn safely finalizes the expired sent predecessor");
    assert!(matches!(
        second_finalizes_expired_first,
        ResponseDeliveryClaimOutcome::Wait
    ));

    let second = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &second_key,
        Some(&second_key),
        Some("2026-07-12 04:20:00"),
        Some(70_100),
        Some(70_150),
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("claim distinct second response after first finalizes");
    assert!(matches!(second, ResponseDeliveryClaimOutcome::Owned(_)));
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_notification_response_delivery WHERE event_key = $1",
    )
    .bind(event.event_key())
    .fetch_one(&pool)
    .await
    .expect("count sequential response rows");
    assert_eq!(
        rows, 2,
        "both logical turns retain separate durable authority"
    );
}

#[tokio::test]
async fn rotated_turn_cannot_consume_a_later_reused_numeric_start_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_response_rotation_epoch_4446",
        "rotated tombstone cannot alias a later numeric start",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let event = event("rotation-reused-start-response-cycle");
    let card = ensure_card(
        Some(&pool),
        &clients(),
        &FakeTransport::new(),
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("confirm card");
    let rotated_key = response_turn_key(4_446, "2026-07-12 04:25:00", Some(20_000_000));
    let rotated = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &rotated_key,
        Some(&rotated_key),
        Some("2026-07-12 04:25:00"),
        Some(20_000_000),
        Some(15_100_000),
        card.message_id,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("claim the turn that crossed a head rotation");
    let ResponseDeliveryClaimOutcome::Owned(rotated) = rotated else {
        panic!("rotated turn owns its response fence")
    };
    mark_task_response_delivered(Some(&pool), &rotated)
        .await
        .expect("deliver rotated turn");

    let later_key = response_turn_key(4_447, "2026-07-12 04:25:01", Some(20_000_000));
    let later = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &later_key,
        Some(&later_key),
        Some("2026-07-12 04:25:01"),
        Some(20_000_000),
        Some(20_100_000),
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("later turn compares in the current coordinate space");
    assert!(
        matches!(later, ResponseDeliveryClaimOutcome::Owned(_)),
        "a later turn reusing the old numeric start must retain POST authority"
    );
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_notification_response_delivery WHERE event_key = $1",
    )
    .bind(event.event_key())
    .fetch_one(&pool)
    .await
    .expect("count rotation-separated response rows");
    assert_eq!(rows, 2);
}

#[tokio::test]
async fn offset_null_turns_use_exact_consumed_end_instead_of_delivery_time_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_response_legacy_sequential_4446",
        "offset-null turns use the consumed end coordinate",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let event = event("legacy-overlapping-response-cycle");
    let card = ensure_card(
        Some(&pool),
        &clients(),
        &FakeTransport::new(),
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("confirm card");
    let first_key = fallback_response_turn_key(
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        80_000,
        "first legacy response",
    );
    let first = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &first_key,
        Some(&first_key),
        Some("2026-07-12 04:20:00"),
        None,
        Some(80_000),
        card.message_id,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("claim first legacy response");
    let ResponseDeliveryClaimOutcome::Owned(first) = first else {
        panic!("first legacy response owns its fence")
    };
    mark_task_response_delivered(Some(&pool), &first)
        .await
        .expect("deliver first response");
    let second_key = fallback_response_turn_key(
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        80_100,
        "second legacy response",
    );
    let second = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &second_key,
        Some(&second_key),
        Some("2026-07-12 04:20:00"),
        None,
        Some(80_100),
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("claim second legacy response");
    assert!(
        matches!(second, ResponseDeliveryClaimOutcome::Owned(_)),
        "distinct consumed ends must preserve same-second sequential turns"
    );
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_notification_response_delivery WHERE event_key = $1",
    )
    .bind(event.event_key())
    .fetch_one(&pool)
    .await
    .expect("count overlapping legacy response rows");
    assert_eq!(rows, 2);
}

#[tokio::test]
async fn divergent_turn_without_monotonic_coordinates_quarantines_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_response_unknown_coordinates_4446",
        "unknown divergent turn coordinates fail closed",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let event = event("unknown-coordinate-response-cycle");
    let card = ensure_card(
        Some(&pool),
        &clients(),
        &FakeTransport::new(),
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .expect("confirm card");
    let first_key = fallback_response_turn_key(
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        0,
        "first unknown-coordinate response",
    );
    let first = claim_task_response_delivery(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &first_key,
        card.message_id,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("claim first response");
    let ResponseDeliveryClaimOutcome::Owned(first) = first else {
        panic!("first response owns its fence")
    };
    mark_task_response_delivered(Some(&pool), &first)
        .await
        .expect("deliver first response");

    let second_key = fallback_response_turn_key(
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        1,
        "divergent unknown-coordinate response",
    );
    let second = claim_task_response_delivery_with_recovery_key_and_started_at(
        Some(&pool),
        event.scope.channel_id,
        &event.scope.provider,
        &event.scope.session_key,
        event.event_key(),
        &second_key,
        Some(&second_key),
        None,
        None,
        None,
        card.message_id,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("unknown coordinates fail closed");
    assert!(matches!(second, ResponseDeliveryClaimOutcome::Wait));
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_notification_response_delivery WHERE event_key = $1",
    )
    .bind(event.event_key())
    .fetch_one(&pool)
    .await
    .expect("count quarantined rows");
    assert_eq!(rows, 1, "quarantine must delete the provisional authority");
}

#[tokio::test]
async fn memory_delivered_reconciliation_respects_channel_provider_and_session() {
    let event_key = "shared-memory-event-key";
    let first_key = response_turn_key(44_460, "2026-07-12T04:40:00Z", None);
    let first = claim_task_response_delivery_with_recovery_key_and_started_at(
        None,
        44_460,
        "claude",
        "AgentDesk-claude-memory-scope-a",
        event_key,
        &first_key,
        Some(&first_key),
        Some("2026-07-12T04:40:00Z"),
        None,
        Some(100),
        99_001,
        ResponseDeliveryOwner::Watcher,
    )
    .await
    .expect("claim first memory-scoped response");
    let ResponseDeliveryClaimOutcome::Owned(first) = first else {
        panic!("first memory scope owns its response")
    };
    mark_task_response_delivered(None, &first)
        .await
        .expect("deliver first memory-scoped response");

    let second_key = response_turn_key(44_461, "2026-07-12T04:40:00Z", None);
    let second = claim_task_response_delivery_with_recovery_key_and_started_at(
        None,
        44_460,
        "claude",
        "AgentDesk-claude-memory-scope-b",
        event_key,
        &second_key,
        Some(&second_key),
        Some("2026-07-12T04:40:00Z"),
        None,
        Some(100),
        99_001,
        ResponseDeliveryOwner::Sink,
    )
    .await
    .expect("claim unrelated memory-scoped response");
    assert!(
        matches!(second, ResponseDeliveryClaimOutcome::Owned(_)),
        "matching event/card/coordinates in another scope cannot look delivered"
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
async fn old_restart_after_card_post_reconciles_history_without_nonce_replay_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_card_restart_4055",
        "task notification card old restart history recovery",
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
    let boundary = store::begin_card_post(Some(&pool), &claimed)
        .await
        .expect("persist card POST boundary before Discord call");
    assert!(!boundary.resumed);
    let bot = clients.by_key("notify").expect("notify test bot");
    let original_id = transport
        .post_card(bot, event.scope.channel_id, &seed, &claimed.discord_nonce)
        .await
        .expect("Discord accepted create before worker crash");
    sqlx::query(
        "UPDATE task_notification_card_state
         SET lease_expires_at = NOW() - INTERVAL '1 second',
             post_started_at = NOW() - INTERVAL '10 minutes'
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
    .expect("old stale lease reconciles Discord history without nonce replay");
    assert_eq!(recovered.message_id, original_id);
    assert_eq!(
        transport.post_calls.load(Ordering::Acquire),
        1,
        "POST outside the bounded nonce window must reconcile history instead of replaying"
    );
    assert_eq!(transport.physical_posts.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn old_card_post_boundary_without_history_fails_closed_without_repost_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_task_card_old_ambiguous_4055",
        "old task card POST boundary without history proof",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;
    let transport = FakeTransport::new();
    let clients = clients();
    let event = event("postgres-old-ambiguous-card");
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
    .expect("claim card before ambiguous boundary");
    let CardClaim::Owned(claimed) = claim else {
        panic!("first worker must own the post lease");
    };
    store::begin_card_post(Some(&pool), &claimed)
        .await
        .expect("persist ambiguous network boundary");
    sqlx::query(
        "UPDATE task_notification_card_state
         SET lease_expires_at = NOW() - INTERVAL '1 second',
             post_started_at = NOW() - INTERVAL '10 minutes'
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4",
    )
    .bind(i64::try_from(event.scope.channel_id).expect("test channel id"))
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .bind(&event.scope.event_key)
    .execute(&pool)
    .await
    .expect("age ambiguous card boundary");

    let result = ensure_card(
        Some(&pool),
        &clients,
        &transport,
        &event,
        EnsureIntent::Promotion,
    )
    .await;
    assert!(matches!(result, Err(CardEnsureError::Ambiguous(_))));
    assert_eq!(transport.post_calls.load(Ordering::Acquire), 0);
    let persisted_boundary: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT post_started_at FROM task_notification_card_state
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4",
    )
    .bind(i64::try_from(event.scope.channel_id).expect("test channel id"))
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .bind(&event.scope.event_key)
    .fetch_one(&pool)
    .await
    .expect("load quarantined card boundary");
    assert!(persisted_boundary.is_some());
    sqlx::query(
        "UPDATE task_notification_card_state
         SET updated_at = NOW() - INTERVAL '8 days'
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4",
    )
    .bind(i64::try_from(event.scope.channel_id).expect("test channel id"))
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .bind(&event.scope.event_key)
    .execute(&pool)
    .await
    .expect("age quarantined card row");
    store::cleanup_old_rows_pg_checked(&pool)
        .await
        .expect("run bounded retention cleanup");
    let retained: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_notification_card_state
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4",
    )
    .bind(i64::try_from(event.scope.channel_id).expect("test channel id"))
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .bind(&event.scope.event_key)
    .fetch_one(&pool)
    .await
    .expect("count quarantined card row");
    assert_eq!(
        retained, 1,
        "ambiguous POST authority must outlive ordinary retention cleanup"
    );
}
