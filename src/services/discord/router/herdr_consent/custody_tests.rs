use super::*;
use crate::services::discord::{
    IdleQueueKickoffChannelOutcome, make_shared_data_for_tests, queue_io,
};
use crate::services::session_host::policy::consent::settlement::tests::{
    CustodySinkFixture, custody_sink_conformance,
};
use crate::services::turn_orchestrator::{Intervention, InterventionMode, QueuePersistenceContext};
use poise::serenity_prelude::{MessageId, UserId};
use std::collections::BTreeMap;
use std::sync::Mutex;

fn consent_key() -> ConsentKey {
    ConsentKey {
        node: "fixture-node".into(),
        user: 7,
        socket: "/fixture/herdr.sock".into(),
        named_session: "desk".into(),
        config: "/fixture/config.toml".into(),
        target: "/fixture/config.toml".into(),
    }
}

async fn eligible(sink: &LiveSink, intent: &IntentRef) -> Result<bool, String> {
    let queue = sink
        .shared
        .mailbox(ChannelId::new(intent.channel))
        .try_snapshot()
        .await
        .map_err(|error| format!("fixture mailbox: {error:?}"))?;
    Ok(queue.intervention_queue.iter().any(|item| {
        item.source_message_queued_generations()
            .iter()
            .any(|source| {
                source.message_id.get() == intent.source_message
                    && source.queued_generation == intent.fifo
            })
    }) && sink.released(intent).await?)
}

fn mapped(intent: &IntentRef) -> IntentRef {
    let source = if intent.id == "intent-1" {
        891_301
    } else {
        891_302
    };
    IntentRef {
        store: "mailbox".into(),
        id: format!("{source}:17"),
        provider: "claude".into(),
        channel: source + 10,
        source_message: source,
        fifo: 17,
    }
}
fn key_for(key: &SettlementKey) -> SettlementKey {
    let intent = mapped(&IntentRef {
        store: key.store.clone(),
        id: key.id.clone(),
        provider: String::new(),
        channel: 0,
        source_message: 0,
        fifo: 0,
    });
    SettlementKey {
        nonce: key.nonce.clone(),
        store: intent.store,
        id: intent.id,
    }
}

struct Fixture {
    sink: LiveSink,
    effects: Arc<Mutex<BTreeMap<u64, (usize, usize)>>>,
    fail_wake: bool,
}
#[async_trait::async_trait]
impl CustodySink for Fixture {
    async fn release(&mut self, key: &SettlementKey, intent: &IntentRef) -> Result<(), String> {
        let intent = mapped(intent);
        let before = eligible(&self.sink, &intent).await?;
        self.sink.release(&key_for(key), &intent).await?;
        if !before && eligible(&self.sink, &intent).await? {
            self.effects
                .lock()
                .unwrap()
                .entry(intent.source_message)
                .or_default()
                .0 += 1;
        }
        Ok(())
    }
    async fn wake(&mut self, key: &SettlementKey, intent: &IntentRef) -> Result<(), String> {
        if std::mem::take(&mut self.fail_wake) {
            return Err("injected failure before existing drain".into());
        }
        self.sink.wake(&key_for(key), &mapped(intent)).await
    }
}
impl CustodySinkFixture for Fixture {
    fn effect_counts(&self, key: &SettlementKey) -> (usize, usize) {
        let mapped = key_for(key);
        let source: u64 = mapped.id.split(':').next().unwrap().parse().unwrap();
        self.effects
            .lock()
            .unwrap()
            .get(&source)
            .copied()
            .unwrap_or_default()
    }
    fn fail_next_wake(&mut self) {
        self.fail_wake = true;
    }
}

