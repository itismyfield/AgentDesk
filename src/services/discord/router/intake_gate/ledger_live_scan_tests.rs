use super::*;
use crate::services::discord::{Data, advance_last_message_checkpoint, router};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

struct LiveTransport {
    ctx: serenity::Context,
    state: Arc<State>,
    download: String,
    pausing: Arc<AtomicBool>,
    history: Arc<Mutex<Vec<Message>>>,
    history_queries: Arc<Mutex<Vec<String>>>,
    http: tokio::task::JoinHandle<()>,
    gateway: tokio::task::JoinHandle<()>,
}

impl Drop for LiveTransport {
    fn drop(&mut self) {
        self.http.abort();
        self.gateway.abort();
    }
}

impl LiveTransport {
    async fn new(channel: u64, mode: Mode) -> Self {
        let mut reply = discord_mock::user_message(channel + 1, "actual referenced message");
        reply.channel_id = ChannelId::new(channel);
        reply.guild_id = Some(serenity::GuildId::new(channel + 900));
        let state = Arc::new(State {
            channel,
            mode,
            reply,
            calls: Mutex::new(vec![]),
            downloads: AtomicUsize::new(0),
            entered: Notify::new(),
            release: Notify::new(),
        });
        let pausing = Arc::new(AtomicBool::new(matches!(
            mode,
            Mode::PauseReply | Mode::PauseDownload
        )));
        let history: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(vec![]));
        let history_queries = Arc::new(Mutex::new(vec![]));
        let (paused_route, history_route, query_route) =
            (pausing.clone(), history.clone(), history_queries.clone());
        let route = state.clone();
        let app = Router::new().fallback(move |method: Method, uri: Uri| {
            let state = route.clone();
            let (pausing, history, queries) = (paused_route.clone(), history_route.clone(), query_route.clone());
            async move {
                let path = uri.path().to_string();
                state.calls.lock().unwrap().push((method.clone(), path.clone()));
                if method == Method::PUT && path.contains("/reactions/") {
                    return StatusCode::NO_CONTENT.into_response();
                }
                if method != Method::GET {
                    return StatusCode::FORBIDDEN.into_response();
                }
                if path.ends_with(&format!("/channels/{}", state.channel)) {
                    return Json(json!({"id":state.channel.to_string(),"type":0,"guild_id":(state.channel+900).to_string(),"name":"c2b-io-cc","position":0,"permission_overwrites":[],"nsfw":false,"parent_id":null})).into_response();
                }
                if path.ends_with(&format!("/channels/{}/messages/{}", state.channel, state.channel+1)) {
                    state.entered.notify_one();
                    if matches!(state.mode, Mode::PauseReply) && pausing.load(Ordering::SeqCst) {
                        state.release.notified().await;
                    }
                    return Json(serde_json::to_value(&state.reply).unwrap()).into_response();
                }
                if path == "/download" {
                    state.downloads.fetch_add(1, Ordering::SeqCst);
                    if !pausing.load(Ordering::SeqCst) { return b"prefix-tail".to_vec().into_response(); }
                    let tail = state.clone();
                    let first = futures::stream::once(async { Ok::<_, std::io::Error>(Bytes::from_static(b"prefix-")) });
                    let second = futures::stream::once(async move {
                        tail.entered.notify_one();
                        tail.release.notified().await;
                        Ok::<_, std::io::Error>(Bytes::from_static(b"tail"))
                    });
                    return Body::from_stream(first.chain(second)).into_response();
                }
                if path.ends_with("/messages") {
                    let query = uri.query().expect("actual history API includes its cursor");
                    queries.lock().unwrap().push(query.to_owned());
                    let parameters: std::collections::HashMap<_, _> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
                    let after: u64 = parameters.get("after").expect("fixture requires after cursor").parse().unwrap();
                    assert_eq!(parameters.get("limit").map(String::as_str), Some("100"));
                    let mut page: Vec<_> = history.lock().unwrap().iter().filter(|message| message.id.get() > after).cloned().collect();
                    page.sort_by_key(|message| std::cmp::Reverse(message.id.get()));
                    return Json(page).into_response();
                }
                StatusCode::NOT_FOUND.into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let download = format!("{proxy}/download");
        let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (_, gateway_url, gateway) =
            discord_mock::start(discord_mock::DiscordMockState::new()).await;
        let ctx = discord_mock::serenity_context(proxy, gateway_url).await;
        Self {
            ctx,
            state,
            download,
            pausing,
            history,
            history_queries,
            http,
            gateway,
        }
    }
}

async fn terminal_while_live_paused(root: &std::path::Path, channel: u64, mode: Mode) {
    let reply = matches!(mode, Mode::PauseReply);
    let message = original(channel, reply, usize::from(!reply));
    let original_value = serde_json::to_value(&message).unwrap();
    let mut terminal = discord_mock::user_message(
        message.id.get() + 1,
        "DISPATCH: c2c2-missing-dispatch - terminal source",
    );
    terminal.channel_id = message.channel_id;
    terminal.guild_id = message.guild_id;
    terminal.author.bot = true;
    terminal.author.id = serenity::UserId::new(message.author.id.get() + 100);
    let terminal_value = serde_json::to_value(&terminal).unwrap();
    let (db, pool) = crate::services::discord::host_defer_gate::tests::postgres().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    {
        let mut settings = shared.settings.write().await;
        settings.provider = ProviderKind::Claude;
        settings.owner_user_id = Some(message.author.id.get());
        settings.allowed_channel_ids = vec![channel];
        settings.allowed_bot_ids = vec![terminal.author.id.get()];
    }
    crate::services::discord::host_defer_gate::tests::map_channel(
        &shared,
        message.channel_id,
        "c2b-io-cc",
    )
    .await;
    let transport = LiveTransport::new(channel, mode).await;
    *transport.history.lock().unwrap() = vec![message.clone(), terminal.clone()];
    assert_eq!(
        crate::services::discord::resolve_runtime_channel_binding_status(
            &transport.ctx.http,
            message.channel_id
        )
        .await,
        crate::services::discord::RuntimeChannelBindingStatus::Owned,
        "fixture has an actual AgentDef-owned GuildText binding before the C gate",
    );
    assert!(
        crate::services::discord::provider_handles_channel(
            &transport.ctx,
            &ProviderKind::Claude,
            &shared.settings.read().await.clone(),
            message.channel_id,
        )
        .await,
        "fixture passes actual provider routing before the C gate"
    );
    assert!(
        crate::services::discord::check_auth(
            message.author.id,
            &message.author.name,
            &shared,
            "test-token",
        )
        .await,
        "fixture passes actual user authorization before the C gate"
    );
    println!("C2C2_LIVE_FIXTURE_BOUND_ROUTED_AUTHORIZED channel={channel}");
    assert!(
        shared
            .http
            .cached_serenity_ctx
            .set(transport.ctx.clone())
            .is_ok()
    );
    shared.turn_view_reconciler.enable_test_http_reactions();
    let _download = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_C2B_TEST_ATTACHMENT_URL",
        std::ffi::OsStr::new(&transport.download),
    );
    let mut voice_config = crate::voice::VoiceConfig::default();
    voice_config.audio.recordings_dir = root.join("recordings");
    let data = Arc::new(Data {
        shared: shared.clone(),
        token: "test-token".into(),
        provider: ProviderKind::Claude,
        voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
        voice_config,
    });
    let before = message.id.get() - 1;
    advance_last_message_checkpoint(
        &shared,
        &ProviderKind::Claude,
        message.channel_id,
        serenity::MessageId::new(before),
    );
    let checkpoint_path = crate::services::discord::runtime_store::last_message_root()
        .unwrap()
        .join("claude")
        .join(format!("{channel}.txt"));
    assert_eq!(
        std::fs::read_to_string(&checkpoint_path).unwrap(),
        before.to_string()
    );
    assert!(
        crate::services::discord::stale_dispatch_turn_for_text(Some(&pool), &terminal.content)
            .await
            .is_some(),
        "terminal classification comes from the actual PostgreSQL dispatch lookup"
    );
    let (_, evidence) = runtime::supervisor::run_messages(
        runtime::runtime::registry(), channel, root, false, |client, overflow| {
            let (message, terminal, transport, data, shared, checkpoint_path) =
                (&message, &terminal, &transport, &data, &shared, &checkpoint_path);
            async move {
            let old_ticket = client.fetch_ticket().await.unwrap();
            let ctx = transport.ctx.clone();
            let live_data = data.clone();
            let live_event = serenity::FullEvent::Message { new_message: message.clone() };
            let mut live = tokio::spawn(async move {
                router::handle_event(&ctx, &live_event, &live_data).await
            });
            tokio::select! {
                () = transport.state.entered.notified() => {},
                result = &mut live => panic!("actual reply/download must pause after pending: {result:?}"),
            }
            println!("C2C2_LIVE_FIXTURE_PENDING_IO_CONFIRMED source={}", message.id);
            let terminal_event = serenity::FullEvent::Message { new_message: terminal.clone() };
            let result = tokio::time::timeout(Duration::from_secs(10),
                router::handle_event(&transport.ctx, &terminal_event, &data)).await.unwrap();
            let feedback = transport.state.calls.lock().unwrap().iter().filter(|(method, path)| {
                *method == Method::PUT && path.contains(&format!("/messages/{}/reactions/", terminal.id))
            }).count();
            assert!(feedback > 0,
                "actual terminal N feedback is retained while earlier M IO is paused; result={result:?}");
            assert_eq!(shared.last_message_ids.get(&message.channel_id).map(|value| *value), Some(before));
            assert_eq!(std::fs::read_to_string(&checkpoint_path).unwrap(), before.to_string());
            assert!(client.begin_scan(old_ticket, vec![message.id.get(), terminal.id.get()], terminal.id.get(), true).await.is_err(),
                "a ticket issued before original pending cannot settle this scan");
            let ticket = client.fetch_ticket().await.unwrap();
            assert!(client.begin_scan(ticket, vec![terminal.id.get()], terminal.id.get(), true).await.is_err(),
                "N-only page cannot omit the pending M");
            let generation = overflow.generation();
            live.abort();
            assert!(live.await.unwrap_err().is_cancelled());
            assert_eq!(overflow.generation(), generation + 1);
            transport.pausing.store(false, Ordering::SeqCst);
            transport.state.release.notify_waiters();
            let (mut cursor, first_page) = fetched_scan(&client, transport, before).await;
            cursor.complete_after = true;
            let (n_first, rejected) = prepare_message(shared, &transport.ctx.http, &ProviderKind::Claude,
                &first_page[1], SourceKind::LiveMessage, None, Some(cursor)).await.unwrap();
            assert!(matches!(n_first, Some(IntakeAdmission::LedgerDeferred(_))),
                "actual N-first settlement refuses an unprocessed M prefix");
            assert!(rejected.unwrap().capability.is_none());
            assert_checkpoint(shared, message.channel_id, checkpoint_path, before);
            let (cursor, page) = fetched_scan(&client, transport, before).await;
            let (m, cursor) = prepare_message(shared, &transport.ctx.http, &ProviderKind::Claude,
                &page[0], SourceKind::LiveMessage, None, Some(cursor)).await.unwrap();
            let m = m.expect("M has an actual typed WAL receipt");
            assert!(custody::settled(&m));
            let original = custody::original_for_test(&m).unwrap();
            assert_eq!(original.identity().source_ids, [message.id.get()]);
            assert_eq!(original.input()["original_text"], message.content);
            let proof = custody::receipt_for_test(&m).unwrap().clone();
            let mut cursor = cursor.unwrap();
            assert_eq!(cursor.last_settled, Some(message.id.get()));
            assert_eq!(cursor.completion, None);
            assert_checkpoint(shared, message.channel_id, checkpoint_path, before);
            cursor.complete_after = true;
            let (n, cursor) = prepare_message(shared, &transport.ctx.http, &ProviderKind::Claude,
                &page[1], SourceKind::LiveMessage, None, Some(cursor)).await.unwrap();
            assert!(n.is_none(), "terminal N uses NonTarget SettleFromScan");
            let cursor = cursor.unwrap();
            assert_eq!(cursor.completion, Some(Ok(())));
            assert_eq!(cursor.last_settled, Some(terminal.id.get()));
            assert!(cursor.capability.is_none());
            assert_checkpoint(shared, message.channel_id, checkpoint_path, terminal.id.get());
            let downloads = transport.state.downloads.load(Ordering::SeqCst);
            let redelivery = serenity::FullEvent::Message { new_message: message.clone() };
            router::handle_event(&transport.ctx, &redelivery, data).await.unwrap();
            let canonical = client.lookup_responsibility(proof.identity.clone()).await.unwrap();
            assert_eq!(canonical.receipt, Some(proof));
            assert_eq!(transport.state.downloads.load(Ordering::SeqCst), downloads);
            assert_eq!(transport.history_queries.lock().unwrap().len(), 2);
            println!("C2C2_SAME_SCAN_M_N_COMPLETE_CONFIRMED channel={channel}");
            }
        },
    ).await;
    assert_eq!(serde_json::to_value(&message).unwrap(), original_value);
    assert_eq!(serde_json::to_value(&terminal).unwrap(), terminal_value);
    assert!(evidence.pending.sources.is_empty());
    assert_eq!(evidence.rows.len(), 1);
    assert_eq!(evidence.rows[0].input["message_id"], message.id.get());
    assert_eq!(
        &evidence.commands[..4],
        &["ticket", "pending", "lookup", "pending"]
    );
    assert_eq!(
        evidence
            .commands
            .iter()
            .filter(|command| **command == "commit")
            .count(),
        1
    );
    assert_eq!(
        evidence
            .commands
            .iter()
            .filter(|command| **command == "settle")
            .count(),
        2
    );
    shared.mailboxes.remove_fixture_for_test(message.channel_id);
    pool.close().await;
    db.drop().await;
}

#[test]
fn c2c2_actual_live_pause_keeps_terminal_feedback_and_blocks_direct_checkpoint_pg() {
    let test = "live_scan_tests::c2c2_actual_live_pause_keeps_terminal_feedback_and_blocks_direct_checkpoint_pg";
    if !isolated_io_fixture(test) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let _config = configure(root.path());
    let runtime = runtime();
    runtime.block_on(terminal_while_live_paused(
        root.path(),
        940_632_870_000_000,
        Mode::PauseReply,
    ));
    runtime.block_on(terminal_while_live_paused(
        root.path(),
        940_632_870_000_010,
        Mode::PauseDownload,
    ));
    println!("G2C_IO_FIXTURE_CONFIRMED {test}");
}

fn configure(root: &Path) -> crate::config::TestEnvVarGuard {
    let config = crate::runtime_layout::config_file_path(root);
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let agents: Vec<_> = [940_632_870_000_000_u64, 940_632_870_000_010_u64, 940_632_870_000_020_u64]
        .into_iter().map(|channel| json!({
            "id": format!("c2c2-live-{channel}"), "name": "C2c2 live fixture", "voice_enabled": false,
            "provider": "claude", "channels": {"claude": {"id": channel.to_string(),
                "name": "c2b-io-cc", "workspace": root.join("workspace"), "runtime": "tui"}}
        })).collect();
    let fixture = json!({
        "server": {}, "data": {"dir": root.join("data")}, "agents": agents,
    });
    std::fs::write(&config, serde_json::to_vec(&fixture).unwrap()).unwrap();
    let config_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    assert_eq!(crate::config::resolved_config_path(), config);
    let parsed = crate::config::load_from_path(&config)
        .expect("fixture Config passes actual loader validation");
    assert_eq!(parsed.agents.len(), 3);
    for channel in [
        940_632_870_000_000_u64,
        940_632_870_000_010_u64,
        940_632_870_000_020_u64,
    ] {
        assert_eq!(
            crate::services::discord::settings::resolve_workspace(ChannelId::new(channel), None),
            Some(root.join("workspace").to_string_lossy().into_owned()),
            "actual binding accessor reads the canonical fixture config"
        );
    }
    config_guard
}

async fn fetched_scan(
    client: &Arc<runtime::intake::Client>,
    transport: &LiveTransport,
    after: u64,
) -> (ScanCursor, Vec<Message>) {
    let ticket = client.fetch_ticket().await.unwrap();
    let mut messages = ChannelId::new(transport.state.channel)
        .messages(
            &transport.ctx.http,
            serenity::GetMessages::new()
                .after(serenity::MessageId::new(after))
                .limit(100),
        )
        .await
        .unwrap();
    messages.sort_by_key(|message| message.id.get());
    assert_eq!(
        messages
            .iter()
            .map(|message| message.id.get())
            .collect::<Vec<_>>(),
        transport
            .history
            .lock()
            .unwrap()
            .iter()
            .map(|message| message.id.get())
            .collect::<Vec<_>>()
    );
    assert_eq!(messages.len(), 2);
    assert_eq!(
        serde_json::to_value(&messages).unwrap(),
        serde_json::to_value(&*transport.history.lock().unwrap()).unwrap(),
        "the shared body receives actual HTTP Message values with their entire original payload"
    );
    let horizon = messages
        .last()
        .expect("fixture fetches a complete nonempty page")
        .id
        .get();
    let capability = client
        .begin_scan(
            ticket,
            messages.iter().map(|message| message.id.get()).collect(),
            horizon,
            true,
        )
        .await
        .unwrap();
    (
        ScanCursor {
            client: client.clone(),
            channel: transport.state.channel,
            capability: Some(capability),
            last_settled: None,
            complete_after: false,
            completion: None,
        },
        messages,
    )
}

fn assert_checkpoint(shared: &SharedData, channel: ChannelId, path: &Path, expected: u64) {
    assert_eq!(
        shared.last_message_ids.get(&channel).map(|value| *value),
        Some(expected)
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), expected.to_string());
}

