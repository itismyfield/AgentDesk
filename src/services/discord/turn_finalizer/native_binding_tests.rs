use super::*;
use crate::services::tui_input::human_frame::history::v2::native::{Disposition, Episode, Intent};
use crate::services::turn_orchestrator::BindingIntent;
use serde_json::json;

struct Fixture {
    _root: crate::config::TestRuntimeRootGuard,
    shared: Arc<SharedData>,
    ticket: Ticket,
    namespace: String,
    history: Registry,
    history_path: std::path::PathBuf,
    path: std::path::PathBuf,
    nonce: String,
    logical: String,
}

impl Fixture {
    async fn new() -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel = 81489971;
        let logical = format!("cold-source-{}", uuid::Uuid::new_v4().simple());
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let session = uuid::Uuid::new_v4().to_string();
        let projects = root.path().join("projects");
        let path = projects.join("project").join(format!("{session}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let context = binding_context::BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: nonce.clone(),
            tmux_session: logical.clone(),
            channel_id: Some(channel),
            owner_runtime_root: "cold-source-runtime".into(),
            host: None,
            expected_native_session_id: Some(session.clone()),
            launch_mode: "fresh".into(),
            provider_root: Some(projects),
            first_prompt_digest: None,
            source_policy: None,
            resume_start: None,
        };
        binding_context::PreparedIncarnation::create(context).unwrap();
        let marker = std::path::PathBuf::from(crate::services::tmux_common::session_temp_path(
            &logical,
            "spawn_nonce",
        ));
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(marker, &nonce).unwrap();
        dedupe::register_tmux_channel(&logical, channel);
        crate::services::tmux_common::with_tmux_source_authority(&logical, |authority| {
            assert!(
                dedupe::pane_registration::register_claude_pane_under_source_authority(
                    authority,
                    channel,
                    dedupe::TuiRuntimeBinding {
                        runtime_kind:
                            crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                        output_path: path.display().to_string(),
                        session_id: Some(session.clone()),
                        last_offset: 0,
                        relay_output_path: None,
                        input_fifo_path: None,
                        relay_last_offset: None,
                    },
                    dedupe::Record::Stat,
                )
                .is_some_and(dedupe::Persisted::published)
            );
        });
        let history_path = root.path().join("runtime/injected_frames.json");
        let history = Registry::at(history_path.clone());
        let mailbox = shared.mailbox(ChannelId::new(channel));
        mailbox
            .v2_ensure_channel(&persistence(&shared), ChannelId::new(channel))
            .await
            .unwrap();
        let id = 8_000_000_000_081_489_971u64;
        let episode = Episode {
            user_message_id: id,
            turn_nonce: "cold-episode".into(),
            generation: 1,
            extra: Default::default(),
        };
        let attempt = json!({"nonce":"cold-attempt","provider":"claude",
            "target":{"host":"herdr","owner":logical,"root":"test-socket","execution_nonce":nonce},
            "source":{"kind":"pre_source","expected_session_id":session,"expected_transcript":path,"binding_watermark":null}});
        let saved = history.clone();
        let ticket = mailbox.v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            let item = serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":channel,
                "source_message_ids":[id],"queued_generation":1,"text":"cold request"})).unwrap();
            owner.enqueue_fixture(&item).map_err(|e| format!("{e:?}"))?;
            let key = owner.reserve(serde_json::from_value(attempt).unwrap(), serde_json::from_value(json!({
                "frame":"cold request","digest":"digest","attribution":Intent::Primary(episode).attribution(&[id.to_string()])})).unwrap())?;
            owner.prepare_history(&saved)?;
            let ns = owner.snapshot.namespace.clone().unwrap();
            saved.native_ticket(&ns, &serde_json::to_value(key).unwrap())
        }).await.unwrap();
        let namespace = format!("claude/{}", shared.token_hash);
        Self {
            _root: root,
            shared,
            ticket,
            namespace,
            history,
            history_path,
            path,
            nonce,
            logical,
        }
    }

    fn port(&self) -> Port {
        Port {
            channel: self.ticket.channel_id,
            shared: self.shared.clone(),
            bindings: Arc::new(ChannelBindingLog::new(
                self.ticket.channel_id,
                ShadowProvider::Claude,
            )),
            pending: Mutex::default(),
        }
    }

    async fn pin_and_submit(&self) {
        pin_source(&self.shared, &self.namespace, &self.ticket)
            .await
            .unwrap();
        self.history
            .note_native_any_key(&self.namespace, &self.ticket)
            .unwrap();
        let ticket = self.ticket.clone();
        let snapshot = self
            .shared
            .mailbox(ChannelId::new(ticket.channel_id))
            .v2_request(move |slot| {
                let owner = slot.as_mut().unwrap();
                let key = owner.active_attempt()?;
                assert_eq!(serde_json::to_value(&key).unwrap(), ticket.key);
                drop(owner.arm(&key, 1)?);
                owner.settle(&key, true, 2)?;
                assert!(
                    owner.snapshot.head_injection.is_none()
                        && owner.snapshot.items.is_empty()
                        && owner.snapshot.retained_inputs.is_empty()
                );
                owner.json()
            })
            .await
            .unwrap();
        self.history.project(&snapshot).unwrap();
    }

    fn transcript(&self) {
        let session = self.ticket.key["attempt"]["source"]["expected_session_id"]
            .as_str()
            .unwrap();
        std::fs::write(&self.path, format!("{{\"type\":\"user\",\"uuid\":\"cold-user\",\"sessionId\":\"{session}\",\"message\":{{\"content\":\"cold request\"}}}}\n{{\"type\":\"system\",\"subtype\":\"turn_duration\"}}\n")).unwrap();
    }

    async fn intent(&self) -> BindingIntent {
        let ticket = self.ticket.clone();
        self.shared
            .mailbox(ChannelId::new(ticket.channel_id))
            .v2_request(move |slot| Ok(slot.as_ref().unwrap().source_binding(&ticket)?.unwrap()))
            .await
            .unwrap()
    }

    async fn assert_bound(&self) {
        let ticket = self
            .history
            .native_ticket(&self.namespace, &self.ticket.key)
            .unwrap();
        let source = ticket.source.unwrap();
        assert_eq!(source.offset, 0);
        assert_eq!(source.target, self.ticket.key["attempt"]["target"]);
        let ticket = self.ticket.clone();
        self.shared
            .mailbox(ChannelId::new(ticket.channel_id))
            .v2_request(move |slot| {
                assert!(
                    slot.as_ref().unwrap().source_binding(&ticket)?.is_none(),
                    "history durable ACK must reclaim the capsule"
                );
                Ok(())
            })
            .await
            .unwrap();
        let fresh = Registry::at(self.history_path.clone());
        assert!(
            fresh
                .native_ticket(&self.namespace, &self.ticket.key)
                .unwrap()
                .source
                .is_some()
        );
    }
}

