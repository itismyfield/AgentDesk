//! Stale-resume auto-retry e2e: a rejected resume id is re-dispatched once on a fresh
//! session, leaving Discord with the recovery notice and a single answer.

use std::time::Duration;

use super::{ProviderStub, RelayE2eHarness};

const A_MESSAGE_ID: u64 = 940_487_400_000_031;
const B_MESSAGE_ID: u64 = 940_487_400_000_032;
const RECOVERY_NOTICE: &str = "↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다.";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_resume_retry_turns_first_placeholder_into_notice_and_answers_once() {
    let name = concat!(
        module_path!(),
        "::stale_resume_retry_turns_first_placeholder_into_notice_and_answers_once"
    );
    if !crate::services::tui_o::cutover::test_override::in_empty_list_process(name) {
        return;
    }
    let harness = RelayE2eHarness::start_with_provider(ProviderStub::StaleResumeThenSuccess).await;
    harness.cache_relay_transport();

    // The mock fails the first placeholder POST, so a throwaway turn absorbs it.
    let mut released = harness.subscribe_completions();
    let _a_guard = harness
        .spawn_turn_held_at_placeholder(
            A_MESSAGE_ID,
            "absorbs the parked placeholder",
            super::PLACEHOLDER_HOLD_LIMIT,
        )
        .await;
    harness.release_held_placeholder();
    tokio::time::timeout(Duration::from_secs(2), released.recv())
        .await
        .expect("A placeholder-failure release must publish a completion event")
        .expect("completion bus open");

    harness
        .deliver_user_message(
            B_MESSAGE_ID,
            "B resumes a session the provider no longer has",
        )
        .await
        .expect("B traverses FullEvent intake");
    let answered = super::wait_until(Duration::from_secs(10), {
        let messages = harness.mock.messages.clone();
        move || {
            let messages = messages.clone();
            Box::pin(async move {
                let messages = messages.lock().expect("mock messages");
                messages.values().any(|(_, content)| content == "ok")
            })
        }
    })
    .await;
    assert!(answered, "retry must answer B: {:?}", harness.messages());
    // Outlasts the two-second deferred delay, so a second retry would be observable.
    assert!(
        !harness
            .wait_for_placeholder_posts(4, Duration::from_secs(3))
            .await
    );

    // The failed attempt's placeholder ends as the notice, never the provider error;
    // the retry posts its own reply to B, and the answer appears exactly once.
    let messages = harness.messages();
    let contents: Vec<&str> = messages
        .iter()
        .map(|(_, content)| content.as_str())
        .collect();
    assert_eq!(
        contents.len(),
        3,
        "notice, one answer, one summary: {contents:?}"
    );
    assert_eq!(&contents[..2], [RECOVERY_NOTICE, "ok"]);
    assert!(
        regex::Regex::new(
            concat!(
            r"\A-# ⏱️ (?:[0-9]+초|[0-9]+분(?: [0-9]+초)?|[0-9]+시간 [0-9]+분)",
            r"(?: · 📦 (?:100|[1-9]?[0-9])%)?",
            r"(?: · 5h (?:100|[1-9]?[0-9])% \((?:[01][0-9]|2[0-3]):[0-5][0-9]\))?",
            r"(?: · 7d (?:100|[1-9]?[0-9])% \((?:0[1-9]|1[0-2])/(?:0[1-9]|[12][0-9]|3[01]) (?:[01][0-9]|2[0-3]):[0-5][0-9]\))?\z"
        )
        )
        .unwrap()
        .is_match(contents[2]),
        "the last message is a successful turn summary: {contents:?}"
    );
    assert_eq!(
        messages[1].0,
        Some(B_MESSAGE_ID),
        "the retry must reply to B"
    );
    let unhandled = harness.unhandled_requests();
    assert!(
        unhandled.is_empty(),
        "mock Discord swallowed production calls as 404: {unhandled:?}"
    );
}
