//! A TUI input typed while a Discord prompt is handed to Codex: one echo when it joins the
//! Discord turn, its own answer when it opens another, whichever observer sees it first.
use super::*;
use crate::services::discord::make_shared_data_for_tests;
use crate::services::tui_prompt_dedupe as dedupe;

const ORIGIN: &str = "[User: probe (ID: 5845)] codex discord prompt 5845";
const STEER: &str = "codex input typed during the discord handoff 5845";
const DISCORD_ANSWER: &str = "DISCORD_5845_ANSWER";
const STEER_ANSWER: &str = "STEER_5845_ANSWER";

#[derive(Clone, Copy, Debug)]
enum ParkedRace {
    Cursor,
    Generation,
    Source,
    Cancel,
    Owner,
    MissingCustody,
    NativeTurn,
    ForeignSource,
    ForeignGeneration,
    SuccessorProof,
    NonBridge,
    Receipt,
    Expired,
    NextAnswer,
    Allocation,
    Rotation,
    PendingRotation,
    RotationSteer,
    ReceiptLimit,
    DirectPending,
    ReceiptFailure,
    GenerationSwitch,
}

fn native_receipts(
    channel: u64,
) -> Vec<crate::services::discord::outbound::delivery_record::ConfirmedDeliveryReceipt> {
    crate::services::discord::outbound::delivery_record::read_record(&ProviderKind::Codex, channel)
        .map(|record| record.confirmed_deliveries)
        .unwrap_or_default()
}

