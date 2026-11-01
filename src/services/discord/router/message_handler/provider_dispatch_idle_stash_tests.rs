use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude_tui::input::submission::tests::{PROMPT, events, renderer, view, wait};
use crate::services::claude_tui::{
    busy_inject::StashChannelGuard, guarded_outcome, input, submission_fence,
};
use crate::services::discord::inflight::{
    InflightTurnState, install_managed_submission_boundary, save_inflight_state_create_new,
};
use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
use crate::services::turn_host::TurnHost;
use serenity::all::ChannelId;
use std::path::Path;
use std::time::Duration;

fn managed(
    channel: u64,
    session: &str,
    transcript: &Path,
) -> (InflightTurnState, ManagedSubmission) {
    managed_request(channel, session, transcript, channel + 10, PROMPT)
}

fn managed_request(
    channel: u64,
    session: &str,
    transcript: &Path,
    message_id: u64,
    prompt: &str,
) -> (InflightTurnState, ManagedSubmission) {
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        message_id,
        channel + 11,
        prompt.into(),
        None,
        Some(session.into()),
        Some(transcript.display().to_string()),
        None,
        0,
    );
    row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    let cap = install_managed_submission_boundary(&mut row, &TurnHost::Tmux).unwrap();
    save_inflight_state_create_new(&row).unwrap();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        session,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: transcript.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    (row, cap)
}

fn submit(
    cap: &ManagedSubmission,
    session: &str,
    transcript: &Path,
    warm: bool,
) -> Result<(), String> {
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let (stop, stopped) = std::sync::mpsc::channel();
    let timer_token = token.clone();
    let timer = std::thread::spawn(move || {
        if stopped.recv_timeout(Duration::from_secs(3)).is_err() {
            timer_token
                .cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });
    let result = guarded_outcome::with_owner(Some(cap.observer()), || {
        submission_fence::with_scope(Some(cap.fence()), || {
            if warm {
                input::send_warm_followup_owned(session, PROMPT, Some(&token))
            } else {
                input::send_prompt_with_readiness(
                    session,
                    PROMPT,
                    input::PromptReadinessKind::FreshTurn,
                    Some(&token),
                )
            }
        })
    })
    .map_err(guarded_outcome::SubmissionError::into_legacy);
    let _ = stop.send(());
    timer.join().unwrap();
    if result.is_ok() {
        let root = transcript.parent().unwrap();
        wait("managed Enter", || {
            events(root).iter().any(|event| event == "Enter")
        });
    }
    result
}

fn actual_entry(warm: bool) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let id = if warm { 6_899_902 } else { 6_899_901 };
    let _listed = StashChannelGuard::new(id);
    let name = if warm { "act-warm" } else { "act-fresh" };
    let pane = renderer(&f, name, "normal");
    let (row, cap) = managed(id, name, &pane.join("transcript"));
    let durable_path = crate::services::discord::inflight::inflight_state_path(
        &crate::services::discord::runtime_store::discord_inflight_root().unwrap(),
        &ProviderKind::Claude,
        id,
    );
    std::fs::write(pane.join("inflight"), durable_path.display().to_string()).unwrap();
    let result = submit(&cap, name, &pane.join("transcript"), warm);
    assert!(
        result.is_ok(),
        "actual managed entry must submit without draft_hold: {result:?}"
    );
    assert_eq!(events(&pane), ["C-s", "paste", "Enter"]);
    assert_eq!(
        std::fs::read_to_string(pane.join("phase-at-stash")).unwrap(),
        "may_have_submitted"
    );
    assert_eq!(std::fs::read_to_string(pane.join("body")).unwrap(), PROMPT);
    assert!(!crate::services::claude_tui::composer_lock::draft_guarded(
        name
    ));
    let restored = crate::services::claude_tui::host_input::observe_draft(name).unwrap();
    assert!(
        restored.trim_end().ends_with(view("js", false).trim_end()),
        "active composer was not restored exactly: {restored:?}"
    );
    assert_eq!(
        crate::services::claude_tui::busy_inject::composer_owner(&restored),
        crate::services::claude_tui::busy_inject::ComposerOwner::Person
    );
    assert!(!cap.guarded_hold());
    assert!(matches!(
        cap.classify_failure(),
        crate::services::discord::inflight::SubmissionFailure::IdleStashObserved
    ));
    assert!(
        super::producer_error(Some(&cap), id, "provider failed after receipt".into()).is_some()
    );
    let durable = crate::services::discord::inflight::load_inflight_state(
        &ProviderKind::Claude,
        row.channel_id,
    )
    .unwrap();
    assert!(!durable.managed_submission.unwrap().is_waiting());
}

