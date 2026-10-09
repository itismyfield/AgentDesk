//! Hook-announced Claude prompts through the real relay and a mock Discord: the
//! idle scanner's row matches by `prompt_id` only once the announcement was sent.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// What the relay can reach when the hook arrives; either can be restored later.
struct Setup {
    notify_timeout: Option<Duration>,
    owner: bool,
}

const READY: Setup = Setup {
    notify_timeout: Some(WAIT),
    owner: true,
};

/// Boots the relay over the mock and runs the production observer loop and relay on
/// `hooks`; the counter holds how many relays have returned.
async fn start(
    tmux: &str,
    sessions: &[&str],
    hooks: broadcast::Receiver<HookEvent>,
    setup: Setup,
) -> (RelayE2eHarness, Arc<AtomicUsize>) {
    start_with_probe(tmux, sessions, hooks, setup, None).await
}

async fn start_with_probe(
    tmux: &str,
    sessions: &[&str],
    hooks: broadcast::Receiver<HookEvent>,
    setup: Setup,
    probe: Option<Arc<super::super::HookObserverProbe>>,
) -> (RelayE2eHarness, Arc<AtomicUsize>) {
    let harness = RelayE2eHarness::start_with_health_registry().await;
    harness.cache_relay_transport();
    harness.answer_placeholders_immediately();
    if let Some(timeout) = setup.notify_timeout {
        harness.use_mock_notify_bot(timeout).await;
    }
    if setup.owner {
        harness.attach_tmux_watcher(tmux, "prompt-identity.jsonl");
    }
    for session in sessions {
        dedupe::register_provider_session(PROVIDER_KEY, session, tmux);
    }
    let relayed = observe_hooks(&harness, hooks, probe);
    (harness, relayed)
}

fn observe_hooks(
    harness: &RelayE2eHarness,
    hooks: broadcast::Receiver<HookEvent>,
    probe: Option<Arc<super::super::HookObserverProbe>>,
) -> Arc<AtomicUsize> {
    let shared = harness.shared.clone();
    let relayed = Arc::new(AtomicUsize::new(0));
    let counter = relayed.clone();
    super::super::spawn_tui_prompt_relay_observer_inner(
        PROVIDER_KEY.to_string(),
        hooks,
        move |prompt| {
            let shared = shared.clone();
            let counter = counter.clone();
            Box::pin(async move {
                super::super::relay_observed_prompt(&shared, prompt).await;
                counter.fetch_add(1, Ordering::SeqCst);
            })
        },
        probe,
    );
    relayed
}

async fn wait_for_relays(relayed: &Arc<AtomicUsize>, count: usize) {
    let relayed = relayed.clone();
    let done = wait_until(WAIT, move || {
        let done = relayed.load(Ordering::SeqCst) >= count;
        Box::pin(async move { done })
    })
    .await;
    assert!(done, "the relay never returned");
}

fn user_prompt_submit_payload(session: &str) -> serde_json::Value {
    prompt_submit_payload(session, PROMPT)
}

fn prompt_submit_payload(session: &str, prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": session,
        "prompt": prompt,
        "prompt_id": PROMPT_ID,
    })
}

/// Delivers a UserPromptSubmit through the hook server's HTTP route.
async fn post_hook(hooks: &HookServerState, session: &str, prompt: &str) {
    post_aliased_hook(hooks, session, session, prompt).await;
}

async fn post_aliased_hook(hooks: &HookServerState, command: &str, session: &str, prompt: &str) {
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!(
            "/hooks/claude/UserPromptSubmit?session_id={command}"
        ))
        .header("content-type", "application/json")
        .body(Body::from(
            prompt_submit_payload(session, prompt).to_string(),
        ))
        .expect("hook request");
    let response = hook_receiver_router_with_state(hooks.clone())
        .oneshot(request)
        .await
        .expect("hook response");
    assert!(response.status().is_success(), "{}", response.status());
}