fn pin_snapshot(
    root: &Path,
    channel: u64,
    key: u64,
) -> (Vec<u8>, std::time::SystemTime, std::time::SystemTime) {
    let parent = crate::services::tui_input::ledger::dir(root, channel)
        .join("blobs/att")
        .join(key.to_string());
    let files: Vec<_> = std::fs::read_dir(&parent)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(
        files.len(),
        1,
        "one fresh attachment creates exactly one pin"
    );
    (
        std::fs::read(&files[0]).unwrap(),
        std::fs::metadata(&files[0]).unwrap().modified().unwrap(),
        std::fs::metadata(&parent).unwrap().modified().unwrap(),
    )
}

#[test]
fn c2c2_actual_wal_rescan_keeps_receipt_without_reply_download_or_repin_pg() {
    let test =
        "live_scan_tests::c2c2_actual_wal_rescan_keeps_receipt_without_reply_download_or_repin_pg";
    if !isolated_io_fixture(test) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let _config = configure(root.path());
    runtime().block_on(async {
        let channel = 940_632_870_000_020_u64;
        let mut message = original(channel, true, 1);
        message.content = "DISPATCH: c2c2-human-missing - preserved human work".into();
        let mut terminal = discord_mock::user_message(message.id.get() + 1, "DISPATCH: c2c2-wal-terminal - terminal source");
        terminal.channel_id = message.channel_id;
        terminal.guild_id = message.guild_id;
        terminal.author.bot = true;
        terminal.author.id = serenity::UserId::new(message.author.id.get() + 100);
        let (db, pool) = crate::services::discord::host_defer_gate::tests::postgres().await;
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        assert!(crate::services::discord::stale_dispatch_turn_for_text(Some(&pool), &message.content).await.is_some(),
            "actual terminal PostgreSQL facts do not suppress positively identified human work");
        {
            let mut settings = shared.settings.write().await;
            settings.provider = ProviderKind::Claude;
            settings.owner_user_id = Some(message.author.id.get());
            settings.allowed_channel_ids = vec![channel];
            settings.allowed_bot_ids = vec![terminal.author.id.get()];
        }
        crate::services::discord::host_defer_gate::tests::map_channel(&shared, message.channel_id, "c2b-io-cc").await;
        let transport = LiveTransport::new(channel, Mode::Complete).await;
        *transport.history.lock().unwrap() = vec![message.clone(), terminal.clone()];
        let _download = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "ADK_C2B_TEST_ATTACHMENT_URL", std::ffi::OsStr::new(&transport.download));
        let before = message.id.get() - 1;
        advance_last_message_checkpoint(&shared, &ProviderKind::Claude, message.channel_id, serenity::MessageId::new(before));
        let checkpoint_path = crate::services::discord::runtime_store::last_message_root().unwrap().join("claude").join(format!("{channel}.txt"));
        let ((first_proof, second_proof), evidence) = runtime::supervisor::run_messages(
            runtime::runtime::registry(), channel, root.path(), true, |client, overflow| {
                let (shared, transport, message, terminal, root, checkpoint_path) =
                    (&shared, &transport, &message, &terminal, root.path(), &checkpoint_path);
                async move {
                    client.pending(vec![message.id.get(), terminal.id.get()]).await.unwrap();
                    let (cursor, page) = fetched_scan(&client, transport, before).await;
                    let generation = overflow.generation();
                    let (first, cursor) = prepare_message(shared, &transport.ctx.http, &ProviderKind::Claude,
                        &page[0], SourceKind::LiveMessage, None, Some(cursor)).await.unwrap();
                    let first = first.expect("synced WAL returns an actual typed receipt despite settlement failure");
                    let first_proof = custody::receipt_for_test(&first).expect("receipt is retained after actual WAL IO").clone();
                    assert!(!custody::settled(&first), "WAL receipt cannot invent scan settlement success");
                    assert_eq!(overflow.generation(), generation + 1);
                    let mut cursor = cursor.unwrap();
                    assert_eq!(cursor.last_settled, None);
                    assert_eq!(cursor.completion, None);
                    assert!(client.complete_scan(cursor.capability.take().unwrap()).await.is_err());
                    assert_checkpoint(shared, message.channel_id, checkpoint_path, before);
                    let pins = pin_snapshot(root, channel, message.id.get());
                    assert_eq!(pins.0, b"prefix-tail");
                    assert_eq!(transport.state.downloads.load(Ordering::SeqCst), 1);
                    let replies = transport.state.calls.lock().unwrap().iter().filter(|(_, path)| path.ends_with(&format!("/messages/{}", channel + 1))).count();
                    assert_eq!(replies, 1);
                    let (cursor, page) = fetched_scan(&client, transport, before).await;
                    let (second, cursor) = prepare_message(shared, &transport.ctx.http, &ProviderKind::Claude,
                        &page[0], SourceKind::LiveMessage, None, Some(cursor)).await.unwrap();
                    let second = second.expect("actual duplicate CommitFromScan reuses canonical receipt");
                    let second_proof = custody::receipt_for_test(&second).expect("duplicate keeps immutable canonical proof").clone();
                    assert_eq!(first_proof, second_proof);
                    assert!(custody::settled(&second));
                    assert!(custody::original_for_test(&second).unwrap().attachments().iter().all(|attachment| attachment.staged.is_none()),
                        "canonical duplicate is committed with original references, without staging again");
                    assert_eq!(transport.state.downloads.load(Ordering::SeqCst), 1);
                    assert_eq!(transport.state.calls.lock().unwrap().iter().filter(|(_, path)| path.ends_with(&format!("/messages/{}", channel + 1))).count(), replies);
                    assert_eq!(pin_snapshot(root, channel, message.id.get()), pins, "retry performs no new pin filesystem IO");
                    assert_checkpoint(shared, message.channel_id, checkpoint_path, before);
                    let mut cursor = cursor.unwrap();
                    cursor.complete_after = true;
                    let (n, cursor) = prepare_message(shared, &transport.ctx.http, &ProviderKind::Claude,
                        &page[1], SourceKind::LiveMessage, None, Some(cursor)).await.unwrap();
                    assert!(n.is_none());
                    assert_eq!(cursor.unwrap().completion, Some(Ok(())));
                    assert_checkpoint(shared, message.channel_id, checkpoint_path, terminal.id.get());
                    println!("C2C2_WAL_RESCAN_NO_REFETCH_REPIN_CONFIRMED channel={channel}");
                    (first_proof, second_proof)
                }
            },
        ).await;
        assert_eq!(first_proof, second_proof);
        assert_eq!(evidence.rows.len(), 1);
        assert_eq!(evidence.rows[0].since_seq, first_proof.received_seq);
        assert_eq!(evidence.rows[0].input["receipt_identity"], serde_json::to_value(&first_proof.identity).unwrap());
        assert!(evidence.pending.sources.is_empty());
        assert_eq!(evidence.commands.iter().filter(|command| **command == "lookup").count(), 2);
        assert_eq!(evidence.commands.iter().filter(|command| **command == "commit").count(), 2);
        shared.mailboxes.remove_fixture_for_test(message.channel_id);
        pool.close().await;
        db.drop().await;
    });
    println!("G2C_IO_FIXTURE_CONFIRMED {test}");
}

