use std::sync::atomic::Ordering;

use poise::serenity_prelude as serenity;
use serde_json::json;

use super::super::stop_host::tests::{Fixture, Mark, bound_token, mark, run};
use super::*;
use crate::services::discord::{Data, TmuxWatcherHandle};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::file_identity;
use crate::services::tui_o::turn_mode::TestConfirmation;
use crate::services::tui_prompt_dedupe::binding_events as p5;

use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;

fn bind(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    session: &str,
    path: &std::path::Path,
) {
    shared.tmux_watchers.insert(
        channel,
        TmuxWatcherHandle {
            tmux_session_name: session.into(),
            output_path: path.display().to_string(),
            paused: Arc::new(false.into()),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(false.into()),
            pause_epoch: Arc::new(0.into()),
            turn_delivered: Arc::new(false.into()),
            last_heartbeat_ts_ms: Arc::new(chrono::Utc::now().timestamp_millis().into()),
        },
    );
    let (dev, ino) = file_identity(&std::fs::metadata(path).unwrap());
    let event = p5::BindingEvent {
        seq: 1,
        channel_id: channel.get(),
        provider: provider.as_str().into(),
        tmux_session: session.into(),
        execution_nonce: Some("session-stop-test".into()),
        old: None,
        new: p5::BindingTarget::Source(SourceId {
            session_id: "parent".into(),
            path: path.into(),
            dev,
            ino,
        }),
        cause: p5::BindingCause::Startup,
        parent_hint: None,
        evidence: p5::BindingEvidence {
            hook_event: Some("SessionStart".into()),
            received_at: chrono::Utc::now(),
        },
        committed_at: chrono::Utc::now(),
    };
    let root = p5::test_root().unwrap().join(p5::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join(format!("{}.log", channel.get())),
        format!("{}\n", serde_json::to_string(&event).unwrap()),
    )
    .unwrap();
}

fn open(path: &std::path::Path, id: &str) {
    let user = json!({"type":"user","uuid":id,"message":{"content":"direct parent turn"}});
    std::fs::write(path, format!("{user}\n")).unwrap();
}

async fn command(ctx: &serenity::Context, data: &Data, channel: ChannelId) {
    let mut message = serenity::Message::default();
    message.id = serenity::MessageId::new(channel.get() + 1);
    message.channel_id = channel;
    message.author.id = serenity::UserId::new(7);
    message.author.name = "stop-test".into();
    message.content = "!stop".into();
    let handled = crate::services::discord::commands::handle_text_command_with_uploads(
        ctx,
        &message,
        data,
        channel,
        "!stop",
        &[],
        &mut None,
    )
    .await
    .unwrap();
    assert!(handled);
}

fn escapes(calls: &[String]) -> Vec<&str> {
    calls
        .iter()
        .filter(|line| line.starts_with("send-keys "))
        .map(String::as_str)
        .collect()
}

