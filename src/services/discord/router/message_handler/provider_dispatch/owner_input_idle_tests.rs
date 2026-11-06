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

fn fresh() -> Arc<CancelToken> {
    Arc::new(CancelToken::new())
}

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
        let first = scope.spawn(|| {
            idle_turn(
                &rig,
                (session.as_str(), elsewhere.path()),
                fresh(),
                first,
                |_| None,
            )
        });
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
                    idle_turn(
                        &rig,
                        (session.as_str(), elsewhere.path()),
                        fresh(),
                        next,
                        |_| None,
                    )
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

#[derive(Clone)]
struct SettlementProbe {
    settled: std::sync::mpsc::Sender<()>,
    created: Arc<std::sync::atomic::AtomicUsize>,
}

static SETTLEMENT_PROBES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<u64, SettlementProbe>>,
> = std::sync::OnceLock::new();

pub(crate) fn observe_settlement(channel: u64, previous: Option<Owed>) -> Option<Owed> {
    let probe = SETTLEMENT_PROBES
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .get(&channel)
        .cloned();
    let Some(probe) = probe else {
        return previous;
    };
    probe
        .created
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Some(Arc::new(move || {
        let (probe, previous) = (probe.clone(), previous.clone());
        Box::pin(async move {
            if let Some(previous) = previous {
                previous().await;
            }
            let _ = probe.settled.send(());
        })
    }))
}

#[test]
fn active_reconcile_completion_resumes_the_busy_owner_once() {
    busy_hold_case(
        Some(crate::services::discord::queue_io::input_ingress::Hold::RestartRecovery),
        Some("recovery"),
    );
}

#[test]
fn standby_reconcile_completion_never_resumes_the_busy_owner() {
    busy_hold_case(
        Some(crate::services::discord::queue_io::input_ingress::Hold::RestartRecovery),
        Some("standby"),
    );
}

#[test]
fn resume_completion_resumes_the_busy_owner_once() {
    busy_hold_case(
        Some(crate::services::discord::queue_io::input_ingress::Hold::SessionSwitch),
        Some("resume"),
    );
}

#[test]
fn clear_completion_resumes_the_busy_owner_once() {
    busy_hold_case(
        Some(crate::services::discord::queue_io::input_ingress::Hold::SessionSwitch),
        Some("clear"),
    );
}

fn finish_command_transition(
    _completion: crate::services::discord::session_transition::SessionTransitionCompletion<'_>,
    _guard: tokio::sync::OwnedMutexGuard<()>,
    failed: bool,
) -> Result<(), &'static str> {
    if failed {
        Err("transition failed")
    } else {
        Ok(())
    }
}

#[test]
fn short_intake_transition_waits_for_a_and_delivers_once() {
    busy_hold_case(
        Some(crate::services::discord::queue_io::input_ingress::Hold::SessionSwitch),
        Some("short_intake"),
    );
}

/// Exercises a runtime refusal through the real owner drive and reads the mock Discord posts.
fn busy_hold_notice(hold: Option<crate::services::discord::queue_io::input_ingress::Hold>) {
    busy_hold_case(hold, None);
}

