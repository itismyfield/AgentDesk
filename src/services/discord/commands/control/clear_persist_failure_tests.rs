//! A user clear commits in memory whatever its writes do, and reports what did not land.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use super::{ClearReport, clear_channel_session_state, clear_channel_session_state_fenced};
use crate::services::discord::{DiscordSession, SharedData, make_shared_data_for_tests};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::test_support::{fail_queue_saves, queue_save_faults};
use crate::services::turn_orchestrator::{Intervention, InterventionMode, QueuePersistenceContext};

pub(super) const SESSION_ID: &str = "provider-session-6233";
const CHANNEL_NAME: &str = "adk-6233-clear-persist";

fn queued(message_id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(1),
        author_is_bot: false,
        message_id: MessageId::new(message_id),
        queued_generation: crate::services::discord::runtime_store::process_generation(),
        source_message_ids: vec![MessageId::new(message_id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "restored backlog".to_string(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

pub(super) async fn seed_session(shared: &Arc<SharedData>, channel_id: ChannelId) {
    shared.core.lock().await.sessions.insert(
        channel_id,
        DiscordSession {
            session_id: Some(SESSION_ID.to_string()),
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(channel_id.get()),
            // A resolvable channel name keeps session-key lookup off Discord.
            channel_name: Some(CHANNEL_NAME.to_string()),
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: shared.restart.current_generation,
        },
    );
}

pub(super) async fn seed_backlog(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) {
    let persistence = QueuePersistenceContext::new(provider, &shared.token_hash, None);
    let enqueued = shared
        .mailbox(channel_id)
        .enqueue(queued(channel_id.get() + 1), persistence)
        .await;
    assert!(enqueued.enqueued, "the backlog is queued and persisted");
}

fn paused_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("test runtime")
}

async fn clear(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> anyhow::Result<ClearReport> {
    let http = Arc::new(serenity::Http::new(""));
    clear_channel_session_state_fenced(&http, shared, provider, channel_id, "/clear", None).await
}

pub(super) async fn session_state(
    shared: &SharedData,
    channel_id: ChannelId,
) -> (Option<String>, bool) {
    let data = shared.core.lock().await;
    let session = data.sessions.get(&channel_id).expect("session kept");
    (session.session_id.clone(), session.cleared)
}

pub(super) async fn queue_len(shared: &SharedData, channel_id: ChannelId) -> usize {
    shared
        .mailbox(channel_id)
        .snapshot()
        .await
        .intervention_queue
        .len()
}

fn global_active(shared: &SharedData) -> usize {
    shared.restart.global_active.load(Ordering::SeqCst)
}

const RETRY_FAILED: &str = "⚠️ 일부 저장을 한 번 재시도했지만 완료하지 못했어요. 재시작 후 버린 메시지나 이전 세션이 복원될 수 있어요.";

#[test]
fn a_clear_whose_queue_write_fails_twice_still_resets_the_session_and_warns() {
    let root = tempfile::tempdir().expect("scratch runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let provider = ProviderKind::Gemini;
    let channel_id = ChannelId::new(6_233_101);
    paused_runtime().block_on(async {
        let shared = make_shared_data_for_tests();
        seed_session(&shared, channel_id).await;
        seed_backlog(&shared, &provider, channel_id).await;
        fail_queue_saves(channel_id, 2);

        let report = clear(&shared, &provider, channel_id)
            .await
            .expect("a clear always commits");

        assert_eq!(queue_save_faults(channel_id, true), 0, "both attempts ran");
        assert_eq!(report.failed, ["queue_publish", "pg_boundary"]);
        assert_eq!(
            report.reply(),
            format!("세션을 초기화했어요.\n대기 메시지 1건을 비웠어요.\n{RETRY_FAILED}")
        );
        assert_eq!(
            queue_len(&shared, channel_id).await,
            0,
            "the backlog is discarded"
        );
        assert_eq!(session_state(&shared, channel_id).await, (None, true));
        assert!(
            !shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id),
            "nothing is restored, so nothing is re-armed"
        );
    });
}

#[test]
fn a_clear_whose_write_fails_stops_the_released_turn_once_and_runs_no_old_backlog() {
    let root = tempfile::tempdir().expect("scratch runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let provider = ProviderKind::Gemini;
    let channel_id = ChannelId::new(6_233_103);
    paused_runtime().block_on(async {
        let shared = make_shared_data_for_tests();
        seed_session(&shared, channel_id).await;
        // A tmux-bound token parks the stop in its hard-stop grace; the name
        // is absent, so nothing is signalled or killed.
        let token = Arc::new(CancelToken::new());
        token.bind_unmanaged_session_name(&format!("adk-6233-absent-{}", std::process::id()));
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                channel_id,
                token.clone(),
                UserId::new(1),
                MessageId::new(6_233_100),
            )
            .await,
            "the turn being cleared holds the mailbox"
        );
        // Another channel's turn keeps one more count, so a missing or a
        // doubled decrement is visible.
        crate::services::discord::increment_global_active(&shared, "other channel turn");
        crate::services::discord::increment_global_active(&shared, "cleared turn");
        seed_backlog(&shared, &provider, channel_id).await;
        fail_queue_saves(channel_id, 2);

        let starts = Arc::new(AtomicUsize::new(0));
        let hook_starts = starts.clone();
        let _hook = crate::services::discord::queue_io::set_idle_queue_kick_hook_for_tests(
            Arc::new(move |shared, provider, channel, _reason| {
                let hook_starts = hook_starts.clone();
                Box::pin(async move {
                    if channel != channel_id {
                        return None;
                    }
                    let taken = crate::services::discord::idle_queue_take_next_soft_if_ready(
                        &shared, &provider, channel,
                    )
                    .await;
                    let started = taken.intervention.is_some();
                    if started {
                        hook_starts.fetch_add(1, Ordering::SeqCst);
                    }
                    Some(crate::services::discord::IdleQueueKickoffChannelOutcome { started })
                })
            }),
        );

        let report = clear(&shared, &provider, channel_id)
            .await
            .expect("a clear always commits");
        assert!(
            token.cancelled.load(Ordering::SeqCst),
            "the released turn is stopped"
        );
        assert!(report.failed.contains(&"queue_publish"), "{report:?}");
        assert_eq!(
            global_active(&shared),
            1,
            "the released turn is counted down once"
        );

        // The late finalizer finds no anchor, and a kickoff finds no backlog.
        let finish =
            crate::services::discord::mailbox_finish_turn(&shared, &provider, channel_id).await;
        assert!(
            finish.removed_token.is_none(),
            "the clear already released the anchor"
        );
        crate::services::discord::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel_id,
            "released_turn_finalizer",
        );
        tokio::time::sleep(std::time::Duration::from_secs(600)).await;

        assert_eq!(starts.load(Ordering::SeqCst), 0, "no discarded input runs");
        assert_eq!(queue_len(&shared, channel_id).await, 0);
        assert_eq!(session_state(&shared, channel_id).await, (None, true));
        assert_eq!(
            global_active(&shared),
            1,
            "the late finalizer does not count down again"
        );
    });
}

#[test]
fn persisted_clear_still_resets_the_session_and_arms_no_kick() {
    let root = tempfile::tempdir().expect("scratch runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let provider = ProviderKind::Gemini;
    let channel_id = ChannelId::new(6_233_102);
    paused_runtime().block_on(async {
        let shared = make_shared_data_for_tests();
        seed_session(&shared, channel_id).await;
        seed_backlog(&shared, &provider, channel_id).await;

        assert!(
            clear(&shared, &provider, channel_id).await.is_ok(),
            "a persisted clear still succeeds"
        );
        assert_eq!(
            queue_len(&shared, channel_id).await,
            0,
            "the backlog is discarded"
        );
        assert_eq!(
            session_state(&shared, channel_id).await,
            (None, true),
            "the provider session is reset"
        );
        assert!(
            !shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id),
            "an empty cleared queue arms no kickoff"
        );
        assert!(
            shared
                .session_transition_lock(channel_id)
                .try_lock_owned()
                .is_ok(),
            "a persisted clear releases the transition"
        );
    });
}

#[test]
fn persisted_clear_still_resets_the_managed_process() {
    let root = tempfile::tempdir().expect("scratch runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    #[cfg(unix)]
    let _tmux = crate::services::discord::relay_recovery::tests::orphan_token_finish::MissingSessionTmux::new_after_shared_test_env_lock();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(6_233_104);
    let session_name = provider.build_tmux_session_name(CHANNEL_NAME);
    let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
    crate::services::session_backend::insert_process_session(
        session_name.clone(),
        crate::services::session_backend::SessionHandle::TestProcess {
            pid: 6_233_104,
            alive: alive.clone(),
        },
    );
    paused_runtime().block_on(async {
        let shared = make_shared_data_for_tests();
        seed_session(&shared, channel_id).await;
        seed_backlog(&shared, &provider, channel_id).await;

        assert!(
            clear(&shared, &provider, channel_id).await.is_ok(),
            "a persisted clear still succeeds"
        );
        assert_eq!(
            session_state(&shared, channel_id).await,
            (None, true),
            "the provider session is reset"
        );
    });
    assert!(
        !alive.load(Ordering::SeqCst),
        "the managed process is terminated"
    );
    assert!(
        crate::services::session_backend::remove_process_session(&session_name).is_none(),
        "the managed process session is removed"
    );
}

pub(super) async fn boundary_rows(pool: &sqlx::PgPool, channel_id: ChannelId) -> Vec<i64> {
    sqlx::query_scalar(
        "SELECT clear_generation::BIGINT FROM channel_session_clear_boundaries WHERE channel_id = $1",
    )
    .bind(channel_id.get().to_string())
    .fetch_all(pool)
    .await
    .expect("read clear boundaries")
}

#[test]
fn a_clear_commits_its_transcript_boundary_whatever_its_queue_write_did_pg() {
    let root = tempfile::tempdir().expect("scratch runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let provider = ProviderKind::Gemini;
    let channel_id = ChannelId::new(6_233_105);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_clear_boundary_6233",
            "clear transcript boundary",
        )
        .await;
        let pool = db.connect_and_migrate_with_max_connections(4).await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        seed_session(&shared, channel_id).await;
        seed_backlog(&shared, &provider, channel_id).await;
        fail_queue_saves(channel_id, 2);
        let http = Arc::new(serenity::Http::new(""));

        // `/clear` and `!clear` both call this entry.
        let report = clear_channel_session_state(&http, &shared, &provider, channel_id, "/clear")
            .await
            .expect("a clear always commits");

        assert_eq!(report.failed, ["queue_publish"]);
        assert_eq!(
            boundary_rows(&pool, channel_id).await,
            [1],
            "the boundary is independent"
        );
        assert_eq!(session_state(&shared, channel_id).await, (None, true));

        pool.close().await;
        db.drop().await;
    });
}

async fn soft_clear_notifications(pool: &sqlx::PgPool, channel_id: ChannelId) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM message_outbox WHERE target = $1 AND reason_code = $2",
    )
    .bind(format!("channel:{}", channel_id.get()))
    .bind(super::SOFT_CLEAR_REASON_CODE)
    .fetch_one(pool)
    .await
    .expect("read soft-clear notifications")
}

