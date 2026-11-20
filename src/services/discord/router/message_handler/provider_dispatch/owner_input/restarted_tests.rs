use super::*;
use crate::services::discord::queue_io::input_owner::admission::{
    Admission, AdmissionTicket, TurnBind, admit, exact_turn,
};
use crate::services::discord::recovery_engine::herdr_reattach::tests::{CHANNEL, Fixture, run};
use crate::services::discord::router::message_handler::intake_turn::session_preparation;
use crate::services::session_host::herdr_socket_rig_tests::SHELL;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const ORIGINAL: u64 = 8_000_000_000_008_280;
const FOLLOWER: u64 = ORIGINAL + 1;

pub(super) fn omit_guard() -> bool {
    std::env::var("ADK_HERDR_OWNER_TEST_MODE").as_deref() == Ok("omit-first-key")
}

fn item(message: u64, text: &str) -> PendingQueueItem {
    serde_json::from_value(json!({
        "author_id":7, "message_id":message, "channel_id":CHANNEL, "merge_consecutive":true,
        "source_message_ids":[message], "queued_generation":1, "text":text,
    }))
    .unwrap()
}

fn queue_path() -> std::path::PathBuf {
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude/input-owner-reattach");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{CHANNEL}.json"))
}

pub(in super::super) async fn claimed(
    fixture: &Fixture,
) -> (
    OwnerTurn,
    AdmissionTicket,
    tokio::sync::oneshot::Receiver<Result<(), String>>,
) {
    let mailbox = fixture.shared.mailbox(ChannelId::new(CHANNEL));
    mailbox
        .v2_adopt(queue_path(), "input-owner".into())
        .await
        .unwrap();
    let original = item(ORIGINAL, "original");
    let ticket = AdmissionTicket::new();
    for _ in 0..2 {
        assert_eq!(
            admit(&mailbox, &original, ticket.clone()).await,
            Admission::Accepted(ticket.clone())
        );
    }
    let follower = AdmissionTicket::new();
    assert_eq!(
        admit(&mailbox, &item(FOLLOWER, "follower"), follower.clone()).await,
        Admission::Accepted(follower),
    );
    let (owner, answer) = claim_existing(fixture, mailbox).await;
    assert_eq!(owner.front().0.as_deref(), Some(ticket.as_str()));
    (owner, ticket, answer)
}

async fn claim_existing(
    fixture: &Fixture,
    mailbox: ChannelMailboxHandle,
) -> (
    OwnerTurn,
    tokio::sync::oneshot::Receiver<Result<(), String>>,
) {
    let front = mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().unwrap();
            let front = owner.front().unwrap();
            owner.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap();
    let claim = claim_turn(&mailbox, ORIGINAL, &front)
        .await
        .unwrap()
        .unwrap();
    let (settled, answer) = tokio::sync::oneshot::channel();
    let owner = OwnerTurn {
        mailbox,
        runtime: Some(fixture.shared.clone()),
        claim,
        settled,
        origin: InputOrigin::Discord,
        late: LateRead::default(),
        recovery: Default::default(),
        refusal: Arc::default(),
    };
    (owner, answer)
}

async fn prepare(fixture: &Fixture, owner: OwnerTurn) -> Result<OwnerTurn, ()> {
    let channel = ChannelId::new(CHANNEL);
    let lease = SessionTransitionLease::try_acquire(&fixture.shared, channel).unwrap();
    let before = std::env::var("ADK_HERDR_OWNER_TEST_MODE").as_deref() == Ok("before");
    let route = (
        &fixture.shared.provider,
        channel,
        Some(fixture.logical()),
        Some(fixture.logical()),
    );
    session_preparation::prepare_owner(
        &fixture.shared,
        route,
        Some(owner),
        (true, (!before).then_some(&lease)),
    )
    .await
    .map(Option::unwrap)
}

fn payload(fixture: &Fixture) -> PreparedPayload {
    let record = &fixture.record;
    let location = record.location.as_ref().unwrap();
    PreparedPayload {
        frame: "original\nfollower".into(),
        sources: vec![ORIGINAL.to_string(), FOLLOWER.to_string()],
        attempt: json!({
            "nonce":"restart-owned-attempt", "provider":"claude",
            "target":{"host":"herdr", "owner":record.owner.logical_key,
                "root":location.socket_addr, "execution_nonce":record.execution_nonce},
            "source":{"kind":"pre_source", "expected_session_id":null,
                "expected_transcript":null, "binding_watermark":null}
        }),
    }
}