fn busy_hold_case(
    hold: Option<crate::services::discord::queue_io::input_ingress::Hold>,
    resume: Option<&str>,
) {
    use crate::services::discord::queue_io::input_ingress::Hold;
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = (match resume {
        Some("short_intake") => 7_273_110,
        Some("standby") => 7_273_010,
        Some("clear") => 7_272_940,
        Some(_) => 7_272_910,
        None => 7_272_810,
    }) + hold.map_or(7, |hold| Hold::ALL.iter().position(|h| *h == hold).unwrap())
        as u64;
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
    let (settled_probe, settled_events) = std::sync::mpsc::channel();
    let created = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    if resume.is_some() {
        let persistence = crate::services::discord::queue_dispatch::persistence_context(
            &shared,
            &ProviderKind::Claude,
            id,
        );
        let (path, namespace) =
            crate::services::turn_orchestrator::v2_queue(&persistence, id).unwrap();
        rig.runtime.block_on(async {
            rig.mailbox.v2_adopt(path, namespace).await.unwrap();
            assert!(matches!(
                admit(&rig.mailbox, &rig.original, rig.ticket.clone()).await,
                crate::services::discord::queue_io::input_owner::admission::Admission::Accepted(_)
            ));
        });
        SETTLEMENT_PROBES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(
                channel,
                SettlementProbe {
                    settled: settled_probe,
                    created: created.clone(),
                },
            );
    }
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
    let (idle_started, idle_inbox) = std::sync::mpsc::channel();
    let idle_mailbox = rig.mailbox.clone();
    let waits_for_boundary = resume == Some("short_intake");
    let idle: Handoff = Arc::new(move |front| {
        let (mailbox, started) = (idle_mailbox.clone(), idle_started.clone());
        Box::pin(async move {
            if !waits_for_boundary {
                return Err("unexpected idle start".into());
            }
            mailbox
                .restore_active_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(front.1.author_id),
                    MessageId::new(front.1.message_id),
                )
                .await;
            let (settled, answer) = tokio::sync::oneshot::channel();
            started.send((front, settled)).map_err(|e| e.to_string())?;
            answer.await.map_err(|e| e.to_string())?
        })
    });
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
    if resume.is_none() {
        Arc::get_mut(&mut drive).unwrap().settled = Some(Arc::new(move || {
            let settled = settled.clone();
            Box::pin(async move {
                let _ = settled.send(());
            })
        }));
    }
    let mut completion = match resume {
        Some("resume") => Some(shared.session_transition_completion(id)),
        Some("clear") => Some(
            crate::services::discord::session_transition::SessionTransitionCompletion::new(
                &shared, id,
            ),
        ),
        _ => None,
    };
    let mut intake_transition = (resume == Some("short_intake")).then(|| {
        rig.runtime.block_on(crate::services::discord::router::turn_start::try_intake_runtime_transition_after_redirect(
            &shared, id, (None, false, rig.f.root.path().display().to_string()),
        )).unwrap()
    });
    let mut transition = (hold == Some(Hold::SessionSwitch) && resume != Some("short_intake"))
        .then(|| {
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
    if resume.is_some() {
        rig.runtime.block_on(
            crate::services::discord::router::message_handler::owner_recover(&shared, id),
        );
        settled_events
            .recv_timeout(Duration::from_secs(20))
            .expect("boot wake refusal settled");
    } else if hold == Some(Hold::ScreenContention) {
        crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
            &rig.name, exercise,
        );
    } else {
        exercise();
    }
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
    if let Some(resume) = resume {
        assert!(rig.snapshot()["head_injection"].is_null());
        assert!(
            rig.snapshot()["retained_inputs"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let before = created.load(std::sync::atomic::Ordering::SeqCst);
        match resume {
            "recovery" => rig.runtime.block_on(
                crate::services::discord::runtime_bootstrap::complete_active_reconcile(&shared),
            ),
            "standby" => {
                let health = Arc::new(crate::services::discord::health::HealthRegistry::new());
                rig.runtime.block_on(crate::services::discord::runtime_bootstrap::register_standby_and_settle_reconcile(&health, &ProviderKind::Claude, &shared));
            }
            "resume" | "clear" => {}
            "short_intake" => rig.runtime.block_on(
                intake_transition
                    .take()
                    .unwrap()
                    .complete_mailbox_claim(async {}),
            ),
            _ => unreachable!(),
        }
        {
            let _entered = rig.runtime.enter();
            if matches!(resume, "resume" | "clear") {
                let failed = resume == "resume";
                assert_eq!(
                    finish_command_transition(
                        completion.take().unwrap(),
                        transition.take().unwrap(),
                        failed
                    )
                    .is_err(),
                    failed
                );
            }
            drop(transition);
            drop(completion);
        }
        if resume == "short_intake" {
            assert_eq!(
                created.load(std::sync::atomic::Ordering::SeqCst),
                before,
                "short transition release adds no owner wake"
            );
            assert!(rig.events().is_empty());
            assert_eq!(rig.snapshot()["items"].as_array().unwrap().len(), 1);
            let session = uuid::Uuid::new_v4().to_string();
            let elsewhere = tempfile::tempdir().unwrap();
            rehydrate(&rig, &session);
            let mut transcript = fs::OpenOptions::new()
                .append(true)
                .open(rig.transcript())
                .unwrap();
            writeln!(transcript, "{}", IDLE.trim_end()).unwrap();
            rig.runtime.block_on(rig.mailbox.finish_turn(
                crate::services::discord::queue_dispatch::persistence_context(
                    &shared,
                    &ProviderKind::Claude,
                    id,
                ),
            ));
            {
                let _entered = rig.runtime.enter();
                crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
            }
            let front = idle_inbox
                .recv_timeout(Duration::from_secs(20))
                .expect("B starts once at A's boundary");
            std::thread::scope(|scope| {
                let started = scope.spawn(|| {
                    idle_turn(
                        &rig,
                        (session.as_str(), elsewhere.path()),
                        fresh(),
                        front,
                        |_| None,
                    )
                });
                let settled = settled_events.recv_timeout(Duration::from_secs(20));
                writeln!(
                    transcript,
                    "{}\n{}",
                    ASSISTANT.replace("{n}", "B"),
                    IDLE.trim_end()
                )
                .unwrap();
                assert_eq!(started.join().unwrap().0, Ok(()));
                assert!(settled.is_ok(), "B's boundary input settled");
            });
            assert_eq!(rig.events().iter().filter(|e| *e == "paste").count(), 1);
            assert!(rig.snapshot()["items"].as_array().unwrap().is_empty());
            rig.runtime.block_on(rig.mailbox.finish_turn(
                crate::services::discord::queue_dispatch::persistence_context(
                    &shared,
                    &ProviderKind::Claude,
                    id,
                ),
            ));
            {
                let _entered = rig.runtime.enter();
                crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
            }
            settled_events
                .recv_timeout(Duration::from_secs(20))
                .expect("repeat boundary settled");
            assert!(idle_inbox.try_recv().is_err());
            assert_eq!(rig.events().iter().filter(|e| *e == "paste").count(), 1);
            assert_eq!(
                mock.local_note_posts
                    .load(std::sync::atomic::Ordering::SeqCst),
                1
            );
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
        } else if resume == "standby" {
            assert_eq!(
                created.load(std::sync::atomic::Ordering::SeqCst),
                before,
                "standby mark must not even construct a drive"
            );
            assert_eq!(rig.events().iter().filter(|e| *e == "paste").count(), 0);
            assert_eq!(rig.snapshot()["items"].as_array().unwrap().len(), 1);
        } else {
            settled_events
                .recv_timeout(Duration::from_secs(20))
                .expect("production completion must settle a busy paste without a manual wake");
            assert!(
                rig.runtime.block_on(rig.mailbox.has_active_turn()).unwrap(),
                "A remains active"
            );
            assert_eq!(rig.events().iter().filter(|e| *e == "paste").count(), 1);
            assert!(rig.snapshot()["items"].as_array().unwrap().is_empty());
            assert_eq!(
                mock.local_note_posts
                    .load(std::sync::atomic::Ordering::SeqCst),
                1
            );
            rig.runtime.block_on(async {
                rig.mailbox
                    .finish_turn(
                        crate::services::discord::queue_dispatch::persistence_context(
                            &shared,
                            &ProviderKind::Claude,
                            id,
                        ),
                    )
                    .await;
                crate::services::discord::router::message_handler::owner_wake(&shared, id);
            });
            settled_events
                .recv_timeout(Duration::from_secs(20))
                .expect("A boundary settled");
            assert_eq!(
                rig.events().iter().filter(|e| *e == "paste").count(),
                1,
                "no repeat at A end"
            );
        }
        SETTLEMENT_PROBES
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .remove(&channel);
    } else {
        drop(transition);
        drop(completion);
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
    cancel: Arc<CancelToken>,
    (front, settled): (Front, Settled),
    driver: impl FnOnce(&Sender<StreamMessage>) -> Option<ClaudeLegacyProbe>,
) -> (Result<(), String>, Vec<StreamMessage>) {
    let claim = rig.claim(&front).expect("granted front");
    let owner = OwnerTurn {
        mailbox: rig.mailbox.clone(),
        runtime: None,
        claim,
        settled,
        origin: InputOrigin::Discord,
        late: Default::default(),
        refusal: Default::default(),
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
        cancel,
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
        let (result, sent) = idle_turn(&rig, at, fresh(), (front, settled), |_| None);
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

struct RestoreObserverRegistry(Option<Arc<crate::services::discord::health::HealthRegistry>>);

impl Drop for RestoreObserverRegistry {
    fn drop(&mut self) {
        let restored = self
            .0
            .take()
            .unwrap_or_else(|| Arc::new(crate::services::discord::health::HealthRegistry::new()));
        crate::engine::ops::turn_ops::set_global_health_registry(&restored);
    }
}

struct AttachedOwner {
    rig: Rig,
    shared: Arc<crate::services::discord::SharedData>,
    home: std::path::PathBuf,
    drive: Arc<Drive>,
    fronts: Inbox,
    passes: std::sync::mpsc::Receiver<()>,
    wakes: Arc<std::sync::atomic::AtomicUsize>,
    // Restore the prior registry after this fixture's runtime has stopped.
    _registry_restore: RestoreObserverRegistry,
}

impl AttachedOwner {
    fn new(name: &str, channel: u64, transcript: &str) -> Self {
        let registry_restore =
            RestoreObserverRegistry(crate::engine::ops::turn_ops::health_registry());
        let empty = Arc::new(crate::services::discord::health::HealthRegistry::new());
        crate::engine::ops::turn_ops::set_global_health_registry(&empty);
        drop(empty);
        let mut rig = rig(name, channel, "normal", view("", false), transcript);
        let entered = rig.runtime.enter();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let mailbox = shared.mailbox(poise::serenity_prelude::ChannelId::new(channel));
        drop(entered);
        let path = crate::services::discord::runtime_store::discord_pending_queue_root()
            .unwrap()
            .join("claude/input-owner-attached")
            .join(format!("{channel}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        rig.runtime.block_on(async {
            mailbox
                .v2_adopt(path, "attached-owner".into())
                .await
                .unwrap();
            let accepted = admit(&mailbox, &rig.original, rig.ticket.clone()).await;
            assert!(matches!(
                accepted,
                crate::services::discord::queue_io::input_owner::admission::Admission::Accepted(_)
            ));
        });
        rig.mailbox = mailbox;
        let (drive, fronts) = rig.drive();
        let (finished, passes) = std::sync::mpsc::channel();
        let settled = Arc::new(std::sync::Mutex::new(finished));
        let drive = Arc::new(Drive {
            mailbox: drive.mailbox.clone(),
            channel,
            handoff: drive.handoff.clone(),
            settled: Some(Arc::new(move || {
                settled.lock().unwrap().send(()).unwrap();
                Box::pin(async {})
            })),
        });
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (wake_drive, counted) = (drive.clone(), wakes.clone());
        crate::services::discord::tui_prompt_relay::rehydration::OBSERVER_KICKOFF_FOR_TESTS
            .lock()
            .unwrap()
            .insert(
                channel,
                Arc::new(move || {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    crate::services::discord::queue_io::input_owner::drive::wake(
                        wake_drive.clone(),
                    );
                }),
            );
        let home = rig.f.root.path().join("observer-home");
        Self {
            rig,
            shared,
            home,
            drive,
            fronts,
            passes,
            wakes,
            _registry_restore: registry_restore,
        }
    }

    fn launch_source(&self) -> (String, std::path::PathBuf) {
        let session = uuid::Uuid::new_v4().to_string();
        let project = self.rig.f.root.path().join("observer-project");
        fs::create_dir_all(&project).unwrap();
        let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            &project,
            &session,
            Some(&self.home),
        )
        .unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(self.rig.transcript(), &path).unwrap();
        let context = crate::config::runtime_root()
            .unwrap()
            .join("runtime/binding_contexts/claude")
            .join(format!("{}.json", self.rig.nonce));
        let script = crate::services::tmux_common::session_temp_path(
            &self.rig.name,
            crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
        );
        fs::write(script, format!(
            "export AGENTDESK_BINDING_CONTEXT='{}'\ncd '{}'\nexec 'claude' '--session-id' '{session}'\n",
            context.display(), project.display(),
        )).unwrap();
        (session, path)
    }

    fn attach(&self) -> bool {
        let _entered = self.rig.runtime.enter();
        let pass = crate::services::discord::tui_prompt_relay::rehydrate_owner_pane_for_tests(
            &self.shared,
            &self.rig.name,
            self.rig.channel,
            &self.home,
        );
        let spawned = pass.is_some();
        if let Some(pass) = pass {
            self.rig.runtime.block_on(pass).unwrap();
        }
        spawned
    }

    fn serve(&self) -> Result<(), String> {
        self.serve_before(|| {})
    }

    fn serve_before(&self, before_refusal: impl FnOnce()) -> Result<(), String> {
        let (front, settled) = self
            .fronts
            .recv_timeout(Duration::from_secs(10))
            .expect("the owner's drive hands the front once");
        let claim = self.rig.claim(&front).expect("the real front claim");
        let provider = ProviderKind::Claude;
        let mut owner = self.rig.registration(
            &provider,
            target::Phase::Idle,
            &self.rig.transcript(),
            Execution::LocalTui,
        );
        // The provider builds its real observer; only reading the ensuing AI turn is immediate.
        let observed = {
            let _entered = self.rig.runtime.enter();
            crate::services::claude_tui::hosting::owner_observer(
                (&self.rig.name, &front.1.text, self.rig.channel),
                (None, self.rig.f.root.path()),
                std::sync::mpsc::channel().0,
                Arc::new(CancelToken::new()),
            )
        };
        owner.until_done = observed.map(|_| Box::new(|| Ok(())) as super::super::UntilDone);
        before_refusal();
        self.rig
            .with_turn(&provider, Some(&self.rig.name), |turn, sender| {
                execute_front(turn, sender, claim, owner, settled)
            })
    }

    fn settled(&self) {
        self.finished_pass();
        assert!(self.fronts.try_recv().is_err(), "no additional handoff");
    }

    fn finished_pass(&self) {
        self.passes
            .recv_timeout(Duration::from_secs(10))
            .expect("the drive's settled callback completes the pass");
    }

    fn wakes(&self) -> usize {
        self.wakes.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn keys(&self) -> (usize, usize) {
        let trace = self
            .rig
            .f
            .root
            .path()
            .join(format!("{}-owner/trace", self.rig.name));
        let trace = fs::read_to_string(trace).unwrap_or_default();
        let paste = trace
            .lines()
            .filter(|line| line.contains("paste-buffer"))
            .count();
        let enter = trace
            .lines()
            .filter(|line| line.contains("send-keys") && line.contains(" Enter"))
            .count();
        (paste, enter)
    }
}

impl Drop for AttachedOwner {
    fn drop(&mut self) {
        crate::services::discord::tui_prompt_relay::rehydration::OBSERVER_KICKOFF_FOR_TESTS
            .lock()
            .unwrap()
            .remove(&self.rig.channel);
        crate::services::discord::tui_prompt_relay::rehydration::OBSERVER_WAITING_FOR_TESTS
            .lock()
            .unwrap()
            .remove(&self.rig.channel);
        crate::services::discord::tui_prompt_relay::rehydration::OBSERVER_BUSY_FOR_TESTS
            .lock()
            .unwrap()
            .remove(&self.rig.channel);
        crate::services::discord::tui_prompt_relay::rehydration::OBSERVER_PUBLICATION_FOR_TESTS
            .lock()
            .unwrap()
            .remove(&self.rig.name);
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&self.rig.name);
    }
}

/// A source attaching after an idle refusal is the one new event; it delivers the same front once.
#[test]
fn an_observer_attaching_after_idle_refusal_wakes_the_owner_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = AttachedOwner::new("oi-attach-late", 7_676_601, IDLE);
    let _trace = owner.rig.trace_driver();
    owner.rig.wake(&owner.drive);
    assert_eq!(owner.serve(), Err("NotSent(\"idle_unobserved\")".into()));
    owner.settled();
    assert_eq!(owner.keys(), (0, 0), "the refusal sent no keys");
    assert!(owner.rig.snapshot()["head_injection"].is_null());
    assert_eq!(owner.wakes(), 0);

    owner.launch_source();
    assert!(
        owner.attach(),
        "the published idle source schedules its owner check"
    );
    assert_eq!(
        owner.wakes(),
        1,
        "one attachment produces exactly one wake; keys={:?}",
        owner.keys()
    );
    assert_eq!(owner.serve(), Ok(()));
    owner.settled();
    assert_eq!(owner.keys(), (1, 1), "one paste and one Enter");
    owner.rig.delivered();
    assert!(!owner.attach(), "an unchanged binding adds no wake");
    assert_eq!(owner.wakes(), 1);
}

/// The common publication event reaches the registered runtime without the rehydrate fallback.
#[test]
fn a_published_observer_wakes_its_registered_owner_once() {
    use crate::engine::ops::turn_ops::{health_registry, set_global_health_registry};
    use crate::services::discord::health::HealthRegistry;

    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = AttachedOwner::new("oi-attach-published", 7_676_605, IDLE);
    let _trace = owner.rig.trace_driver();
    let registry = Arc::new(HealthRegistry::new());
    owner
        .rig
        .runtime
        .block_on(registry.register("claude".into(), owner.shared.clone()));
    let _restore = RestoreObserverRegistry(health_registry());
    set_global_health_registry(&registry);
    owner.rig.wake(&owner.drive);
    assert_eq!(owner.serve(), Err("NotSent(\"idle_unobserved\")".into()));
    owner.settled();
    assert_eq!(owner.keys(), (0, 0));

    owner.launch_source();
    assert!(
        !owner.attach(),
        "the registered runtime uses the publication event"
    );
    assert_eq!(owner.serve(), Ok(()));
    owner.settled();
    assert_eq!(owner.wakes(), 1, "the common publication wakes once");
    assert_eq!(owner.keys(), (1, 1));
    owner.rig.delivered();
}

/// A source the owner already observed adds no wake on reattachment or after the front settles.
#[test]
fn an_existing_observer_and_an_empty_front_add_no_attachment_wake() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = AttachedOwner::new("oi-attach-known", 7_676_602, IDLE);
    let _trace = owner.rig.trace_driver();
    let session = uuid::Uuid::new_v4().to_string();
    rehydrate(&owner.rig, &session);
    assert!(!owner.attach(), "the existing source is unchanged");
    assert_eq!(owner.wakes(), 0);
    owner.rig.wake(&owner.drive);
    assert_eq!(owner.serve(), Ok(()));
    owner.settled();
    assert_eq!(owner.keys(), (1, 1));
    owner.rig.delivered();
    let empty = owner
        .rig
        .runtime
        .block_on(
            owner
                .rig
                .mailbox
                .v2_request(|slot| Ok(slot.as_ref().is_some_and(|owner| owner.front().is_none()))),
        )
        .unwrap();
    assert!(empty, "the existing owner has no front after delivery");

    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&owner.rig.name);
    let (session, path) = owner.launch_source();
    // The renderer logs a user but no AI completion; this new session has its own idle transcript.
    fs::remove_file(&path).unwrap();
    fs::write(&path, format!("{}\n{IDLE}", json!({"sessionId": &session}))).unwrap();
    assert_eq!(
        crate::services::tui_turn_state::observe_provider_jsonl_turn_state(
            &ProviderKind::Claude,
            &path,
        ),
        crate::services::tui_turn_state::TuiTurnState::Idle,
    );
    assert!(
        owner.attach(),
        "an attachment checks the now-empty idle owner"
    );
    let attached =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&owner.rig.name)
            .expect("the ready source was attached");
    assert_eq!(attached.session_id.as_deref(), Some(session.as_str()));
    assert_eq!(Path::new(&attached.output_path), path.as_path());
    assert_eq!(owner.wakes(), 0, "no front means no wake");
    assert!(owner.fronts.try_recv().is_err());
    assert_eq!(
        owner.keys(),
        (1, 1),
        "empty attachment sends no additional keys"
    );
}

