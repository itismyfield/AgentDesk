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

fn bind_live(
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
    let log = binding_log(channel);
    let _ = std::fs::remove_file(log);
    write_binding_event(provider, channel, session, path, 1);
}

fn binding_log(channel: ChannelId) -> std::path::PathBuf {
    p5::test_root()
        .unwrap()
        .join(p5::BINDING_EVENTS_DIR)
        .join(format!("{}.log", channel.get()))
}

fn bind_owner_only(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    session: &str,
    path: &std::path::Path,
    seq: u64,
) {
    shared
        .tmux_watchers
        .restore_owner_channel_for_tmux_session(session, channel);
    write_binding_event(provider, channel, session, path, seq);
    assert!(
        shared.tmux_watchers.channel_binding(&channel).is_none(),
        "owner authority must not imply watcher coverage"
    );
}

fn write_binding_event(
    provider: &ProviderKind,
    channel: ChannelId,
    session: &str,
    path: &std::path::Path,
    seq: u64,
) {
    let (dev, ino) = file_identity(&std::fs::metadata(path).unwrap());
    let event = p5::BindingEvent {
        seq,
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
    use std::io::Write as _;
    writeln!(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(binding_log(channel))
            .unwrap(),
        "{}",
        serde_json::to_string(&event).unwrap()
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
        bind_live(&shared, &provider, channel, session, &path);
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
            ingress_shadow: None,
        };
        let channel = ChannelId::new(discord_mock::CHANNEL_ID);
        let session = "n1b-direct-parent";
        let path = root.path().join("parent.jsonl");
        open(&path, "first");
        bind_live(&shared, &data.provider, channel, session, &path);
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
        bind_live(&shared, &data.provider, channel, codex_session, &codex_path);
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
        bind_live(&shared, &data.provider, channel, session, &path);

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
            9,
            "delivered stop, two idle turns, four refusals, pending and child must reply"
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
            ingress_shadow: None,
        };
        let channel = ChannelId::new(discord_mock::CHANNEL_ID);
        let session = "codex-without-parent-header";
        let path = root.path().join("codex-no-meta.jsonl");
        let turn = json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"parent"}});
        std::fs::write(&path, format!("{turn}\n")).unwrap();
        bind_live(&shared, &data.provider, channel, session, &path);
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
            ingress_shadow: None,
        };
        let channel = ChannelId::new(discord_mock::CHANNEL_ID);
        let session = "n1c-discord-turn";
        let path = root.path().join("parent.jsonl");
        open(&path, "discord");
        bind_live(&shared, &data.provider, channel, session, &path);
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

#[derive(Clone, Copy)]
enum OwnerCase {
    Slash,
    CodexText,
    Multiple,
    ForeignProvider,
    Nonowner,
    Mirror,
    OwnerChanged,
    SourceChanged,
    InvalidBinding,
}

