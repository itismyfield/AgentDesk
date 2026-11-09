//! A kept front whose Discord message a recorded turn already completed, on any node, leaves the
//! queue without a turn; without that `completed` receipt, or with merged followers, it starts.
use crate::db::turns::{TerminalOutcome, TurnReceipt, record_turn_receipt};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests;
use crate::services::discord::queue_io::input_ingress;
use crate::services::discord::shared_state::test_rest;
use crate::services::discord::{Intervention, SharedData};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::InterventionMode;
use poise::serenity_prelude::{self as serenity, ChannelId, MessageId, UserId};
use serde_json::{Value, json};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};

/// The channel and message a standby kept in its queue file when it fenced during a handback.
const CHANNEL: u64 = 1_509_350_490_461_180_105;
const MESSAGE: u64 = 1_569_308_719_030_865_952;
/// That file's admission receipt begins with this ticket, derived from provider, channel, sources.
const KEPT_TICKET: &str = "tk:discord-513bb3abdb41af218d4d4ffcf73fa0c3";
const ANNOUNCE_BOT: u64 = 1_479_017_284_805_722_200;
const RETIRED_LOG: &str = "owner front retired";

/// Restores the config before the root guard releases the shared environment lock.
struct Env {
    _config: crate::config::TestEnvVarGuard,
    _root: crate::config::TestRuntimeRootGuard,
}

fn env() -> Env {
    let root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let config = crate::runtime_layout::config_file_path(root.path());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "server: {}\ndata: {dir: data}\n").unwrap();
    let config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "977").unwrap();
    Env {
        _config: config,
        _root: root,
    }
}

type Posts = Arc<Mutex<Vec<Value>>>;

