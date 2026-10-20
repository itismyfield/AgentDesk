use super::*;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Transport {
    posts: Mutex<Vec<String>>,
    edits: Mutex<Vec<(u64, String)>>,
}

impl TaskCardTransport for Transport {
    async fn post_card(
        &self,
        _: &CardBot,
        _: u64,
        content: &str,
        _: &str,
    ) -> Result<u64, TaskCardTransportError> {
        self.posts.lock().unwrap().push(content.to_string());
        Ok(581_601)
    }

    async fn edit_card(
        &self,
        _: &CardBot,
        _: u64,
        message_id: u64,
        content: &str,
    ) -> Result<(), TaskCardTransportError> {
        self.edits
            .lock()
            .unwrap()
            .push((message_id, content.to_string()));
        Ok(())
    }
}

fn event(channel: u64) -> TaskCardEvent {
    TaskCardEvent::from_task_prompt(
        channel,
        "claude",
        "fixture-task-card",
        &format!(
            "<task-notification><task-id>footer-{channel}</task-id><status>completed</status><summary>Task completed</summary><result>Result retained.</result></task-notification>"
        ),
    )
}

fn clients() -> CardDeliveryClients {
    CardDeliveryClients::new([CardBot::new(
        "notify",
        Arc::new(super::super::serenity::Http::new("test-token")),
    )])
}

fn assert_no_metadata(content: &str) {
    for prefix in ["-# ⏱", "-# 📦", "-# ⏳", "-# 🖥️"] {
        assert!(
            !content.contains(prefix),
            "retired metadata prefix {prefix}: {content}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn shared_task_card_create_sends_no_completion_metadata_despite_terminal_context() {
    let _isolation = super::super::turn_end_summary::tests::Isolation::new();
    let shared = super::super::make_shared_data_for_tests();
    let event = event(581_611);
    let mut state = super::super::turn_end_summary::tests::row(581_611, 1, "task-card");
    state.tmux_session_name = Some(event.scope.session_key.clone());
    state.started_at = "2026-01-01 00:00:00".to_string();
    super::super::inflight::save_inflight_state(&state).unwrap();
    shared.ui.placeholder_live_events.set_context_panel_usage(
        super::super::ChannelId::new(581_611),
        None,
        100,
        200,
        150,
        1000,
        80,
    );
    let transport = Transport::default();
    let outcome = ensure_card_with_shared(
        &shared,
        &clients(),
        &transport,
        &event,
        EnsureIntent::Observation,
    )
    .await
    .unwrap();
    assert_eq!(outcome.disposition, CardDisposition::Created);
    let posts = transport.posts.lock().unwrap();
    assert_eq!(*posts, [event.payload.render(1)]);
    assert_no_metadata(&posts[0]);
    assert!(transport.edits.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn shared_task_card_refresh_removes_old_metadata_with_same_card_and_content_hash() {
    let _isolation = super::super::turn_end_summary::tests::Isolation::new();
    let shared = super::super::make_shared_data_for_tests();
    let mut event = event(581_612);
    // An updateable card keeps its existing edit authority; terminal replay rules stay intact.
    event.scope.terminal_delivery_fingerprint = None;
    let metadata =
        super::super::completion_footer_metadata::tests::metadata_fixture_for_task_card_4806();
    let transport = Transport::default();
    let clients = clients();
    let old = ensure_card_with_metadata(
        None,
        &clients,
        &transport,
        &event,
        EnsureIntent::Observation,
        Some(&metadata),
    )
    .await
    .unwrap();
    let old_content = transport.posts.lock().unwrap()[0].clone();
    assert!(old_content.contains("-# ⏳"));
    assert!(old_content.contains("-# 🖥️"));
    let refreshed = ensure_card_with_shared(
        &shared,
        &clients,
        &transport,
        &event,
        EnsureIntent::Observation,
    )
    .await
    .unwrap();
    assert_eq!(refreshed.disposition, CardDisposition::Edited);
    assert_eq!(refreshed.message_id, old.message_id);
    let edits = transport.edits.lock().unwrap().clone();
    assert_eq!(edits, [(old.message_id, event.payload.render(2))]);
    assert_no_metadata(&edits[0].1);
    assert_ne!(content_hash(&old_content), content_hash(&edits[0].1));
    let existing = ensure_card_with_shared(
        &shared,
        &clients,
        &transport,
        &event,
        EnsureIntent::Promotion,
    )
    .await
    .unwrap();
    assert_eq!(existing.disposition, CardDisposition::Existing);
    assert_eq!(existing.message_id, old.message_id);
    assert_eq!(transport.posts.lock().unwrap().len(), 1);
    assert_eq!(transport.edits.lock().unwrap().len(), 1);
}
