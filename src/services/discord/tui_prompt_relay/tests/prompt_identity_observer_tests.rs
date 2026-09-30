//! A hook-announced Claude prompt reaches the relay once through the production
//! observer loop, even after the idle scanner sees its row (`prompt_id` match).

use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request};
use tokio::sync::{broadcast, mpsc};
use tower::ServiceExt;

use super::*;
use crate::services::claude_tui::hook_server::{HookServerState, hook_receiver_router_with_state};
use crate::services::tui_prompt_dedupe as dedupe;

const PROVIDER: &str = "claude";
const PROMPT: &str = "턴 도중에 입력한 질문";
const PROMPT_ID: &str = "7d4a7a0e-d052-488b-9a89-d0f3aac426bb";
const ROW_UUID: &str = "5845e2e0-0000-0000-0000-00000000a001";

/// Runs the production observer loop on `hooks`; each `tmux` prompt it relays
/// (one notice and one `...` placeholder in production) lands on the receiver.
fn observe(
    tmux: &'static str,
    sessions: &[&str],
    hooks: broadcast::Receiver<HookEvent>,
) -> mpsc::UnboundedReceiver<ObservedTuiPrompt> {
    for session in sessions {
        dedupe::register_provider_session(PROVIDER, session, tmux);
    }
    let (relayed_tx, relayed_rx) = mpsc::unbounded_channel();
    spawn_tui_prompt_relay_observer(PROVIDER.to_string(), hooks, move |prompt| {
        if prompt.tmux_session_name == tmux {
            let _ = relayed_tx.send(prompt);
        }
        Box::pin(async {})
    });
    relayed_rx
}

fn user_prompt_submit_payload(session: &str) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": session,
        "prompt": PROMPT,
        "prompt_id": PROMPT_ID,
    })
}

async fn next_relayed(
    relayed: &mut mpsc::UnboundedReceiver<ObservedTuiPrompt>,
) -> ObservedTuiPrompt {
    tokio::time::timeout(Duration::from_secs(5), relayed.recv())
        .await
        .expect("the hook prompt was never relayed")
        .expect("observer alive")
}

/// The idle scanner's call for the prompt's transcript row, after the 30s content window.
fn scanner_sees_the_row(tmux: &str) -> dedupe::PromptObservation {
    dedupe::age_observed_prompt_records_for_tests(PROVIDER, tmux, Duration::from_secs(31));
    dedupe::observe_prompt_by_tmux_with_row_ids_at(
        PROVIDER,
        tmux,
        PROMPT,
        Some(ROW_UUID),
        Some(PROMPT_ID),
        chrono::Utc::now(),
    )
}

/// Nothing more reaches the relay within a window the loop drains well inside.
async fn assert_nothing_more_relayed(relayed: &mut mpsc::UnboundedReceiver<ObservedTuiPrompt>) {
    let extra = tokio::time::timeout(Duration::from_millis(500), relayed.recv()).await;
    assert!(
        extra.is_err(),
        "a second announcement reached the relay: {extra:?}"
    );
}

/// Serializes against dedupe-state resets without holding the guard across awaits.
fn with_dedupe_lock(scenario: impl std::future::Future<Output = ()>) {
    let _guard = dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(scenario);
}

#[test]
fn a_hook_announced_prompt_is_not_reannounced_by_the_idle_scanner() {
    with_dedupe_lock(hook_then_scanner());
}

async fn hook_then_scanner() {
    let tmux = "AgentDesk-claude-5845-hook-then-scan";
    let session = "5845e2e0-0000-0000-0000-0000000000c1";
    let hooks = HookServerState::new();
    let mut relayed = observe(tmux, &[session], hooks.subscribe());
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

    let announced = next_relayed(&mut relayed).await;
    assert_eq!(
        (announced.prompt.as_str(), announced.source_event_id),
        (PROMPT, None)
    );
    assert_eq!(
        scanner_sees_the_row(tmux),
        dedupe::PromptObservation::SuppressedReplayedEntry
    );
    assert_nothing_more_relayed(&mut relayed).await;
    // Held to here: dropping the server's sender ends the observer loop.
    drop(hooks);
}

/// The hook server's alias fan-out: one hook, re-sent under the second registered session.
#[test]
fn an_aliased_hook_broadcast_twice_is_announced_once() {
    with_dedupe_lock(aliased_hook());
}

async fn aliased_hook() {
    let tmux = "AgentDesk-claude-5845-aliased-hook";
    let command = "5845e2e0-0000-0000-0000-0000000000c2";
    let payload = "5845e2e0-0000-0000-0000-0000000000c3";
    let (hook_tx, hook_rx) = broadcast::channel(8);
    let mut relayed = observe(tmux, &[command, payload], hook_rx);
    let event = HookEvent {
        provider: PROVIDER.to_string(),
        session_id: command.to_string(),
        kind: HookEventKind::UserPromptSubmit,
        received_at: chrono::Utc::now(),
        payload: user_prompt_submit_payload(payload),
    };
    let alias = HookEvent {
        session_id: payload.to_string(),
        ..event.clone()
    };
    hook_tx.send(event).expect("observer subscribed");
    hook_tx.send(alias).expect("observer subscribed");

    let announced = next_relayed(&mut relayed).await;
    assert_eq!(announced.prompt, PROMPT);
    assert_nothing_more_relayed(&mut relayed).await;
    assert_eq!(
        scanner_sees_the_row(tmux),
        dedupe::PromptObservation::SuppressedReplayedEntry
    );
    assert_nothing_more_relayed(&mut relayed).await;
    drop(hook_tx);
}
