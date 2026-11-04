//! Owned fronts into an idle Claude pane through the drive and the provider's own observer, whose
//! transcript is found as the live path finds it: by the account's home or the pane's binding.
use super::*;
use crate::services::agent_protocol::StreamMessage;
use crate::services::tui_prompt_dedupe::{TuiRuntimeBinding, register_tmux_runtime_binding};
use std::io::Write;
use std::sync::mpsc::Sender;

use crate::services::claude_tui::input::submission::tests::wait;
use crate::services::discord::router::message_handler::provider_dispatch::{
    CLAUDE_LEGACY_PROBE, ClaudeLegacyProbe,
};

const ASSISTANT: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"answer {n}"}]}}"#;

/// Each Enter the pane records gets its AI turn: an answer, then the turn's end.
fn answers(rig: &Rig) -> std::thread::JoinHandle<()> {
    let (renderer, transcript) = (rig.renderer.clone(), rig.transcript());
    std::thread::spawn(move || {
        for n in 1..=2 {
            let entered = || events(&renderer).iter().filter(|e| *e == "Enter").count() >= n;
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while !entered() {
                assert!(std::time::Instant::now() < deadline, "Enter {n} never came");
                std::thread::sleep(Duration::from_millis(20));
            }
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .unwrap();
            let answer = ASSISTANT.replace("{n}", &n.to_string());
            writeln!(file, "{answer}\n{}", IDLE.trim_end()).unwrap();
        }
    })
}

/// The pane's runtime binding naming `session`, as the relay rehydrates it after each turn ends.
fn rehydrate(rig: &Rig, session: &str) {
    let binding = TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        output_path: rig.transcript().display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session.to_string()),
        last_offset: 0,
        relay_last_offset: None,
    };
    register_tmux_runtime_binding(&rig.name, binding);
}

/// A's idle handoff settles before its AI turn ends; B uses that pane's binding while the
/// Claude inflight row still has the prelaunch empty output path.
#[test]
fn a_busy_arrival_uses_the_idle_turns_live_binding_before_its_row_handoff() {
    busy_arrival_before_row_handoff(false, false);
}

#[test]
fn queued_and_new_busy_arrivals_merge_once_before_the_idle_turns_row_handoff() {
    busy_arrival_before_row_handoff(true, false);
}

#[test]
fn an_unobserved_busy_arrival_waits_and_starts_once_after_the_turn_boundary() {
    busy_arrival_before_row_handoff(false, true);
}