/// Attaching to a busy source waits for that turn's existing Idle transition.
#[test]
fn a_busy_source_attachment_does_not_wake_the_owner() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = AttachedOwner::new("oi-attach-busy", 7_676_603, BUSY);
    owner.launch_source();
    assert!(
        owner.attach(),
        "the source check runs without an idle-submit event"
    );
    assert_eq!(owner.wakes(), 0);
    assert!(owner.fronts.try_recv().is_err());
    assert_eq!(owner.keys(), (0, 0));
}

/// An attachment while the refused handoff is still outstanding coalesces into its next pass.
#[test]
fn an_observer_attaching_during_the_idle_refusal_preserves_its_wake() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = AttachedOwner::new("oi-attach-race", 7_676_604, IDLE);
    let _trace = owner.rig.trace_driver();
    owner.rig.wake(&owner.drive);
    owner.launch_source();
    let refused = owner.serve_before(|| {
        assert!(owner.attach());
        assert_eq!(owner.wakes(), 1);
    });
    assert_eq!(refused, Err("NotSent(\"idle_unobserved\")".into()));
    owner.finished_pass();
    assert_eq!(owner.keys(), (0, 0));
    assert_eq!(owner.serve(), Ok(()));
    owner.settled();
    assert_eq!(owner.keys(), (1, 1));
    assert_eq!(owner.wakes(), 1);
    owner.rig.delivered();
}

