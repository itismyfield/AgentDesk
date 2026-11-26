//! The idle-kill policy's kill-tmux call on a Bound Herdr row: the pane closes and the row retires
//! with its provider session kept only when no dispatch, turn or inflight row holds the channel.

use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedOwner, HostedRecord,
};
use crate::services::codex::herdr_turn::{CodexHerdrPorts, CodexHerdrTurn, restart_intent};
use crate::services::discord::host_teardown_gate::test_support::shared_on;
use crate::services::session_host::HerdrTarget;
use crate::services::tui_prompt_dedupe::binding_events::SourceId;

const IDLE: &str = "idle 7시간 초과 — 자동 정리";
const IDLE_AGAIN: &str = "UPDATE sessions SET status = 'idle', active_dispatch_id = $2, last_heartbeat = NOW() - INTERVAL '7 hours' WHERE session_key = $1";

impl Node {
    /// The row as idle-kill selects it: idle for 7 hours, its provider session recorded.
    async fn idle(rig: &herdr::P11TerminationRig, dispatch: Option<&str>) -> Self {
        let node = Self::bind(rig, herdr::HostedState::Bound, dispatch).await;
        node.sql("UPDATE sessions SET claude_session_id = 'kept-session' WHERE session_key = $1 AND $2::TEXT IS NULL", None).await;
        node.sql(IDLE_AGAIN, dispatch).await;
        node
    }

    async fn idle_kill(&self) -> Value {
        self.idle_kill_on(self.registry.clone()).await
    }

    /// The policy's call through a server whose runtimes are `registry`'s.
    async fn idle_kill_on(&self, registry: Arc<HealthRegistry>) -> Value {
        self.kill_request(registry).await
    }

    /// The same call as a request that runs on its own.
    fn kill_request(
        &self,
        registry: Arc<HealthRegistry>,
    ) -> impl std::future::Future<Output = Value> + Send + 'static {
        let key = self.key.replace('/', "%2F").replace(':', "%3A");
        let pool = Some(self.pool.clone());
        async move {
            let body = json!({"reason": IDLE, "minimum_idle_minutes": 360}).to_string();
            let mut config = crate::config::Config::default();
            config.server.auth_token = Some(AUTH.into());
            let app = crate::server::routes::cancel_route_test_app(
                config,
                pool,
                Some(registry),
                herdr::NODE,
            );
            let bearer = format!("Bearer {AUTH}");
            let uri = format!("/sessions/{key}/kill-tmux");
            let post = crate::server::routes::cancel_route_test_post;
            let headers = [("authorization", bearer.as_str()), JSON];
            let (status, reply) = post(&app, &uri, &headers, &body).await;
            assert_eq!(status, StatusCode::OK, "{reply}");
            reply
        }
    }

    /// The pane closed once, the row retired and left idle, and the provider session stayed.
    async fn assert_ended(&self, rig: &herdr::P11TerminationRig, reply: &Value) {
        assert_eq!(reply["tmux_killed"], true, "{reply}");
        assert_eq!(reply["session_row_disconnected"], true, "{reply}");
        let after = self.effects(rig).await;
        assert_eq!(
            (after.closes(), after.row, after.session),
            (1, retired(), disconnected())
        );
        let kept: Option<String> =
            sqlx::query_scalar("SELECT claude_session_id FROM sessions WHERE session_key = $1")
                .bind(&self.key)
                .fetch_one(&self.pool)
                .await
                .unwrap();
        assert_eq!(kept.as_deref(), Some("kept-session"));
        let alarms = crate::services::observability::idle_cleanup_preserved_count(&self.key);
        assert_eq!(alarms, 0, "no preserve alarm for a Herdr row");
    }

    /// The policy's call was refused for `why`: nothing was written to Herdr and the row is Bound.
    async fn assert_refused(&self, rig: &herdr::P11TerminationRig, why: &str) {
        let reply = self.idle_kill().await;
        assert_eq!(
            (&reply["tmux_killed"], &reply["detail"]),
            (&json!(false), &json!(why))
        );
        let after = self.effects(rig).await;
        assert!(
            after.writes.is_empty() && after.row == bound(),
            "{why}: {after:?}"
        );
    }

    /// The dispatch PATCH a worker sends, through the sessions row writer.
    fn patch(&self) -> impl std::future::Future<Output = u64> + Send + 'static {
        let (pool, key) = (self.pool.clone(), self.key.clone());
        async move {
            let id: i64 = sqlx::query_scalar("SELECT id FROM sessions WHERE session_key = $1")
                .bind(&key)
                .fetch_one(&pool)
                .await
                .unwrap();
            let params = crate::db::dispatched_sessions::UpdateSessionParams {
                status: Some("turn_active"),
                active_dispatch_id: Some(SUCCESSOR),
                model: None,
                tokens: None,
                cwd: None,
                session_info: None,
            };
            crate::db::dispatched_sessions::update_session_pg(&pool, id, params)
                .await
                .unwrap()
        }
    }
}