async fn seed(shared: &Arc<SharedData>, source: u64) {
    seed_at(shared, source, source + 10).await;
}
async fn seed_at(shared: &Arc<SharedData>, source: u64, channel: u64) {
    let channel = ChannelId::new(channel);
    let result = shared
        .mailbox(channel)
        .enqueue(
            Intervention {
                author_id: UserId::new(1),
                author_is_bot: false,
                message_id: MessageId::new(source),
                queued_generation: 17,
                source_message_ids: vec![MessageId::new(source)],
                source_message_queued_generations: Vec::new(),
                source_text_segments: Vec::new(),
                text: format!("original payload {source}"),
                mode: InterventionMode::Soft,
                created_at: std::time::Instant::now(),
                reply_context: Some("original reply".into()),
                has_reply_boundary: true,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
                voice_announcement: None,
            },
            QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None),
        )
        .await;
    assert!(result.enqueued);
    assert!(result.persistence_error.is_none());
}

// The existing mailbox/drain owns the payload; only its executor is replaced by this fake rig.
#[test]
fn live_mailbox_custody_sink_passes_shared_durable_conformance() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let shared = make_shared_data_for_tests();
            let effects = Arc::new(Mutex::new(BTreeMap::<u64, (usize, usize)>::new()));
            seed(&shared, 891_301).await;
            seed(&shared, 891_302).await;
            let counts = effects.clone();
            let _kick = queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
                move |shared, provider, channel, _| {
                    let counts = counts.clone();
                    Box::pin(async move {
                        let intent = mapped(&IntentRef {
                            id: if channel.get() == 891_311 {
                                "intent-1"
                            } else {
                                "intent-2"
                            }
                            .into(),
                            store: String::new(),
                            provider: String::new(),
                            channel: 0,
                            source_message: 0,
                            fifo: 0,
                        });
                        let gate = LiveSink::new(shared.clone(), provider.clone(), &consent_key())
                            .unwrap();
                        if !eligible(&gate, &intent).await.unwrap() {
                            return Some(IdleQueueKickoffChannelOutcome { started: false });
                        }
                        let result = shared
                            .mailbox(channel)
                            .take_next_soft(QueuePersistenceContext::new(
                                &provider,
                                &shared.token_hash,
                                None,
                            ))
                            .await;
                        if let Some(item) = result.intervention {
                            assert_eq!(
                                item.text,
                                format!("original payload {}", item.message_id.get())
                            );
                            counts
                                .lock()
                                .unwrap()
                                .entry(item.message_id.get())
                                .or_default()
                                .1 += 1;
                            Some(IdleQueueKickoffChannelOutcome { started: true })
                        } else {
                            Some(IdleQueueKickoffChannelOutcome { started: false })
                        }
                    })
                },
            ));
            for source in [891_301, 891_302] {
                let result = queue_io::kick_idle_queue_channel_if_context_available(
                    &shared,
                    &ProviderKind::Claude,
                    ChannelId::new(source + 10),
                    "fixture_held_probe",
                )
                .await;
                assert!(
                    !result.started,
                    "live_custody_pending_source_cannot_drain_contract"
                );
            }
            assert!(
                effects.lock().unwrap().is_empty(),
                "live_custody_release_is_actual_eligibility_contract"
            );
            custody_sink_conformance(|| Fixture {
                sink: LiveSink::new(shared.clone(), ProviderKind::Claude, &consent_key()).unwrap(),
                effects: effects.clone(),
                fail_wake: false,
            })
            .await;
        });
}