/// An idle source attaching before the refused turn releases its token wakes after that release.
#[test]
fn an_attached_observer_waits_for_the_refused_turn_token_to_finish() {
    use crate::services::turn_orchestrator::QueuePersistenceContext;
    use poise::serenity_prelude::{MessageId, UserId};

    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = AttachedOwner::new("oi-attach-token", 7_676_606, IDLE);
    let _trace = owner.rig.trace_driver();
    owner.rig.wake(&owner.drive);
    assert_eq!(owner.serve(), Err("NotSent(\"idle_unobserved\")".into()));
    owner.settled();
    assert_eq!(owner.keys(), (0, 0));

    let token = Arc::new(CancelToken::new());
    assert!(owner.rig.runtime.block_on(owner.rig.mailbox.try_start_turn(
        token.clone(),
        UserId::new(7),
        MessageId::new(9),
    )));
    let (waiting, observed_wait) = std::sync::mpsc::channel();
    let waiting = std::sync::Mutex::new(waiting);
    crate::services::discord::tui_prompt_relay::rehydration::OBSERVER_WAITING_FOR_TESTS
        .lock()
        .unwrap()
        .insert(
            owner.rig.channel,
            Arc::new(move || {
                waiting.lock().unwrap().send(()).unwrap();
            }),
        );
    owner.launch_source();
    let pending = {
        let _entered = owner.rig.runtime.enter();
        crate::services::discord::tui_prompt_relay::rehydrate_owner_pane_for_tests(
            &owner.shared,
            &owner.rig.name,
            owner.rig.channel,
            &owner.home,
        )
        .expect("the attached source checks its active owner")
    };
    observed_wait
        .recv_timeout(Duration::from_secs(10))
        .expect("the attachment observed the active token before its finish latch wait");
    assert!(
        owner
            .rig
            .runtime
            .block_on(owner.rig.mailbox.has_active_turn())
            .unwrap()
    );
    assert_eq!(
        owner.wakes(),
        0,
        "the active turn has not released its token"
    );
    let released =
        owner
            .rig
            .runtime
            .block_on(owner.rig.mailbox.finish_turn(QueuePersistenceContext::new(
                &ProviderKind::Claude,
                "attached-owner",
                None,
            )));
    assert!(
        released
            .removed_token
            .is_some_and(|removed| Arc::ptr_eq(&removed, &token))
    );
    owner.rig.runtime.block_on(pending).unwrap();
    assert_eq!(owner.wakes(), 1);
    assert_eq!(owner.serve(), Ok(()));
    owner.settled();
    assert_eq!(owner.keys(), (1, 1));
    owner.rig.delivered();
}