fn owner_stop_case(name: &str, case: OwnerCase) {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(name) {
        return;
    }
    let fx = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let _binding_root = TestBindingRoot::enter(Some(root.path()));
    run(async {
        use crate::services::discord::commands::{
            HOST_REFUSED_STOP_RESPONSE, NO_ACTIVE_TURN_RESPONSE, STOPPING_RESPONSE,
            stop::run_slash_stop,
        };
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(discord_mock::CHANNEL_ID);
        let provider = if matches!(case, OwnerCase::CodexText) {
            ProviderKind::Codex
        } else {
            ProviderKind::Claude
        };
        let _confirmed = TestConfirmation::new(channel.get());
        let session = "owner-only-a";
        let path = root.path().join("parent.jsonl");
        if matches!(case, OwnerCase::CodexText) {
            let meta = json!({"type":"session_meta","payload":{"id":"parent","cwd":root.path()}});
            let started =
                json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"parent"}});
            std::fs::write(&path, format!("{meta}\n{started}\n")).unwrap();
            fx.ready(session);
        } else {
            open(&path, "parent");
        }
        match case {
            OwnerCase::Nonowner => {
                shared.tmux_watchers.restore_owner_channel_for_tmux_session(
                    session,
                    ChannelId::new(channel.get() + 10),
                );
                shared
                    .tmux_watchers
                    .restore_owner_channel_for_tmux_session("untyped-owner", channel);
                write_binding_event(&provider, channel, session, &path, 1);
            }
            OwnerCase::Mirror => {
                crate::services::tui_prompt_dedupe::register_tmux_channel(session, channel.get());
                assert_eq!(
                    crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(session),
                    Some(channel.get())
                );
                assert_eq!(
                    shared.tmux_watchers.owner_channel_for_tmux_session(session),
                    None
                );
                write_binding_event(&provider, channel, session, &path, 1);
            }
            OwnerCase::ForeignProvider => {
                bind_owner_only(&shared, &ProviderKind::Codex, channel, session, &path, 1)
            }
            _ => bind_owner_only(&shared, &provider, channel, session, &path, 1),
        }
        if matches!(case, OwnerCase::Multiple) {
            let idle = root.path().join("idle.jsonl");
            std::fs::write(
                &idle,
                format!("{}\n", json!({"type":"system","subtype":"turn_duration"})),
            )
            .unwrap();
            bind_owner_only(&shared, &provider, channel, "owner-only-b", &idle, 2);
        }
        if matches!(case, OwnerCase::Slash) {
            // Old rows omitted the optional nonce; retain the existing reader's defaults.
            let mut row: serde_json::Value =
                serde_json::from_slice(&std::fs::read(binding_log(channel)).unwrap()).unwrap();
            row.as_object_mut().unwrap().remove("execution_nonce");
            std::fs::write(binding_log(channel), format!("{row}\n")).unwrap();
        }
        if matches!(case, OwnerCase::InvalidBinding) {
            let good = std::fs::read(binding_log(channel)).unwrap();
            let row: p5::BindingEvent = serde_json::from_slice(&good).unwrap();
            let mut pending = row.clone();
            pending.seq = 2;
            pending.new = p5::BindingTarget::Pending {
                payload_session_id: "pending".into(),
                payload_transcript_path: None,
            };
            let mut rejected = row;
            rejected.new = p5::BindingTarget::Rejected {
                payload_session_id: "rejected".into(),
                payload_transcript_path: None,
                reason: "unproven".into(),
            };
            for bytes in [
                format!("{}\n", serde_json::to_string(&rejected).unwrap()),
                format!(
                    "{}{}\n",
                    String::from_utf8_lossy(&good),
                    serde_json::to_string(&pending).unwrap()
                ),
                "corrupt\n".into(),
            ] {
                std::fs::write(binding_log(channel), bytes).unwrap();
                let reply = run_slash_stop(&shared, &provider, channel).await;
                assert_eq!(reply.text(), HOST_REFUSED_STOP_RESPONSE);
                reply.finish(&shared, &provider, channel).await;
                assert!(fx.take_calls().is_empty());
            }
            return;
        }
        let generation_owner = bound_token(&provider, session);
        let generation = generation_owner.claude_interrupt_generation();
        let expected = match case {
            OwnerCase::Multiple | OwnerCase::ForeignProvider => HOST_REFUSED_STOP_RESPONSE,
            OwnerCase::Nonowner | OwnerCase::Mirror => NO_ACTIVE_TURN_RESPONSE,
            OwnerCase::Slash => super::super::NativeStop::observed_text(true),
            OwnerCase::OwnerChanged | OwnerCase::SourceChanged => {
                super::super::NativeStop::observed_text(false)
            }
            _ => STOPPING_RESPONSE,
        };
        if matches!(case, OwnerCase::CodexText) {
            let mock = discord_mock::DiscordMockState::new();
            let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
            let ctx = discord_mock::serenity_context(proxy, gateway).await;
            shared.settings.write().await.owner_user_id = Some(7);
            let mut voice_config = crate::voice::VoiceConfig::default();
            voice_config.keep_recordings = true;
            voice_config.audio.recordings_dir = root.path().join("recordings");
            let data = Data {
                shared: shared.clone(),
                token: "test-token".into(),
                provider: provider.clone(),
                voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
                voice_config,
                ingress_shadow: None,
            };
            command(&ctx, &data, channel).await;
            assert!(mock.unhandled.lock().unwrap().is_empty());
            server.abort();
        } else {
            let raced = matches!(case, OwnerCase::OwnerChanged | OwnerCase::SourceChanged);
            let (release, held, worker) = if raced {
                let (holding, held) = std::sync::mpsc::channel();
                let (release, released) = std::sync::mpsc::channel();
                let worker = std::thread::spawn(move || {
                    crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
                        session,
                        || {
                            holding.send(()).unwrap();
                            released.recv().unwrap();
                        },
                    );
                });
                (Some(release), Some(held), Some(worker))
            } else {
                (None, None, None)
            };
            if let Some(held) = held {
                held.recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap();
            }
            if matches!(case, OwnerCase::Slash) {
                let home =
                    crate::services::cluster::channel_home::register_for_test(channel.get(), None);
                assert_eq!(
                    home.refusal(),
                    Some(crate::services::cluster::channel_home::HomeRefusal::NotHeld)
                );
            }
            let request = run_slash_stop(&shared, &provider, channel);
            let change = async {
                let Some(release) = release else {
                    return;
                };
                tokio::time::timeout(std::time::Duration::from_secs(3), async {
                    loop {
                        if fx
                            .calls()
                            .iter()
                            .any(|call| call.starts_with("capture-pane"))
                        {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if matches!(case, OwnerCase::OwnerChanged) {
                    shared
                        .tmux_watchers
                        .clear_restored_owner_for_tmux_session(session);
                    shared.tmux_watchers.restore_owner_channel_for_tmux_session(
                        session,
                        ChannelId::new(channel.get() + 10),
                    );
                } else {
                    let replacement = root.path().join("replacement.jsonl");
                    open(&replacement, "parent");
                    write_binding_event(&provider, channel, session, &replacement, 2);
                }
                release.send(()).unwrap();
            };
            let (reply, ()) = tokio::join!(request, change);
            if let Some(worker) = worker {
                worker.join().unwrap();
            }
            assert_eq!(
                reply.text(),
                expected,
                "residual home must not refuse slash stop"
            );
            reply.finish(&shared, &provider, channel).await;
            if matches!(case, OwnerCase::Slash) {
                crate::services::cluster::channel_home::unregister(&channel.get().to_string());
            }
        }
        let calls = fx.take_calls();
        if matches!(case, OwnerCase::Slash | OwnerCase::CodexText) {
            let writes = escapes(&calls);
            assert_eq!(writes.len(), 1, "unique owner-only parent: {calls:?}");
            assert!(writes[0].contains(session) && writes[0].contains("Escape"));
        } else if matches!(case, OwnerCase::OwnerChanged | OwnerCase::SourceChanged) {
            assert!(
                escapes(&calls).is_empty(),
                "changed observation must send no Escape: {calls:?}"
            );
        } else {
            assert!(
                calls.is_empty(),
                "refusal must perform no provider I/O: {calls:?}"
            );
        }
        assert_eq!(generation_owner.claude_interrupt_generation(), generation);
        assert!(!generation_owner.cancelled.load(Ordering::Acquire));
        assert!(
            shared.tmux_relay_coords.is_empty(),
            "owner-only stop cannot allocate a delivery lease"
        );
        assert!(shared.mailbox_peek(channel).is_none());
        assert!(crate::services::discord::tmux::recent_turn_stop_for_channel(channel).is_none());
        assert!(
            crate::services::discord::inflight::load_inflight_state_read_only(
                &provider,
                channel.get()
            )
            .is_none()
        );
        assert!(shared.tmux_watchers.channel_binding(&channel).is_none());
    });
}

