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
use crate::services::discord::{ProviderKind, inflight, mailbox_snapshot};
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
        super::boxed_intake(&harness.ctx, &event, &harness.data)
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

// ---- The THREAD-GUARD clean finishes only the witnessed episode ----

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::router::thread_guard_test_api::{clean, witness};
use crate::services::turn_orchestrator::{
    Intervention, InterventionMode, SourceMessageQueuedGeneration,
};

const PARENT_ID: u64 = CHANNEL_ID;

/// A harness whose parent maps to a thread on a confirmed legacy host with a live dispatch, as
/// the test above builds it for `Stored::Legacy`; the tmux stand-in reports every session dead.
async fn legacy_thread_harness() -> (RelayE2eHarness, TestPostgresDb, ModeTmux, String) {
    let mut db = None;
    let storage = async {
        let (fixture, pool) = postgres().await;
        db = Some(fixture);
        Some(pool)
    };
    let harness = RelayE2eHarness::start_inner(ProviderStub::Success, true, storage, false).await;
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
    let shared = &harness.shared;
    (shared.dispatch.thread_parents).insert(ChannelId::new(PARENT_ID), ChannelId::new(THREAD_ID));
    let pool = shared.pg_pool.clone().expect("a runtime on PostgreSQL");
    let name = ProviderKind::Claude.build_tmux_session_name("i6199-thread");
    let host = Host::Case(Case::Stored(Stored::Legacy));
    host.seed(&pool, &channel_key(shared, &name), &name, THREAD_ID)
        .await;
    sqlx::query("INSERT INTO task_dispatches (id, status) VALUES ($1, 'dispatched')")
        .bind(DISPATCH_ID)
        .execute(&pool)
        .await
        .expect("a live dispatch for the bot message");
    (harness, db, tmux, name)
}

/// A soft follow-up queued on a channel.
fn follow_up(message: MessageId) -> Intervention {
    let generation = crate::services::discord::runtime_store::process_generation();
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: message,
        queued_generation: generation,
        source_message_ids: vec![message],
        source_message_queued_generations: vec![SourceMessageQueuedGeneration::user_instruction(
            message, generation,
        )],
        source_text_segments: Vec::new(),
        text: "follow-up".to_string(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    }
}

fn queued_ids(queue: &[Intervention]) -> Vec<u64> {
    queue.iter().map(|item| item.message_id.get()).collect()
}

/// Queue-eligible completions published for `channel` since `rx` subscribed.
fn completions(
    rx: &mut tokio::sync::broadcast::Receiver<
        crate::services::discord::turn_completion_events::TurnCompletionEvent,
    >,
    channel: ChannelId,
) -> usize {
    let mut count = 0;
    while let Ok(event) = rx.try_recv() {
        count += usize::from(event.channel_id == channel && event.queue_is_eligible());
    }
    count
}

async fn intake(harness: &RelayE2eHarness, message: u64) {
    let event = serenity::FullEvent::Message {
        new_message: bot_message(message),
    };
    super::boxed_intake(&harness.ctx, &event, &harness.data)
        .await
        .expect("intake");
}

/// Captures this thread's INFO logs while it lives.
struct Logs {
    lines: Arc<std::sync::Mutex<Vec<u8>>>,
    _default: tracing::subscriber::DefaultGuard,
}