fn hook_event(session: &str) -> HookEvent {
    HookEvent {
        provider: PROVIDER_KEY.to_string(),
        session_id: session.to_string(),
        kind: HookEventKind::UserPromptSubmit,
        received_at: chrono::Utc::now(),
        payload: user_prompt_submit_payload(session),
        fanout: None,
    }
}

/// Announcements Discord created (`...` placeholders are counted apart).
fn announcements(harness: &RelayE2eHarness) -> usize {
    announcements_of(harness, PROMPT)
}

fn announcements_of(harness: &RelayE2eHarness, prompt: &str) -> usize {
    harness
        .messages()
        .iter()
        .filter(|(_, content)| content != "..." && content.contains(prompt))
        .count()
}

async fn wait_for_announcement(harness: &RelayE2eHarness) {
    wait_for_announcement_of(harness, PROMPT, 1).await;
}

/// Waits for `prompt`'s announcement and the `placeholders`-th `...` placeholder.
async fn wait_for_announcement_of(harness: &RelayE2eHarness, prompt: &str, placeholders: usize) {
    let messages = harness.mock.messages.clone();
    let wanted = prompt.to_string();
    let announced = wait_until(WAIT, move || {
        let (messages, wanted) = (messages.clone(), wanted.clone());
        Box::pin(async move {
            messages
                .lock()
                .expect("mock messages")
                .values()
                .any(|(_, content)| content != "..." && content.contains(&wanted))
        })
    })
    .await;
    assert!(
        announced,
        "the prompt was never announced; messages={:?} unhandled={:?}",
        harness.messages(),
        harness.unhandled_requests()
    );
    assert!(harness.wait_for_placeholder_posts(placeholders, WAIT).await);
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
    scanner_reads_the_row_now(tmux)
}

fn scanner_reads_the_row_now(tmux: &str) -> dedupe::PromptObservation {
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
    let (harness, _) = start(tmux, &[session], hooks.subscribe(), READY).await;
    post_hook(&hooks, session, PROMPT).await;

    wait_for_announcement(&harness).await;
    assert_eq!(
        scanner_sees_the_row(tmux),
        dedupe::PromptObservation::SuppressedReplayedEntry
    );
    assert_eq!(settled_counts(&harness).await, (1, 1, 1));
    drop(hooks);
}

/// Separate queued submissions are announced even when they repeat the opening text;
/// only the opening prompt's late transcript row is suppressed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_queued_into_a_running_prompt_does_not_reannounce_its_opening() {
    const QUEUED: &str = "같은 턴에 큐로 넣은 다음 입력";
    let tmux = "AgentDesk-claude-echo-dup-queued";
    let session = "5845e2e0-0000-0000-0000-0000000000ca";
    let hooks = HookServerState::new();
    let (harness, relayed) = start(tmux, &[session], hooks.subscribe(), READY).await;
    post_hook(&hooks, session, PROMPT).await;
    wait_for_announcement(&harness).await;
    wait_for_relays(&relayed, 1).await;
    dedupe::age_observed_prompt_records_for_tests(PROVIDER_KEY, tmux, Duration::from_secs(31));
    post_hook(&hooks, session, QUEUED).await;
    wait_for_announcement_of(&harness, QUEUED, 2).await;
    wait_for_relays(&relayed, 2).await;
    dedupe::age_observed_prompt_records_for_tests(PROVIDER_KEY, tmux, Duration::from_secs(31));
    post_hook(&hooks, session, PROMPT).await;
    wait_for_announcement_of(&harness, PROMPT, 3).await;
    wait_for_relays(&relayed, 3).await;
    assert_eq!(
        announcements(&harness),
        2,
        "the third A submission is announced"
    );

    assert_eq!(
        scanner_sees_the_row(tmux),
        dedupe::PromptObservation::SuppressedReplayedEntry
    );
    assert_eq!(settled_counts(&harness).await, (3, 2, 3));
    assert_eq!(announcements_of(&harness, QUEUED), 1);
    drop(hooks);
}