fn busy_arrival_before_row_handoff(merge: bool, missing: bool) {
    use crate::services::discord::inflight::{InflightTurnState, save_inflight_state};
    use poise::serenity_prelude::{MessageId, UserId};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    fs::create_dir_all(generation.parent().unwrap()).unwrap();
    fs::write(generation, "1").unwrap();
    let rig = rig(
        "oi-busy-arrival",
        7_272_797,
        "normal",
        view("", false),
        IDLE,
    );
    let session = uuid::Uuid::new_v4().to_string();
    rehydrate(&rig, &session);
    let elsewhere = tempfile::tempdir().unwrap();
    let (asks, asked) = std::sync::mpsc::channel::<(Front, Settled)>();
    let mailbox = rig.mailbox.clone();
    let idle: Handoff = Arc::new(move |front| {
        let (mailbox, asks) = (mailbox.clone(), asks.clone());
        Box::pin(async move {
            mailbox
                .restore_active_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(front.1.author_id),
                    MessageId::new(front.1.message_id),
                )
                .await;
            let (settled, answer) = tokio::sync::oneshot::channel();
            asks.send((front, settled)).map_err(|e| e.to_string())?;
            answer.await.map_err(|e| e.to_string())?
        })
    });
    let mut drive = owner_drive(
        (rig.mailbox.clone(), None),
        (ProviderKind::Claude, rig.channel),
        idle,
        None,
    );
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (release_claim, claim_gate) = tokio::sync::oneshot::channel();
    let claim_gate = Arc::new(std::sync::Mutex::new(Some(claim_gate)));
    let (claim_entered, on_claim) = std::sync::mpsc::channel();
    let (record, handoff) = (seen.clone(), drive.handoff.clone());
    Arc::get_mut(&mut drive).unwrap().handoff = Arc::new(move |front| {
        let (record, handoff) = (record.clone(), handoff.clone());
        let gate = if merge && front.1.message_id == 1_567_441_638_974_947_377 {
            claim_gate.lock().unwrap().take()
        } else {
            None
        };
        let entered = claim_entered.clone();
        Box::pin(async move {
            let id = front.1.message_id;
            if let Some(gate) = gate {
                let _ = entered.send(());
                let _ = gate.await;
            }
            let result = handoff(front).await;
            record.lock().unwrap().push((id, result.clone()));
            result
        })
    });
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().mailboxes = rig._registry.clone();
    let persistence = crate::services::turn_orchestrator::QueuePersistenceContext::new(
        &ProviderKind::Claude,
        &shared.token_hash,
        None,
    );
    let queue = crate::services::turn_orchestrator::v2_queue(
        &persistence,
        poise::serenity_prelude::ChannelId::new(rig.channel),
    )
    .unwrap();
    rig.runtime.block_on(async {
        rig.mailbox.v2_adopt(queue.0, queue.1).await.unwrap();
        assert!(matches!(
            admit(&rig.mailbox, &rig.original, rig.ticket.clone()).await,
            crate::services::discord::queue_io::input_owner::admission::Admission::Accepted(_)
        ));
    });
    shared
        .restart
        .reconcile_done
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let (release_pass, first_pass) = tokio::sync::oneshot::channel();
    let first_pass = std::sync::Mutex::new(Some(first_pass));
    let (settled, on_settled) = std::sync::mpsc::channel();
    Arc::get_mut(&mut drive).unwrap().settled = Some(Arc::new(move || {
        let settled = settled.clone();
        let first_pass = first_pass.lock().unwrap().take();
        Box::pin(async move {
            let _ = settled.send(());
            if let Some(first_pass) = first_pass {
                let _ = first_pass.await;
            }
        })
    }));
    {
        let _entered = rig.runtime.enter();
        crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
    }
    let first = asked
        .recv_timeout(Duration::from_secs(20))
        .expect("A's idle handoff");
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        rig.channel,
        None,
        7,
        rig.original.message_id,
        1,
        PROMPT.into(),
        Some(session.clone()),
        Some(rig.name.clone()),
        None,
        None,
        0,
    );
    row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    save_inflight_state(&row).unwrap();
    std::thread::scope(|scope| {
        let first =
            scope.spawn(|| idle_turn(&rig, (session.as_str(), elsewhere.path()), first, |_| None));
        struct CloseOnUnwind(std::path::PathBuf);
        impl Drop for CloseOnUnwind {
            fn drop(&mut self) {
                if let Ok(mut file) = fs::OpenOptions::new().append(true).open(&self.0) {
                    let _ = writeln!(file, "{}", IDLE.trim_end());
                }
            }
        }
        let _close = CloseOnUnwind(rig.transcript());
        on_settled
            .recv_timeout(Duration::from_secs(20))
            .expect("A settled before turn end");
        assert!(
            !first.is_finished(),
            "A's observer still owns its running AI turn"
        );
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(rig.transcript())
            .unwrap();
        writeln!(file, "{}", BUSY.trim_end()).unwrap();
        if missing {
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
        }
        let id = 1_567_441_638_974_947_378;
        let channel = poise::serenity_prelude::ChannelId::new(rig.channel);
        let mut release_pass = Some(release_pass);
        if merge {
            let older =
                crate::services::discord::router::response_format::build_race_requeued_intervention(
                    UserId::new(7),
                    MessageId::new(id - 1),
                    "older queued input",
                    true,
                    None,
                    false,
                    true,
                    vec![],
                );
            let intake = crate::services::discord::queue_io::input_ingress::ADOPTS.scope(
                true,
                crate::services::discord::queue_io::mailbox_enqueue_observed_intervention(
                    &shared,
                    &ProviderKind::Claude,
                    channel,
                    older,
                    None,
                ),
            );
            assert!(rig.runtime.block_on(intake).enqueued);
            release_pass.take().unwrap().send(()).unwrap();
            on_claim
                .recv_timeout(Duration::from_secs(20))
                .expect("older front's handoff in flight");
            assert!(
                rig.runtime
                    .block_on(
                        rig.mailbox
                            .v2_request(|slot| Ok(slot.as_ref().unwrap().front_outstanding()))
                    )
                    .unwrap()
            );
            assert!(
                rig.snapshot()["head_injection"].is_null(),
                "no attempt before the claim"
            );
        }
        let second =
            crate::services::discord::router::response_format::build_race_requeued_intervention(
                UserId::new(7),
                MessageId::new(id),
                "B arrived during A",
                true,
                None,
                false,
                true,
                vec![],
            );
        let intake = crate::services::discord::queue_io::input_ingress::ADOPTS.scope(
            true,
            crate::services::discord::queue_io::mailbox_enqueue_observed_intervention(
                &shared,
                &ProviderKind::Claude,
                channel,
                second,
                None,
            ),
        );
        let intake = rig.runtime.block_on(intake);
        assert!(
            intake.enqueued,
            "B committed through the real ingress: {intake:?}"
        );
        if merge {
            release_claim.send(()).unwrap();
        } else {
            release_pass.take().unwrap().send(()).unwrap();
        }
        let busy_settled = on_settled.recv_timeout(Duration::from_secs(20));
        let before_end = rig.events();
        let kept = rig.snapshot();
        writeln!(
            file,
            "{}\n{}",
            ASSISTANT.replace("{n}", "A"),
            IDLE.trim_end()
        )
        .unwrap();
        let (result, _) = first.join().unwrap();
        assert_eq!(result, Ok(()), "A's observer completes once");
        println!(
            "BUSY_ARRIVAL merge={merge} B_settled={busy_settled:?} handoffs={:?}",
            seen.lock().unwrap()
        );
        assert!(
            busy_settled.is_ok(),
            "the arrival wake must service B before A ends"
        );
        if missing {
            assert_eq!(
                before_end.iter().filter(|e| *e == "paste").count(),
                1,
                "no key for an unobserved pane"
            );
            assert_eq!(kept["items"].as_array().unwrap().len(), 1);
            assert!(kept["head_injection"].is_null());
            assert!(
                seen.lock().unwrap()[1]
                    .1
                    .as_ref()
                    .unwrap_err()
                    .contains("TranscriptUnavailable")
            );
            rehydrate(&rig, &session);
            rig.runtime.block_on(rig.mailbox.finish_turn(
                crate::services::turn_orchestrator::QueuePersistenceContext::new(
                    &ProviderKind::Claude,
                    "input-owner-dispatch",
                    None,
                ),
            ));
            {
                let _entered = rig.runtime.enter();
                crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
            }
            let next = asked
                .recv_timeout(Duration::from_secs(20))
                .expect("B's one idle start after A ends");
            std::thread::scope(|scope| {
                let next = scope.spawn(|| {
                    idle_turn(&rig, (session.as_str(), elsewhere.path()), next, |_| None)
                });
                let _close = CloseOnUnwind(rig.transcript());
                let settled = on_settled.recv_timeout(Duration::from_secs(20));
                writeln!(
                    file,
                    "{}\n{}",
                    ASSISTANT.replace("{n}", "B"),
                    IDLE.trim_end()
                )
                .unwrap();
                assert_eq!(next.join().unwrap().0, Ok(()));
                assert!(settled.is_ok(), "B's idle submit settled once");
            });
            rig.runtime.block_on(rig.mailbox.finish_turn(
                crate::services::turn_orchestrator::QueuePersistenceContext::new(
                    &ProviderKind::Claude,
                    "input-owner-dispatch",
                    None,
                ),
            ));
            {
                let _entered = rig.runtime.enter();
                crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
            }
            on_settled.recv_timeout(Duration::from_secs(20)).unwrap();
            assert_eq!(
                seen.lock().unwrap().len(),
                3,
                "A, B refused busy, B's one idle start"
            );
            assert!(asked.try_recv().is_err(), "B never starts a second time");
            assert_eq!(rig.events().iter().filter(|e| *e == "paste").count(), 2);
            assert!(rig.snapshot()["items"].as_array().unwrap().is_empty());
            return;
        }
        let handed = seen.lock().unwrap();
        assert_eq!(handed.len(), 2, "exactly one handoff for A and the B unit");
        assert!(
            handed.iter().all(|(_, result)| result.is_ok()),
            "{handed:?}"
        );
        let body = fs::read_to_string(rig.renderer.join("body")).unwrap();
        assert!(body.contains("B arrived during A"), "{body}");
        if merge {
            assert!(
                body.find("older queued input").unwrap() < body.find("B arrived during A").unwrap(),
                "{body}"
            );
        }
        let pastes = before_end.iter().filter(|event| *event == "paste").count();
        assert_eq!(
            pastes, 2,
            "A then B each pasted once before A ended; queue={kept}"
        );
        assert!(kept["items"].as_array().unwrap().is_empty(), "{kept}");
        assert!(kept["head_injection"].is_null(), "{kept}");
        assert_eq!(kept["retained_inputs"], json!([]));
        drop(handed);
        rig.runtime.block_on(rig.mailbox.finish_turn(
            crate::services::turn_orchestrator::QueuePersistenceContext::new(
                &ProviderKind::Claude,
                "input-owner-dispatch",
                None,
            ),
        ));
        {
            let _entered = rig.runtime.enter();
            crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
        }
        on_settled
            .recv_timeout(Duration::from_secs(20))
            .expect("turn-end wake settled");
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "the turn end never starts B again"
        );
        assert!(asked.try_recv().is_err(), "no second idle start");
        assert_eq!(
            rig.events(),
            before_end,
            "no duplicate keys after the turn end"
        );
    });
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
}