#[derive(Clone)]
struct LogWriter(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
    type Writer = LogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Logs {
    fn capture() -> Self {
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .with_writer(LogWriter(lines.clone()))
            .finish();
        crate::logging::test_capture::pin_callsite_interest();
        let _default = tracing::subscriber::set_default(subscriber);
        Self { lines, _default }
    }

    /// Whether intake committed `message` to a queue through the THREAD-GUARD.
    fn thread_guard_queued(&self, message: u64) -> bool {
        let text = String::from_utf8_lossy(&self.lines.lock().unwrap()).to_string();
        text.lines().any(|line| {
            line.contains("discord intake queue transaction committed")
                && line.contains("thread_guard")
                && line.contains(&message.to_string())
        })
    }
}

// The stale thread turn A is released and its queued follow-up Q stays to run, with one
// completion for the thread, instead of being superseded by a channel clear.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thread_guard_releases_a_stale_thread_and_keeps_its_queue_pg() {
    let (harness, db, tmux, name) = legacy_thread_harness().await;
    let (shared, thread) = (&harness.shared, ChannelId::new(THREAD_ID));
    let (token, _bytes) = stale_thread_turn(&harness, &name).await;
    let q = MessageId::new(THREAD_ID + 5);
    let enqueue = crate::services::discord::mailbox_enqueue_intervention;
    assert!(
        enqueue(shared, &ProviderKind::Claude, thread, follow_up(q))
            .await
            .enqueued
    );
    let before = mailbox_snapshot(shared, thread).await;
    assert_eq!(
        queued_ids(&before.intervention_queue),
        vec![q.get()],
        "Q queued"
    );
    let mut rx = crate::services::discord::turn_completion_events::subscribe_turn_completion_events(
        shared.as_ref(),
    );

    intake(&harness, PARENT_ID + 7).await;

    assert!(token.cancelled.load(Ordering::Relaxed), "A released");
    assert!(!inflight_path().exists(), "A's row cleared");
    let mapped = shared
        .dispatch
        .thread_parents
        .get(&ChannelId::new(PARENT_ID));
    assert_eq!(mapped.map(|entry| *entry.value()), None, "parent released");
    let after = mailbox_snapshot(shared, thread).await;
    let kept = queued_ids(&after.intervention_queue).contains(&q.get())
        || after.active_user_message_id == Some(q);
    assert!(kept, "Q kept: {:?}", queued_ids(&after.intervention_queue));
    assert_eq!(
        completions(&mut rx, thread),
        1,
        "one completion for the thread"
    );
    drop(tmux);
    db.drop().await;
    drop(harness);
}

// A thread turn with no anchored message is left alone: the bot message queues behind it and the
// thread's turn, queue, row and route stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thread_guard_leaves_an_unanchored_thread_turn_alone_pg() {
    let (harness, db, tmux, name) = legacy_thread_harness().await;
    let (shared, thread) = (&harness.shared, ChannelId::new(THREAD_ID));
    let (token, _) = stale_thread_turn(&harness, &name).await;
    // Rebind the turn without an anchor, as a recovery turn with no user message holds it.
    let finish = crate::services::discord::mailbox_finish::mailbox_finish_turn;
    assert!(
        finish(shared, &ProviderKind::Claude, thread)
            .await
            .removed_token
            .is_some()
    );
    let handle = shared.mailbox(thread);
    handle
        .recovery_kickoff(token.clone(), UserId::new(7), None)
        .await;
    let path = inflight_path();
    let mut row: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    row["user_msg_id"] = 0.into();
    row["finalizer_turn_id"] = 0.into();
    std::fs::write(&path, serde_json::to_vec_pretty(&row).unwrap()).unwrap();
    let q = MessageId::new(THREAD_ID + 5);
    let enqueue = crate::services::discord::mailbox_enqueue_intervention;
    assert!(
        enqueue(shared, &ProviderKind::Claude, thread, follow_up(q))
            .await
            .enqueued
    );
    let before = mailbox_snapshot(shared, thread).await;
    assert!(before.active_user_message_id.is_none() && before.cancel_token.is_some());
    let bytes = std::fs::read(&path).unwrap();

    intake(&harness, PARENT_ID + 7).await;

    let after = mailbox_snapshot(shared, thread).await;
    let owned = (after.cancel_token.as_ref()).is_some_and(|t| Arc::ptr_eq(t, &token));
    assert!(
        owned && !token.cancelled.load(Ordering::Relaxed),
        "turn kept"
    );
    assert_eq!(
        queued_ids(&after.intervention_queue),
        vec![q.get()],
        "Q kept"
    );
    // The verdict's writing load may backfill the finalizer id; the episode the row names stays.
    let episode = |bytes: &[u8]| {
        let row: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        ["user_msg_id", "turn_nonce", "started_at", "channel_id"].map(|key| row[key].clone())
    };
    let kept = std::fs::read(&path).expect("the row stays");
    assert_eq!(
        episode(&kept),
        episode(&bytes),
        "row names the same episode"
    );
    let mapped = shared
        .dispatch
        .thread_parents
        .get(&ChannelId::new(PARENT_ID));
    assert_eq!(
        mapped.map(|entry| *entry.value()),
        Some(thread),
        "route kept"
    );
    let parent = mailbox_snapshot(shared, ChannelId::new(PARENT_ID)).await;
    assert_eq!(
        queued_ids(&parent.intervention_queue),
        vec![PARENT_ID + 7],
        "queued behind"
    );
    assert_eq!(harness.provider_starts(), 0);
    drop(tmux);
    db.drop().await;
    drop(harness);
}