#[test]
fn c2c2_terminal_feedback_wiring_preserves_human_guard_and_off_effect_order() {
    let module_src = include_str!("../intake_gate.rs");
    let guard_pos = module_src.find("if !preserve_on_cancel").unwrap();
    let stale_lookup_pos = module_src[guard_pos..]
        .find("stale_dispatch_turn_for_text(")
        .unwrap()
        + guard_pos;
    let feedback_pos = module_src[stale_lookup_pos..]
        .find("ledger::terminal_feedback(")
        .map(|offset| stale_lookup_pos + offset)
        .expect("terminal feedback stays behind the same human preservation predicate");
    assert!(guard_pos < stale_lookup_pos && stale_lookup_pos < feedback_pos);
    let helper = include_str!("ledger.rs");
    let helper = &helper[helper
        .find("pub(super) async fn terminal_feedback(")
        .unwrap()..];
    let legacy_guard = helper
        .find("legacy_allowed(")
        .expect("terminal checkpoint checks the actual Legacy fence");
    let checkpoint = helper.find("advance_last_message_checkpoint(").unwrap();
    let feedback = helper.find("note_exit_feedback_added(").unwrap();
    assert!(
        legacy_guard < checkpoint && checkpoint < feedback,
        "OFF preserves checkpoint-before-feedback ordering; actual protected fixture verifies CP remains blocked"
    );
}