#[test]
fn live_custody_replays_a_lost_response_and_wake_without_recreating_payload() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let shared = make_shared_data_for_tests();
            seed(&shared, 891_301).await;
            let intent = mapped(&IntentRef {
                store: String::new(),
                id: "intent-1".into(),
                provider: String::new(),
                channel: 0,
                source_message: 0,
                fifo: 0,
            });
            let key = SettlementKey {
                nonce: "generation-1".into(),
                store: intent.store.clone(),
                id: intent.id.clone(),
            };
            let effects = Arc::new(Mutex::new(0usize));
            let counts = effects.clone();
            let kicks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let kick_calls = kicks.clone();
            let _kick = queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
                move |shared, provider, channel, _| {
                    kick_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let counts = counts.clone();
                    Box::pin(async move {
                        let taken = shared
                            .mailbox(channel)
                            .take_next_soft(QueuePersistenceContext::new(
                                &provider,
                                &shared.token_hash,
                                None,
                            ))
                            .await;
                        if taken.intervention.is_some() {
                            *counts.lock().unwrap() += 1;
                        }
                        Some(IdleQueueKickoffChannelOutcome { started: true })
                    })
                },
            ));
            let mut first = LiveSink::new(shared.clone(), ProviderKind::Claude, &consent_key()).unwrap();
            let lost_response = match first.release(&key, &intent).await {
                Ok(()) => Err::<(), String>("injected response loss after durable release".into()),
                Err(error) => Err(error),
            };
            assert!(matches!(&lost_response, Err(error) if error == "injected response loss after durable release"), "live_custody_response_loss_after_effect_is_real_contract");
            drop(first);
            let mut reopened = LiveSink::new(shared.clone(), ProviderKind::Claude, &consent_key()).unwrap();
            reopened.release(&key, &intent).await.unwrap();
            reopened.wake(&key, &intent).await.unwrap();
            seed_at(&shared, 891_399, intent.channel).await;
            let name = reopened.identity(&key, &intent).unwrap();
            reopened
                .directory
                .change(&name, |bytes| {
                    let mut released: Released = serde_json::from_slice(&bytes.unwrap()).unwrap();
                    released.woken = false;
                    serde_json::to_vec(&released).map_err(|e| e.to_string())
                })
                .unwrap();
            drop(reopened);
            let mut replay = LiveSink::new(shared.clone(), ProviderKind::Claude, &consent_key()).unwrap();
            replay.release(&key, &intent).await.unwrap();
            replay.wake(&key, &intent).await.unwrap();
            assert_eq!(
                *effects.lock().unwrap(),
                1,
                "live_custody_replayed_wake_never_recreates_payload_contract"
            );
            assert_eq!(
                kicks.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "live_custody_replay_never_kicks_an_absent_original_contract"
            );
            assert!(
                shared
                    .mailbox(ChannelId::new(intent.channel))
                    .try_snapshot()
                    .await
                    .unwrap()
                    .intervention_queue
                    .iter().map(|item| item.message_id.get()).eq([891_399]),
                "live_custody_replayed_wake_does_not_take_later_payload_contract"
            );
            let later = SettlementKey {
                nonce: "generation-2".into(),
                ..key.clone()
            };
            replay.release(&later, &intent).await.unwrap();
            replay.wake(&later, &intent).await.unwrap();
            assert_eq!(
                *effects.lock().unwrap(),
                1,
                "live_custody_cross_nonce_payload_once_contract"
            );
            let consents = crate::services::session_host::policy::consent::ConsentStore::new(
                crate::config::runtime_root()
                    .map(|root| root.join("runtime/herdr_policy/consents")),
            );
            assert!(
                consents.load_all().unwrap().is_empty(),
                "live_custody_sidecar_does_not_pollute_consent_inventory_contract"
            );
        });
}

