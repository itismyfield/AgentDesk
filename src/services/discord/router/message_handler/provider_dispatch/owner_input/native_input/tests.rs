use super::*;
use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::discord::queue_io::input_owner::admission::{AdmissionTicket, admit};
use crate::services::tui_input::human_frame::history::v2::Registry;
use crate::test_support::wait::{WAIT_CAP, await_within};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_8148_b2b2_created_primary_and_follower_ack_before_key() {
    let guard = crate::config::TestRuntimeRootGuard::new();
    let root = guard.path().join("runtime");
    std::fs::create_dir_all(&root).unwrap();
    for (primary, stale) in [(true, false), (false, false), (true, true)] {
        let channel = ChannelId::new(if stale {
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
        mailbox
            .restore_active_turn(
                token.clone(),
                poise::serenity_prelude::UserId::new(7),
                poise::serenity_prelude::MessageId::new(if primary { id } else { id + 100 }),
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
                "native-production".into(),
            )
            .await
            .unwrap();
        let item = serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":channel.get(),"source_message_ids":[id],"queued_generation":1,"text":"our input"})).unwrap();
        let _ = admit(&mailbox, &item, AdmissionTicket::new()).await;
        let transcript = root
            .as_path()
            .join(format!("source-{}.jsonl", channel.get()));
        std::fs::write(&transcript, "").unwrap();
        let meta = std::fs::metadata(&transcript).unwrap();
        let file = crate::services::tui_o::shadow::capture::file_identity(&meta);
        let shared_leaf = shared.clone();
        let mailbox_leaf = mailbox.clone();
        let token_leaf = token.clone();
        let leaf: effect::Leaf = Box::new(move |_| {
            assert!(
                !stale,
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
                        session_id: "native-created".into(),
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
            attempt: json!({"nonce":format!("native-{}",channel.get()),"provider":"claude","target":{"host":"herdr","owner":"fixture-pane","root":"/fixture","execution_nonce":"created-birth"},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}}),
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
        if stale {
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
                &token
            ));
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
    let guard = crate::config::TestRuntimeRootGuard::new();
    let root = guard.path().join("runtime");
    std::fs::create_dir_all(&root).unwrap();
    let channel = ChannelId::new(8148963);
    let id = 8_000_000_000_000_814_963;
    let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    std::sync::Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = 1;
    let token = std::sync::Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        "no-key".into(),
    )));
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
    mailbox
        .v2_adopt(
            root.as_path().join("no-key-owner.json"),
            "native-no-key".into(),
        )
        .await
        .unwrap();
    let item=serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":channel.get(),"source_message_ids":[id],"queued_generation":1,"text":"our input"})).unwrap();
    let _ = admit(&mailbox, &item, AdmissionTicket::new()).await;
    let producer = Producer::at(shared.clone(), key, Some(token.clone()));
    let intent = producer.intent.clone();
    let prepared = PreparedPayload {
        frame: "our input".into(),
        sources: vec![id.to_string()],
        attempt: json!({"nonce":"no-key-attempt","provider":"claude","target":{"host":"herdr","owner":"no-key-pane","root":"/fixture","execution_nonce":"no-key-birth"},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}}),
    };
    let leaf: effect::Leaf = Box::new(|_| {
        assert_eq!(scope::key_attempted(), Some(false));
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
    assert_eq!(result, Completion::NotSent("fixture refusal".into()));
    assert_eq!(
        shared.turn_finalizer.native_scope(key, &token).await,
        Ok(false)
    );
    let snapshot = mailbox
        .v2_request(|s| s.as_ref().unwrap().json())
        .await
        .unwrap();
    assert!(snapshot.to_string().contains("our input"));
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
}