/// Exercises a runtime refusal through the real owner drive and reads the mock Discord posts.
fn busy_hold_notice(hold: Option<crate::services::discord::queue_io::input_ingress::Hold>) {
    use crate::services::discord::queue_io::input_ingress::Hold;
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 7_272_810
        + hold.map_or(7, |hold| Hold::ALL.iter().position(|h| *h == hold).unwrap()) as u64;
    let before = if hold == Some(Hold::Modal) {
        format!("Run this tool? allow / deny\n{}", view("", false))
    } else {
        view("", false)
    };
    let rig = rig(
        &format!("oi-hold-{channel}"),
        channel,
        "normal",
        before,
        BUSY,
    );
    let id = ChannelId::new(channel);
    let (mut shared, mock) = (
        crate::services::discord::make_shared_data_for_tests(),
        discord_mock::DiscordMockState::new(),
    );
    Arc::get_mut(&mut shared).unwrap().mailboxes = rig._registry.clone();
    shared.restart.reconcile_done.store(
        hold != Some(Hold::RestartRecovery),
        std::sync::atomic::Ordering::Relaxed,
    );
    mock.allow_channel(channel);
    let (proxy, gateway, server) = rig.runtime.block_on(discord_mock::start(mock.clone()));
    let context = rig
        .runtime
        .block_on(discord_mock::serenity_context(proxy, gateway));
    shared.http.cached_serenity_ctx.set(context).unwrap();
    let mut item = rig.original.clone();
    if hold == Some(Hold::Uploads) {
        let persistence = crate::services::turn_orchestrator::QueuePersistenceContext::new(
            &ProviderKind::Claude,
            "input-owner-dispatch",
            None,
        );
        let cancelled = rig.runtime.block_on(
            rig.mailbox
                .cancel_queued_primary_message(MessageId::new(item.message_id), persistence),
        );
        assert!(cancelled.removed.is_some());
        item.message_id += 10;
        item.source_message_ids = vec![item.message_id];
        item.pending_uploads = vec![
            "UPLOAD-RECORD".into();
            crate::services::cluster::attachment_transfer::store::MAX_FILES
                + 1
        ];
        assert!(matches!(
            rig.runtime
                .block_on(admit(&rig.mailbox, &item, AdmissionTicket::new())),
            crate::services::discord::queue_io::input_owner::admission::Admission::Accepted(_)
        ));
    }
    if hold == Some(Hold::ScreenMissing) {
        running_row(&rig, "oi-absent-held-pane");
    } else {
        running_row(&rig, &rig.name);
    }
    rig.runtime.block_on(rig.mailbox.restore_active_turn(
        Arc::new(CancelToken::new()),
        UserId::new(9),
        MessageId::new(1),
    ));
    let idle: Handoff = Arc::new(|_| Box::pin(async { Err("unexpected idle start".into()) }));
    let mut drive = owner_drive(
        (rig.mailbox.clone(), None),
        (ProviderKind::Claude, channel),
        idle,
        Some(shared.clone()),
    );
    if hold == Some(Hold::OwnerChange) {
        let (mailbox, handoff) = (rig.mailbox.clone(), drive.handoff.clone());
        Arc::get_mut(&mut drive).unwrap().handoff = Arc::new(move |front| {
            let (mailbox, handoff) = (mailbox.clone(), handoff.clone());
            Box::pin(async move {
                let stolen = front.clone();
                mailbox
                    .v2_request(move |slot| {
                        let owner = slot.as_mut().ok_or("no owner")?;
                        assert!(owner.claim_turn(stolen.1.message_id, &stolen).is_some());
                        Ok(())
                    })
                    .await?;
                handoff(front).await
            })
        });
    }
    let (settled, on_settled) = std::sync::mpsc::channel();
    Arc::get_mut(&mut drive).unwrap().settled = Some(Arc::new(move || {
        let settled = settled.clone();
        Box::pin(async move {
            let _ = settled.send(());
        })
    }));
    let transition = (hold == Some(Hold::SessionSwitch)).then(|| {
        rig.runtime
            .block_on(shared.session_transition_lock(id).lock_owned())
    });
    let exercise = || {
        for _ in 0..2 {
            {
                let _entered = rig.runtime.enter();
                crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
            }
            on_settled
                .recv_timeout(Duration::from_secs(20))
                .expect("real drive settled");
        }
    };
    if hold == Some(Hold::ScreenContention) {
        crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
            &rig.name, exercise,
        );
    } else {
        exercise();
    }
    drop(transition);
    let texts = mock.written_texts();
    let notices: Vec<_> = texts
        .iter()
        .filter(|t| t.ends_with("즉시주입 하지 못하고 메시지큐에 들어갔습니다"))
        .collect();
    let posts = mock
        .local_note_posts
        .load(std::sync::atomic::Ordering::SeqCst);
    if let Some(hold) = hold {
        assert_eq!(posts, 1, "one POST after repeated real refusals: {texts:?}");
        assert!(
            texts.contains(&hold.notice()),
            "the actual refusal is explained: {texts:?}"
        );
        assert!(
            rig.events().is_empty(),
            "a refused input sent keys: {:?}",
            rig.events()
        );
        assert_eq!(rig.snapshot()["items"].as_array().unwrap().len(), 1);
    } else {
        assert_eq!(posts, 0);
        assert!(
            notices.is_empty(),
            "successful input owes no hold notice: {texts:?}"
        );
        assert_eq!(rig.events().iter().filter(|e| *e == "paste").count(), 1);
    }
    assert!(mock.unhandled.lock().unwrap().is_empty());
    server.abort();
}