async fn restored_historical_answer(root: PathBuf, writer_first: bool, race: Option<ParkedRace>) {
    let _tmux = live_pane_tmux(&root);
    let base_tmux = if writer_first {
        "AgentDesk-codex-6903-writer-first"
    } else {
        "AgentDesk-codex-6903-custody"
    };
    let case = match race {
        None => 0,
        Some(ParkedRace::Cursor) => 1,
        Some(ParkedRace::Generation) => 2,
        Some(ParkedRace::Source) => 3,
        Some(ParkedRace::Cancel) => 4,
        Some(ParkedRace::Owner) => 5,
        Some(ParkedRace::MissingCustody) => 6,
        Some(ParkedRace::NativeTurn) => 7,
        Some(ParkedRace::ForeignSource) => 8,
        Some(ParkedRace::ForeignGeneration) => 9,
        Some(ParkedRace::SuccessorProof) => 10,
        Some(ParkedRace::NonBridge) => 11,
        Some(ParkedRace::Receipt) => 12,
        Some(ParkedRace::Expired) => 13,
        Some(ParkedRace::NextAnswer) => 14,
        Some(ParkedRace::Allocation) => 15,
        Some(ParkedRace::Rotation) => 16,
        Some(ParkedRace::PendingRotation) => 17,
        Some(ParkedRace::RotationSteer) => 18,
        Some(ParkedRace::ReceiptLimit) => 101,
        Some(ParkedRace::DirectPending) => 102,
        Some(ParkedRace::ReceiptFailure) => 103,
        Some(ParkedRace::GenerationSwitch) => 104,
    };
    let tmux_name = format!("{base_tmux}-{case}");
    let tmux = tmux_name.as_str();
    let mut fx = Fixture::start(&root, 6_903_101 + u64::from(writer_first) + case * 10, tmux).await;
    discord_row(&fx, 6_903_201);
    std::mem::forget(reserve(&fx));
    send_captured_hook(&fx, STEER, &turn(2));
    held(&fx, 1).await;
    if matches!(race, Some(ParkedRace::RotationSteer)) {
        send_captured_hook(&fx, "parked same-turn steer 6903", &turn(2));
        held(&fx, 2).await;
    }
    let old = user_record_at(
        "legacy L0",
        &turn(10),
        chrono::Utc::now() - chrono::TimeDelta::minutes(10),
    );
    fx.append(&(old + &answer_for(&turn(10), "LEGACY_6903_L0")));
    let origin_start = std::fs::metadata(&fx.codex.rollout).unwrap().len();
    let parked_answer = if race.is_some() {
        rollout_line(serde_json::json!({"type":"event_msg", "payload":{
            "type":"task_complete", "turn_id":turn(2), "last_agent_message":STEER_ANSWER}}))
    } else {
        answer_for(&turn(2), STEER_ANSWER)
    };
    let joined = if matches!(race, Some(ParkedRace::RotationSteer)) {
        user_record("parked same-turn steer 6903", &turn(2))
    } else {
        String::new()
    };
    let records = opening_record(&turn(1), ORIGIN)
        + &answer_for(&turn(1), DISCORD_ANSWER)
        + &opening_record(&turn(11), "legacy L1")
        + &answer_for(&turn(11), "LEGACY_6903_L1")
        + &opening_record(&turn(2), STEER)
        + &joined
        + &parked_answer
        + &opening_record(&turn(12), "legacy L2")
        + &answer_for(&turn(12), "LEGACY_6903_L2");
    let boundary = fx.append(&records);
    if !matches!(
        race,
        Some(
            ParkedRace::ReceiptLimit
                | ParkedRace::DirectPending
                | ParkedRace::ReceiptFailure
                | ParkedRace::GenerationSwitch
        )
    ) {
        fx.append(user_record("next partial 6903", &turn(3)).trim_end_matches('\n'));
    }
    if matches!(race, Some(ParkedRace::Expired)) {
        let path = root
            .join("runtime/codex_handoff_held")
            .join(format!("codex-{tmux}.json"));
        let mut held: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        held["expires_at"] = serde_json::json!(chrono::Utc::now() - chrono::TimeDelta::seconds(1));
        std::fs::write(path, serde_json::to_vec(&held).unwrap()).unwrap();
    }
    dedupe::reset_state_for_tests();
    let restore = super::super::super::super::codex_tui_rehydrated_binding_from_rollout_path;
    let binding = restore(tmux, &fx.codex.rollout, Some("s-5704".into())).unwrap();
    assert!(
        origin_start < binding.last_offset && binding.last_offset < boundary,
        "handoff input floor: {binding:?}"
    );
    dedupe::register_tmux_runtime_binding(tmux, binding.clone());
    if writer_first {
        let relay = crate::services::discord::recovery_engine::codex_rebind_complete_for_tests(
            tmux,
            fx.codex.rollout.to_str().unwrap(),
            origin_start,
        );
        assert!(
            wait_for(Duration::from_secs(10), || {
                dedupe::runtime_binding_for_tmux_session(tmux)
                    .is_some_and(|binding| binding.last_offset == boundary)
            })
            .await,
            "writer complete checkpoint must precede raw EOF"
        );
        assert!(
            std::fs::read_to_string(relay)
                .unwrap()
                .contains(DISCORD_ANSWER)
        );
        assert!(
            fx.sent(STEER_ANSWER).is_empty(),
            "normalized writer is not Discord delivery"
        );
    }
    let prepare = writer_first.then(|| {
        Arc::new(
            crate::services::discord::turn_bridge::TerminalPrepareTestHook {
                channel_id: fx.codex.channel.get(),
                ..Default::default()
            },
        )
    });
    *crate::services::discord::turn_bridge::TERMINAL_PREPARE_TEST_HOOK
        .lock()
        .unwrap() = prepare.clone();
    assert!(
        native_receipts(fx.codex.channel.get()).is_empty(),
        "writer success is not a receipt"
    );
    let mut admitted_row = None;
    let mut g1_generation = 0;
    assert!(inflight::clear_inflight_state(
        &ProviderKind::Codex,
        fx.codex.channel.get()
    ));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut admitted = 0;
    while let Ok(event) = tokio::time::timeout_at(deadline, fx.observed.recv()).await {
        let event = event.unwrap();
        if event.tmux_session_name != tmux {
            continue;
        }
        assert!(
            !event.prompt.starts_with("legacy L"),
            "unrelated historical input admitted: {event:?}"
        );
        if event.prompt == STEER {
            assert!(!event.steer_echo);
            admitted += 1;
            super::super::super::super::relay_observed_prompt(&fx.shared, event).await;
            if let Some(prepare) = &prepare {
                tokio::time::timeout(Duration::from_secs(5), prepare.entered.notified())
                    .await
                    .expect("actual bridge admitted the parked terminal before publication");
                let row = fx.codex.row().expect("admitted range persists before ACK");
                assert!(
                    row.turn_start_offset.unwrap() < row.last_offset && row.last_offset < boundary
                );
                assert!(row.tui_terminal_source_file_identity.is_some());
                assert!(!row.terminal_delivery_committed);
                assert_eq!(row.full_response, STEER_ANSWER);
                assert!(
                    native_receipts(fx.codex.channel.get()).is_empty(),
                    "no receipt before gateway ACK"
                );
                if race.is_some() {
                    assert!(
                        fx.sent(STEER_ANSWER).is_empty(),
                        "terminal-only fixture must have no pre-pause body edit"
                    );
                }
                let actor =
                    crate::services::discord::mailbox_snapshot(&fx.shared, fx.codex.channel)
                        .await
                        .cancel_token
                        .unwrap();
                admitted_row = Some(row.clone());
                match race {
                    Some(ParkedRace::Cursor) => {
                        fx.append("\n");
                        let expected = fx.append(&token_count_line());
                        let relay = crate::services::discord::recovery_engine::codex_rebind_complete_for_tests(
                            tmux, fx.codex.rollout.to_str().unwrap(), origin_start);
                        assert!(
                            wait_for(Duration::from_secs(10), || {
                                dedupe::runtime_binding_for_tmux_session(tmux).is_some_and(
                                    |binding| {
                                        binding.last_offset > boundary
                                            && binding.last_offset >= expected
                                    },
                                )
                            })
                            .await
                        );
                        assert!(std::fs::metadata(relay).unwrap().len() > 0);
                    }
                    Some(ParkedRace::Generation) => {
                        let path =
                            crate::services::tmux_common::session_temp_path(tmux, "generation");
                        let time = std::fs::metadata(&path).unwrap().modified().unwrap();
                        std::fs::File::open(path)
                            .unwrap()
                            .set_times(
                                std::fs::FileTimes::new()
                                    .set_modified(time + Duration::from_secs(1)),
                            )
                            .unwrap();
                    }
                    Some(ParkedRace::Source) => {
                        std::fs::rename(
                            &fx.codex.rollout,
                            fx.codex.rollout.with_extension("original"),
                        )
                        .unwrap();
                        std::fs::copy(
                            fx.codex.rollout.with_extension("original"),
                            &fx.codex.rollout,
                        )
                        .unwrap();
                    }
                    Some(ParkedRace::Cancel) => actor.cancelled.store(true, Ordering::Release),
                    Some(case @ (ParkedRace::Owner | ParkedRace::Allocation)) => {
                        let nonce = if matches!(case, ParkedRace::Allocation) {
                            row.turn_nonce.clone()
                        } else {
                            Some("successor-6903".into())
                        };
                        let successor = Arc::new(CancelToken::from_persisted_turn_nonce(nonce));
                        fx.shared
                            .mailbox(fx.codex.channel)
                            .restore_active_turn(
                                successor.clone(),
                                serenity::UserId::new(row.request_owner_user_id),
                                MessageId::new(row.user_msg_id),
                            )
                            .await;
                        let now = crate::services::discord::mailbox_snapshot(
                            &fx.shared,
                            fx.codex.channel,
                        )
                        .await;
                        assert!(
                            now.cancel_token
                                .as_ref()
                                .is_some_and(|current| Arc::ptr_eq(current, &successor))
                        );
                        assert!(!Arc::ptr_eq(&actor, &successor));
                        if matches!(case, ParkedRace::Allocation) {
                            assert_eq!(successor.turn_nonce(), actor.turn_nonce());
                            assert!(!actor.cancelled.load(Ordering::Acquire));
                            assert_eq!(
                                serde_json::to_value(fx.codex.row().unwrap()).unwrap(),
                                serde_json::to_value(&row).unwrap()
                            );
                        }
                        if matches!(case, ParkedRace::Owner) {
                            let mut changed = row.clone();
                            changed.turn_nonce = Some("successor-6903".into());
                            inflight::save_inflight_state(&changed).unwrap();
                        }
                    }
                    Some(
                        case @ (ParkedRace::MissingCustody
                        | ParkedRace::NativeTurn
                        | ParkedRace::ForeignSource
                        | ParkedRace::ForeignGeneration
                        | ParkedRace::SuccessorProof
                        | ParkedRace::NonBridge),
                    ) => {
                        assert_parked_gate_denies(&fx, row, actor.clone(), case).await;
                        actor.cancelled.store(true, Ordering::Release);
                    }
                    Some(ParkedRace::ReceiptFailure) => {
                        assert_eq!(
                            dedupe::codex_restore_settled_for_tests(tmux),
                            Some(0),
                            "ACK must precede settlement"
                        );
                        let path = crate::services::discord::outbound::delivery_record::delivery_record_path(&ProviderKind::Codex, row.delivery_record_owner_channel_id()).unwrap();
                        *prepare.after_send.lock().unwrap() = Some(Box::new(move || {
                            if path.is_file() {
                                std::fs::remove_file(&path).unwrap();
                            }
                            std::fs::create_dir_all(&path).unwrap();
                        }));
                    }
                    Some(ParkedRace::GenerationSwitch) => {
                        assert_eq!(dedupe::codex_restore_settled_for_tests(tmux), Some(0));
                        g1_generation = crate::services::discord::outbound::delivery_record::current_generation_mtime_ns(tmux);
                        let generation =
                            crate::services::tmux_common::session_temp_path(tmux, "generation");
                        let nonce =
                            crate::services::tmux_common::session_temp_path(tmux, "spawn_nonce");
                        // The wrapper respawn lands after the Discord ACK and before commit.
                        *prepare.after_send.lock().unwrap() = Some(Box::new(move || {
                            let time = std::fs::metadata(&generation).unwrap().modified().unwrap();
                            std::fs::File::open(&generation)
                                .unwrap()
                                .set_times(
                                    std::fs::FileTimes::new()
                                        .set_modified(time + Duration::from_secs(1)),
                                )
                                .unwrap();
                            std::fs::write(&nonce, "g2-6903\n").unwrap();
                        }));
                    }
                    Some(ParkedRace::PendingRotation) => {
                        assert!(native_receipts(fx.codex.channel.get()).is_empty());
                        let previous = dedupe::runtime_binding_for_tmux_session(tmux).unwrap();
                        let next = root.join("pending-rotation.jsonl");
                        std::fs::write(&next, rollout_line(serde_json::json!({"type":"session_meta", "payload":{"id":"rotation-6903"}}))).unwrap();
                        crate::services::codex_tui::session::write_codex_tui_rollout_marker(
                            tmux,
                            &next,
                            Some("rotation-6903"),
                        )
                        .unwrap();
                        assert!(super::super::super::super::rehydration::rehydrate_codex_tui_binding_for_tests(tmux, fx.codex.channel.get()).is_none(), "unfinished parked custody must block source replacement");
                        assert_eq!(
                            dedupe::runtime_binding_for_tmux_session(tmux).unwrap(),
                            previous
                        );
                        let key = (
                            row.channel_id,
                            row.turn_nonce.clone().unwrap(),
                            row.external_turn_id.clone().unwrap(),
                            row.session_key.clone().unwrap(),
                            row.user_msg_id,
                            row.turn_start_offset.unwrap(),
                        );
                        assert!(
                            dedupe::codex_parked_owner_source(
                                tmux,
                                row.native_turn_id.as_deref().unwrap(),
                                &key
                            )
                            .is_some(),
                            "pending owner proof must remain available"
                        );
                        crate::services::codex_tui::session::write_codex_tui_rollout_marker(
                            tmux,
                            &fx.codex.rollout,
                            Some("s-5704"),
                        )
                        .unwrap();
                    }
                    Some(
                        ParkedRace::Receipt
                        | ParkedRace::Expired
                        | ParkedRace::NextAnswer
                        | ParkedRace::Rotation
                        | ParkedRace::RotationSteer
                        | ParkedRace::ReceiptLimit
                        | ParkedRace::DirectPending,
                    )
                    | None => {}
                }
                prepare.release.notify_one();
                if matches!(race, Some(ParkedRace::ReceiptFailure)) {
                    assert!(fx.delivered(STEER_ANSWER).await);
                    fx.polls(3).await;
                    assert!(native_receipts(fx.codex.channel.get()).is_empty());
                    assert_eq!(
                        dedupe::codex_restore_settled_for_tests(tmux),
                        Some(0),
                        "failed receipt must not settle custody"
                    );
                    *crate::services::discord::turn_bridge::TERMINAL_PREPARE_TEST_HOOK
                        .lock()
                        .unwrap() = None;
                    fx.finish();
                    return;
                }
                if matches!(race, Some(ParkedRace::GenerationSwitch)) {
                    let row = admitted_row.clone().unwrap();
                    generation_switch_settles_parked(&root, &mut fx, &row, g1_generation).await;
                    fx.finish();
                    return;
                }
                if matches!(
                    race,
                    Some(
                        ParkedRace::Generation
                            | ParkedRace::Source
                            | ParkedRace::Cancel
                            | ParkedRace::Owner
                            | ParkedRace::Allocation
                            | ParkedRace::MissingCustody
                            | ParkedRace::NativeTurn
                            | ParkedRace::ForeignSource
                            | ParkedRace::ForeignGeneration
                            | ParkedRace::SuccessorProof
                            | ParkedRace::NonBridge
                    )
                ) {
                    fx.polls(3).await;
                    assert!(
                        fx.sent(STEER_ANSWER).is_empty(),
                        "lost authority must not publish the parked answer: {race:?}, requests={:?}, receipts={:?}",
                        fx.sent(STEER_ANSWER),
                        native_receipts(fx.codex.channel.get())
                    );
                    assert!(native_receipts(fx.codex.channel.get()).is_empty());
                    assert_eq!(fx.completed_turns(), 0);
                    *crate::services::discord::turn_bridge::TERMINAL_PREPARE_TEST_HOOK
                        .lock()
                        .unwrap() = None;
                    fx.finish();
                    return;
                }
            }
            if fx.delivered(STEER_ANSWER).await {
                break;
            }
        }
    }
    assert_eq!(admitted, 1, "parked input is recovered once");
    assert!(
        fx.delivered(STEER_ANSWER).await,
        "already completed parked answer was lost: row={:?}, lease={:?}, requests={:?}",
        fx.codex.row(),
        fx.lease(),
        fx.requests.lock().unwrap()
    );
    fx.polls(3).await;
    while let Ok(event) = fx.observed.try_recv() {
        assert!(
            event.tmux_session_name != tmux || !event.prompt.starts_with("legacy L"),
            "unrelated history must never reach NotifyAndLease: {event:?}"
        );
    }
    assert_eq!(tail_starts(fx.codex.channel), 1);
    assert!(
        fx.sent("LEGACY_6903_").is_empty(),
        "legacy responses are not custody"
    );
    let anchor = fx.anchor_of(STEER_ANSWER);
    fx.assert_answer(STEER_ANSWER, anchor);
    assert_eq!(fx.completed_turns(), 1);
    if let Some(row) = admitted_row.as_ref() {
        assert!(
            wait_for(Duration::from_secs(5), || !native_receipts(
                fx.codex.channel.get()
            )
            .is_empty())
            .await
        );
        let receipts = native_receipts(fx.codex.channel.get());
        assert_eq!(
            receipts.len(),
            1,
            "one native receipt for the actual admitted range"
        );
        assert_eq!(
            receipts[0].source.range,
            (row.turn_start_offset.unwrap(), row.last_offset)
        );
        assert_eq!(
            Some(receipts[0].source.turn_nonce.as_str()),
            row.turn_nonce.as_deref()
        );
        assert_eq!(receipts[0].source.tmux_session_name, tmux);
        assert_eq!(receipts[0].message_id, anchor);
        assert_eq!(receipts[0].delivery_channel_id, fx.codex.channel.get());
        let record = crate::services::discord::outbound::delivery_record::read_record(
            &ProviderKind::Codex,
            row.delivery_record_owner_channel_id(),
        )
        .unwrap();
        assert_eq!(
            record
                .delivered_frontier
                .as_ref()
                .map(|frontier| frontier.range),
            Some((row.turn_start_offset.unwrap(), row.last_offset)),
            "current source delivery must commit its actual terminal range, not only a historical receipt"
        );
    }
    *crate::services::discord::turn_bridge::TERMINAL_PREPARE_TEST_HOOK
        .lock()
        .unwrap() = None;
    assert!(
        wait_for(Duration::from_secs(5), || {
            dedupe::runtime_binding_for_tmux_session(tmux)
                .is_some_and(|binding| binding.last_offset >= boundary)
        })
        .await,
        "idle scanner must complete its checkpoint after the terminal ACK"
    );
    if matches!(race, Some(ParkedRace::Receipt)) {
        assert_retained_receipt_consumed(&mut fx, admitted_row.unwrap()).await;
    }
    if matches!(race, Some(ParkedRace::NextAnswer)) {
        let prior = fx.sent(STEER_ANSWER).len();
        fx.append("\n");
        let mut events = events_for(&mut fx, "next partial 6903", Duration::from_secs(5)).await;
        assert_eq!(events.len(), 1, "sealed next user admitted once");
        super::super::super::super::relay_observed_prompt(&fx.shared, events.remove(0)).await;
        fx.append(&answer_for(&turn(3), "NEXT_6903_ANSWER"));
        assert!(fx.delivered("NEXT_6903_ANSWER").await);
        fx.polls(3).await;
        assert_eq!(fx.sent("NEXT_6903_ANSWER").len(), 1);
        assert_eq!(
            fx.sent(STEER_ANSWER).len(),
            prior,
            "another turn's receipt cannot broaden the prior range"
        );
        assert_eq!(native_receipts(fx.codex.channel.get()).len(), 2);
    }
    if matches!(race, Some(ParkedRace::ReceiptLimit)) {
        let parked_receipt = native_receipts(fx.codex.channel.get())[0].clone();
        let direct = 100..132;
        for n in direct.clone() {
            deliver_direct_terminal(&mut fx, n, n + 1 == direct.end).await;
        }
        let receipts = native_receipts(fx.codex.channel.get());
        assert_eq!(receipts.len(), 32);
        assert!(
            !receipts
                .iter()
                .any(|receipt| receipt.source == parked_receipt.source),
            "P receipt must actually be drained"
        );
    }
    if matches!(race, Some(ParkedRace::DirectPending)) {
        assert_eq!(dedupe::codex_restore_settled_for_tests(tmux), Some(1));
        direct_pending_rotation(&root, &mut fx).await;
        fx.finish();
        return;
    }
    if matches!(
        race,
        Some(ParkedRace::Rotation | ParkedRace::RotationSteer | ParkedRace::ReceiptLimit)
    ) {
        let prior_receipts = native_receipts(fx.codex.channel.get()).len();
        let old = fx.codex.rollout.clone();
        let next = root.join("completed-rotation.jsonl");
        std::fs::write(
            &next,
            rollout_line(
                serde_json::json!({"type":"session_meta", "payload":{"id":"rotation-6903"}}),
            ),
        )
        .unwrap();
        crate::services::codex_tui::session::write_codex_tui_rollout_marker(
            tmux,
            &next,
            Some("rotation-6903"),
        )
        .unwrap();
        let fresh = super::super::super::super::rehydration::rehydrate_codex_tui_binding_for_tests(
            tmux,
            fx.codex.channel.get(),
        )
        .expect("completed parked snapshot must release the actual marker rotation");
        assert!(old.exists());
        assert_eq!(fresh.output_path, next.display().to_string());
        assert_eq!(fresh.session_id.as_deref(), Some("rotation-6903"));
        fx.codex.rollout = next;
        fx.append(
            &(opening(&turn(72), "AFTER_ROTATION_6903")
                + &answer_for(&turn(72), "AFTER_ROTATION_6903_ANSWER")),
        );
        let event = loop {
            let event = tokio::time::timeout(Duration::from_secs(5), fx.observed.recv())
                .await
                .unwrap()
                .unwrap();
            if event.tmux_session_name == tmux && event.prompt == "AFTER_ROTATION_6903" {
                break event;
            }
        };
        super::super::super::super::relay_observed_prompt(&fx.shared, event).await;
        assert!(fx.delivered("AFTER_ROTATION_6903_ANSWER").await);
        fx.polls(3).await;
        assert_eq!(fx.sent("AFTER_ROTATION_6903_ANSWER").len(), 1);
        fx.assert_answer(
            "AFTER_ROTATION_6903_ANSWER",
            fx.anchor_of("AFTER_ROTATION_6903_ANSWER"),
        );
        assert_eq!(fx.sent(STEER_ANSWER).len(), 1);
        assert_eq!(
            native_receipts(fx.codex.channel.get()).len(),
            (prior_receipts + 1).min(32)
        );
    }
    fx.finish();
}