// A ends normally after the witness and B claims, writes its row, queues and routes: the clean
// leaves all of it, and the parent's next bot message queues behind B through the THREAD-GUARD.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thread_guard_keeps_a_successor_admitted_after_the_witness_pg() {
    let (harness, db, tmux, name) = legacy_thread_harness().await;
    let (shared, thread) = (&harness.shared, ChannelId::new(THREAD_ID));
    let parent = ChannelId::new(PARENT_ID);
    let provider = ProviderKind::Claude;
    let (_a, _) = stale_thread_turn(&harness, &name).await;
    let w = witness(shared, &provider, thread)
        .await
        .expect("an admitted stale thread");
    // A ends on its normal path: mailbox finish and its own row cleared.
    let finish = crate::services::discord::mailbox_finish::mailbox_finish_turn;
    assert!(
        finish(shared, &provider, thread)
            .await
            .removed_token
            .is_some()
    );
    assert!(inflight::clear_inflight_state(&provider, THREAD_ID));
    // B claims, writes its row, queues behind itself and publishes its route.
    let b = Arc::new(CancelToken::new());
    let b_message = MessageId::new(THREAD_ID + 2);
    let start = crate::services::discord::mailbox_try_start_turn;
    assert!(start(shared, thread, b.clone(), UserId::new(7), b_message).await);
    let mut row = inflight::InflightTurnState::new(
        provider.clone(),
        THREAD_ID,
        None,
        7,
        b_message.get(),
        b_message.get() + 1,
        "successor".to_string(),
        None,
        Some(name.clone()),
        None,
        None,
        0,
    );
    row.turn_nonce = b.turn_nonce().map(str::to_string);
    inflight::save_inflight_state_create_new(&row).expect("B's row");
    let qb = MessageId::new(THREAD_ID + 3);
    let enqueue = crate::services::discord::mailbox_enqueue_intervention;
    assert!(
        enqueue(shared, &provider, thread, follow_up(qb))
            .await
            .enqueued
    );
    shared.dispatch.thread_parents.insert(parent, thread);
    let before = mailbox_snapshot(shared, thread).await;
    assert!(
        before
            .cancel_token
            .as_ref()
            .is_some_and(|t| Arc::ptr_eq(t, &b)),
        "B holds"
    );
    let bytes = std::fs::read(inflight_path()).expect("B's row on disk");
    let counter = shared.restart.global_active.load(Ordering::Relaxed);
    let starts = harness.provider_starts();

    let released = clean(shared, &provider, parent, thread, w).await;

    let after = mailbox_snapshot(shared, thread).await;
    assert!(
        after
            .cancel_token
            .as_ref()
            .is_some_and(|t| Arc::ptr_eq(t, &b)),
        "B holds"
    );
    assert!(!b.cancelled.load(Ordering::Relaxed));
    assert_eq!(after.active_user_message_id, Some(b_message));
    assert_eq!(after.active_turn_nonce, before.active_turn_nonce);
    assert_eq!(queued_ids(&after.intervention_queue), vec![qb.get()]);
    assert_eq!(
        std::fs::read(inflight_path()).unwrap(),
        bytes,
        "B's row bytes"
    );
    assert_eq!(
        shared.restart.global_active.load(Ordering::Relaxed),
        counter
    );
    let mapped = shared.dispatch.thread_parents.get(&parent);
    assert_eq!(
        mapped.map(|entry| *entry.value()),
        Some(thread),
        "B's route"
    );
    // The parent stays idle; its next bot message queues behind B through the THREAD-GUARD.
    let idle = mailbox_snapshot(shared, parent).await;
    assert!(idle.cancel_token.is_none() && idle.intervention_queue.is_empty());
    let logs = Logs::capture();
    intake(&harness, PARENT_ID + 8).await;
    let queued = logs.thread_guard_queued(PARENT_ID + 8);
    drop(logs);
    let parent_after = mailbox_snapshot(shared, parent).await;
    assert_eq!(
        queued_ids(&parent_after.intervention_queue),
        vec![PARENT_ID + 8]
    );
    assert!(
        parent_after.cancel_token.is_none(),
        "nothing runs on the parent"
    );
    assert_eq!(harness.provider_starts(), starts);
    assert!(queued, "queued through the THREAD-GUARD");
    assert!(!released, "A's episode no longer holds the thread");
    drop(tmux);
    db.drop().await;
    drop(harness);
}

