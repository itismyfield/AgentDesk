//! Native gateway redelivery and REST catch-up meet the real paused writer and durable queue.
use super::*;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{Data, router};
use axum::response::IntoResponse;

async fn prepare_gateway(f: &Fixture) {
    let channel = serenity::ChannelId::new(f.channel);
    {
        let mut settings = f.shared.settings.write().await;
        settings.owner_user_id = Some(7);
        settings.allow_all_users = true;
        settings.allowed_channel_ids = vec![f.channel];
    }
    crate::services::discord::host_defer_gate::tests::map_channel(
        &f.shared,
        channel,
        "boot-gateway",
    )
    .await;
    let ctx = f
        .shared
        .http
        .cached_serenity_ctx
        .get()
        .expect("real mock Gateway context");
    assert!(
        crate::services::discord::provider_handles_channel(
            ctx,
            &ProviderKind::Claude,
            &f.shared.settings.read().await.clone(),
            channel,
        )
        .await,
        "the native gateway must pass provider routing"
    );
    assert_eq!(
        crate::services::discord::resolve_runtime_channel_binding_status(&ctx.http, channel,).await,
        crate::services::discord::RuntimeChannelBindingStatus::Owned
    );
    assert!(
        crate::services::discord::check_auth(
            serenity::UserId::new(7),
            "boot-user",
            &f.shared,
            "test-token",
        )
        .await,
        "a missing authorization setup must not turn recovery into a zero-selection pass"
    );
    assert!(f.shared.restart.reconcile_done.load(Ordering::SeqCst));
    assert!(!f.shared.restart.restart_pending.load(Ordering::SeqCst));
    assert!(!crate::services::discord::mailbox_has_active_turn(&f.shared, channel).await);
    assert!(f.disk().is_empty());
    assert!(matches!(
        for_input_placement("claude", f.channel).await,
        IntakeRoute::Hold(_)
    ));
}

fn gateway_message(f: &Fixture) -> serenity::FullEvent {
    let request = f.request();
    let mut message = discord_mock::user_message(f.message, &request.user_text);
    message.channel_id = request.channel_id;
    message.author.id = request.request_owner;
    message.author.name = request.request_owner_name;
    message.guild_id = Some(serenity::GuildId::new(940_487_400_000_009));
    serenity::FullEvent::Message {
        new_message: message,
    }
}

async fn deliver_gateway(f: &Fixture, event: &serenity::FullEvent) {
    let ctx = f
        .shared
        .http
        .cached_serenity_ctx
        .get()
        .expect("real mock Gateway context");
    let data = Data {
        shared: f.shared.clone(),
        token: "test-token".into(),
        provider: ProviderKind::Claude,
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        Box::pin(router::handle_event(ctx, event, &data)),
    )
    .await
    .expect("the native FullEvent must finish admission")
    .unwrap();
}

fn assert_one_custody(f: &Fixture) {
    let disk = f.disk();
    assert_eq!(
        disk.len(),
        1,
        "one durable original, including after redelivery/recovery"
    );
    assert_eq!(disk[0].message_id.get(), f.message);
    assert_eq!(
        disk[0].source_message_ids,
        vec![serenity::MessageId::new(f.message)]
    );
    assert_eq!(disk[0].text, f.request().user_text);
    assert_eq!(
        f.effects.load(Ordering::SeqCst),
        0,
        "the writer is still paused"
    );
    assert_eq!(
        f.shared
            .last_message_ids
            .get(&serenity::ChannelId::new(f.channel))
            .map(|id| *id),
        Some(f.message),
        "the actual gateway checkpoint follows its custody commit"
    );
}