#[test]
fn owner_session_switch_posts_one_runtime_hold_notice() {
    busy_hold_notice(Some(
        crate::services::discord::queue_io::input_ingress::Hold::SessionSwitch,
    ));
}

#[test]
fn owner_restart_recovery_posts_one_runtime_hold_notice() {
    busy_hold_notice(Some(
        crate::services::discord::queue_io::input_ingress::Hold::RestartRecovery,
    ));
}

#[test]
fn owner_upload_failure_posts_one_runtime_hold_notice() {
    busy_hold_notice(Some(
        crate::services::discord::queue_io::input_ingress::Hold::Uploads,
    ));
}

#[test]
fn owner_missing_screen_posts_one_runtime_hold_notice() {
    busy_hold_notice(Some(
        crate::services::discord::queue_io::input_ingress::Hold::ScreenMissing,
    ));
}

#[test]
fn owner_modal_posts_one_runtime_hold_notice() {
    busy_hold_notice(Some(
        crate::services::discord::queue_io::input_ingress::Hold::Modal,
    ));
}

#[test]
fn owner_screen_contention_posts_one_runtime_hold_notice() {
    busy_hold_notice(Some(
        crate::services::discord::queue_io::input_ingress::Hold::ScreenContention,
    ));
}

#[test]
fn owner_changed_claim_posts_one_runtime_hold_notice() {
    busy_hold_notice(Some(
        crate::services::discord::queue_io::input_ingress::Hold::OwnerChange,
    ));
}