#[test]
fn live_custody_missing_corrupt_or_changed_references_fail_closed() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let shared = make_shared_data_for_tests();
            let intent = mapped(&IntentRef {
                store: String::new(),
                id: "intent-1".into(),
                provider: String::new(),
                channel: 0,
                source_message: 0,
                fifo: 0,
            });
            let key = SettlementKey {
                nonce: "generation-1".into(),
                store: intent.store.clone(),
                id: intent.id.clone(),
            };
            let mut sink =
                LiveSink::new(shared.clone(), ProviderKind::Claude, &consent_key()).unwrap();
            assert!(
                sink.release(&key, &intent).await.is_err(),
                "live_custody_missing_is_not_released_contract"
            );
            assert!(
                sink.wake(&key, &intent).await.is_err(),
                "live_custody_wake_requires_durable_release_contract"
            );
            seed(&shared, 891_301).await;
            assert!(
                !sink.released(&intent).await.unwrap(),
                "live_custody_original_hold_precedes_release_contract"
            );
            sink.release(&key, &intent).await.unwrap();
            assert!(
                sink.released(&intent).await.unwrap(),
                "live_custody_release_visible_before_ack_contract"
            );
            let different = ConsentKey {
                named_session: "another".into(),
                ..consent_key()
            };
            let wrong = LiveSink::new(shared.clone(), ProviderKind::Claude, &different).unwrap();
            assert!(
                wrong.released(&intent).await.is_err(),
                "live_custody_stable_consent_identity_contract"
            );
            let changed = IntentRef {
                fifo: intent.fifo + 1,
                ..intent.clone()
            };
            assert!(
                sink.release(&key, &changed).await.is_err(),
                "live_custody_changed_source_is_not_released_contract"
            );
            let name = sink.identity(&key, &intent).unwrap();
            let path = sink
                .directory
                .0
                .as_ref()
                .unwrap()
                .join(format!("{name}.json"));
            let original = std::fs::read(&path).unwrap();
            let mut legacy: serde_json::Value = serde_json::from_slice(&original).unwrap();
            legacy.as_object_mut().unwrap().remove("consent");
            std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
            assert!(
                sink.released(&intent).await.is_err(),
                "live_custody_ambiguous_old_marker_is_not_permission_contract"
            );
            std::fs::write(&path, original).unwrap();
            std::fs::write(
                sink.directory
                    .0
                    .as_ref()
                    .unwrap()
                    .join(format!("{name}.json")),
                b"{",
            )
            .unwrap();
            assert!(
                sink.release(&key, &intent).await.is_err(),
                "live_custody_corruption_is_not_absence_contract"
            );
        });
}

struct OutboxFixture {
    sink: LiveSink,
    rows: Arc<BTreeMap<String, (i64, u64, String, ConsentKey)>>,
    effects: Arc<Mutex<BTreeMap<i64, (usize, usize)>>>,
    fail_wake: bool,
}
impl OutboxFixture {
    fn mapped(&self, key: &SettlementKey, intent: &IntentRef) -> (SettlementKey, IntentRef) {
        let (id, source, nonce, _) = &self.rows[&intent.id];
        let intent = IntentRef {
            store: "intake_outbox".into(),
            id: id.to_string(),
            provider: "claude".into(),
            channel: *source,
            source_message: *source,
            fifo: 1,
        };
        let nonce = if key.nonce == "generation-1" {
            nonce.clone()
        } else {
            format!("{nonce}-later")
        };
        (
            SettlementKey {
                nonce,
                store: intent.store.clone(),
                id: intent.id.clone(),
            },
            intent,
        )
    }
}
#[async_trait::async_trait]
impl CustodySink for OutboxFixture {
    async fn release(&mut self, key: &SettlementKey, intent: &IntentRef) -> Result<(), String> {
        self.sink = LiveSink::new(
            self.sink.shared.clone(),
            self.sink.provider.clone(),
            &self.rows[&intent.id].3,
        )?;
        let (key, intent) = self.mapped(key, intent);
        let pool = self.sink.shared.pg_pool.as_ref().unwrap().clone();
        let id: i64 = intent.id.parse().unwrap();
        let before: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .map_err(|e| e.to_string())?;
        self.sink.release(&key, &intent).await?;
        let after: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .map_err(|e| e.to_string())?;
        if before == "spawned" && after == "pending" {
            self.effects.lock().unwrap().entry(id).or_default().0 += 1;
        }
        Ok(())
    }
    async fn wake(&mut self, key: &SettlementKey, intent: &IntentRef) -> Result<(), String> {
        if std::mem::take(&mut self.fail_wake) {
            return Err("injected failure before existing worker poll".into());
        }
        self.sink = LiveSink::new(
            self.sink.shared.clone(),
            self.sink.provider.clone(),
            &self.rows[&intent.id].3,
        )?;
        let (key, intent) = self.mapped(key, intent);
        self.sink.wake(&key, &intent).await?;
        let pool = self.sink.shared.pg_pool.as_ref().unwrap().clone();
        if let Some(row) = crate::db::intake_outbox::claim_pending_for_target(
            &pool,
            "fence-worker",
            "claude",
            "token",
        )
        .await
        .map_err(|e| e.to_string())?
        {
            assert_eq!(
                row.id.to_string(),
                intent.id,
                "live_outbox_poll_preserves_original_identity_contract"
            );
            assert_eq!(row.user_text, "hello");
            assert!(
                crate::db::intake_outbox::mark_accepted(&pool, row.id, "token")
                    .await
                    .map_err(|e| e.to_string())?
            );
            assert!(
                crate::db::intake_outbox::mark_spawned(&pool, row.id, "token")
                    .await
                    .map_err(|e| e.to_string())?
            );
            self.effects.lock().unwrap().entry(row.id).or_default().1 += 1;
        }
        Ok(())
    }
}
impl CustodySinkFixture for OutboxFixture {
    fn effect_counts(&self, key: &SettlementKey) -> (usize, usize) {
        let id = self.rows[&key.id].0;
        self.effects
            .lock()
            .unwrap()
            .get(&id)
            .copied()
            .unwrap_or_default()
    }
    fn fail_next_wake(&mut self) {
        self.fail_wake = true;
    }
}