/// Resolving a previously missing exact source wakes once even though its binding key is unchanged.
#[test]
fn a_pending_observer_becoming_readable_wakes_after_the_refused_token_finishes() {
    pending_observer_becomes_readable(false);
}

/// A hook verifying the held exact source emits the same one owner-readiness event.
#[test]
fn a_hook_verifying_a_pending_observer_wakes_after_the_refused_token_finishes() {
    pending_observer_becomes_readable(true);
}

fn pending_observer_becomes_readable(via_hook: bool) {
    use crate::engine::ops::turn_ops::{health_registry, set_global_health_registry};
    use crate::services::discord::health::HealthRegistry;
    use crate::services::tui_prompt_dedupe::binding_events::{
        BindingTarget, CauseSource, HookSignal, PendingRecord, Proposal,
    };
    use crate::services::tui_prompt_dedupe::pending::{PendingRestore, last_restore_outcome};
    use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events};
    use crate::services::turn_orchestrator::QueuePersistenceContext;
    use poise::serenity_prelude::{MessageId, UserId};

    struct BindingLogScope(Option<std::path::PathBuf>, u64);
    impl Drop for BindingLogScope {
        fn drop(&mut self) {
            binding_events::forget_channel_for_tests(self.1);
            binding_events::set_test_root(self.0.as_deref());
        }
    }

    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 7_676_607 + u64::from(via_hook);
    let name = if via_hook {
        "oi-attach-hook"
    } else {
        "oi-attach-pending"
    };
    let _binding_log = BindingLogScope(binding_events::test_root(), channel);
    binding_events::set_test_root(crate::config::runtime_root().as_deref());
    let owner = AttachedOwner::new(name, channel, IDLE);
    let _trace = owner.rig.trace_driver();
    let registry = Arc::new(HealthRegistry::new());
    owner
        .rig
        .runtime
        .block_on(registry.register("claude".into(), owner.shared.clone()));
    let _restore = RestoreObserverRegistry(health_registry());
    set_global_health_registry(&registry);
    let (session, path) = owner.launch_source();
    let command_session = if via_hook {
        let command = uuid::Uuid::new_v4().to_string();
        let script = crate::services::tmux_common::session_temp_path(
            &owner.rig.name,
            crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
        );
        let old = fs::read_to_string(&script).unwrap();
        let launch = format!("'--session-id' '{session}'");
        assert!(old.contains(&launch));
        fs::write(
            &script,
            old.replacen(&launch, &format!("'--session-id' '{command}'"), 1),
        )
        .unwrap();
        command
    } else {
        session.clone()
    };
    fs::remove_file(&path).unwrap();
    let missing = TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session.clone()),
        last_offset: 0,
        relay_last_offset: None,
    };
    let proposal = Proposal::for_binding(
        Some(owner.rig.channel),
        &owner.rig.name,
        &missing,
        None,
        CauseSource::Observed,
    )
    .unwrap();
    assert_eq!(
        binding_events::record_pending(&proposal).unwrap(),
        PendingRecord::Recorded
    );
    assert!(
        !owner.attach(),
        "a missing exact source cannot start an observer"
    );
    let held = dedupe::runtime_binding_for_tmux_session(&owner.rig.name)
        .expect("the durable Pending publishes its still-missing exact binding");
    assert_eq!(held.output_path, missing.output_path);
    assert_eq!(
        dedupe::resolve_tmux_session_name("claude", &command_session).as_deref(),
        Some(owner.rig.name.as_str()),
        "the real restore kept the launch command's routing alias",
    );
    assert!(matches!(
        last_restore_outcome(&owner.rig.name),
        Some(PendingRestore::BoundFromLedger {
            exact_wait: Some(_),
            ..
        })
    ));
    assert_eq!(owner.wakes(), 0);

    owner.rig.wake(&owner.drive);
    assert_eq!(owner.serve(), Err("NotSent(\"idle_unobserved\")".into()));
    owner.settled();
    assert_eq!(owner.keys(), (0, 0));
    let token = Arc::new(CancelToken::new());
    assert!(owner.rig.runtime.block_on(owner.rig.mailbox.try_start_turn(
        token.clone(),
        UserId::new(7),
        MessageId::new(9),
    )));
    let (waiting, observed_wait) = std::sync::mpsc::channel();
    let waiting = std::sync::Mutex::new(waiting);
    crate::services::discord::tui_prompt_relay::rehydration::OBSERVER_WAITING_FOR_TESTS
        .lock()
        .unwrap()
        .insert(
            owner.rig.channel,
            Arc::new(move || {
                waiting.lock().unwrap().send(()).unwrap();
            }),
        );
    // The same exact path now opens on its own native first record; its source key stays unchanged.
    fs::write(
        owner.rig.transcript(),
        format!("{{\"type\":\"mode\",\"sessionId\":\"{session}\"}}\n{IDLE}"),
    )
    .unwrap();
    std::os::unix::fs::symlink(owner.rig.transcript(), &path).unwrap();
    if via_hook {
        let hook = HookSignal::from_payload("stop", &json!({"transcript_path":path}));
        let adopted = {
            let _entered = owner.rig.runtime.enter();
            dedupe::adopt_claude_continuation_explained(&command_session, &session, &hook).unwrap()
        };
        assert_eq!(
            adopted,
            (
                Some((owner.rig.name.clone(), path.display().to_string())),
                None
            )
        );
    } else {
        assert!(
            !owner.attach(),
            "the registered runtime uses the resolved-source event"
        );
    }
    let ready = dedupe::runtime_binding_for_tmux_session(&owner.rig.name).unwrap();
    let source = |binding: &TuiRuntimeBinding| {
        (
            binding.runtime_kind,
            binding.session_id.clone(),
            binding.output_path.clone(),
        )
    };
    assert_eq!(
        source(&ready),
        source(&held),
        "readability changed without a new binding key"
    );
    match last_restore_outcome(&owner.rig.name) {
        Some(PendingRestore::BoundFromLedger { exact_wait, .. }) => {
            assert_eq!(
                exact_wait.is_some(),
                via_hook,
                "the hook resolves the durable log without running the restore pass"
            );
        }
        other => panic!("the held source's restore outcome changed: {other:?}"),
    }
    let resolved = || {
        binding_events::records_strict(owner.rig.channel)
            .unwrap()
            .unwrap()
            .into_iter()
            .filter(|event| {
                matches!(&event.new, BindingTarget::Resolved { source, .. }
            if source.session_id == session)
            })
            .count()
    };
    assert_eq!(resolved(), 1, "the durable Pending resolved once");
    assert_eq!(owner.wakes(), 0, "the old turn token is still held");
    assert!(
        observed_wait.recv_timeout(Duration::from_secs(10)).is_ok(),
        "ready source stranded; wake_count={} keys={:?}",
        owner.wakes(),
        owner.keys(),
    );
    let released =
        owner
            .rig
            .runtime
            .block_on(owner.rig.mailbox.finish_turn(QueuePersistenceContext::new(
                &ProviderKind::Claude,
                "attached-owner",
                None,
            )));
    assert!(
        released
            .removed_token
            .is_some_and(|removed| Arc::ptr_eq(&removed, &token))
    );
    assert_eq!(owner.serve(), Ok(()));
    owner.settled();
    assert_eq!(owner.wakes(), 1);
    assert_eq!(owner.keys(), (1, 1));
    owner.rig.delivered();
    assert!(
        !owner.attach(),
        "a settled exact source does not resolve again"
    );
    assert_eq!(resolved(), 1);
    assert_eq!(owner.wakes(), 1);
    assert!(matches!(
        last_restore_outcome(&owner.rig.name),
        Some(PendingRestore::BoundFromLedger {
            exact_wait: None,
            ..
        })
    ));
}

