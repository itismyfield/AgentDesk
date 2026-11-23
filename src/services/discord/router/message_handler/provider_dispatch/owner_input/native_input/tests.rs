use super::*;
use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::discord::queue_io::input_owner::admission::{AdmissionTicket, admit};
use crate::services::tui_input::human_frame::history::v2::Registry;
use crate::test_support::wait::{WAIT_CAP, await_within};

fn cold_attempt(
    root: &std::path::Path,
    channel: u64,
) -> (serde_json::Value, std::path::PathBuf, String) {
    use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context};
    let session = uuid::Uuid::new_v4().to_string();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let logical = format!("native-prekey-{channel}");
    let projects = root.join("projects");
    let path = projects.join("project").join(format!("{session}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "").unwrap();
    binding_context::PreparedIncarnation::create(binding_context::BindingContext {
        schema: 1,
        provider: "claude".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: nonce.clone(),
        tmux_session: logical.clone(),
        channel_id: Some(channel),
        owner_runtime_root: "native-prekey".into(),
        host: None,
        expected_native_session_id: Some(session.clone()),
        launch_mode: "fresh".into(),
        provider_root: Some(projects),
        first_prompt_digest: None,
        source_policy: None,
        resume_start: None,
    })
    .unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&logical, "spawn_nonce"),
        &nonce,
    )
    .unwrap();
    dedupe::register_tmux_channel(&logical, channel);
    crate::services::tmux_common::with_tmux_source_authority(&logical, |authority| {
        assert!(
            dedupe::pane_registration::register_claude_pane_under_source_authority(
                authority,
                channel,
                dedupe::TuiRuntimeBinding {
                    runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                    output_path: path.display().to_string(),
                    session_id: Some(session.clone()),
                    last_offset: 0,
                    relay_output_path: None,
                    input_fifo_path: None,
                    relay_last_offset: None,
                },
                dedupe::Record::Stat
            )
            .is_some_and(dedupe::Persisted::published)
        );
    });
    (
        json!({"nonce":format!("native-{channel}"),"provider":"claude","target":{"host":"herdr","owner":logical,"root":"/fixture","execution_nonce":nonce},
        "source":{"kind":"pre_source","expected_session_id":session,"expected_transcript":path,"binding_watermark":null}}),
        path,
        session,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_8148_b2b2_created_primary_and_follower_ack_before_key() {
    let guard = crate::config::TestRuntimeRootGuard::new();
    let _binding_root = native_prepare::BindingRoot::enter(Some(guard.path()));
    let root = guard.path().join("runtime");
    std::fs::create_dir_all(&root).unwrap();
    for (primary, stale, foreign) in [
        (true, false, 0),
        (false, false, 0),
        (true, true, 0),
        (true, false, 1),
        (true, false, 2),
        (true, false, 3),
        (true, false, 4),
    ] {
        let channel = ChannelId::new(if foreign >= 3 {
            81489954 + foreign
        } else if foreign == 2 {
            81489954
        } else if foreign == 1 {
            81489950
        } else if stale {
            8148966
        } else if primary {
            8148961
        } else {
            8148962
        });
        let id = 8_000_000_000_000_000_000 + channel.get();
        let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        std::sync::Arc::get_mut(&mut shared)
            .unwrap()
            .restart
            .current_generation = if stale { 2 } else { 1 };
        let token = std::sync::Arc::new(CancelToken::from_persisted_turn_nonce(Some(
            "captured-native".into(),
        )));
        shared
            .restart
            .global_active
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let mailbox = shared.mailbox(channel);
        let active_token = if foreign == 2 {
            std::sync::Arc::new(CancelToken::from_persisted_turn_nonce(
                token.turn_nonce().map(str::to_owned),
            ))
        } else {
            token.clone()
        };
        mailbox
            .restore_active_turn(
                active_token.clone(),
                poise::serenity_prelude::UserId::new(7),
                poise::serenity_prelude::MessageId::new(if primary && foreign != 1 {
                    id
                } else {
                    id + 100
                }),
            )
            .await;
        let key = TurnKey::new(channel, id, 1);
        let key = if primary {
            key.with_episode_nonce(token.turn_nonce())
        } else {
            key
        };
        shared.turn_finalizer.register_start(
            key,
            ProviderKind::Claude,
            RelayOwnerKind::Watcher,
            &shared,
        );
        mailbox
            .v2_adopt(
                root.as_path().join(format!("owner-{}.json", channel.get())),
                format!("claude/{}", shared.token_hash),
            )
            .await
            .unwrap();
        let item = serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":channel.get(),"source_message_ids":[id],"queued_generation":1,"text":"our input"})).unwrap();
        let _ = admit(&mailbox, &item, AdmissionTicket::new()).await;
        let (attempt, transcript, session_id) = cold_attempt(&root, channel.get());
        let meta = std::fs::metadata(&transcript).unwrap();
        let file = crate::services::tui_o::shadow::capture::file_identity(&meta);
        if foreign == 3 {
            std::fs::write(&transcript, "partial record").unwrap();
        }
        if foreign == 4 {
            std::fs::remove_file(&transcript).unwrap();
            std::fs::create_dir(&transcript).unwrap();
        }
        let shared_leaf = shared.clone();
        let mailbox_leaf = mailbox.clone();
        let token_leaf = token.clone();
        let leaf: effect::Leaf = Box::new(move |_| {
            assert!(
                !stale && foreign == 0,
                "failed registration acknowledgement must prevent the first key"
            );
            let handle = tokio::runtime::Handle::current();
            assert_eq!(
                handle.block_on(shared_leaf.turn_finalizer.native_scope(key, &token_leaf)),
                Ok(primary),
                "registration ack precedes the first key"
            );
            assert_eq!(scope::key_attempted(), Some(false));
            handle
                .block_on(mailbox_leaf.v2_request(move |slot| {
                    let owner = slot.as_mut().unwrap();
                    let key = owner.active_attempt()?;
                    let history = Registry::runtime()?;
                    let source = crate::services::tui_input::human_frame::history::v2::Source {
                        target: serde_json::to_value(&key).unwrap()["attempt"]["target"].clone(),
                        provider: "claude".into(),
                        session_id,
                        transcript,
                        file: [file.0, file.1],
                        offset: 0,
                        binding_watermark: None,
                    };
                    owner.bind_source(&key, &source, &history)
                }))
                .unwrap();
            scope::keyed();
            assert_eq!(scope::key_attempted(), Some(true));
            Ok(Completion::WriteUncertain("after key".into()))
        });
        let producer = Producer::at(shared.clone(), key, primary.then(|| token.clone()));
        let intent = producer.intent.clone();
        let prepared = PreparedPayload {
            frame: "our input".into(),
            sources: vec![id.to_string()],
            attempt,
        };
        let result = await_within(
            pump::submit_native_front(
                &mailbox,
                None,
                prepared,
                intent,
                std::sync::Arc::new(|| 10_000),
                producer.wrap(leaf),
            ),
            WAIT_CAP,
        )
        .await;
        if foreign >= 3 {
            assert!(matches!(result, Ok(Completion::NotSent(_))), "{result:?}");
            assert!(mailbox.snapshot().await.cancel_token.is_none());
            assert!(token.is_completion_cleanup());
            continue;
        }
        if stale || foreign != 0 {
            assert!(matches!(result, Ok(Completion::NotSent(_))), "{result:?}");
            let history = Registry::runtime().unwrap().json().unwrap();
            let settlements: Vec<_> = ["planned", "live", "passive"]
                .into_iter()
                .flat_map(|section| history[section].as_array().into_iter().flatten())
                .map(|record| {
                    if record["registration"].is_object() {
                        &record["registration"]
                    } else {
                        record
                    }
                })
                .map(|record| &record["native_settlement"])
                .filter(|settlement| settlement["ticket"]["channel_id"] == channel.get())
                .collect();
            assert_eq!(settlements.len(), 1);
            assert!(
                settlements
                    .iter()
                    .all(|s| s["any_key"] != true && s["outcome"].is_null())
            );
            assert!(std::sync::Arc::ptr_eq(
                mailbox.snapshot().await.cancel_token.as_ref().unwrap(),
                &active_token
            ));
            assert_eq!(
                shared
                    .restart
                    .global_active
                    .load(std::sync::atomic::Ordering::SeqCst),
                1
            );
            continue;
        }
        assert_eq!(result, Ok(Completion::WriteUncertain("after key".into())));
        let history = Registry::runtime().unwrap().json().unwrap();
        let records: Vec<_> = history["live"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| {
                r["registration"]["native_settlement"]["ticket"]["channel_id"] == channel.get()
            })
            .collect();
        assert_eq!(records.len(), 1);
        let settlement = &records[0]["registration"]["native_settlement"];
        assert_eq!(settlement["any_key"], true);
        assert!(settlement["outcome"].is_null());
        assert_eq!(settlement["cleanup_complete"], false);
        assert_eq!(settlement["ticket"]["owning_episode"].is_null(), !primary);
        assert_eq!(
            shared
                .restart
                .global_active
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let snap = mailbox.snapshot().await;
        assert!(std::sync::Arc::ptr_eq(
            snap.cancel_token.as_ref().unwrap(),
            &token
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_8148_b2b2_no_key_keeps_original_and_disarms_native_custody() {
    let result = no_key_round(8148963, "herdr", None).await;
    assert_eq!(result, Completion::NotSent("fixture refusal".into()));
}

/// A no-key record that meets a brief registry contention still lands, so the input stays held.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_no_key_record_outlasts_a_brief_registry_contention() {
    let hold = std::time::Duration::from_millis(200);
    let result = no_key_round(8148964, "herdr", Some(hold)).await;
    assert_eq!(result, Completion::NotSent("fixture refusal".into()));
}

/// A no-key turn whose record stays busy past the retries frees its channel on either host, and a
/// restart before the deferred record lands settles it at boot from the owner's retained proof.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_no_key_turn_frees_the_channel_when_its_record_stays_busy() {
    for (channel, host) in [(8148965, "herdr"), (8148966, "tmux")] {
        let result = no_key_round(channel, host, Some(WAIT_CAP)).await;
        assert!(
            matches!(&result, Completion::NotSent(r) if r.contains("busy")),
            "{result:?}"
        );
    }
}

/// Runs one unkeyed native turn on `host` with the registry lock held from the leaf's end for
/// `hold` (`WAIT_CAP`: until released here), and checks the next turn starts under that lock.
async fn no_key_round(channel: u64, host: &str, hold: Option<std::time::Duration>) -> Completion {
    let guard = crate::config::TestRuntimeRootGuard::new();
    let _binding_root = native_prepare::BindingRoot::enter(Some(guard.path()));
    let root = guard.path().join("runtime");
    std::fs::create_dir_all(&root).unwrap();
    let id = 8_000_000_000_000_000_000 + channel;
    let channel = ChannelId::new(channel);
    let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    std::sync::Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = 1;
    let token = std::sync::Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        "no-key".into(),
    )));
    shared
        .restart
        .global_active
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let mailbox = shared.mailbox(channel);
    mailbox
        .restore_active_turn(
            token.clone(),
            poise::serenity_prelude::UserId::new(7),
            poise::serenity_prelude::MessageId::new(id),
        )
        .await;
    let key = TurnKey::new(channel, id, 1).with_episode_nonce(token.turn_nonce());
    shared.turn_finalizer.register_start(
        key,
        ProviderKind::Claude,
        RelayOwnerKind::Watcher,
        &shared,
    );
    let persistence = crate::services::turn_orchestrator::QueuePersistenceContext::new(
        &ProviderKind::Claude,
        &shared.token_hash,
        None,
    );
    let (queue, namespace) =
        crate::services::turn_orchestrator::v2_queue(&persistence, channel).unwrap();
    mailbox.v2_adopt(queue.clone(), namespace).await.unwrap();
    let item=serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":channel.get(),"source_message_ids":[id],"queued_generation":1,"text":"our input"})).unwrap();
    let _ = admit(&mailbox, &item, AdmissionTicket::new()).await;
    let producer = Producer::at(shared.clone(), key, Some(token.clone()));
    let intent = producer.intent.clone();
    let mut prepared = PreparedPayload {
        frame: "our input".into(),
        sources: vec![id.to_string()],
        attempt: cold_attempt(&root, channel.get()).0,
    };
    prepared.attempt["target"]["host"] = json!(host);
    let (lock, (released, done)) = (
        root.join("injected_frames.lock"),
        std::sync::mpsc::channel(),
    );
    let (release, release_rx) = std::sync::mpsc::channel::<()>();
    let leaf: effect::Leaf = Box::new(move |_| {
        assert_eq!(scope::key_attempted(), Some(false));
        if let Some(hold) = hold {
            let mut file = std::fs::File::options();
            let file = file.create(true).truncate(false).write(true);
            let file = file.open(&lock).unwrap();
            file.lock().unwrap();
            std::thread::spawn(move || {
                let _ = release_rx.recv_timeout(hold);
                drop(file);
                released.send(()).unwrap();
            });
        }
        scope::vetoed("fixture refusal");
        Ok(Completion::Held("fixture refusal".into()))
    });
    let result = await_within(
        pump::submit_native_front(
            &mailbox,
            None,
            prepared,
            intent,
            std::sync::Arc::new(|| 10_000),
            producer.wrap(leaf),
        ),
        WAIT_CAP,
    )
    .await
    .unwrap();
    assert!(mailbox.snapshot().await.cancel_token.is_none());
    assert_eq!(
        shared
            .restart
            .global_active
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(token.is_completion_cleanup());
    assert_eq!(
        shared.turn_finalizer.native_scope(key, &token).await,
        Ok(false)
    );
    let snapshot = mailbox
        .v2_request(|s| s.as_ref().unwrap().json())
        .await
        .unwrap();
    assert!(snapshot.to_string().contains("our input"));
    let next = std::sync::Arc::new(CancelToken::new());
    let user = poise::serenity_prelude::UserId::new(7);
    assert!(mailbox.try_start_turn(next, user, (id + 1).into()).await);
    let frames = root.join("injected_frames.json");
    let crashed = (hold == Some(WAIT_CAP)).then(|| {
        (
            std::fs::read(&frames).unwrap(),
            std::fs::read(&queue).unwrap(),
        )
    });
    release.send(()).ok();
    let history = Registry::runtime().unwrap().json().unwrap();
    let settlements: Vec<_> = ["planned", "live", "passive"]
        .into_iter()
        .flat_map(|section| history[section].as_array().into_iter().flatten())
        .map(|record| {
            if record["registration"].is_object() {
                &record["registration"]
            } else {
                record
            }
        })
        .map(|registration| &registration["native_settlement"])
        .filter(|settlement| settlement["ticket"]["channel_id"] == channel.get())
        .collect();
    assert_eq!(settlements.len(), 1);
    for settlement in settlements {
        assert!(settlement["outcome"].is_null());
        assert_ne!(settlement["any_key"], true);
    }
    if hold.is_some() {
        done.recv_timeout(WAIT_CAP).unwrap();
    }
    if let Some((frames_at_crash, queue_at_crash)) = crashed {
        // In process the deferred record and source ACK land once the lock frees.
        await_within(
            async {
                while !(Registry::runtime()
                    .unwrap()
                    .json()
                    .unwrap()
                    .to_string()
                    .contains("\"not_sent\":true")
                    && !mailbox
                        .v2_request(|s| s.as_ref().unwrap().json())
                        .await
                        .unwrap()
                        .to_string()
                        .contains("\"pin\""))
                {
                    tokio::task::yield_now().await;
                }
            },
            WAIT_CAP,
        )
        .await;
        // A process that died before that leaves these bytes; the next generation boots on them.
        std::fs::write(&frames, frames_at_crash).unwrap();
        std::fs::write(&queue, &queue_at_crash).unwrap();
        let mut next = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        std::sync::Arc::get_mut(&mut next)
            .unwrap()
            .restart
            .current_generation = 2;
        let plan = native_prepare::BootInputs::prepare(&root, 2).unwrap();
        plan.restore(&next).await.unwrap();
        assert!(native_prepare::boot_ready(&next, channel).await);
        assert_eq!(
            std::fs::read(&queue).unwrap(),
            queue_at_crash,
            "the retained original and its notice stay"
        );
    }
    result
}

#[cfg(target_os = "macos")]
#[path = "tests/activation_e2e_tests.rs"]
mod activation_e2e;

#[cfg(target_os = "macos")]
async fn activation_units(
    store: &crate::services::tui_o::store::OStore,
    posts: &crate::services::tui_o::writer::host::test_io::Posts,
    channel: u64,
    bodies: &[String],
    turns: &[String],
) {
    use crate::services::tui_o::{
        shadow::{UnitKey, UnitKind},
        store::ledger::PieceOutcome,
        writer::DiscordPort,
    };
    let era = store.read_era().unwrap().unwrap();
    let body_key = |unit: &String| UnitKey {
        channel_id: channel,
        provider: crate::services::tui_o::shadow::ShadowProvider::Claude,
        native_key: format!("{unit}:0"),
        kind: UnitKind::Body,
    };
    let footer_key = |unit: &String| UnitKey {
        channel_id: channel,
        provider: crate::services::tui_o::shadow::ShadowProvider::Claude,
        native_key: format!("{unit}:0#turn_end"),
        kind: UnitKind::Tool,
    };
    let keys: Vec<_> = bodies
        .iter()
        .map(body_key)
        .chain(turns.iter().map(footer_key))
        .collect();
    await_within(async {loop {
        let c=store.open_channel(&era,channel).unwrap().unwrap();
        let complete=keys.iter().all(|key|matches!(c.ledger().latest_piece(key,0),Some((_,p)) if matches!(p.outcome,Some(PieceOutcome::Posted(_)))));
        drop(c);
        if complete {break;}
        tokio::task::yield_now().await;
    }},WAIT_CAP).await;
    let c = store.open_channel(&era, channel).unwrap().unwrap();
    let ledger = c.ledger();
    assert_eq!(
        ledger.next_serial() as usize,
        keys.len(),
        "no extra body or footer"
    );
    let seen = posts.history_after(channel, 0).await.unwrap();
    assert_eq!(seen.len(), keys.len(), "no duplicate or unowned post");
    for key in keys {
        let pieces: Vec<_> = (0..ledger.next_serial())
            .filter_map(|i| ledger.piece(i))
            .filter(|p| p.unit_key == key)
            .collect();
        assert_eq!(pieces.len(), 1, "canonical unit {key:?}");
        let Some(PieceOutcome::Posted(id)) = pieces[0].outcome else {
            panic!("unit not posted");
        };
        assert_eq!(
            seen.iter().filter(|m| m.id == id).count(),
            1,
            "unit delivery {key:?}"
        );
    }
    for body in bodies.iter().filter(|body| !turns.contains(body)) {
        assert!(
            ledger.latest_piece(&footer_key(body), 0).is_none(),
            "a mid-turn assistant must not acquire a separate footer"
        );
    }
    println!(
        "ISOLATED_UNITS bodies={} turns={} delivery=1 each",
        bodies.len(),
        turns.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_8148_activation_prekey_cutoff_keeps_a_rebound_epoch() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    std::sync::Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = 1;
    let channel = ChannelId::new(81489952);
    let id = 81489953;
    let actor = std::sync::Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        "cutoff-rebind".into(),
    )));
    let mailbox = shared.mailbox(channel);
    mailbox
        .restore_active_turn(
            actor.clone(),
            poise::serenity_prelude::UserId::new(7),
            poise::serenity_prelude::MessageId::new(id),
        )
        .await;
    shared
        .restart
        .global_active
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let key = TurnKey::new(channel, id, 1).with_episode_nonce(actor.turn_nonce());
    let producer = Producer::at(shared.clone(), key, Some(actor.clone()));
    let old=crate::services::discord::mailbox_finish::mailbox_finish_turn_if_matches_episode_started_before_with_actor_without_completion(&shared,&ProviderKind::Claude,channel,id.into(),actor.turn_nonce().map(str::to_owned),std::time::Instant::now(),Some(actor.clone())).await;
    assert!(old.removed_token.is_some());
    mailbox
        .restore_active_turn(
            actor.clone(),
            poise::serenity_prelude::UserId::new(7),
            poise::serenity_prelude::MessageId::new(id),
        )
        .await;
    await_within(
        tokio::task::spawn_blocking(move || producer.unwind()),
        WAIT_CAP,
    )
    .await
    .unwrap();
    assert!(std::sync::Arc::ptr_eq(
        mailbox.snapshot().await.cancel_token.as_ref().unwrap(),
        &actor
    ));
    assert_eq!(
        shared
            .restart
            .global_active
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}