#[test]
fn managed_idle_person_draft_submits_and_restores_at_general_entry() {
    actual_entry(false);
}
#[test]
fn managed_idle_person_draft_submits_and_restores_at_warm_entry() {
    actual_entry(true);
}

#[tokio::test(flavor = "current_thread")]
async fn successful_writes_do_not_need_receipt_or_restore_and_cannot_resubmit() {
    use crate::services::discord::inflight::SubmissionFailure;
    use crate::services::discord::turn_bridge::followup_requeue::{BusyAttempt, requeue_if_needed};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let shared = crate::services::discord::make_shared_data_for_tests();
    for (n, mode) in ["stash", "paste", "Enter", "never"].into_iter().enumerate() {
        let id = 6_899_920 + n as u64;
        let _listed = StashChannelGuard::new(id);
        let name = format!("act-unknown-{n}");
        let pane = renderer(&f, &name, if mode == "never" { "never" } else { "normal" });
        let (row, cap) = managed(id, &name, &pane.join("transcript"));
        let shim = f.root.path().join("tmux");
        let original = std::fs::read_to_string(&shim).unwrap();
        if mode != "never" {
            let needle = match mode {
                "stash" => "C-s",
                "paste" => "paste-buffer",
                _ => " Enter",
            };
            let tail = format!(
                "exec '{}' -L {} -f /dev/null \"$@\"",
                f.real.display(),
                f.socket
            );
            assert!(original.contains(&tail));
            let hook = format!(
                "case \"$*\" in *'{needle}'*) '{}' -L {} -f /dev/null \"$@\" >/dev/null; exit 0;; esac\n",
                f.real.display(),
                f.socket
            );
            std::fs::write(&shim, original.replace(&tail, &(hook + &tail))).unwrap();
        }
        let result = submit(&cap, &name, &pane.join("transcript"), true);
        std::fs::write(&shim, &original).unwrap();
        assert!(result.is_ok(), "mode={mode}: {result:?}");
        assert!(matches!(
            cap.classify_failure(),
            SubmissionFailure::IdleStashObserved
        ));
        assert!(!crate::services::claude_tui::composer_lock::draft_guarded(
            &name
        ));
        let before = events(&pane);
        let again = submit(&cap, &name, &pane.join("transcript"), false);
        assert!(again.is_err());
        assert_eq!(
            events(&pane),
            before,
            "the same managed attempt cannot submit twice"
        );
        let frame = super::producer_error(
            Some(&cap),
            id,
            "timeout waiting for claude tui follow-up after provider failure".into(),
        );
        assert_eq!(
            frame.is_some(),
            true,
            "Enter completion reaches the provider-error bridge"
        );
        let mut requeued = None;
        requeue_if_needed(
            &mut requeued,
            (true, false),
            (&shared, &ProviderKind::Claude, ChannelId::new(id)),
            &row,
            None,
            None,
            BusyAttempt {
                turn_id: "actual idle attempt",
                cost: Duration::ZERO,
            },
        )
        .await;
        assert!(
            requeued.is_none(),
            "a consumed idle attempt must not regain queue permission"
        );
        assert!(
            crate::services::discord::mailbox_snapshot(&shared, ChannelId::new(id))
                .await
                .intervention_queue
                .is_empty()
        );
    }
}

