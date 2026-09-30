//! A Claude prompt announced from its `UserPromptSubmit` hook, through the real
//! relay and a mock Discord: the idle scanner's later row is matched by
//! `prompt_id`, and an announcement Discord certainly never got leaves no match.

use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request};
use tokio::sync::broadcast;
use tower::ServiceExt;

use super::discord_mock::NoteAnswer;
use super::{PROVIDER_KEY, RelayE2eHarness, wait_until};
use crate::services::claude_tui::hook_server::{
    HookEvent, HookEventKind, HookServerState, hook_receiver_router_with_state,
};
use crate::services::tui_prompt_dedupe as dedupe;

const PROMPT: &str = "턴 도중에 입력한 질문";
const PROMPT_ID: &str = "7d4a7a0e-d052-488b-9a89-d0f3aac426bb";
const ROW_UUID: &str = "5845e2e0-0000-0000-0000-00000000a001";
const QUIET: Duration = Duration::from_millis(1500);
const WAIT: Duration = Duration::from_secs(10);

/// Boots the relay over the mock and runs the production observer loop and relay on `hooks`.
async fn start(
    tmux: &str,
    sessions: &[&str],
    hooks: broadcast::Receiver<HookEvent>,
    notify_timeout: Duration,
) -> RelayE2eHarness {
    let harness = RelayE2eHarness::start_with_health_registry().await;
    harness.cache_relay_transport();
    harness.answer_placeholders_immediately();
    harness.use_mock_notify_bot(notify_timeout).await;
    harness.attach_tmux_watcher(tmux, "prompt-identity.jsonl");
    for session in sessions {
        dedupe::register_provider_session(PROVIDER_KEY, session, tmux);
    }
    let shared = harness.shared.clone();
    super::super::spawn_tui_prompt_relay_observer(PROVIDER_KEY.to_string(), hooks, move |prompt| {
        let shared = shared.clone();
        Box::pin(async move { super::super::relay_observed_prompt(&shared, prompt).await })
    });
    harness
}

fn user_prompt_submit_payload(session: &str) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": session,
        "prompt": PROMPT,
        "prompt_id": PROMPT_ID,
    })
}

fn hook_event(session: &str) -> HookEvent {
    HookEvent {
        provider: PROVIDER_KEY.to_string(),
        session_id: session.to_string(),
        kind: HookEventKind::UserPromptSubmit,
        received_at: chrono::Utc::now(),
        payload: user_prompt_submit_payload(session),
    }
}

/// Announcements Discord created (`...` placeholders are counted apart).
fn announcements(harness: &RelayE2eHarness) -> usize {
    harness
        .messages()
        .iter()
        .filter(|(_, content)| content != "..." && content.contains(PROMPT))
        .count()
}

async fn wait_for_announcement(harness: &RelayE2eHarness) {
    let messages = harness.mock.messages.clone();
    let announced = wait_until(WAIT, move || {
        let messages = messages.clone();
        Box::pin(async move {
            messages
                .lock()
                .expect("mock messages")
                .values()
                .any(|(_, content)| content != "..." && content.contains(PROMPT))
        })
    })
    .await;
    assert!(
        announced,
        "the prompt was never announced; messages={:?} unhandled={:?}",
        harness.messages(),
        harness.unhandled_requests()
    );
    assert!(harness.wait_for_placeholder_posts(1, WAIT).await);
}

/// Returns once the relay has POSTed `attempts` announcements and dropped its lease.
async fn wait_for_failed_announcement(harness: &RelayE2eHarness, tmux: &str, attempts: usize) {
    let posts = harness.mock.local_note_posts.clone();
    let posted = wait_until(WAIT, move || {
        let posts = posts.clone();
        Box::pin(async move { posts.load(Ordering::SeqCst) >= attempts })
    })
    .await;
    assert!(posted, "the announcement POST never reached the mock");
    let tmux = tmux.to_string();
    let released = wait_until(WAIT, move || {
        let released =
            !dedupe::external_input_relay_lease_present(PROVIDER_KEY, &tmux, super::CHANNEL_ID);
        Box::pin(async move { released })
    })
    .await;
    assert!(released, "the failed relay kept its lease");
}

/// The idle scanner's call for the prompt's transcript row, after the 30s content window.
fn scanner_sees_the_row(tmux: &str) -> dedupe::PromptObservation {
    dedupe::age_observed_prompt_records_for_tests(PROVIDER_KEY, tmux, Duration::from_secs(31));
    dedupe::observe_prompt_by_tmux_with_row_ids_at(
        PROVIDER_KEY,
        tmux,
        PROMPT,
        Some(ROW_UUID),
        Some(PROMPT_ID),
        chrono::Utc::now(),
    )
}

