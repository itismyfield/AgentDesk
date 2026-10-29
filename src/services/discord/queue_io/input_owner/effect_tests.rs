use super::*;
use crate::services::claude_tui::input::submission::tests::{frame_renderer, screen};
use crate::services::discord::queue_io::input_owner::pump;
use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
use crate::services::turn_orchestrator::ChannelMailboxRegistry;
use serde_json::json;

/// A created Claude pane takes the owner's attempt as its first prompt: the frame once, one Enter,
/// no wait for a transcript, and the attempt settles delivered.
#[test]
fn cold_claude_first_prompt_is_one_owner_attempt() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let (name, channel) = ("owner-cold-claude", 7_272_521u64);
    let frame = "first + attachments\nsecond source";
    frame_renderer(&f, name, frame, &screen(""), &screen(frame), &screen(""));
    let nonce = "00112233445566778899aabbccdd7521";
    let root = crate::services::tmux_common::current_tmux_owner_marker();
    let context = json!({"schema":1,"provider":"claude","created_at":chrono::Utc::now(),
        "execution_nonce":nonce,"tmux_session":name,"channel_id":channel,
        "owner_runtime_root":root,"host":"owner-cold-test","expected_native_session_id":null,
        "launch_mode":"fresh","provider_root":null});
    let _prepared =
        crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(
            serde_json::from_value(context).unwrap(),
        )
        .unwrap();
    let marker = crate::services::tmux_common::session_temp_path(name, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, nonce).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let registry = ChannelMailboxRegistry::default();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    let id = 8_000_000_000_000_006_521u64;
    let original: crate::services::turn_orchestrator::PendingQueueItem =
        serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":channel,
            "source_message_ids":[id],"queued_generation":1,"text":frame}))
        .unwrap();
    let transcript = f.root.path().join("not-created.jsonl");
    let completion = runtime.block_on(async {
        let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
            .unwrap()
            .join("claude/input-owner-cold");
        std::fs::create_dir_all(&dir).unwrap();
        mailbox
            .v2_adopt(dir.join(format!("{channel}.json")), "input-owner".into())
            .await
            .unwrap();
        mailbox
            .v2_request(move |s| {
                s.as_mut()
                    .unwrap()
                    .enqueue_fixture(&original)
                    .map_err(|e| format!("{e:?}"))
            })
            .await
            .unwrap();
        let prepared = PreparedPayload {
            frame: frame.into(),
            sources: vec!["s1".into(), "s2".into()],
            attempt: json!({"nonce":nonce,"provider":"claude",
                "target":{"host":"tmux","owner":name,"root":root,"execution_nonce":nonce},
                "source":{"kind":"pre_source","expected_session_id":null,
                    "expected_transcript":null,"binding_watermark":null}}),
        };
        let leaf = cold_claude(name.into(), transcript.display().to_string(), 0);
        pump::submit(&mailbox, prepared, Arc::new(|| 10_000), leaf).await
    });
    assert_eq!(
        completion,
        Ok(Completion::Submitted {
            enter_resent: false
        })
    );
    assert!(
        !transcript.exists(),
        "a cold submit does not wait for a transcript"
    );
    let pane = f.root.path().join(name);
    assert_eq!(std::fs::read_to_string(pane.join("frame")).unwrap(), frame);
    assert_eq!(std::fs::read(pane.join("payloads")).unwrap().len(), 1);
    assert_eq!(std::fs::read(pane.join("enters")).unwrap().len(), 1);
    let snapshot = runtime
        .block_on(mailbox.v2_request(|s| s.as_ref().unwrap().json()))
        .unwrap();
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert!(
        snapshot["items"].as_array().unwrap().is_empty(),
        "{snapshot}"
    );
}
