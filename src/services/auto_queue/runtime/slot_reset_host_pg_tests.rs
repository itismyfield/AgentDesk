//! Slot resets skip every thread with a row that is not a confirmed legacy tmux session.

use crate::services::engine_runtime::health_port::SlotResetOutcome;

use crate::services::auto_queue::runtime::test_support::take_runtime_clears_after_done;

/// The runtime clears recorded for `slot_threads` once their spawned loop signalled it finished.
pub(crate) async fn runtime_clears_after_done(
    slot_threads: &[u64],
) -> Vec<(u64, SlotResetOutcome)> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(clears) = take_runtime_clears_after_done(slot_threads) {
            return clears;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no completion signal for slot threads {slot_threads:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
#[path = "slot_reset_archive_fixture.rs"]
mod archive_fixture;

#[cfg(unix)]
pub(crate) mod host {
    use std::os::unix::fs::PermissionsExt as _;

    use poise::serenity_prelude::ChannelId;
    use sqlx::PgPool;

    use super::runtime_clears_after_done;
    use crate::config::TestEnvVarGuard;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::auto_queue::runtime::{
        clear_slot_threads_for_slot_pg, reset_slot_thread_bindings_excluding_pg,
        test_support::{runtime_clear_completion_count, runtime_target_session_keys},
    };
    use crate::services::discord::health::slot_row_host_refusal;
    use crate::services::discord::host_teardown_gate::test_support::{
        Stored, busy_turn, channel_key, inflight_needing_backfill, mailbox_turn_active,
        nameless_turn, running_session, runtime, runtime_state, seed, shared_on,
    };

    /// Where `tmux` resolves: a live isolated server, a missing binary, or no server socket.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Tmux {
        LiveServer,
        MissingBinary,
        NoServerSocket,
    }

    /// A call-logging `tmux` first on PATH and a private `TMUX_TMPDIR`; drop kills that server.
    struct TmuxEnv {
        live: bool,
        real: std::path::PathBuf,
        sockets: tempfile::TempDir,
        probe: tempfile::TempDir,
        _guards: Vec<TestEnvVarGuard>,
    }

    impl TmuxEnv {
        fn install(condition: Tmux) -> Self {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let real = std::env::split_paths(&path)
                .map(|dir| dir.join("tmux"))
                .find(|path| path.is_file())
                .expect("the live and socketless conditions need a real tmux binary");
            let (sockets, probe) = (
                tempfile::TempDir::new().unwrap(),
                tempfile::TempDir::new().unwrap(),
            );
            let tail = match condition {
                Tmux::MissingBinary => "exit 127".to_string(),
                _ => format!("exec '{}' \"$@\"", real.display()),
            };
            let binary = probe.path().join("tmux");
            let record = "[ \"$1\" = -u ] && shift\necho \"$*\" >> \"$(dirname \"$0\")/calls\"";
            std::fs::write(&binary, format!("#!/bin/sh\n{record}\n{tail}\n")).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
            let paths =
                std::iter::once(probe.path().to_path_buf()).chain(std::env::split_paths(&path));
            let path = std::env::join_paths(paths).unwrap();
            let set = TestEnvVarGuard::set_path_after_shared_test_env_lock;
            let guards = vec![
                set("TMUX_TMPDIR", sockets.path()),
                TestEnvVarGuard::capture_after_shared_test_env_lock("TMUX"),
                set("PATH", std::path::Path::new(&path)),
            ];
            unsafe { std::env::remove_var("TMUX") };
            let live = condition == Tmux::LiveServer;
            Self {
                live,
                real,
                sockets,
                probe,
                _guards: guards,
            }
        }

        fn real_tmux(&self, args: &[&str]) -> bool {
            let mut command = std::process::Command::new(&self.real);
            let command = command.args(args).env("TMUX_TMPDIR", self.sockets.path());
            let status = command
                .env_remove("TMUX")
                .stderr(std::process::Stdio::null())
                .status();
            status.is_ok_and(|status| status.success())
        }

        fn start(&self, name: &str) {
            let args = ["new-session", "-d", "-s", name, "sleep 600"];
            assert!(
                !self.live || self.real_tmux(&args),
                "start live tmux {name}"
            );
        }

        fn start_with(&self, name: &str, command: &str) {
            let args = ["new-session", "-d", "-s", name, command];
            assert!(
                !self.live || self.real_tmux(&args),
                "start live tmux {name}"
            );
        }

        fn alive(&self, name: &str) -> bool {
            self.real_tmux(&["has-session", "-t", &format!("={name}:")])
        }

        fn take_calls(&self) -> Vec<String> {
            let path = self.probe.path().join("calls");
            let calls = std::fs::read_to_string(&path).unwrap_or_default();
            let _ = std::fs::remove_file(path);
            calls.lines().map(str::to_string).collect()
        }
    }

    impl Drop for TmuxEnv {
        fn drop(&mut self) {
            let _ = self.real_tmux(&["kill-server"]);
        }
    }

    pub(crate) async fn exec(pool: &PgPool, sql: &'static str, binds: &[Option<&str>]) {
        let query = binds
            .iter()
            .fold(sqlx::query(sql), |query, bind| query.bind(*bind));
        query.execute(pool).await.unwrap();
    }

    /// One text value per row of `sql`.
    async fn rows(pool: &PgPool, sql: &'static str, bind: &str) -> Vec<String> {
        let query = sqlx::query_scalar::<_, String>(sql).bind(bind);
        query.fetch_all(pool).await.unwrap()
    }

    pub(crate) async fn thread_rows(pool: &PgPool, thread: u64) -> Vec<String> {
        let sql =
            "SELECT to_jsonb(s)::text FROM sessions s WHERE thread_channel_id = $1 ORDER BY id";
        rows(pool, sql, &thread.to_string()).await
    }

    pub(crate) async fn slot_map(pool: &PgPool, agent: &str) -> Vec<String> {
        let sql = "SELECT thread_id_map::text FROM auto_queue_slots WHERE agent_id = $1";
        rows(pool, sql, agent).await
    }

    /// Slot 0 of a new `agent`, bound to `threads`.
    pub(crate) async fn slot(pool: &PgPool, agent: &str, threads: &[u64]) {
        let sql = "INSERT INTO agents (id, name, provider) VALUES ($1, $1, 'claude')";
        exec(pool, sql, &[Some(agent)]).await;
        let map: serde_json::Map<_, _> = (threads.iter().enumerate())
            .map(|(n, thread)| (n.to_string(), serde_json::json!(thread.to_string())))
            .collect();
        let map = serde_json::Value::Object(map).to_string();
        let sql = "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
                   VALUES ($1, 0, $2::jsonb)";
        exec(pool, sql, &[Some(agent), Some(&map)]).await;
    }

    /// A `stored` row for `name` on `thread` with `status` and the raw `provider`; returns its key.
    pub(crate) async fn thread_row(
        pool: &PgPool,
        shared: &crate::services::discord::SharedData,
        (thread, name, stored): (u64, &str, Stored),
        status: &str,
        provider: Option<&str>,
    ) -> String {
        let key = channel_key(shared, name);
        seed(pool, &key, name, thread, stored).await;
        let sql = "UPDATE sessions SET thread_channel_id = $2, status = $3, provider = $4
                   WHERE session_key = $1";
        let thread = thread.to_string();
        exec(
            pool,
            sql,
            &[Some(&key), Some(&thread), Some(status), provider],
        )
        .await;
        key
    }

    mod activation_bridge {
        use super::*;
        use crate::engine::{PolicyEngine, hooks::Hook};
        use crate::services::session_host::host_decision::{
            force_presence_for_test, forced_probes, host_decision_for_rows,
        };

        const ACTIVATE: &str = "agentdesk.autoQueue.activate({run_id: 'slot-guard-run'})";

        async fn check_slot_clear(deferred: bool) {
            let root = crate::config::TestRuntimeRootGuard::new();
            crate::services::discord::install_engine_probes();
            let db = TestPostgresDb::create().await;
            let pool = db.connect_and_migrate_with_max_connections(8).await;
            let shared = crate::services::discord::make_shared_data_for_tests();
            exec(
                &pool,
                "INSERT INTO auto_queue_runs (id, repo, status, max_concurrent_threads, thread_group_count)
                 VALUES ('slot-guard-run', 'slot-guard-repo', 'active', 3, 3)",
                &[],
            )
            .await;
            let id = 1_480_000_000_007_000 + u64::from(deferred) * 10;
            let (hosted, legacy, unknown) = (id + 1, id + 2, id + 3);
            let raw_provider = "unrecognized-provider";
            for (group, agent, thread, stored, provider) in [
                ("0", "bridge-hosted", hosted, Stored::Hosted, "claude"),
                ("1", "bridge-legacy", legacy, Stored::Legacy, "claude"),
                ("2", "bridge-unknown", unknown, Stored::Legacy, raw_provider),
            ] {
                slot(&pool, agent, &[thread]).await;
                let name = format!("AgentDesk-claude-{agent}-{id}");
                let key = thread_row(
                    &pool,
                    &shared,
                    (thread, &name, stored),
                    "idle",
                    Some(provider),
                )
                .await;
                exec(
                    &pool,
                    "UPDATE sessions SET claude_session_id = 'prior-session', tokens = 19
                     WHERE session_key = $1",
                    &[Some(&key)],
                )
                .await;
                exec(
                    &pool,
                    "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, repo_id)
                     VALUES ($1, $1, 'in_progress', $1, 'slot-guard-repo')",
                    &[Some(agent)],
                )
                .await;
                exec(
                    &pool,
                    "INSERT INTO auto_queue_entries
                        (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
                     VALUES ($1, 'slot-guard-run', $1, $1, 'pending', $2::text::bigint, 0)",
                    &[Some(agent), Some(group)],
                )
                .await;
            }

            let unknown_name = format!("AgentDesk-claude-bridge-unknown-{id}");
            let unknown_key = channel_key(&shared, &unknown_name);
            {
                use crate::services::platform::tmux::SessionPresence;
                use crate::services::session_host::surface_policy::{
                    Relation, force_relation_for_test,
                };
                let _host = crate::config::session_hosts::force_for_test(
                    Some("mac-mini"),
                    &[(id, "mac-mini")],
                );
                let _relation = force_relation_for_test(unknown, Relation::Child { parent: id });
                force_presence_for_test(&unknown_name, SessionPresence::Missing);
                assert!(
                    host_decision_for_rows(&pool, unknown)
                        .await
                        .tmux_refusal()
                        .is_none()
                );
                assert_eq!(
                    forced_probes(&unknown_name),
                    1,
                    "the key identified the provider"
                );
            }
            assert!(
                host_decision_for_rows(&pool, unknown)
                    .await
                    .tmux_refusal()
                    .is_none()
            );
            let refused = crate::services::discord::host_teardown_gate::row_host_refusal(
                &pool,
                Some(raw_provider),
                Some(&unknown.to_string()),
                &unknown_key,
                &unknown_name,
                "auto_queue_slot_reset",
            )
            .await;
            assert!(refused.is_some_and(|reason| reason.contains("provider unknown")));
            let (before_hosted, before_unknown) = (
                thread_rows(&pool, hosted).await,
                thread_rows(&pool, unknown).await,
            );

            let mut config = crate::config::Config::default();
            config.policies.dir = root.path().join("policies");
            config.policies.hot_reload = false;
            std::fs::create_dir(&config.policies.dir).unwrap();
            std::fs::write(
                config.policies.dir.join("slot-guard.js"),
                format!(
                    "agentdesk.registerPolicy({{ name: 'slot-guard', onTick: function() {{
                        globalThis.activationDeferred = {ACTIVATE}.deferred;
                    }} }});"
                ),
            )
            .unwrap();
            let engine =
                PolicyEngine::new_with_pg(&config, Some(pool.clone()), slot_row_host_refusal)
                    .expect("assemble policy engine");
            if deferred {
                engine
                    .fire_hook(Hook::OnTick, serde_json::json!({}))
                    .unwrap();
                assert!(
                    engine
                        .eval_js::<bool>("globalThis.activationDeferred === true")
                        .unwrap()
                );
            } else {
                let js = format!(
                    "const result = {ACTIVATE}; Array.isArray(result.dispatched) && !result.deferred"
                );
                assert!(engine.eval_js::<bool>(&js).unwrap());
            }
            assert_eq!(
                rows(
                    &pool,
                    "SELECT agent_id FROM auto_queue_slots
                            WHERE assigned_run_id = $1 ORDER BY agent_id",
                    "slot-guard-run"
                )
                .await,
                ["bridge-hosted", "bridge-legacy", "bridge-unknown"],
            );
            assert_eq!(
                thread_rows(&pool, unknown).await,
                before_unknown,
                "the row guard keeps the unknown provider",
            );
            assert_eq!(
                thread_rows(&pool, hosted).await,
                before_hosted,
                "hosted row kept"
            );
            assert_eq!(
                rows(
                    &pool,
                    "SELECT (claude_session_id IS NULL AND tokens = 0
                             AND session_info = 'Slot thread reset')::text
                     FROM sessions WHERE thread_channel_id = $1",
                    &legacy.to_string(),
                )
                .await,
                ["true"],
                "legacy slot cleared before dispatch",
            );
            drop(engine);
            pool.close().await;
            db.drop().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn js_activate_clears_legacy_slot_and_keeps_guarded_rows_pg() {
            check_slot_clear(false).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn drained_activate_intent_clears_legacy_slot_and_keeps_guarded_rows_pg() {
            check_slot_clear(true).await;
        }
    }

    /// Both entries, under three tmux conditions, reset a thread only when every row it idles and
    /// its runtime-clear row are legacy; the runtime has no pool, so only the filter decides.
    #[tokio::test(flavor = "current_thread")]
    async fn slot_reset_skips_threads_with_any_unconfirmed_row_pg() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::TempDir::new().unwrap();
        let _root =
            TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
        let archive = super::archive_fixture::ArchiveFixture::install(root.path()).await;
        crate::services::discord::install_engine_probes();
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let registry = std::sync::Arc::new(crate::services::discord::health::HealthRegistry::new());
        registry
            .register("claude".to_string(), shared.clone())
            .await;
        let conditions = [Tmux::LiveServer, Tmux::MissingBinary, Tmux::NoServerSocket];
        for (round, condition) in conditions.into_iter().enumerate() {
            let tmux = TmuxEnv::install(condition);
            let id = |n: u64| 1_480_000_000_000_000 + round as u64 * 100 + n;
            let name = |n: u64| format!("AgentDesk-claude-p4r2-{round}-{n}");
            let ctx = format!("{condition:?}");

            // Clear: the legacy T1 (no stored provider) clears; T2, two-row T2b and the
            // disconnected T3 that only the runtime clear would use are untouched.
            let (t1, t2, t2b, t3) = (id(1), id(2), id(3), id(4));
            let agent = format!("p4r2-clear-{round}");
            slot(&pool, &agent, &[t1, t2, t2b, t3]).await;
            thread_row(&pool, &shared, (t1, &name(1), Stored::Legacy), "idle", None).await;
            thread_row(
                &pool,
                &shared,
                (t2, &name(2), Stored::Hosted),
                "idle",
                Some("claude"),
            )
            .await;
            let t2b_key = thread_row(
                &pool,
                &shared,
                (t2b, &name(3), Stored::Legacy),
                "idle",
                Some("claude"),
            )
            .await;
            // T2b's hosted row sits on its own channel (one identity row per channel) and is
            // older, so the runtime clear picks the legacy row and only the idled set holds it.
            let (hosted, row) = (name(5), id(9));
            let row = (row, hosted.as_str(), Stored::Hosted);
            let key = thread_row(&pool, &shared, row, "idle", Some("claude")).await;
            let sql = "UPDATE sessions SET thread_channel_id = $2,
                       last_heartbeat = NOW() - INTERVAL '1 hour' WHERE session_key = $1";
            exec(&pool, sql, &[Some(&key), Some(&t2b.to_string())]).await;
            let t3_key = thread_row(
                &pool,
                &shared,
                (t3, &name(4), Stored::Hosted),
                "disconnected",
                Some("claude"),
            )
            .await;
            for (thread, n) in [(t1, 1), (t2, 2), (t2b, 5), (t3, 4)] {
                nameless_turn(&shared, ChannelId::new(thread)).await;
                tmux.start(&name(n));
            }
            // T3 is a real runtime target no earlier check already keeps; T2b's is its legacy row.
            let chosen = runtime_target_session_keys(&pool, &agent, 0, &[t3, t2b])
                .await
                .unwrap();
            assert_eq!(chosen, [Some(t3_key), Some(t2b_key)], "{ctx}");
            let defer = crate::services::discord::should_defer_thread_archive_pg;
            assert_eq!(
                defer(Some(&pool), &t3.to_string()).await,
                Ok(false),
                "{ctx}"
            );
            assert!(
                !crate::services::discord::has_fresh_inflight_for_channel(t3),
                "{ctx}"
            );
            let kept = [t2, t2b, t3];
            let mut before = Vec::new();
            for thread in kept {
                before.push(thread_rows(&pool, thread).await);
            }
            let _ = tmux.take_calls();
            let clear = clear_slot_threads_for_slot_pg;
            let cleared = clear(
                slot_row_host_refusal,
                Some(registry.clone()),
                &pool,
                &agent,
                0,
            )
            .await
            .unwrap();
            assert_eq!(cleared, 1, "{ctx}: only T1's row is idled");
            for (thread, before) in kept.into_iter().zip(&before) {
                assert_eq!(
                    &thread_rows(&pool, thread).await,
                    before,
                    "{ctx}: {thread} unchanged"
                );
            }
            let clears = runtime_clears_after_done(&[t1, t2, t2b, t3]).await;
            let only_t1 = matches!(clears.as_slice(), [(t, Ok(_))] if *t == t1);
            assert!(only_t1, "{ctx}: {clears:?}");
            assert!(
                !mailbox_turn_active(&shared, ChannelId::new(t1)).await,
                "{ctx}"
            );
            for thread in kept {
                assert!(
                    mailbox_turn_active(&shared, ChannelId::new(thread)).await,
                    "{ctx}: {thread}"
                );
            }
            assert!(!tmux.alive(&name(1)), "{ctx}: the legacy session is reset");
            for n in [2, 4, 5] {
                assert!(
                    !tmux.live || tmux.alive(&name(n)),
                    "{ctx}: {} survives",
                    name(n)
                );
            }
            let calls = tmux.take_calls();
            assert!(
                calls.iter().all(|call| call.contains(&name(1))),
                "{ctx}: {calls:?}"
            );

            // Reset: a refused thread's compat inflight row needing a backfill keeps its bytes,
            // and the map, Discord archive and rows stay as they were.
            let t2c = id(6);
            let agent = format!("p4r2-hosted-{round}");
            slot(&pool, &agent, &[t2c]).await;
            thread_row(
                &pool,
                &shared,
                (t2c, &name(6), Stored::Hosted),
                "idle",
                Some("claude"),
            )
            .await;
            busy_turn(&shared, ChannelId::new(t2c), &name(6)).await;
            let inflight = inflight_needing_backfill(ChannelId::new(t2c));
            let (raw, rows, map) = (
                std::fs::read(&inflight).unwrap(),
                thread_rows(&pool, t2c).await,
                slot_map(&pool, &agent).await,
            );
            let reset = reset_slot_thread_bindings_excluding_pg;
            assert_eq!(
                reset(slot_row_host_refusal, &pool, &agent, 0, None, None).await,
                Ok((0, 0, 0)),
                "{ctx}"
            );
            assert_eq!(
                std::fs::read(&inflight).unwrap(),
                raw,
                "{ctx}: no inflight write"
            );
            assert_eq!(thread_rows(&pool, t2c).await, rows, "{ctx}");
            assert_eq!(slot_map(&pool, &agent).await, map, "{ctx}");

            // Reset: a selected row with neither provider nor key is refused, not dropped.
            let t4 = id(7);
            let agent = format!("p4r2-raw-{round}");
            slot(&pool, &agent, &[t4]).await;
            let sql = "INSERT INTO sessions (session_key, provider, status, thread_channel_id)
                       VALUES (NULL, NULL, 'disconnected', $1)";
            exec(&pool, sql, &[Some(&t4.to_string())]).await;
            let map = slot_map(&pool, &agent).await;
            assert_eq!(
                reset(slot_row_host_refusal, &pool, &agent, 0, None, None).await,
                Ok((0, 0, 0)),
                "{ctx}"
            );
            assert_eq!(
                slot_map(&pool, &agent).await,
                map,
                "{ctx}: the binding is kept"
            );

            // Reset positive: a legacy-only slot is idled and unbound as in main.
            let t5 = id(8);
            let agent = format!("p4r2-legacy-{round}");
            slot(&pool, &agent, &[t5]).await;
            thread_row(
                &pool,
                &shared,
                (t5, &name(8), Stored::Legacy),
                "idle",
                Some("claude"),
            )
            .await;
            let result = reset(slot_row_host_refusal, &pool, &agent, 0, None, None).await;
            assert_eq!(archive.take_patches(), [t5], "{ctx}: loopback PATCH");
            assert_eq!(result, Ok((1, 1, 1)), "{ctx}");
            assert_eq!(slot_map(&pool, &agent).await, ["{}"], "{ctx}");
            assert!(
                tmux.take_calls().is_empty(),
                "{ctx}: the resets run no tmux"
            );
        }
        pool.close().await;
        db.drop().await;
    }

    /// With a pool-connected runtime, under three tmux conditions, a slot clear takes each
    /// thread's runtime-clear verdict before its first write and runs only that verdict.
    #[tokio::test(flavor = "current_thread")]
    async fn slot_clear_runs_only_the_runtime_verdict_taken_before_the_update_pg() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::TempDir::new().unwrap();
        let _root =
            TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
        crate::services::discord::install_engine_probes();
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let (shared, registry) = runtime(&pool).await;
        let conditions = [Tmux::LiveServer, Tmux::MissingBinary, Tmux::NoServerSocket];
        for (round, condition) in conditions.into_iter().enumerate() {
            let tmux = TmuxEnv::install(condition);
            let id = |n: u64| 1_481_000_000_000_000 + round as u64 * 100 + n;
            let name = |n: u64| format!("AgentDesk-claude-p4r2v-{round}-{n}");
            let ctx = format!("{condition:?}");
            let (ta, tb, tc, td) = (id(1), id(2), id(3), id(4));
            let channel = ChannelId::new;
            let agent = format!("p4r2-verdict-{round}");
            slot(&pool, &agent, &[ta, tb, tc, td]).await;
            let legacy = |thread: u64, n: u64| (thread, name(n), Stored::Legacy);
            for (thread, n) in [(ta, 1), (tc, 3), (td, 5)] {
                let (thread, name, stored) = legacy(thread, n);
                thread_row(
                    &pool,
                    &shared,
                    (thread, &name, stored),
                    "idle",
                    Some("claude"),
                )
                .await;
            }
            for (thread, n) in [(ta, 1), (td, 5)] {
                running_session(&shared, &pool, channel(thread), &name(n), false, None).await;
            }
            // B is hosted and its compat inflight row would be saved again by a full inflight load.
            let b_name = name(2);
            let row = (tb, b_name.as_str(), Stored::Hosted);
            thread_row(&pool, &shared, row, "idle", Some("claude")).await;
            busy_turn(&shared, channel(tb), &name(2)).await;
            let inflight = inflight_needing_backfill(channel(tb));
            let raw = std::fs::read(&inflight).unwrap();
            // C's runtime names the channel's current hosted row, disconnected so the update skips it.
            running_session(&shared, &pool, channel(tc), &name(4), false, Some(&name(4))).await;
            let sql = "UPDATE sessions SET thread_channel_id = $2, status = 'disconnected'
                       WHERE session_key = $1";
            let hosted_key = channel_key(&shared, &name(4));
            exec(&pool, sql, &[Some(&hosted_key), Some(&tc.to_string())]).await;
            for n in 1..=6 {
                tmux.start(&name(n));
            }
            let (b_rows, c_rows) = (thread_rows(&pool, tb).await, thread_rows(&pool, tc).await);
            let state = |thread: u64| runtime_state(&shared, channel(thread));
            let (a_state, b_state, c_state) = (state(ta).await, state(tb).await, state(tc).await);
            let _ = tmux.take_calls();

            let clear = clear_slot_threads_for_slot_pg;
            let cleared = clear(
                slot_row_host_refusal,
                Some(registry.clone()),
                &pool,
                &agent,
                0,
            )
            .await;
            // Before the spawned clear runs, D's runtime switches to another session's name.
            running_session(&shared, &pool, channel(td), &name(6), false, None).await;
            let slot_threads = [ta, tb, tc, td];
            assert_eq!(
                runtime_clear_completion_count(&slot_threads),
                0,
                "{ctx}: the rename runs before the spawned clear"
            );
            assert_eq!(cleared, Ok(2), "{ctx}: only A's and D's rows are idled");

            // Waits for the spawned clears; their records are checked after the effects.
            let clears = runtime_clears_after_done(&slot_threads).await;
            assert_eq!(thread_rows(&pool, tb).await, b_rows, "{ctx}");
            assert_eq!(thread_rows(&pool, tc).await, c_rows, "{ctx}");
            assert_eq!(runtime_state(&shared, channel(tb)).await, b_state, "{ctx}");
            assert_eq!(runtime_state(&shared, channel(tc)).await, c_state, "{ctx}");
            assert_eq!(
                std::fs::read(&inflight).unwrap(),
                raw,
                "{ctx}: B's inflight bytes"
            );
            assert_ne!(
                state(ta).await,
                a_state,
                "{ctx}: A's provider session is cleared"
            );
            let renamed = name(6).replace("AgentDesk-claude-", "") + "-sid";
            assert!(
                state(td).await.contains(&renamed),
                "{ctx}: D's renamed session is kept"
            );
            assert!(!tmux.alive(&name(1)), "{ctx}: A's session is reset");
            for n in 2..=6 {
                assert!(
                    !tmux.live || tmux.alive(&name(n)),
                    "{ctx}: {} survives",
                    name(n)
                );
            }
            let calls = tmux.take_calls();
            assert!(
                calls.iter().all(|call| call.contains(&name(1))),
                "{ctx}: {calls:?}"
            );
            let applied = Ok(Some(name(1)));
            assert!(
                matches!(clears.as_slice(), [(a, a_reset), (d, Err(_))]
                    if *a == ta && *a_reset == applied && *d == td),
                "{ctx}: {clears:?}"
            );
        }
        pool.close().await;
        db.drop().await;
    }

    /// With a pool-connected runtime, under three tmux conditions, a slot clear stops a turn only
    /// when its verdict judged the session the turn runs, and then stops it on that verdict alone.
    #[tokio::test(flavor = "current_thread")]
    async fn slot_clear_stops_only_the_turn_its_verdict_approved_pg() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::TempDir::new().unwrap();
        let _root =
            TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
        crate::services::discord::install_engine_probes();
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let (shared, registry) = runtime(&pool).await;
        let qwen = shared_on(&pool).await;
        registry.register("qwen".to_string(), qwen.clone()).await;
        let conditions = [Tmux::LiveServer, Tmux::MissingBinary, Tmux::NoServerSocket];
        for (round, condition) in conditions.into_iter().enumerate() {
            let tmux = TmuxEnv::install(condition);
            let id = |n: u64| 1_482_000_000_000_000 + round as u64 * 100 + n;
            let name = |n: u64| format!("AgentDesk-claude-p4r2t-{round}-{n}");
            let ctx = format!("{condition:?}");
            let (te, tf, channel) = (id(1), id(2), ChannelId::new);
            let agent = format!("p4r2-turn-{round}");
            slot(&pool, &agent, &[te, tf]).await;
            // No inflight row is left behind, so the archive-defer probe does not skip the turns.
            let drop_inflight = |thread: u64| {
                std::fs::remove_file(inflight_needing_backfill(channel(thread))).unwrap();
            };

            // E: a Qwen turn running the session its legacy row names; an INT reaches its pane.
            let e_name = format!("AgentDesk-qwen-p4r2t-{round}-1");
            let e_key = format!("qwen/test-token-hash/test-host:{e_name}");
            seed(&pool, &e_key, &e_name, te, Stored::Legacy).await;
            let sql = "UPDATE sessions SET thread_channel_id = $2, status = 'awaiting_user',
                       provider = 'qwen', claude_session_id = 'e-sid' WHERE session_key = $1";
            exec(&pool, sql, &[Some(&e_key), Some(&te.to_string())]).await;
            let e_token = busy_turn(&qwen, channel(te), &e_name).await;
            drop_inflight(te);
            let flags = tempfile::TempDir::new().unwrap();
            let flag = flags.path().join("interrupted");
            let trap = format!(
                "trap 'echo INT >> {}' INT; while :; do sleep 0.1; done",
                flag.display()
            );
            tmux.start_with(&e_name, &trap);

            // F: the legacy row and channel name F judges, but its turn runs a Herdr session.
            thread_row(
                &pool,
                &shared,
                (tf, &name(2), Stored::Legacy),
                "idle",
                Some("claude"),
            )
            .await;
            running_session(&shared, &pool, channel(tf), &name(2), false, None).await;
            let f_token = busy_turn(&shared, channel(tf), &name(3)).await;
            drop_inflight(tf);
            seed(&pool, "unused", &name(3), tf, Stored::MissingHerdrMarker).await;
            for n in [2, 3] {
                tmux.start(&name(n));
            }
            let f_rows = thread_rows(&pool, tf).await;
            let f_state = runtime_state(&shared, channel(tf)).await;
            let _ = tmux.take_calls();

            let clear = clear_slot_threads_for_slot_pg;
            let cleared = clear(
                slot_row_host_refusal,
                Some(registry.clone()),
                &pool,
                &agent,
                0,
            )
            .await;
            // Before the spawned clear runs, E's marker turns Herdr; its verdict was taken already.
            let marker = crate::services::tmux_common::session_temp_path(&e_name, "host_kind");
            std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(&marker, "herdr").unwrap();
            let slot_threads = [te, tf];
            assert_eq!(
                runtime_clear_completion_count(&slot_threads),
                0,
                "{ctx}: the marker is written before the spawned clear"
            );
            assert_eq!(cleared, Ok(1), "{ctx}: only E's row is idled");

            // Waits for the spawned clears; their records are checked after the effects.
            let clears = runtime_clears_after_done(&slot_threads).await;
            let sql = "SELECT status || '/' || COALESCE(claude_session_id, '-')
                       FROM sessions WHERE session_key = $1";
            assert_eq!(rows(&pool, sql, &e_key).await, ["idle/-"], "{ctx}");
            assert!(!mailbox_turn_active(&qwen, channel(te)).await, "{ctx}");
            let cancelled = |token: &crate::services::provider::CancelToken| {
                token.cancelled.load(std::sync::atomic::Ordering::SeqCst)
            };
            assert!(cancelled(&e_token), "{ctx}: E's approved turn is stopped");
            let interrupted = std::fs::read_to_string(&flag).unwrap_or_default();
            assert!(
                !tmux.live || interrupted.contains("INT"),
                "{ctx}: E's pane receives the stop's interrupt"
            );
            assert_eq!(thread_rows(&pool, tf).await, f_rows, "{ctx}");
            assert_eq!(runtime_state(&shared, channel(tf)).await, f_state, "{ctx}");
            assert!(!cancelled(&f_token), "{ctx}: F's turn keeps running");
            for n in [2, 3] {
                assert!(
                    !tmux.live || tmux.alive(&name(n)),
                    "{ctx}: {} survives",
                    name(n)
                );
            }
            let applied = Ok(Some(e_name.clone()));
            assert!(
                matches!(clears.as_slice(), [(e, reset)] if *e == te && *reset == applied),
                "{ctx}: {clears:?}"
            );
        }
        pool.close().await;
        db.drop().await;
    }

    /// A Herdr-configured legacy idle thread keeps its rows and binding through the
    /// registry-less reset and a clear whose registry yields no runtime verdict.
    #[tokio::test(flavor = "current_thread")]
    async fn slot_reset_keeps_a_configured_thread_with_or_without_a_runtime_verdict_pg() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::TempDir::new().unwrap();
        let _root =
            TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
        crate::services::discord::install_engine_probes();
        let tmux = TmuxEnv::install(Tmux::MissingBinary);
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let id = |n: u64| 1_480_000_000_000_900 + n;
        let name = |n: u64| format!("AgentDesk-claude-p9b1-slot-{n}");
        let (configured, plain) = ([id(1), id(2)], id(3));
        let forced: Vec<_> = configured.iter().map(|t| (*t, "mac-mini")).collect();
        let _hosts = crate::config::session_hosts::force_for_test(Some("mac-mini"), &forced);
        for (n, thread) in [(1, configured[0]), (2, configured[1]), (3, plain)] {
            let tmux_name = name(n);
            let row = (thread, tmux_name.as_str(), Stored::Legacy);
            thread_row(&pool, &shared, row, "idle", Some("claude")).await;
            slot(&pool, &format!("p9b1-slot-{n}"), &[thread]).await;
        }
        let before = |thread: u64, agent: &'static str| {
            let pool = pool.clone();
            async move {
                (
                    thread_rows(&pool, thread).await,
                    slot_map(&pool, agent).await,
                )
            }
        };

        let reset = reset_slot_thread_bindings_excluding_pg;
        let kept = before(configured[0], "p9b1-slot-1").await;
        let done = reset(slot_row_host_refusal, &pool, "p9b1-slot-1", 0, None, None).await;
        assert_eq!(done, Ok((0, 0, 0)), "registry-less reset");
        assert_eq!(before(configured[0], "p9b1-slot-1").await, kept, "no write");

        let empty = std::sync::Arc::new(crate::services::discord::health::HealthRegistry::new());
        let kept = before(configured[1], "p9b1-slot-2").await;
        let clear = clear_slot_threads_for_slot_pg;
        let cleared = clear(slot_row_host_refusal, Some(empty), &pool, "p9b1-slot-2", 0).await;
        assert_eq!(cleared, Ok(0), "no runtime verdict");
        assert_eq!(before(configured[1], "p9b1-slot-2").await, kept, "no write");

        assert_eq!(
            reset(slot_row_host_refusal, &pool, "p9b1-slot-3", 0, None, None).await,
            Ok((0, 1, 1))
        );
        assert_eq!(
            slot_map(&pool, "p9b1-slot-3").await,
            ["{}"],
            "unconfigured resets"
        );
        assert!(tmux.take_calls().is_empty(), "the resets run no tmux");
        pool.close().await;
        db.drop().await;
    }

    /// Under a Herdr parent, a slot thread whose rows thread_gc deleted, and one whose only row has
    /// a legacy `host:tmux` key and no provider, release their binding; a failed probe keeps it.
    #[tokio::test(flavor = "current_thread")]
    async fn a_herdr_parents_gc_or_legacy_keyed_slot_thread_releases_its_binding_pg() {
        use crate::services::platform::tmux::SessionPresence;
        use crate::services::session_host::host_decision::force_presence_for_test;
        use crate::services::session_host::surface_policy::{Relation, force_relation_for_test};
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::TempDir::new().unwrap();
        let _root =
            TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
        crate::services::discord::install_engine_probes();
        let tmux = TmuxEnv::install(Tmux::MissingBinary);
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let parent = 1_480_000_000_000_950;
        let (gone, legacy, failed) = (parent + 1, parent + 2, parent + 3);
        let _hosts =
            crate::config::session_hosts::force_for_test(Some("mac-mini"), &[(parent, "mac-mini")]);
        let _relations: Vec<_> = [gone, legacy, failed]
            .into_iter()
            .map(|thread| force_relation_for_test(thread, Relation::Child { parent }))
            .collect();
        for (n, thread) in [(1, gone), (2, legacy), (3, failed)] {
            slot(&pool, &format!("s1b-fix-slot-{n}"), &[thread]).await;
        }
        for (thread, name, presence) in [
            (
                legacy,
                "AgentDesk-claude-s1b-fix-legacy",
                SessionPresence::Missing,
            ),
            (
                failed,
                "AgentDesk-claude-s1b-fix-failed",
                SessionPresence::ProbeFailed,
            ),
        ] {
            let sql = "INSERT INTO sessions (session_key, status, thread_channel_id)
                       VALUES ($1, 'idle', $2)";
            let key = format!("mac-mini:{name}");
            exec(&pool, sql, &[Some(&key), Some(&thread.to_string())]).await;
            force_presence_for_test(name, presence);
        }
        let reset = reset_slot_thread_bindings_excluding_pg;
        assert_eq!(
            reset(
                slot_row_host_refusal,
                &pool,
                "s1b-fix-slot-1",
                0,
                None,
                None
            )
            .await,
            Ok((0, 0, 1))
        );
        assert_eq!(slot_map(&pool, "s1b-fix-slot-1").await, ["{}"], "rows gone");
        assert_eq!(
            reset(
                slot_row_host_refusal,
                &pool,
                "s1b-fix-slot-2",
                0,
                None,
                None
            )
            .await,
            Ok((0, 1, 1))
        );
        assert_eq!(
            slot_map(&pool, "s1b-fix-slot-2").await,
            ["{}"],
            "legacy key"
        );
        assert_eq!(
            reset(
                slot_row_host_refusal,
                &pool,
                "s1b-fix-slot-3",
                0,
                None,
                None
            )
            .await,
            Ok((0, 0, 0))
        );
        let kept = format!("{{\"0\": \"{failed}\"}}");
        assert_eq!(
            slot_map(&pool, "s1b-fix-slot-3").await,
            [kept],
            "probe failed"
        );
        assert!(tmux.take_calls().is_empty(), "the resets run no tmux");
        pool.close().await;
        db.drop().await;
    }
}