#[test]
fn live_outbox_custody_sink_passes_shared_durable_conformance_pg() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            use crate::db::intake_outbox::consent_handoff::tests::setup;
            let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
                pool.clone(),
            ));
            let mut rows = BTreeMap::new();
            let mut roots = Vec::new();
            for (id, channel) in [("intent-1", "891401"), ("intent-2", "891402")] {
                let (row, root, store, consent, intent) = setup(&pool, channel).await;
                crate::db::intake_outbox::consent_handoff::tests::hold_before_effects(
                    &pool, &row, &store, &consent, &intent,
                )
                .await
                .unwrap();
                rows.insert(
                    id.into(),
                    (row.id, intent.source_message, consent.nonce, consent.key),
                );
                roots.push(root);
            }
            let rows = Arc::new(rows);
            let effects = Arc::new(Mutex::new(BTreeMap::new()));
            custody_sink_conformance(|| OutboxFixture {
                sink: LiveSink::new(shared.clone(), ProviderKind::Claude, &consent_key()).unwrap(),
                rows: rows.clone(),
                effects: effects.clone(),
                fail_wake: false,
            })
            .await;
            pool.close().await;
            db.drop().await;
        });
}

struct AckFailure<'a> {
    sink: &'a mut LiveSink,
    marker: std::path::PathBuf,
}
#[async_trait::async_trait]
impl CustodySink for AckFailure<'_> {
    async fn release(&mut self, key: &SettlementKey, intent: &IntentRef) -> Result<(), String> {
        self.sink.release(key, intent).await
    }
    async fn wake(&mut self, key: &SettlementKey, intent: &IntentRef) -> Result<(), String> {
        self.sink.wake(key, intent).await?;
        std::fs::write(&self.marker, b"interrupted acknowledgement").map_err(|e| e.to_string())
    }
}

