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
const END_UNCONFIRMED: &str = "이전 세션 프로세스의 종료를 확인하지 못했어요.";

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

/// A channel's managed session: its fake tmux, a live process and a stored provider session.
struct Managed {
    tmux: String,
    fake: FakeTmux,
    alive: Arc<AtomicBool>,
    key: String,
}

impl Managed {
    async fn start(
        shared: &Arc<SharedData>,
        pool: &sqlx::PgPool,
        provider: &ProviderKind,
        (channel, name): (ChannelId, &str),
    ) -> Self {
        let managed = Self::launch(shared, provider, (channel, name)).await;
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, claude_session_id)
             VALUES ($1, $2, 'idle', $3)",
        )
        .bind(&managed.key)
        .bind(provider.as_str())
        .bind(OLD_SESSION)
        .execute(pool)
        .await
        .expect("stored provider session");
        managed
    }

    /// As `start`, leaving the channel's stored session row to the caller.
    async fn launch(
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        (channel, name): (ChannelId, &str),
    ) -> Self {
        let tmux = provider.build_tmux_session_name(name);
        let fake = FakeTmux::install(&tmux);
        let alive = Arc::new(AtomicBool::new(true));
        let handle = crate::services::session_backend::SessionHandle::TestProcess {
            pid: 6_995_200 + (channel.get() % 100) as u32,
            alive: alive.clone(),
        };
        crate::services::session_backend::insert_process_session(tmux.clone(), handle);
        map_channel(shared, channel, name).await;
        let mut data = shared.core.lock().await;
        data.sessions.get_mut(&channel).unwrap().session_id = Some(OLD_SESSION.into());
        drop(data);
        let key = super::super::super::adk_session::build_adk_session_key;
        let key = key(shared, channel, provider, None)
            .await
            .expect("session key");
        Self {
            tmux,
            fake,
            alive,
            key,
        }
    }

    /// Whether the clear ended the process and asked tmux to kill the session.
    fn ended(&self) -> bool {
        let killed = format!("kill-session -t ={}:", self.tmux);
        !self.alive.load(Ordering::SeqCst) && self.fake.take_calls().contains(&killed)
    }
}

impl Drop for Managed {
    fn drop(&mut self) {
        crate::services::session_backend::remove_process_session(&self.tmux);
    }
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
        let managed = Managed::start(&shared, &pool, &provider, (channel, &name)).await;
        let token = Arc::new(CancelToken::new());
        let start = crate::services::discord::mailbox_try_start_turn;
        let turn = MessageId::new(channel.get() + 9);
        assert!(start(&shared, channel, token.clone(), UserId::new(1), turn).await);
        crate::services::discord::increment_global_active(&shared, "cleared turn");
        seed_backlog(&shared, &provider, channel).await;
        managed.fake.take_calls();

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
            stored_selector(&pool, &managed.key).await,
            None,
            "{provider:?}: nothing to resume"
        );
        assert!(managed.ended(), "{provider:?}: the process and tmux end");
    }
    pool.close().await;
    db.drop().await;
}

// A tmux session whose kill fails outlives the clear: the clear still commits, and its reply
// warns that the old session's end was not confirmed.
#[tokio::test]
async fn a_clear_whose_tmux_kill_fails_commits_and_warns_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = postgres().await;
    let shared = make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let (provider, channel) = (
        ProviderKind::Claude,
        ChannelId::new(1_479_671_305_341_100_010),
    );
    let name = format!("adk-clear-kill-fails-{}", uuid::Uuid::new_v4().simple());
    let managed = Managed::start(&shared, &pool, &provider, (channel, &name)).await;
    managed.fake.fail_kills();

    let reply = clear(&shared, &provider, channel).await;

    let warned = format!("{SESSION_CLEARED_RESPONSE}\n⚠️ {END_UNCONFIRMED}");
    assert_eq!(reply, warned);
    assert!(managed.ended(), "the process ends and the kill is tried");
    assert!(crate::services::platform::tmux::has_session(&managed.tmux));
    assert_eq!(session_state(&shared, channel).await, (None, true));
    pool.close().await;
    db.drop().await;
}

