//! The real watcher loop on a channel whose TUI body O posts: Legacy consumes the turn
//! without showing its body or recording delivery evidence.

use super::*;

const T0: &str = "ADK-O T0 delivered before the watcher attached";
const BODY: &str = "ADK-O T1 body that only O may post";

fn turn(prompt: &str, body: &str) -> String {
    format!("{}{}{}", user(prompt), said(body), stop())
}

/// One streamed-then-finished turn over a delivered T0, with the watcher attached at T0's end.
async fn finished_turn(case: u64, delegated: bool) -> (Harness, u64, u64) {
    let seed = turn("T0", T0);
    let mut h = Harness::new(case, &seed).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    let _bound = delegated.then(|| {
        crate::services::tui_o::cutover::test_override::bind_claude_tui_session(&h.tmux, &h.path)
    });
    h.row_at(f);
    h.spawn(f);
    h.append(turn("T1", BODY).as_bytes());
    let t1 = h.drained("terminal frame").await;
    (h, f, t1)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn o_delegated_watcher_turn_shows_no_body_and_records_no_frontier() {
    let flag = crate::services::tui_o::cutover::test_override::CHILD_ENV;
    if !isolated_in(
        "o_delegated_watcher_tests",
        "o_delegated_watcher_turn_shows_no_body_and_records_no_frontier",
        &[
            (flag, "1"),
            (
                crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
                "[[6284122,\"claude_tui\"]]",
            ),
        ],
    ) {
        return;
    }
    let (legacy, f, t1) = finished_turn(21, false).await;
    let seen = legacy.observe(&[BODY]);
    assert_eq!(
        (seen.copies, seen.frontier),
        (vec![vec![1]], Some((f, t1))),
        "Legacy control"
    );

    let (h, f, _) = finished_turn(22, true).await;
    let seen = h.observe(&[BODY]);
    assert_eq!(
        seen.copies,
        vec![Vec::<u64>::new()],
        "no message shows the delegated body"
    );
    assert_eq!(seen.overwritten, 0, "and none ever showed it");
    assert_eq!(
        seen.frontier,
        Some((0, f)),
        "no durable frontier for the delegated range"
    );
}

const TASK_SUMMARY: &str = "ADK-O background task finished";

fn task_value() -> serde_json::Value {
    serde_json::json!({"type": "system", "subtype": "task_notification", "task_id": "adk-o-task",
        "status": "completed", "summary": TASK_SUMMARY, "task_notification_kind": "background"})
}

/// A task-notification turn whose card the prompt observer left footer-only, plus the
/// response key the watcher's task path claims under.
async fn prepare_task_turn(case: u64) -> (Harness, String, u64) {
    use crate::services::discord::task_notification_delivery as task_delivery;
    let seed = turn("T0", T0);
    let h = Harness::new(case, &seed).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    let row = h.row_at(f);
    let state = crate::services::session_backend::StreamLineState::new();
    let context = task_delivery::TaskNotificationContext::from_stream_json(&task_value(), &state);
    let event = context
        .unwrap()
        .to_event(h.channel.get(), "claude", &h.tmux);
    task_delivery::record_footer_only(None, &event)
        .await
        .unwrap();
    let key = task_delivery::durable_response_turn_key(
        h.channel.get(),
        "claude",
        &h.tmux,
        row.user_msg_id,
        &row.started_at,
        row.turn_start_offset,
        0,
        BODY,
    );
    (h, key, f)
}

async fn task_turn(case: u64, delegated: bool) -> (Harness, String) {
    let (mut h, key, f) = prepare_task_turn(case).await;
    let _bound = delegated.then(|| {
        crate::services::tui_o::cutover::test_override::bind_claude_tui_session(&h.tmux, &h.path)
    });
    h.spawn(f);
    let task = format!("{}\n", task_value());
    h.append(format!("{}{task}{}{}", user("T1"), said(BODY), stop()).as_bytes());
    h.drained("terminal frame").await;
    (h, key)
}

async fn response_claimed(h: &Harness, key: &str) -> bool {
    use crate::services::discord::task_notification_delivery as task_delivery;
    let owner = task_delivery::ResponseDeliveryOwner::Watcher;
    let claim = task_delivery::claim_existing_task_response_delivery(
        None,
        h.channel.get(),
        "claude",
        &h.tmux,
        key,
        owner,
    );
    claim.await.unwrap().is_some()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn o_delegated_task_notification_turn_promotes_card_without_body_or_claim() {
    let flag = crate::services::tui_o::cutover::test_override::CHILD_ENV;
    if !isolated_in(
        "o_delegated_watcher_tests",
        "o_delegated_task_notification_turn_promotes_card_without_body_or_claim",
        &[
            (flag, "1"),
            (
                crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
                "[[6284124,\"claude_tui\"]]",
            ),
        ],
    ) {
        return;
    }
    let (legacy, key) = task_turn(23, false).await;
    assert!(
        legacy.showing(TASK_SUMMARY),
        "Legacy control promotes the card"
    );
    assert!(
        response_claimed(&legacy, &key).await,
        "Legacy control claims the response"
    );

    let (h, key) = task_turn(24, true).await;
    assert!(h.showing(TASK_SUMMARY), "the footer-only card is promoted");
    let seen = h.observe(&[BODY]);
    assert!(
        seen.copies[0].is_empty(),
        "no message shows the delegated body"
    );
    assert_eq!(seen.overwritten, 0, "and none ever showed it");
    assert!(!response_claimed(&h, &key).await, "no response claim");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn o_delegated_task_card_post_failure_retries_before_consuming_turn() {
    let flag = crate::services::tui_o::cutover::test_override::CHILD_ENV;
    if !isolated_in(
        "o_delegated_watcher_tests",
        "o_delegated_task_card_post_failure_retries_before_consuming_turn",
        &[
            (flag, "1"),
            (
                crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
                "[[6284126,\"claude_tui\"]]",
            ),
        ],
    ) {
        return;
    }
    let (mut h, key, f) = prepare_task_turn(26).await;
    let _bound =
        crate::services::tui_o::cutover::test_override::bind_claude_tui_session(&h.tmux, &h.path);
    let retry = PostRetry::new(TASK_SUMMARY);
    h.discord.lock().unwrap().post_retry = Some(retry.clone());
    h.spawn(f);
    let task = format!("{}\n", task_value());
    h.append(format!("{}{task}{}{}", user("T1"), said(BODY), stop()).as_bytes());
    h.until("card retry or premature terminal consume", |h| {
        retry.attempts.load(Ordering::Acquire) >= 2
            || h.row().is_none_or(|row| row.terminal_delivery_committed)
    })
    .await;

    let row = h
        .row()
        .expect("a failed card POST must preserve the inflight row");
    assert!(
        !row.terminal_delivery_committed,
        "a failed card POST must not consume the turn"
    );
    assert_eq!(
        row.last_watcher_relayed_offset, None,
        "no success watermark after failure"
    );
    assert_eq!(
        h.shared
            .tmux_relay_coord(h.channel)
            .confirmed_end_offset
            .load(Ordering::Acquire),
        f,
        "the consumed range must not advance before the card succeeds"
    );
    assert_eq!(
        retry.attempts.load(Ordering::Acquire),
        2,
        "the card is retried"
    );
    assert_eq!(
        h.frames().len(),
        2,
        "the watcher reprocesses the terminal, not an HTTP retry"
    );
    let pending = h.observe(&[TASK_SUMMARY, BODY]);
    assert!(pending.copies.iter().all(Vec::is_empty));
    assert_eq!(pending.frontier, Some((0, f)));
    assert!(
        !response_claimed(&h, &key).await,
        "no response claim during retry"
    );

    retry.release.notify_one();
    h.drained("successful card retry").await;
    let seen = h.observe(&[TASK_SUMMARY, BODY]);
    assert_eq!(seen.copies[0].len(), 1, "one task card after the retry");
    assert!(seen.copies[1].is_empty(), "no Legacy body");
    assert_eq!(seen.overwritten, 0, "Legacy never showed the body");
    assert_eq!(
        retry.attempts.load(Ordering::Acquire),
        2,
        "only one failed POST"
    );
    assert!(
        !response_claimed(&h, &key).await,
        "no response claim after success"
    );
}

const PARTIAL: &str = "Working ADK-O partial Legacy streamed before O owned the channel";

/// Legacy streams part of T1 into its placeholder, then O takes the channel before T1 ends.
async fn streamed_then_delegated_turn(case: u64) -> Harness {
    let seed = turn("T0", T0);
    let mut h = Harness::new(case, &seed).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    h.row_at(f);
    h.spawn(f);
    h.append(format!("{}{}", user("T1"), said(PARTIAL)).as_bytes());
    h.until("streamed partial", |h| h.showing(PARTIAL)).await;
    // The writer list is fixed per process, so this child lists the channel only from the cutover.
    let _env = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let listed = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
        std::ffi::OsStr::new(&format!("[[{},\"claude_tui\"]]", h.channel.get())),
    );
    let bound =
        crate::services::tui_o::cutover::test_override::bind_claude_tui_session(&h.tmux, &h.path);
    h.append(format!("{}{}", said(BODY), stop()).as_bytes());
    h.drained("terminal frame").await;
    drop(bound);
    drop(listed);
    h
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn o_delegated_mid_turn_cutover_shows_no_post_cutover_body() {
    let flag = crate::services::tui_o::cutover::test_override::CHILD_ENV;
    if !isolated_in(
        "o_delegated_watcher_tests",
        "o_delegated_mid_turn_cutover_shows_no_post_cutover_body",
        &[
            (flag, "1"),
            (
                crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
                "[]",
            ),
        ],
    ) {
        return;
    }
    let h = streamed_then_delegated_turn(25).await;
    let shown = h.discord.lock().unwrap().shown.clone();
    assert!(
        !shown.iter().any(|text| text.contains(BODY)),
        "Legacy writes nothing O posts onto its streamed anchor: {shown:?}"
    );
}
