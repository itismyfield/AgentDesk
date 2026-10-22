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
    effects: Arc<Mutex<BTreeMap<u64, usize>>>,
    fail_wake: bool,
}
#[async_trait::async_trait]
impl CustodySink for Fixture {
    async fn release(&mut self, key: &SettlementKey, intent: &IntentRef) -> Result<(), String> {
        self.sink.release(&key_for(key), &mapped(intent)).await
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
        let name = durable::key(&(mapped.store.as_str(), mapped.id.as_str())).unwrap();
        let release = usize::from(self.sink.directory.read(&name).unwrap().is_some());
        let source: u64 = mapped.id.split(':').next().unwrap().parse().unwrap();
        let wakes = self
            .effects
            .lock()
            .unwrap()
            .get(&source)
            .copied()
            .unwrap_or_default();
        (release, wakes)
    }
    fn fail_next_wake(&mut self) {
        self.fail_wake = true;
    }
}

async fn seed(shared: &Arc<SharedData>, source: u64) {
    let channel = ChannelId::new(source + 10);
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
            let effects = Arc::new(Mutex::new(BTreeMap::<u64, usize>::new()));
            seed(&shared, 891_301).await;
            seed(&shared, 891_302).await;
            let counts = effects.clone();
            let _kick = queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
                move |shared, provider, channel, _| {
                    let counts = counts.clone();
                    Box::pin(async move {
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
                            *counts
                                .lock()
                                .unwrap()
                                .entry(item.message_id.get())
                                .or_default() += 1;
                            Some(IdleQueueKickoffChannelOutcome { started: true })
                        } else {
                            Some(IdleQueueKickoffChannelOutcome { started: false })
                        }
                    })
                },
            ));
            custody_sink_conformance(|| Fixture {
                sink: LiveSink::new(shared.clone(), ProviderKind::Claude),
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
            let _kick = queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
                move |shared, provider, channel, _| {
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
            let mut first = LiveSink::new(shared.clone(), ProviderKind::Claude);
            let lost_response = match first.release(&key, &intent).await {
                Ok(()) => Err::<(), String>("injected response loss after durable release".into()),
                Err(error) => Err(error),
            };
            assert!(matches!(&lost_response, Err(error) if error == "injected response loss after durable release"), "live_custody_response_loss_after_effect_is_real_contract");
            drop(first);
            let mut reopened = LiveSink::new(shared.clone(), ProviderKind::Claude);
            reopened.release(&key, &intent).await.unwrap();
            reopened.wake(&key, &intent).await.unwrap();
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
            let mut replay = LiveSink::new(shared.clone(), ProviderKind::Claude);
            replay.release(&key, &intent).await.unwrap();
            replay.wake(&key, &intent).await.unwrap();
            assert_eq!(
                *effects.lock().unwrap(),
                1,
                "live_custody_replayed_wake_never_recreates_payload_contract"
            );
            assert!(
                shared
                    .mailbox(ChannelId::new(intent.channel))
                    .try_snapshot()
                    .await
                    .unwrap()
                    .intervention_queue
                    .is_empty()
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
            let mut sink = LiveSink::new(shared.clone(), ProviderKind::Claude);
            assert!(
                sink.release(&key, &intent).await.is_err(),
                "live_custody_missing_is_not_released_contract"
            );
            assert!(
                sink.wake(&key, &intent).await.is_err(),
                "live_custody_wake_requires_durable_release_contract"
            );
            seed(&shared, 891_301).await;
            sink.release(&key, &intent).await.unwrap();
            let changed = IntentRef {
                fifo: intent.fifo + 1,
                ..intent.clone()
            };
            assert!(
                sink.release(&key, &changed).await.is_err(),
                "live_custody_changed_source_is_not_released_contract"
            );
            let name = sink.identity(&key, &intent).unwrap();
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
    rows: Arc<BTreeMap<String, (i64, u64, String)>>,
    effects: Arc<Mutex<BTreeMap<i64, (usize, usize)>>>,
    fail_wake: bool,
}
impl OutboxFixture {
    fn mapped(&self, key: &SettlementKey, intent: &IntentRef) -> (SettlementKey, IntentRef) {
        let (id, source, nonce) = &self.rows[&intent.id];
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
                crate::db::intake_outbox::consent_handoff::hold_before_effects(
                    &pool, &row, &store, &consent, &intent,
                )
                .await
                .unwrap();
                rows.insert(id.into(), (row.id, intent.source_message, consent.nonce));
                roots.push(root);
            }
            let rows = Arc::new(rows);
            let effects = Arc::new(Mutex::new(BTreeMap::new()));
            custody_sink_conformance(|| OutboxFixture {
                sink: LiveSink::new(shared.clone(), ProviderKind::Claude),
                rows: rows.clone(),
                effects: effects.clone(),
                fail_wake: false,
            })
            .await;
            pool.close().await;
            db.drop().await;
        });
}