#[tokio::test]
async fn native_8148_coldsource_producerless_pending_resolves_from_exact_path() {
    let f = Fixture::new().await;
    f.pin_and_submit().await;
    f.transcript();
    f.port()
        .bind_source(&f.namespace, &f.ticket, &f.history)
        .await
        .unwrap();
    f.assert_bound().await;
}

#[tokio::test]
async fn native_8148_coldsource_empty_owner_boot_without_enqueue() {
    let f = Fixture::new().await;
    f.pin_and_submit().await;
    f.transcript();
    f.shared
        .mailbox(ChannelId::new(f.ticket.channel_id))
        .v2_request(|slot| {
            *slot = None;
            Ok(())
        })
        .await
        .unwrap();
    let history = Registry::at(f.history_path.clone());
    f.port()
        .bind_source(&f.namespace, &f.ticket, &history)
        .await
        .unwrap();
    f.assert_bound().await;
}

#[tokio::test]
async fn native_8148_coldsource_history_ack_loss_then_control_is_idempotent() {
    let f = Fixture::new().await;
    f.pin_and_submit().await;
    f.transcript();
    let port = f.port();
    let intent = f.intent().await;
    port.resolve_pending(&f.ticket, &intent.pin).unwrap();
    let id = bound_source(
        &port
            .bindings
            .binding_events_since(f.ticket.channel_id, 0)
            .unwrap(),
        &f.ticket,
        &intent.pin,
    )
    .unwrap();
    let source = Source {
        target: f.ticket.key["attempt"]["target"].clone(),
        provider: "claude".into(),
        session_id: id.session_id,
        transcript: id.path,
        file: [id.dev, id.ino],
        offset: 0,
        binding_watermark: None,
    };
    let ticket = f.ticket.clone();
    let selected = source.clone();
    f.shared
        .mailbox(ChannelId::new(ticket.channel_id))
        .v2_request(move |slot| slot.as_mut().unwrap().select_source(&ticket, &selected))
        .await
        .unwrap();
    f.history
        .bind_native_source(&f.namespace, &f.ticket, &source)
        .unwrap();
    // The original source=None control custody refines without reinstallation.
    f.history
        .try_native_control(&f.namespace, &f.ticket, Disposition::Cancelled, None)
        .unwrap();
    std::fs::remove_file(&f.path).unwrap();
    let fresh = Registry::at(f.history_path.clone());
    port.bind_source(&f.namespace, &f.ticket, &fresh)
        .await
        .unwrap();
    f.assert_bound().await;
    let current = fresh.native_ticket(&f.namespace, &f.ticket.key).unwrap();
    assert_eq!(
        fresh
            .native_settlement(&f.namespace, &current)
            .unwrap()
            .outcome
            .unwrap()
            .disposition,
        Disposition::Cancelled
    );
}