// Each holder refuses for its own reason with nothing written, then the free channel ends. A turn
// that finished after the policy picked the row leaves a fresh heartbeat, which refuses too.
#[test]
fn idle_kill_ends_only_an_unheld_herdr_execution_pg() {
    let name = "idle_expire_tests::idle_kill_ends_only_an_unheld_herdr_execution_pg";
    let Some((rig, runtime)) = rig_child(name) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::idle(&rig, Some(DISPATCH)).await;
        node.assert_refused(&rig, "active dispatch").await;
        node.sql(IDLE_AGAIN, None).await;
        node.start(51, "turn-a", None).await;
        node.assert_refused(&rig, "inflight row open").await;
        let row = inflight::load_inflight_state(&ProviderKind::Claude, CHANNEL).unwrap();
        inflight::delete_inflight_state_file_checked(&ProviderKind::Claude, CHANNEL);
        let unmatched = "active turn has no matching captured inflight evidence";
        node.assert_refused(&rig, unmatched).await;
        inflight::save_inflight_state(&row).unwrap();
        let token = test_turn::active(&node.shared, channel()).await.unwrap();
        test_turn::finish_naturally(&node.shared, channel(), 51, "turn-a", &token).await;
        inflight::delete_inflight_state_file_checked(&ProviderKind::Claude, CHANNEL);
        node.sql("UPDATE sessions SET status = 'idle', last_heartbeat = NOW() WHERE session_key = $1 AND $2::TEXT IS NULL", None).await;
        node.assert_refused(&rig, "row no longer idle").await;
        node.sql(IDLE_AGAIN, None).await;
        // A restart can leave the inflight row with no turn in the mailbox.
        inflight::save_inflight_state(&row).unwrap();
        node.assert_refused(&rig, "inflight row open").await;
        inflight::delete_inflight_state_file_checked(&ProviderKind::Claude, CHANNEL);
        let reply = node.idle_kill().await;
        node.assert_ended(&rig, &reply).await;
        node.finish().await;
    });
}

// A dispatch written before the row reservation refuses the end; one written after it waits for
// the close and lands on the retired row, where the next launch serves it.
#[test]
fn idle_kill_and_a_dispatch_patch_share_one_row_reservation_pg() {
    let name = "idle_expire_tests::idle_kill_and_a_dispatch_patch_share_one_row_reservation_pg";
    let Some((rig, runtime)) = rig_child(name) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::idle(&rig, None).await;
        let (patched, earlier) = (ran(), node.patch());
        let flag = patched.clone();
        probe_after_capture(Box::new(move || {
            Box::pin(async move { flag.store(earlier.await == 1, Ordering::SeqCst) })
        }));
        node.assert_refused(&rig, "active dispatch").await;
        assert!(patched.load(Ordering::SeqCst), "the earlier PATCH ran");
        node.sql(IDLE_AGAIN, None).await;
        let later = Arc::new(Mutex::new(None));
        let (slot, patch) = (later.clone(), node.patch());
        probe_after_fence(Box::new(move || {
            Box::pin(async move {
                *slot.lock().unwrap() = Some(tokio::spawn(patch));
                tokio::time::sleep(Duration::from_millis(200)).await;
            })
        }));
        let (slot, waited) = (later.clone(), ran());
        let flag = waited.clone();
        probe_after_close(Box::new(move || {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let pending = slot
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|h| !h.is_finished());
                flag.store(pending, Ordering::SeqCst);
            })
        }));
        let reply = node.idle_kill().await;
        assert_eq!(reply["tmux_killed"], true, "{reply}");
        assert!(
            waited.load(Ordering::SeqCst),
            "the later PATCH waited for the close"
        );
        let handle = later
            .lock()
            .unwrap()
            .take()
            .expect("the later PATCH started");
        assert_eq!(handle.await.unwrap(), 1, "the later PATCH lands");
        let after = node.effects(&rig).await;
        assert_eq!((after.closes(), after.row), (1, retired()));
        assert_eq!(after.session, active(Some(SUCCESSOR)));
        node.finish().await;
    });
}

