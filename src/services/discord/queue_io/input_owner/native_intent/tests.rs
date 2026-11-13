use super::*;
use crate::services::discord::queue_io::input_owner::{
    admission::{Admission, AdmissionTicket, admit},
    effect::{Completion, PreparedPayload},
    pump,
};
use crate::services::tui_input::human_frame::history::v2::native::Episode;
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, PendingQueueItem};
use poise::serenity_prelude::ChannelId;
use serde_json::json;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_8148_b1_prekey_primary_and_busy_follower_intent_survive_unknown_restore() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let base = _root.path().join("runtime");
    std::fs::create_dir_all(&base).unwrap();
    let dir = tempfile::tempdir_in(base).unwrap();
    for primary in [true, false] {
        let channel = if primary { 814812 } else { 814813 };
        let id = 8000000000000000000_u64 + channel;
        let item:PendingQueueItem=serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":channel,"source_message_ids":[id],"queued_generation":1,"text":"our input"})).unwrap();
        let registry = ChannelMailboxRegistry::default();
        let mailbox = registry.handle(ChannelId::new(channel));
        let path = dir.path().join(format!("{channel}.json"));
        mailbox
            .v2_adopt(path.clone(), "native-intent".into())
            .await
            .unwrap();
        assert!(matches!(
            admit(&mailbox, &item, AdmissionTicket::new()).await,
            Admission::Accepted(_)
        ));
        let front = mailbox
            .v2_request(|slot| {
                let owner = slot.as_mut().unwrap();
                let front = owner.front().unwrap();
                owner.grant_front(&front)?;
                Ok(front)
            })
            .await
            .unwrap();
        let claim = crate::services::discord::queue_io::input_owner::drive::claim_turn(
            &mailbox, id, &front,
        )
        .await
        .unwrap()
        .unwrap();
        let episode = Episode {
            user_message_id: id,
            turn_nonce: "primary-nonce".into(),
            generation: 1,
            extra: Default::default(),
        };
        let intent = if primary {
            Intent::Primary(episode.clone())
        } else {
            Intent::Follower
        };
        let expected = primary.then_some(episode);
        let inside = mailbox.clone();
        let original_path = path.clone();
        let transcript = dir.path().join(format!("native-{channel}.jsonl"));
        std::fs::write(&transcript, "").unwrap();
        let source_file = crate::services::tui_o::shadow::capture::file_identity(
            &std::fs::metadata(&transcript).unwrap(),
        );
        let history = crate::services::tui_input::human_frame::history::v2::Registry::at(
            dir.path().join(format!("frames-{channel}.json")),
        );
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        let prepared = PreparedPayload {
            frame: "our input".into(),
            sources: vec![id.to_string()],
            attempt: json!({
                "nonce": format!("attempt-{channel}"), "provider": "claude",
                "target": {"host":"herdr", "owner":format!("pane-{channel}"),
                    "root":"/native-intent", "execution_nonce":"birth"},
                "source": {"kind":"pre_source", "expected_session_id":null,
                    "expected_transcript":null, "binding_watermark":null}
            }),
        };
        let leaf = Box::new(move |_: String| {
            let observed =
                tokio::runtime::Handle::current().block_on(inside.v2_request(move |slot| {
                    let owner = slot.as_mut().unwrap();
                    let before = owner.native_intent()?;
                    owner.prepare_history(&history)?;
                    let key = owner.active_attempt()?;
                    let encoded = serde_json::to_value(&key).unwrap();
                    let source = crate::services::tui_input::human_frame::history::v2::Source {
                        target: encoded["attempt"]["target"].clone(),
                        provider: "claude".into(),
                        session_id: "native".into(),
                        transcript,
                        file: [source_file.0, source_file.1],
                        offset: 0,
                        binding_watermark: None,
                    };
                    owner.bind_source(&key, &source, &history)?;
                    assert_eq!(
                        owner.native_intent()?,
                        before,
                        "RuntimeReady preserves authority"
                    );
                    assert_eq!(
                        history.json()?["version"],
                        2,
                        "B1 does not activate native ticket writes"
                    );
                    Ok(before)
                }));
            assert_eq!(observed, Ok(expected));
            let disk: serde_json::Value =
                serde_json::from_slice(&std::fs::read(original_path).unwrap()).unwrap();
            assert!(!disk["head_injection"]["payload"]["attribution"]["native_intent"].is_null());
            sent.send(()).unwrap();
            Ok(Completion::WriteUncertain("after first key".into()))
        });
        let result = pump::submit_native_front(
            &mailbox,
            Some(claim),
            prepared,
            intent,
            Arc::new(|| 10_000),
            leaf,
        )
        .await;
        assert_eq!(
            result,
            Ok(Completion::WriteUncertain("after first key".into()))
        );
        assert_eq!(received.try_recv(), Ok(()));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("native_intent"));
        let next_registry = ChannelMailboxRegistry::default();
        let reopened = next_registry.handle(ChannelId::new(channel));
        reopened
            .v2_adopt(path, "native-intent".into())
            .await
            .unwrap();
        let present = reopened
            .v2_request(|slot| Ok(slot.as_ref().unwrap().native_intent()?.is_some()))
            .await
            .unwrap();
        assert_eq!(present, primary);
        let empty = reopened
            .v2_request(|slot| {
                Ok(slot.as_ref().unwrap().json()?["terminal_identities"]
                    .as_array()
                    .unwrap()
                    .is_empty())
            })
            .await
            .unwrap();
        assert!(empty);
    }
}