#[test]
fn restart_records_and_stale_waiting_cannot_issue_new_stash_authority() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let id = 6_899_940;
    let _listed = StashChannelGuard::new(id);
    let name = "act-restart";
    let pane = renderer(&f, name, "normal");
    let (row, cap) = managed(id, name, &pane.join("transcript"));
    let mut stale: InflightTurnState =
        serde_json::from_value(serde_json::to_value(&row).unwrap()).unwrap();
    assert!(submit(&cap, name, &pane.join("transcript"), true).is_ok());
    let actual =
        crate::services::discord::inflight::load_inflight_state(&ProviderKind::Claude, id).unwrap();
    let mut restored: InflightTurnState =
        serde_json::from_value(serde_json::to_value(actual).unwrap()).unwrap();
    assert!(install_managed_submission_boundary(&mut restored, &TurnHost::Tmux).is_none());
    assert!(!restored.managed_submission.as_ref().unwrap().is_waiting());
    for phase in ["unconfirmed", "future-phase"] {
        let mut encoded = serde_json::to_value(&restored).unwrap();
        encoded["managed_submission"]["phase"] = phase.into();
        let mut unknown: InflightTurnState = serde_json::from_value(encoded).unwrap();
        assert!(install_managed_submission_boundary(&mut unknown, &TurnHost::Tmux).is_none());
        assert_eq!(
            serde_json::to_value(unknown).unwrap()["managed_submission"]["phase"],
            phase
        );
    }
    let stale_cap = install_managed_submission_boundary(&mut stale, &TurnHost::Tmux).unwrap();
    let before = events(&pane);
    assert!(submit(&stale_cap, name, &pane.join("transcript"), true).is_err());
    assert_eq!(events(&pane), before);
    assert!(!stale_cap.idle_stash_entered());
    assert!(
        !crate::services::discord::inflight::load_inflight_state(&ProviderKind::Claude, id)
            .unwrap()
            .managed_submission
            .unwrap()
            .is_waiting()
    );
}

#[test]
fn stale_episode_wrong_target_and_fence_write_failure_send_no_key() {
    use crate::services::discord::inflight::fault::{self, FenceWriteFault};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    for (n, kind) in ["stale", "target", "write"].into_iter().enumerate() {
        let id = 6_899_950 + n as u64;
        let _listed = StashChannelGuard::new(id);
        let name = format!("act-refused-{n}");
        let pane = renderer(&f, &name, "normal");
        let (mut row, cap) = managed(id, &name, &pane.join("transcript"));
        if kind == "stale" {
            row.turn_nonce = Some("successor episode".into());
            crate::services::discord::inflight::save_inflight_state(&row).unwrap();
        }
        if kind == "write" {
            fault::arm(id, FenceWriteFault::Write);
        }
        let target = if kind == "target" {
            "wrong-idle-target"
        } else {
            &name
        };
        assert!(submit(&cap, target, &pane.join("transcript"), true).is_err());
        assert!(events(&pane).is_empty(), "{kind}");
        assert!(!cap.idle_stash_entered());
    }
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn held_head_new_input_and_duplicate_wakes_deliver_fifo_once() {
    held_fifo(false, false).await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn capped_held_head_new_input_and_duplicate_wakes_deliver_fifo_once() {
    held_fifo(true, false).await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn capped_held_capture_failure_preserves_fifo_until_reobservation() {
    held_fifo(true, true).await;
}

#[test]
fn wake_observation_separates_failed_reads_and_confirmed_unsupported_geometry() {
    use crate::services::claude_tui::input::submission::wake_hint_observed;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let name = "act-wake-observation";
    let pane = renderer(&f, name, "normal");
    let path = pane.join("transcript");
    let (_, _cap) = managed(6_899_912, name, &path);
    crate::services::claude_tui::composer_lock::guard_draft(
        name,
        crate::services::claude_tui::composer_lock::DraftGuard::DraftRestored,
    );
    assert!(wake_hint_observed(name).unwrap().is_some());
    let target = format!("={name}:");
    f.tmux(&["resize-window", "-t", &target, "-x", "159", "-y", "40"]);
    assert_eq!(wake_hint_observed(name), Ok(None));
    f.tmux(&["resize-window", "-t", &target, "-x", "160", "-y", "40"]);
    f.tmux(&["set-window-option", "-t", &target, "window-size", "latest"]);
    assert_eq!(wake_hint_observed(name), Ok(None));
    f.tmux(&["set-window-option", "-t", &target, "window-size", "manual"]);
    f.tmux(&["resize-window", "-t", &target, "-x", "160", "-y", "40"]);
    let shim = f.root.path().join("tmux");
    let original = std::fs::read_to_string(&shim).unwrap();
    let tail = format!(
        "exec '{}' -L {} -f /dev/null \"$@\"",
        f.real.display(),
        f.socket
    );
    let mode = f.root.path().join("observation-mode");
    let displaced = pane.join("transcript-displaced");
    let hook = format!(
        r#"mode=$(cat '{}')
case "$mode" in
  pin_exit) exit 1;;
  pin_stderr) printf warning >&2; exit 0;;
  pin_malformed) printf unavailable; exit 0;;
esac
for arg in "$@"; do
  if [ "$arg" = capture-pane ]; then
    case "$mode" in
      capture_exit) exit 1;;
      capture_loses_transcript)
        '{}' -L {} -f /dev/null "$@"
        mv '{}' '{}'
        exit 0;;
    esac
  fi