// A channel no runtime serves any more is ended by the runtime of the bot owning the row.
#[test]
fn idle_kill_ends_a_herdr_execution_no_runtime_serves_pg() {
    let name = "idle_expire_tests::idle_kill_ends_a_herdr_execution_no_runtime_serves_pg";
    let Some((rig, runtime)) = rig_child(name) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::idle(&rig, None).await;
        node.shared.settings.write().await.allowed_channel_ids = vec![OTHER];
        let served = node
            .registry
            .shared_for_provider_on_channel(&ProviderKind::Claude, channel());
        assert!(served.await.is_none(), "no runtime serves the channel");
        let reply = node.idle_kill().await;
        node.assert_ended(&rig, &reply).await;
        node.finish().await;
    });
}

/// As `idle`, for a Codex execution; answers the thread its live O cursor reads at EOF.
async fn idle_codex(rig: &herdr::P11TerminationRig) -> (Node, String) {
    use crate::services::tui_prompt_dedupe::binding_context::{
        BindingContext, PreparedIncarnation,
    };
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut shared = shared_on(&pool).await;
    let registry = Arc::new(HealthRegistry::new());
    Arc::get_mut(&mut shared).unwrap().health_registry = Arc::downgrade(&registry);
    shared.settings.write().await.provider = ProviderKind::Codex;
    registry.register("codex".into(), shared.clone()).await;
    // Codex channels run on the local TUI, whose input owner keeps the thread to resume.
    let mut config = crate::config::Config::default();
    let hosted = crate::config::ProviderConfig {
        tui_hosting: Some(true),
        ..Default::default()
    };
    config.providers.insert("codex".into(), hosted);
    crate::services::provider_hosting::install_provider_hosting_config(&config);
    let logical = "AgentDesk-codex-e07".to_string();
    let key = format!("codex/{}/{}:{logical}", shared.token_hash, herdr::NODE);
    let context: BindingContext = serde_json::from_value(json!({"schema": 1, "provider": "codex",
        "created_at": chrono::Utc::now(), "execution_nonce": herdr::EXECUTION,
        "tmux_session": logical, "channel_id": CHANNEL, "launch_mode": "fresh",
        "owner_runtime_root": crate::services::tmux_common::current_tmux_owner_marker()}))
    .unwrap();
    let prepared = PreparedIncarnation::create(context).unwrap();
    let named = (shared.token_hash.as_str(), key.as_str());
    let bound = herdr::HostedState::Bound;
    let switches = rig
        .bind_as(&pool, named, bound, "codex", &prepared.path)
        .await;
    let stamp = crate::services::tmux_common::spawn_markers::stamp_spawn_markers;
    stamp(&logical, Some(&prepared)).unwrap();
    let node = Node {
        db,
        pool,
        shared,
        registry,
        key,
        _switches: switches,
    };
    node.sql(IDLE_AGAIN, None).await;
    // The ended rollout, and the channel's one live O cursor captured through its EOF.
    use sha2::Digest;
    use std::os::unix::fs::MetadataExt;
    let runtime = crate::config::runtime_root().unwrap();
    let native = uuid::Uuid::new_v4().to_string();
    let rollout = runtime.join(format!("codex/sessions/rollout-{native}.jsonl"));
    std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    let line = json!({"type":"session_meta","payload":{"id":native,"cwd":"/tmp"}});
    let bytes = format!("{line}\n").into_bytes();
    std::fs::write(&rollout, &bytes).unwrap();
    let path = rollout.canonicalize().unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let (dev, ino, session_id) = (meta.dev(), meta.ino(), native.clone());
    let cursor = crate::services::tui_o::store::spool::Cursor {
        source: SourceId {
            session_id,
            path,
            dev,
            ino,
        },
        captured_through: meta.len(),
        prefix_hash: hex::encode(sha2::Sha256::digest(&bytes)),
        retired: false,
        transition_cleanup: None,
    };
    let dir = runtime.join(format!("o_store/{CHANNEL}/cursor"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("source.json"),
        serde_json::to_vec(&cursor).unwrap(),
    )
    .unwrap();
    (node, native)
}

/// Ports that launch nothing; the effort resume is the one the channel's owner holds.
struct Owed(Option<restart_intent::EffortResume>);

impl CodexHerdrPorts for Owed {
    fn launch_host(&self) -> Option<Arc<dyn crate::services::herdr_launch::HerdrLaunchHost>> {
        None
    }
    fn attach(
        &self,
        _: &HostedOwner,
        _: &HostedExecution,
        _: &SourceId,
        _: &HerdrTarget,
    ) -> Result<bool, String> {
        Err("no attach".into())
    }
    fn confirm_bound(
        &self,
        _: &HostedOwner,
        _: &HostedExecution,
        _: &HerdrTarget,
    ) -> Result<(), String> {
        Err("no bound".into())
    }
    fn owner_port(&self) -> crate::services::herdr_launch::OwnerPort {
        crate::services::discord::HERDR_OWNER_PORT
    }
    fn effort_resume(&self) -> Option<restart_intent::EffortResume> {
        self.0.clone()
    }
}

/// The owner's next ordinary launch from the retired row, with `effort` as its owner holds it.
fn next_launch(
    node: &Node,
    rig: &herdr::P11TerminationRig,
    effort: Owed,
) -> crate::services::herdr_launch::HerdrLaunch {
    let ended = (node.shared.token_hash.as_str(), node.key.as_str());
    let record = rig.record_as(ended, herdr::HostedState::Retired, "codex");
    let row = HostedRecord::Known(record.clone());
    let endpoint = json!({"execution_node": "test", "config_key": "test",
        "socket_addr": "/test/herdr.sock", "herdr_session": "test"});
    let turn = CodexHerdrTurn {
        pool: &node.pool,
        owner: record.owner,
        channel_id: CHANNEL,
        endpoint: serde_json::from_value(endpoint).unwrap(),
        row: Some(&row),
        prompt: "다음 메시지",
        working_dir: "/tmp",
        system_prompt: None,
        allowed_tools: &[],
        model: None,
        effort: None,
        fast_mode: None,
        goals: None,
        compact_token_limit: None,
        cancel: None,
        bootstrap: false,
    };
    let (root, port) = (
        Some(std::path::Path::new("/test")),
        crate::services::discord::HERDR_OWNER_PORT,
    );
    let launch = crate::services::codex::herdr_turn::launch_of_for_test(&turn, root, &effort, port);
    launch.expect("the next launch starts")
}

/// That launch resumes `native` at the boundary the O cursor captured.
fn assert_resumes(node: &Node, rig: &herdr::P11TerminationRig, native: &str, effort: Owed) {
    let launch = next_launch(node, rig, effort);
    assert!(
        launch.resume,
        "an ordinary message after the idle end resumes"
    );
    assert_eq!(launch.expected_native_session_id.as_deref(), Some(native));
    assert!(
        launch.resume_start.is_some(),
        "resumed at the captured boundary"
    );
}

/// The owner's `/clear`, then its next launch: a fresh session that never names `native`.
async fn assert_clear_starts_fresh(node: &Node, rig: &herdr::P11TerminationRig, native: &str) {
    let http = Arc::new(poise::serenity_prelude::Http::new(""));
    let clear = crate::services::discord::commands::control::clear_channel_session_state;
    let report = clear(
        &http,
        &node.shared,
        &ProviderKind::Codex,
        channel(),
        "!clear",
    );
    report.await.unwrap();
    let launch = next_launch(node, rig, owner_effort(node).await);
    assert!(
        !launch.resume,
        "the next message after a clear starts fresh"
    );
    assert_ne!(launch.expected_native_session_id.as_deref(), Some(native));
}

/// Production adopts a local TUI channel's queue owner on its first input.
async fn adopt(node: &Node) {
    use crate::services::discord::queue_io::input_owner::ingress;
    let adopt = ingress::adopt(&node.shared, &ProviderKind::Codex, channel());
    assert_eq!(ingress::ADOPTS.scope(true, adopt).await, Ok(true));
}

/// The resume the channel's owner holds, as the production port reads it.
async fn owner_effort(node: &Node) -> Owed {
    let owed = crate::services::discord::commands::effort_restart::herdr_resume;
    Owed(owed(&node.shared, channel()).await)
}

// The thread an idle end closes is the one the next ordinary launch from the retired row resumes,
// at the boundary its O cursor captured.
#[test]
fn idle_kill_keeps_the_codex_thread_for_the_next_launch_pg() {
    let name = "idle_expire_tests::idle_kill_keeps_the_codex_thread_for_the_next_launch_pg";
    let Some((rig, runtime)) = rig_child(name) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let (node, native) = idle_codex(&rig).await;
        adopt(&node).await;
        let reply = node.idle_kill().await;
        assert_eq!(reply["tmux_killed"], true, "{reply}");
        assert_eq!(node.effects(&rig).await.row, retired());
        assert_resumes(&node, &rig, &native, owner_effort(&node).await);
        assert_clear_starts_fresh(&node, &rig, &native).await;
        node.finish().await;
    });
}