#[test]
fn native_8148_b1_primary_pin_refuses_other_original_or_missing_episode() {
    let episode = Episode {
        user_message_id: 7,
        turn_nonce: "nonce".into(),
        generation: 1,
        extra: Default::default(),
    };
    assert!(Intent::Primary(episode.clone()).episode(8).is_err());
    let mut empty = episode.clone();
    empty.turn_nonce.clear();
    assert!(Intent::Primary(empty).episode(7).is_err());
    let mut old = episode;
    old.generation = 0;
    assert!(Intent::Primary(old).episode(7).is_err());
    assert_eq!(Intent::Follower.episode(7), Ok(None));
    assert_ne!(
        Intent::Follower.attribution(&[])["native_intent"],
        serde_json::Value::Null
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_8148_b1_invalid_primary_is_not_sent_and_boot_keeps_the_original() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let base = _root.path().join("runtime");
    std::fs::create_dir_all(&base).unwrap();
    let dir = tempfile::tempdir_in(base).unwrap();
    let registry = ChannelMailboxRegistry::default();
    let mailbox = registry.handle(ChannelId::new(814814));
    let path = dir.path().join("held.json");
    mailbox
        .v2_adopt(path.clone(), "native-invalid".into())
        .await
        .unwrap();
    let id = 8000000000000814814_u64;
    let item: PendingQueueItem = serde_json::from_value(json!({
        "author_id":7, "message_id":id, "channel_id":814814,
        "source_message_ids":[id], "queued_generation":1, "text":"original"
    }))
    .unwrap();
    assert!(matches!(
        admit(&mailbox, &item, AdmissionTicket::new()).await,
        Admission::Accepted(_)
    ));
    let prepared = PreparedPayload {
        frame: "original".into(),
        sources: vec![id.to_string()],
        attempt: json!({"nonce":"invalid-primary", "provider":"claude",
            "target":{"host":"herdr", "owner":"invalid-primary", "root":"/fixture", "execution_nonce":"birth"},
            "source":{"kind":"pre_source", "expected_session_id":null, "expected_transcript":null, "binding_watermark":null}}),
    };
    let intent = Intent::Primary(Episode {
        user_message_id: id + 1,
        turn_nonce: "foreign".into(),
        generation: 1,
        extra: Default::default(),
    });
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let result = pump::submit_native_front(
        &mailbox,
        None,
        prepared,
        intent,
        Arc::new(|| 10_000),
        Box::new(move |_| {
            sent.send(()).unwrap();
            Ok(Completion::Submitted)
        }),
    )
    .await;
    assert!(
        matches!(result,Err(ref error) if error.starts_with("NotSent(")),
        "{result:?}"
    );
    assert!(received.try_recv().is_err(), "the first-key leaf never ran");
    let next = ChannelMailboxRegistry::default();
    let restored = next.handle(ChannelId::new(814814));
    restored
        .v2_adopt(path, "native-invalid".into())
        .await
        .unwrap();
    let front = restored
        .v2_request(|slot| Ok(slot.as_ref().unwrap().front()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(front.1.message_id, id);
    assert_eq!(front.1.text, "original");
}