// The clean is held after its actor release while B claims and routes; once it finishes, B runs
// with its route kept and the idle parent's next bot message queues behind B (THREAD-GUARD).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thread_guard_keeps_a_successor_route_published_after_the_release_pg() {
    use crate::services::discord::router::thread_guard_test_api::pause;
    let (harness, db, tmux, name) = legacy_thread_harness().await;
    let (shared, thread) = (&harness.shared, ChannelId::new(THREAD_ID));
    let parent = ChannelId::new(PARENT_ID);
    let provider = ProviderKind::Claude;
    let (a, _) = stale_thread_turn(&harness, &name).await;
    let idle = mailbox_snapshot(shared, parent).await;
    assert!(
        idle.cancel_token.is_none() && idle.intervention_queue.is_empty(),
        "idle parent"
    );
    let w = witness(shared, &provider, thread)
        .await
        .expect("an admitted stale thread");
    let mut rx = crate::services::discord::turn_completion_events::subscribe_turn_completion_events(
        shared.as_ref(),
    );
    let gate = pause::arm(thread);
    let cleaner = {
        let (shared, provider) = (shared.clone(), provider.clone());
        tokio::spawn(async move { clean(&shared, &provider, parent, thread, w).await })
    };
    gate.reached.notified().await;
    assert!(
        !a.cancelled.load(Ordering::Relaxed),
        "A finalized only after the pause"
    );
    assert_eq!(
        completions(&mut rx, thread),
        0,
        "no completion before the cleanup"
    );
    // B claims the released thread and publishes its route, then waits short of its row.
    let b = Arc::new(CancelToken::new());
    let b_message = MessageId::new(THREAD_ID + 2);
    let start = crate::services::discord::mailbox_try_start_turn;
    assert!(start(shared, thread, b.clone(), UserId::new(7), b_message).await);
    shared.dispatch.thread_parents.insert(parent, thread);

    gate.resume.notify_one();
    assert!(cleaner.await.unwrap(), "A was released");
    assert_eq!(
        completions(&mut rx, thread),
        1,
        "one completion after the cleanup"
    );
    assert!(a.cancelled.load(Ordering::Relaxed), "A finalized");

    let mut row = inflight::InflightTurnState::new(
        provider.clone(),
        THREAD_ID,
        None,
        7,
        b_message.get(),
        b_message.get() + 1,
        "successor".to_string(),
        None,
        Some(name.clone()),
        None,
        None,
        0,
    );
    row.turn_nonce = b.turn_nonce().map(str::to_string);
    inflight::save_inflight_state_create_new(&row).expect("B starts: its row is written");
    let held = mailbox_snapshot(shared, thread).await;
    assert!(
        held.cancel_token
            .as_ref()
            .is_some_and(|t| Arc::ptr_eq(t, &b)),
        "B holds"
    );
    let mapped = shared.dispatch.thread_parents.get(&parent);
    assert_eq!(
        mapped.map(|entry| *entry.value()),
        Some(thread),
        "B's route"
    );
    let starts = harness.provider_starts();
    let logs = Logs::capture();
    intake(&harness, PARENT_ID + 8).await;
    let queued = logs.thread_guard_queued(PARENT_ID + 8);
    drop(logs);
    let after = mailbox_snapshot(shared, parent).await;
    assert_eq!(queued_ids(&after.intervention_queue), vec![PARENT_ID + 8]);
    assert!(after.cancel_token.is_none(), "nothing runs on the parent");
    assert_eq!(harness.provider_starts(), starts);
    assert!(queued, "queued through the THREAD-GUARD");
    drop(tmux);
    db.drop().await;
    drop(harness);
}