#[test]
fn codex_6903_completed_parked_receipt_allows_actual_marker_rotation() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::Rotation),
        ))
    });
}

#[test]
fn codex_6903_pending_parked_owner_blocks_actual_marker_rotation() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::PendingRotation),
        ))
    });
}

#[test]
fn codex_6903_restore_recovers_only_the_parked_historical_answer() {
    run(|root| Box::pin(restored_historical_answer(root, false, None)));
}

#[test]
fn codex_6903_writer_checkpoint_does_not_consume_parked_input_replay() {
    run(|root| Box::pin(restored_historical_answer(root, true, None)));
}

#[test]
fn codex_6903_parked_terminal_survives_writer_advance_after_admission() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::Cursor),
        ))
    });
}
#[test]
fn codex_6903_parked_terminal_rejects_generation_change_after_admission() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::Generation),
        ))
    });
}
#[test]
fn codex_6903_parked_terminal_rejects_source_replacement_after_admission() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::Source),
        ))
    });
}
#[test]
fn codex_6903_parked_terminal_rejects_cancel_after_admission() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::Cancel),
        ))
    });
}
#[test]
fn codex_6903_parked_terminal_rejects_successor_after_admission() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::Owner),
        ))
    });
}

async fn assert_parked_gate_denies(
    fx: &Fixture,
    mut row: inflight::InflightTurnState,
    mut actor: Arc<CancelToken>,
    case: ParkedRace,
) {
    use crate::services::agent_protocol::{
        CapturedTuiTerminalSource, NativeTerminalKind, StreamMessage,
    };
    use std::os::unix::fs::MetadataExt;
    let tmux = &fx.codex.tmux;
    let mut binding = dedupe::runtime_binding_for_tmux_session(tmux).unwrap();
    let before_cursor = binding.last_offset;
    match case {
        ParkedRace::MissingCustody => {
            dedupe::clear_tmux_runtime_binding(tmux);
            dedupe::register_tmux_runtime_binding(tmux, binding.clone());
        }
        ParkedRace::NativeTurn => row.native_turn_id = Some(turn(91)),
        ParkedRace::ForeignSource => {
            let foreign = fx.codex.rollout.with_extension("foreign.jsonl");
            std::fs::copy(&fx.codex.rollout, &foreign).unwrap();
            row.output_path = Some(foreign.to_string_lossy().into_owned());
            binding.output_path = row.output_path.clone().unwrap();
            dedupe::register_tmux_runtime_binding(tmux, binding);
        }
        ParkedRace::ForeignGeneration => {
            let path = crate::services::tmux_common::session_temp_path(tmux, "generation");
            let time = std::fs::metadata(&path).unwrap().modified().unwrap();
            std::fs::File::open(path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(time + Duration::from_secs(1)))
                .unwrap();
        }
        ParkedRace::SuccessorProof => {
            actor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
                "successor-proof-6903".into(),
            )));
            row.turn_nonce = actor.turn_nonce().map(str::to_owned);
            fx.shared
                .mailbox(fx.codex.channel)
                .restore_active_turn(
                    actor.clone(),
                    serenity::UserId::new(row.request_owner_user_id),
                    MessageId::new(row.user_msg_id),
                )
                .await;
        }
        ParkedRace::NonBridge => row.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher),
        _ => panic!("not a negative admission case"),
    }
    assert!(inflight::clear_inflight_state(
        &ProviderKind::Codex,
        row.channel_id
    ));
    inflight::save_inflight_state(&row).unwrap();
    let mut local = fx.codex.row().unwrap();
    let mut baseline = local.clone();
    let expected = inflight::InflightTurnIdentity::from_state(&local);
    let path = PathBuf::from(local.output_path.as_ref().unwrap());
    let metadata = std::fs::metadata(&path).unwrap();
    let frame = StreamMessage::CodexTuiTerminalDone {
        result: STEER_ANSWER.into(),
        session_id: local.session_id.clone(),
        rollout_path: path.to_string_lossy().into_owned(),
        tmux_session_name: tmux.clone(),
        turn_nonce: actor.turn_nonce().unwrap().into(),
        source_start: local.turn_start_offset.unwrap(),
        complete_record_end: local.last_offset,
        captured_source: Some(CapturedTuiTerminalSource {
            generation_mtime_ns:
                crate::services::discord::turn_bridge::tmux_generation_file_mtime_ns(tmux),
            source_file_dev: metadata.dev(),
            source_file_ino: metadata.ino(),
            actor: Arc::downgrade(&actor),
        }),
        kind: NativeTerminalKind::Completed,
    };
    let before = serde_json::to_value(&local).unwrap();
    let admitted = local
        .admit_tui_terminal_frame(
            &mut baseline,
            &expected,
            true,
            (&fx.shared, &actor),
            STEER_ANSWER,
            frame,
        )
        .await;
    assert!(
        admitted.is_err(),
        "non-custody historical frame gained terminal admission: {case:?}"
    );
    assert_eq!(
        serde_json::to_value(fx.codex.row().unwrap()).unwrap(),
        before
    );
    assert!(native_receipts(row.channel_id).is_empty());
    assert!(fx.sent(STEER_ANSWER).is_empty());
    assert_eq!(
        dedupe::runtime_binding_for_tmux_session(tmux)
            .unwrap()
            .last_offset,
        before_cursor
    );
}

async fn assert_retained_receipt_consumed(fx: &mut Fixture, row: inflight::InflightTurnState) {
    let posted = fx.sent(STEER_ANSWER).len();
    assert_eq!(posted, 1, "one original final answer");
    // Repeat input replay is a distinct check from the receipt consumer below.
    dedupe::codex_replay_restored_handoff_inputs(&fx.codex.tmux, &fx.codex.rollout);
    dedupe::codex_replay_restored_handoff_inputs(&fx.codex.tmux, &fx.codex.rollout);
    fx.polls(3).await;
    assert_eq!(fx.sent(STEER_ANSWER).len(), posted);
    crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
        .deregister(&fx.codex.tmux);
    fx.shared.tmux_watchers.remove(&fx.codex.channel);
    assert!(wait_for(Duration::from_secs(5), || fx.codex.row().is_none()).await);
    inflight::save_inflight_state(&row).unwrap();
    let retained = fx.codex.row().unwrap();
    assert_eq!(
        retained.turn_nonce, row.turn_nonce,
        "retained recovery must reuse the same episode nonce"
    );
    let shared = make_shared_data_for_tests();
    let gateway = crate::services::discord::gateway::DiscordGateway::new(
        fx.http.clone(),
        shared.clone(),
        ProviderKind::Codex,
        None,
    );
    assert!(
        crate::services::discord::recovery_engine::recover_idle_partial_response_from_ready_source(
            &fx.http,
            &shared,
            &retained,
            &fx.codex.rollout,
            &gateway
        )
        .await,
        "actual retained recovery must consume the matching native receipt"
    );
    assert_eq!(
        fx.sent(STEER_ANSWER).len(),
        posted,
        "matching receipt must suppress retained recovery publication"
    );
    assert!(
        fx.codex.row().is_none(),
        "receipt consumption retires the retained obligation"
    );
    assert_eq!(native_receipts(fx.codex.channel.get()).len(), 1);
}

#[test]
fn codex_6903_parked_gate_rejects_history_without_custody() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::MissingCustody),
        ))
    });
}
#[test]
fn codex_6903_parked_gate_rejects_same_text_with_another_native_turn() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::NativeTurn),
        ))
    });
}
#[test]
fn codex_6903_parked_gate_rejects_another_valid_source() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::ForeignSource),
        ))
    });
}
#[test]
fn codex_6903_parked_gate_rejects_another_valid_generation() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::ForeignGeneration),
        ))
    });
}
#[test]
fn codex_6903_parked_gate_rejects_a_current_successor_without_owner_handoff() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::SuccessorProof),
        ))
    });
}
#[test]
fn codex_6903_parked_gate_keeps_nonbridge_authority_closed() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::NonBridge),
        ))
    });
}
#[test]
fn codex_6903_matching_receipt_is_consumed_by_actual_retained_recovery() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::Receipt),
        ))
    });
}

#[test]
fn codex_6903_expired_restore_keeps_and_delivers_its_parked_answer() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::Expired),
        ))
    });
}
#[test]
fn codex_6903_native_receipt_does_not_suppress_the_next_sealed_answer() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::NextAnswer),
        ))
    });
}

#[test]
fn codex_6903_idle_unknown_prefix_is_counted_once_and_leaves_partial_input() {
    run(|root| {
        Box::pin(async move {
            let _tmux = live_pane_tmux(&root);
            let mut fx =
                Fixture::start(&root, 6_903_501, "AgentDesk-codex-6903-unknown-prefix").await;
            dedupe::reset_state_for_tests();
            let binding =
                super::super::super::super::codex_tui_rehydrated_binding_from_rollout_path(
                    &fx.codex.tmux,
                    &fx.codex.rollout,
                    Some("s-5704".into()),
                )
                .unwrap();
            dedupe::register_tmux_runtime_binding(&fx.codex.tmux, binding);
            let before = dedupe::codex_restore_observation_for_tests(&fx.codex.tmux).unwrap();
            let unknown = rollout_line(serde_json::json!({"type":"response_item", "payload":{
            "type":"message","role":"assistant","content":[{"type":"output_text","text":"한🙂unknown"}]}}));
            let end = fx.append(&unknown);
            fx.append(user_record(STEER, &turn(2)).trim_end_matches('\n'));
            fx.scanned_past(end).await;
            let seen = dedupe::codex_restore_observation_for_tests(&fx.codex.tmux).unwrap();
            assert_eq!(seen.3.records, before.3.records + 1);
            assert_eq!(
                seen.3.assistant_bytes,
                before.3.assistant_bytes + "한🙂unknown".len() as u64
            );
            assert_eq!(
                seen.3.source_bytes,
                before.3.source_bytes + unknown.len() as u64
            );
            assert!(fx.observed.try_recv().is_err());
            fx.polls(3).await;
            assert_eq!(
                dedupe::codex_restore_observation_for_tests(&fx.codex.tmux)
                    .unwrap()
                    .3,
                seen.3
            );
            fx.append("\n");
            let mut events = events_for(&mut fx, STEER, Duration::from_secs(3)).await;
            assert_eq!(events.len(), 1);
            super::super::super::super::relay_observed_prompt(&fx.shared, events.remove(0)).await;
            fx.append(&answer_for(&turn(2), STEER_ANSWER));
            assert!(fx.delivered(STEER_ANSWER).await);
            assert!(
                fx.sent("한🙂unknown").is_empty(),
                "unknown scan is observation, never historical delivery"
            );
            fx.finish();
        })
    });
}