done
"#,
        mode.display(),
        f.real.display(),
        f.socket,
        path.display(),
        displaced.display()
    );
    assert!(original.contains(&tail));
    std::fs::write(&shim, original.replace(&tail, &(hook + &tail))).unwrap();
    for failure in [
        "pin_exit",
        "pin_stderr",
        "pin_malformed",
        "capture_exit",
        "capture_loses_transcript",
    ] {
        std::fs::write(&mode, failure).unwrap();
        assert_eq!(wake_hint_observed(name), Err(()), "{failure}");
        if displaced.exists() {
            std::fs::rename(&displaced, &path).unwrap();
        }
        assert!(events(&pane).is_empty(), "{failure}");
    }
    std::fs::write(shim, original).unwrap();
    assert!(wake_hint_observed(name).unwrap().is_some());
    assert!(events(&pane).is_empty());
}

async fn held_fifo(capped: bool, capture_failure: bool) {
    use crate::services::claude_tui::composer_lock::{DraftGuard, guard_draft};
    use crate::services::discord::turn_bridge::followup_requeue::rewatch_draft_holds;
    use crate::services::discord::{Intervention, mailbox_enqueue_intervention};
    use serenity::all::{MessageId, UserId};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let id = if capture_failure {
        6_899_992
    } else if capped {
        6_899_991
    } else {
        6_899_990
    };
    let channel = ChannelId::new(id);
    let _listed = StashChannelGuard::new(id);
    let name = if capture_failure {
        "act-capped-capture-fifo"
    } else if capped {
        "act-capped-fifo"
    } else {
        "act-fifo"
    };
    let pane = renderer(&f, name, "normal");
    let path = pane.join("transcript");
    let shared = crate::services::discord::make_shared_data_for_tests();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        name,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let mut held = InflightTurnState::new(
        ProviderKind::Claude,
        id,
        None,
        7,
        id + 10,
        id + 11,
        "first held".into(),
        None,
        Some(name.into()),
        None,
        None,
        0,
    );
    held.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    guard_draft(name, DraftGuard::DraftRestored);
    let _initial=crate::services::discord::turn_bridge::followup_requeue::requeue_claude_tui_followup_pre_submit_timeout(
        &shared,&ProviderKind::Claude,channel,&held,None,None,
        crate::services::discord::turn_bridge::followup_requeue::BusyAttempt {turn_id:"held original",cost:Duration::ZERO}).await;
    if capped {
        for _ in 0..crate::services::discord::busy_followup_retry_store::MAX_BUSY_RETRY_COUNT {
            crate::services::discord::busy_followup_retry_store::record_attempt(
                &ProviderKind::Claude,
                (id, id + 10, id + 11),
                &[],
                Duration::ZERO,
                None,
            )
            .unwrap();
        }
        crate::services::discord::busy_followup_retry_store::record_attempt(
            &ProviderKind::Claude,
            (id, id + 10, id + 11),
            &[],
            Duration::ZERO,
            Some(name),
        )
        .unwrap();
        assert!(
            crate::services::discord::busy_followup_retry_store::load(
                &ProviderKind::Claude,
                id,
                id + 10
            )
            .unwrap()
            .exhausted()
        );
    }
    let later = Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(id + 12),
        queued_generation: shared.restart.current_generation,
        source_message_ids: vec![MessageId::new(id + 12)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "later new".into(),
        mode: crate::services::turn_orchestrator::InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    };
    assert!(
        mailbox_enqueue_intervention(&shared, &ProviderKind::Claude, channel, later)
            .await
            .enqueued
    );
    let failed_capture = f.root.path().join("failed-capture");
    if capture_failure {
        let shim = f.root.path().join("tmux");
        let original = std::fs::read_to_string(&shim).unwrap();
        let tail = format!(
            "exec '{}' -L {} -f /dev/null \"$@\"",
            f.real.display(),
            f.socket
        );
        let hook = format!(
            "for arg in \"$@\"; do\n  if [ \"$arg\" = capture-pane ] && [ ! -f '{}' ]; then\n    printf x > '{}'\n    exit 1\n  fi\ndone\n",
            failed_capture.display(),
            failed_capture.display(),
        );
        assert!(original.contains(&tail));
        std::fs::write(shim, original.replace(&tail, &(hook + &tail))).unwrap();
    }
    let dir = pane.clone();
    let idle = path.clone();
    let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = delivered.clone();
    let failed_pass = Arc::new(std::sync::Mutex::new(None));
    let first_pass = failed_pass.clone();
    let _hook = crate::services::discord::queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
        move |shared, provider, ch, _| {
            let (dir, idle, seen, first_pass) =
                (dir.clone(), idle.clone(), seen.clone(), first_pass.clone());
            Box::pin(async move {
                if ch != channel {
                    return None;
                }
                let taken = crate::services::discord::mailbox_take_next_automatic_intervention(
                    &shared, &provider, ch,
                )
                .await;
                if capture_failure {
                    let mut pass = first_pass.lock().unwrap();
                    if pass.is_none() {
                        *pass = Some((
                            taken.intervention.as_ref().map(|input| input.message_id),
                            events(&dir),
                        ));
                    }
                }
                let started = taken.intervention.is_some();
                if let Some(input) = taken.intervention {
                    std::fs::write(dir.join("own"), view(&input.text, true)).unwrap();
                    let (row, cap) =
                        managed_request(id, name, &idle, input.message_id.get(), &input.text);
                    let result = guarded_outcome::with_owner(Some(cap.observer()), || {
                        submission_fence::with_scope(Some(cap.fence()), || {
                            input::send_warm_followup_owned(name, &input.text, None)
                        })
                    });
                    assert!(
                        result.is_ok(),
                        "the real dequeued input must submit: {result:?}"
                    );
                    assert!(!cap.guarded_hold());
                    seen.lock().unwrap().push(input.message_id);
                    assert_eq!(
                        crate::services::discord::inflight::clear_inflight_state_if_matches(
                            &provider,
                            id,
                            row.user_msg_id
                        ),
                        crate::services::discord::inflight::GuardedClearOutcome::Cleared
                    );
                    use std::io::Write;
                    writeln!(
                        std::fs::OpenOptions::new()
                            .append(true)
                            .open(&idle)
                            .unwrap(),
                        "{{\"type\":\"system\",\"subtype\":\"turn_duration\"}}"
                    )
                    .unwrap();
                    let lease = taken.dispatch_lease.unwrap();
                    crate::services::discord::mailbox_abandon_unclaimed_dispatch_after_success(
                        &shared,
                        &provider,
                        ch,
                        input.message_id,
                        lease,
                    )
                    .await;
                    // The real terminal cleanup wakes the remaining queue after projection.
                    crate::services::discord::queue_io::schedule_deferred_idle_queue_kickoff_immediate(
                        shared.clone(), provider.clone(), ch, "native-receipt-terminal-projected",
                    );
                }
                Some(crate::services::discord::IdleQueueKickoffChannelOutcome { started })
            })
        },
    ));
    let live = std::collections::HashMap::from([(
        channel,
        std::collections::HashSet::from([id + 10, id + 12]),
    )]);
    for _ in 0..3 {
        rewatch_draft_holds(&shared, &ProviderKind::Claude, &live);
        crate::services::discord::queue_io::schedule_deferred_idle_queue_kickoff_immediate(
            shared.clone(),
            ProviderKind::Claude,
            channel,
            "new-input-arrived-with-duplicate-wake",
        );
    }
    // Run the ready kickoff before the watch's five-second poll, as a newly arriving input can.
    for _ in 0..25 {
        tokio::task::yield_now().await;
    }
    for poll in 0..8 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        if capped && poll == 0 {
            eprintln!("fifo barrier=watch-release waiting; virtual poll={poll}");
            tokio::task::spawn_blocking(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while crate::services::discord::busy_followup_retry_store::load(
                    &ProviderKind::Claude, id, id + 10,
                ).is_some_and(|budget| budget.exhausted()) {
                    assert!(std::time::Instant::now() < deadline,
                        "fifo barrier=watch-release: native observer did not release the capped head");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }).await.expect("watch release barrier worker");
            eprintln!("fifo barrier=watch-release reached");
        }
        if delivered.lock().unwrap().len() == 2 {
            break;
        }
    }
    let raw = std::fs::read_to_string(&path).unwrap();
    let received: Vec<_> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|row| row["type"] == "user")
        .map(|row| row["message"]["content"].as_str().unwrap().to_string())
        .collect();
    eprintln!(
        "actual receiver capped={capped} ids={:?} bodies={received:?} effects={:?}",
        *delivered.lock().unwrap(),
        events(&pane)
    );
    assert_eq!(received, ["first held", "later new"]);
    assert_eq!(
        *delivered.lock().unwrap(),
        [MessageId::new(id + 10), MessageId::new(id + 12)]
    );
    assert_eq!(
        events(&pane),
        ["C-s", "paste", "Enter", "C-s", "paste", "Enter"]
    );
    if capture_failure {
        assert_eq!(std::fs::read_to_string(failed_capture).unwrap(), "x");
        assert_eq!(
            *failed_pass.lock().unwrap(),
            Some((None, Vec::new())),
            "failed observation must dequeue neither input and cause no effects"
        );
    }
}
#[test]
fn warm_host_recovery_and_busy_wait_reach_managed_idle_submission() {
    use crate::services::claude_tui::hosting::{
        ClaudeTuiWarmFollowupOutcome, FollowupHost, try_claude_tui_warm_followup,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let id = 6_899_910;
    let _listed = StashChannelGuard::new(id);
    let name = "act-full-warm";
    let pane = renderer(&f, name, "normal");
    let path = pane.join("transcript");
    let (_, cap) = managed(id, name, &path);
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let timer_token = token.clone();
    let timer_pane = pane.clone();
    let timer = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !events(&timer_pane).iter().any(|event| event == "Enter")
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(400));
        timer_token
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let (sender, _output) = std::sync::mpsc::channel();
    let result = guarded_outcome::with_owner(Some(cap.observer()), || {
        submission_fence::with_scope(Some(cap.fence()), || {
            try_claude_tui_warm_followup(
                "0b7c4f0e-5d0b-4e57-9d55-3f3a3c1f7a10".into(),
                path.clone(),
                path.display().to_string(),
                true,
                f.root.path(),
                PROMPT,
                sender,
                Some(token),
                &FollowupHost::legacy_tmux(name),
                Some(id),
            )
        })
    });
    timer.join().unwrap();
    assert!(matches!(
        result,
        ClaudeTuiWarmFollowupOutcome::Terminal(Ok(()))
    ));
    assert_eq!(events(&pane), ["C-s", "paste", "Enter"]);
    assert_eq!(std::fs::read_to_string(pane.join("body")).unwrap(), PROMPT);
    let restored = crate::services::claude_tui::host_input::observe_draft(name).unwrap();
    assert!(restored.trim_end().ends_with(view("js", false).trim_end()));
    assert!(!cap.guarded_hold());
    assert!(cap.idle_stash_entered());
}

#[test]
fn unpredicted_body_submits_without_a_draft_hold() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let id = 6_899_999;
    let _listed = StashChannelGuard::new(id);
    let name = "act-unpredicted";
    let pane = renderer(&f, name, "normal");
    let path = pane.join("transcript");
    let prompt = "unmeasured\tcolumn";
    let (_, cap) = managed_request(id, name, &path, id + 10, prompt);
    let result = guarded_outcome::with_owner(Some(cap.observer()), || {
        submission_fence::with_scope(Some(cap.fence()), || {
            input::send_warm_followup_owned(name, prompt, None)
        })
    });
    assert!(result.is_ok(), "{result:?}");
    wait("unpredicted Enter", || events(&pane).len() == 3);
    assert_eq!(events(&pane), ["C-s", "paste", "Enter"]);
    assert_eq!(std::fs::read_to_string(pane.join("body")).unwrap(), prompt);
    assert!(!crate::services::claude_tui::composer_lock::draft_guarded(
        name
    ));
}