/// Pinning an already readable observer changes no readiness and adds no owner wake.
#[test]
fn a_verified_hook_adds_no_wake_for_an_already_readable_observer() {
    use crate::services::tui_prompt_dedupe::binding_events::{BindingTarget, HookSignal};
    use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct BindingLogScope(Option<std::path::PathBuf>, u64);
    impl Drop for BindingLogScope {
        fn drop(&mut self) {
            binding_events::forget_channel_for_tests(self.1);
            binding_events::set_test_root(self.0.as_deref());
        }
    }

    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 7_676_609;
    let _binding_log = BindingLogScope(binding_events::test_root(), channel);
    binding_events::set_test_root(crate::config::runtime_root().as_deref());
    let owner = AttachedOwner::new("oi-attach-first-pin", channel, IDLE);
    let _trace = owner.rig.trace_driver();
    let (session, path) = owner.launch_source();
    fs::write(
        owner.rig.transcript(),
        format!("{{\"type\":\"mode\",\"sessionId\":\"{session}\"}}\n{IDLE}"),
    )
    .unwrap();
    let command = uuid::Uuid::new_v4().to_string();
    assert_ne!(
        command, session,
        "the stable launch alias differs from its continuation"
    );
    dedupe::register_tmux_channel(&owner.rig.name, channel);
    dedupe::register_provider_session("claude", &command, &owner.rig.name);
    let existing = TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session.clone()),
        last_offset: 12,
        relay_last_offset: None,
    };
    // The source predates this hook and the drive's first wake.
    dedupe::register_tmux_runtime_binding(&owner.rig.name, existing.clone());
    let records = || binding_events::records_strict(channel).unwrap().unwrap();
    let before = records();
    assert!(
        matches!(&before.last().unwrap().new, BindingTarget::Source(source)
        if source.session_id == session && source.path == path)
    );
    assert!(
        binding_events::pinned_source(channel, &owner.rig.name)
            .unwrap()
            .is_none()
    );
    assert!(binding_events::pending_seq(channel, &owner.rig.name, &session).is_none());
    let front = owner
        .rig
        .mailbox
        .v2_request(|slot| Ok(slot.as_ref().is_some_and(|owner| owner.front().is_some())));
    assert!(owner.rig.runtime.block_on(front).unwrap());
    let observed = {
        let _entered = owner.rig.runtime.enter();
        crate::services::claude_tui::hosting::owner_observer(
            (&owner.rig.name, PROMPT, channel),
            (None, owner.rig.f.root.path()),
            std::sync::mpsc::channel().0,
            Arc::new(CancelToken::new()),
        )
    };
    assert!(
        observed.is_some(),
        "the provider's observer was already available"
    );
    drop(observed);

    let publications = Arc::new(AtomicUsize::new(0));
    let counted = publications.clone();
    crate::services::discord::tui_prompt_relay::rehydration::OBSERVER_PUBLICATION_FOR_TESTS
        .lock()
        .unwrap()
        .insert(
            owner.rig.name.clone(),
            Arc::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
            }),
        );
    let hook = HookSignal::from_payload("stop", &json!({"transcript_path":path}));
    let adopted = {
        let _entered = owner.rig.runtime.enter();
        dedupe::adopt_claude_continuation_explained(&command, &session, &hook).unwrap()
    };
    assert_eq!(
        adopted,
        (
            Some((owner.rig.name.clone(), path.display().to_string())),
            None
        )
    );
    let after = records();
    assert_eq!(
        after.len(),
        before.len() + 1,
        "the first pin appended its Source"
    );
    assert!(
        matches!(&after.last().unwrap().new, BindingTarget::Source(source)
        if source.session_id == session && source.path == path)
    );
    assert!(
        binding_events::pinned_source(channel, &owner.rig.name)
            .unwrap()
            .is_some()
    );
    assert!(binding_events::pending_seq(channel, &owner.rig.name, &session).is_none());
    assert_eq!(
        dedupe::runtime_binding_for_tmux_session(&owner.rig.name),
        Some(existing),
        "the source key and reader cursor are unchanged"
    );
    assert_eq!(
        publications.load(Ordering::SeqCst),
        0,
        "pinning an existing observer emitted an extra readiness callback"
    );
    assert_eq!(owner.wakes(), 0);
    assert_eq!(owner.keys(), (0, 0));

    owner.rig.wake(&owner.drive);
    assert_eq!(owner.serve(), Ok(()));
    owner.settled();
    assert_eq!(owner.keys(), (1, 1));
    assert_eq!(
        owner.wakes(),
        0,
        "only the explicit owner wake delivered the front"
    );
    owner.rig.delivered();
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
        let (result, sent) = idle_turn(&rig, at, fresh(), (front, settled), driver);
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

/// A short native turn the periodic reader misses still ends the attachment's Busy deferral.
#[test]
fn a_short_native_busy_attachment_wakes_at_the_next_idle_presence() {
    short_native_busy_attachment(false);
}

/// A brief native turn can end before the refused bridge releases its old token.
#[test]
fn a_short_native_busy_attachment_waits_for_the_refused_token_to_finish() {
    short_native_busy_attachment(true);
}