#[test]
fn codex_6903_idle_prompt_preserves_unknown_prefix_statistics() {
    run(|root| {
        Box::pin(async move {
            let _tmux = live_pane_tmux(&root);
            let mut fx =
                Fixture::start(&root, 6_903_502, "AgentDesk-codex-6903-prompt-prefix").await;
            dedupe::reset_state_for_tests();
            let binding =
                super::super::super::super::codex_tui_rehydrated_binding_from_rollout_path(
                    &fx.codex.tmux,
                    &fx.codex.rollout,
                    Some("s-5704".into()),
                )
                .unwrap();
            dedupe::register_tmux_runtime_binding(&fx.codex.tmux, binding);
            let before = dedupe::codex_restore_observation_for_tests(&fx.codex.tmux).unwrap();
            let unknown = rollout_line(serde_json::json!({"type":"response_item", "payload":{
            "type":"message","role":"assistant","content":[{"type":"output_text","text":"한🙂unknown"}]}}));
            fx.append(&format!("{unknown}{}", user_record(STEER, &turn(2))));
            let mut events = events_for(&mut fx, STEER, Duration::from_secs(3)).await;
            assert_eq!(events.len(), 1);
            let seen = dedupe::codex_restore_observation_for_tests(&fx.codex.tmux).unwrap();
            assert_eq!(seen.3.records, before.3.records + 1);
            assert_eq!(
                seen.3.assistant_bytes,
                before.3.assistant_bytes + "한🙂unknown".len() as u64
            );
            assert_eq!(
                seen.3.source_bytes,
                before.3.source_bytes + unknown.len() as u64
            );
            super::super::super::super::relay_observed_prompt(&fx.shared, events.remove(0)).await;
            fx.append(&answer_for(&turn(2), STEER_ANSWER));
            assert!(fx.delivered(STEER_ANSWER).await);
            assert!(
                fx.sent("한🙂unknown").is_empty(),
                "unknown scan is observation, never historical delivery"
            );
            fx.finish();
        })
    });
}

#[test]
fn codex_6903_parked_terminal_rejects_same_nonce_actor_replacement_after_admission() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::Allocation),
        ))
    });
}

async fn codex_6903_main_writer_first_complete(root: PathBuf, writer_first: bool) {
    let _tmux = live_pane_tmux(&root);
    let tmux = if writer_first {
        "AgentDesk-codex-6903-writer-first"
    } else {
        "AgentDesk-codex-6903-custody"
    };
    let mut fx = Fixture::start(&root, 6_903_901 + u64::from(writer_first), tmux).await;
    discord_row(&fx, 6_903_201);
    std::mem::forget(reserve(&fx));
    send_captured_hook(&fx, STEER, &turn(2));
    held(&fx, 1).await;
    let old = user_record_at(
        "legacy L0",
        &turn(10),
        chrono::Utc::now() - chrono::TimeDelta::minutes(10),
    );
    fx.append(&(old + &answer_for(&turn(10), "LEGACY_6903_L0")));
    let origin_start = std::fs::metadata(&fx.codex.rollout).unwrap().len();
    let records = opening_record(&turn(1), ORIGIN)
        + &answer_for(&turn(1), DISCORD_ANSWER)
        + &opening_record(&turn(11), "legacy L1")
        + &answer_for(&turn(11), "LEGACY_6903_L1")
        + &opening_record(&turn(2), STEER)
        + &answer_for(&turn(2), STEER_ANSWER)
        + &opening_record(&turn(12), "legacy L2")
        + &answer_for(&turn(12), "LEGACY_6903_L2");
    let boundary = fx.append(&records);
    dedupe::reset_state_for_tests();
    let restore = super::super::super::super::codex_tui_rehydrated_binding_from_rollout_path;
    let binding = restore(tmux, &fx.codex.rollout, Some("s-5704".into())).unwrap();
    assert!(
        origin_start < binding.last_offset && binding.last_offset < boundary,
        "handoff input floor: {binding:?}"
    );
    dedupe::register_tmux_runtime_binding(tmux, binding.clone());
    if writer_first {
        let relay = crate::services::discord::recovery_engine::codex_rebind_complete_for_tests(
            tmux,
            fx.codex.rollout.to_str().unwrap(),
            origin_start,
        );
        assert!(
            wait_for(Duration::from_secs(10), || {
                dedupe::runtime_binding_for_tmux_session(tmux)
                    .is_some_and(|binding| binding.last_offset == boundary)
            })
            .await,
            "writer complete checkpoint must precede raw EOF"
        );
        assert!(
            std::fs::read_to_string(relay)
                .unwrap()
                .contains(DISCORD_ANSWER)
        );
        assert!(
            fx.sent(STEER_ANSWER).is_empty(),
            "normalized writer is not Discord delivery"
        );
    }
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    *super::super::super::super::codex_idle_rollout::CAPTURE_PAUSE
        .lock()
        .unwrap() = Some((fx.codex.channel.get(), entered.clone(), resume.clone()));
    assert!(inflight::clear_inflight_state(
        &ProviderKind::Codex,
        fx.codex.channel.get()
    ));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut admitted = 0;
    while let Ok(event) = tokio::time::timeout_at(deadline, fx.observed.recv()).await {
        let event = event.unwrap();
        if event.tmux_session_name != tmux {
            continue;
        }
        assert!(
            !event.prompt.starts_with("legacy L"),
            "unrelated historical input admitted: {event:?}"
        );
        if event.prompt == STEER {
            assert!(!event.steer_echo);
            admitted += 1;
            super::super::super::super::relay_observed_prompt(&fx.shared, event).await;
            tokio::time::timeout(Duration::from_secs(5), entered.notified())
                .await
                .expect("actual own-turn reader reached the adapter pause");
            let input_cursor = dedupe::runtime_binding_for_tmux_session(tmux)
                .unwrap()
                .last_offset;
            assert!(
                input_cursor <= boundary,
                "input cursor stays within the bootstrap boundary"
            );
            let second_relay =
                crate::services::discord::recovery_engine::codex_rebind_complete_for_tests(
                    tmux,
                    fx.codex.rollout.to_str().unwrap(),
                    origin_start,
                );
            assert!(
                wait_for(Duration::from_secs(10), || {
                    dedupe::runtime_binding_for_tmux_session(tmux)
                        .is_some_and(|binding| binding.last_offset == boundary)
                })
                .await,
                "actual concurrent writer must finish before adapter admission"
            );
            assert!(
                std::fs::read_to_string(second_relay)
                    .unwrap()
                    .contains(DISCORD_ANSWER)
            );
            assert!(
                fx.sent(STEER_ANSWER).is_empty(),
                "writer is not a Discord ACK"
            );
            resume.notify_one();
            if fx.delivered(STEER_ANSWER).await {
                break;
            }
        }
    }
    assert_eq!(admitted, 1, "parked input is recovered once");
    assert!(
        fx.delivered(STEER_ANSWER).await,
        "already completed parked answer was lost: row={:?}, lease={:?}, requests={:?}",
        fx.codex.row(),
        fx.lease(),
        fx.requests.lock().unwrap()
    );
    fx.polls(3).await;
    assert_eq!(tail_starts(fx.codex.channel), 1);
    assert!(
        fx.sent("LEGACY_6903_").is_empty(),
        "legacy responses are not custody"
    );
    let anchor = fx.anchor_of(STEER_ANSWER);
    fx.assert_answer(STEER_ANSWER, anchor);
    assert_eq!(fx.completed_turns(), 1);
    let receipts = native_receipts(fx.codex.channel.get());
    assert_eq!(receipts.len(), 1);
    let record = crate::services::discord::outbound::delivery_record::read_record(
        &ProviderKind::Codex,
        fx.codex.channel.get(),
    )
    .unwrap();
    assert_eq!(
        record
            .delivered_frontier
            .as_ref()
            .map(|frontier| frontier.range),
        Some(receipts[0].source.range),
        "current source delivery must commit its actual terminal range, not only a historical receipt"
    );
    assert!(
        receipts[0].source.range.0 < receipts[0].source.range.1
            && receipts[0].source.range.1 < boundary
    );
    assert_eq!(receipts[0].message_id, anchor);
    assert!(
        dedupe::runtime_binding_for_tmux_session(tmux)
            .is_some_and(|binding| binding.last_offset == boundary)
    );
    fx.finish();
}

#[test]
fn codex_6903_main_writer_first_rejects_completed_parked_answer() {
    run(|root| Box::pin(codex_6903_main_writer_first_complete(root, true)));
}

