//! A user clear ends the session it resets (tmux and its process, or a Herdr provider CLI) and
//! forgets the stored provider session, so the next input launches fresh.
#![cfg(unix)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use super::super::SESSION_CLEARED_RESPONSE;
use super::clear_channel_session_state;
use super::clear_persist_failure_tests::{
    queue_len, seed_backlog, seed_backlog_message, session_state,
};
use crate::services::discord::host_defer_gate::tests::{map_channel, postgres};
use crate::services::discord::{SharedData, make_shared_data_for_tests_with_storage};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::provider_teardown::tests::test_support::FakeTmux;

const OLD_SESSION: &str = "provider-session-before-clear";
const END_UNCONFIRMED: &str = "이전 세션 프로세스의 종료를 확인하지 못했어요.";

/// The cleared notice with its count of discarded waiting messages.
fn cleared(discarded: usize) -> String {
    format!("{SESSION_CLEARED_RESPONSE}\n대기 메시지 {discarded}건을 비웠어요.")
}

async fn clear(shared: &Arc<SharedData>, provider: &ProviderKind, channel: ChannelId) -> String {
    let http = Arc::new(serenity::Http::new(""));

    let clear = clear_channel_session_state(&http, shared, provider, channel, "/clear");
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

        assert_eq!(reply, cleared(1), "{provider:?}");
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

    let warned = format!("{}\n⚠️ {END_UNCONFIRMED}", cleared(0));
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
    let warned = format!("{}\n⚠️ {END_UNCONFIRMED}", cleared(0));
    for (n, (kill_fails, reply)) in [(true, warned), (false, cleared(0))]
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

    assert_eq!(reply, cleared(0));
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
    assert_eq!(task.await.unwrap(), cleared(0));
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

        assert_eq!(reply, cleared(0));
        assert_eq!(rig.writes().len(), 1, "one close reaches the pane");
        let retired = (herdr::HostedState::Retired, herdr::EXECUTION.to_string());
        assert_eq!(herdr::row(&pool, &key).await, Some(retired));
        assert_eq!(session_state(&shared, channel).await, (None, true));
        pool.close().await;
        db.drop().await;
    });
}

const HERDR_RETIRED_CHANNEL: u64 = 1_479_671_305_341_200_002;

// A Retired execution cannot authorize managed fallback; an unmatched owner stays Held.
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
        let mut foreign = rig.record_as(
            (&shared.token_hash, &key),
            herdr::HostedState::Retired,
            provider.as_str(),
        );
        foreign.owner.runtime_root = "another-runtime".into();
        foreign
            .source_ref
            .runtime_root
            .clone_from(&foreign.owner.runtime_root);
        sqlx::query("UPDATE sessions SET hosted_execution = $1 WHERE session_key = $2")
            .bind(serde_json::to_value(foreign).unwrap())
            .bind(&key)
            .execute(&pool)
            .await
            .unwrap();
        let managed = Managed::launch(&shared, &provider, (channel, "p11-clear")).await;

        let reply = clear(&shared, &provider, channel).await;

        assert!(reply.contains("대기 메시지 0건을 비웠어요."), "{reply}");
        assert!(
            reply.contains("이전 Herdr 실행은 정리하지 못했어요."),
            "{reply}"
        );
        assert!(reply.contains("local owner or endpoint differs"), "{reply}");
        assert!(rig.writes().is_empty(), "no pane is closed");
        assert!(!managed.ended(), "a Retired row grants no managed fallback");
        assert_eq!(
            herdr::row(&pool, &key).await,
            Some((herdr::HostedState::Retired, herdr::EXECUTION.to_string()))
        );
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
        assert_eq!(reply, cleared(0));
        assert_eq!(rig.writes().len(), 1, "one close reaches the pane");
        let retired = (herdr::HostedState::Retired, herdr::EXECUTION.to_string());
        assert_eq!(herdr::row(&pool, &key).await, Some(retired));
        pool.close().await;
        db.drop().await;
    });
}

