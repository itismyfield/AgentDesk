//! A Codex direct input's native turn decides where its answer starts and ends; a rollout that
//! names no turns keeps the text search, and a steering input joins the turn it names.
use super::*;

/// An answer and `task_complete` as a Codex that names no turns writes them.
fn legacy_answer(text: &str) -> String {
    rollout_line(serde_json::json!({"type": "response_item", "payload": {
        "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}}))
        + &rollout_line(serde_json::json!({"type": "event_msg", "payload": {
            "type": "task_complete", "last_agent_message": text}}))
}

/// The hook names a turn but the rollout names none: the text search answers on the anchor.
#[test]
fn codex_direct_answer_on_a_rollout_without_turn_ids_keeps_the_text_search() {
    run(|root| {
        Box::pin(async move {
            let _live_pane = live_pane_tmux(&root);
            let mut fx = Fixture::start(&root, 5_705_010, "AgentDesk-codex-5704-legacy").await;
            let first = fx.hook(PROMPT, &turn(1)).await;
            super::super::super::super::relay_observed_prompt(&fx.shared, first).await;
            let anchor = fx.codex.row().expect("claim").user_msg_id;
            fx.append(&(user_line_for(PROMPT) + &legacy_answer(RESPONSE)));
            assert!(
                fx.delivered(RESPONSE).await,
                "answer never reached Discord: {:?}",
                fx.requests.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
            fx.assert_answer(RESPONSE, anchor);
            assert_eq!(fx.completed_turns(), 1);
            assert_eq!(tail_starts(fx.codex.channel), 1);
            fx.assert_released().await;
            fx.finish();
        })
    });
}

/// A claim made before its turn's records waits through a torn opening record, then answers
/// from that turn's prompt.
#[test]
fn codex_direct_answer_waits_for_its_turn_through_a_torn_opening() {
    run(|root| {
        Box::pin(async move {
            let _live_pane = live_pane_tmux(&root);
            let mut fx = Fixture::start(&root, 5_705_020, "AgentDesk-codex-5704-torn-open").await;
            let t1 = turn(1);
            let first = fx.hook(PROMPT, &t1).await;
            super::super::super::super::relay_observed_prompt(&fx.shared, first).await;
            let claimed = fx.codex.row().expect("claim");
            let opening = opening(&t1, PROMPT);
            let (head, rest) = opening.split_at(opening.find('\n').unwrap() / 2);
            fx.append(head);
            fx.polls(3).await;
            assert_eq!(tail_starts(fx.codex.channel), 0, "no tail before the turn");
            assert_eq!(
                fx.codex.row().expect("row waits").turn_start_offset,
                claimed.turn_start_offset
            );
            fx.append(&(rest.to_string() + &item_completed_line_for(&t1)));
            fx.append(&answer_for(&t1, RESPONSE));
            assert!(
                fx.delivered(RESPONSE).await,
                "answer never reached Discord: {:?}",
                fx.requests.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
            fx.assert_answer(RESPONSE, claimed.user_msg_id);
            assert_eq!(fx.completed_turns(), 1);
            assert_eq!(tail_starts(fx.codex.channel), 1);
            fx.assert_released().await;
            fx.finish();
        })
    });
}

/// Another turn with the same prompt lands first: the claimed input waits for its own turn and
/// never takes the other turn's answer.
#[test]
fn codex_direct_answer_skips_another_turn_with_the_same_prompt() {
    run(|root| {
        Box::pin(async move {
            let _live_pane = live_pane_tmux(&root);
            let mut fx = Fixture::start(&root, 5_705_030, "AgentDesk-codex-5704-other").await;
            let (t1, t2) = (turn(1), turn(2));
            let own = fx.hook(PROMPT, &t2).await;
            super::super::super::super::relay_observed_prompt(&fx.shared, own).await;
            let anchor = fx.codex.row().expect("claim").user_msg_id;
            fx.append(
                &(opening(&t1, PROMPT)
                    + &item_completed_line_for(&t1)
                    + &answer_for(&t1, RESPONSE)),
            );
            fx.polls(3).await;
            assert_eq!(tail_starts(fx.codex.channel), 0, "no tail before the turn");
            fx.append(
                &(opening(&t2, PROMPT)
                    + &item_completed_line_for(&t2)
                    + &answer_for(&t2, RESPONSE2)),
            );
            assert!(
                fx.delivered(RESPONSE2).await,
                "answer never reached Discord: {:?}",
                fx.requests.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
            fx.assert_answer(RESPONSE2, anchor);
            assert!(
                fx.sent(RESPONSE).is_empty(),
                "the other turn's answer leaked"
            );
            assert_eq!(fx.completed_turns(), 1);
            assert_eq!(tail_starts(fx.codex.channel), 1);
            fx.assert_released().await;
            fx.finish();
        })
    });
}

/// Where a steering input's hook and record land relative to the running turn's answer.
#[derive(Clone, Copy)]
enum Steer {
    HookAfterBody,
    HookBeforeBody,
    /// The tail reads the steer record before its hook arrives.
    RecordFirst,
    /// The idle loop scans the steer record before the running input is claimed.
    ScannedFirst,
}

/// A steering input joins the running native turn: its text is echoed once, and one anchor,
/// tail and turn answer it with no pending start while the running turn's lease stays.
async fn steer_into_running_turn(root: PathBuf, channel: u64, tmux: &str, order: Steer) {
    let _live_pane = live_pane_tmux(&root);
    let mut fx = Fixture::start(&root, channel, tmux).await;
    let t1 = turn(1);
    let first = fx.hook(PROMPT, &t1).await;
    let mut end = fx.append(&(opening(&t1, PROMPT) + &item_completed_line_for(&t1)));
    let progress = rollout_line(serde_json::json!({"type": "response_item", "payload": {
        "type": "message", "role": "assistant",
        "content": [{"type": "output_text", "text": "running the command"}]}}))
        + &token_count_line();
    let steer = user_line_for(PROMPT2) + &item_completed_line_for(&t1);
    if matches!(order, Steer::ScannedFirst) {
        end = fx.append(&(progress.clone() + &steer));
    }
    fx.scanned_past(end).await;
    super::super::super::super::relay_observed_prompt(&fx.shared, first).await;
    let anchor = fx.codex.row().expect("claim").user_msg_id;
    assert!(
        wait_for(Duration::from_secs(10), || tail_starts(fx.codex.channel)
            == 1)
        .await,
        "the running turn's tail never started"
    );
    let lease = fx.lease();
    assert!(lease.is_some());
    let mut echoes = match order {
        Steer::HookAfterBody => {
            fx.append(&progress);
            let echoes = fx.steer_hook(PROMPT2, &t1).await;
            fx.append(&steer);
            echoes
        }
        Steer::HookBeforeBody => {
            let echoes = fx.steer_hook(PROMPT2, &t1).await;
            fx.append(&(steer + &progress));
            echoes
        }
        Steer::RecordFirst => {
            fx.append(&(progress + &steer));
            tokio::time::sleep(Duration::from_secs(1)).await;
            fx.steer_hook(PROMPT2, &t1).await
        }
        Steer::ScannedFirst => fx.steer_hook(PROMPT2, &t1).await,
    };
    echoes += fx.relay_echoes(PROMPT2).await;
    assert_eq!(fx.lease(), lease, "the join replaced the running lease");
    fx.append(&answer_for(&t1, RESPONSE2));
    assert!(
        fx.delivered(RESPONSE2).await,
        "the turn's answer never reached Discord: {:?}",
        fx.requests.lock().unwrap()
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    echoes += fx.relay_echoes(PROMPT2).await;
    assert_eq!(echoes, 1, "the steer is echoed once");
    let echoed = fx.sent(PROMPT2);
    let messages = format!("/channels/{}/messages", fx.codex.channel.get());
    assert!(
        echoed.len() == 1 && echoed[0].0 == "POST" && echoed[0].1.ends_with(&messages),
        "{echoed:?}"
    );
    let target = format!("{messages}/{anchor}");
    let sent = fx.sent(RESPONSE2);
    assert!(
        sent.iter()
            .all(|(method, path, _)| method == "PATCH" && path.ends_with(&target)),
        "{sent:?}"
    );
    assert_eq!(fx.completed_turns(), 1, "one native turn, one bridge turn");
    assert_eq!(tail_starts(fx.codex.channel), 1, "one tail");
    assert!(
        fx.pending_starts().is_empty(),
        "nothing waits for a second turn"
    );
    fx.assert_released().await;
    fx.finish();
}

#[test]
fn codex_direct_steering_input_joins_the_running_turn() {
    run(|root| {
        Box::pin(steer_into_running_turn(
            root,
            5_704_995,
            "AgentDesk-codex-5704-steer",
            Steer::HookAfterBody,
        ))
    });
}

#[test]
fn codex_direct_steering_input_before_the_first_body_joins_the_running_turn() {
    run(|root| {
        Box::pin(steer_into_running_turn(
            root,
            5_705_040,
            "AgentDesk-codex-5704-steer-early",
            Steer::HookBeforeBody,
        ))
    });
}

#[test]
fn codex_direct_steering_record_read_before_its_hook_joins_the_running_turn() {
    run(|root| {
        Box::pin(steer_into_running_turn(
            root,
            5_705_041,
            "AgentDesk-codex-5704-steer-tail",
            Steer::RecordFirst,
        ))
    });
}

#[test]
fn codex_direct_steering_record_scanned_before_the_claim_joins_the_running_turn() {
    run(|root| {
        Box::pin(steer_into_running_turn(
            root,
            5_705_042,
            "AgentDesk-codex-5704-steer-scan",
            Steer::ScannedFirst,
        ))
    });
}

/// The running input's announcement fails, so nothing owns its turn: a later steer of that turn
/// is its own input and gets its answer on its own anchor.
#[test]
fn codex_direct_steer_after_a_failed_announcement_answers_on_its_own_anchor() {
    run(|root| {
        Box::pin(async move {
            let _live_pane = live_pane_tmux(&root);
            let mut fx =
                Fixture::start(&root, 5_705_043, "AgentDesk-codex-5704-steer-orphan").await;
            let t1 = turn(1);
            fx.failing_posts.store(1, Ordering::SeqCst);
            let first = fx.hook(PROMPT, &t1).await;
            super::super::super::super::relay_observed_prompt(&fx.shared, first).await;
            assert_eq!(
                fx.failing_posts.load(Ordering::SeqCst),
                0,
                "the announcement failed"
            );
            let progress = rollout_line(serde_json::json!({"type": "response_item", "payload": {
                "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "running the command"}]}}));
            let end =
                fx.append(&(opening(&t1, PROMPT) + &item_completed_line_for(&t1) + &progress));
            fx.scanned_past(end).await;
            assert!(fx.codex.row().is_none() && fx.pending_starts().is_empty());
            let steer = fx.hook(PROMPT2, &t1).await;
            assert!(!steer.steer_echo, "the steer joined a turn nothing answers");
            super::super::super::super::relay_observed_prompt(&fx.shared, steer).await;
            let anchor = fx.codex.row().expect("the steer claims").user_msg_id;
            fx.append(&(user_line_for(PROMPT2) + &item_completed_line_for(&t1)));
            fx.append(&answer_for(&t1, RESPONSE2));
            assert!(
                fx.delivered(RESPONSE2).await,
                "the steer's answer never reached Discord: {:?}",
                fx.requests.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
            fx.assert_answer(RESPONSE2, anchor);
            assert_eq!(fx.sent(PROMPT2).len(), 1, "one announcement, no echo");
            assert_eq!(fx.completed_turns(), 1);
            assert_eq!(tail_starts(fx.codex.channel), 1);
            fx.assert_released().await;
            fx.finish();
        })
    });
}

/// A steer text repeated past the recent-duplicate window is a new submit and is echoed again, as
/// is a later steer repeating the opening text; the running turn keeps its anchor, lease and tail.
#[test]
fn codex_direct_repeated_steer_text_past_the_duplicate_window_is_echoed_again() {
    run(|root| {
        Box::pin(async move {
            let _live_pane = live_pane_tmux(&root);
            let mut fx = Fixture::start(&root, 5_705_044, "AgentDesk-codex-5704-steer-again").await;
            let t1 = turn(1);
            let first = fx.hook(PROMPT, &t1).await;
            let end = fx.append(&(opening(&t1, PROMPT) + &item_completed_line_for(&t1)));
            fx.scanned_past(end).await;
            super::super::super::super::relay_observed_prompt(&fx.shared, first).await;
            let anchor = fx.codex.row().expect("claim").user_msg_id;
            assert!(
                wait_for(Duration::from_secs(10), || tail_starts(fx.codex.channel)
                    == 1)
                .await,
                "the running turn's tail never started"
            );
            let lease = fx.lease();
            let mut echoes = Vec::new();
            for text in [PROMPT2, PROMPT2, PROMPT] {
                crate::services::tui_prompt_dedupe::age_observed_prompt_records_for_tests(
                    "codex",
                    &fx.codex.tmux,
                    Duration::from_secs(31),
                );
                let hooked = fx.steer_hook(text, &t1).await;
                fx.append(&(user_line_for(text) + &item_completed_line_for(&t1)));
                tokio::time::sleep(Duration::from_secs(1)).await;
                echoes.push(hooked + fx.relay_echoes(text).await);
            }
            assert_eq!(echoes, [1, 1, 1], "each submit is echoed once");
            assert_eq!(fx.lease(), lease, "an echo replaced the running lease");
            fx.append(&answer_for(&t1, RESPONSE2));
            assert!(
                fx.delivered(RESPONSE2).await,
                "the turn's answer never reached Discord: {:?}",
                fx.requests.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_secs(3)).await;
            // PROMPT is a prefix of PROMPT2, so its count leaves PROMPT2's messages out.
            let posts = |text: &str| {
                let sent = fx.sent(text).into_iter();
                sent.filter(|(method, _, body)| {
                    method == "POST" && (text == PROMPT2 || !body.contains(PROMPT2))
                })
                .count()
            };
            assert_eq!(
                (posts(PROMPT2), posts(PROMPT)),
                (2, 2),
                "two steer echoes; the announcement and one opening-text echo"
            );
            let target = format!("/channels/{}/messages/{anchor}", fx.codex.channel.get());
            assert!(
                fx.sent(RESPONSE2)
                    .iter()
                    .all(|(method, path, _)| method == "PATCH" && path.ends_with(&target))
            );
            assert_eq!(fx.completed_turns(), 1, "one native turn, one bridge turn");
            assert_eq!(tail_starts(fx.codex.channel), 1, "one tail");
            assert!(fx.pending_starts().is_empty());
            fx.assert_released().await;
            fx.finish();
        })
    });
}

/// Leaves the durable pending-start store unwritable until dropped: a file stands at its path.
struct UnwritablePendingStore(PathBuf);

impl UnwritablePendingStore {
    fn new() -> Self {
        let root = crate::services::discord::runtime_store::tui_direct_pending_start_root()
            .expect("pending-start root");
        std::fs::create_dir_all(root.parent().expect("runtime root")).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        std::fs::write(&root, b"").unwrap();
        Self(root)
    }
}

impl Drop for UnwritablePendingStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The next deferred worker on this thread sees its prior turn finalized and fails every claim.
fn refuse_deferred_claims(attempts: Arc<AtomicUsize>) {
    use crate::services::discord::tui_direct_pending_start::{PriorTurnObservation, PriorTurnView};
    synthetic_start::RESTORE_VIEW_FOR_TEST.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(|_shared, _record| {
            Box::pin(async {
                Some(PriorTurnObservation {
                    view: PriorTurnView {
                        inflight_present: false,
                        inflight_is_own_anchor: false,
                        mailbox_blocking_turn_present: false,
                        mailbox_turn_is_own_anchor: false,
                        runtime_binding_present: true,
                    },
                    foreign_inflight_identity: None,
                })
            })
        }))
    });
    synthetic_start::RESTORE_CLAIM_FOR_TEST.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |_shared, _record| {
            let attempts = attempts.clone();
            Box::pin(async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                false
            })
        }))
    });
}

/// A deferred input whose pending record cannot be written is still answered by its in-memory
/// worker on its own anchor, and once the worker claims, its turn owns the turn's steers.
#[test]
fn codex_direct_deferred_input_with_an_unwritable_pending_record_is_answered_by_its_worker() {
    run(|root| {
        Box::pin(async move {
            let _live_pane = live_pane_tmux(&root);
            let mut fx = Fixture::start(&root, 5_705_070, "AgentDesk-codex-5704-unwritable").await;
            let _store = UnwritablePendingStore::new();
            let (t1, t2) = (turn(1), turn(2));
            let (first, second) = (fx.hook(PROMPT, &t1).await, fx.hook(PROMPT2, &t2).await);
            fx.append(
                &(opening(&t1, PROMPT)
                    + &item_completed_line_for(&t1)
                    + &answer_for(&t1, RESPONSE)),
            );
            let end = fx.append(&(opening(&t2, PROMPT2) + &item_completed_line_for(&t2)));
            fx.scanned_past(end).await;
            super::super::super::super::relay_observed_prompt(&fx.shared, first).await;
            let first_anchor = fx.codex.row().expect("claim").user_msg_id;
            super::super::super::super::relay_observed_prompt(&fx.shared, second).await;
            assert!(
                fx.pending_starts().is_empty(),
                "the pending record was written"
            );
            // The worker drops its in-memory presence as it returns from a successful claim.
            let channel = fx.codex.channel.get();
            assert!(
                wait_for(Duration::from_secs(20), || {
                    fx.codex.row().is_some_and(|row| row.user_msg_id != first_anchor)
                        && !crate::services::discord::tui_direct_pending_start::pending_synthetic_start_present("codex", channel)
                })
                .await,
                "the worker never claimed the deferred input"
            );
            assert_eq!(
                fx.steer_hook(PROMPT3, &t2).await,
                1,
                "the claimed turn owns its steer"
            );
            fx.append(
                &(user_line_for(PROMPT3)
                    + &item_completed_line_for(&t2)
                    + &answer_for(&t2, RESPONSE2)),
            );
            fx.assert_answers_in_order(&[RESPONSE, RESPONSE2], Some(2))
                .await;
            fx.finish();
        })
    });
}

/// The worker of a deferred input with an unwritable pending record gives up: the turn it held is
/// withdrawn, so a later steer of that turn is relayed and answered as its own input.
#[test]
fn codex_direct_steer_after_an_abandoned_deferred_input_answers_on_its_own_anchor() {
    run(|root| {
        Box::pin(async move {
            let _live_pane = live_pane_tmux(&root);
            let mut fx = Fixture::start(&root, 5_705_071, "AgentDesk-codex-5704-abandoned").await;
            let _store = UnwritablePendingStore::new();
            let (t1, t2) = (turn(1), turn(2));
            let (first, second) = (fx.hook(PROMPT, &t1).await, fx.hook(PROMPT2, &t2).await);
            fx.append(
                &(opening(&t1, PROMPT)
                    + &item_completed_line_for(&t1)
                    + &answer_for(&t1, RESPONSE)),
            );
            let end = fx.append(&(opening(&t2, PROMPT2) + &item_completed_line_for(&t2)));
            fx.scanned_past(end).await;
            super::super::super::super::relay_observed_prompt(&fx.shared, first).await;
            let first_anchor = fx.codex.row().expect("claim").user_msg_id;
            let attempts = Arc::new(AtomicUsize::new(0));
            refuse_deferred_claims(attempts.clone());
            super::super::super::super::relay_observed_prompt(&fx.shared, second).await;
            let limit = crate::services::discord::tui_direct_pending_start::PENDING_START_MAX_CLAIM_ATTEMPTS;
            assert!(
                wait_for(Duration::from_secs(10), || attempts.load(Ordering::SeqCst)
                    >= limit as usize)
                .await,
                "the worker never gave up"
            );
            assert!(
                fx.delivered(RESPONSE).await,
                "the first answer never reached Discord"
            );
            assert!(
                wait_for(Duration::from_secs(10), || fx.codex.row().is_none()).await,
                "the first turn never released its row"
            );
            let steer = fx.hook(PROMPT3, &t2).await;
            assert!(
                !steer.steer_echo,
                "the steer joined the turn its abandoned worker held"
            );
            super::super::super::super::relay_observed_prompt(&fx.shared, steer).await;
            let anchor = fx.codex.row().expect("the steer claims").user_msg_id;
            fx.append(
                &(user_line_for(PROMPT3)
                    + &item_completed_line_for(&t2)
                    + &answer_for(&t2, RESPONSE3)),
            );
            assert!(
                fx.delivered(RESPONSE3).await,
                "the steer's answer never reached Discord: {:?}",
                fx.requests.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
            fx.assert_answer(RESPONSE, first_anchor);
            fx.assert_answer(RESPONSE3, anchor);
            assert_eq!(fx.completed_turns(), 2);
            assert_eq!(tail_starts(fx.codex.channel), 2);
            fx.assert_released().await;
            fx.finish();
        })
    });
}

/// A real deferral survives a restart: the reloaded pending record and the worker's row keep
/// the native turn, and the row keeps its prompt end after the repair restamps its lease.
#[test]
fn codex_direct_deferred_input_keeps_its_native_turn_across_a_restart() {
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("isolated root");
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    };
    let root = root.path().to_path_buf();
    let (channel, tmux) = (5_705_050_u64, "AgentDesk-codex-5704-durable");
    let (t1, t2) = (turn(1), turn(2));
    // The earlier process: the first input holds the channel before its records, the second defers.
    let anchor = runtime().block_on(async {
        let mut fx = Fixture::start(&root, channel, tmux).await;
        let first = fx.hook(PROMPT, &t1).await;
        super::super::super::super::relay_observed_prompt(&fx.shared, first).await;
        let second = fx.hook(PROMPT2, &t2).await;
        super::super::super::super::relay_observed_prompt(&fx.shared, second).await;
        let pending = fx.pending_starts();
        assert_eq!(pending.len(), 1, "the second input defers: {pending:?}");
        fx.finish();
        pending[0].1
    });
    // The restart drops process memory; the first input was answered before it.
    crate::services::tui_prompt_dedupe::reset_state_for_tests();
    inflight::clear_inflight_state(&ProviderKind::Codex, channel);
    let record = crate::services::discord::tui_direct_pending_start::load_all()
        .into_iter()
        .find(|record| record.channel_id == channel)
        .expect("durable pending record");
    assert_eq!(record.native_turn_id.as_deref(), Some(t2.as_str()));
    let rollout = root.join(format!("{tmux}-rollout.jsonl"));
    append(
        &rollout,
        &(opening(&t1, PROMPT)
            + &item_completed_line_for(&t1)
            + &answer_for(&t1, RESPONSE)
            + &opening(&t2, PROMPT2)),
    );
    let second_end = std::fs::metadata(&rollout).unwrap().len();
    append(&rollout, &item_completed_line_for(&t2));
    runtime().block_on(async {
        let _live_pane = live_pane_tmux(&root);
        let fx = Fixture::start_with(&root, channel, tmux, true).await;
        synthetic_start::restore_pending_starts(&fx.shared, &ProviderKind::Codex);
        assert!(
            wait_for(Duration::from_secs(15), || fx
                .codex
                .row()
                .is_some_and(|row| row.external_turn_id != record.lease_turn_id))
            .await,
            "the restored input was never claimed and repaired"
        );
        let row = fx.codex.row().expect("restored row");
        assert_eq!(row.user_msg_id, anchor);
        assert_eq!(row.native_turn_id.as_deref(), Some(t2.as_str()));
        assert_eq!(row.turn_start_offset, Some(second_end));
        fx.append(&answer_for(&t2, RESPONSE2));
        assert!(
            fx.delivered(RESPONSE2).await,
            "the restored answer never reached Discord: {:?}",
            fx.requests.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        fx.assert_answer(RESPONSE2, anchor);
        assert!(
            fx.sent(RESPONSE).is_empty(),
            "the earlier answer was replayed"
        );
        fx.assert_released().await;
        fx.finish();
    });
}

/// The next turn opens while this turn's tool call is still open: that end is not decoded, so
/// no captured answer finalizes the turn and the row stays for the existing recovery.
#[test]
fn codex_direct_turn_left_with_an_open_tool_is_never_a_captured_answer() {
    run(|root| {
        Box::pin(async move {
            let _live_pane = live_pane_tmux(&root);
            let mut fx = Fixture::start(&root, 5_705_060, "AgentDesk-codex-5704-open-tool").await;
            let (t1, t2) = (turn(1), turn(2));
            let first = fx.hook(PROMPT, &t1).await;
            let end = fx.append(&(opening(&t1, PROMPT) + &item_completed_line_for(&t1)));
            fx.scanned_past(end).await;
            super::super::super::super::relay_observed_prompt(&fx.shared, first).await;
            let anchor = fx.codex.row().expect("claim").user_msg_id;
            assert!(
                wait_for(Duration::from_secs(10), || tail_starts(fx.codex.channel)
                    == 1)
                .await,
                "the tail never started"
            );
            fx.append(
                &(rollout_line(serde_json::json!({"type": "response_item", "payload": {
                    "type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": RESPONSE}]}}))
                    + &rollout_line(serde_json::json!({"type": "response_item", "payload": {
                        "type": "function_call", "name": "shell", "arguments": "{}",
                        "call_id": "call_5704"}}))
                    + &opening(&t2, PROMPT2)
                    + &item_completed_line_for(&t2)
                    + &answer_for(&t2, RESPONSE2)),
            );
            tokio::time::sleep(Duration::from_secs(8)).await;
            assert_eq!(
                fx.completed_turns(),
                0,
                "an undecoded end finalized the turn"
            );
            assert!(
                fx.sent(RESPONSE).is_empty(),
                "an undecoded end was captured"
            );
            assert!(
                fx.sent(RESPONSE2).is_empty(),
                "the next turn's answer leaked"
            );
            assert_eq!(fx.codex.row().map(|row| row.user_msg_id), Some(anchor));
            fx.finish();
        })
    });
}