#[test]
fn owner_success_posts_no_runtime_hold_notice() {
    busy_hold_notice(None);
}

/// A Discord turn on the rig's pane for `front`, resuming `session` from a dir without its transcript;
/// `driver` stands in for the provider's driver when the turn launches.
fn idle_turn(
    rig: &Rig,
    (session, dir): (&str, &Path),
    (front, settled): (Front, Settled),
    driver: impl FnOnce(&Sender<StreamMessage>) -> Option<ClaudeLegacyProbe>,
) -> (Result<(), String>, Vec<StreamMessage>) {
    let claim = rig.claim(&front).expect("granted front");
    let owner = OwnerTurn {
        mailbox: rig.mailbox.clone(),
        claim,
        settled,
        origin: InputOrigin::Discord,
    };
    let (host, provider) = (TurnHost::Tmux, ProviderKind::Claude);
    let turn = StreamingTurn {
        pool: None,
        shared: None,
        provider: &provider,
        prompt: &front.1.text,
        session_id: Some(session),
        working_dir: dir.to_str().unwrap(),
        system_prompt: None,
        allowed_tools: &[],
        cancel: Arc::new(CancelToken::new()),
        codex_observation: None,
        tmux_session_name: Some(&rig.name),
        teardown: None,
        host: &host,
        channel_id: rig.channel,
        model: None,
        native_fast_mode: None,
        effort: None,
        codex_goals: None,
        compact_percent: None,
        compact_lower_bound_tokens: 0,
        compact_token_limit: None,
        cache_ttl_minutes: None,
        dispatch_type: None,
        force_fresh: false,
        submission: None,
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    if let Some(driver) = driver(&sender) {
        *CLAUDE_LEGACY_PROBE.lock().unwrap() = Some((rig.channel, driver));
    }
    let _entered = rig.runtime.enter();
    let result = execute_owned(turn, sender, owner);
    (result, receiver.try_iter().collect())
}

/// After a person's turn ends, one wake hands two queued inputs to an idle pane whose transcript only
/// its binding names: in order, each pasted, entered and answered once, then nothing more.
#[test]
fn an_idle_pane_takes_each_queued_front_once_through_its_own_observer() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oi-idle-d2", 7_272_595, "normal", view("", false), BUSY);
    let second: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":rig.original.message_id + 1,"channel_id":rig.channel,
        "source_message_ids":[rig.original.message_id + 1],"queued_generation":1,
        "text":"second question"}))
    .unwrap();
    let admitted = admit(&rig.mailbox, &second, AdmissionTicket::new());
    rig.runtime.block_on(admitted);
    let session = uuid::Uuid::new_v4().to_string();
    let elsewhere = tempfile::tempdir().unwrap();
    let (fronts, inbox) = std::sync::mpsc::channel::<(Front, Settled)>();
    let fronts = std::sync::Mutex::new(fronts);
    let idle: Handoff = Arc::new(move |front| {
        let (settled, answer) = tokio::sync::oneshot::channel();
        let sent = fronts.lock().unwrap().send((front, settled));
        Box::pin(async move {
            sent.map_err(|e| e.to_string())?;
            answer.await.map_err(|e| e.to_string())?
        })
    });
    let drive = owner_drive(
        (rig.mailbox.clone(), None),
        (ProviderKind::Claude, rig.channel),
        idle,
        None,
    );
    // The person's turn ends; the supervisor's boundary wake is the one event.
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(rig.transcript())
        .unwrap();
    writeln!(file, "{}", IDLE.trim_end()).unwrap();
    let answering = answers(&rig);
    {
        let _entered = rig.runtime.enter();
        crate::services::discord::queue_io::input_owner::drive::wake(drive);
    }
    let mut handed = Vec::new();
    let mut said = Vec::new();
    while let Ok((front, settled)) = inbox.recv_timeout(Duration::from_secs(15)) {
        handed.push(front.1.text.clone());
        rehydrate(&rig, &session);
        let at = (session.as_str(), elsewhere.path());
        let (result, sent) = idle_turn(&rig, at, (front, settled), |_| None);
        assert_eq!(result, Ok(()), "{handed:?}");
        said.extend(sent.into_iter().filter_map(|m| match m {
            StreamMessage::Text { content } => Some(content),
            _ => None,
        }));
        if handed.len() == 2 {
            break;
        }
    }
    answering.join().unwrap();
    assert_eq!(handed, [PROMPT, "second question"], "in order, each once");
    let events = rig.events();
    let count = |key: &str| events.iter().filter(|e| *e == key).count();
    assert_eq!((count("paste"), count("Enter")), (2, 2), "{events:?}");
    let answered = |n: &str| said.iter().filter(|text| text.contains(n)).count();
    assert_eq!(
        (answered("answer 1"), answered("answer 2")),
        (1, 1),
        "{said:?}"
    );
    assert!(
        inbox.recv_timeout(Duration::from_secs(3)).is_err(),
        "nothing more is handed on"
    );
    let snapshot = rig.snapshot();
    assert!(
        snapshot["items"].as_array().unwrap().is_empty(),
        "{snapshot}"
    );
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
}

