//! Actual live dispatch, the existing catch-up enqueue boundary, and persisted queue restore.
//! Native provider execution remains the parent fixture's counted boundary.
use super::*;
use crate::services::discord::queue_io::cancel_backstop_test_support::KickoffCompletion;
use crate::services::discord::{mailbox_snapshot, queue_io};
use crate::services::turn_orchestrator::Intervention;

const CAP: Duration = Duration::from_secs(5);
fn channel(f: &Fixture) -> serenity::ChannelId {
    serenity::ChannelId::new(f.channel)
}

fn one_original(f: &Fixture) -> Intervention {
    let disk = f.disk();
    assert_eq!(disk.len(), 1);
    assert_eq!(disk[0].message_id.get(), f.message);
    assert_eq!(disk[0].text, "one boot original");
    assert_eq!(
        disk[0].source_message_ids,
        [serenity::MessageId::new(f.message)]
    );
    disk.into_iter().next().unwrap()
}

fn source_generations(item: &Intervention) -> Vec<serde_json::Value> {
    item.source_message_queued_generations.iter().map(|source| serde_json::json!({
        "message_id": source.message_id.get(), "queued_generation": source.queued_generation,
        "enqueued_at_epoch_us": source.enqueued_at_epoch_us, "preserve_on_cancel": source.preserve_on_cancel,
        "route": source.route, "origin": source.origin, "origin_id": source.origin_id,
    })).collect()
}

async fn one_effect(f: &Fixture, done: &KickoffCompletion, snapshot_count: usize) {
    done.post_enqueue_snapshots(1, snapshot_count, CAP).await;
    assert_eq!(
        f.effects.load(Ordering::SeqCst),
        1,
        "one provider-boundary call, rather than physical native key evidence"
    );
    assert!(
        f.disk().is_empty(),
        "the actual queue drain took the one original"
    );
}

struct ReleaseOnDrop(Arc<tokio::sync::Semaphore>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.add_permits(1);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn writer_boot_commit_after_ready_callback_rechecks_current_level_pg() {
    scenario("race_tests::writer_boot_commit_after_ready_callback_rechecks_current_level_pg",
        73_369_531, "mac-a", |f| Box::pin(async move {
            let done = KickoffCompletion::install(channel(f));
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Semaphore::new(0));
            let release_guard = ReleaseOnDrop(release.clone());
            let preflight_guard = herdr_consent::pause_boot_preflight_for_tests(entered.clone(), release.clone());
            let live = f.live(ProviderKind::Claude);
            tokio::pin!(live);
            tokio::select! {
                result = &mut live => panic!("actual live admission must pause before custody: {result:?}"),
                () = entered.notified() => {}
            }
            assert!(f.disk().is_empty());
            f.release.add_permits(1);
            // This ACK is after the ready callback's empty snapshot task finished.
            done.post_enqueue_snapshots(0, 1, CAP).await;
            assert!(host::process_readiness().accepts(f.channel));
            assert_eq!(f.effects.load(Ordering::SeqCst), 0);
            release.add_permits(1);
            live.await.unwrap();
            drop((preflight_guard, release_guard));
            one_effect(f, &done, 1).await;
            assert_eq!(f.checkpoints.load(Ordering::SeqCst), 1);
        })).await;
}