/// After a quiet window: POSTs made, announcements created, `...` placeholders.
async fn settled_counts(harness: &RelayE2eHarness) -> (usize, usize, usize) {
    tokio::time::sleep(QUIET).await;
    let unhandled = harness.unhandled_requests();
    assert!(
        unhandled.is_empty(),
        "mock Discord swallowed calls: {unhandled:?}"
    );
    (
        harness.local_note_posts(),
        announcements(harness),
        harness.placeholder_posts(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_announced_prompt_is_not_reannounced_by_the_idle_scanner() {
    let tmux = "AgentDesk-claude-5845-hook-then-scan";
    let session = "5845e2e0-0000-0000-0000-0000000000c1";
    let hooks = HookServerState::new();
    let harness = start(tmux, &[session], hooks.subscribe(), WAIT).await;
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!(
            "/hooks/claude/UserPromptSubmit?session_id={session}"
        ))
        .header("content-type", "application/json")
        .body(Body::from(user_prompt_submit_payload(session).to_string()))
        .expect("hook request");
    let response = hook_receiver_router_with_state(hooks.clone())
        .oneshot(request)
        .await
        .expect("hook response");
    assert!(response.status().is_success(), "{}", response.status());

    wait_for_announcement(&harness).await;
    assert_eq!(
        scanner_sees_the_row(tmux),
        dedupe::PromptObservation::SuppressedReplayedEntry
    );
    assert_eq!(settled_counts(&harness).await, (1, 1, 1));
    drop(hooks);
}

/// The hook server's alias fan-out: one hook, re-sent under the second registered session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aliased_hook_broadcast_twice_is_announced_once() {
    let tmux = "AgentDesk-claude-5845-aliased-hook";
    let command = "5845e2e0-0000-0000-0000-0000000000c2";
    let payload = "5845e2e0-0000-0000-0000-0000000000c3";
    let (hook_tx, hook_rx) = broadcast::channel(8);
    let harness = start(tmux, &[command, payload], hook_rx, WAIT).await;
    let event = HookEvent {
        payload: user_prompt_submit_payload(payload),
        ..hook_event(command)
    };
    let alias = HookEvent {
        session_id: payload.to_string(),
        ..event.clone()
    };
    hook_tx.send(event).expect("observer subscribed");
    hook_tx.send(alias).expect("observer subscribed");

    wait_for_announcement(&harness).await;
    assert_eq!(
        scanner_sees_the_row(tmux),
        dedupe::PromptObservation::SuppressedReplayedEntry
    );
    assert_eq!(settled_counts(&harness).await, (1, 1, 1));
    drop(hook_tx);
}

/// Discord refused the first announcement, so the scanner's row 31s later is announced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_announcement_leaves_the_prompt_to_the_idle_scanner() {
    let tmux = "AgentDesk-claude-5845-refused-hook";
    let session = "5845e2e0-0000-0000-0000-0000000000c4";
    let (hook_tx, hook_rx) = broadcast::channel(8);
    let harness = start(tmux, &[session], hook_rx, WAIT).await;
    harness.answer_notes_with(NoteAnswer::Refuse);
    hook_tx
        .send(hook_event(session))
        .expect("observer subscribed");
    wait_for_failed_announcement(&harness, tmux, 1).await;

    harness.answer_notes_with(NoteAnswer::Create);
    assert_eq!(
        scanner_sees_the_row(tmux),
        dedupe::PromptObservation::PublishedSshDirect
    );
    wait_for_announcement(&harness).await;
    assert_eq!(settled_counts(&harness).await, (2, 1, 1));
    drop(hook_tx);
}

/// A timed-out announcement may have been created, so the scanner's row stays suppressed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timed_out_announcement_keeps_the_scanner_row_suppressed() {
    let tmux = "AgentDesk-claude-5845-timed-out-hook";
    let session = "5845e2e0-0000-0000-0000-0000000000c5";
    let (hook_tx, hook_rx) = broadcast::channel(8);
    let harness = start(tmux, &[session], hook_rx, Duration::from_millis(300)).await;
    harness.answer_notes_with(NoteAnswer::Stall);
    hook_tx
        .send(hook_event(session))
        .expect("observer subscribed");
    wait_for_failed_announcement(&harness, tmux, 1).await;

    assert_eq!(
        scanner_sees_the_row(tmux),
        dedupe::PromptObservation::SuppressedReplayedEntry
    );
    let (posts, _, placeholders) = settled_counts(&harness).await;
    assert_eq!((posts, placeholders), (1, 0));
    drop(hook_tx);
}