/// A boot re-adopts a channel's kept V2 queue and wakes its owner drive, which grants the kept front
/// and hands it to the idle start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_boot_recovery_hands_the_kept_front_to_the_owner_drive() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = poise::serenity_prelude::ChannelId::new(7_272_596);
    let persistence = crate::services::discord::queue_dispatch::persistence_context;
    let persisted = persistence(&shared, &shared.provider, channel);
    let queue = crate::services::turn_orchestrator::v2_queue(&persisted, channel).unwrap();
    let kept: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":72_725_961,"channel_id":channel.get(),"source_message_ids":[72_725_961],
        "queued_generation":1,"text":"kept"}))
    .unwrap();
    {
        let before = ChannelMailboxRegistry::default();
        let handle = before.handle(channel);
        handle.v2_adopt(queue.0, queue.1).await.unwrap();
        let _ = admit(&handle, &kept, AdmissionTicket::new()).await;
    }
    recover(&shared, channel).await;
    let mailbox = shared.mailbox(channel);
    let granted =
        || mailbox.v2_request(|slot| Ok(slot.as_ref().is_some_and(|o| o.front_outstanding())));
    let mut handed = false;
    for _ in 0..100 {
        handed = granted().await.unwrap();
        if handed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(handed, "the recovered owner's drive granted its kept front");
}

