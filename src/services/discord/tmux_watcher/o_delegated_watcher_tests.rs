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
    if !isolated_in(
        "o_delegated_watcher_tests",
        "o_delegated_watcher_turn_shows_no_body_and_records_no_frontier",
        &[(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[[6284122,\"claude_tui\"]]",
        )],
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

/// A held watcher terminal is retried, never finalized, and shows nothing; once O adopts, the next
/// retry ends the turn through O with no new input, and Legacy still shows nothing.
#[tokio::test(flavor = "current_thread")]
async fn a_held_watcher_terminal_retries_until_o_adopts_and_then_ends_through_o() {
    if !isolated_in(
        "o_delegated_watcher_tests",
        "a_held_watcher_terminal_retries_until_o_adopts_and_then_ends_through_o",
        &[],
    ) {
        return;
    }
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::cutover::test_override;
    let seed = turn("T0", T0);
    let mut h = Harness::new(25, &seed).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    let _bound = test_override::bind_claude_tui_session(&h.tmux, &h.path);
    // A thread-local candidate: this runtime runs the watcher on the test's own thread.
    let _pending = test_override::force_candidates(&[(h.channel.get(), ClaudeTui)]);
    h.row_at(f);
    h.spawn(f);
    h.append(turn("T1", BODY).as_bytes());
    let held = || Harness::logged("tui_o output held for adoption").len();
    // Past the three rewinds a failing send gets before the watcher gives up on its turn.
    h.until("the held terminal retried past the rewind cap", |_| {
        held() >= 20
    })
    .await;
    let gave_up = Harness::logged("rewind cap exceeded");
    assert!(
        gave_up.is_empty(),
        "a held turn is never given up: {gave_up:?}"
    );
    let waiting = h.observe(&[BODY]);
    assert_eq!(
        waiting.copies,
        vec![Vec::<u64>::new()],
        "nothing shows the held body"
    );
    assert_eq!(
        waiting.row,
        Some((f, false)),
        "the held turn's row stays open"
    );

    let candidate = test_override::with_channels(|boot| boot?.candidate(h.channel.get()).cloned());
    assert!(candidate.unwrap().confirm_store(), "the adoption commits");
    h.until("the turn ends through O", |h| {
        h.row().is_none_or(|row| row.terminal_delivery_committed)
    })
    .await;
    let ended = h.observe(&[BODY]);
    assert_eq!(
        ended.copies,
        vec![Vec::<u64>::new()],
        "O, not Legacy, owns the body"
    );
    assert_eq!(ended.overwritten, 0, "and Legacy never showed it");
    assert_eq!(
        ended.frontier,
        Some((0, f)),
        "no Legacy frontier for the range"
    );
}

/// A refused adoption holds the watcher terminal as well: the gate answers refused from the start,
/// the watcher reads the same range again instead of finalizing it, and the turn's row stays open.
#[tokio::test(flavor = "current_thread")]
async fn a_refused_watcher_terminal_is_read_again_and_never_finalized() {
    if !isolated_in(
        "o_delegated_watcher_tests",
        "a_refused_watcher_terminal_is_read_again_and_never_finalized",
        &[],
    ) {
        return;
    }
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::cutover::{
        IdentityError, peek_o_owns_tui_output_for_channel_tmux, test_override,
    };
    let seed = turn("T0", T0);
    let mut h = Harness::new(26, &seed).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    let _bound = test_override::bind_claude_tui_session(&h.tmux, &h.path);
    let _refused = test_override::force_candidates(&[(h.channel.get(), ClaudeTui)]);
    let candidate = test_override::with_channels(|boot| boot?.candidate(h.channel.get()).cloned());
    candidate.unwrap().release(h.channel.get());
    let gate = peek_o_owns_tui_output_for_channel_tmux(h.channel.get(), Some(&h.tmux));
    assert_eq!(gate, Err(IdentityError::AdoptionRefused));
    h.row_at(f);
    h.spawn(f);
    h.append(turn("T1", BODY).as_bytes());
    let end = h.len();
    let reads = |h: &Harness| {
        let frames = h.frames().into_iter();
        frames
            .filter(|frame| (frame.0, frame.1) == (f, end))
            .count()
    };
    // Past the rewind cap (3) by two, so a refused retry counted as a rewind would give up first.
    h.until("the refused terminal's range read again", |h| reads(h) >= 5)
        .await;
    let gave_up = Harness::logged("rewind cap exceeded");
    assert!(
        gave_up.is_empty(),
        "a refused turn is not given up: {gave_up:?}"
    );
    let seen = h.observe(&[BODY]);
    assert_eq!(
        seen.copies,
        vec![Vec::<u64>::new()],
        "nothing shows the refused body"
    );
    assert_eq!(
        seen.row,
        Some((f, false)),
        "the refused turn's row stays open"
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
    if !isolated_in(
        "o_delegated_watcher_tests",
        "o_delegated_task_notification_turn_promotes_card_without_body_or_claim",
        &[(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[[6284124,\"claude_tui\"]]",
        )],
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
    if !isolated_in(
        "o_delegated_watcher_tests",
        "o_delegated_task_card_post_failure_retries_before_consuming_turn",
        &[(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[[6284126,\"claude_tui\"]]",
        )],
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

const PARTIAL: &str = "Working ADK-O partial streamed before the TUI binding resolved";

/// On a listed channel, part of T1 streams before its TUI binding resolves and the rest after.
async fn streamed_then_delegated_turn(case: u64) -> Harness {
    let seed = turn("T0", T0);
    let mut h = Harness::new(case, &seed).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    h.row_at(f);
    h.spawn(f);
    h.append(format!("{}{}", user("T1"), said(PARTIAL)).as_bytes());
    // A held partial leaves no frame to wait on; two poll-loop returns prove the watcher read it.
    for _ in 0..2 {
        let seen = crate::services::discord::tmux_watcher_now_ms();
        h.until("poll loop return", |h| h.heartbeat() > seen).await;
    }
    let bound =
        crate::services::tui_o::cutover::test_override::bind_claude_tui_session(&h.tmux, &h.path);
    // A held identity also suppresses the Legacy body, so pin the production decision to O ownership.
    assert_eq!(
        crate::services::tui_o::cutover::o_owns_tui_output_for_channel_tmux(
            h.channel.get(),
            Some(&h.tmux)
        ),
        Ok(true)
    );
    h.append(format!("{}{}", said(BODY), stop()).as_bytes());
    h.drained("terminal frame").await;
    drop(bound);
    h
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn o_delegated_mid_turn_cutover_shows_no_post_cutover_body() {
    if !isolated_in(
        "o_delegated_watcher_tests",
        "o_delegated_mid_turn_cutover_shows_no_post_cutover_body",
        &[(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[[6284125,\"claude_tui\"]]",
        )],
    ) {
        return;
    }
    let h = streamed_then_delegated_turn(25).await;
    let shown = h.discord.lock().unwrap().shown.clone();
    assert!(
        !shown
            .iter()
            .any(|text| text.contains(BODY) || text.contains(PARTIAL)),
        "Legacy writes no part of a listed channel's TUI body: {shown:?}"
    );
}

mod self_started_publish_tests {
    //! Native task-notification output through capture, O delivery and the live watcher loop.

    use super::*;
    use crate::services::discord::outbound::o_delivery_evidence::tests::Alarms;
    use crate::services::discord::outbound::o_writer_io::{ChannelLeases, GatewayPort};
    use crate::services::tui_o::ownership::OwnershipGate;
    use crate::services::tui_o::shadow::{
        CaptureOutcome, CaptureSource, ShadowProvider, binding_reader::source_id_for,
        capture::SourceCapture,
    };
    use crate::services::tui_o::store::{InitSource, Initialized, OStore, StoreConfig};
    use crate::services::tui_o::writer::{
        deliver::{ChannelWriter, Step},
        pieces::UnitDeriver,
    };
    use crate::test_support::wait::{WAIT_CAP, await_within};

    const BODY: &str = "The background task completed; here is its final result.";

    fn notification_turn() -> String {
        let prompt = serde_json::json!({
            "type":"user", "uuid":"task-completed", "sessionId":"native-session", "isMeta":true,
            "message":{"role":"user", "content":[{"type":"text", "text":
                "<task-notification>\n<task-id>background-1</task-id>\n<status>completed</status>\n<summary>Background task finished</summary>\n</task-notification>"}]}
        });
        let answer = serde_json::json!({
            "type":"assistant", "uuid":"task-answer", "apiBlockIndex":0,
            "message":{"id":"task-answer-message", "role":"assistant",
                "content":[{"type":"text", "text":BODY}]}
        });
        format!("{prompt}\n{answer}\n{}", stop())
    }

    async fn publish(h: &Harness, start: u64, replay: bool) {
        let root = crate::config::runtime_root().unwrap();
        let source = source_id_for("native-session", std::path::Path::new(&h.path)).unwrap();
        let mut capture = SourceCapture::open(source.clone(), start).unwrap();
        let initialized = Initialized {
            policy_parent: None,
            channel: h.channel.get(),
            sources: vec![InitSource {
                source_id: source,
                delivery_start: start,
                prefix_hash: capture.prefix_hash(),
            }],
            initial_anchor: 0,
            build_digest: "self-started fixture".into(),
            at: chrono::Utc::now(),
        };
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root)
            .unwrap()
            .unwrap();
        store.init_fixture_channel(&initialized).unwrap();
        store
            .begin_fixture_era(&[h.channel.get()], chrono::Utc::now(), |_| {
                Ok(initialized.clone())
            })
            .unwrap();
        let era = store.read_era().unwrap().unwrap();
        let mut channel = store.open_channel(&era, h.channel.get()).unwrap().unwrap();
        let mut deriver = UnitDeriver::new(h.channel.get(), ShadowProvider::Claude);
        let mut work = Vec::new();
        while capture.captured_through() < h.len() {
            let CaptureOutcome::Batch(batch) = capture.poll(1024 * 1024) else {
                panic!("native transcript must be captured");
            };
            channel
                .append_spool(&batch, &capture.prefix_hash())
                .unwrap();
            work.extend(
                batch
                    .records
                    .iter()
                    .flat_map(|record| deriver.derive(record)),
            );
        }
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        let port = Arc::new(GatewayPort::new(h.http.clone(), 1));
        let mut writer = ChannelWriter::new(
            channel,
            gate,
            port,
            ChannelLeases::from_shared(h.shared.clone()),
            Alarms,
        );
        for _ in 0..if replay { 2 } else { 1 } {
            for piece in &work {
                assert_eq!(
                    await_within(writer.deliver(piece), WAIT_CAP).await,
                    Step::Done
                );
            }
        }
        assert_eq!(writer.store().ledger().violation(), None);
    }

    fn assert_one_body(h: &Harness) {
        assert_eq!(
            h.observe(&[BODY]).copies[0].len(),
            1,
            "one visible final body"
        );
        assert_eq!(
            h.discord
                .lock()
                .unwrap()
                .shown
                .iter()
                .filter(|text| text.contains(BODY))
                .count(),
            1,
            "one body transport across O, watcher and replay"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_notification_without_inflight_is_published_once() {
        if !isolated_in(
            "o_delegated_watcher_tests::self_started_publish_tests",
            "task_notification_without_inflight_is_published_once",
            &[(
                crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
                "[[6284281,\"claude_tui\"]]",
            )],
        ) {
            return;
        }
        let mut h = Harness::new(181, "").await;
        let _bound = crate::services::tui_o::cutover::test_override::bind_claude_tui_session(
            &h.tmux, &h.path,
        );
        h.append(notification_turn().as_bytes());
        assert!(h.row().is_none(), "AgentDesk never started this turn");
        publish(&h, 0, false).await;
        h.spawn(0);
        await_within(h.drained("rowless task notification"), WAIT_CAP).await;
        assert_one_body(&h);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_notification_overlapping_agentdesk_turn_and_replay_has_no_duplicate() {
        if !isolated_in(
            "o_delegated_watcher_tests::self_started_publish_tests",
            "task_notification_overlapping_agentdesk_turn_and_replay_has_no_duplicate",
            &[(
                crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
                "[[6284282,\"claude_tui\"]]",
            )],
        ) {
            return;
        }
        let seed = format!(
            "{}{}{}",
            user("AgentDesk input"),
            said("Earlier answer"),
            stop()
        );
        let mut h = Harness::new(182, &seed).await;
        let start = h.len();
        h.commit(0, start);
        let mut row = h.row_at(start);
        row.user_msg_id = 91;
        row.turn_source = crate::services::discord::inflight::TurnSource::Managed;
        h.save(&row);
        let _bound = crate::services::tui_o::cutover::test_override::bind_claude_tui_session(
            &h.tmux, &h.path,
        );
        h.spawn(start);
        h.append(notification_turn().as_bytes());
        tokio::join!(
            publish(&h, start, true),
            await_within(h.drained("overlapping task notification"), WAIT_CAP),
        );
        assert_one_body(&h);
    }
}
