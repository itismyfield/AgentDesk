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
fn c1_stop_some_none_and_unreachable_preserve_exact_authority() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::c1_stop_some_none_and_unreachable_preserve_exact_authority"
    )) {
        return;
    }
    let _fx = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let _binding_root = TestBindingRoot::enter(Some(root.path()));
    run(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(6_325_443);
        let provider = ProviderKind::Claude;
        let session = "c1-stop-parent";
        let path = root.path().join("parent.jsonl");
        open(&path, "first");
        bind(&shared, &provider, channel, session, &path);
        let _confirmed = TestConfirmation::new(channel.get());
        let token = bound_token(&provider, session);
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                channel,
                token.clone(),
                serenity::UserId::new(7),
                serenity::MessageId::new(8),
            )
            .await
        );
        let stopped = super::super::begin_command_stop(&shared, &provider, channel, true).await;
        let CommandStop::Stop(stop) = stopped else {
            panic!("Some token must retain ChannelStop authority")
        };
        assert!(Arc::ptr_eq(stop.token(), &token));
        assert!(token.cancelled.load(Ordering::SeqCst));
        crate::services::discord::mailbox_finish_turn(&shared, &provider, channel).await;
        let mailbox = shared.mailbox_peek(channel).unwrap();
        assert!(mailbox.cancel_token().await.unwrap().is_none());
        let empty = super::super::begin_command_stop(&shared, &provider, channel, true).await;
        assert!(
            matches!(empty, CommandStop::Session(_)),
            "Ok(None) retains SessionStop fallback"
        );
        shared.mailboxes.remove_fixture_for_test(channel);
        shared.mailboxes.insert_unreachable_for_test(channel);
        let unreachable = super::super::begin_command_stop(&shared, &provider, channel, true).await;
        assert!(
            matches!(unreachable, CommandStop::HostRefused),
            "Err cannot borrow the parent SessionStop authority"
        );
        shared.mailboxes.remove_fixture_for_test(channel);
    });
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
        // Herdr without an executor-owned token remains refused even with Escape enabled.
        use crate::services::provider::cancel_token_claude_interrupt::HERDR_CANCEL_OVERRIDE;
        HERDR_CANCEL_OVERRIDE.set(Some(true));
        mark(session, Mark::Herdr);
        assert!(matches!(
            super::super::judged_stop::begin_command_stop(&shared, &data.provider, channel, true)
                .await,
            CommandStop::HostRefused
        ));
        assert!(fx.take_calls().is_empty());
        HERDR_CANCEL_OVERRIDE.set(None);
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
        let codex_meta = json!({"type":"session_meta","payload":{"id":"parent","cwd":root.path()}});
        std::fs::write(&codex_path, format!("{codex_meta}\n{codex_open}\n")).unwrap();
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
        std::fs::write(
            &codex_path,
            format!("{codex_meta}\n{codex_open}\n{codex_idle}\n"),
        )
        .unwrap();
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

#[test]
fn codex_parent_without_session_meta_refuses_stop_and_replies() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::codex_parent_without_session_meta_refuses_stop_and_replies"
    )) {
        return;
    }
    let fx = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let _binding_root = TestBindingRoot::enter(Some(root.path()));
    run(async {
        let mock = discord_mock::DiscordMockState::new();
        let (proxy, gateway, server) = discord_mock::start(mock).await;
        let mut ctx = discord_mock::serenity_context(proxy, gateway).await;
        let replies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let reply_state = replies.clone();
        let app = axum::Router::new().route(
            "/api/v10/channels/{channel}/messages",
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let replies = reply_state.clone();
                async move {
                    let content = body["content"].as_str().unwrap().to_owned();
                    replies.lock().unwrap().push(content.clone());
                    let mut message = serenity::Message::default();
                    message.id = serenity::MessageId::new(9001);
                    message.channel_id = ChannelId::new(discord_mock::CHANNEL_ID);
                    message.content = content;
                    axum::Json(message)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let reply_server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        ctx.http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy(format!("http://{address}"))
                .ratelimiter_disabled(true)
                .build(),
        );
        let shared = crate::services::discord::make_shared_data_for_tests();
        shared.settings.write().await.owner_user_id = Some(7);
        let mut voice_config = crate::voice::VoiceConfig::default();
        voice_config.keep_recordings = true;
        voice_config.audio.recordings_dir = root.path().join("recordings");
        let data = Data {
            shared: shared.clone(),
            token: "test-token".into(),
            provider: ProviderKind::Codex,
            voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
            voice_config,
        };
        let channel = ChannelId::new(discord_mock::CHANNEL_ID);
        let session = "codex-without-parent-header";
        let path = root.path().join("codex-no-meta.jsonl");
        let turn = json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"parent"}});
        std::fs::write(&path, format!("{turn}\n")).unwrap();
        bind(&shared, &data.provider, channel, session, &path);
        let _confirmed = TestConfirmation::new(channel.get());
        fx.ready(session);
        command(&ctx, &data, channel).await;
        assert!(
            fx.take_calls().is_empty(),
            "unknown parent must receive no provider I/O"
        );
        assert_eq!(
            replies.lock().unwrap().as_slice(),
            ["이 세션의 호스트를 확인하지 못해 중지하지 않았어요. 턴은 계속 진행돼요."],
            "a missing parent header must refuse visibly, not silently"
        );
        assert!(shared.mailbox_peek(channel).is_none());
        reply_server.abort();
        server.abort();
    });
}