#[tokio::test]
async fn native_8148_coldsource_foreign_lineage_is_rejected() {
    let f = Fixture::new().await;
    f.pin_and_submit().await;
    f.transcript();
    let intent = f.intent().await;
    let port = f.port();
    port.resolve_pending(&f.ticket, &intent.pin).unwrap();
    let mut ticket = f.ticket.clone();
    ticket.key["attempt"]["target"]["execution_nonce"] = json!("foreign");
    assert!(
        bound_source(
            &port
                .bindings
                .binding_events_since(f.ticket.channel_id, 0)
                .unwrap(),
            &ticket,
            &intent.pin
        )
        .is_err()
    );
}

#[tokio::test]
async fn native_8148_coldsource_pending_successor_is_untouched() {
    let f = Fixture::new().await;
    f.pin_and_submit().await;
    f.transcript();
    let port = f.port();
    let marker = crate::services::tmux_common::session_temp_path(&f.logical, "spawn_nonce");
    std::fs::write(&marker, "successor-N2").unwrap();
    assert!(
        port.bind_source(&f.namespace, &f.ticket, &f.history)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "successor-N2");
    assert!(
        f.history
            .native_ticket(&f.namespace, &f.ticket.key)
            .unwrap()
            .source
            .is_none()
    );
    assert!(f.intent().await.selected.is_none());
}

#[tokio::test]
async fn native_8148_coldsource_terminal_none_refuses_new_source() {
    let f = Fixture::new().await;
    pin_source(&f.shared, &f.namespace, &f.ticket)
        .await
        .unwrap();
    f.transcript();
    f.history
        .try_native_not_sent(&f.namespace, &f.ticket)
        .unwrap();
    f.port()
        .bind_source(&f.namespace, &f.ticket, &f.history)
        .await
        .unwrap();
    assert!(
        f.history
            .native_ticket(&f.namespace, &f.ticket.key)
            .unwrap()
            .source
            .is_none()
    );
    let file = crate::services::tui_o::shadow::capture::file_identity(
        &std::fs::metadata(&f.path).unwrap(),
    );
    let source = Source {
        target: f.ticket.key["attempt"]["target"].clone(),
        provider: "claude".into(),
        session_id: f.ticket.key["attempt"]["source"]["expected_session_id"]
            .as_str()
            .unwrap()
            .into(),
        transcript: f.path.clone(),
        file: [file.0, file.1],
        offset: 0,
        binding_watermark: None,
    };
    assert!(
        f.history
            .bind_native_source(&f.namespace, &f.ticket, &source)
            .is_err()
    );
}

#[test]
fn native_8148_coldsource_prekey_boundary_rejects_partial_and_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    assert_eq!(boundary(&path).unwrap(), (0, None));
    std::fs::write(&path, b"partial").unwrap();
    assert!(boundary(&path).is_err());
    std::fs::write(&path, b"complete\n").unwrap();
    assert_eq!(boundary(&path).unwrap().0, 9);
    assert!(boundary(dir.path()).is_err());
}