#[test]
fn n2a_slash_stop_interrupts_unique_owner_only_claude_parent_without_watcher() {
    owner_stop_case(
        concat!(
            module_path!(),
            "::n2a_slash_stop_interrupts_unique_owner_only_claude_parent_without_watcher"
        ),
        OwnerCase::Slash,
    );
}

#[test]
fn n2a_text_stop_interrupts_unique_owner_only_codex_parent_without_watcher() {
    owner_stop_case(
        concat!(
            module_path!(),
            "::n2a_text_stop_interrupts_unique_owner_only_codex_parent_without_watcher"
        ),
        OwnerCase::CodexText,
    );
}

#[test]
fn n2a_multiple_typed_owner_sessions_refuse_before_open_state() {
    owner_stop_case(
        concat!(
            module_path!(),
            "::n2a_multiple_typed_owner_sessions_refuse_before_open_state"
        ),
        OwnerCase::Multiple,
    );
}

#[test]
fn n2a_foreign_provider_binding_is_host_refused_without_io() {
    owner_stop_case(
        concat!(
            module_path!(),
            "::n2a_foreign_provider_binding_is_host_refused_without_io"
        ),
        OwnerCase::ForeignProvider,
    );
}

#[test]
fn n2a_nonowner_typed_session_is_not_a_candidate() {
    owner_stop_case(
        concat!(
            module_path!(),
            "::n2a_nonowner_typed_session_is_not_a_candidate"
        ),
        OwnerCase::Nonowner,
    );
}

#[test]
fn n2a_ttl_mirror_only_session_is_not_a_candidate() {
    owner_stop_case(
        concat!(
            module_path!(),
            "::n2a_ttl_mirror_only_session_is_not_a_candidate"
        ),
        OwnerCase::Mirror,
    );
}