#[test]
fn n1b_actual_stop_targets_open_parent_without_lease_and_keeps_idle_or_refused() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::n1b_actual_stop_targets_open_parent_without_lease_and_keeps_idle_or_refused"
    )) {
        return;
    }
    let fx = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let _binding_root = TestBindingRoot::enter(Some(root.path()));
    run(async {
        let mock = discord_mock::DiscordMockState::new();
        let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
        let ctx = discord_mock::serenity_context(proxy, gateway).await;
        let shared = crate::services::discord::make_shared_data_for_tests();
        shared.settings.write().await.owner_user_id = Some(7);
        let mut voice_config = crate::voice::VoiceConfig::default();
        voice_config.keep_recordings = true;
        voice_config.audio.recordings_dir = root.path().join("recordings");
        let mut data = Data {
            shared: shared.clone(),
            token: "test-token".into(),
            provider: ProviderKind::Claude,
            voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
            voice_config,
        };
        let channel = ChannelId::new(discord_mock::CHANNEL_ID);
        let session = "n1b-direct-parent";
        let path = root.path().join("parent.jsonl");
        open(&path, "first");
        bind(&shared, &data.provider, channel, session, &path);
        let _confirmed = TestConfirmation::new(channel.get());
        assert!(shared.mailbox_peek(channel).is_none());
        let generation_owner = bound_token(&ProviderKind::Claude, session);
        let generation = generation_owner.claude_interrupt_generation();
        command(&ctx, &data, channel).await;
        let calls = fx.take_calls();
        let writes = escapes(&calls);
        assert_eq!(
            writes.len(),
            1,
            "open parent must receive exactly one interrupt: {calls:?}"
        );
        assert!(
            writes[0].contains(session),
            "interrupt must target the bound session"
        );
        assert!(writes[0].contains("Escape"));
        assert_eq!(generation_owner.claude_interrupt_generation(), generation);
        assert!(
            generation_owner
                .lock_current_claude_interrupt_session(session)
                .is_some(),
            "session stop cannot replace managed generation authority"
        );
        assert!(
            shared.mailbox_peek(channel).is_none(),
            "session stop cannot create a mailbox"
        );
        assert!(crate::services::discord::tmux::recent_turn_stop_for_channel(channel).is_none());
        assert!(
            crate::services::discord::inflight::load_inflight_state_read_only(
                &ProviderKind::Claude,
                channel.get()
            )
            .is_none()
        );

        use std::io::Write;
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
            "{}",
            json!({"type":"system","subtype":"turn_duration"})
        )
        .unwrap();
        command(&ctx, &data, channel).await;
        assert!(fx.take_calls().is_empty(), "idle must not even probe tmux");
        open(&path, "second");
        for marker in [Mark::Herdr, Mark::Process, Mark::Zellij, Mark::Unreadable] {
            mark(session, marker);
            command(&ctx, &data, channel).await;
            assert!(
                fx.take_calls().is_empty(),
                "refused host must receive no provider I/O"
            );
            assert!(shared.mailbox_peek(channel).is_none());
            assert!(
                crate::services::discord::tmux::recent_turn_stop_for_channel(channel).is_none()
            );
        }
        mark(session, Mark::Absent);
        let judged = SessionStop::judge(&shared, &ProviderKind::Claude, channel).await;
        let CommandStop::Session(stop) = judged else {
            panic!("open parent must be judged");
        };
        open(&path, "replacement");
        assert!(
            !stop.interrupt("!stop").await.sent_keys,
            "a later parent turn is not the judged turn"
        );
        assert!(fx.take_calls().is_empty());

        let log = root
            .path()
            .join(p5::BINDING_EVENTS_DIR)
            .join(format!("{}.log", channel.get()));
        let good_log = std::fs::read(&log).unwrap();
        let first: p5::BindingEvent = serde_json::from_slice(&good_log).unwrap();
        let mut pending = first.clone();
        pending.seq = 2;
        pending.new = p5::BindingTarget::Pending {
            payload_session_id: "pending".into(),
            payload_transcript_path: None,
        };
        writeln!(
            std::fs::OpenOptions::new().append(true).open(&log).unwrap(),
            "{}",
            serde_json::to_string(&pending).unwrap()
        )
        .unwrap();
        command(&ctx, &data, channel).await;
        assert!(
            fx.take_calls().is_empty(),
            "pending binding cannot interrupt the previous source"
        );
        std::fs::write(&log, good_log).unwrap();
        std::fs::write(&path, format!("{}\n", json!({"type":"user","uuid":"child","isSidechain":true,"message":{"content":"child prompt"}}))).unwrap();
        command(&ctx, &data, channel).await;
        assert!(
            fx.take_calls().is_empty(),
            "a child record cannot supply parent turn authority"
        );

        data.provider = ProviderKind::Codex;
        let codex_session = "n1b-codex-parent";
        let codex_path = root.path().join("codex-parent.jsonl");
        let codex_open =
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"codex-parent"}});
        std::fs::write(&codex_path, format!("{codex_open}\n")).unwrap();
        bind(&shared, &data.provider, channel, codex_session, &codex_path);
        fx.ready(codex_session);
        command(&ctx, &data, channel).await;
        let calls = fx.take_calls();
        let writes = escapes(&calls);
        assert_eq!(
            writes.len(),
            1,
            "Codex open parent must receive one interrupt: {calls:?}"
        );
        assert!(writes[0].contains(codex_session));
        assert!(writes[0].contains("Escape"));
        assert!(shared.mailbox_peek(channel).is_none());
        assert!(crate::services::discord::tmux::recent_turn_stop_for_channel(channel).is_none());
        let codex_idle =
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"codex-parent"}});
        std::fs::write(&codex_path, format!("{codex_open}\n{codex_idle}\n")).unwrap();
        command(&ctx, &data, channel).await;
        assert!(
            fx.take_calls().is_empty(),
            "Codex idle cannot probe or interrupt"
        );
        data.provider = ProviderKind::Claude;
        bind(&shared, &data.provider, channel, session, &path);

        drop(_confirmed);
        let log = root
            .path()
            .join(p5::BINDING_EVENTS_DIR)
            .join(format!("{}.log", channel.get()));
        std::fs::write(log, "corrupt O binding log\n").unwrap();
        let token = bound_token(&ProviderKind::Claude, session);
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                channel,
                token.clone(),
                serenity::UserId::new(7),
                serenity::MessageId::new(channel.get() + 2)
            )
            .await
        );
        command(&ctx, &data, channel).await;
        assert!(
            token.cancelled.load(Ordering::Acquire),
            "unconfirmed stop must retain the legacy cancel effect even with an unreadable O log"
        );
        assert!(crate::services::discord::tmux::recent_turn_stop_for_channel(channel).is_some());
        assert!(mock.unhandled.lock().unwrap().is_empty());
        assert_eq!(
            mock.local_note_posts.load(Ordering::Acquire),
            8,
            "two idle turns, four refusals, pending and child must reply"
        );
        server.abort();
    });
}
