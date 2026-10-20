use super::*;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Request {
    Post(String, String),
    Edit(u64, String),
}

#[derive(Default)]
struct Transport {
    calls: Mutex<Vec<Request>>,
    messages: Mutex<std::collections::BTreeMap<String, (u64, String)>>,
    next: AtomicU64,
    fail_edit: AtomicBool,
}

impl TaskCardTransport for Transport {
    async fn post_card(
        &self,
        _: &CardBot,
        _: u64,
        content: &str,
        nonce: &str,
    ) -> Result<u64, TaskCardTransportError> {
        self.calls
            .lock()
            .unwrap()
            .push(Request::Post(nonce.into(), content.into()));
        let mut messages = self.messages.lock().unwrap();
        if let Some((id, original)) = messages.get(nonce) {
            assert_eq!(
                content, original,
                "pending nonce must retain the exact original POST bytes"
            );
            return Ok(*id);
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 581_900;
        messages.insert(nonce.into(), (id, content.into()));
        Ok(id)
    }

    async fn edit_card(
        &self,
        _: &CardBot,
        _: u64,
        id: u64,
        content: &str,
    ) -> Result<(), TaskCardTransportError> {
        self.calls
            .lock()
            .unwrap()
            .push(Request::Edit(id, content.into()));
        if self.fail_edit.swap(false, Ordering::SeqCst) {
            return Err(TaskCardTransportError::Transient(
                "fixture rejects cleanup PATCH".into(),
            ));
        }
        let mut messages = self.messages.lock().unwrap();
        let (_, text) = messages
            .values_mut()
            .find(|(message_id, _)| *message_id == id)
            .unwrap();
        *text = content.into();
        Ok(())
    }
}

fn event(channel: u64) -> TaskCardEvent {
    TaskCardEvent::from_task_prompt_with_source_event_id(
        channel,
        "claude",
        "metadata-replay-fixture",
        &format!(
            "<task-notification><task-id>retire-{channel}</task-id><status>completed</status><summary>Task complete</summary><result>Original result retained.</result></task-notification>"
        ),
        Some(&format!("metadata-source-{channel}")),
    )
}

fn clients() -> CardDeliveryClients {
    CardDeliveryClients::new([CardBot::new(
        "notify",
        std::sync::Arc::new(crate::services::discord::serenity::Http::new("test-token")),
    )])
}

fn metadata() -> crate::services::discord::completion_footer_metadata::CompletionFooterMetadata {
    crate::services::discord::completion_footer_metadata::completion_footer_metadata_from_block(
        Some(
            "\n\u{2063}\n⏱ 2m 34s\n📦 265.0k / 1.0M (26%) · auto-compact 80%\n⏳ 5h: 3% (4h39m)\n🖥️ node-a",
        ),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn missing_old_metadata_card_posts_a_clean_new_nonce_before_any_network_attempt() {
    let event = event(581_921);
    let transport = Transport::default();
    let clients = clients();
    let old = ensure_card_with_metadata(
        None,
        &clients,
        &transport,
        &event,
        EnsureIntent::Observation,
        Some(&metadata()),
    )
    .await
    .unwrap();
    let rich = event
        .payload
        .render_with_completion_metadata(1, &metadata());
    let clean = event.payload.render(1);
    assert_ne!(rich, clean);
    for prefix in ["-# ⏱ ", "-# 📦 ", "-# ⏳ ", "-# 🖥️ "] {
        assert!(
            rich.contains(prefix),
            "fixture must contain known metadata {prefix}"
        );
        assert!(
            !clean.contains(prefix),
            "clean task body cannot already contain metadata {prefix}"
        );
    }
    let replacement =
        replace_confirmed_missing_card(None, &clients, &transport, &event, old.message_id)
            .await
            .unwrap();
    let calls = transport.calls.lock().unwrap().clone();
    let [
        Request::Post(first_nonce, first),
        Request::Post(next_nonce, second),
    ] = calls.as_slice()
    else {
        panic!("fresh replacement must post exactly one clean card: {calls:?}");
    };
    assert_eq!(first, &rich);
    assert_ne!(first_nonce, next_nonce);
    assert_eq!(
        second, &clean,
        "fresh replacement nonce must start with metadata-free durable bytes"
    );
    assert_ne!(replacement.message_id, old.message_id);
    assert_eq!(transport.messages.lock().unwrap().len(), 2);
    let replay = replace_confirmed_missing_card(None, &clients, &transport, &event, old.message_id)
        .await
        .unwrap();
    assert_eq!(replay.message_id, replacement.message_id);
    assert_eq!(
        transport.calls.lock().unwrap().len(),
        2,
        "stale missing receipt cannot recreate twice"
    );
}

async fn pending(
    event: &TaskCardEvent,
    transport: &Transport,
    clients: &CardDeliveryClients,
) -> (String, String, u64) {
    let rich = event
        .payload
        .render_with_completion_metadata(1, &metadata());
    let claim = store::claim_card(
        None,
        &event.scope,
        "notify",
        &rich,
        &content_hash(&rich),
        StoreIntent::Observation,
    )
    .await
    .unwrap();
    let CardClaim::Owned(claim) = claim else {
        panic!("fixture must own original pending card")
    };
    assert!(!store::begin_card_post(None, &claim).await.unwrap().resumed);
    let id = transport
        .post_card(
            clients.preferred().unwrap(),
            event.scope.channel_id,
            &rich,
            &claim.discord_nonce,
        )
        .await
        .unwrap();
    store::mark_post_failure(None, &claim, "fixture lost DB response after accepted POST")
        .await
        .unwrap();
    (claim.discord_nonce, rich, id)
}

#[tokio::test(flavor = "current_thread")]
async fn old_pending_nonce_replays_identical_bytes_then_cleans_only_its_known_receipt() {
    let _isolation = crate::services::discord::turn_end_summary::tests::Isolation::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let event = event(581_922);
    let transport = Transport::default();
    let clients = clients();
    let (nonce, rich, id) = pending(&event, &transport, &clients).await;
    let outcome = super::super::ensure_card_with_shared(
        &shared,
        &clients,
        &transport,
        &event,
        EnsureIntent::Observation,
    )
    .await
    .unwrap();
    assert_eq!(outcome.message_id, id);
    let clean = event.payload.render(1);
    assert_eq!(
        *transport.calls.lock().unwrap(),
        [
            Request::Post(nonce.clone(), rich.clone()),
            Request::Post(nonce, rich),
            Request::Edit(id, clean.clone()),
        ],
        "old nonce replay must converge through one known-receipt metadata PATCH"
    );
    assert_eq!(
        transport.messages.lock().unwrap().len(),
        1,
        "receipt cleanup must not create a second physical card"
    );
    assert_eq!(
        transport
            .messages
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .1,
        clean
    );
    super::super::ensure_card_with_shared(
        &shared,
        &clients,
        &transport,
        &event,
        EnsureIntent::Observation,
    )
    .await
    .unwrap();
    assert_eq!(
        transport.calls.lock().unwrap().len(),
        3,
        "clean exact terminal replay is a no-op"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn failed_metadata_patch_keeps_known_card_and_retries_without_another_post() {
    let _isolation = crate::services::discord::turn_end_summary::tests::Isolation::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let event = event(581_923);
    let transport = Transport::default();
    let clients = clients();
    let (_, _, id) = pending(&event, &transport, &clients).await;
    transport.fail_edit.store(true, Ordering::SeqCst);
    let first = super::super::ensure_card_with_shared(
        &shared,
        &clients,
        &transport,
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .unwrap();
    assert_eq!(
        first.message_id, id,
        "cosmetic PATCH failure cannot revoke an accepted card"
    );
    let second = super::super::ensure_card_with_shared(
        &shared,
        &clients,
        &transport,
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .unwrap();
    assert_eq!(second.message_id, id);
    let calls = transport.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|call| matches!(call, Request::Post(_, _)))
            .count(),
        2
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| matches!(call, Request::Edit(_, _)))
            .count(),
        2,
        "failed metadata PATCH must retry exactly once without losing its known receipt"
    );
    assert_eq!(
        transport
            .messages
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .1,
        event.payload.render(1)
    );
}

#[test]
fn metadata_projection_preserves_opaque_body_literals_and_structured_task_footer() {
    let mut event = event(581_924);
    let canonical = event.payload.render(1);
    let elapsed = "-# ⏱ 2m 34s";
    assert_eq!(
        without_metadata(&format!("{canonical}\n\n{elapsed}"), &event.payload, 1),
        Some(canonical.clone()),
        "canonical body proof removes elapsed-only completion metadata"
    );
    let opaque = format!("different opaque result\n\n{elapsed}");
    assert_eq!(
        without_metadata(&opaque, &event.payload, 1),
        None,
        "opaque elapsed-only provenance is ambiguous and must be preserved"
    );
    let rich = event
        .payload
        .render_with_completion_metadata(1, &metadata());
    let TaskCardPayload::Task(note) = &mut event.payload else {
        unreachable!()
    };
    note.result = Some("New result must not replace the durable original body.".into());
    assert_eq!(
        without_metadata(&rich, &event.payload, 1),
        None,
        "opaque old body has no proof that a metadata-looking suffix was appended"
    );
    let TaskCardPayload::Task(note) = &mut event.payload else {
        unreachable!()
    };
    note.task_id = None;
    note.duration = Some("2m 34s".into());
    let own_footer = event.payload.render(1);
    assert!(own_footer.ends_with(elapsed));
    assert_eq!(
        without_metadata(&own_footer, &event.payload, 1),
        None,
        "structured task duration remains part of the semantic card"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn opaque_structured_usage_survives_promotion_and_fresh_replacement() {
    for (index, usage) in [
        "⏳ 5h: 3% (4h39m)",
        "🖥️ node-a",
        "📦 265.0k / 1.0M (26%) · auto-compact 80%",
    ]
    .into_iter()
    .enumerate()
    {
        let mut event = TaskCardEvent::from_task_prompt(
            581_950 + index as u64,
            "claude",
            "opaque-task-usage",
            &format!(
                "<task-notification><tool-use-id>stable-{index}</tool-use-id><status>completed</status><summary>Old task body</summary><result>Literal -# ⏳ and 🖥️ remain semantic prose.</result><usage>{usage}</usage></task-notification>"
            ),
        );
        let clients = clients();
        let transport = Transport::default();
        let old_content = event.payload.render(1);
        assert!(old_content.ends_with(&format!("-# {usage}")));
        let old = ensure_card_with_metadata(
            None,
            &clients,
            &transport,
            &event,
            EnsureIntent::Observation,
            None,
        )
        .await
        .unwrap();
        let TaskCardPayload::Task(note) = &mut event.payload else {
            unreachable!()
        };
        note.summary = Some("Changed task body without old provenance".into());
        note.result = Some("Changed result must not overwrite the old receipt body.".into());
        assert_ne!(old_content, event.payload.render(1));
        let promoted = ensure_card_with_metadata(
            None,
            &clients,
            &transport,
            &event,
            EnsureIntent::Promotion,
            None,
        )
        .await
        .unwrap();
        assert_eq!(promoted.message_id, old.message_id);
        assert_eq!(
            transport.calls.lock().unwrap().len(),
            1,
            "opaque structured usage must not trigger a metadata cleanup PATCH"
        );
        let replacement =
            replace_confirmed_missing_card(None, &clients, &transport, &event, old.message_id)
                .await
                .unwrap();
        assert_ne!(replacement.message_id, old.message_id);
        let calls = transport.calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "opaque body preservation must not add a cleanup PATCH after replacement"
        );
        assert!(
            matches!(&calls[1], Request::Post(_, content) if content == &old_content),
            "opaque structured usage must survive the fresh replacement POST byte-for-byte"
        );
    }
}

#[path = "metadata_retirement_pg_tests.rs"]
mod pg;