/// With no owner runtime on a booted server the execution still ends; a returning Codex owner resumes
/// the thread until it clears. `moved`: the new bot starts its own session (no handover).
fn orphaned(name: &str, moved: bool, codex: bool) {
    let Some((rig, runtime)) = rig_child(&format!("idle_expire_tests::{name}")) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let (node, native) = match codex {
            true => idle_codex(&rig).await,
            false => (Node::idle(&rig, None).await, String::new()),
        };
        let mut registry = HealthRegistry::new();
        registry.age_for_tests(3_600);
        let registry = Arc::new(registry);
        let provider = if codex {
            ProviderKind::Codex
        } else {
            ProviderKind::Claude
        };
        if moved {
            let mut other = shared_on(&node.pool).await;
            Arc::get_mut(&mut other).unwrap().token_hash = "another-bot".into();
            let mut settings = other.settings.write().await;
            (settings.provider, settings.allowed_channel_ids) = (provider.clone(), vec![CHANNEL]);
            drop(settings);
            registry.register(provider.as_str().into(), other).await;
        }
        let reply = node.idle_kill_on(registry).await;
        if codex {
            assert_eq!(reply["tmux_killed"], true, "{reply}");
            let after = node.effects(&rig).await;
            assert_eq!((after.closes(), after.row), (1, retired()));
            if !moved {
                assert_resumes(&node, &rig, &native, Owed(None));
                assert_clear_starts_fresh(&node, &rig, &native).await;
            }
        } else {
            node.assert_ended(&rig, &reply).await;
        }
        node.finish().await;
    });
}