/// Rejects the boundary writes: every one, only the first, or none.
#[derive(Clone, Copy, Debug)]
pub(super) enum Trap {
    Every,
    First,
    Off,
}

pub(super) async fn trap_boundary_writes(pool: &sqlx::PgPool, trap: Trap) {
    let reject = match trap {
        Trap::Every => "RAISE EXCEPTION 'injected clear boundary failure';",
        Trap::First => {
            "IF nextval('clear_boundary_trap') = 1 THEN
                 RAISE EXCEPTION 'injected first clear boundary failure'; END IF; RETURN NEW;"
        }
        Trap::Off => "",
    };
    let function = format!(
        "CREATE FUNCTION reject_clear_boundary() RETURNS trigger AS $$
         BEGIN {reject} END; $$ LANGUAGE plpgsql"
    );
    let statements: Vec<&str> = match trap {
        Trap::Off => vec![
            "DROP TRIGGER reject_clear_boundary_trigger ON channel_session_clear_boundaries",
            "DROP FUNCTION reject_clear_boundary()",
        ],
        _ => vec![
            "CREATE SEQUENCE IF NOT EXISTS clear_boundary_trap",
            &function,
            "CREATE TRIGGER reject_clear_boundary_trigger
             BEFORE INSERT OR UPDATE ON channel_session_clear_boundaries
             FOR EACH ROW EXECUTE FUNCTION reject_clear_boundary()",
        ],
    };
    for statement in statements {
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(pool)
            .await
            .expect("toggle the clear boundary trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    }
}

const RETRY_LANDED: &str = "⚠️ 일부 저장이 처음에 실패했지만 재시도로 완료했어요.";

/// A boundary that fails twice still leaves the session reset and the process ended; one that
/// lands on its retry says so; a pool that cannot even begin a transaction is one more failed write.
#[test]
fn a_clear_whose_boundary_write_fails_still_resets_the_session_and_process_pg() {
    let root = tempfile::tempdir().expect("scratch runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    #[cfg(unix)]
    let _tmux = crate::services::discord::relay_recovery::tests::orphan_token_finish::MissingSessionTmux::new_after_shared_test_env_lock();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(6_233_106);
    let session_name = provider.build_tmux_session_name(CHANNEL_NAME);
    let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
    crate::services::session_backend::insert_process_session(
        session_name.clone(),
        crate::services::session_backend::SessionHandle::TestProcess {
            pid: 6_233_106,
            alive: alive.clone(),
        },
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_clear_boundary_reject_6233",
            "rejected clear transcript boundary",
        )
        .await;
        let pool = db.connect_and_migrate_with_max_connections(4).await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        seed_session(&shared, channel_id).await;
        seed_backlog(&shared, &provider, channel_id).await;
        trap_boundary_writes(&pool, Trap::Every).await;
        let http = Arc::new(serenity::Http::new(""));
        let clear = |shared| {
            let (http, provider) = (http.clone(), provider.clone());
            async move {
                let clear = clear_channel_session_state;
                clear(&http, &shared, &provider, channel_id, "/clear").await
            }
        };

        let report = clear(shared.clone()).await.expect("a clear always commits");
        assert_eq!(report.failed, ["pg_boundary"]);
        assert_eq!(
            report.reply(),
            format!("세션을 초기화했어요.\n대기 메시지 1건을 비웠어요.\n{RETRY_FAILED}")
        );
        assert_eq!(queue_len(&shared, channel_id).await, 0);
        assert_eq!(session_state(&shared, channel_id).await, (None, true));
        assert!(
            !alive.load(Ordering::SeqCst),
            "the managed process is reset"
        );
        assert!(boundary_rows(&pool, channel_id).await.is_empty());
        assert_eq!(soft_clear_notifications(&pool, channel_id).await, 0);

        trap_boundary_writes(&pool, Trap::Off).await;
        trap_boundary_writes(&pool, Trap::First).await;
        let report = clear(shared.clone()).await.expect("a clear always commits");
        assert_eq!(
            report.reply(),
            format!("세션을 초기화했어요.\n대기 메시지 0건을 비웠어요.\n{RETRY_LANDED}")
        );
        assert_eq!(
            (report.retried, report.failed),
            (vec!["pg_boundary"], vec![])
        );
        assert_eq!(boundary_rows(&pool, channel_id).await, [1]);

        let closed = db.connect_and_migrate_with_max_connections(1).await;
        closed.close().await;
        let unreachable =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(closed));
        seed_session(&unreachable, channel_id).await;
        let report = clear(unreachable.clone())
            .await
            .expect("a clear always commits");
        assert!(report.failed.contains(&"pg_boundary"), "{report:?}");
        assert_eq!(session_state(&unreachable, channel_id).await, (None, true));

        pool.close().await;
        db.drop().await;
    });
    crate::services::session_backend::remove_process_session(&session_name);
}