async fn release_and_assert_one_effect(f: &Fixture) {
    f.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.effects.load(Ordering::SeqCst) == 0 || !f.disk().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("only the real readiness callback drains the saved original");
    assert!(host::channel_accepts(f.channel));
    assert_eq!(
        f.effects.load(Ordering::SeqCst),
        1,
        "one input effect after readiness"
    );
    assert_eq!(
        f.shared
            .last_message_ids
            .get(&serenity::ChannelId::new(f.channel))
            .map(|id| *id),
        Some(f.message)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn writer_boot_native_gateway_redelivery_starts_one_original_after_ready_pg() {
    scenario(
        "boot_input_gateway_tests::writer_boot_native_gateway_redelivery_starts_one_original_after_ready_pg",
        73_369_541, "mac-a", |f| Box::pin(async move {
            prepare_gateway(f).await;
            let event = gateway_message(f);
            // Deliver exactly the same Discord Message twice through the universal gateway gate.
            for _ in 0..2 {
                deliver_gateway(f, &event).await;
                assert_one_custody(f);
            }
            assert!(f.shared.dispatch.intake_dedup.contains_key(&format!("mid:{}", f.message)));
            release_and_assert_one_effect(f).await;
        }),
    ).await;
}

#[derive(Clone)]
struct HistoryState {
    channel: u64,
    rows: Vec<Value>,
    fetched: Arc<AtomicUsize>,
    returned: Arc<Mutex<Vec<u64>>>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
    first: Arc<AtomicBool>,
    unhandled: Arc<Mutex<Vec<String>>>,
}

struct HistoryHttp {
    http: Arc<serenity::Http>,
    state: HistoryState,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for HistoryHttp {
    fn drop(&mut self) {
        self.state.release.add_permits(1);
        self.server.abort();
    }
}

fn history_id(row: &Value) -> u64 {
    row["id"].as_str().unwrap().parse().unwrap()
}

async fn history_response(
    axum::extract::State(state): axum::extract::State<HistoryState>,
    request: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    let path = request.uri().path().to_owned();
    if request.method() == axum::http::Method::GET {
        if path == "/api/v10/users/@me" {
            return axum::Json(json!({"id":"1","username":"fixture","discriminator":"0001",
                "avatar":null,"bot":true}))
            .into_response();
        }
        if path == format!("/api/v10/channels/{}", state.channel) {
            return axum::Json(json!({"id":state.channel.to_string(), "type":1,
                "last_message_id":null,"last_pin_timestamp":null,"recipients":[{
                    "id":"7","username":"boot-user","discriminator":"0001","avatar":null,
                    "bot":false}]}))
            .into_response();
        }
        if path == format!("/api/v10/channels/{}/messages", state.channel) {
            state.fetched.fetch_add(1, Ordering::SeqCst);
            if !state.first.swap(true, Ordering::SeqCst) {
                state.entered.notify_one();
                state
                    .release
                    .clone()
                    .acquire_owned()
                    .await
                    .unwrap()
                    .forget();
            }
            let query: std::collections::HashMap<_, _> =
                url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
                    .into_owned()
                    .collect();
            let value = |key: &str| query.get(key).map(|value| value.parse::<u64>().unwrap());
            let (before, after) = (value("before"), value("after"));
            assert!(before.is_none() || after.is_none());
            let limit = value("limit").unwrap_or(50) as usize;
            assert!((1..=100).contains(&limit));
            let mut rows = state.rows.clone();
            rows.retain(|row| {
                before.is_none_or(|before| history_id(row) < before)
                    && after.is_none_or(|after| history_id(row) > after)
            });
            rows.sort_by_key(|row| std::cmp::Reverse(history_id(row)));
            if after.is_some() {
                rows.drain(..rows.len().saturating_sub(limit));
            } else {
                rows.truncate(limit);
            }
            state
                .returned
                .lock()
                .unwrap()
                .extend(rows.iter().map(history_id));
            return axum::Json(rows).into_response();
        }
    }
    state
        .unhandled
        .lock()
        .unwrap()
        .push(format!("{} {}", request.method(), request.uri()));
    (
        axum::http::StatusCode::NOT_FOUND,
        axum::Json(json!({"message":"unexpected history fixture endpoint","code":0})),
    )
        .into_response()
}

impl HistoryHttp {
    async fn start(f: &Fixture) -> Self {
        let mut human =
            discord_mock::history_message_json(f.message, &f.request().user_text, false);
        human["channel_id"] = json!(f.channel.to_string());
        human["author"]["id"] = json!("7");
        let mut anchor = discord_mock::history_message_json(f.message - 1, "previous reply", true);
        anchor["channel_id"] = json!(f.channel.to_string());
        anchor["author"]["id"] = json!("1");
        let state = HistoryState {
            channel: f.channel,
            rows: vec![human, anchor],
            fetched: Arc::new(AtomicUsize::new(0)),
            returned: Arc::new(Mutex::new(Vec::new())),
            entered: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Semaphore::new(0)),
            first: Arc::new(AtomicBool::new(false)),
            unhandled: Arc::new(Mutex::new(Vec::new())),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let app = axum::Router::new()
            .fallback(history_response)
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy(proxy)
                .ratelimiter_disabled(true)
                .build(),
        );
        Self {
            http,
            state,
            server,
        }
    }
}

struct CatchUpTask(tokio::task::JoinHandle<()>);
impl Drop for CatchUpTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn writer_boot_native_gateway_and_inflight_catchup_keep_one_original_pg() {
    scenario(
        "boot_input_gateway_tests::writer_boot_native_gateway_and_inflight_catchup_keep_one_original_pg",
        73_369_542, "mac-a", |f| Box::pin(async move {
            prepare_gateway(f).await;
            let history = HistoryHttp::start(f).await;
            // Only a test-root checkpoint is seeded: the real sweep must select this channel.
            crate::services::discord::advance_last_message_checkpoint(
                &f.shared, &ProviderKind::Claude, serenity::ChannelId::new(f.channel),
                serenity::MessageId::new(f.message - 1),
            );
            let (http, shared) = (history.http.clone(), f.shared.clone());
            let mut catchup = CatchUpTask(tokio::spawn(async move {
                crate::services::discord::catch_up::catch_up_missed_messages(
                    &http, &shared, &ProviderKind::Claude,
                ).await;
            }));
            tokio::time::timeout(Duration::from_secs(5), history.state.entered.notified())
                .await.expect("the actual catch-up must request the seeded history");
            // The real gateway commits while catch-up's same-message fetch is still in flight.
            let event = gateway_message(f);
            deliver_gateway(f, &event).await;
            assert_one_custody(f);
            history.state.release.add_permits(1);
            tokio::time::timeout(Duration::from_secs(10), &mut catchup.0)
                .await.expect("actual catch-up finishes against durable gateway custody").unwrap();
            assert!(history.state.fetched.load(Ordering::SeqCst) > 0);
            assert!(history.state.returned.lock().unwrap().contains(&f.message),
                "same source ID was returned to catch-up, so zero fetched messages cannot pass");
            assert!(history.state.unhandled.lock().unwrap().is_empty(),
                "a mock transport error must not silently suppress recovery");
            assert_one_custody(f);
            release_and_assert_one_effect(f).await;
        }),
    ).await;
}