#[test]
fn idle_kill_ends_a_herdr_execution_whose_owner_bot_is_gone_pg() {
    orphaned(
        "idle_kill_ends_a_herdr_execution_whose_owner_bot_is_gone_pg",
        false,
        false,
    );
}

#[test]
fn idle_kill_ends_a_herdr_execution_of_a_channel_moved_to_another_bot_pg() {
    let name = "idle_kill_ends_a_herdr_execution_of_a_channel_moved_to_another_bot_pg";
    orphaned(name, true, false);
}

#[test]
fn idle_kill_ends_a_codex_execution_whose_owner_bot_is_gone_pg() {
    orphaned(
        "idle_kill_ends_a_codex_execution_whose_owner_bot_is_gone_pg",
        false,
        true,
    );
}

#[test]
fn idle_kill_ends_a_codex_execution_of_a_channel_moved_to_another_bot_pg() {
    orphaned(
        "idle_kill_ends_a_codex_execution_of_a_channel_moved_to_another_bot_pg",
        true,
        true,
    );
}

// A second idle end of the same execution that arrives while the first holds its fence is
// refused, and leaves the thread the first end kept for the next launch.
#[test]
fn a_losing_idle_kill_leaves_the_kept_codex_thread_pg() {
    let name = "idle_expire_tests::a_losing_idle_kill_leaves_the_kept_codex_thread_pg";
    let Some((rig, runtime)) = rig_child(name) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let (node, native) = idle_codex(&rig).await;
        adopt(&node).await;
        let second = Arc::new(Mutex::new(None));
        let (slot, request) = (second.clone(), node.kill_request(node.registry.clone()));
        probe_after_fence(Box::new(move || {
            Box::pin(async move {
                *slot.lock().unwrap() = Some(tokio::spawn(request));
                tokio::time::sleep(Duration::from_millis(200)).await;
            })
        }));
        let reply = node.idle_kill().await;
        assert_eq!(reply["tmux_killed"], true, "{reply}");
        let handle = second
            .lock()
            .unwrap()
            .take()
            .expect("the second end started");
        let lost = handle.await.unwrap();
        assert_eq!(lost["tmux_killed"], false, "{lost}");
        assert_eq!(node.effects(&rig).await.closes(), 1);
        assert_resumes(&node, &rig, &native, owner_effort(&node).await);
        node.finish().await;
    });
}