#[tokio::test(flavor = "current_thread")]
async fn a2_t11_clear_absent_accepted_and_bound_preserves_body_and_never_reposts_pg() {
    use crate::services::discord::gateway::{DiscordGateway, TurnGateway};
    use crate::services::discord::{
        inflight as disk,
        recovery_engine::{
            BodyCreateTestRemote, a2_admitted_for_tmux, a2_post_captured_without_bind,
            a2_stale_captured_recovery_is_held,
        },
    };
    // The pinned recovery publisher reads the O writer list from runtime worker threads.
    let name = concat!(
        module_path!(),
        "::a2_t11_clear_absent_accepted_and_bound_preserves_body_and_never_reposts_pg"
    );
    if !crate::services::tui_o::cutover::test_override::in_empty_list_process(name) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let source_root = tempfile::tempdir().unwrap();
    let (db, pool) = postgres().await;
    let mut shared = make_shared_data_for_tests_with_storage(Some(pool.clone()));
    Arc::get_mut(&mut shared).unwrap().api_port = 0;
    let provider = ProviderKind::Claude;
    for stage in 0..3 {
        let channel = ChannelId::new(75_072_110 + stage);
        let name = format!("a2-clear-{stage}-{}", uuid::Uuid::new_v4().simple());
        let managed = Managed::start(&shared, &pool, &provider, (channel, &name)).await;
        let remote = BodyCreateTestRemote::new(channel.get()).await;
        let _http = crate::services::discord::shared_state::test_rest::install(remote.http.clone());
        let gateway =
            DiscordGateway::new(remote.http.clone(), shared.clone(), provider.clone(), None);
        let current = if stage == 2 {
            gateway
                .send_message(channel, "owned partial body")
                .await
                .unwrap()
                .get()
        } else {
            0
        };
        if stage == 1 {
            a2_admitted_for_tmux(
                source_root.path(),
                channel.get(),
                "accepted body before bind",
                0,
                &managed.tmux,
            )
            .await;
        } else {
            let mut row = disk::InflightTurnState::new(
                provider.clone(),
                channel.get(),
                None,
                7,
                channel.get() + 1,
                current,
                "original prompt".into(),
                None,
                Some(managed.tmux.clone()),
                None,
                None,
                0,
            );
            row.turn_nonce = Some(format!("a2-clear-{stage}"));
            row.full_response = if stage == 2 {
                "owned partial body".into()
            } else {
                String::new()
            };
            disk::save_inflight_state(&row).unwrap();
        };
        let row = disk::load_inflight_state_read_only(&provider, channel.get()).unwrap();
        let token = Arc::new(CancelToken::from_persisted_turn_nonce(
            row.turn_nonce.clone(),
        ));
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                channel,
                token.clone(),
                UserId::new(7),
                MessageId::new(row.user_msg_id)
            )
            .await
        );
        crate::services::discord::increment_global_active(&shared, "a2 clear captured turn");
        // A queued input distinct from the active one, which the mailbox already holds.
        seed_backlog_message(&shared, &provider, channel, row.user_msg_id + 1_000).await;
        let actual = if stage == 1 {
            Some(a2_post_captured_without_bind(&remote, &shared, &row).await)
        } else {
            disk::opt_message_id(current)
        };
        let visible = remote.messages.lock().unwrap().clone();
        let calls = remote.calls.lock().unwrap().len();
        managed.fake.take_calls();
        let report =
            clear_channel_session_state(&remote.http, &shared, &provider, channel, "/clear")
                .await
                .unwrap();
        assert_eq!(report.reply(), cleared(1));
        assert!(token.cancelled.load(Ordering::SeqCst));
        assert_eq!(queue_len(&shared, channel).await, 0);
        assert!(managed.ended());
        let retained = disk::load_inflight_state_read_only(&provider, channel.get())
            .expect("clear leaves durable retirement with the captured producer");
        assert_eq!(retained.current_msg_id, row.current_msg_id);
        assert!(!retained.terminal_delivery_committed);
        let cleanup = disk::clear_inflight_state_for_captured_episode(
            &provider,
            channel.get(),
            &disk::InflightTurnIdentity::from_state(&row),
            row.turn_nonce.as_deref(),
        );
        assert_eq!(
            cleanup,
            disk::GuardedClearOutcome::Cleared,
            "existing exact-episode producer retirement accepts response absence"
        );
        assert_eq!(
            *remote.messages.lock().unwrap(),
            visible,
            "clear preserves any already accepted Discord body"
        );
        assert!(
            !remote.calls.lock().unwrap()[calls..]
                .iter()
                .any(|(route, _)| ["POST", "PATCH", "DELETE"]
                    .iter()
                    .any(|method| route.starts_with(method))
                    && actual.is_some_and(|id| route.ends_with(&format!("/{}", id.get()))))
        );
        if stage == 1 {
            let before = remote.calls.lock().unwrap().len();
            assert!(a2_stale_captured_recovery_is_held(&remote, &shared, &row).await);
            assert_eq!(
                remote.calls.lock().unwrap().len(),
                before,
                "cleared captured work cannot POST or rebind"
            );
            assert!(disk::load_inflight_state_read_only(&provider, channel.get()).is_none());
            let mut successor = disk::InflightTurnState::new(
                provider.clone(),
                channel.get(),
                None,
                7,
                row.user_msg_id + 50,
                0,
                "successor prompt".into(),
                None,
                None,
                None,
                None,
                0,
            );
            successor.turn_nonce = Some("a2-clear-successor".into());
            disk::save_inflight_state(&successor).unwrap();
            let successor = disk::load_inflight_state_read_only(&provider, channel.get()).unwrap();
            assert_eq!(
                disk::clear_inflight_state_for_captured_episode(
                    &provider,
                    channel.get(),
                    &disk::InflightTurnIdentity::from_state(&row),
                    row.turn_nonce.as_deref()
                ),
                disk::GuardedClearOutcome::UserMsgMismatch
            );
            assert!(a2_stale_captured_recovery_is_held(&remote, &shared, &row).await);
            assert_eq!(
                serde_json::to_value(
                    disk::load_inflight_state_read_only(&provider, channel.get()).unwrap()
                )
                .unwrap(),
                serde_json::to_value(&successor).unwrap()
            );
            assert_eq!(remote.calls.lock().unwrap().len(), before);
        }
    }
    pool.close().await;
    db.drop().await;
}