fn short_native_busy_attachment(with_token: bool) {
    use crate::services::discord::queue_io::cancel_backstop_test_support as counted;
    use crate::services::discord::turn_presence::{activity, supervisor::tests::LiveSupervisor};
    use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events};
    use crate::services::turn_orchestrator::QueuePersistenceContext;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};

    struct BindingLogScope(Option<std::path::PathBuf>, u64);
    impl Drop for BindingLogScope {
        fn drop(&mut self) {
            binding_events::forget_channel_for_tests(self.1);
            binding_events::set_test_root(self.0.as_deref());
        }
    }
    struct WakeScope(ChannelId, Option<Arc<dyn Fn() + Send + Sync>>);
    impl Drop for WakeScope {
        fn drop(&mut self) {
            counted::set_owner_wake_for_tests(self.0, self.1.take());
        }
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 7_676_610 + u64::from(with_token);
    let _binding_log = BindingLogScope(binding_events::test_root(), channel);
    binding_events::set_test_root(crate::config::runtime_root().as_deref());
    let name = if with_token {
        "oi-attach-short-busy-token"
    } else {
        "oi-attach-short-busy"
    };
    let owner = AttachedOwner::new(name, channel, IDLE);
    crate::services::discord::tui_prompt_relay::rehydration::OBSERVER_KICKOFF_FOR_TESTS
        .lock()
        .unwrap()
        .remove(&channel);
    let _trace = owner.rig.trace_driver();
    let (session, path) = owner.launch_source();
    let binding = TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session.clone()),
        last_offset: 0,
        relay_last_offset: None,
    };
    assert!(dedupe::register_rehydrated_tmux_runtime_binding(
        "claude",
        &owner.rig.name,
        channel,
        binding.clone()
    ));
    let sequence = || {
        binding_events::records_strict(channel)
            .unwrap()
            .unwrap()
            .last()
            .unwrap()
            .seq
    };
    let before = sequence();
    activity::tests::companion_attach_log_watcher(&owner.shared, channel, &owner.rig.name, &path);
    let _confirmed = crate::services::tui_o::turn_mode::TestConfirmation::new(channel);
    let live = LiveSupervisor::new(owner.shared.clone(), ProviderKind::Claude, channel);
    let token = Arc::new(CancelToken::new());
    assert!(
        owner.rig.runtime.block_on(owner.rig.mailbox.try_start_turn(
            token,
            UserId::new(7),
            MessageId::new(9)
        ))
    );
    let old_idle = owner.rig.runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let reading = activity::reading_now(
                    &owner.shared,
                    &ProviderKind::Claude,
                    ChannelId::new(channel),
                )
                .await;
                if reading.observed.activity == activity::Activity::Idle {
                    break reading;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    });
    owner.rig.runtime.block_on(live.tick());
    assert_eq!(live.state()["activity"], json!("idle"));
    dedupe::clear_tmux_runtime_binding(&owner.rig.name);
    owner
        .rig
        .runtime
        .block_on(owner.rig.mailbox.finish_turn(QueuePersistenceContext::new(
            &ProviderKind::Claude,
            "attached-owner",
            None,
        )));
    owner.rig.wake(&owner.drive);
    assert_eq!(owner.serve(), Err("NotSent(\"idle_unobserved\")".into()));
    owner.settled();
    assert_eq!(owner.keys(), (0, 0));

    let held = with_token.then(|| Arc::new(CancelToken::new()));
    if let Some(token) = &held {
        assert!(owner.rig.runtime.block_on(owner.rig.mailbox.try_start_turn(
            token.clone(),
            UserId::new(7),
            MessageId::new(9),
        )));
    }
    let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    if !with_token {
        owner
            .rig
            .runtime
            .block_on(registry.register("claude".into(), owner.shared.clone()));
        crate::engine::ops::turn_ops::set_global_health_registry(&registry);
    }
    let drive = owner.drive.clone();
    let (woke, owner_woken) = std::sync::mpsc::channel();
    let woke = std::sync::Mutex::new(woke);
    let _wake = WakeScope(
        ChannelId::new(channel),
        counted::set_owner_wake_for_tests(
            ChannelId::new(channel),
            Some(Arc::new(move || {
                crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
                woke.lock().unwrap().send(()).unwrap();
            })),
        ),
    );
    let (seen, source_seen) = std::sync::mpsc::channel();
    let seen = Arc::new(std::sync::Mutex::new(seen));
    let busy = seen.clone();
    crate::services::discord::tui_prompt_relay::rehydration::OBSERVER_BUSY_FOR_TESTS
        .lock()
        .unwrap()
        .insert(
            channel,
            Arc::new(move || {
                busy.lock().unwrap().send("busy").unwrap();
            }),
        );
    if with_token {
        crate::services::discord::tui_prompt_relay::rehydration::OBSERVER_WAITING_FOR_TESTS
            .lock()
            .unwrap()
            .insert(
                channel,
                Arc::new(move || {
                    seen.lock().unwrap().send("waiting").unwrap();
                }),
            );
    }
    let mut transcript = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(
        transcript,
        "{{\"type\":\"user\",\"uuid\":\"brief-native\",\"message\":{{\"content\":\"native\"}}}}"
    )
    .unwrap();
    let source_task = {
        let _entered = owner.rig.runtime.enter();
        if with_token {
            Some(
                crate::services::discord::tui_prompt_relay::rehydrate_owner_pane_for_tests(
                    &owner.shared,
                    &owner.rig.name,
                    channel,
                    &owner.home,
                )
                .expect("the rehydrated source checks its held owner"),
            )
        } else {
            assert!(dedupe::register_rehydrated_tmux_runtime_binding(
                "claude",
                &owner.rig.name,
                channel,
                binding,
            ));
            None
        }
    };
    let phase = source_seen
        .recv_timeout(Duration::from_secs(10))
        .expect("the source check reached its real waiting or Busy event");
    assert_eq!(sequence(), before, "the same durable source is Unchanged");
    assert_eq!(
        counted::owner_wakes(ChannelId::new(channel)),
        0,
        "Busy sends no wake"
    );
    assert_eq!(owner.keys(), (0, 0));
    let old_idle_invalidated = owner
        .rig
        .runtime
        .block_on(old_idle.publish_if_current(|| ()))
        .is_none();
    writeln!(transcript, "{}", IDLE.trim_end()).unwrap();
    owner.rig.runtime.block_on(live.tick());
    if let Some(token) = &held {
        assert_eq!(
            counted::owner_wakes(ChannelId::new(channel)),
            0,
            "the observed Idle cannot wake while the refused token is held"
        );
        let released = owner.rig.runtime.block_on(owner.rig.mailbox.finish_turn(
            QueuePersistenceContext::new(&ProviderKind::Claude, "attached-owner", None),
        ));
        assert!(
            released
                .removed_token
                .as_ref()
                .is_some_and(|removed| Arc::ptr_eq(removed, token))
        );
    }
    if let Some(task) = source_task {
        owner.rig.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("the source check finishes after the real token release")
                .unwrap();
        });
    }
    let wake_seen = owner_woken.recv_timeout(Duration::from_secs(10)).is_ok();
    assert_eq!(
        counted::owner_wakes(ChannelId::new(channel)),
        1,
        "the next Idle wakes the kept front once; token={with_token}; phase={phase}; wake_seen={wake_seen}; old_idle_invalidated={old_idle_invalidated}; actor={}; keys={:?}",
        owner.rig.snapshot(),
        owner.keys(),
    );
    if !with_token {
        assert!(
            old_idle_invalidated,
            "Busy invalidates the earlier stamped Idle read"
        );
    }
    assert_eq!(owner.serve(), Ok(()));
    owner.settled();
    assert_eq!(owner.keys(), (1, 1));
    owner.rig.delivered();
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
pub(crate) type RootRecoveryLaunch = Box<dyn FnOnce(OwnerTurn, &str) -> Result<(), String>>;