/// Stands in for the live path's driver after it replaced the stale pane: the pane runs again under
/// the channel's account, and the owner's hook writes the created execution's first prompt.
fn relaunch(
    rig: &Rig,
    (session, dir): (&str, &Path),
    sender: &Sender<StreamMessage>,
) -> ClaudeLegacyProbe {
    let (name, channel, transcript) = (rig.name.clone(), rig.channel, rig.transcript());
    let (real, socket) = (rig.f.real.clone(), rig.f.socket.clone());
    let (ready, events) = (rig.renderer.join("ready"), rig.renderer.join("events"));
    let (sender, session, dir) = (sender.clone(), session.to_string(), dir.to_path_buf());
    Box::new(move || {
        if events.exists() {
            return Err("a key reached the other account's pane".into());
        }
        let default = crate::services::provider_auth_profile::DEFAULT_PROFILE_ID;
        crate::services::tmux_common::write_tmux_session_auth_profile(&name, default)?;
        fs::remove_file(&ready).map_err(|e| e.to_string())?;
        let pane = format!("={name}:");
        let respawn = [
            "-L",
            &socket,
            "-f",
            "/dev/null",
            "respawn-pane",
            "-k",
            "-t",
            &pane,
        ];
        let status = std::process::Command::new(&real).args(respawn).status();
        status
            .map_err(|e| e.to_string())?
            .success()
            .then_some(())
            .ok_or("respawn failed")?;
        wait("relaunched", || ready.exists());
        let hook = scope::launch_hook().ok_or("no owner hook")?;
        let target = scope::created_tmux_target("claude", &name)?;
        let source = json!({"kind":"pre_source","expected_session_id":null,
            "expected_transcript":null,"binding_watermark":null});
        let observe = crate::services::claude_tui::hosting::owner_observer;
        let cancel = Arc::new(CancelToken::new());
        let at = (name.as_str(), PROMPT, channel);
        let until_done = observe(at, (Some(&session), &dir), sender, cancel).ok_or("unobserved")?;
        let path = transcript.display().to_string();
        let submit: scope::CreatedSubmit = Box::new(move |frame| {
            crate::services::claude::first_input::submit_created(&path, 0, &name, &frame).map(drop)
        });
        hook(scope::Lineage::Created, target, source, None, submit)?;
        until_done()
    })
}

/// A live idle pane under another account than the channel's takes no key: the first front goes to
/// the driver's replacement under the channel's account, the second into that pane, each answered once.
#[test]
fn a_pane_under_another_account_is_replaced_before_any_front_is_written() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oi-idle-acct", 7_272_597, "normal", view("", false), BUSY);
    let second: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":rig.original.message_id + 1,"channel_id":rig.channel,
        "source_message_ids":[rig.original.message_id + 1],"queued_generation":1,
        "text":"second question"}))
    .unwrap();
    let admitted = admit(&rig.mailbox, &second, AdmissionTicket::new());
    rig.runtime.block_on(admitted);
    crate::services::tmux_common::write_tmux_session_auth_profile(&rig.name, "account-a").unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let elsewhere = tempfile::tempdir().unwrap();
    let (fronts, inbox) = std::sync::mpsc::channel::<(Front, Settled)>();
    let fronts = std::sync::Mutex::new(fronts);
    let idle: Handoff = Arc::new(move |front| {
        let (settled, answer) = tokio::sync::oneshot::channel();
        let sent = fronts.lock().unwrap().send((front, settled));
        Box::pin(async move {
            sent.map_err(|e| e.to_string())?;
            answer.await.map_err(|e| e.to_string())?
        })
    });
    let drive = owner_drive(
        (rig.mailbox.clone(), None),
        (ProviderKind::Claude, rig.channel),
        idle,
        None,
    );
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(rig.transcript())
        .unwrap();
    writeln!(file, "{}", IDLE.trim_end()).unwrap();
    let answering = answers(&rig);
    {
        let _entered = rig.runtime.enter();
        crate::services::discord::queue_io::input_owner::drive::wake(drive);
    }
    let (mut handed, mut said) = (Vec::new(), Vec::new());
    while let Ok((front, settled)) = inbox.recv_timeout(Duration::from_secs(15)) {
        handed.push(front.1.text.clone());
        rehydrate(&rig, &session);
        let at = (session.as_str(), elsewhere.path());
        let first = handed.len() == 1;
        let driver = |sender: &Sender<StreamMessage>| first.then(|| relaunch(&rig, at, sender));
        let (result, sent) = idle_turn(&rig, at, (front, settled), driver);
        assert_eq!(result, Ok(()), "{handed:?}");
        assert!(
            CLAUDE_LEGACY_PROBE.lock().unwrap().is_none(),
            "the driver ran"
        );
        said.extend(sent.into_iter().filter_map(|m| match m {
            StreamMessage::Text { content } => Some(content),
            _ => None,
        }));
        if handed.len() == 2 {
            break;
        }
    }
    answering.join().unwrap();
    assert_eq!(handed, [PROMPT, "second question"], "in order, each once");
    let events = rig.events();
    let count = |key: &str| events.iter().filter(|e| *e == key).count();
    assert_eq!((count("paste"), count("Enter")), (2, 2), "{events:?}");
    let answered = |n: &str| said.iter().filter(|text| text.contains(n)).count();
    assert_eq!(
        (answered("answer 1"), answered("answer 2")),
        (1, 1),
        "{said:?}"
    );
    let snapshot = rig.snapshot();
    assert!(
        snapshot["items"].as_array().unwrap().is_empty(),
        "{snapshot}"
    );
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
}

