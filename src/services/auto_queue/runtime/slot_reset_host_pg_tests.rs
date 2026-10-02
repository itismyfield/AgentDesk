//! Slot resets skip every thread with a row that is not a confirmed legacy tmux session.

use crate::services::discord::admin_host_guard::ManagedReset;

/// The runtime clears recorded for `slot_threads` once their spawned loop signalled it finished.
pub(crate) async fn runtime_clears_after_done(
    slot_threads: &[u64],
) -> Vec<(u64, Option<ManagedReset>)> {
    let done = || {
        let runs = super::RUNTIME_CLEARS_DONE.lock();
        let runs = runs.unwrap_or_else(|poison| poison.into_inner());
        runs.iter().any(|run| run.as_slice() == slot_threads)
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !done() {
        assert!(
            std::time::Instant::now() < deadline,
            "no completion signal for slot threads {slot_threads:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let clears = super::RUNTIME_CLEARS.lock();
    let clears = clears.unwrap_or_else(|poison| poison.into_inner());
    let ours = clears
        .iter()
        .filter(|(thread, _)| slot_threads.contains(thread));
    ours.cloned().collect()
}

#[cfg(unix)]
mod host {
    use std::os::unix::fs::PermissionsExt as _;

    use poise::serenity_prelude::ChannelId;
    use sqlx::PgPool;

    use super::runtime_clears_after_done;
    use crate::config::TestEnvVarGuard;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::discord::admin_host_guard::ManagedReset;
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

    async fn exec(pool: &PgPool, sql: &str, binds: &[Option<&str>]) {
        let query = binds
            .iter()
            .fold(sqlx::query(sql), |query, bind| query.bind(*bind));
        query.execute(pool).await.unwrap();
    }

    /// One text value per row of `sql`.
    async fn rows(pool: &PgPool, sql: &str, bind: &str) -> Vec<String> {
        let query = sqlx::query_scalar::<_, String>(sql).bind(bind);
        query.fetch_all(pool).await.unwrap()
    }

    async fn thread_rows(pool: &PgPool, thread: u64) -> Vec<String> {
        let sql =
            "SELECT to_jsonb(s)::text FROM sessions s WHERE thread_channel_id = $1 ORDER BY id";
        rows(pool, sql, &thread.to_string()).await
    }

    async fn slot_map(pool: &PgPool, agent: &str) -> Vec<String> {
        let sql = "SELECT thread_id_map::text FROM auto_queue_slots WHERE agent_id = $1";
        rows(pool, sql, agent).await
    }

    /// Slot 0 of a new `agent`, bound to `threads`.
    async fn slot(pool: &PgPool, agent: &str, threads: &[u64]) {
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
    async fn thread_row(
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

    /// Both entries, under three tmux conditions, reset a thread only when every row it idles and
    /// its runtime-clear row are legacy; the runtime has no pool, so only the filter decides.
    #[tokio::test(flavor = "current_thread")]
    async fn slot_reset_skips_threads_with_any_unconfirmed_row_pg() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::TempDir::new().unwrap();
        let _root =
            TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
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
            let target = super::super::build_slot_clear_target_pg(&pool, &agent, 0)
                .await
                .unwrap();
            let chosen = |thread: u64| {
                let mut found = target.runtime_targets.iter();
                let found = found.find(|target| target.thread_channel_id == thread);
                found.and_then(|target| target.session_key.clone())
            };
            let chosen = [chosen(t3), chosen(t2b)];
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
            let clear = super::super::clear_slot_threads_for_slot_pg;
            let cleared = clear(Some(registry.clone()), &pool, &agent, 0)
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
            let only_t1 =
                matches!(clears.as_slice(), [(t, Some(ManagedReset::Applied(_)))] if *t == t1);
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
            let reset = super::super::reset_slot_thread_bindings_excluding_pg;
            assert_eq!(
                reset(&pool, &agent, 0, None, None).await,
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
                reset(&pool, &agent, 0, None, None).await,
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
            assert_eq!(
                reset(&pool, &agent, 0, None, None).await,
                Ok((0, 1, 1)),
                "{ctx}"
            );
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

            let clear = super::super::clear_slot_threads_for_slot_pg;
            let cleared = clear(Some(registry.clone()), &pool, &agent, 0).await;
            // Before the spawned clear runs, D's runtime switches to another session's name.
            running_session(&shared, &pool, channel(td), &name(6), false, None).await;
            let slot_threads = [ta, tb, tc, td];
            let runs = super::super::RUNTIME_CLEARS_DONE.lock().unwrap();
            let early = runs.iter().any(|run| run.as_slice() == slot_threads);
            drop(runs);
            assert!(!early, "{ctx}: the rename runs before the spawned clear");
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
            let applied = Some(ManagedReset::Applied(Some(name(1))));
            assert!(
                matches!(clears.as_slice(), [(a, a_reset), (d, Some(ManagedReset::Refused(_)))]
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

            let clear = super::super::clear_slot_threads_for_slot_pg;
            let cleared = clear(Some(registry.clone()), &pool, &agent, 0).await;
            // Before the spawned clear runs, E's marker turns Herdr; its verdict was taken already.
            let marker = crate::services::tmux_common::session_temp_path(&e_name, "host_kind");
            std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(&marker, "herdr").unwrap();
            let slot_threads = [te, tf];
            let runs = super::super::RUNTIME_CLEARS_DONE.lock().unwrap();
            let early = runs.iter().any(|run| run.as_slice() == slot_threads);
            drop(runs);
            assert!(
                !early,
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
            let applied = Some(ManagedReset::Applied(Some(e_name.clone())));
            assert!(
                matches!(clears.as_slice(), [(e, reset)] if *e == te && *reset == applied),
                "{ctx}: {clears:?}"
            );
        }
        pool.close().await;
        db.drop().await;
    }
}