// A tmux probe that fails with a socket error cannot rule the session out: the clear still kills
// it, and warns exactly once only when that kill also fails.
#[tokio::test]
async fn a_clear_whose_tmux_probe_fails_kills_and_warns_only_if_the_kill_fails_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = postgres().await;
    let shared = make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let provider = ProviderKind::Claude;
    let warned = format!("{SESSION_CLEARED_RESPONSE}\n⚠️ {END_UNCONFIRMED}");
    for (n, (kill_fails, reply)) in [(true, warned.as_str()), (false, SESSION_CLEARED_RESPONSE)]
        .into_iter()
        .enumerate()
    {
        let channel = ChannelId::new(1_479_671_305_341_100_020 + n as u64);
        let name = format!(
            "adk-clear-probe-fails-{n}-{}",
            uuid::Uuid::new_v4().simple()
        );
        let managed = Managed::start(&shared, &pool, &provider, (channel, &name)).await;
        managed.fake.fail_probes();
        if kill_fails {
            managed.fake.fail_kills();
        }

        assert_eq!(
            clear(&shared, &provider, channel).await,
            reply,
            "kill fails: {kill_fails}"
        );
        assert!(
            managed.ended(),
            "kill fails: {kill_fails}: the kill is tried"
        );
        assert_eq!(session_state(&shared, channel).await, (None, true));
    }
    pool.close().await;
    db.drop().await;
}

// A Herdr endpoint with no live execution behind it is no Herdr session: the channel's managed
// tmux and process still end, without a warning.
#[tokio::test]
async fn a_herdr_configured_channel_without_a_live_execution_ends_its_tmux_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = postgres().await;
    let shared = make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let (provider, channel) = (
        ProviderKind::Claude,
        ChannelId::new(1_479_671_305_341_100_011),
    );
    let hosts = [(channel.get(), "mac-mini")];
    let _hosts = crate::config::session_hosts::force_for_test(Some("mac-mini"), &hosts);
    assert!(crate::config::session_hosts::herdr_endpoint(channel.get()).is_some());
    let name = format!("adk-clear-endpoint-only-{}", uuid::Uuid::new_v4().simple());
    let managed = Managed::start(&shared, &pool, &provider, (channel, &name)).await;

    let reply = clear(&shared, &provider, channel).await;

    assert_eq!(reply, SESSION_CLEARED_RESPONSE);
    assert!(managed.ended(), "the managed tmux and process end");
    pool.close().await;
    db.drop().await;
}

