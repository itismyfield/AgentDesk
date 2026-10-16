//! A user clear ends the session it resets (tmux and its process, or a Herdr provider CLI) and
//! forgets the stored provider session, so the next input launches fresh.
#![cfg(unix)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use super::super::SESSION_CLEARED_RESPONSE;
use super::clear_persist_failure_tests::{queue_len, seed_backlog, session_state};
use super::{SoftClearNotifyMode, clear_channel_session_state};
use crate::services::discord::host_defer_gate::tests::{map_channel, postgres};
use crate::services::discord::{SharedData, make_shared_data_for_tests_with_storage};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::provider_teardown::tests::test_support::FakeTmux;

const OLD_SESSION: &str = "provider-session-before-clear";

async fn clear(shared: &Arc<SharedData>, provider: &ProviderKind, channel: ChannelId) -> String {
    let http = Arc::new(serenity::Http::new(""));
    let notify = SoftClearNotifyMode::Suppress;
    let clear = clear_channel_session_state(&http, shared, provider, channel, "/clear", notify);
    clear.await.expect("a clear always commits").reply()
}

/// The provider session the next launch would resume from PostgreSQL.
async fn stored_selector(pool: &sqlx::PgPool, key: &str) -> Option<String> {
    let query = "SELECT claude_session_id FROM sessions WHERE session_key = $1";
    let row: Option<Option<String>> = sqlx::query_scalar(query)
        .bind(key)
        .fetch_optional(pool)
        .await
        .expect("read the stored selector");
    row.flatten()
}

// Claude and Codex alike: the running turn stops, the queue goes, the tmux session and its
// process end, and neither memory nor PostgreSQL keeps the session a next launch would resume.
#[tokio::test]
async fn a_clear_ends_a_running_tmux_session_and_forgets_its_provider_session_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = postgres().await;
    let shared = make_shared_data_for_tests_with_storage(Some(pool.clone()));
    for (n, provider) in [ProviderKind::Claude, ProviderKind::Codex]
        .into_iter()
        .enumerate()
    {
        let channel = ChannelId::new(1_479_671_305_341_100_000 + n as u64);
        let name = format!("adk-clear-end-{n}-{}", uuid::Uuid::new_v4().simple());
        let tmux = provider.build_tmux_session_name(&name);
        let fake = FakeTmux::install(&tmux);
        let alive = Arc::new(AtomicBool::new(true));
        let handle = crate::services::session_backend::SessionHandle::TestProcess {
            pid: 6_995_200 + n as u32,
            alive: alive.clone(),
        };
        crate::services::session_backend::insert_process_session(tmux.clone(), handle);
        map_channel(&shared, channel, &name).await;
        shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&channel)
            .unwrap()
            .session_id = Some(OLD_SESSION.into());
        let key = super::super::super::adk_session::build_adk_session_key;
        let key = key(&shared, channel, &provider, None)
            .await
            .expect("session key");
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, claude_session_id)
             VALUES ($1, $2, 'idle', $3)",
        )
        .bind(&key)
        .bind(provider.as_str())
        .bind(OLD_SESSION)
        .execute(&pool)
        .await
        .expect("stored provider session");
        let token = Arc::new(CancelToken::new());
        let start = crate::services::discord::mailbox_try_start_turn;
        let turn = MessageId::new(channel.get() + 9);
        assert!(start(&shared, channel, token.clone(), UserId::new(1), turn).await);
        crate::services::discord::increment_global_active(&shared, "cleared turn");
        seed_backlog(&shared, &provider, channel).await;
        fake.take_calls();

        let reply = clear(&shared, &provider, channel).await;

        assert_eq!(reply, SESSION_CLEARED_RESPONSE, "{provider:?}");
        assert!(
            token.cancelled.load(Ordering::SeqCst),
            "{provider:?}: the turn stops"
        );
        assert_eq!(queue_len(&shared, channel).await, 0, "{provider:?}");
        assert_eq!(
            session_state(&shared, channel).await,
            (None, true),
            "{provider:?}"
        );
        assert_eq!(
            stored_selector(&pool, &key).await,
            None,
            "{provider:?}: nothing to resume"
        );
        assert!(
            !alive.load(Ordering::SeqCst),
            "{provider:?}: the process ends"
        );
        let killed = format!("kill-session -t ={tmux}:");
        let calls = fake.take_calls();
        assert!(calls.contains(&killed), "{provider:?}: {calls:?}");
        crate::services::session_backend::remove_process_session(&tmux);
    }
    pool.close().await;
    db.drop().await;
}

const HERDR_CHANNEL: u64 = 1_479_671_305_341_200_001;

// A Herdr channel's clear closes its provider CLI's pane through the terminate service and
// retires the execution, so the next input launches a fresh one.
#[test]
fn a_herdr_clear_closes_the_provider_cli_through_the_terminate_service_pg() {
    use crate::services::session_host::herdr_termination_rig as herdr;
    let path = concat!(
        module_path!(),
        "::a_herdr_clear_closes_the_provider_cli_through_the_terminate_service_pg"
    );
    if !herdr::in_child(path, HERDR_CHANNEL) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(HERDR_CHANNEL);
    let install = rig.thread_installer();
    install();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap();
    runtime.block_on(async {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (db, pool) = postgres().await;
        let shared = make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let (provider, channel) = (ProviderKind::Claude, ChannelId::new(HERDR_CHANNEL));
        map_channel(&shared, channel, "p11-clear").await;
        let key = super::super::super::adk_session::build_adk_session_key;
        let key = key(&shared, channel, &provider, None)
            .await
            .expect("session key");
        let state = herdr::HostedState::Bound;
        let _switches = rig.bind(&pool, &shared.token_hash, &key, state).await;

        let reply = clear(&shared, &provider, channel).await;

        assert_eq!(reply, SESSION_CLEARED_RESPONSE);
        assert_eq!(rig.writes().len(), 1, "one close reaches the pane");
        let retired = (herdr::HostedState::Retired, herdr::EXECUTION.to_string());
        assert_eq!(herdr::row(&pool, &key).await, Some(retired));
        assert_eq!(session_state(&shared, channel).await, (None, true));
        pool.close().await;
        db.drop().await;
    });
}