/// Discord REST on this thread; keeps every request body the code sends.
async fn rest() -> (Posts, test_rest::Guard) {
    use axum::{Json, Router, body::Bytes};
    let posts = Posts::default();
    let kept = posts.clone();
    let app = Router::new().fallback(move |body: Bytes| {
        let kept = kept.clone();
        async move {
            kept.lock()
                .unwrap()
                .push(serde_json::from_slice(&body).unwrap_or(Value::Null));
            Json(json!({
                "id": "1", "channel_id": CHANNEL.to_string(), "content": "",
                "author": {"id":"1", "username":"bot", "discriminator":"0001", "avatar":null},
                "timestamp":"2026-11-09T00:00:00+00:00", "edited_timestamp":null,
                "tts":false, "mention_everyone":false, "mentions":[], "mention_roles":[],
                "attachments":[], "embeds":[], "pinned":false, "type":0
            }))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        serenity::HttpBuilder::new("settled-front")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (posts, test_rest::install(http))
}

fn queue_notices(posts: &Posts) -> usize {
    let posts = posts.lock().unwrap();
    posts
        .iter()
        .filter(|body| body["content"] == "큐에 들어갔습니다")
        .count()
}

/// A process of one node on this root: its owner adopts the channel's queue file as recovery does.
async fn node(pool: &sqlx::PgPool) -> Arc<SharedData> {
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    shared
        .http
        .cached_bot_token
        .set("settled-front".into())
        .unwrap();
    let channel = ChannelId::new(CHANNEL);
    let context = crate::services::discord::queue_dispatch::persistence_context(
        &shared,
        &ProviderKind::Claude,
        channel,
    );
    let (path, namespace) =
        crate::services::turn_orchestrator::v2_queue(&context, channel).unwrap();
    shared
        .mailbox(channel)
        .v2_ensure(path, namespace)
        .await
        .unwrap();
    shared
}

fn arrival(message: u64, author: u64, bot: bool, merge: bool) -> Intervention {
    Intervention {
        author_id: UserId::new(author),
        author_is_bot: bot,
        message_id: MessageId::new(message),
        queued_generation: 977,
        source_message_ids: vec![MessageId::new(message)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("failover probe {message}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: merge,
        pending_uploads: Vec::new(),
    }
}

/// Catch-up's queue seam during a restart window, which owes the arrival its queue notice.
async fn catch_up_enqueue(shared: &Arc<SharedData>, item: &Intervention) {
    shared.restart.reconcile_done.store(false, Relaxed);
    let queued =
        input_ingress::enqueue(shared, &ProviderKind::Claude, ChannelId::new(CHANNEL), item).await;
    assert!(queued.is_some_and(|outcome| outcome.enqueued));
}

async fn record(pool: &sqlx::PgPool, message: u64, outcome: TerminalOutcome, aged_days: i64) {
    let (turn_id, channel_id) = (format!("discord:{CHANNEL}:{message}"), CHANNEL.to_string());
    let receipt = TurnReceipt {
        turn_id: &turn_id,
        channel_id: &channel_id,
        agent_id: None,
        provider: "claude",
        session_key: None,
        started_at: None,
        consumed_origin_ids: &[],
        outcome,
        confirmed_at: chrono::Utc::now() - chrono::Duration::days(aged_days),
    };
    record_turn_receipt(pool, &receipt).await.unwrap();
}

async fn items(shared: &Arc<SharedData>) -> Vec<u64> {
    let state = shared
        .mailbox(ChannelId::new(CHANNEL))
        .v2_request(|slot| slot.as_ref().ok_or("no owner")?.json())
        .await
        .unwrap();
    let items = state["items"].as_array().unwrap();
    items
        .iter()
        .map(|item| item["message_id"].as_u64().unwrap())
        .collect()
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn start_front(shared: &Arc<SharedData>) -> (Result<(), String>, Vec<String>) {
    start_front_logging(shared, RETIRED_LOG).await
}

/// The idle start the owner's drive runs for the granted front, with its log lines naming `kind`.
async fn start_front_logging(
    shared: &Arc<SharedData>,
    kind: &str,
) -> (Result<(), String>, Vec<String>) {
    shared.restart.reconcile_done.store(true, Relaxed);
    let channel = ChannelId::new(CHANNEL);
    let front = shared
        .mailbox(channel)
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let front = owner.front().ok_or("no front")?;
            owner.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap();
    let sink = Sink::default();
    let writer = sink.clone();
    crate::logging::test_capture::pin_callsite_interest();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let result = {
        let _log = tracing::subscriber::set_default(subscriber);
        let start = super::super::start_idle(shared.clone(), channel, front);
        crate::test_support::wait::await_within(start, crate::test_support::wait::WAIT_CAP).await
    };
    let log = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    let retired = log
        .lines()
        .filter(|line| line.contains(kind))
        .map(str::to_string)
        .collect();
    (result, retired)
}

/// This node is not the channel's O home, as the standby was, so a start it may not run is held.
fn standby_site() -> crate::services::tui_o::cutover::test_override::ChannelsGuard {
    crate::services::tui_o::cutover::test_override::force_foreign(
        &[(CHANNEL, RuntimeHandoffKind::ClaudeTui)],
        "mac-book-release",
    )
}

fn assert_started_and_held(result: &Result<(), String>, retired: &[String]) {
    let error = result.as_ref().expect_err("a start that ran admission");
    assert!(error.contains("owner_front_not_admitted"), "{error}");
    assert!(retired.is_empty(), "{retired:?}");
}

/// The standby holds the arrival and keeps it in its queue file through the fence; the preferred
/// node then completes the same message; the standby's next lease retires the kept front.
#[tokio::test(flavor = "current_thread")]
async fn a_kept_front_another_node_completed_leaves_without_a_turn_pg() {
    let _env = env();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = ChannelId::new(CHANNEL);
    let previous = set_owner_wake_for_tests(channel, Some(Arc::new(|| {})));
    let _site = standby_site();
    let (posts, _rest) = rest().await;
    let path = {
        let standby = node(&pool).await;
        catch_up_enqueue(&standby, &arrival(MESSAGE, ANNOUNCE_BOT, true, false)).await;
        crate::services::discord::input_queue_path_for_test(&standby, CHANNEL)
    };
    assert_eq!(
        queue_notices(&posts),
        1,
        "the standby noticed its hold once"
    );
    let kept: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let keys = |value: &Value| {
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    };
    assert_eq!(
        keys(&kept),
        [
            "admission_binds",
            "admission_receipts",
            "admission_sequence",
            "head_injection",
            "items",
            "namespace",
            "retained_inputs",
            "terminal_identities",
            "version"
        ]
    );
    assert_eq!(
        keys(&kept["items"][0]),
        [
            "author_id",
            "author_is_bot",
            "channel_id",
            "channel_name",
            "created_at_wall_time_ms",
            "has_reply_boundary",
            "merge_consecutive",
            "message_id",
            "override_channel_id",
            "queue_notice_claimed",
            "queued_generation",
            "reply_context",
            "source_message_ids",
            "source_message_queued_generations",
            "text"
        ]
    );
    assert_eq!(kept["version"], 2);
    assert_eq!(kept["items"][0]["message_id"], MESSAGE);
    assert_eq!(kept["items"][0]["queue_notice_claimed"], true);
    assert_eq!(kept["admission_binds"][0]["at"]["kind"], "queued");
    let receipt = kept["admission_receipts"][0].as_str().unwrap();
    assert!(receipt.starts_with(&format!("{KEPT_TICKET}:")), "{receipt}");

    record(&pool, MESSAGE, TerminalOutcome::Completed, 0).await;
    let standby = node(&pool).await;
    let (result, retired) = start_front(&standby).await;
    assert_eq!(result, Ok(()), "the retired front settles its drive");
    assert!(items(&standby).await.is_empty(), "no kept input remains");
    assert_eq!(retired.len(), 1, "{retired:?}");
    assert!(retired[0].contains(&MESSAGE.to_string()), "{retired:?}");
    let owed = standby
        .mailbox(channel)
        .v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.owed_drops().len()))
        .await
        .unwrap();
    assert_eq!(
        owed, 0,
        "a Discord input carries no origin to owe a receipt to"
    );
    assert_eq!(queue_notices(&posts), 1, "retiring posts no second notice");
    set_owner_wake_for_tests(channel, previous);
    pool.close().await;
    db.drop().await;
}

/// Before any node completed the message the kept front starts as before; once a turn completed
/// it, the next start retires it. A cancelled turn may be retried, so it never retires.
#[tokio::test(flavor = "current_thread")]
async fn only_a_completed_receipt_retires_and_its_absence_starts_as_before_pg() {
    let _env = env();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = ChannelId::new(CHANNEL);
    let previous = set_owner_wake_for_tests(channel, Some(Arc::new(|| {})));
    let _site = standby_site();
    let (_posts, _rest) = rest().await;
    let standby = node(&pool).await;
    catch_up_enqueue(&standby, &arrival(MESSAGE, ANNOUNCE_BOT, true, false)).await;

    let (result, retired) = start_front(&standby).await;
    assert_started_and_held(&result, &retired);
    assert_eq!(items(&standby).await, [MESSAGE]);

    record(&pool, MESSAGE, TerminalOutcome::Cancelled, 0).await;
    let (result, retired) = start_front(&standby).await;
    assert_started_and_held(&result, &retired);
    assert_eq!(
        items(&standby).await,
        [MESSAGE],
        "a cancelled turn keeps it"
    );

    let other = MESSAGE + 1;
    record(&pool, other, TerminalOutcome::Completed, 0).await;
    let (result, retired) = start_front(&standby).await;
    assert_started_and_held(&result, &retired);
    assert_eq!(
        items(&standby).await,
        [MESSAGE],
        "another message's turn keeps it"
    );
    set_owner_wake_for_tests(channel, previous);
    pool.close().await;
    db.drop().await;
}

/// A front carrying merged followers is one input; its primary's receipt does not prove them.
#[tokio::test(flavor = "current_thread")]
async fn a_front_with_merged_followers_starts_as_before_pg() {
    let _env = env();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = ChannelId::new(CHANNEL);
    let previous = set_owner_wake_for_tests(channel, Some(Arc::new(|| {})));
    let _site = standby_site();
    let (_posts, _rest) = rest().await;
    let standby = node(&pool).await;
    for message in [MESSAGE, MESSAGE + 1] {
        catch_up_enqueue(&standby, &arrival(message, 343_742_347, false, true)).await;
    }
    let followers = standby
        .mailbox(channel)
        .v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.followers().len()))
        .await
        .unwrap();
    assert_eq!(followers, 1, "the second arrival merges behind the front");
    record(&pool, MESSAGE, TerminalOutcome::Completed, 0).await;

    let (result, retired) = start_front(&standby).await;
    assert_started_and_held(&result, &retired);
    assert_eq!(items(&standby).await, [MESSAGE, MESSAGE + 1]);
    set_owner_wake_for_tests(channel, previous);
    pool.close().await;
    db.drop().await;
}

/// The preferred node's own catch-up queued a message another node already completed, and
/// retention since moved that receipt to the archive; the start still retires it.
#[tokio::test(flavor = "current_thread")]
async fn a_catch_up_front_retires_on_an_archived_receipt_pg() {
    let _env = env();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = ChannelId::new(CHANNEL);
    let previous = set_owner_wake_for_tests(channel, Some(Arc::new(|| {})));
    let (_posts, _rest) = rest().await;
    let preferred = node(&pool).await;
    catch_up_enqueue(&preferred, &arrival(MESSAGE, ANNOUNCE_BOT, true, false)).await;
    record(&pool, MESSAGE, TerminalOutcome::Completed, 91).await;
    crate::services::maintenance::jobs::db_retention::db_retention_job(&pool, false)
        .await
        .unwrap();
    let hot: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE turn_id = $1")
        .bind(format!("discord:{CHANNEL}:{MESSAGE}"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hot, 0, "the receipt lives only in the archive");

    let (result, retired) = start_front(&preferred).await;
    assert_eq!(result, Ok(()));
    assert!(items(&preferred).await.is_empty());
    assert_eq!(retired.len(), 1, "{retired:?}");
    set_owner_wake_for_tests(channel, previous);
    pool.close().await;
    db.drop().await;
}

/// A receipt read that fails starts the front as before and warns once.
#[tokio::test(flavor = "current_thread")]
async fn an_unreadable_receipt_starts_the_front_as_before_pg() {
    let _env = env();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = ChannelId::new(CHANNEL);
    let previous = set_owner_wake_for_tests(channel, Some(Arc::new(|| {})));
    let _site = standby_site();
    let (_posts, _rest) = rest().await;
    let standby = node(&pool).await;
    catch_up_enqueue(&standby, &arrival(MESSAGE, ANNOUNCE_BOT, true, false)).await;
    record(&pool, MESSAGE, TerminalOutcome::Completed, 0).await;
    sqlx::query("ALTER TABLE turns RENAME TO turns_unreadable")
        .execute(&pool)
        .await
        .unwrap();

    let (result, unread) = start_front_logging(&standby, "owner front completion unread").await;
    assert_started_and_held(&result, &[]);
    assert_eq!(unread.len(), 1, "{unread:?}");
    assert!(unread[0].contains("WARN"), "{unread:?}");
    assert_eq!(items(&standby).await, [MESSAGE]);
    set_owner_wake_for_tests(channel, previous);
    pool.close().await;
    db.drop().await;
}