#[tokio::test]
async fn native_8148_coldsource_owner_only_and_unflushed_history_recover_after_restart() {
    use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};
    for step in [Step::CreateTemp, Step::ParentSync] {
        let f = Fixture::new().await;
        f.pin_and_submit().await;
        f.transcript();
        {
            let watch = Watch::new(
                &f.history_path,
                Plan {
                    fail: vec![step],
                    ..Default::default()
                },
            );
            assert!(
                f.port()
                    .bind_source(&f.namespace, &f.ticket, &f.history)
                    .await
                    .is_err()
            );
            assert!(watch.events().iter().any(|event| !event.ok));
            assert!(
                f.intent().await.selected.is_some(),
                "owner must retain its selected source before history ACK"
            );
        }
        f.shared
            .mailbox(ChannelId::new(f.ticket.channel_id))
            .v2_request(|slot| {
                *slot = None;
                Ok(())
            })
            .await
            .unwrap();
        let fresh = Registry::at(f.history_path.clone());
        f.port()
            .bind_source(&f.namespace, &f.ticket, &fresh)
            .await
            .unwrap();
        f.assert_bound().await;
    }
}

#[tokio::test]
async fn native_8148_coldsource_cap_preserves_debt_and_reclaims_only_after_ack() {
    use crate::services::tui_input::human_frame::history::CAPACITY;
    let f = Fixture::new().await;
    pin_source(&f.shared, &f.namespace, &f.ticket)
        .await
        .unwrap();
    let intent = f.intent().await;
    let ticket = f.ticket.clone();
    let pin = intent.pin.clone();
    f.shared
        .mailbox(ChannelId::new(ticket.channel_id))
        .v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            owner.snapshot.source_bindings = (0..CAPACITY)
                .map(|n| {
                    let mut old = intent.clone();
                    old.ticket.key["attempt"]["nonce"] = json!(format!("old-debt-{n}"));
                    old
                })
                .collect();
            assert!(
                owner
                    .pin_source(ticket.clone(), pin.clone())
                    .unwrap_err()
                    .contains("debt full")
            );
            assert_eq!(owner.snapshot.source_bindings.len(), CAPACITY);
            owner.snapshot.source_bindings[0] = BindingIntent {
                ticket: ticket.clone(),
                pin: pin.clone(),
                selected: None,
            };
            owner.pin_source(ticket.clone(), pin.clone())?;
            assert_eq!(
                owner.snapshot.source_bindings.len(),
                CAPACITY,
                "same full key takes no new slot"
            );
            // Arm publishes the fixture's bounded set through the same owner CAS; no key is sent.
            let key = owner.active_attempt()?;
            drop(owner.arm(&key, 1)?);
            Ok(())
        })
        .await
        .unwrap();
    f.history
        .try_native_not_sent(&f.namespace, &f.ticket)
        .unwrap();
    release_unkeyed_source(&f.shared, &f.history, &f.namespace, &f.ticket)
        .await
        .unwrap();
    f.shared
        .mailbox(ChannelId::new(f.ticket.channel_id))
        .v2_request(|slot| {
            assert_eq!(
                slot.as_ref().unwrap().snapshot.source_bindings.len(),
                CAPACITY - 1
            );
            Ok(())
        })
        .await
        .unwrap();
}

