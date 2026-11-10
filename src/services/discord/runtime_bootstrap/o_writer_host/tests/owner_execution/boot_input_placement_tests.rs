//! Writer readiness alone cannot release an input whose placement still holds it.
use super::*;
use crate::services::discord::queue_io::{
    self, cancel_backstop_test_support as wakes, input_owner::drive,
};
use crate::services::tui_o::cutover::intake_route;

const CAP: Duration = Duration::from_secs(5);

struct WakeScope(serenity::ChannelId, Option<Arc<dyn Fn() + Send + Sync>>);
impl Drop for WakeScope {
    fn drop(&mut self) {
        drop(wakes::set_owner_wake_for_tests(self.0, self.1.take()));
    }
}

struct ReleasePlacement(Arc<AtomicBool>);
impl Drop for ReleasePlacement {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn writer_ready_placement_hold_does_not_rewake_its_owner_pg() {
    placement_case(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn writer_ready_late_placement_hold_does_not_rewake_its_claimed_start_pg() {
    placement_case(true).await;
}

async fn placement_case(late: bool) {
    let (test, channel) = if late {
        (
            "boot_input_placement_tests::writer_ready_late_placement_hold_does_not_rewake_its_claimed_start_pg",
            73_369_562,
        )
    } else {
        (
            "boot_input_placement_tests::writer_ready_placement_hold_does_not_rewake_its_owner_pg",
            73_369_561,
        )
    };
    scenario(test, channel, "mac-a", move |f| {
        Box::pin(async move {
            let channel = serenity::ChannelId::new(f.channel);
            let done = wakes::KickoffCompletion::install(channel);
            f.release.add_permits(1);
            done.post_enqueue_snapshots(0, 1, CAP).await;
            assert!(host::channel_accepts(f.channel));

            let generation = crate::services::discord::runtime_store::generation_path().unwrap();
            std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
            std::fs::write(generation, "7").unwrap();
            f.shared
                .restart
                .reconcile_done
                .store(true, Ordering::SeqCst);
            assert_eq!(
                queue_io::input_ingress::ADOPTS
                    .scope(
                        true,
                        queue_io::input_ingress::adopt(&f.shared, &ProviderKind::Claude, channel),
                    )
                    .await,
                Ok(true)
            );

            // This isolates the same ready/Hold split as Codex's successor-unproved guard.
            // The late case passes the drive's first read, then holds its claimed admission.
            let eligible = Arc::new(AtomicBool::new(false));
            let release_placement = ReleasePlacement(eligible.clone());
            let reads = Arc::new(AtomicUsize::new(0));
            let observed_reads = reads.clone();
            let allowed = eligible.clone();
            let placement = intake_route::test_probe::answer_with(move |observed| {
                assert_eq!(observed, channel.get());
                let first = observed_reads.fetch_add(1, Ordering::SeqCst) == 0;
                (late && first) || allowed.load(Ordering::SeqCst)
            });

            let shared = f.shared.clone();
            let idle: drive::Handoff = Arc::new(move |front| {
                Box::pin(
                    crate::services::discord::router::message_handler::start_idle_for_test(
                        shared.clone(),
                        channel,
                        front,
                    ),
                )
            });
            let mut owner = crate::services::discord::router::message_handler::owner_drive_for_test(
                (f.shared.mailbox(channel), f.shared.pg_pool.clone()),
                (ProviderKind::Claude, channel.get()),
                idle,
                Some(f.shared.clone()),
            );
            let handoffs = Arc::new(AtomicUsize::new(0));
            let counted = handoffs.clone();
            let handoff = owner.handoff.clone();
            Arc::get_mut(&mut owner).unwrap().handoff = Arc::new(move |front| {
                counted.fetch_add(1, Ordering::SeqCst);
                handoff(front)
            });
            let (settled_send, mut settled) = tokio::sync::mpsc::unbounded_channel();
            let prior = owner.settled.clone();
            let counted = handoffs.clone();
            Arc::get_mut(&mut owner).unwrap().settled = Some(Arc::new(move || {
                let (prior, settled_send, counted) =
                    (prior.clone(), settled_send.clone(), counted.clone());
                Box::pin(async move {
                    if let Some(prior) = prior {
                        prior().await;
                    }
                    drop(settled_send.send(counted.load(Ordering::SeqCst)));
                })
            }));
            let wake_count = Arc::new(AtomicUsize::new(0));
            let counted = wake_count.clone();
            let driven = owner.clone();
            let capture = WakeScope(
                channel,
                wakes::set_owner_wake_for_tests(
                    channel,
                    Some(Arc::new(move || {
                        counted.fetch_add(1, Ordering::SeqCst);
                        drive::wake(driven.clone());
                    })),
                ),
            );

            let expected = f.request();
            let effects = f.effects.clone();
            let effect = herdr_consent::override_boot_effect_for_tests(move |request| {
                assert_eq!(request.user_msg_id, expected.user_msg_id);
                assert_eq!(request.user_text.as_bytes(), expected.user_text.as_bytes());
                effects.fetch_add(1, Ordering::SeqCst);
                Ok(crate::services::discord::IntakeExecution::Executed)
            });
            assert_eq!(
                queue_io::input_ingress::ADOPTS
                    .scope(
                        true,
                        queue_io::input_ingress::intake(
                            &f.shared,
                            &ProviderKind::Claude,
                            &f.request(),
                            Vec::new(),
                            false,
                            false,
                        ),
                    )
                    .await,
                Some(queue_io::input_ingress::Intake::Accepted)
            );
            assert_eq!(
                tokio::time::timeout(CAP, settled.recv())
                    .await
                    .expect("the actual held handoff settles"),
                Some(1)
            );
            assert!(host::channel_accepts(f.channel));
            assert_eq!(
                wake_count.load(Ordering::SeqCst),
                1,
                "one accepted wake must not schedule itself while placement still holds"
            );
            assert_eq!(handoffs.load(Ordering::SeqCst), 1);
            assert_eq!(f.effects.load(Ordering::SeqCst), 0);
            assert!(reads.load(Ordering::SeqCst) >= if late { 2 } else { 1 });
            let mailbox = f.shared.mailbox(channel);
            let kept = mailbox
                .v2_request(|slot| {
                    slot.as_ref()
                        .ok_or_else(|| "V2 missing".to_string())?
                        .json()
                })
                .await
                .unwrap();
            assert_eq!(kept["items"].as_array().unwrap().len(), 1);
            assert_eq!(kept["items"][0]["message_id"], f.message);
            assert_eq!(kept["items"][0]["text"], f.request().user_text);
            assert_eq!(kept["items"][0]["source_message_ids"], json!([f.message]));
            assert!(kept["head_injection"].is_null());
            let persisted: serde_json::Value = serde_json::from_slice(
                &std::fs::read(crate::services::discord::input_queue_path_for_test(
                    &f.shared, f.channel,
                ))
                .unwrap(),
            )
            .unwrap();
            assert_eq!(persisted["items"], kept["items"]);

            eligible.store(true, Ordering::SeqCst);
            assert_eq!(
                intake_route::for_input_placement("claude", f.channel).await,
                intake_route::IntakeRoute::Gateway
            );
            crate::services::discord::router::message_handler::owner_wake(&f.shared, channel);
            assert_eq!(
                tokio::time::timeout(CAP, settled.recv())
                    .await
                    .expect("a real eligible wake resumes the same front"),
                Some(2)
            );
            assert_eq!(wake_count.load(Ordering::SeqCst), 2);
            assert_eq!(handoffs.load(Ordering::SeqCst), 2);
            assert_eq!(f.effects.load(Ordering::SeqCst), 1);
            let finished = mailbox
                .v2_request(|slot| {
                    slot.as_ref()
                        .ok_or_else(|| "V2 missing".to_string())?
                        .json()
                })
                .await
                .unwrap();
            assert!(finished["items"].as_array().unwrap().is_empty());
            assert!(finished["head_injection"].is_null());
            drop((effect, capture, placement, release_placement));
        })
    })
    .await;
}
