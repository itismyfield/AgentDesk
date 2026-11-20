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

pub(in super::super) async fn claimed(
    fixture: &Fixture,
) -> (
    OwnerTurn,
    AdmissionTicket,
    tokio::sync::oneshot::Receiver<Result<(), String>>,
) {
    let mailbox = fixture.shared.mailbox(ChannelId::new(CHANNEL));
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude/input-owner-reattach");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{CHANNEL}.json"));
    mailbox.v2_adopt(path, "input-owner".into()).await.unwrap();
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
    assert_eq!(front.0.as_deref(), Some(ticket.as_str()));
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
    (owner, ticket, answer)
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
                let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
                let handoff: Handoff = Arc::new(move |front| {
                    let send = send.clone();
                    Box::pin(async move {
                        send.send(front).map_err(|error| error.to_string())?;
                        Err("test leaves the granted front in custody".into())
                    })
                });
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
                assert!(fixture.rig.sends().is_empty());
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