/// The hook server's alias fan-out: one hook, re-sent under the second registered session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aliased_hook_broadcast_twice_is_announced_once() {
    let tmux = "AgentDesk-claude-5845-aliased-hook";
    let command = "5845e2e0-0000-0000-0000-0000000000c2";
    let payload = "5845e2e0-0000-0000-0000-0000000000c3";
    let hooks = HookServerState::new();
    let probe = Arc::new(super::super::HookObserverProbe::default());
    probe.pause_after_first.store(true, Ordering::SeqCst);
    let (harness, relayed) = start_with_probe(
        tmux,
        &[command, payload],
        hooks.subscribe(),
        READY,
        Some(probe.clone()),
    )
    .await;
    post_aliased_hook(&hooks, command, payload, PROMPT).await;
    wait_for_relays(&relayed, 1).await;
    wait_for_announcement(&harness).await;
    let barrier = probe.clone();
    assert!(
        wait_until(WAIT, move || {
            let paused = barrier.paused.load(Ordering::SeqCst);
            Box::pin(async move { paused })
        })
        .await,
        "hook recv branch never paused"
    );
    assert_eq!(probe.alias_dequeued.load(Ordering::SeqCst), 0);
    assert_eq!(probe.dequeued.load(Ordering::SeqCst), 1);
    assert_eq!(
        scanner_sees_the_row(tmux),
        dedupe::PromptObservation::SuppressedReplayedEntry
    );
    probe.pause_after_first.store(false, Ordering::SeqCst);
    probe.release.notify_one();
    let witness = probe.clone();
    assert!(
        wait_until(WAIT, move || {
            let done = witness.processed.load(Ordering::SeqCst) >= 2;
            Box::pin(async move { done })
        })
        .await,
        "alias never processed"
    );
    assert_eq!(probe.alias_dequeued.load(Ordering::SeqCst), 1);
    assert_eq!(probe.observation_calls.load(Ordering::SeqCst), 1);
    assert_eq!(settled_counts(&harness).await, (1, 1, 1));
    drop(hooks);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_http_alias_keeps_its_distinct_pane_channel() {
    let first = "AgentDesk-claude-echo-first-pane";
    let second = "AgentDesk-claude-echo-second-pane";
    let command = "echo-distinct-command";
    let payload = "echo-distinct-payload";
    let hooks = HookServerState::new();
    let (harness, relayed) = start(first, &[command], hooks.subscribe(), READY).await;
    let channel = poise::serenity_prelude::ChannelId::new(super::CHANNEL_ID + 10);
    harness.mock.allow_channel(channel.get());
    crate::services::discord::rebind_channel_session(
        &harness.shared,
        &crate::services::provider::ProviderKind::Claude,
        channel,
        harness.root.path().to_str().unwrap(),
        "echo-second-binding",
    )
    .await;
    let transcript = harness.root.path().join("second-pane.jsonl");
    std::fs::write(&transcript, "").unwrap();
    harness
        .shared
        .tmux_watchers
        .insert(channel, super::watcher_handle(second, &transcript));
    dedupe::register_provider_session(PROVIDER_KEY, payload, second);
    post_aliased_hook(&hooks, command, payload, PROMPT).await;
    wait_for_relays(&relayed, 2).await;
    let posts = harness.mock.channel_posts.lock().unwrap();
    for target in [super::CHANNEL_ID, channel.get()] {
        assert_eq!(
            posts
                .iter()
                .filter(|(id, text)| *id == target && text.contains(PROMPT))
                .count(),
            1,
            "the input announcement must reach channel {target}; posts={posts:?}"
        );
        assert_eq!(
            posts
                .iter()
                .filter(|(id, text)| *id == target && text == "...")
                .count(),
            1
        );
    }
    assert!(harness.unhandled_requests().is_empty());
    drop(hooks);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_http_alias_survives_an_explicit_primary_discard() {
    let tmux = "AgentDesk-claude-echo-primary-discard";
    let command = "echo-discard-command";
    let payload = "echo-discard-payload";
    let mut hooks = HookServerState::new();
    let probe = Arc::new(crate::services::claude_tui::hook_server::HookBroadcastProbe::default());
    hooks.broadcast_probe = Some(probe.clone());
    let harness = RelayE2eHarness::start_with_health_registry().await;
    harness.cache_relay_transport();
    harness.answer_placeholders_immediately();
    harness.use_mock_notify_bot(WAIT).await;
    harness.attach_tmux_watcher(tmux, "primary-discard.jsonl");
    dedupe::register_provider_session(PROVIDER_KEY, command, tmux);
    dedupe::register_provider_session(PROVIDER_KEY, payload, tmux);
    let request_hooks = hooks.clone();
    let request = tokio::spawn(async move {
        post_aliased_hook(&request_hooks, command, payload, PROMPT).await;
    });
    tokio::time::timeout(WAIT, probe.primary_sent.notified())
        .await
        .unwrap();
    assert!(probe.primary_discarded.load(Ordering::SeqCst));
    let relayed = observe_hooks(&harness, hooks.subscribe(), None);
    probe.release_alias.notify_one();
    request.await.unwrap();
    wait_for_relays(&relayed, 1).await;
    assert_eq!(settled_counts(&harness).await, (1, 1, 1));
    drop(hooks);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_primary_send_to_another_waiter_does_not_claim_observer_receipt() {
    let tmux = "AgentDesk-claude-echo-late-observer";
    let command = "echo-late-command";
    let payload = "echo-late-payload";
    let harness = RelayE2eHarness::start_with_health_registry().await;
    let mut hooks = HookServerState::new();
    let probe = Arc::new(crate::services::claude_tui::hook_server::HookBroadcastProbe::default());
    hooks.broadcast_probe = Some(probe.clone());
    let _waiter = hooks.subscribe();
    for session in [command, payload] {
        dedupe::register_provider_session(PROVIDER_KEY, session, tmux);
    }
    let request_hooks = hooks.clone();
    let request = tokio::spawn(async move {
        post_aliased_hook(&request_hooks, command, payload, PROMPT).await;
    });
    tokio::time::timeout(WAIT, probe.primary_sent.notified())
        .await
        .unwrap();
    assert!(!probe.primary_discarded.load(Ordering::SeqCst));
    let observer = Arc::new(super::super::HookObserverProbe::default());
    observe_hooks(&harness, hooks.subscribe(), Some(observer.clone()));
    probe.release_alias.notify_one();
    request.await.unwrap();
    let witness = observer.clone();
    assert!(
        wait_until(WAIT, move || {
            let done = witness.processed.load(Ordering::SeqCst) == 1;
            Box::pin(async move { done })
        })
        .await
    );
    assert_eq!(observer.observation_calls.load(Ordering::SeqCst), 0);
    assert_eq!(harness.local_note_posts(), 0);
    drop(hooks);
}

#[test]
fn an_unresolved_alias_is_not_a_same_pane_clone() {
    let mut alias = hook_event("echo-unresolved-alias");
    alias.fanout = Some(crate::services::claude_tui::hook_server::HookFanout {
        origin_session_id: "echo-unresolved-origin".to_string(),
        primary_discarded: false,
    });
    assert_eq!(
        super::super::hook_observation_target(&alias),
        Some(alias.session_id.clone())
    );
}

/// Discord refused the first announcement, so the scanner's row 31s later is announced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_announcement_leaves_the_prompt_to_the_idle_scanner() {
    let tmux = "AgentDesk-claude-5845-refused-hook";
    let session = "5845e2e0-0000-0000-0000-0000000000c4";
    let (hook_tx, hook_rx) = broadcast::channel(8);
    let (harness, _) = start(tmux, &[session], hook_rx, READY).await;
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
    let setup = Setup {
        notify_timeout: Some(Duration::from_millis(300)),
        ..READY
    };
    let (harness, _) = start(tmux, &[session], hook_rx, setup).await;
    harness.answer_notes_with(NoteAnswer::Stall);
    hook_tx
        .send(hook_event(session))
        .expect("observer subscribed");
    wait_for_failed_announcement(&harness, tmux, 1).await;

    assert_eq!(
        scanner_sees_the_row(tmux),
        dedupe::PromptObservation::SuppressedReplayedEntry
    );
    assert_eq!(settled_counts(&harness).await, (1, 1, 0));
    drop(hook_tx);
}

/// The scanner reads the row while the announcement POST is open; the 403 then leaves
/// neither the prompt_id nor a row uuid derived from it, so the row is announced later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_read_during_a_refused_announcement_is_announced_afterwards() {
    let tmux = "AgentDesk-claude-5845-held-refused-hook";
    let session = "5845e2e0-0000-0000-0000-0000000000c6";
    let (hook_tx, hook_rx) = broadcast::channel(8);
    let (harness, _) = start(tmux, &[session], hook_rx, READY).await;
    harness.answer_notes_with(NoteAnswer::Refuse);
    harness.hold_next_note();
    hook_tx
        .send(hook_event(session))
        .expect("observer subscribed");
    assert!(
        harness.wait_for_held_note(WAIT).await,
        "no announcement POST"
    );

    assert_eq!(
        scanner_reads_the_row_now(tmux),
        dedupe::PromptObservation::SuppressedRecentDuplicate
    );
    harness.release_held_note();
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

/// The scanner publishes the row 31s on while the hook's announcement POST is open, so the row
/// waits behind that relay; it is announced only when the hook's announcement was refused.
async fn a_row_queued_behind_an_open_announcement(tmux: &str, session: &str, answer: NoteAnswer) {
    let refused = matches!(answer, NoteAnswer::Refuse);
    let (hook_tx, hook_rx) = broadcast::channel(8);
    // A stalled POST outlives the client's timeout, so its result stays unknown.
    let setup = match answer {
        NoteAnswer::Stall => Setup {
            notify_timeout: Some(Duration::from_secs(2)),
            ..READY
        },
        _ => READY,
    };
    let (harness, relayed) = start(tmux, &[session], hook_rx, setup).await;
    harness.answer_notes_with(answer);
    harness.hold_next_note();
    hook_tx
        .send(hook_event(session))
        .expect("observer subscribed");
    assert!(
        harness.wait_for_held_note(WAIT).await,
        "no announcement POST"
    );
    assert_eq!(
        scanner_sees_the_row(tmux),
        dedupe::PromptObservation::PublishedSshDirect
    );
    if refused {
        // The row's own POST is held too, so it is created after the hook's was refused.
        harness.hold_next_note();
        harness.release_held_note();
        assert!(
            harness.wait_for_held_note(WAIT).await,
            "the row was not announced"
        );
        harness.answer_notes_with(NoteAnswer::Create);
    }
    harness.release_held_note();
    wait_for_relays(&relayed, 2).await;
    let expected = match answer {
        NoteAnswer::Refuse => (2, 1, 1),
        NoteAnswer::Create => (1, 1, 1),
        NoteAnswer::Stall => (1, 1, 0),
    };
    assert_eq!(settled_counts(&harness).await, expected);
    let lease = dedupe::external_input_relay_lease_present(PROVIDER_KEY, tmux, super::CHANNEL_ID);
    assert!(
        refused || !lease,
        "the dropped row keeps no lease of its own"
    );
    drop(hook_tx);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_queued_behind_a_created_announcement_is_not_announced_again() {
    a_row_queued_behind_an_open_announcement(
        "AgentDesk-claude-5845-held-created-hook",
        "5845e2e0-0000-0000-0000-0000000000cb",
        NoteAnswer::Create,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_queued_behind_a_timed_out_announcement_is_not_announced_again() {
    a_row_queued_behind_an_open_announcement(
        "AgentDesk-claude-5845-held-stalled-hook",
        "5845e2e0-0000-0000-0000-0000000000cd",
        NoteAnswer::Stall,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_queued_behind_a_refused_announcement_is_announced() {
    a_row_queued_behind_an_open_announcement(
        "AgentDesk-claude-5845-held-refused-queued-hook",
        "5845e2e0-0000-0000-0000-0000000000cc",
        NoteAnswer::Refuse,
    )
    .await;
}

/// The relay returns before any announcement POST; once that cause is repaired, the
/// scanner's row 31s later is announced exactly once.
async fn a_prompt_unsent_before_its_post_is_announced_by_the_scanner(
    tmux: &str,
    session: &str,
    setup: Setup,
) {
    let (restore_bot, restore_owner) = (setup.notify_timeout.is_none(), !setup.owner);
    let (hook_tx, hook_rx) = broadcast::channel(8);
    let (harness, relayed) = start(tmux, &[session], hook_rx, setup).await;
    hook_tx
        .send(hook_event(session))
        .expect("observer subscribed");
    wait_for_relays(&relayed, 1).await;
    assert_eq!(harness.local_note_posts(), 0);

    if restore_bot {
        harness.use_mock_notify_bot(WAIT).await;
    }
    if restore_owner {
        harness.attach_tmux_watcher(tmux, "prompt-identity.jsonl");
    }
    assert_eq!(
        scanner_sees_the_row(tmux),
        dedupe::PromptObservation::PublishedSshDirect
    );
    wait_for_announcement(&harness).await;
    assert_eq!(settled_counts(&harness).await, (1, 1, 1));
    drop(hook_tx);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_without_an_owner_channel_is_announced_once_the_owner_returns() {
    let setup = Setup {
        owner: false,
        ..READY
    };
    a_prompt_unsent_before_its_post_is_announced_by_the_scanner(
        "AgentDesk-claude-5845-ownerless-hook",
        "5845e2e0-0000-0000-0000-0000000000c7",
        setup,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_without_a_notify_bot_is_announced_once_the_bot_returns() {
    let setup = Setup {
        notify_timeout: None,
        ..READY
    };
    a_prompt_unsent_before_its_post_is_announced_by_the_scanner(
        "AgentDesk-claude-5845-botless-hook",
        "5845e2e0-0000-0000-0000-0000000000c8",
        setup,
    )
    .await;
}

/// A relay that returned before its POST leaves no id behind, so the same hook sent
/// again once the owner is back can suppress the scanner's row after its announcement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_resent_after_a_relay_without_a_post_is_announced_once() {
    let tmux = "AgentDesk-claude-5845-resent-hook";
    let session = "5845e2e0-0000-0000-0000-0000000000c9";
    let (hook_tx, hook_rx) = broadcast::channel(8);
    let setup = Setup {
        owner: false,
        ..READY
    };
    let (harness, relayed) = start(tmux, &[session], hook_rx, setup).await;
    hook_tx
        .send(hook_event(session))
        .expect("observer subscribed");
    wait_for_relays(&relayed, 1).await;

    harness.attach_tmux_watcher(tmux, "prompt-identity.jsonl");
    dedupe::age_observed_prompt_records_for_tests(PROVIDER_KEY, tmux, Duration::from_secs(31));
    hook_tx
        .send(hook_event(session))
        .expect("observer subscribed");
    wait_for_announcement(&harness).await;
    assert_eq!(
        scanner_sees_the_row(tmux),
        dedupe::PromptObservation::SuppressedReplayedEntry
    );
    assert_eq!(settled_counts(&harness).await, (1, 1, 1));
    drop(hook_tx);
}