#[tokio::test(flavor = "current_thread")]
async fn writer_boot_same_canonical_live_redelivery_keeps_one_original_pg() {
    scenario(
        "race_tests::writer_boot_same_canonical_live_redelivery_keeps_one_original_pg",
        73_369_532,
        "mac-a",
        |f| {
            Box::pin(async move {
                let done = KickoffCompletion::install(channel(f));
                f.live(ProviderKind::Claude).await.unwrap();
                let first = one_original(f);
                f.live(ProviderKind::Claude).await.unwrap();
                let second = one_original(f);
                assert_eq!(source_generations(&first), source_generations(&second));
                assert_eq!(first.pending_uploads, second.pending_uploads);
                assert_eq!(f.effects.load(Ordering::SeqCst), 0);
                assert_eq!(
                    f.checkpoints.load(Ordering::SeqCst),
                    2,
                    "the same live source receives the existing durable duplicate outcome twice"
                );
                f.release.add_permits(1);
                one_effect(f, &done, 1).await;
            })
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn writer_boot_live_and_observed_enqueue_keep_one_canonical_source_pg() {
    scenario("race_tests::writer_boot_live_and_observed_enqueue_keep_one_canonical_source_pg",
        73_369_533, "mac-a", |f| Box::pin(async move {
            let done = KickoffCompletion::install(channel(f));
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Semaphore::new(0));
            let release_guard = ReleaseOnDrop(release.clone());
            let preflight_guard = herdr_consent::pause_boot_preflight_for_tests(entered.clone(), release.clone());
            let live = f.live(ProviderKind::Claude);
            tokio::pin!(live);
            tokio::select! {
                result = &mut live => panic!("live admission must wait while the observed enqueue commits: {result:?}"),
                () = entered.notified() => {}
            }
            let snapshot = mailbox_snapshot(&f.shared, channel(f)).await;
            let original = crate::services::discord::router::input_payload::owned_original(
                &f.request(), false, Vec::new(), false);
            let enqueued = queue_io::mailbox_enqueue_observed_intervention(
                &f.shared, &ProviderKind::Claude, channel(f), original,
                Some(snapshot.claim_observation)).await;
            assert!(enqueued.enqueued && enqueued.persistence_error.is_none());
            // The first generic task really saw the saved original while writer boot held it.
            done.post_enqueue_snapshots(1, 1, CAP).await;
            one_original(f);
            assert_eq!(f.effects.load(Ordering::SeqCst), 0);
            release.add_permits(1);
            live.await.unwrap();
            one_original(f);
            assert_eq!(f.checkpoints.load(Ordering::SeqCst), 1);
            f.release.add_permits(1);
            one_effect(f, &done, 2).await;
            drop((preflight_guard, release_guard));
        })).await;
}

#[tokio::test(flavor = "current_thread")]
async fn writer_boot_original_survives_new_runtime_queue_restore_pg() {
    scenario(
        "race_tests::writer_boot_original_survives_new_runtime_queue_restore_pg",
        73_369_534,
        "mac-a",
        |f| {
            Box::pin(async move {
                let done = KickoffCompletion::install(channel(f));
                f.live(ProviderKind::Claude).await.unwrap();
                let original = one_original(f);
                for handle in f.handles.drain(..) {
                    handle.stop_and_join().await.unwrap();
                }
                assert!(!host::process_readiness().is_hosted(f.channel));
                let old = f.shared.clone();
                old.mailboxes.remove_fixture_for_test(channel(f));
                let pool = old.pg_pool.as_ref().unwrap().clone();
                let (restored_registry, restored) =
                    runtime(&pool, ProviderKind::Claude, f.channel, "gateway").await;
                assert_eq!(old.token_hash, restored.token_hash);
                assert!(
                    restored
                        .http
                        .cached_serenity_ctx
                        .set(old.http.cached_serenity_ctx.get().unwrap().clone())
                        .is_ok()
                );
                let permit = execution::read(&pool, "claude", f.channel, "mac-a", || Ok(false))
                    .await
                    .unwrap()
                    .unwrap();
                let local = permit.gate();
                assert!(!Arc::ptr_eq(&local, &f.global));
                cutover::boot_snapshot()
                    .unwrap()
                    .install_execution(f.channel, Some(Ok(permit)));
                let entered = Arc::new(tokio::sync::Notify::new());
                let release = Arc::new(tokio::sync::Semaphore::new(0));
                let release_guard = ReleaseOnDrop(release.clone());
                let parts = parts_with_rest(
                    &restored,
                    &ProviderKind::Claude,
                    None,
                    Some(f.discord.http.clone()),
                );
                f.handles = host::start_managed(
                    ShadowProvider::Claude,
                    true,
                    cutover::boot_ownership(),
                    || host::HostParts {
                        io: Arc::new(Paused {
                            io: parts.io,
                            entered: entered.clone(),
                            release: release.clone(),
                            once: AtomicBool::new(false),
                        }),
                        runtime_root: parts.runtime_root,
                        gate: parts.gate,
                        readiness: host::process_readiness(),
                    },
                );
                assert_eq!(f.handles.len(), 1);
                tokio::time::timeout(CAP, entered.notified()).await.unwrap();
                assert!(!host::process_readiness().accepts(f.channel));
                let http = restored
                    .http
                    .cached_serenity_ctx
                    .get()
                    .unwrap()
                    .http
                    .clone();
                crate::services::discord::runtime_bootstrap::restore_queued_and_inflight_for_tests(
                    &http,
                    &restored,
                    &ProviderKind::Claude,
                )
                .await;
                let snapshot = mailbox_snapshot(&restored, channel(f)).await;
                assert_eq!(snapshot.intervention_queue.len(), 1);
                let actual = &snapshot.intervention_queue[0];
                assert_eq!(actual.message_id, original.message_id);
                assert_eq!(actual.text, original.text);
                assert_eq!(actual.source_message_ids, original.source_message_ids);
                assert_eq!(source_generations(actual), source_generations(&original));
                assert_eq!(actual.pending_uploads, original.pending_uploads);
                assert_eq!(f.effects.load(Ordering::SeqCst), 0);
                f.shared = restored;
                f.local = Some(local);
                f.release = release;
                f.release.add_permits(1);
                one_effect(f, &done, 1).await;
                assert_eq!(f.checkpoints.load(Ordering::SeqCst), 1);
                drop((release_guard, restored_registry));
            })
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn writer_boot_ready_normal_local_keeps_pre_effect_checkpoint_and_no_queue_pg() {
    scenario(
        "race_tests::writer_boot_ready_normal_local_keeps_pre_effect_checkpoint_and_no_queue_pg",
        73_369_535,
        "mac-a",
        |f| {
            Box::pin(async move {
                let done = KickoffCompletion::install(channel(f));
                f.release.add_permits(1);
                done.post_enqueue_snapshots(0, 1, CAP).await;
                assert_eq!(
                    for_input_placement("claude", f.channel).await,
                    IntakeRoute::Gateway
                );
                let path =
                    crate::services::discord::input_queue_path_for_test(&f.shared, f.channel);
                assert!(!path.exists());
                let (checkpoints, effects, message) =
                    (f.checkpoints.clone(), f.effects.clone(), f.message);
                let effect_override =
                    herdr_consent::override_boot_effect_for_tests(move |request| {
                        assert_eq!(request.user_msg_id.get(), message);
                        assert_eq!(
                            checkpoints.load(Ordering::SeqCst),
                            1,
                            "normal Local keeps the gate's checkpoint before the provider boundary"
                        );
                        assert!(!path.exists(), "normal Local must not gain a new queue hop");
                        effects.fetch_add(1, Ordering::SeqCst);
                        Ok(crate::services::discord::IntakeExecution::Executed)
                    });
                let checkpoint = f.checkpoints.clone();
                let deps = crate::services::discord::router::IntakeDeps {
                    http: &f.discord.http,
                    cache: None,
                    ctx_for_chained_dispatch: None,
                    shared: &f.shared,
                    token: "test-token",
                };
                queue_io::input_ingress::ADOPTS
                    .scope(
                        false,
                        herdr_consent::dispatch_boot_gate_for_tests(
                            &deps,
                            f.request(),
                            move || {
                                checkpoint.fetch_add(1, Ordering::SeqCst);
                            },
                        ),
                    )
                    .await
                    .unwrap();
                assert_eq!(f.effects.load(Ordering::SeqCst), 1);
                assert_eq!(f.checkpoints.load(Ordering::SeqCst), 1);
                assert!(f.disk().is_empty());
                drop(effect_override);
            })
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn writer_boot_v2_active_front_is_held_before_busy_branch_and_resumes_once_pg() {
    scenario(
        "race_tests::writer_boot_v2_active_front_is_held_before_busy_branch_and_resumes_once_pg",
        73_369_536,
        "mac-a",
        |f| {
            Box::pin(async move {
                let done = KickoffCompletion::install(channel(f));
                let generation =
                    crate::services::discord::runtime_store::generation_path().unwrap();
                std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
                std::fs::write(generation, "7").unwrap();
                f.shared
                    .restart
                    .reconcile_done
                    .store(true, Ordering::SeqCst);
                let old_message = serenity::MessageId::new(f.message - 1);
                let mailbox = f.shared.mailbox(channel(f));
                mailbox
                    .restore_active_turn(
                        Arc::new(crate::services::provider::CancelToken::new()),
                        serenity::UserId::new(7),
                        old_message,
                    )
                    .await;
                queue_io::input_ingress::ADOPTS
                    .scope(true, f.live(ProviderKind::Claude))
                    .await
                    .unwrap();
                assert_eq!(queue_io::input_ingress::installed(&mailbox).await, Ok(true));
                done.owner_boot_holds(f.message, 1, CAP).await;
                queue_io::input_ingress::ADOPTS
                    .scope(true, f.live(ProviderKind::Claude))
                    .await
                    .unwrap();
                done.owner_boot_holds(f.message, 2, CAP).await;
                let (snapshot, outstanding) = mailbox
                    .v2_request(|slot| {
                        let owner = slot.as_ref().ok_or("V2 missing")?;
                        Ok((owner.json()?, owner.front_outstanding()))
                    })
                    .await
                    .unwrap();
                assert!(
                    !outstanding,
                    "boot hold revoked the unattempted front's grant"
                );
                assert_eq!(snapshot["items"].as_array().unwrap().len(), 1);
                assert_eq!(snapshot["items"][0]["message_id"], f.message);
                assert_eq!(
                    snapshot["items"][0]["source_message_ids"],
                    serde_json::json!([f.message])
                );
                assert!(
                    snapshot["head_injection"].is_null(),
                    "no attempted input while host boots"
                );
                let path =
                    crate::services::discord::input_queue_path_for_test(&f.shared, f.channel);
                let disk: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
                assert_eq!(disk["version"], 2);
                assert_eq!(disk["items"][0]["message_id"], f.message);
                assert_eq!(
                    mailbox_snapshot(&f.shared, channel(f))
                        .await
                        .active_user_message_id,
                    Some(old_message)
                );
                assert_eq!(f.effects.load(Ordering::SeqCst), 0);
                assert_eq!(f.checkpoints.load(Ordering::SeqCst), 2);
                // End the unrelated active turn through the existing actor before the ready event.
                mailbox
                    .finish_turn(
                        crate::services::turn_orchestrator::QueuePersistenceContext::new(
                            &ProviderKind::Claude,
                            &f.shared.token_hash,
                            None,
                        ),
                    )
                    .await;
                f.release.add_permits(1);
                tokio::time::timeout(CAP, async {
                    loop {
                        let snapshot = mailbox
                            .v2_request(|slot| {
                                slot.as_ref()
                                    .ok_or_else(|| "V2 missing".to_string())?
                                    .json()
                            })
                            .await
                            .unwrap();
                        if snapshot["items"].as_array().unwrap().is_empty()
                            && snapshot["head_injection"].is_null()
                            && f.effects.load(Ordering::SeqCst) == 1
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("ready wakes the same accepted owner original once");
                assert_eq!(f.effects.load(Ordering::SeqCst), 1);
            })
        },
    )
    .await;
}

#[path = "boot_input_payload_tests.rs"]
mod payload_tests;