async fn actual_partial_watcher_restore(root: PathBuf, marker: bool) {
    let _tmux = live_pane_tmux(&root);
    let tmux = if marker {
        "AgentDesk-codex-6903-watcher-marker-cdx"
    } else {
        "AgentDesk-codex-6903-watcher-markerless-cdx"
    };
    let mut fx = Fixture::start(&root, 6_903_301 + u64::from(marker), tmux).await;
    let native = PathBuf::from(crate::services::tmux_common::session_temp_path(
        tmux, "jsonl",
    ));
    std::fs::copy(&fx.codex.rollout, &native).unwrap();
    fx.codex.rollout = native;
    fx.append(&answer_for(&turn(10), "LEGACY_6903_WATCHER"));
    let boundary = std::fs::metadata(&fx.codex.rollout).unwrap().len();
    fx.append(user_record(STEER, &turn(2)).trim_end_matches('\n'));
    let raw_end = std::fs::metadata(&fx.codex.rollout).unwrap().len();
    fx.shared.tmux_watchers.remove(&fx.codex.channel);
    dedupe::reset_state_for_tests();
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        tmux,
        RuntimeHandoffKind::CodexTui,
    )
    .unwrap();
    crate::services::tmux_common::write_tmux_channel_binding(tmux, fx.codex.channel.get()).unwrap();
    if marker {
        crate::services::codex_tui::session::write_codex_tui_rollout_marker(
            tmux,
            &fx.codex.rollout,
            Some("s-5704"),
        )
        .unwrap();
    }
    std::fs::write(root.join("fake-tmux/names"), format!("{tmux}\n")).unwrap();
    {
        let mut settings = fx.shared.settings.write().await;
        settings.provider = ProviderKind::Codex;
        settings.allowed_channel_ids = vec![fx.codex.channel.get()];
    }
    let submitted =
        crate::services::discord::task_supervisor::watcher_spawn_observation::Scope::new(
            &fx.shared,
        );
    crate::services::discord::restore_tmux_watchers(&fx.http, &fx.shared).await;
    assert_eq!(
        submitted.submissions(),
        1,
        "actual restore must claim and spawn a watcher"
    );
    assert_eq!(
        crate::services::discord::tmux::watcher_install_fixture::reader_start(
            fx.codex.channel.get()
        ),
        Some(boundary),
        "actual native constructor must start before the unsealed suffix"
    );
    assert!(boundary < raw_end);
    let binding = super::super::super::super::codex_tui_rehydrated_binding_from_rollout_path(
        tmux,
        &fx.codex.rollout,
        Some("s-5704".into()),
    )
    .unwrap();
    dedupe::register_tmux_runtime_binding(tmux, binding);
    fx.polls(3).await;
    assert!(
        fx.observed.try_recv().is_err(),
        "partial user must not be admitted"
    );
    assert!(fx.sent(STEER_ANSWER).is_empty());
    let sink = Arc::new(
        crate::services::discord::session_relay_sink::SessionBoundDiscordRelaySink::new(
            fx._registry.clone(),
        ),
    );
    fx._registry
        .register_http("codex".into(), fx.http.clone())
        .await;
    let relay = crate::services::cluster::stream_relay::spawn_stream_relay(
        crate::services::cluster::session_matcher::MatchedChannel {
            channel_id: fx.codex.channel.get().to_string(),
            agent_id: "i6903".into(),
            provider: ProviderKind::Codex,
            expected_session_name: tmux.into(),
            expected_rollout_path: fx.codex.rollout.to_string_lossy().into_owned(),
        },
        sink,
    );
    crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
        .register(tmux.into(), relay.producer());
    fx.append("\n");
    let mut events = events_for(&mut fx, STEER, Duration::from_secs(3)).await;
    assert_eq!(events.len(), 1, "sealed prompt admits once");
    super::super::super::super::relay_observed_prompt(&fx.shared, events.remove(0)).await;
    fx.append(&answer_for(&turn(2), STEER_ANSWER));
    assert!(
        fx.delivered(STEER_ANSWER).await,
        "suffix seal must preserve the complete prompt and its answer"
    );
    assert!(fx.sent("LEGACY_6903_WATCHER").is_empty());
    assert_eq!(
        fx.sent(STEER_ANSWER)
            .iter()
            .filter(|(method, _, _)| method == "POST")
            .count(),
        1
    );
    submitted.cancel_and_join_all().await;
    fx.finish();
}

#[test]
fn codex_6903_actual_marker_watcher_starts_before_the_partial_user() {
    run(|root| Box::pin(actual_partial_watcher_restore(root, true)));
}

#[test]
fn codex_6903_actual_markerless_watcher_starts_before_the_partial_user() {
    run(|root| Box::pin(actual_partial_watcher_restore(root, false)));
}

/// A user record with the metadata codex-cli 0.160 writes: its turn and creation time.
fn user_record(text: &str, turn: &str) -> String {
    user_record_at(text, turn, chrono::Utc::now())
}

fn user_record_at(text: &str, turn: &str, created: chrono::DateTime<chrono::Utc>) -> String {
    let created = created.timestamp_millis() as f64 / 1000.0;
    rollout_line(serde_json::json!({"type": "response_item", "payload": {
        "type": "message", "role": "user", "content": [{"type": "input_text", "text": text}],
        "internal_chat_message_metadata_passthrough": {
            "turn_id": turn, "create_time": created, "content_item_kinds": ["user.text"]}}}))
}

fn opening_record(turn: &str, text: &str) -> String {
    rollout_line(serde_json::json!({"type": "event_msg", "payload": {
        "type": "task_started", "turn_id": turn}}))
        + &rollout_line(serde_json::json!({"type": "turn_context", "payload": {"turn_id": turn}}))
        + &user_record(text, turn)
}

/// The captured codex-cli UserPromptSubmit payload, re-aimed at `prompt` in `turn`.
fn send_captured_hook(fx: &Fixture, prompt: &str, turn: &str) {
    send_captured_hook_on(&fx.hooks, &fx.codex.tmux, prompt, turn);
}

fn send_captured_hook_on(
    hooks: &tokio::sync::broadcast::Sender<HookEvent>,
    tmux: &str,
    prompt: &str,
    turn: &str,
) {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hook_payload/codex-0.157.1.json"
    )))
    .expect("captured hook fixture");
    let mut payload = fixture["runs"][1]["events"]
        .as_array()
        .expect("captured events")
        .iter()
        .find(|event| event["event"] == "UserPromptSubmit")
        .expect("captured UserPromptSubmit")["payload"]
        .clone();
    payload["prompt"] = prompt.into();
    if turn.is_empty() {
        payload.as_object_mut().unwrap().remove("turn_id");
    } else {
        payload["turn_id"] = turn.into();
    }
    hooks
        .send(HookEvent {
            provider: "codex".to_string(),
            session_id: tmux.to_string(),
            kind: HookEventKind::UserPromptSubmit,
            received_at: chrono::Utc::now(),
            payload,
            fanout: None,
        })
        .expect("hook observer listening");
}

/// The Discord turn's row; the idle scan waits while it stands.
fn discord_row(fx: &Fixture, anchor: u64) {
    let row = inflight::InflightTurnState::new(
        ProviderKind::Codex,
        fx.codex.channel.get(),
        None,
        1,
        anchor,
        anchor + 1,
        ORIGIN.to_string(),
        Some("s-5704".to_string()),
        Some(fx.codex.tmux.clone()),
        Some(fx.codex.rollout.to_str().unwrap().to_string()),
        None,
        0,
    );
    inflight::save_inflight_state(&row).expect("discord row");
}

/// The Discord turn's handoff as the warm path holds it before submitting.
fn reserve(fx: &Fixture) -> dedupe::HandoffGuard {
    let guard = dedupe::reserve_discord_handoff("codex", &fx.codex.tmux, ORIGIN);
    dedupe::record_discord_originated_prompt("codex", &fx.codex.tmux, ORIGIN);
    guard
}

fn terminal_results(
    rx: std::sync::mpsc::Receiver<crate::services::agent_protocol::StreamMessage>,
) -> Vec<String> {
    rx.try_iter()
        .filter_map(|message| match message {
            crate::services::agent_protocol::StreamMessage::Done { result, .. }
            | crate::services::agent_protocol::StreamMessage::CodexTuiTerminalDone {
                result, ..
            } => Some(result),
            _ => None,
        })
        .collect()
}

/// The production warm handoff tail from the rollout's current end.
fn start_handoff_tail(fx: &Fixture) -> std::thread::JoinHandle<Vec<String>> {
    let rollout = fx.codex.rollout.clone();
    let tmux = fx.codex.tmux.clone();
    let start = std::fs::metadata(&rollout).unwrap().len();
    std::thread::spawn(move || {
        let (sender, rx) = std::sync::mpsc::channel();
        crate::services::codex_tui::rollout_tail::tail_warm_followup_rollout_for_tmux(
            &rollout,
            start,
            "s-5704",
            sender,
            None,
            || true,
            &tmux,
            ORIGIN,
        )
        .expect("handoff tail");
        terminal_results(rx)
    })
}

async fn tail_answers(tail: std::thread::JoinHandle<Vec<String>>) -> Vec<String> {
    assert!(
        wait_for(Duration::from_secs(15), || tail.is_finished()).await,
        "the handoff tail never returned"
    );
    tail.join().expect("handoff tail thread")
}

/// Events published for `prompt` within `window`, unrelayed.
async fn events_for(fx: &mut Fixture, prompt: &str, window: Duration) -> Vec<ObservedTuiPrompt> {
    let (deadline, mut events) = (tokio::time::Instant::now() + window, Vec::new());
    while let Ok(event) = tokio::time::timeout_at(deadline, fx.observed.recv()).await {
        let event = event.expect("observation channel open");
        if event.tmux_session_name == fx.codex.tmux && event.prompt == prompt {
            events.push(event);
        }
    }
    events
}

async fn held(fx: &Fixture, inputs: usize) {
    let tmux = fx.codex.tmux.clone();
    assert!(
        wait_for(Duration::from_secs(5), || {
            dedupe::held_handoff_inputs_for_tests("codex", &tmux) == Some(inputs)
        })
        .await,
        "the handoff never held {inputs} input(s)"
    );
}

/// Only the echo reached Discord for the input, the Discord answer came from its own tail,
/// and nothing opened a turn for the input; `row` is the Discord row still standing.
async fn assert_echo_only(fx: &Fixture, row: Option<u64>, answers: &[String]) {
    fx.polls(3).await;
    assert_eq!(
        answers,
        [DISCORD_ANSWER.to_string()],
        "the handoff tail answers once"
    );
    assert_eq!(fx.sent(STEER).len(), 1, "the input is echoed once");
    assert!(fx.sent(DISCORD_ANSWER).is_empty(), "no relay answer");
    assert!(
        fx.sent("codex discord prompt 5845").is_empty(),
        "the Discord prompt is not relayed"
    );
    assert!(fx.pending_starts().is_empty(), "no deferred start");
    assert_eq!(tail_starts(fx.codex.channel), 0, "no repair tail");
    assert_eq!(
        fx.codex.row().map(|row| row.user_msg_id),
        row,
        "no row for the input"
    );
}

/// The input's own owning event, relayed once its turn's records are scanned: its answer
/// reaches its own anchor.
async fn assert_own_answer(fx: &mut Fixture, event: ObservedTuiPrompt, turn: &str) {
    assert!(!event.steer_echo, "the input owns its turn");
    assert_eq!(event.native_turn_id.as_deref(), Some(turn));
    let user = user_record(STEER, turn);
    let started = rollout_line(serde_json::json!({"type": "event_msg", "payload": {
        "type": "task_started", "turn_id": turn}}));
    let end = fx.append(
        &(user + &started + &item_completed_line_for(turn) + &answer_for(turn, STEER_ANSWER)),
    );
    assert!(
        matches!(
            super::super::super::super::idle_transcript_scan::resolve_codex_turn_prompt_end(
                &fx.codex.rollout,
                turn,
                STEER
            )
            .unwrap(),
            super::super::super::super::idle_transcript_scan::CodexTurnPrompt::Found(_)
        ),
        "a user-first record resolves its own turn"
    );
    fx.scanned_past(end).await;
    super::super::super::super::relay_observed_prompt(&fx.shared, event).await;
    let anchor = fx.codex.row().expect("the input's row").user_msg_id;
    fx.assert_answers_in_order(&[STEER_ANSWER], Some(1)).await;
    assert_eq!(fx.anchor_of(STEER_ANSWER), anchor);
    assert!(
        events_for(fx, STEER, Duration::from_secs(1))
            .await
            .is_empty(),
        "published once"
    );
}

/// The handoff tail reads the Discord prompt and the input in its turn, no hook first.
#[test]
fn codex_input_read_after_the_discord_prompt_is_only_echoed() {
    run(|root| {
        Box::pin(async move {
            let mut fx = Fixture::start(&root, 5_845_010, "AgentDesk-codex-5845-seq").await;
            let t1 = turn(1);
            discord_row(&fx, 5_845_011);
            let guard = reserve(&fx);
            let tail = start_handoff_tail(&fx);
            fx.append(
                &(opening_record(&t1, ORIGIN)
                    + &user_record(STEER, &t1)
                    + &answer_for(&t1, DISCORD_ANSWER)),
            );
            assert_eq!(fx.relay_echoes(STEER).await, 1, "one echo");
            let answers = tail_answers(tail).await;
            drop(guard);
            assert_echo_only(&fx, Some(5_845_011), &answers).await;
            fx.finish();
        })
    });
}