#[test]
fn c2c2_terminal_scan_rechecks_authorization_after_actual_pg_wait_pg() {
    let test = "live_scan_tests::c2c2_terminal_scan_rechecks_authorization_after_actual_pg_wait_pg";
    if !isolated_io_fixture(test) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let _config = configure(root.path());
    runtime().block_on(terminal_pg_wait_case(root.path(), false));
    println!("G2C_IO_FIXTURE_CONFIRMED {test}");
}

#[test]
fn c2c2_terminal_scan_preserves_human_owner_after_actual_pg_wait_pg() {
    let test = "live_scan_tests::c2c2_terminal_scan_preserves_human_owner_after_actual_pg_wait_pg";
    if !isolated_io_fixture(test) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let _config = configure(root.path());
    runtime().block_on(terminal_pg_wait_case(root.path(), true));
    println!("G2C_IO_FIXTURE_CONFIRMED {test}");
}

async fn terminal_pg_wait_case(root: &Path, human: bool) {
    let channel = 940_632_870_000_020_u64;
    let mut terminal = original(channel, false, 0);
    terminal.author.bot = !human;
    terminal.content = "DISPATCH: c2c2-auth-wait-terminal - terminal source".into();
    let mut later = discord_mock::user_message(terminal.id.get() + 1, "later preserved source");
    later.channel_id = terminal.channel_id;
    later.guild_id = terminal.guild_id;
    let (db, pool) = crate::services::discord::host_defer_gate::tests::postgres().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    {
        let mut settings = shared.settings.write().await;
        settings.provider = ProviderKind::Claude;
        settings.owner_user_id = Some(later.author.id.get());
        settings.allowed_channel_ids = vec![channel];
        settings.allowed_bot_ids = vec![terminal.author.id.get()];
    }
    crate::services::discord::host_defer_gate::tests::map_channel(
        &shared,
        terminal.channel_id,
        "c2b-io-cc",
    )
    .await;
    let transport = LiveTransport::new(channel, Mode::Complete).await;
    *transport.history.lock().unwrap() = vec![terminal.clone(), later.clone()];
    assert!(
        crate::services::discord::stale_dispatch_turn_for_text(Some(&pool), &terminal.content)
            .await
            .is_some(),
        "fixture starts with an actual positive terminal dispatch lookup"
    );
    let before = terminal.id.get() - 1;
    advance_last_message_checkpoint(
        &shared,
        &ProviderKind::Claude,
        terminal.channel_id,
        serenity::MessageId::new(before),
    );
    let checkpoint_path = crate::services::discord::runtime_store::last_message_root()
        .unwrap()
        .join("claude")
        .join(format!("{channel}.txt"));
    let (proof, evidence) = runtime::supervisor::run_messages(
        runtime::runtime::registry(), channel, root, false, |client, _| {
            let (shared, transport, terminal, later, pool, checkpoint_path) =
                (&shared, &transport, &terminal, &later, &pool, &checkpoint_path);
            async move {
                client.pending(vec![terminal.id.get(), later.id.get()]).await.unwrap();
                let (cursor, page) = fetched_scan(&client, transport, before).await;
                let source = admit::prepare(SourceKind::LiveMessage,
                    draft(&page[0], channel, page[0].content.clone()), Some("regular")).unwrap();
                let facts = runtime::source_policy_facts(shared, &transport.ctx.http, &source).await;
                assert_eq!(facts.authorized, Some(true));
                assert_eq!(runtime::intake::classify(&facts, &source), Classification::Target,
                    "fixture's real current policy authorizes the actual fetched sender before the terminal IO wait");
                println!("C2C2_TERMINAL_WAIT_FIXTURE_AUTHORIZED source={}", terminal.id);
                let mut lock = pool.begin().await.unwrap();
                sqlx::query("LOCK TABLE task_dispatches IN ACCESS EXCLUSIVE MODE").execute(&mut *lock).await.unwrap();
                let provider = ProviderKind::Claude;
                let mut processing = Box::pin(prepare_message(shared, &transport.ctx.http, &provider,
                    &page[0], SourceKind::LiveMessage, None, Some(cursor)));
                let blocked = async {
                    tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            let waiting: bool = sqlx::query_scalar(
                                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND state = 'active' AND wait_event_type = 'Lock' AND query LIKE $1)")
                                .bind("%FROM task_dispatches%")
                                .fetch_one(pool).await.unwrap();
                            if waiting { break; }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }).await.expect("fixture observes the actual terminal SELECT blocked on its owned table lock");
                };
                tokio::select! {
                    () = blocked => {},
                    result = &mut processing => panic!("fixture terminal processing ended before actual PG wait: ok={}", result.is_ok()),
                }
                println!("C2C2_TERMINAL_QUERY_LOCK_WAIT_CONFIRMED source={}", terminal.id);
                let owner = shared.settings.read().await.owner_user_id;
                shared.settings.write().await.allowed_bot_ids.clear();
                assert_eq!(shared.settings.read().await.owner_user_id, owner,
                    "changing explicit automation identity preserves the actual human request owner");
                if human {
                    assert_eq!(owner, Some(terminal.author.id.get()));
                    let current = shared.settings.read().await.clone();
                    assert!(crate::services::discord::discord_io::user_is_authorized(
                        &current, terminal.author.id.get()));
                }
                lock.commit().await.unwrap();
                let (admission, cursor) = processing.await.unwrap();
                if human {
                    let admission = admission.expect("actual human owner work keeps Target receipt after explicit automation identity removal");
                    assert!(custody::settled(&admission));
                    let original = custody::original_for_test(&admission).unwrap();
                    assert_eq!(original.identity(), source.identity());
                    assert_eq!(original.input()["author_is_bot"], false);
                    let proof = custody::receipt_for_test(&admission).expect("actual preserved human work has synced WAL responsibility").clone();
                    assert_eq!(proof.identity.author_id, terminal.author.id.get());
                    let cursor = cursor.unwrap();
                    assert_eq!(cursor.last_settled, Some(terminal.id.get()));
                    assert_eq!(cursor.completion, None);
                    assert!(cursor.capability.is_some());
                    assert_checkpoint(shared, terminal.channel_id, checkpoint_path, before);
                    println!("C2C2_TERMINAL_HUMAN_OWNER_PRESERVED_CONFIRMED source={}", terminal.id);
                    Some(proof)
                } else {
                    assert!(matches!(admission, Some(IntakeAdmission::LedgerDeferred(_))),
                        "actual terminal scan must defer after authorization changes during terminal PostgreSQL IO");
                    let admission = admission.unwrap();
                    assert_eq!(custody::classification_for_test(&admission),
                        Some(Classification::Rejected(runtime::intake::Refusal::Unauthorized)),
                        "fresh policy keeps the exact unauthorized refusal instead of an unknown or terminal disposition");
                    assert!(custody::receipt_for_test(&admission).is_none());
                    assert_eq!(custody::original_for_test(&admission).unwrap().identity(), source.identity());
                    let cursor = cursor.unwrap();
                    assert_eq!(cursor.last_settled, None);
                    assert_eq!(cursor.completion, None);
                    assert!(cursor.capability.is_some());
                    assert_checkpoint(shared, terminal.channel_id, checkpoint_path, before);
                    println!("C2C2_TERMINAL_FINAL_AUTH_RECHECK_CONFIRMED source={}", terminal.id);
                    None
                }
            }
        },
    ).await;
    if let Some(proof) = proof {
        assert_eq!(evidence.pending.sources, [later.id.get()]);
        assert_eq!(evidence.rows.len(), 1);
        assert_eq!(evidence.rows[0].since_seq, proof.received_seq);
        assert_eq!(
            evidence.rows[0].input["receipt_identity"],
            serde_json::to_value(proof.identity).unwrap()
        );
        assert_eq!(
            evidence
                .commands
                .iter()
                .filter(|command| **command == "commit")
                .count(),
            1
        );
    } else {
        assert_eq!(
            evidence.pending.sources,
            [terminal.id.get(), later.id.get()]
        );
        assert!(evidence.rows.is_empty());
        assert!(!evidence.commands.contains(&"commit"));
    }
    assert!(!evidence.commands.contains(&"settle"));
    assert!(!evidence.commands.contains(&"complete"));
    shared
        .mailboxes
        .remove_fixture_for_test(terminal.channel_id);
    pool.close().await;
    db.drop().await;
}