#[test]
fn early_held_wakes_the_original_grant_pg() {
    run(
        ProviderKind::Claude,
        "s2-held-wake",
        false,
        |fixture, runtime| {
            fixture.rig.foreground(&[SHELL]);
            runtime.block_on(async {
                let (owner, ticket, answer) = claimed(&fixture).await;
                let mailbox = owner.mailbox.clone();
                assert!(prepare(&fixture, owner).await.is_err());
                assert_eq!(
                    answer.await.unwrap(),
                    Err("NotSent(\"herdr_restart_held\")".into())
                );
                assert!(held_watch::armed(CHANNEL));
                let after_deadline =
                    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap() + 20_000;
                let retained = mailbox
                    .v2_request(move |slot| {
                        let owner = slot.as_mut().unwrap();
                        owner.tick(after_deadline)?;
                        owner.json()
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    retained["items"],
                    json!([item(ORIGINAL, "original"), item(FOLLOWER, "follower")]),
                    "identity wait must preserve both inputs past the composer deadline"
                );
                assert!(retained["refused_fronts"].is_null(), "{retained}");
                assert!(retained["head_injection"].is_null(), "{retained}");
                assert!(matches!(
                    exact_turn(&mailbox, &ticket, &item(ORIGINAL, "original")).await,
                    Ok(TurnBind::NotArmed)
                ));

                let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
                let prepared = payload(&fixture);
                let leaf_runs = Arc::new(AtomicUsize::new(0));
                let counted = leaf_runs.clone();
                let holder = mailbox.clone();
                let handoff: Handoff = Arc::new(move |front| {
                    let (send, holder, counted) = (send.clone(), holder.clone(), counted.clone());
                    let prepared = PreparedPayload {
                        frame: prepared.frame.clone(),
                        sources: prepared.sources.clone(),
                        attempt: prepared.attempt.clone(),
                    };
                    Box::pin(async move {
                        let claim = claim_turn(&holder, ORIGINAL, &front)
                            .await?
                            .ok_or("no original claim")?;
                        let result = pump::submit_front(
                            &holder,
                            Some(claim),
                            prepared,
                            clock(),
                            Box::new(move |frame| {
                                assert_eq!(frame, "original\nfollower");
                                counted.fetch_add(1, Ordering::SeqCst);
                                Ok(Completion::Submitted)
                            }),
                        )
                        .await?;
                        assert_eq!(result, Completion::Submitted);
                        send.send(front).map_err(|error| error.to_string())
                    })
                });
                let drive_mailbox = mailbox.clone();
                let drive = Arc::new(Drive {
                    mailbox,
                    channel: CHANNEL,
                    handoff,
                    settled: None,
                });
                fixture
                    .rig
                    .foreground(&[crate::services::session_host::herdr_socket_rig_tests::PROVIDER]);
                let (_, observed) = held_watch::watch_observed(drive, ORIGINAL);
                let observed = tokio::time::timeout(Duration::from_secs(8), observed)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(observed.verdict, held_watch::Verdict::Wake);
                let front = tokio::time::timeout(Duration::from_secs(5), receive.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(front.0.as_deref(), Some(ticket.as_str()));
                assert_eq!(
                    (front.1.message_id, front.1.queued_generation),
                    (ORIGINAL, 1)
                );
                assert_eq!(leaf_runs.load(Ordering::SeqCst), 1);
                assert!(matches!(
                    exact_turn(&drive_mailbox, &ticket, &item(ORIGINAL, "original")).await,
                    Ok(TurnBind::Settled(_))
                ));
                assert!(fixture.rig.sends().is_empty());
            });
            runtime.block_on(fixture.close());
        },
    );
}

#[test]
fn identity_hold_outlives_the_restored_composer_deadline_pg() {
    run(
        ProviderKind::Claude,
        "s2-restored-due",
        false,
        |fixture, runtime| {
            fixture.rig.foreground(&[SHELL]);
            runtime.block_on(async {
                let (owner, ticket, _) = claimed(&fixture).await;
                let mailbox = owner.mailbox.clone();
                let front = owner.front().clone();
                let now = chrono::Utc::now().timestamp_millis() as u64;
                let due = mailbox
                    .v2_request(move |slot| {
                        let owner = slot.as_mut().unwrap();
                        let held = owner.hold_readiness(&front, "busy_readiness_unproven", now)?;
                        Ok(held.due_at_ms)
                    })
                    .await
                    .unwrap();
                drop(owner);
                mailbox
                    .v2_adopt(queue_path(), "input-owner".into())
                    .await
                    .unwrap();
                assert_eq!(
                    mailbox
                        .v2_request(|slot| Ok(slot.as_ref().unwrap().settle_due()))
                        .await
                        .unwrap(),
                    Some(due)
                );
                mailbox
                    .v2_request(move |slot| slot.as_mut().unwrap().tick(due - 1))
                    .await
                    .unwrap();
                let (owner, answer) = claim_existing(&fixture, mailbox.clone()).await;
                assert!(prepare(&fixture, owner).await.is_err());
                assert_eq!(
                    answer.await.unwrap(),
                    Err("NotSent(\"herdr_restart_held\")".into())
                );
                assert!(held_watch::armed(CHANNEL));

                // Re-adopt again while identity is still unknown. An already armed timer can fire.
                mailbox
                    .v2_adopt(queue_path(), "input-owner".into())
                    .await
                    .unwrap();
                let (retained, scheduled) = mailbox
                    .v2_request(move |slot| {
                        let owner = slot.as_mut().unwrap();
                        let scheduled = owner.settle_due();
                        owner.tick(due + 1)?;
                        Ok((owner.json()?, scheduled))
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    retained["items"],
                    json!([item(ORIGINAL, "original"), item(FOLLOWER, "follower")]),
                    "the restored composer deadline must not delete the identity-held unit"
                );
                assert_eq!(scheduled, None);
                assert!(retained["refused_fronts"].is_null(), "{retained}");
                assert!(retained["head_injection"].is_null(), "{retained}");
                assert_eq!(retained["readiness_holds"][0]["due_at_ms"], due);
                let original = item(ORIGINAL, "original");
                assert!(matches!(
                    exact_turn(&mailbox, &ticket, &original).await,
                    Ok(TurnBind::NotArmed)
                ));

                fixture
                    .rig
                    .foreground(&[crate::services::session_host::herdr_socket_rig_tests::PROVIDER]);
                let (offer, mut receive) = tokio::sync::mpsc::unbounded_channel();
                let handoff: Handoff = Arc::new(move |front| {
                    let offered = offer.send(front);
                    Box::pin(async move {
                        offered.map_err(|error| error.to_string())?;
                        Err("the test drives this front to submission".into())
                    })
                });
                let drive = Arc::new(Drive {
                    mailbox: mailbox.clone(),
                    channel: CHANNEL,
                    handoff,
                    settled: None,
                });
                let (_, observed) = held_watch::watch_observed(drive, ORIGINAL);
                let observed = tokio::time::timeout(Duration::from_secs(8), observed)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(observed.verdict, held_watch::Verdict::Wake);
                let offered = tokio::time::timeout(Duration::from_secs(5), receive.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(offered.0.as_deref(), Some(ticket.as_str()));
                assert_eq!(offered.1.message_id, ORIGINAL);
                let (owner, _) = claim_existing(&fixture, mailbox.clone()).await;
                let mut owner = prepare(&fixture, owner)
                    .await
                    .ok()
                    .expect("reattached owner");
                assert_eq!(owner.front().0.as_deref(), Some(ticket.as_str()));
                let existing = owner.late.restarted.take().expect("verified owner binding");
                let prepared = payload(&fixture);
                let writes = Arc::new(AtomicUsize::new(0));
                let counted = writes.clone();
                let leaf = guard(
                    Some(existing),
                    &prepared.attempt,
                    Box::new(move |frame| {
                        assert_eq!(frame, "original\nfollower");
                        counted.fetch_add(1, Ordering::SeqCst);
                        Ok(Completion::Submitted)
                    }),
                );
                let after_due: Clock = Arc::new(move || due + 1);
                assert!(matches!(
                    pump::submit_front(&mailbox, Some(owner.claim), prepared, after_due, leaf)
                        .await,
                    Ok(Completion::Submitted)
                ));
                assert_eq!(writes.load(Ordering::SeqCst), 1);
                assert!(matches!(
                    exact_turn(&mailbox, &ticket, &original).await,
                    Ok(TurnBind::Settled(_))
                ));
            });
            runtime.block_on(fixture.close());
        },
    );
}

fn native_modal_after_identity_hold(adopt_after_attach: bool, tag: &str) {
    use crate::services::discord::recovery_engine::herdr_reader::reconnect_restarted_herdr_with_shared;
    use crate::services::discord::tui_prompt_relay::synthetic_start::claim::claim_tui_direct_synthetic_turn_for_tests;
    use crate::services::tui_prompt_dedupe::{
        self as dedupe, ExternalInputRelayLease, ExternalInputRelayOwner,
    };
    use poise::serenity_prelude::MessageId;
    use std::io::Write;
    use std::path::Path;

    run(ProviderKind::Claude, tag, false, |mut fixture, runtime| {
        let generation = 1;
        let path = crate::services::discord::runtime_store::generation_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, generation.to_string()).unwrap();
        Arc::get_mut(&mut fixture.shared)
            .unwrap()
            .restart
            .current_generation = generation;
        fixture.rig.foreground(&[SHELL]);
        let (mailbox, ticket, old_due) = runtime.block_on(async {
            let (owner, ticket, _) = claimed(&fixture).await;
            let mailbox = owner.mailbox.clone();
            let front = owner.front().clone();
            let old_due = mailbox
                .v2_request(move |slot| {
                    let owner = slot.as_mut().unwrap();
                    let held = owner.hold_readiness(
                        &front,
                        "busy_readiness_unproven",
                        chrono::Utc::now().timestamp_millis().max(0) as u64,
                    )?;
                    Ok(held.due_at_ms)
                })
                .await
                .unwrap();
            drop(owner);
            mailbox
                .v2_adopt(queue_path(), "input-owner".into())
                .await
                .unwrap();
            let (owner, answer) = claim_existing(&fixture, mailbox.clone()).await;
            assert!(prepare(&fixture, owner).await.is_err());
            assert_eq!(
                answer.await.unwrap(),
                Err("NotSent(\"herdr_restart_held\")".into())
            );
            assert_eq!(mailbox.has_active_turn().await, Ok(false));
            assert!(held_watch::armed(CHANNEL));
            if adopt_after_attach {
                mailbox
                    .v2_request(|slot| {
                        slot.take();
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
            (mailbox, ticket, old_due)
        });
        fixture
            .rig
            .foreground(&[crate::services::session_host::herdr_socket_rig_tests::PROVIDER]);
        {
            let entered = runtime.enter();
            reconnect_restarted_herdr_with_shared(&fixture.shared, &ProviderKind::Claude);
            drop(entered);
        }
        assert_eq!(
            fixture
                .shared
                .tmux_watchers
                .owner_channel_for_tmux_session(fixture.logical()),
            Some(ChannelId::new(CHANNEL)),
            "separate recovery published the original execution"
        );

        runtime.block_on(async {
            if adopt_after_attach {
                mailbox.v2_adopt(queue_path(), "input-owner".into()).await.unwrap();
            }
            let attached_snapshot = mailbox.v2_request(|slot| slot.as_ref().unwrap().json()).await.unwrap();
            let channel = ChannelId::new(CHANNEL);
            let binding = dedupe::runtime_binding_for_tmux_session(fixture.logical()).unwrap();
            let path = Path::new(&binding.output_path);
            let session = binding.session_id.as_deref();
            let body = "native B after background attach";
            let mut transcript = std::fs::OpenOptions::new().append(true).open(path).unwrap();
            writeln!(transcript, "{}", json!({"type":"user","sessionId":session,
                "uuid":uuid::Uuid::new_v4(),"timestamp":chrono::Utc::now().to_rfc3339(),
                "message":{"role":"user","content":body}})).unwrap();
            let mut lease = ExternalInputRelayLease::unassigned(Some(CHANNEL));
            lease.turn_id = Some("native-b-after-reattach".into());
            lease.session_key = Some(fixture.logical().to_owned());
            lease.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
            lease.relay_owner = ExternalInputRelayOwner::TmuxWatcher;
            lease.hosted_source = Some(dedupe::hosted_source::capture(
                "claude", fixture.logical(), CHANNEL, path, session,
            ).expect("native B carries the verified source"));
            let lease = dedupe::record_external_input_turn_lease("claude", fixture.logical(), lease);
            assert!(claim_tui_direct_synthetic_turn_for_tests(
                &fixture.shared, &ProviderKind::Claude, channel, fixture.logical(), body,
                MessageId::new(FOLLOWER + 1), &lease,
            ).await, "the production synthetic claim starts native B");
            assert_eq!(mailbox.has_active_turn().await, Ok(true));
            let live = crate::services::discord::inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL).unwrap();
            assert_eq!(live.born_generation, generation);
            let modal = "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to select · Esc to cancel";
            assert!(crate::services::claude_tui::busy_inject::modal(modal));
            fixture.rig.answer("pane.read", json!({"type":"pane_read","read":{
                "pane_id":fixture.record.location.as_ref().unwrap().pane_id, "source":"recent_unwrapped",
                "text":modal,"revision":7,"truncated":false
            }}));

            let previous = OwnerTurn::drive_results(CHANNEL).len();
            let idle: Handoff = Arc::new(|_| Box::pin(async { panic!("native B owns the turn") }));
            let drive = owner_drive(
                (mailbox.clone(), fixture.shared.pg_pool.clone()),
                (ProviderKind::Claude, CHANNEL),
                idle,
                Some(fixture.shared.clone()),
            );
            let (_, observed) = held_watch::watch_observed(drive, ORIGINAL);
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(8), observed).await.unwrap().unwrap().verdict,
                held_watch::Verdict::Wake
            );
            let result = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let results = OwnerTurn::drive_results(CHANNEL);
                    if let Some(result) = results.get(previous) {
                        break result.clone();
                    }
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
            assert!(result.contains("busy_source_unavailable"), "{result}");
            let (due, snapshot) = mailbox.v2_request(|slot| {
                let owner = slot.as_ref().unwrap();
                Ok((owner.settle_due(), owner.json()?))
            }).await.unwrap();
            let due = due.expect("the new composer refusal has an active due");
            if adopt_after_attach {
                assert!(attached_snapshot["readiness_holds"][0]["identity_held_at_ms"].is_u64());
            } else {
                assert!(
                    attached_snapshot["readiness_holds"].is_null(),
                    "background confirmation still held the old deadline: {}",
                    attached_snapshot["readiness_holds"],
                );
            }
            assert!(due > old_due);
            assert!(snapshot["readiness_holds"][0]["identity_held_at_ms"].is_null());
            assert!(snapshot["head_injection"].is_null());
            mailbox.v2_request(move |slot| {
                let owner = slot.as_mut().unwrap();
                owner.tick(old_due + 1)?;
                assert_eq!(
                    owner.json()?["items"],
                    json!([item(ORIGINAL, "original"), item(FOLLOWER, "follower")]),
                );
                Ok(())
            }).await.unwrap();
            assert!(matches!(exact_turn(&mailbox, &ticket, &item(ORIGINAL, "original")).await, Ok(TurnBind::NotArmed)));
            mailbox.v2_request(move |slot| {
                let owner = slot.as_mut().unwrap();
                owner.tick(due)?;
                owner.tick(due + 1)?;
                let state = owner.json()?;
                assert_eq!(state["items"], json!([]));
                let refusals = state["refused_fronts"].as_array().unwrap();
                assert_eq!(refusals.len(), 1);
                assert_eq!(refusals[0]["members"].as_array().unwrap().len(), 2);
                Ok(())
            }).await.unwrap();
            assert!(fixture.rig.sends().is_empty());
        });
        runtime.block_on(fixture.close());
    });
}

#[test]
fn native_busy_after_background_reattach_gets_a_new_composer_due_pg() {
    native_modal_after_identity_hold(false, "native-modal-after-attach");
}

#[test]
fn native_busy_after_queue_adoption_gets_a_new_composer_due_pg() {
    native_modal_after_identity_hold(true, "native-modal-after-adopt");
}

#[test]
fn stop_leaves_identity_held_inputs_and_proven_end_releases_their_pause_pg() {
    use crate::services::discord::commands::stop::run_slash_stop;
    use crate::services::session_host::herdr_socket_rig_tests::PROVIDER;
    use poise::serenity_prelude::MessageId;

    run(
        ProviderKind::Claude,
        "identity-stop-then-end",
        false,
        |fixture, runtime| {
            fixture.rig.foreground(&[SHELL]);
            runtime.block_on(async {
                let (owner, _, _) = claimed(&fixture).await;
                let mailbox = owner.mailbox.clone();
                let front = owner.front().clone();
                let due = mailbox
                    .v2_request(move |slot| {
                        let owner = slot.as_mut().unwrap();
                        Ok(owner
                            .hold_readiness(
                                &front,
                                "busy_readiness_unproven",
                                chrono::Utc::now().timestamp_millis().max(0) as u64,
                            )?
                            .due_at_ms)
                    })
                    .await
                    .unwrap();
                assert!(prepare(&fixture, owner).await.is_err());
                let channel = ChannelId::new(CHANNEL);
                let reply = run_slash_stop(&fixture.shared, &ProviderKind::Claude, channel).await;
                reply
                    .finish(&fixture.shared, &ProviderKind::Claude, channel)
                    .await;
                mailbox
                    .v2_request(move |slot| {
                        let owner = slot.as_mut().unwrap();
                        owner.tick(due + 1)?;
                        let snapshot = owner.json()?;
                        assert_eq!(
                            snapshot["items"],
                            json!([item(ORIGINAL, "original"), item(FOLLOWER, "follower")])
                        );
                        assert!(snapshot["readiness_holds"][0]["identity_held_at_ms"].is_u64());
                        Ok(())
                    })
                    .await
                    .unwrap();
                fixture.rig.exists(PROVIDER, false);
                let (owner, _) = claim_existing(&fixture, mailbox.clone()).await;
                let owner = prepare(&fixture, owner).await.ok().expect("proved ended");
                assert!(!owner.continues_herdr());
                let front = owner.front().clone();
                mailbox
                    .v2_request(move |slot| {
                        let owner = slot.as_mut().unwrap();
                        assert!(owner.json()?["readiness_holds"].is_null());
                        owner.hold_readiness(
                            &front,
                            "busy_readiness_unproven",
                            chrono::Utc::now().timestamp_millis().max(0) as u64,
                        )?;
                        owner.hold_identity(&front)
                    })
                    .await
                    .unwrap();
                drop(owner);
                let persistence = crate::services::discord::queue_persistence_context(
                    &fixture.shared,
                    &ProviderKind::Claude,
                    channel,
                );
                let removed = mailbox
                    .cancel_queued_primary_message(MessageId::new(ORIGINAL), persistence)
                    .await;
                assert!(removed.removed.is_some());
                mailbox
                    .v2_request(move |slot| {
                        let owner = slot.as_mut().unwrap();
                        let snapshot = owner.json()?;
                        assert_eq!(snapshot["items"], json!([item(FOLLOWER, "follower")]));
                        assert_eq!(owner.settle_due(), None);
                        let next = owner.front().unwrap();
                        owner.hold_readiness(
                            &next,
                            "busy_readiness_unproven",
                            chrono::Utc::now().timestamp_millis().max(0) as u64,
                        )?;
                        owner.hold_identity(&next)?;
                        owner.clear_waiting()?;
                        owner.tick(due + 2)?;
                        let clear = owner.json()?;
                        assert_eq!(clear["items"], json!([]));
                        assert!(clear["readiness_holds"].is_null());
                        Ok(())
                    })
                    .await
                    .unwrap();
            });
            runtime.block_on(fixture.close());
        },
    );
}

fn pre_key_control_case(kind: &'static str) {
    run(ProviderKind::Claude, kind, false, |fixture, runtime| {
        runtime.block_on(async {
            let (owner, ticket, _) = claimed(&fixture).await;
            let mut owner = prepare(&fixture, owner).await.ok().expect("existing owner");
            let mut existing = owner.late.restarted.take().expect("verified owner binding");
            let cancel = Arc::new(CancelToken::new());
            existing.0.cancel = Some(cancel.clone());
            let prepared = payload(&fixture);
            let calls = Arc::new(AtomicUsize::new(0));
            let counted = calls.clone();
            let leaf = guard(
                Some(existing),
                &prepared.attempt,
                Box::new(move |_| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    Ok(Completion::Submitted)
                }),
            );
            let rig = fixture.rig.clone();
            let token = fixture.record.owner.discord_token_hash.clone();
            let interrupt: effect::Leaf = Box::new(move |frame| {
                match kind {
                    "s2-death" => rig.show_panes(&[]),
                    "s2-stop" => {
                        cancel.native_stop.receipt_sink().arm_first_key();
                        assert_eq!(
                            cancel.native_stop.receipt_sink().stop_before_key(),
                            Some(true)
                        );
                    }
                    "s2-reset" => {
                        drop(clear_epoch::begin_clear("claude", &token, CHANNEL));
                    }
                    _ => unreachable!(),
                }

                leaf(frame)
            });
            let done = pump::submit_front(
                &owner.mailbox,
                Some(owner.claim),
                prepared,
                clock(),
                interrupt,
            )
            .await;
            assert!(matches!(done, Ok(Completion::NotSent(_))), "{done:?}");
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(fixture.rig.sends().is_empty());
            assert!(matches!(
                exact_turn(&owner.mailbox, &ticket, &item(ORIGINAL, "original")).await,
                Ok(TurnBind::NotArmed),
            ));
        });
        runtime.block_on(fixture.close());
    });
}

fn clock() -> Clock {
    Arc::new(|| 10_000)
}

#[test]
fn death_after_reserve_candidate_keeps_the_original_unsent_pg() {
    pre_key_control_case("s2-death");
}

#[test]
fn stop_after_reserve_candidate_keeps_the_original_unsent_pg() {
    pre_key_control_case("s2-stop");
}

#[test]
fn reset_after_reserve_candidate_keeps_the_original_unsent_pg() {
    pre_key_control_case("s2-reset");
}

fn receipt_case(uncertain: bool, tag: &str) {
    run(ProviderKind::Claude, tag, uncertain, |fixture, runtime| {
        runtime.block_on(async {
            let (owner, ticket, _) = claimed(&fixture).await;
            let mut owner = prepare(&fixture, owner).await.ok().expect("existing owner");
            assert_eq!(
                owner
                    .claim
                    .followers()
                    .iter()
                    .map(|f| f.message_id)
                    .collect::<Vec<_>>(),
                [FOLLOWER]
            );
            let existing = owner.late.restarted.take().expect("verified owner binding");
            let prepared = payload(&fixture);
            let calls = Arc::new(AtomicUsize::new(0));
            let counted = calls.clone();
            let leaf = guard(
                Some(existing),
                &prepared.attempt,
                Box::new(move |frame| {
                    assert_eq!(frame, "original\nfollower");
                    counted.fetch_add(1, Ordering::SeqCst);
                    Ok(if uncertain {
                        Completion::WriteUncertain("enter reply missing".into())
                    } else {
                        Completion::Submitted
                    })
                }),
            );
            let done =
                pump::submit_front(&owner.mailbox, Some(owner.claim), prepared, clock(), leaf)
                    .await;
            assert!(
                matches!(
                    (&done, uncertain),
                    (Ok(Completion::Submitted), false) | (Ok(Completion::WriteUncertain(_)), true)
                ),
                "{done:?}"
            );
            let original = item(ORIGINAL, "original");
            assert!(matches!(
                (
                    exact_turn(&owner.mailbox, &ticket, &original).await,
                    uncertain
                ),
                (Ok(TurnBind::Armed(_)), true) | (Ok(TurnBind::Settled(_)), false)
            ));
            assert_eq!(
                admit(&owner.mailbox, &original, ticket.clone()).await,
                Admission::Accepted(ticket)
            );
            let counted = calls.clone();
            let rerun = pump::submit(
                &owner.mailbox,
                payload(&fixture),
                clock(),
                Box::new(move |_| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    Ok(Completion::Submitted)
                }),
            )
            .await;
            assert!(rerun.is_err(), "{rerun:?}");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
        assert!(fixture.rig.sends().is_empty());
        runtime.block_on(fixture.close());
    });
}

#[test]
fn idle_continuation_submits_the_frozen_original_once_pg() {
    receipt_case(false, "s2-submitted");
}

#[test]
fn busy_continuation_ack_unknown_keeps_custody_without_resend_pg() {
    receipt_case(true, "s2-unknown");
}