#[test]
fn n1c_confirmed_stop_keeps_a_discord_token_on_the_channel_stop_and_refuses_an_unread_turn() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::n1c_confirmed_stop_keeps_a_discord_token_on_the_channel_stop_and_refuses_an_unread_turn"
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
        let data = Data {
            shared: shared.clone(),
            token: "test-token".into(),
            provider: ProviderKind::Claude,
            voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
            voice_config,
        };
        let channel = ChannelId::new(discord_mock::CHANNEL_ID);
        let session = "n1c-discord-turn";
        let path = root.path().join("parent.jsonl");
        open(&path, "discord");
        bind(&shared, &data.provider, channel, session, &path);
        let _confirmed = TestConfirmation::new(channel.get());

        let log = root
            .path()
            .join(p5::BINDING_EVENTS_DIR)
            .join(format!("{}.log", channel.get()));
        let good_log = std::fs::read(&log).unwrap();
        std::fs::write(&log, "corrupt O binding log\n").unwrap();
        let judged = super::super::begin_command_stop(&shared, &data.provider, channel, true).await;
        assert!(
            matches!(judged, CommandStop::HostRefused),
            "an unreadable binding is not proof the turn ended"
        );
        assert!(fx.take_calls().is_empty());
        std::fs::write(&log, good_log).unwrap();
        fx.ready(session);
        command(&ctx, &data, channel).await;
        assert!(
            escapes(&fx.take_calls()).is_empty(),
            "an open transcript turn does not override a pane ready for input"
        );
        fx.ready("n1c-no-ready-pane");

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
            "a confirmed channel's Discord turn keeps the channel stop's cancel"
        );
        assert!(crate::services::discord::tmux::recent_turn_stop_for_channel(channel).is_some());
        assert!(mock.unhandled.lock().unwrap().is_empty());
        server.abort();
    });
}

// The withdrawal rides the delivery-time recheck: neither the judgment nor a recheck that finds
// another parent turn withdraws the channel's approval.
#[test]
fn b2b1_a_session_stop_withdraws_at_its_delivery_recheck_not_at_its_judgment() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::b2b1_a_session_stop_withdraws_at_its_delivery_recheck_not_at_its_judgment"
    )) {
        return;
    }
    use crate::services::discord::turn_presence::entrypoints::tests::Probe;
    let fx = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let _binding_root = TestBindingRoot::enter(Some(root.path()));
    run(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let probe = Probe::install(&shared);
        let channel = ChannelId::new(6_325_445);
        let provider = ProviderKind::Claude;
        let session = "b2b1-session-stop-parent";
        let path = root.path().join("parent.jsonl");
        open(&path, "first");
        bind(&shared, &provider, channel, session, &path);
        let _confirmed = TestConfirmation::new(channel.get());
        mark(session, Mark::Absent);
        let ticket = probe.arm(channel.get());
        let CommandStop::Session(stop) = SessionStop::judge(&shared, &provider, channel).await
        else {
            panic!("open parent must be judged");
        };
        assert!(Probe::current(&ticket), "judging withdraws nothing");
        open(&path, "replacement");
        assert!(!stop.interrupt("!stop").await.sent_keys);
        assert!(
            Probe::current(&ticket),
            "a failed recheck withdraws nothing"
        );
        let CommandStop::Session(stop) = SessionStop::judge(&shared, &provider, channel).await
        else {
            panic!("the replacement parent must be judged");
        };
        assert!(stop.interrupt("!stop").await.sent_keys);
        assert!(!Probe::current(&ticket), "the delivered interrupt withdrew");
        let _ = fx.take_calls();
    });
}