#[test]
fn live_mailbox_crash_at_receipt_release_wake_or_ack_replays_one_original_payload() {
    use crate::services::session_host::policy::consent::{
        ApplyResult, ConsentStore, Preview, Question, settlement,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for (index, stage) in ["receipt", "release", "wake", "ack"]
                .into_iter()
                .enumerate()
            {
                let shared = make_shared_data_for_tests();
                let source = 891_701 + index as u64 * 20;
                seed(&shared, source).await;
                let intent = IntentRef {
                    store: "mailbox".into(),
                    id: format!("{source}:17"),
                    provider: "claude".into(),
                    channel: source + 10,
                    source_message: source,
                    fifo: 17,
                };
                let key = ConsentKey {
                    named_session: stage.into(),
                    ..consent_key()
                };
                let directory = crate::config::runtime_root()
                    .unwrap()
                    .join("runtime/herdr_policy/consents");
                let store = ConsentStore::new(Some(directory.clone()));
                let pending = store
                    .hold(
                        Question {
                            key: key.clone(),
                            preview: Preview {
                                revision: 1,
                                witness: crate::services::session_host::ServerWitness::for_test(7),
                                target_hash: [1; 32],
                                chain_hash: [2; 32],
                                checker_hash: [3; 32],
                            },
                            owner: 1,
                            bot: 2,
                            provider: "claude".into(),
                            channel: intent.channel,
                            message: 4,
                            intent: intent.clone(),
                        },
                        100,
                    )
                    .unwrap()
                    .into_consent();
                let accepted = store.decide(&pending, 1, true, 101).unwrap();
                let applied = store
                    .record_apply(
                        &accepted,
                        ApplyResult {
                            before: Some([1; 32]),
                            after: Some([4; 32]),
                            backup: None,
                            detail: None,
                        },
                    )
                    .unwrap();
                let receipt = SettlementKey {
                    nonce: applied.nonce.clone(),
                    store: intent.store.clone(),
                    id: intent.id.clone(),
                };
                let effects = Arc::new(Mutex::new(0));
                let counted = effects.clone();
                let original = intent.clone();
                let consent_key = key.clone();
                let _kick = queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
                    move |shared, provider, channel, _| {
                        let counted = counted.clone();
                        let intent = original.clone();
                        let key = consent_key.clone();
                        Box::pin(async move {
                            let gate =
                                LiveSink::new(shared.clone(), provider.clone(), &key).unwrap();
                            assert!(
                                gate.released(&intent).await.unwrap(),
                                "live_startup_release_precedes_wake_contract"
                            );
                            let result = shared
                                .mailbox(channel)
                                .take_next_soft(QueuePersistenceContext::new(
                                    &provider,
                                    &shared.token_hash,
                                    None,
                                ))
                                .await;
                            if let Some(item) = result.intervention {
                                assert_eq!(item.message_id.get(), intent.source_message);
                                *counted.lock().unwrap() += 1;
                            }
                            Some(IdleQueueKickoffChannelOutcome { started: true })
                        })
                    },
                ));
                let mut sink = LiveSink::new(shared.clone(), ProviderKind::Claude, &key).unwrap();
                assert!(
                    !sink.released(&intent).await.unwrap(),
                    "live_startup_receipt_alone_does_not_release_contract"
                );
                match stage {
                    "release" => sink.release(&receipt, &intent).await.unwrap(),
                    "wake" => {
                        sink.release(&receipt, &intent).await.unwrap();
                        sink.wake(&receipt, &intent).await.unwrap();
                    }
                    "ack" => {
                        let marker =
                            directory.join(format!("{}.pending", durable::key(&key).unwrap()));
                        let mut failed = AckFailure {
                            sink: &mut sink,
                            marker: marker.clone(),
                        };
                        assert!(
                            settlement::release(&store, &applied, &mut failed)
                                .await
                                .is_err(),
                            "live_startup_ack_crash_is_not_success_contract"
                        );
                        std::fs::remove_file(marker).unwrap();
                        assert!(
                            store.load(&key).unwrap().unwrap().settlements.is_empty(),
                            "live_startup_ack_crash_keeps_reference_contract"
                        );
                    }
                    _ => {}
                }
                drop(sink);
                let reopened = ConsentStore::new(Some(directory));
                let mut sink = LiveSink::new(shared.clone(), ProviderKind::Claude, &key).unwrap();
                let settled = settlement::release(&reopened, &applied, &mut sink)
                    .await
                    .unwrap();
                assert!(
                    settled.unresolved_intents().is_empty(),
                    "live_startup_replay_acknowledges_only_after_effect_contract"
                );
                for nonce in [&receipt.nonce, "another-generation"] {
                    let replay = SettlementKey {
                        nonce: nonce.to_string(),
                        ..receipt.clone()
                    };
                    sink.release(&replay, &intent).await.unwrap();
                    sink.wake(&replay, &intent).await.unwrap();
                }
                assert_eq!(
                    *effects.lock().unwrap(),
                    1,
                    "live_startup_each_crash_original_payload_once_contract: {stage}"
                );
            }
        });
}