pub(crate) enum RootRecoveryDriveEvent {
    Handoff(u64, Result<(), String>),
    Launch {
        owner: Box<OwnerTurn>,
        text: String,
        reply: std::sync::mpsc::SyncSender<Result<(), String>>,
    },
}

static ROOT_RECOVERY_DRIVE_PROBES: std::sync::LazyLock<
    Mutex<std::collections::HashMap<ChannelId, Sender<RootRecoveryDriveEvent>>>,
> = std::sync::LazyLock::new(Default::default);

pub(crate) struct RootRecoveryDriveProbe {
    channel: ChannelId,
    created: Arc<std::sync::atomic::AtomicUsize>,
}

impl RootRecoveryDriveProbe {
    pub(crate) fn created(&self) -> usize {
        self.created.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for RootRecoveryDriveProbe {
    fn drop(&mut self) {
        ROOT_RECOVERY_DRIVE_PROBES
            .lock()
            .unwrap()
            .remove(&self.channel);
        SETTLEMENT_PROBES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .remove(&self.channel.get());
    }
}

pub(crate) fn install_root_recovery_drive_probe(
    channel: ChannelId,
) -> (
    RootRecoveryDriveProbe,
    std::sync::mpsc::Receiver<RootRecoveryDriveEvent>,
) {
    let (sender, receiver) = std::sync::mpsc::channel();
    let (settled, _) = std::sync::mpsc::channel();
    let created = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    assert!(
        SETTLEMENT_PROBES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(
                channel.get(),
                SettlementProbe {
                    settled,
                    created: created.clone(),
                },
            )
            .is_none()
    );
    assert!(
        ROOT_RECOVERY_DRIVE_PROBES
            .lock()
            .unwrap()
            .insert(channel, sender)
            .is_none()
    );
    (RootRecoveryDriveProbe { channel, created }, receiver)
}

pub(crate) fn observe_root_recovery_handoff(
    channel: u64,
    message: u64,
    result: &Result<(), String>,
) {
    if let Some(sender) = ROOT_RECOVERY_DRIVE_PROBES
        .lock()
        .unwrap()
        .get(&ChannelId::new(channel))
    {
        let _ = sender.send(RootRecoveryDriveEvent::Handoff(message, result.clone()));
    }
}

thread_local! {
    static ROOT_RECOVERY_LAUNCH: std::cell::RefCell<Option<(ChannelId, RootRecoveryLaunch)>> =
        const { std::cell::RefCell::new(None) };
}

pub(crate) struct RootRecoveryLaunchGuard;

impl Drop for RootRecoveryLaunchGuard {
    fn drop(&mut self) {
        ROOT_RECOVERY_LAUNCH.with(|slot| slot.borrow_mut().take());
    }
}

pub(crate) fn install_root_recovery_launch(
    channel: ChannelId,
    launch: RootRecoveryLaunch,
) -> RootRecoveryLaunchGuard {
    ROOT_RECOVERY_LAUNCH.with(|slot| {
        assert!(slot.replace(Some((channel, launch))).is_none());
    });
    RootRecoveryLaunchGuard
}

pub(crate) fn root_recovery_launch(
    channel: ChannelId,
    owner: OwnerTurn,
    text: &str,
) -> Result<OwnerTurn, Result<(), String>> {
    let probe = ROOT_RECOVERY_DRIVE_PROBES
        .lock()
        .unwrap()
        .get(&channel)
        .cloned();
    if let Some(probe) = probe {
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        if probe
            .send(RootRecoveryDriveEvent::Launch {
                owner: Box::new(owner),
                text: text.into(),
                reply,
            })
            .is_err()
        {
            return Err(Err("recovery launch observer closed".into()));
        }
        return Err(tokio::task::block_in_place(|| {
            result.recv().unwrap_or_else(|error| Err(error.to_string()))
        }));
    }
    let launch = ROOT_RECOVERY_LAUNCH.with(|slot| {
        let mut slot = slot.borrow_mut();
        slot.as_ref()
            .is_some_and(|(wanted, _)| *wanted == channel)
            .then(|| slot.take().unwrap().1)
    });
    match launch {
        Some(launch) => Err(launch(owner, text)),
        None => Ok(owner),
    }
}

/// Carries the real turn claim into the normal launch hook and its input settlement.
pub(crate) fn claimed_root_recovery_launch(
    owner: OwnerTurn,
    text: &str,
    expected_origin: InputOrigin,
) -> scope::LaunchHook {
    let OwnerTurn {
        mailbox,
        runtime: _,
        claim,
        settled,
        origin,
        ..
    } = owner;
    assert_eq!(
        origin, expected_origin,
        "the real idle claim keeps its input origin"
    );
    let original = &claim.front().1;
    let sources = match original.source_message_ids.as_slice() {
        [] => vec![original.message_id.to_string()],
        ids => ids.iter().map(u64::to_string).collect(),
    };
    let registration = Registration {
        mailbox,
        execution: Execution::LocalTui,
        origin,
        phase: target::Phase::Absent,
        attempt: json!({"nonce":busy_inject::fresh_nonce(),"provider":"claude",
            "target":null,"source":{"kind":"pre_source","expected_session_id":null,
                "expected_transcript":null,"binding_watermark":null}}),
        sources,
        transcript: std::path::PathBuf::new(),
        program: "tmux".into(),
        herdr: None,
        author: original.author_id.to_string(),
        name: "recovery fixture".into(),
        clock: Arc::new(|| chrono::Utc::now().timestamp_millis().max(0) as u64),
        timing: &busy_inject::TIMING,
        until_done: None,
        native_pin_submitted: None,
    };
    created(
        registration,
        text.into(),
        scope::Lineage::Created,
        (Some(claim), reply_once(Some(settled))),
    )
}

#[path = "owner_input_busy_gate_tests.rs"]
mod busy_gate;