#[test]
fn n2a_owner_change_after_judgment_prevents_slash_finish_interrupt() {
    owner_stop_case(
        concat!(
            module_path!(),
            "::n2a_owner_change_after_judgment_prevents_slash_finish_interrupt"
        ),
        OwnerCase::OwnerChanged,
    );
}

#[test]
fn n2a_source_rotation_after_judgment_prevents_slash_finish_interrupt() {
    owner_stop_case(
        concat!(
            module_path!(),
            "::n2a_source_rotation_after_judgment_prevents_slash_finish_interrupt"
        ),
        OwnerCase::SourceChanged,
    );
}

#[test]
fn n2a_owner_only_pending_or_corrupt_binding_refuses_without_io() {
    owner_stop_case(
        concat!(
            module_path!(),
            "::n2a_owner_only_pending_or_corrupt_binding_refuses_without_io"
        ),
        OwnerCase::InvalidBinding,
    );
}

pub(super) fn mutate_initial_owners(
    channel: ChannelId,
    owners: Vec<AuthoritativeSessionBinding>,
) -> Vec<AuthoritativeSessionBinding> {
    if !owners.is_empty()
        || !crate::services::discord::tmux_watcher_registry::owner_stop_mutant("2a-mirror-fallback")
    {
        return owners;
    }
    crate::services::tui_o::writer::binding::BindingLog
        .binding_events_since(channel.get(), 0)
        .unwrap_or_default()
        .into_iter()
        .filter(|event| {
            crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(&event.tmux_session)
                == Some(channel.get())
        })
        .map(|event| AuthoritativeSessionBinding {
            owner_channel_id: channel,
            tmux_session_name: event.tmux_session,
        })
        .collect()
}

type TypedEvents = Vec<crate::services::tui_o::writer::binding::BindingEvent>;

pub(super) fn mutate_events(
    channel: ChannelId,
    events: Result<TypedEvents, String>,
) -> Result<TypedEvents, String> {
    if crate::services::discord::tmux_watcher_registry::owner_stop_mutant("2a-provider-check-off") {
        crate::services::tui_o::writer::binding::BindingLog.binding_events_since(channel.get(), 0)
    } else {
        events
    }
}

pub(super) fn mutate_bound_owners(
    channel: ChannelId,
    events: &TypedEvents,
    owners: Vec<AuthoritativeSessionBinding>,
) -> Vec<AuthoritativeSessionBinding> {
    if crate::services::discord::tmux_watcher_registry::owner_stop_mutant("2a-owner-check-off") {
        events
            .iter()
            .map(|event| AuthoritativeSessionBinding {
                owner_channel_id: channel,
                tmux_session_name: event.tmux_session.clone(),
            })
            .collect()
    } else {
        owners
    }
}

pub(super) fn mutate_candidates(candidates: &mut Vec<(String, SourceBinding)>) {
    use crate::services::discord::tmux_watcher_registry::owner_stop_mutant;
    if owner_stop_mutant("2a-open-disambiguates") {
        candidates.retain(|(_, binding)| {
            InputFacts::open(binding.clone())
                .and_then(|mut facts| facts.poll(crate::services::tui_o::shadow::MAX_READ_BYTES))
                .is_ok_and(|fact| matches!(fact.state, TurnState::Open { .. }))
        });
    }
    if owner_stop_mutant("2a-first-candidate-wins") {
        candidates.truncate(1);
    }
}

#[test]
fn preserve_slash_stop_session_skip_reports_unconfirmed_without_mailbox() {
    let fx = Fixture::new();
    let root = tempfile::tempdir().unwrap();
    let _binding_root = TestBindingRoot::enter(Some(root.path()));
    fx.pane("Allow\nDeny\nEnter to confirm\n❯");
    run(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(7_193_090);
        let _confirmed = TestConfirmation::new(channel.get());
        let session = "native-stop-session-skip";
        let path = root.path().join("session.jsonl");
        open(&path, "first");
        bind_live(&shared, &ProviderKind::Claude, channel, session, &path);
        let reply = crate::services::discord::commands::stop::run_slash_stop(
            &shared,
            &ProviderKind::Claude,
            channel,
        )
        .await;
        assert!(reply.text().contains("중단 미확인"));
        reply.finish(&shared, &ProviderKind::Claude, channel).await;
        assert!(shared.mailbox_peek(channel).is_none());
        assert!(
            !fx.take_calls()
                .iter()
                .any(|call| call.starts_with("send-keys"))
        );
    });
}