async fn actual_o_recovery(boot: bool) {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::discord::{inflight, turn_finalizer as finalizer};
    use crate::services::tui_o::{
        ownership::OwnershipGate,
        store::{Initialized, OStore, StoreConfig},
        writer::{
            deliver::ChannelWriter,
            host::test_io::{Alarms, AnyLease, Posts},
            pieces::UnitDeriver,
            rotation::Sources,
        },
    };
    let f = Fixture::new().await;
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    f.pin_and_submit().await;
    f.transcript();
    let episode = f.ticket.owning_episode.as_ref().unwrap();
    let session = f.ticket.key["attempt"]["source"]["expected_session_id"]
        .as_str()
        .unwrap();
    let mut row = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        f.ticket.channel_id,
        None,
        7,
        episode.user_message_id,
        episode.user_message_id,
        "cold request".into(),
        Some(session.into()),
        Some(f.logical.clone()),
        Some(f.path.display().to_string()),
        None,
        0,
    );
    row.born_generation = 1;
    row.turn_nonce = Some(episode.turn_nonce.clone());
    row.hosted_execution_nonce = Some(f.nonce.clone());
    inflight::save_inflight_state(&row).unwrap();
    let mut shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = if boot { 2 } else { 1 };
    assert_eq!(shared.token_hash, f.shared.token_hash);
    let channel = ChannelId::new(f.ticket.channel_id);
    if boot {
        finalizer::native_prepare::BootInputs::prepare(&f._root.path().join("runtime"), 2)
            .unwrap()
            .restore(&shared)
            .await
            .unwrap();
    } else {
        let actor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
            episode.turn_nonce.clone(),
        )));
        shared
            .mailbox(channel)
            .restore_active_turn(
                actor.clone(),
                poise::serenity_prelude::UserId::new(7),
                episode.user_message_id.into(),
            )
            .await;
        shared
            .restart
            .global_active
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let key = TurnKey::new(channel, episode.user_message_id, 1)
            .with_episode_nonce(Some(&episode.turn_nonce));
        shared.turn_finalizer.register_start(
            key,
            ProviderKind::Claude,
            crate::services::discord::inflight::RelayOwnerKind::None,
            &shared,
        );
        let (ack, rx) = oneshot::channel();
        shared
            .turn_finalizer
            .tx
            .send(FinalizeMsg::NativeRegister {
                key,
                actor: Some(actor),
                history: f.history.clone(),
                namespace: f.namespace.clone(),
                ticket: Box::new(f.ticket.clone()),
                shared: shared.clone(),
                ack,
            })
            .unwrap();
        let input =
            crate::test_support::wait::await_within(rx, crate::test_support::wait::WAIT_CAP)
                .await
                .unwrap()
                .unwrap();
        let (ack, rx) = oneshot::channel();
        shared
            .turn_finalizer
            .tx
            .send(FinalizeMsg::NativeKeyed {
                key,
                input,
                keyed: true,
                history: f.history.clone(),
                shared: shared.clone(),
                ack,
            })
            .unwrap();
        crate::test_support::wait::await_within(rx, crate::test_support::wait::WAIT_CAP)
            .await
            .unwrap()
            .unwrap();
    }
    // This new runtime has no queue owner, producer or RuntimeReady; its normal O poll must recover.
    assert!(
        shared
            .mailbox(channel)
            .v2_request(|slot| Ok(slot.is_none()))
            .await
            .unwrap()
    );
    let root = f._root.path().join("cold-source-test-store");
    std::fs::create_dir_all(&root).unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root)
        .unwrap()
        .unwrap();
    let era = store
        .begin_fixture_era(&[f.ticket.channel_id], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: Vec::new(),
                initial_anchor: 100,
                build_digest: "cold-source-test".into(),
                at: chrono::Utc::now(),
                policy_parent: None,
            })
        })
        .unwrap();
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let mut writer = ChannelWriter::new(
        store
            .open_channel(&era, f.ticket.channel_id)
            .unwrap()
            .unwrap(),
        gate,
        Arc::new(Posts::default()),
        AnyLease,
        Alarms::default(),
    );
    let bindings = Arc::new(ChannelBindingLog::new(
        f.ticket.channel_id,
        ShadowProvider::Claude,
    ));
    let mut sources = Sources::new(
        f.ticket.channel_id,
        ShadowProvider::Claude,
        bindings.clone(),
    )
    .with_native_inputs(true);
    let mut deriver = UnitDeriver::new(f.ticket.channel_id, ShadowProvider::Claude);
    let mut owed = std::collections::VecDeque::new();
    let port = super::super::port(f.ticket.channel_id, shared.clone(), bindings);
    for _ in 0..4 {
        sources.follow(&mut writer).unwrap();
        sources
            .capture(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        sources.offer_native(&port).await;
    }
    let ticket = f
        .history
        .native_ticket(&f.namespace, &f.ticket.key)
        .unwrap();
    let saved = f.history.native_settlement(&f.namespace, &ticket).unwrap();
    assert_eq!(
        saved.outcome.as_ref().map(|o| &o.disposition),
        Some(&Disposition::Executed)
    );
    assert!(saved.cleanup_complete);
    assert!(
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, f.ticket.channel_id)
            .is_none()
    );
    assert!(
        shared
            .mailbox(channel)
            .snapshot()
            .await
            .cancel_token
            .is_none()
    );
    assert!(
        shared
            .mailbox(channel)
            .v2_request(move |slot| Ok(slot.as_ref().unwrap().source_binding(&ticket)?.is_none()))
            .await
            .unwrap()
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn native_8148_coldsource_actual_o_producerless_live_receipt_cleans_row_pg() {
    actual_o_recovery(false).await;
}

#[tokio::test]
async fn native_8148_coldsource_actual_o_empty_owner_boot_receipt_cleans_row_pg() {
    actual_o_recovery(true).await;
}
