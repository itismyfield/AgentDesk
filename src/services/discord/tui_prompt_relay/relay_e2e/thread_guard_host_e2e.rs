//! A bot message to a parent whose dispatch thread holds a stale turn on an unconfirmed host:
//! production intake queues it behind the thread and leaves the thread's turn as it was.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use super::{CHANNEL_ID, ProviderStub, RelayE2eHarness};
use crate::services::discord::admin_host_guard::tests::{Host, ModeTmux, Tmux};
use crate::services::discord::host_defer_gate::tests::{Case, postgres};
use crate::services::discord::host_teardown_gate::test_support::{Stored, channel_key};
use crate::services::discord::{ProviderKind, inflight, mailbox_snapshot, router};
use crate::services::provider::CancelToken;
use crate::services::provider::cancel_token_cleanup::authority::TmuxBinding;

const THREAD_ID: u64 = CHANNEL_ID + 100;
const ALLOWED_BOT_ID: u64 = 940_487_400_000_009;
const DISPATCH_ID: &str = "1f3c2b1a-0000-4000-8000-000000000000";

/// A thread turn bound to `name` whose inflight row went stale and predates the finalizer id,
/// so a writing load would backfill it; returns the token and the row's bytes on disk.
async fn stale_thread_turn(harness: &RelayE2eHarness, name: &str) -> (Arc<CancelToken>, Vec<u8>) {
    let (shared, thread) = (&harness.shared, ChannelId::new(THREAD_ID));
    let token = Arc::new(CancelToken::new());
    let binding = TmuxBinding::NameOnly {
        name: name.to_string(),
    };
    *token.tmux_binding.lock().unwrap() = Some(binding);
    let message = MessageId::new(THREAD_ID + 1);
    let start = crate::services::discord::mailbox_try_start_turn;
    assert!(start(shared, thread, token.clone(), UserId::new(7), message).await);
    shared.restart.global_active.store(1, Ordering::Relaxed);
    let row = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        THREAD_ID,
        None,
        1,
        message.get(),
        message.get() + 1,
        "stale thread turn".to_string(),
        None,
        Some(name.to_string()),
        None,
        None,
        0,
    );
    let threshold = inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64;
    let stale = chrono::Local::now() - chrono::Duration::seconds(threshold + 5);
    let stale = stale.format("%Y-%m-%d %H:%M:%S").to_string();
    let mut wire = serde_json::to_value(&row).unwrap();
    wire["updated_at"] = stale.clone().into();
    wire["started_at"] = stale.into();
    wire["turn_nonce"] = token.turn_nonce().map(str::to_string).into();
    wire.as_object_mut().unwrap().remove("finalizer_turn_id");
    let path = inflight_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(&wire).unwrap()).unwrap();
    (token, std::fs::read(&path).unwrap())
}

fn inflight_path() -> std::path::PathBuf {
    let root = inflight::inflight_runtime_root().expect("runtime root");
    root.join("claude").join(format!("{THREAD_ID}.json"))
}

fn bot_message(id: u64) -> serenity::Message {
    let mut message = serenity::Message::default();
    message.id = MessageId::new(id);
    message.channel_id = ChannelId::new(CHANNEL_ID);
    message.author.id = UserId::new(ALLOWED_BOT_ID);
    message.author.name = "report-bot".to_string();
    message.author.bot = true;
    message.content = format!("DISPATCH:{DISPATCH_ID} report");
    message.timestamp = message.id.created_at();
    message
}

// A refused host reads as an active thread: intake queues the bot message, runs nothing and
// keeps the thread's mapping, turn, counter and row bytes; a legacy one is cleaned as in main.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn intake_queues_behind_a_stale_thread_on_an_unconfirmed_host_pg() {
    for stored in [Stored::Hosted, Stored::LegacyHerdrMarker, Stored::Legacy] {
        let mut db = None;
        let storage = async {
            let (fixture, pool) = postgres().await;
            db = Some(fixture);
            Some(pool)
        };
        let start = RelayE2eHarness::start_inner(ProviderStub::Success, true, storage, false);
        let harness = start.await;
        let db = db.expect("a database under the harness lock");
        let tmux = ModeTmux::install();
        tmux.serve(Tmux::Dead);
        let registry = harness.health_registry.clone().expect("a health registry");
        registry
            .register("claude".to_string(), harness.shared.clone())
            .await;
        harness.register_channel_in_role_map();
        harness.answer_placeholders_immediately();
        harness.shared.settings.write().await.allowed_bot_ids = vec![ALLOWED_BOT_ID];
        let (shared, thread) = (&harness.shared, ChannelId::new(THREAD_ID));
        shared
            .dispatch
            .thread_parents
            .insert(ChannelId::new(CHANNEL_ID), thread);
        let pool = shared.pg_pool.clone().expect("a runtime on PostgreSQL");
        let name = ProviderKind::Claude.build_tmux_session_name("p4r1b-thread");
        let host = Host::Case(Case::Stored(stored));
        host.seed(&pool, &channel_key(shared, &name), &name, THREAD_ID)
            .await;
        let (token, bytes) = stale_thread_turn(&harness, &name).await;
        sqlx::query("INSERT INTO task_dispatches (id, status) VALUES ($1, 'dispatched')")
            .bind(DISPATCH_ID)
            .execute(&pool)
            .await
            .expect("a live dispatch for the bot message");

        let event = serenity::FullEvent::Message {
            new_message: bot_message(CHANNEL_ID + 7),
        };
        router::handle_event(&harness.ctx, &event, &harness.data)
            .await
            .expect("intake");

        let label = format!("{stored:?}");
        let mapped = shared
            .dispatch
            .thread_parents
            .get(&ChannelId::new(CHANNEL_ID));
        let mapped = mapped.map(|entry| *entry.value());
        if host.admitted() {
            assert_eq!(mapped, None, "{label}: force-cleaned as in main");
            assert!(token.cancelled.load(Ordering::Relaxed), "{label}");
        } else {
            let parent = mailbox_snapshot(shared, ChannelId::new(CHANNEL_ID)).await;
            assert_eq!(parent.intervention_queue.len(), 1, "{label}: queued");
            assert_eq!(parent.active_user_message_id, None, "{label}: no turn");
            assert_eq!(harness.provider_starts(), 0, "{label}");
            assert_eq!(harness.placeholder_posts(), 0, "{label}");
            assert_eq!(mapped, Some(thread), "{label}: mapping kept");
            let turn = mailbox_snapshot(shared, thread).await;
            let owned = turn
                .cancel_token
                .as_ref()
                .is_some_and(|t| Arc::ptr_eq(t, &token));
            assert!(owned && !token.cancelled.load(Ordering::Relaxed), "{label}");
            let counter = shared.restart.global_active.load(Ordering::Relaxed);
            assert_eq!(counter, 1, "{label}");
            let now = std::fs::read(inflight_path()).unwrap();
            assert_eq!(now, bytes, "{label}: inflight bytes unchanged");
            assert_eq!(tmux.take_writes(), Vec::<String>::new(), "{label}");
        }
        drop(tmux);
        pool.close().await;
        db.drop().await;
        drop(harness);
    }
}
