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
    let ticket = Arc::new(std::sync::Mutex::new(None));
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
        let leaf = cold_claude(
            name.into(),
            transcript.display().to_string(),
            0,
            ticket.clone(),
        );
        pump::submit(&mailbox, prepared, Arc::new(|| 10_000), leaf).await
    });
    assert_eq!(completion, Ok(Completion::Submitted));
    assert!(
        ticket.lock().unwrap().is_some(),
        "the observer keeps the submit's ticket"
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

/// A scripted busy Claude pane with `composer` typed in it and `chip` shown in its box instead.
fn drafted_pane(composer: &str, chip: Option<&str>) -> tempfile::TempDir {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/busy-inject-tmp");
    std::fs::create_dir_all(&root).unwrap();
    let dir = tempfile::tempdir_in(root).unwrap();
    let path = |name: &str| dir.path().join(name);
    std::fs::write(
        path("transcript.jsonl"),
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n",
    )
    .unwrap();
    let script = busy_inject::FAKE_STASH_TUI
        .replace("@D@", &dir.path().display().to_string())
        .replace("@T@", &path("transcript.jsonl").display().to_string());
    std::fs::write(path("tmux"), script).unwrap();
    let mode = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    std::fs::set_permissions(path("tmux"), mode).unwrap();
    let border = format!("{}\n", "─".repeat(60));
    for (name, value) in [
        ("attached", "0"),
        ("last", "100"),
        ("width", "160"),
        ("height", "50"),
        (
            "head",
            "⏺ Working.\n\n✻ Thinking… (3s · esc to interrupt)\n",
        ),
        ("border", border.as_str()),
        ("prompt", "❯\u{00a0}"),
        ("footer", "  ⏵⏵ bypass permissions on\n"),
        ("composer", composer),
    ] {
        std::fs::write(path(name), value).unwrap();
    }
    if let Some(chip) = chip {
        std::fs::write(path("chip"), chip).unwrap();
    }
    dir
}

/// A draft beside an image chip, which C-s cannot be proven to keep, gets one paste, one Enter, no
/// C-s and one notice for its original; a parkable draft is parked and owes none.
#[test]
fn an_owned_paste_beside_an_unparkable_draft_notices_its_original_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    const DRAFT: &str = "half typed private words";
    let chip = format!("{DRAFT} [Image #1]");
    for (at, chip, keys, notices) in [
        (1, Some(chip.as_str()), vec!["paste", "Enter"], 1),
        (2, None, vec!["C-s", "paste", "Enter"], 0),
    ] {
        let dir = drafted_pane(DRAFT, chip);
        let session = format!("owned-draft-{at}");
        let raised = Arc::new(std::sync::Mutex::new(0));
        let counted = raised.clone();
        let nonce = busy_inject::fresh_nonce();
        let leaf = BusyClaude {
            pane: busy_inject::Pane::with_program(
                &session,
                crate::services::platform::tmux::session_identity_tests::managed_fixture_program(
                    dir.path().join("tmux"),
                ),
            )
            .expect("managed scripted pane"),
            session: session.clone(),
            transcript: dir.path().join("transcript.jsonl"),
            channel: 7_676_400 + at,
            source: "discord".into(),
            author: "200".into(),
            name: "ann".into(),
            nonce: nonce.clone(),
            text: "owned question".into(),
            timing: &busy_inject::TIMING,
            declared: false,
            draft_notice: Some(Box::new(move || *counted.lock().unwrap() += 1)),
            cancel: Arc::new(crate::services::provider::CancelToken::new()),
        }
        .leaf();
        let frame = busy_inject::frame("discord", "200", "ann", &nonce, "owned question");
        assert_eq!(leaf(frame).unwrap(), Completion::Submitted, "{chip:?}");
        let applied = std::fs::read_to_string(dir.path().join("applied")).unwrap();
        assert_eq!(applied.lines().collect::<Vec<_>>(), keys, "{chip:?}");
        assert_eq!(*raised.lock().unwrap(), notices, "{chip:?}");
    }
}