/// The input's hook lands before the handoff tail read the Discord prompt: it is held, then
/// echoed once the Discord prompt's record names the same turn.
#[test]
fn codex_input_hooked_before_the_discord_prompt_is_only_echoed() {
    run(|root| {
        Box::pin(async move {
            let mut fx = Fixture::start(&root, 5_845_020, "AgentDesk-codex-5845-rev").await;
            let t1 = turn(1);
            discord_row(&fx, 5_845_021);
            let guard = reserve(&fx);
            let tail = start_handoff_tail(&fx);
            send_captured_hook(&fx, STEER, &t1);
            held(&fx, 1).await;
            assert!(
                events_for(&mut fx, STEER, Duration::from_millis(500))
                    .await
                    .is_empty()
            );
            fx.append(
                &(opening_record(&t1, ORIGIN)
                    + &user_record(STEER, &t1)
                    + &answer_for(&t1, DISCORD_ANSWER)),
            );
            assert_eq!(fx.relay_echoes(STEER).await, 1, "one echo");
            let answers = tail_answers(tail).await;
            drop(guard);
            assert_echo_only(&fx, Some(5_845_021), &answers).await;
            fx.finish();
        })
    });
}

/// The held input names the next turn: once the Discord turn ends, it owns that turn and gets
/// its own answer, and the Discord answer stays the handoff tail's.
#[test]
fn codex_input_hooked_into_the_next_turn_gets_its_own_answer() {
    run(|root| {
        Box::pin(async move {
            let mut fx = Fixture::start(&root, 5_845_030, "AgentDesk-codex-5845-next").await;
            let (t1, t2) = (turn(1), turn(2));
            discord_row(&fx, 5_845_031);
            let guard = reserve(&fx);
            let tail = start_handoff_tail(&fx);
            send_captured_hook(&fx, STEER, &t2);
            held(&fx, 1).await;
            fx.append(&(opening_record(&t1, ORIGIN) + &answer_for(&t1, DISCORD_ANSWER)));
            // The next turn's records land only after the handoff tail returned.
            let answers = tail_answers(tail).await;
            assert_eq!(answers, [DISCORD_ANSWER.to_string()]);
            drop(guard);
            assert!(crate::services::discord::inflight::clear_inflight_state(
                &ProviderKind::Codex,
                fx.codex.channel.get()
            ));
            let mut events = events_for(&mut fx, STEER, Duration::from_secs(1)).await;
            assert_eq!(events.len(), 1, "{events:?}");
            assert_own_answer(&mut fx, events.remove(0), &t2).await;
            assert!(
                fx.sent(DISCORD_ANSWER).is_empty(),
                "no relay answer for the Discord turn"
            );
            fx.finish();
        })
    });
}

/// The handoff ends without its prompt ever being read: the held input is published once.
#[test]
fn codex_input_held_by_an_unsubmitted_handoff_is_published_once() {
    run(|root| {
        Box::pin(async move {
            let mut fx = Fixture::start(&root, 5_845_040, "AgentDesk-codex-5845-rel").await;
            let guard = reserve(&fx);
            send_captured_hook(&fx, STEER, &turn(1));
            held(&fx, 1).await;
            drop(guard);
            let events = events_for(&mut fx, STEER, Duration::from_secs(1)).await;
            assert_eq!(events.len(), 1, "{events:?}");
            assert!(!events[0].steer_echo && events[0].source_event_id.is_none());
            fx.finish();
        })
    });
}

/// The scan reads an earlier record of the Discord prompt's text: it binds nothing, so an input
/// hooked next still waits for the submission's own record.
#[test]
fn codex_scan_of_an_earlier_discord_prompt_record_binds_nothing() {
    run(|root| {
        Box::pin(async move {
            let mut fx = Fixture::start(&root, 5_845_090, "AgentDesk-codex-5845-stale").await;
            let (t0, t1) = (turn(0), turn(1));
            let guard = reserve(&fx);
            let earlier = chrono::Utc::now() - chrono::TimeDelta::minutes(5);
            let started = rollout_line(serde_json::json!({"type": "event_msg", "payload": {
                "type": "task_started", "turn_id": t0}}));
            let end = fx.append(
                &(started
                    + &user_record_at(ORIGIN, &t0, earlier)
                    + &answer_for(&t0, "EARLIER_5845")),
            );
            fx.scanned_past(end).await;
            send_captured_hook(&fx, STEER, &t1);
            held(&fx, 1).await;
            drop(guard);
            let events = events_for(&mut fx, STEER, Duration::from_secs(1)).await;
            assert_eq!(events.len(), 1, "{events:?}");
            fx.finish();
        })
    });
}