/// Synthetic starts keep their own failure channel and wait for idle; neither phase posts a
/// person's queue notice to their synthetic message id.
#[tokio::test]
async fn synthetic_starts_wait_busy_and_owe_no_human_hold_notice() {
    use crate::services::discord::queue_io::input_owner::admission::admit_with_context;
    use crate::services::discord::shared_state::test_rest;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared.restart.reconcile_done.store(true, Ordering::Relaxed);
    for (index, origin) in ["headless", "routine", "scheduled"].into_iter().enumerate() {
        let channel = ChannelId::new(7_272_830 + index as u64);
        let mailbox = shared.mailbox(channel);
        let path = crate::services::discord::runtime_store::discord_pending_queue_root()
            .unwrap()
            .join(format!("synthetic-notice/{channel}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        mailbox
            .v2_adopt(path, "synthetic-notice".into())
            .await
            .unwrap();
        let item: PendingQueueItem = serde_json::from_value(json!({
            "author_id":1, "message_id":8_000_000_000_000_007_000u64 + index as u64,
            "channel_id":channel.get(), "queued_generation":1,"text":"own turn only"
        }))
        .unwrap();
        assert!(matches!(
            admit_with_context(
                &mailbox,
                &item,
                AdmissionTicket::new(),
                json!({"origin":origin})
            )
            .await,
            crate::services::discord::queue_io::input_owner::admission::Admission::Accepted(_)
        ));
        let (requests, _http) = test_rest::recording_mock(100, channel.get()).await;
        let started = Arc::new(AtomicUsize::new(0));
        let counted = started.clone();
        let idle: Handoff = Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err("NotSent(\"idle_unobserved\")".into()) })
        });
        let mut drive = owner_drive(
            (mailbox.clone(), None),
            (ProviderKind::Claude, channel.get()),
            idle,
            Some(shared.clone()),
        );
        let (settled, mut on_settled) = tokio::sync::mpsc::unbounded_channel();
        Arc::get_mut(&mut drive).unwrap().settled = Some(Arc::new(move || {
            let settled = settled.clone();
            Box::pin(async move {
                let _ = settled.send(());
            })
        }));
        crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
        tokio::time::timeout(Duration::from_secs(20), on_settled.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(started.load(Ordering::SeqCst), 1);
        mailbox
            .restore_active_turn(
                Arc::new(CancelToken::new()),
                UserId::new(9),
                MessageId::new(1),
            )
            .await;
        crate::services::discord::queue_io::input_owner::drive::wake(drive);
        tokio::time::timeout(Duration::from_secs(20), on_settled.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            started.load(Ordering::SeqCst),
            1,
            "{origin} waits while busy"
        );
        let snapshot = mailbox
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap();
        assert_eq!(
            snapshot["items"].as_array().unwrap().len(),
            1,
            "{origin} stays queued"
        );
        assert!(snapshot["head_injection"].is_null());
        assert!(
            requests.lock().unwrap().is_empty(),
            "{origin} never posts human hold guidance"
        );
    }
}