// A clear that took the channel transition keeps it until the session has ended, so an input
// arriving mid-teardown can neither read the old session nor launch one the teardown kills.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clear_keeps_its_transition_until_the_session_has_ended_pg() {
    use crate::services::discord::try_intake_runtime_transition_after_redirect as intake;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool) = postgres().await;
    let shared = make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let (provider, channel) = (
        ProviderKind::Claude,
        ChannelId::new(1_479_671_305_341_100_012),
    );
    let name = format!("adk-clear-transition-{}", uuid::Uuid::new_v4().simple());
    let managed = Managed::start(&shared, &pool, &provider, (channel, &name)).await;
    managed.fake.hold_kills();
    let task = tokio::spawn({
        let (shared, provider) = (shared.clone(), provider.clone());
        async move { clear(&shared, &provider, channel).await }
    });
    // The kill blocks a runtime worker, so the wait polls on this thread rather than a timer.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !managed.fake.killing() {
        assert!(
            std::time::Instant::now() < deadline,
            "the clear reaches its kill"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let fallback = (None, false, String::new());
    let during = intake(&shared, channel, fallback.clone()).await;
    assert!(during.is_err(), "no input takes the channel mid-teardown");
    managed.fake.release_kills();
    assert_eq!(task.await.unwrap(), SESSION_CLEARED_RESPONSE);
    let after = intake(&shared, channel, fallback).await;
    let after = after.unwrap_or_else(|_| panic!("the transition is free after the clear"));
    assert_eq!(after.state.0, None, "the next input reads a fresh session");
    drop(after);
    assert!(managed.ended());
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

const HERDR_RETIRED_CHANNEL: u64 = 1_479_671_305_341_200_002;

// A Herdr channel whose execution already retired still ends the managed tmux and process it
// left behind, and closes no pane.
#[test]
fn a_herdr_clear_with_a_retired_execution_ends_the_managed_tmux_pg() {
    use crate::services::session_host::herdr_termination_rig as herdr;
    let path = concat!(
        module_path!(),
        "::a_herdr_clear_with_a_retired_execution_ends_the_managed_tmux_pg"
    );
    if !herdr::in_child(path, HERDR_RETIRED_CHANNEL) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(HERDR_RETIRED_CHANNEL);
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
        let (provider, channel) = (ProviderKind::Claude, ChannelId::new(HERDR_RETIRED_CHANNEL));
        map_channel(&shared, channel, "p11-clear").await;
        let key = super::super::super::adk_session::build_adk_session_key;
        let key = key(&shared, channel, &provider, None)
            .await
            .expect("session key");
        let _switches = rig
            .bind(&pool, &shared.token_hash, &key, herdr::HostedState::Retired)
            .await;
        let managed = Managed::launch(&shared, &provider, (channel, "p11-clear")).await;

        let reply = clear(&shared, &provider, channel).await;

        assert_eq!(reply, SESSION_CLEARED_RESPONSE);
        assert!(rig.writes().is_empty(), "no pane is closed");
        assert!(managed.ended(), "the managed tmux and process end");
        pool.close().await;
        db.drop().await;
    });
}

const HERDR_NEVER_ATTACHED_CHANNEL: u64 = 1_479_671_305_341_200_003;

// A clear ends a never-attached Pending with recorded evidence when the switch is on; with it off
// nothing is closed or retired and the end is unconfirmed, yet the queue is still dropped.
#[test]
fn a_herdr_clear_ends_a_never_attached_pending_with_recorded_evidence_pg() {
    use crate::services::session_host::herdr_termination_rig as herdr;
    let path = concat!(
        module_path!(),
        "::a_herdr_clear_ends_a_never_attached_pending_with_recorded_evidence_pg"
    );
    if !herdr::in_child(path, HERDR_NEVER_ATTACHED_CHANNEL) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(HERDR_NEVER_ATTACHED_CHANNEL);
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
        let channel = ChannelId::new(HERDR_NEVER_ATTACHED_CHANNEL);
        let provider = ProviderKind::Claude;
        map_channel(&shared, channel, "p11-clear").await;
        let key = super::super::super::adk_session::build_adk_session_key;
        let key = key(&shared, channel, &provider, None)
            .await
            .expect("session key");
        let switches = rig
            .bind_never_attached(&pool, &shared.token_hash, &key)
            .await;
        let pending = (herdr::HostedState::Pending, herdr::EXECUTION.to_string());

        switches.set_terminate(None);
        seed_backlog(&shared, &provider, channel).await;
        let reply = clear(&shared, &provider, channel).await;
        assert!(reply.contains(END_UNCONFIRMED), "{reply}");
        assert!(rig.writes().is_empty(), "no pane is closed");
        assert_eq!(herdr::row(&pool, &key).await, Some(pending));
        assert_eq!(
            queue_len(&shared, channel).await,
            0,
            "the clear drops the queue"
        );

        switches.set_terminate(Some(true));
        let reply = clear(&shared, &provider, channel).await;
        assert_eq!(reply, SESSION_CLEARED_RESPONSE);
        assert_eq!(rig.writes().len(), 1, "one close reaches the pane");
        let retired = (herdr::HostedState::Retired, herdr::EXECUTION.to_string());
        assert_eq!(herdr::row(&pool, &key).await, Some(retired));
        pool.close().await;
        db.drop().await;
    });
}