/// After a Discord turn ended with no input, a later direct input the scan reads first owns
/// its turn.
#[test]
fn codex_input_scanned_after_a_finished_discord_turn_owns_its_turn() {
    run(|root| {
        Box::pin(async move {
            let mut fx = Fixture::start(&root, 5_845_050, "AgentDesk-codex-5845-scan").await;
            let (t1, t2) = (turn(1), turn(2));
            discord_row(&fx, 5_845_051);
            let guard = reserve(&fx);
            let tail = start_handoff_tail(&fx);
            fx.append(&(opening_record(&t1, ORIGIN) + &answer_for(&t1, DISCORD_ANSWER)));
            assert_eq!(tail_answers(tail).await, [DISCORD_ANSWER.to_string()]);
            drop(guard);
            assert!(crate::services::discord::inflight::clear_inflight_state(
                &ProviderKind::Codex,
                fx.codex.channel.get()
            ));
            let entered = Arc::new(tokio::sync::Notify::new());
            let resume = Arc::new(tokio::sync::Notify::new());
            *super::super::super::super::codex_idle_rollout::SCAN_LEASE_PAUSE
                .lock()
                .unwrap() = Some((fx.codex.channel.get(), entered.clone(), resume.clone()));
            fx.append(
                &(user_record(PROMPT3, &t2)
                    + &rollout_line(serde_json::json!({"type":"event_msg", "payload": {
                    "type":"task_started", "turn_id": t2}}))
                    + &item_completed_line_for(&t2)),
            );
            tokio::time::timeout(Duration::from_secs(5), entered.notified())
                .await
                .expect("scanner selected its lease");
            let event = tokio::time::timeout(Duration::from_secs(5), fx.observed.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(event.prompt, PROMPT3);
            assert_eq!(event.tmux_session_name, fx.codex.tmux);
            assert!(!event.steer_echo);
            assert_eq!(event.native_turn_id.as_deref(), Some(t2.as_str()));
            super::super::super::super::relay_observed_prompt(&fx.shared, event).await;
            let anchor = fx.codex.row().expect("the input's row").user_msg_id;
            fx.append(&answer_for(&t2, RESPONSE3));
            resume.notify_one();
            fx.assert_answers_in_order(&[RESPONSE3], Some(1)).await;
            assert_eq!(fx.anchor_of(RESPONSE3), anchor);
            assert!(fx.sent(DISCORD_ANSWER).is_empty());
            fx.finish();
        })
    });
}

/// The production hook observer on its own thread, so a hook paused mid-decision leaves the
/// idle loop running.
struct HookThread {
    hooks: tokio::sync::broadcast::Sender<HookEvent>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HookThread {
    fn start() -> Self {
        let (hooks, hook_rx) = tokio::sync::broadcast::channel(8);
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("hook runtime");
            runtime.block_on(async move {
                super::super::super::super::hook_observer::spawn_tui_prompt_relay_observer_inner(
                    "codex".to_string(),
                    hook_rx,
                    |_| Box::pin(async {}),
                    None,
                );
                let _ = stopped.await;
            });
        });
        Self {
            hooks,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for HookThread {
    fn drop(&mut self) {
        let _ = self.stop.take().map(|stop| stop.send(()));
        let _ = self.thread.take().map(std::thread::JoinHandle::join);
    }
}

/// The scan reads a direct input while its hook already decided to own the turn but has not
/// published yet: the hook's owning event is the only one, and the answer reaches its anchor.
#[test]
fn codex_input_scanned_between_its_hook_decision_and_publish_is_published_once() {
    run(|root| {
        Box::pin(async move {
            let mut fx = Fixture::start(&root, 5_845_060, "AgentDesk-codex-5845-race").await;
            let t1 = turn(1);
            let (paused_tx, paused_rx) = std::sync::mpsc::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let gate = std::sync::Mutex::new(Some((paused_tx, release_rx)));
            let _probe = dedupe::on_owning_publish(move |prompt| {
                if prompt != PROMPT {
                    return;
                }
                if let Some((paused, release)) = gate.lock().unwrap().take() {
                    let _ = paused.send(());
                    let _ = release.recv_timeout(Duration::from_secs(20));
                }
            });
            let hook = HookThread::start();
            send_captured_hook_on(&hook.hooks, &fx.codex.tmux, PROMPT, &t1);
            assert!(
                wait_for(Duration::from_secs(5), || paused_rx.try_recv().is_ok()).await,
                "the hook never reached its publish"
            );
            let end = fx.append(&(opening(&t1, PROMPT) + &item_completed_line_for(&t1)));
            fx.scanned_past(end).await;
            release_tx.send(()).expect("hook waiting");
            let mut events = events_for(&mut fx, PROMPT, Duration::from_secs(2)).await;
            assert_eq!(events.len(), 1, "one owning event, no echo: {events:?}");
            let event = events.remove(0);
            assert!(!event.steer_echo);
            super::super::super::super::relay_observed_prompt(&fx.shared, event).await;
            let anchor = fx.codex.row().expect("the input's row").user_msg_id;
            fx.append(&answer_for(&t1, RESPONSE));
            fx.assert_answers_in_order(&[RESPONSE], Some(1)).await;
            assert_eq!(fx.anchor_of(RESPONSE), anchor);
            drop(hook);
            fx.finish();
        })
    });
}

/// The process stops while a handoff holds an input; the restarted process restores the
/// binding through the rehydration path, and the scan settles the input from the records.
async fn restart_with_held_input(
    root: PathBuf,
    channel: u64,
    tmux: &str,
    same_turn: bool,
    already_bound: bool,
    records_after_restore: bool,
) {
    let _tmux = live_pane_tmux(&root);
    let mut fx = Fixture::start(&root, channel, tmux).await;
    let (t1, t2) = (turn(1), turn(2));
    let steer_turn = if same_turn { &t1 } else { &t2 };
    discord_row(&fx, channel + 1);
    std::mem::forget(reserve(&fx));
    send_captured_hook(&fx, STEER, if already_bound { "" } else { steer_turn });
    held(&fx, 1).await;
    if already_bound {
        let origin: serde_json::Value = serde_json::from_str(&user_record(ORIGIN, &t1)).unwrap();
        assert_eq!(
            dedupe::bind_discord_handoff_record(
                tmux,
                ORIGIN,
                &dedupe::RecordEvidence::from_record(&origin)
            ),
            dedupe::BindOutcome::Bound
        );
        held(&fx, 1).await;
    }
    assert!(dedupe::durable_handoff_present_for_tests("codex", tmux));
    let a_start = std::fs::metadata(&fx.codex.rollout).unwrap().len();
    let mut records = opening_record(&t1, ORIGIN);
    if same_turn {
        records += &user_record(STEER, &t1);
    }
    records += &answer_for(&t1, DISCORD_ANSWER);
    if already_bound && !same_turn {
        records += &user_record(STEER, &t2);
    }
    let eof = if records_after_restore {
        a_start
    } else {
        fx.append(&records)
    };

    // SIGTERM marks the rows and exits without dropping the guard; memory is lost, the mirror stays.
    let mode = crate::services::discord::InflightRestartMode::DrainRestart;
    let marked = inflight::mark_restart_mode_blocking(ProviderKind::Codex, mode).await;
    assert_eq!(marked.marked, 1, "{marked:?}");
    dedupe::reset_state_for_tests();
    let restore = super::super::super::super::codex_tui_rehydrated_binding_from_rollout_path;
    let binding = restore(tmux, &fx.codex.rollout, Some("s-5704".to_string())).expect("binding");
    assert!(
        if records_after_restore {
            binding.last_offset == eof
        } else {
            binding.last_offset > a_start && binding.last_offset < eof
        },
        "{}",
        binding.last_offset
    );
    dedupe::register_tmux_runtime_binding(tmux, binding);
    let plain = restore("AgentDesk-codex-5845-plain", &fx.codex.rollout, None).expect("binding");
    assert_eq!(
        plain.last_offset, eof,
        "no held handoff keeps the end-of-file cursor"
    );

    let eof = if records_after_restore {
        fx.append(&records)
    } else {
        eof
    };
    // The actual recovery writer consumes the response range and advances the shared cursor.
    let relay = crate::services::discord::recovery_engine::codex_rebind_complete_for_tests(
        tmux,
        fx.codex.rollout.to_str().unwrap(),
        a_start,
    );
    assert!(
        wait_for(Duration::from_secs(10), || {
            dedupe::runtime_binding_for_tmux_session(tmux)
                .is_some_and(|binding| binding.last_offset == eof)
                && crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux)
                    .is_some_and(|marker| marker.rollout_start_offset == Some(eof))
        })
        .await,
        "recovery writer never advanced its cursor"
    );
    let normalized = std::fs::read_to_string(&relay).unwrap();
    let answers = normalized
        .lines()
        .filter_map(|line| {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            (row["type"] == "result").then(|| row["result"].as_str().unwrap().to_string())
        })
        .collect::<Vec<_>>();
    held(&fx, 1).await;
    assert!(crate::services::discord::inflight::clear_inflight_state(
        &ProviderKind::Codex,
        fx.codex.channel.get()
    ));
    fx.polls(3).await;
    if same_turn {
        assert_eq!(fx.relay_echoes(STEER).await, 1, "one echo");
        assert_echo_only(&fx, None, &answers).await;
    } else {
        let mut events = events_for(&mut fx, STEER, Duration::from_secs(5)).await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(answers, [DISCORD_ANSWER.to_string()]);
        assert_own_answer(&mut fx, events.remove(0), &t2).await;
        assert!(fx.sent(DISCORD_ANSWER).is_empty());
    }
    assert!(!dedupe::durable_handoff_present_for_tests("codex", tmux));
    fx.finish();
}

#[test]
fn codex_input_held_across_a_restart_is_only_echoed() {
    run(|root| {
        Box::pin(restart_with_held_input(
            root,
            5_845_070,
            "AgentDesk-codex-5845-restart",
            true,
            false,
            false,
        ))
    });
}

#[test]
fn codex_input_held_across_a_restart_for_the_next_turn_gets_its_own_answer() {
    run(|root| {
        Box::pin(restart_with_held_input(
            root,
            5_845_080,
            "AgentDesk-codex-5845-restart-next",
            false,
            false,
            false,
        ))
    });
}

#[test]
fn codex_bound_handoff_keeps_a_turnless_input_across_restart_in_its_turn() {
    run(|root| {
        Box::pin(restart_with_held_input(
            root,
            5_845_100,
            "AgentDesk-codex-5845-bound-restart",
            true,
            true,
            false,
        ))
    });
}

#[test]
fn codex_bound_handoff_keeps_a_turnless_input_across_restart_in_the_next_turn() {
    run(|root| {
        Box::pin(restart_with_held_input(
            root,
            5_845_110,
            "AgentDesk-codex-5845-bound-restart-next",
            false,
            true,
            false,
        ))
    });
}

#[test]
fn codex_held_input_restores_before_its_origin_record_is_written() {
    run(|root| {
        Box::pin(restart_with_held_input(
            root,
            5_845_120,
            "AgentDesk-codex-5845-late-origin",
            true,
            false,
            true,
        ))
    });
}

async fn rebind_complete_boundary(root: PathBuf) {
    let writer_first = true;
    let _tmux = live_pane_tmux(&root);
    let tmux = if writer_first {
        "AgentDesk-codex-6903-writer-first"
    } else {
        "AgentDesk-codex-6903-custody"
    };
    let fx = Fixture::start(&root, 6_903_901 + u64::from(writer_first), tmux).await;
    discord_row(&fx, 6_903_201);
    std::mem::forget(reserve(&fx));
    send_captured_hook(&fx, STEER, &turn(2));
    held(&fx, 1).await;
    let old = user_record_at(
        "legacy L0",
        &turn(10),
        chrono::Utc::now() - chrono::TimeDelta::minutes(10),
    );
    fx.append(&(old + &answer_for(&turn(10), "LEGACY_6903_L0")));
    let origin_start = std::fs::metadata(&fx.codex.rollout).unwrap().len();
    let records = opening_record(&turn(1), ORIGIN)
        + &answer_for(&turn(1), DISCORD_ANSWER)
        + &opening_record(&turn(11), "legacy L1")
        + &answer_for(&turn(11), "LEGACY_6903_L1")
        + &opening_record(&turn(2), STEER)
        + &answer_for(&turn(2), STEER_ANSWER)
        + &opening_record(&turn(12), "legacy L2")
        + &answer_for(&turn(12), "LEGACY_6903_L2");
    let boundary = fx.append(&records);
    fx.append(user_record("partial writer boundary 6903", &turn(99)).trim_end_matches('\n'));
    dedupe::reset_state_for_tests();
    let restore = super::super::super::super::codex_tui_rehydrated_binding_from_rollout_path;
    let binding = restore(tmux, &fx.codex.rollout, Some("s-5704".into())).unwrap();
    assert!(
        origin_start < binding.last_offset && binding.last_offset < boundary,
        "handoff input floor: {binding:?}"
    );
    dedupe::register_tmux_runtime_binding(tmux, binding.clone());
    assert!(boundary < std::fs::metadata(&fx.codex.rollout).unwrap().len());
    if writer_first {
        let relay = crate::services::discord::recovery_engine::codex_rebind_complete_for_tests(
            tmux,
            fx.codex.rollout.to_str().unwrap(),
            origin_start,
        );
        assert!(
            wait_for(Duration::from_secs(10), || {
                dedupe::runtime_binding_for_tmux_session(tmux)
                    .is_some_and(|binding| binding.last_offset == boundary)
            })
            .await,
            "writer complete checkpoint must precede raw EOF"
        );
        assert!(
            std::fs::read_to_string(relay)
                .unwrap()
                .contains(DISCORD_ANSWER)
        );
        assert!(
            fx.sent(STEER_ANSWER).is_empty(),
            "normalized writer is not Discord delivery"
        );
    }
    assert!(native_receipts(fx.codex.channel.get()).is_empty());
    fx.finish();
}

#[test]
fn codex_6903_rebind_writer_stores_the_complete_record_end() {
    run(|root| Box::pin(rebind_complete_boundary(root)));
}

#[test]
fn codex_6903_completed_parked_owner_also_releases_its_held_steer_on_rotation() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::RotationSteer),
        ))
    });
}

async fn markerless_saved_custody(root: PathBuf, custody: bool) {
    let _tmux = live_pane_tmux(&root);
    assert!(matches!(
        crate::config::shared_test_env_lock().try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    ));
    let _home =
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("CODEX_HOME", &root);
    let tmux = "AgentDesk-codex-6903-markerless-custody-cdx";
    let mut fx = Fixture::start(&root, 6_903_951, tmux).await;
    let sessions = root.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let native = sessions.join("rollout-markerless-custody.jsonl");
    std::fs::write(
        &native,
        rollout_line(serde_json::json!({
            "type":"session_meta", "payload":{"id":"s-5704", "cwd":root, "source":"cli"}
        })),
    )
    .unwrap();
    fx.codex.rollout = native;
    let mut binding = dedupe::runtime_binding_for_tmux_session(tmux).unwrap();
    binding.output_path = fx.codex.rollout.to_string_lossy().into_owned();
    binding.last_offset = std::fs::metadata(&fx.codex.rollout).unwrap().len();
    dedupe::register_tmux_runtime_binding(tmux, binding);
    let executable = root.join("fake-tmux/tmux");
    let script = std::fs::read_to_string(&executable).unwrap();
    let cwd = root.display().to_string().replace('\'', "'\\''");
    std::fs::write(
        &executable,
        script.replace(
            "list-sessions)",
            &format!("display-message) printf '%s\\n' '{cwd}';; list-sessions)"),
        ),
    )
    .unwrap();
    discord_row(&fx, 6_903_952);
    if custody {
        std::mem::forget(reserve(&fx));
        send_captured_hook(&fx, STEER, &turn(2));
        held(&fx, 1).await;
    }
    let b = fx.append(&(opening_record(&turn(1), ORIGIN) + &user_record(STEER, &turn(2))));
    let marked = inflight::mark_restart_mode_blocking(
        ProviderKind::Codex,
        crate::services::discord::InflightRestartMode::DrainRestart,
    )
    .await;
    assert_eq!(marked.marked, 1);
    dedupe::reset_state_for_tests();
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        tmux,
        RuntimeHandoffKind::CodexTui,
    )
    .unwrap();
    crate::services::tmux_common::write_tmux_channel_binding(tmux, fx.codex.channel.get()).unwrap();
    assert!(crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux).is_none());
    dedupe::register_tmux_channel(tmux, fx.codex.channel.get());
    super::super::super::super::rehydration::run_codex_rehydrate_pass_for_tests(&fx.shared, tmux);
    let binding = dedupe::runtime_binding_for_tmux_session(tmux)
        .expect("actual markerless discovery binding");
    assert_eq!(binding.output_path, fx.codex.rollout.to_string_lossy());
    if custody {
        assert_eq!(
            dedupe::held_handoff_inputs_for_tests("codex", tmux),
            Some(1),
            "markerless discovery skipped saved reservation"
        );
        assert!(
            binding.last_offset < b,
            "saved custody lost its independent input floor"
        );
    } else {
        assert_eq!(
            binding.last_offset, b,
            "no custody starts at the complete observation boundary"
        );
    }
    assert!(inflight::clear_inflight_state(
        &ProviderKind::Codex,
        fx.codex.channel.get()
    ));
    if custody {
        let mut events = events_for(&mut fx, STEER, Duration::from_secs(3)).await;
        assert_eq!(
            events.len(),
            1,
            "independent replay decides the parked input once"
        );
        let event = events.remove(0);
        assert!(!event.steer_echo);
        super::super::super::super::relay_observed_prompt(&fx.shared, event).await;
        let anchor = fx.codex.row().unwrap().user_msg_id;
        fx.append(&answer_for(&turn(2), STEER_ANSWER));
        assert!(
            fx.delivered(STEER_ANSWER).await,
            "restored markerless parked answer was lost"
        );
        fx.assert_answer(STEER_ANSWER, anchor);
        fx.polls(3).await;
        assert!(
            events_for(&mut fx, STEER, Duration::from_secs(1))
                .await
                .is_empty()
        );
        assert_eq!(fx.completed_turns(), 1);
    } else {
        fx.polls(3).await;
        assert!(
            events_for(&mut fx, STEER, Duration::from_secs(1))
                .await
                .is_empty()
        );
        assert!(fx.sent(STEER_ANSWER).is_empty());
    }
    fx.finish();
}

#[test]
fn codex_6903_markerless_discovery_restores_saved_parked_input() {
    run(|root| Box::pin(markerless_saved_custody(root, true)));
}

#[test]
fn codex_6903_markerless_discovery_without_custody_keeps_complete_boundary() {
    run(|root| Box::pin(markerless_saved_custody(root, false)));
}

/// Direct prompts are admitted once; seeing one again after its admission is a duplicate.
fn assert_direct_admission_unique(fx: &Fixture, event: &ObservedTuiPrompt) {
    assert!(
        event.tmux_session_name != fx.codex.tmux
            || !event.prompt.starts_with("DIRECT_6903_PROMPT_"),
        "direct admission is unique: {event:?}"
    );
}

fn terminal_only(native: &str, answer: &str) -> String {
    rollout_line(serde_json::json!({"type":"event_msg", "payload":{
        "type":"task_complete", "turn_id":native, "last_agent_message":answer}}))
}

/// The next call watches this prompt for re-admission; `last` keeps watching to the
/// admission deadline instead, since no later call will.
async fn deliver_direct_terminal(fx: &mut Fixture, n: u32, last: bool) {
    let prompt = format!("DIRECT_6903_PROMPT_{n}");
    let answer = format!("DIRECT_6903_ANSWER_{n}");
    fx.append(&opening(&turn(n), &prompt));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let event = loop {
        let event = tokio::time::timeout_at(deadline, fx.observed.recv())
            .await
            .expect("direct admission is published")
            .expect("observation channel open");
        if event.tmux_session_name == fx.codex.tmux && event.prompt == prompt {
            break event;
        }
        assert_direct_admission_unique(fx, &event);
    };
    super::super::super::super::relay_observed_prompt(&fx.shared, event).await;
    let end = fx.append(&terminal_only(&turn(n), &answer));
    assert!(fx.delivered(&answer).await);
    assert!(
        wait_for(Duration::from_secs(5), || native_receipts(
            fx.codex.channel.get()
        )
        .iter()
        .any(|r| r.source.range.1 == end))
        .await,
        "actual receipt must commit"
    );
    assert!(
        wait_for(Duration::from_secs(5), || fx.lease().is_none()).await,
        "completed direct lease must release"
    );
    loop {
        match fx.observed.try_recv() {
            Ok(event) => assert_direct_admission_unique(fx, &event),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(error) => panic!("direct admission observations lost: {error:?}"),
        }
    }
    if last {
        while let Ok(event) = tokio::time::timeout_at(deadline, fx.observed.recv()).await {
            assert_direct_admission_unique(fx, &event.expect("observation channel open"));
        }
    }
    assert_eq!(fx.sent(&answer).len(), 1);
    fx.assert_answer(&answer, fx.anchor_of(&answer));
}

async fn direct_pending_rotation(root: &Path, fx: &mut Fixture) {
    let prompt = "DIRECT_Q_6903";
    let answer = "DIRECT_Q_6903_ANSWER";
    let tmux = fx.codex.tmux.clone();
    fx.append(&opening(&turn(80), prompt));
    let mut events = events_for(fx, prompt, Duration::from_secs(5)).await;
    assert_eq!(events.len(), 1);
    let pause = Arc::new(
        crate::services::discord::turn_bridge::TerminalPrepareTestHook {
            channel_id: fx.codex.channel.get(),
            before_admission: true,
            ..Default::default()
        },
    );
    *crate::services::discord::turn_bridge::TERMINAL_PREPARE_TEST_HOOK
        .lock()
        .unwrap() = Some(pause.clone());
    super::super::super::super::relay_observed_prompt(&fx.shared, events.remove(0)).await;
    fx.append(&terminal_only(&turn(80), answer));
    tokio::time::timeout(Duration::from_secs(5), pause.entered.notified())
        .await
        .expect("actual adapter admission pause");
    assert!(
        fx.sent(answer).is_empty(),
        "terminal-only Q cannot publish before admission"
    );
    let previous = dedupe::runtime_binding_for_tmux_session(&tmux).unwrap();
    let lease = fx.lease();
    assert!(lease.is_some());
    let next = root.join("direct-pending-rotation.jsonl");
    std::fs::write(
        &next,
        rollout_line(
            serde_json::json!({"type":"session_meta", "payload":{"id":"direct-rotation-6903"}}),
        ),
    )
    .unwrap();
    crate::services::codex_tui::session::write_codex_tui_rollout_marker(
        &tmux,
        &next,
        Some("direct-rotation-6903"),
    )
    .unwrap();
    assert!(
        super::super::super::super::rehydration::rehydrate_codex_tui_binding_for_tests(
            &tmux,
            fx.codex.channel.get()
        )
        .is_none(),
        "unfinished direct lease must block source replacement"
    );
    assert_eq!(
        dedupe::runtime_binding_for_tmux_session(&tmux).unwrap(),
        previous
    );
    assert_eq!(fx.lease(), lease);
    pause.release.notify_one();
    assert!(fx.delivered(answer).await);
    assert!(
        wait_for(Duration::from_secs(5), || native_receipts(
            fx.codex.channel.get()
        )
        .iter()
        .any(|r| r.source.range.1 > previous.last_offset))
        .await
    );
    assert!(wait_for(Duration::from_secs(5), || fx.lease().is_none()).await);
    *crate::services::discord::turn_bridge::TERMINAL_PREPARE_TEST_HOOK
        .lock()
        .unwrap() = None;
    fx.polls(3).await;
    let after_ack =
        crate::services::codex_tui::session::read_codex_tui_rollout_marker(&tmux).unwrap();
    assert_eq!(
        after_ack.rollout_path.display().to_string(),
        previous.output_path,
        "the current source checkpoints its ACK before a successor is published"
    );
    // Reoffer the successor after the current source has finished its checkpoint.
    crate::services::codex_tui::session::write_codex_tui_rollout_marker(
        &tmux,
        &next,
        Some("direct-rotation-6903"),
    )
    .unwrap();
    let fresh = super::super::super::super::rehydration::rehydrate_codex_tui_binding_for_tests(
        &tmux,
        fx.codex.channel.get(),
    )
    .expect("ACK and receipt release source replacement");
    assert_eq!(fresh.output_path, next.display().to_string());
    fx.codex.rollout = next;
    deliver_direct_terminal(fx, 81, true).await;
    fx.polls(3).await;
    assert_eq!(fx.sent(answer).len(), 1);
    fx.assert_answer(answer, fx.anchor_of(answer));
}

#[test]
fn codex_6903_parked_completion_survives_receipt_limit_rotation() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::ReceiptLimit),
        ))
    });
}

#[test]
fn codex_6903_failed_receipt_keeps_parked_custody_unsettled() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::ReceiptFailure),
        ))
    });
}

#[test]
fn codex_6903_settled_custody_still_protects_pending_direct_lease() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::DirectPending),
        ))
    });
}

#[test]
fn codex_6903_custody_free_restore_protects_pending_direct_lease() {
    run(|root| {
        Box::pin(async move {
            let _tmux = live_pane_tmux(&root);
            let mut fx =
                Fixture::start(&root, 6_903_991, "AgentDesk-codex-6903-direct-pending").await;
            dedupe::reset_state_for_tests();
            let binding =
                super::super::super::super::codex_tui_rehydrated_binding_from_rollout_path(
                    &fx.codex.tmux,
                    &fx.codex.rollout,
                    Some("s-5704".into()),
                )
                .unwrap();
            dedupe::register_tmux_runtime_binding(&fx.codex.tmux, binding);
            direct_pending_rotation(&root, &mut fx).await;
            fx.finish();
        })
    });
}

/// A G1 exact delivery that commits as historical after the wrapper publishes G2 still
/// ends the G1 parked custody, so the G2 source restores and keeps relaying.
async fn generation_switch_settles_parked(
    root: &Path,
    fx: &mut Fixture,
    row: &inflight::InflightTurnState,
    g1: i64,
) {
    use crate::services::discord::outbound::delivery_record as dr;
    let tmux = fx.codex.tmux.clone();
    let channel = fx.codex.channel.get();
    assert!(fx.delivered(STEER_ANSWER).await);
    assert!(
        wait_for(Duration::from_secs(5), || !native_receipts(channel)
            .is_empty())
        .await,
        "the G1 exact delivery must still record its receipt"
    );
    let receipts = native_receipts(channel);
    assert_eq!(receipts.len(), 1);
    let parked = receipts[0].clone();
    let g2 = dr::current_generation_mtime_ns(&tmux);
    assert_ne!(g2, g1, "the wrapper must publish G2 before the commit");
    assert_eq!(parked.source.generation_mtime_ns, g1);
    assert_eq!(
        parked.source.range,
        (row.turn_start_offset.unwrap(), row.last_offset)
    );
    assert!(dr::historical_pinned_delivery_exists(
        &parked.source,
        parked.message_id
    ));
    let record = dr::read_record(&ProviderKind::Codex, row.delivery_record_owner_channel_id());
    assert!(
        record
            .and_then(|record| record.delivered_frontier)
            .is_none_or(|frontier| frontier.generation_mtime_ns != g1),
        "a G1 delivery after the switch is historical, never the current frontier"
    );
    // No restore pass runs here; only the delivery's own completion can settle G1.
    assert!(
        wait_for(Duration::from_secs(5), || {
            dedupe::codex_restore_settled_for_tests(&tmux) == Some(1)
        })
        .await,
        "the historical receipt must settle the G1 parked owner on commit"
    );
    *crate::services::discord::turn_bridge::TERMINAL_PREPARE_TEST_HOOK
        .lock()
        .unwrap() = None;
    assert!(
        wait_for(Duration::from_secs(5), || fx.lease().is_none()).await,
        "the parked relay lease must release"
    );
    let next = root.join("generation-switch.jsonl");
    std::fs::write(
        &next,
        rollout_line(serde_json::json!({"type":"session_meta", "payload":{"id":"g2-6903"}})),
    )
    .unwrap();
    crate::services::codex_tui::session::write_codex_tui_rollout_marker(
        &tmux,
        &next,
        Some("g2-6903"),
    )
    .unwrap();
    let fresh = super::super::super::super::rehydration::rehydrate_codex_tui_binding_for_tests(
        &tmux, channel,
    )
    .expect("settled G1 custody must release the G2 marker source");
    assert_eq!(fresh.output_path, next.display().to_string());
    assert_eq!(fresh.session_id.as_deref(), Some("g2-6903"));
    fx.codex.rollout = next;
    for n in 100..132 {
        deliver_direct_terminal(fx, n, false).await;
    }
    let receipts = native_receipts(channel);
    assert_eq!(receipts.len(), 32);
    assert!(
        !receipts
            .iter()
            .any(|receipt| receipt.source == parked.source),
        "the G1 receipt must actually be drained"
    );
    deliver_direct_terminal(fx, 132, true).await;
    fx.polls(3).await;
    assert!(
        native_receipts(channel)
            .iter()
            .all(|receipt| receipt.source.generation_mtime_ns == g2)
    );
    let frontier = dr::read_record(&ProviderKind::Codex, row.delivery_record_owner_channel_id())
        .and_then(|record| record.delivered_frontier)
        .unwrap();
    assert_eq!(frontier.generation_mtime_ns, g2);
    assert_eq!(fx.sent(STEER_ANSWER).len(), 1, "P is never republished");
}

#[test]
fn codex_6903_generation_switch_historical_receipt_settles_parked_owner() {
    run(|root| {
        Box::pin(restored_historical_answer(
            root,
            true,
            Some(ParkedRace::GenerationSwitch),
        ))
    });
}
