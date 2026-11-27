//! Resume rebind tests that drive ROOT health and DL shared data through the KE resume path.
use std::sync::Arc;

use crate::services::discord::health::{
    HealthRegistry, rebind_channel_provider_session, resume_ports,
};
use crate::services::discord::host_teardown_gate::test_support::{
    channel_key, session_transition_lock, shared_on,
};
use crate::services::provider::ProviderKind;
use crate::services::session_resume::*;
use filetime::{FileTime, set_file_mtime};
use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

async fn perform_resume_rebind(
    pool: &PgPool,
    registry: Option<&HealthRegistry>,
    session_key: &str,
    provider: Option<ProviderKind>,
    channel_id: Option<ChannelId>,
    tmux_name: &str,
    opts: &ResumePreviousOptions,
) -> Result<ResumeRebindOutcome, ResumeRebindError> {
    let home = tempfile::tempdir().unwrap();
    if let (Some(cwd), Some(id)) = (&opts.cwd, &opts.session_id) {
        write_transcript(home.path(), std::path::Path::new(cwd), id, 1);
    }
    catalog::scope_claude_home_for_tests(
        home.path().to_path_buf(),
        crate::services::session_resume::perform_resume_rebind(
            pool,
            resume_ports(registry),
            session_key,
            provider,
            channel_id,
            tmux_name,
            opts,
        ),
    )
    .await
}

fn write_transcript(
    claude_home: &std::path::Path,
    cwd: &std::path::Path,
    sid: &str,
    mtime_secs: i64,
) -> std::path::PathBuf {
    let dir = crate::services::claude_tui::transcript_tail::claude_project_dir_for_cwd(
        cwd,
        Some(claude_home),
    )
    .unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, b"{}\n").unwrap();
    set_file_mtime(&path, FileTime::from_unix_time(mtime_secs, 0)).unwrap();
    path
}

fn runtime_binding(session_id: &str) -> crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
    crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        output_path: format!("/runtime/{session_id}.jsonl"),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session_id.to_string()),
        last_offset: 0,
        relay_last_offset: None,
    }
}

#[test]
fn resume_production_path_clears_stale_binding_and_rebinds_runtime() {
    crate::services::tui_prompt_dedupe::run_serial_test(async {
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        // As in production, the runtime has a pool and the row is the channel's own key, so
        // the force-kill host guard finds the legacy row it admits.
        let shared = shared_on(&pool).await;
        let registry = HealthRegistry::new();
        registry
            .register("claude".to_string(), Arc::clone(&shared))
            .await;
        let channel_id = ChannelId::new(4_794_101);
        let tmux = "AgentDesk-claude-resume-production-path";
        let unrelated = "AgentDesk-claude-resume-unrelated";
        let session_key = &channel_key(&shared, tmux);
        let old_session_id = "11111111-1111-1111-1111-111111111111";
        let target_session_id = "22222222-2222-2222-2222-222222222222";
        let old_cwd = tempfile::tempdir().expect("old cwd");
        let target_cwd = tempfile::tempdir().expect("target cwd");
        let old_cwd = old_cwd.path().to_str().expect("utf8 old cwd");
        let target_cwd = target_cwd.path().to_str().expect("utf8 target cwd");
        // #5185: this test asserts the `DeadOrAbsent` branch. Left to the real
        // probe, that answer comes from a `tmux has-session` subprocess under a
        // two-second wall-clock bound, and a bound that is generous on an idle
        // machine is not generous inside a ~6.9k-test parallel sweep: the probe
        // times out, maps to `ProbeError`, and the assertion below sees
        // `RetainedLive`. Measured exactly that way in a full sweep, passing
        // when run alone. State the pane's liveness instead of racing for it.
        //
        // Held as a guard, not cleared after the assertions: a failing assert
        // unwinds, and a hand-rolled cleanup at the bottom of the test would
        // then leak the forced answer into the process-global override map --
        // the same shape this branch removed from `tmux_common.rs`.
        let _liveness_override = crate::services::tmux_diagnostics::PaneLivenessOverrideGuard::set(
            tmux,
            crate::services::platform::tmux::PaneLiveness::DeadOrAbsent,
        );

        sqlx::query(
            "INSERT INTO sessions
             (session_key, provider, status, cwd, claude_session_id,
              raw_provider_session_id, last_heartbeat)
             VALUES ($1, 'claude', 'idle', $2, $3, $3, NOW())",
        )
        .bind(session_key)
        .bind(old_cwd)
        .bind(old_session_id)
        .execute(&pool)
        .await
        .expect("seed resume session");
        rebind_channel_provider_session(
            &shared,
            &ProviderKind::Claude,
            channel_id,
            old_cwd,
            old_session_id,
            tmux,
            false,
        )
        .await;
        crate::services::tui_prompt_dedupe::register_provider_session(
            "claude",
            old_session_id,
            tmux,
        );
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux,
            runtime_binding(old_session_id),
        );
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            unrelated,
            runtime_binding("unrelated-sid"),
        );

        let outcome = perform_resume_rebind(
            &pool,
            Some(&registry),
            session_key,
            Some(ProviderKind::Claude),
            Some(channel_id),
            tmux,
            &ResumePreviousOptions {
                session_id: Some(target_session_id.to_string()),
                cwd: Some(target_cwd.to_string()),
            },
        )
        .await
        .expect("resume target");

        assert_eq!(outcome.lifecycle_path, "runtime-fallback");
        assert!(outcome.in_memory_rebound);
        assert_eq!(
            outcome.runtime_binding_clear,
            ResumeRuntimeBindingClearOutcome::Cleared {
                tmux_session: tmux.to_string(),
            }
        );
        assert!(
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux).is_none(),
            "the retired runtime must not restore its provider session id"
        );
        assert_eq!(
            crate::services::tui_prompt_dedupe::provider_session_for_tmux("claude", tmux),
            None
        );
        assert!(
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(unrelated)
                .is_some(),
            "resume must retain unrelated runtime bindings"
        );
        let (selected_session_id, selected_cwd) =
            crate::services::discord::resume_launch_state_for_tests(&shared, channel_id)
                .await
                .expect("target launch state");
        assert_eq!(selected_session_id.as_deref(), Some(target_session_id));
        assert_eq!(selected_cwd, target_cwd);

        let durable: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT cwd, claude_session_id, raw_provider_session_id
             FROM sessions WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .expect("load durable resume target");
        assert_eq!(durable.0.as_deref(), Some(target_cwd));
        assert_eq!(durable.1.as_deref(), Some(target_session_id));
        assert_eq!(durable.2.as_deref(), Some(target_session_id));

        pool.close().await;
        pg_db.drop().await;
    });
}

#[cfg(unix)]
#[test]
fn resume_production_path_retains_live_owner_after_teardown() {
    let _env_guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    if !crate::services::platform::tmux::is_available() {
        eprintln!("skipping retained-live resume test: tmux unavailable");
        return;
    }
    crate::services::tui_prompt_dedupe::run_serial_test(async {
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let registry = HealthRegistry::new();
        registry
            .register("claude".to_string(), Arc::clone(&shared))
            .await;
        let channel_id = ChannelId::new(4_794_103);
        let tmux = format!("AgentDesk-resume-retained-live-{}", std::process::id());
        let session_key = channel_key(&shared, &tmux);
        let old_session_id = "77777777-7777-7777-7777-777777777777";
        let target_session_id = "88888888-8888-8888-8888-888888888888";
        let old_cwd = tempfile::tempdir().expect("old cwd");
        let target_cwd = tempfile::tempdir().expect("target cwd");
        let old_cwd = old_cwd.path().to_str().expect("utf8 old cwd");
        let target_cwd = target_cwd.path().to_str().expect("utf8 target cwd");

        let created = crate::services::platform::tmux::create_session(&tmux, None, "sleep 60")
            .expect("create retained-live tmux session");
        assert!(created.status.success(), "tmux session must start");
        sqlx::query(
            "INSERT INTO sessions
             (session_key, provider, status, cwd, claude_session_id,
              raw_provider_session_id, last_heartbeat)
             VALUES ($1, 'claude', 'idle', $2, $3, $3, NOW())",
        )
        .bind(&session_key)
        .bind(old_cwd)
        .bind(old_session_id)
        .execute(&pool)
        .await
        .expect("seed retained-live session");
        rebind_channel_provider_session(
            &shared,
            &ProviderKind::Claude,
            channel_id,
            old_cwd,
            old_session_id,
            &tmux,
            false,
        )
        .await;
        crate::services::discord::register_resume_watcher_for_tests(&shared, channel_id, &tmux);
        crate::services::tui_prompt_dedupe::register_provider_session(
            "claude",
            old_session_id,
            &tmux,
        );
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            &tmux,
            runtime_binding(old_session_id),
        );
        crate::services::turn_lifecycle::set_force_kill_preserve_tmux_for_test(&tmux, true);

        let outcome = perform_resume_rebind(
            &pool,
            Some(&registry),
            &session_key,
            Some(ProviderKind::Claude),
            Some(channel_id),
            &tmux,
            &ResumePreviousOptions {
                session_id: Some(target_session_id.to_string()),
                cwd: Some(target_cwd.to_string()),
            },
        )
        .await
        .expect("retained-live resume target");

        assert_eq!(
            outcome.runtime_binding_clear,
            ResumeRuntimeBindingClearOutcome::RetainedLive {
                tmux_session: tmux.clone(),
            }
        );
        assert_eq!(
            crate::services::discord::resume_owner_channel_for_tests(&shared, &tmux),
            Some(channel_id),
            "production teardown must leave an authoritative owner-only binding"
        );
        assert!(
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&tmux).is_some(),
            "retained live transcript binding and offset must survive"
        );

        crate::services::turn_lifecycle::set_force_kill_preserve_tmux_for_test(&tmux, false);
        let _ = crate::services::platform::tmux::kill_session(&tmux, "retained-live test cleanup");
        pool.close().await;
        pg_db.drop().await;
    });
}

#[tokio::test]
async fn production_resume_lock_blocks_effective_intake_snapshot_until_rebind_finishes() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let registry = HealthRegistry::new();
    registry
        .register("claude".to_string(), Arc::clone(&shared))
        .await;
    let channel_id = ChannelId::new(4_794_102);
    let old_cwd = tempfile::tempdir().expect("old cwd");
    let target_cwd = tempfile::tempdir().expect("target cwd");
    let old_cwd = old_cwd.path().to_str().expect("utf8 old cwd").to_string();
    let target_cwd = target_cwd
        .path()
        .to_str()
        .expect("utf8 target cwd")
        .to_string();
    let old_session_id = "44444444-4444-4444-4444-444444444444";
    let target_session_id = "55555555-5555-5555-5555-555555555555";
    let tmux = "AgentDesk-claude-resume-intake-lock";
    let session_key =
        crate::services::discord::host_teardown_gate::test_support::channel_key(&shared, tmux);
    sqlx::query(
        "INSERT INTO sessions
         (session_key, provider, status, cwd, claude_session_id,
          raw_provider_session_id, last_heartbeat)
         VALUES ($1, 'claude', 'idle', $2, $3, $3, NOW())",
    )
    .bind(&session_key)
    .bind(&old_cwd)
    .bind(old_session_id)
    .execute(&pool)
    .await
    .expect("seed lock test session");
    rebind_channel_provider_session(
        &shared,
        &ProviderKind::Claude,
        channel_id,
        &old_cwd,
        old_session_id,
        tmux,
        false,
    )
    .await;

    let held = session_transition_lock(&shared, channel_id)
        .lock_owned()
        .await;
    #[cfg(unix)]
    let queue_dir = tempfile::tempdir().expect("owner queue directory");
    #[cfg(unix)]
    let (owner, wake_probe, mut wakes) = {
        use crate::services::discord::host_teardown_gate::test_support::ResumeOwnerWakeProbe;
        use crate::services::turn_orchestrator::PendingQueueItem;
        let owner = crate::services::discord::input_mailbox_for_test(&shared, channel_id.get());
        owner
            .v2_adopt(
                queue_dir.path().join("owner.json"),
                "claude/resume-test".into(),
            )
            .await
            .expect("adopt the channel owner");
        let waiting: PendingQueueItem = serde_json::from_value(serde_json::json!({
            "author_id": 1, "message_id": 2, "queued_generation": 5,
            "channel_id": channel_id.get(), "text": "waiting for resume",
        }))
        .unwrap();
        assert!(
            owner
                .v2_request(move |slot| {
                    slot.as_mut()
                        .expect("installed owner")
                        .enqueue_fixture(&waiting)
                        .map_err(|error| format!("{error:?}"))
                })
                .await
                .unwrap()
        );
        let (probe, events) = ResumeOwnerWakeProbe::install(&shared, channel_id, &owner);
        (owner, probe, events)
    };
    let resume_pool = pool.clone();
    let resume_registry = registry;
    let resume_target_cwd = target_cwd.clone();
    let resume_key = session_key.clone();
    let resume = tokio::spawn(async move {
        perform_resume_rebind(
            &resume_pool,
            Some(&resume_registry),
            &resume_key,
            Some(ProviderKind::Claude),
            Some(channel_id),
            tmux,
            &ResumePreviousOptions {
                session_id: Some(target_session_id.to_string()),
                cwd: Some(resume_target_cwd),
            },
        )
        .await
    });
    tokio::task::yield_now().await;
    assert!(
        !resume.is_finished(),
        "production resume must acquire the channel lock"
    );
    #[cfg(unix)]
    assert!(
        wakes.try_recv().is_err(),
        "no owner wake while the lock is held"
    );
    drop(held);

    let resume_lock = session_transition_lock(&shared, channel_id);
    let resume_guard = resume_lock.lock().await;
    let busy_transition = crate::services::discord::try_intake_runtime_transition_after_redirect(
        &shared,
        channel_id,
        (None, false, old_cwd.clone()),
    )
    .await;
    assert!(
        busy_transition.is_err(),
        "production intake must defer immediately while resume holds the lock"
    );
    drop(resume_guard);

    let outcome = resume.await.unwrap().expect("production resume succeeds");
    assert!(outcome.in_memory_rebound);
    #[cfg(unix)]
    {
        let (lock_released, same_owner) =
            tokio::time::timeout(std::time::Duration::from_secs(5), wakes.recv())
                .await
                .expect("production resume completion wakes the waiting owner")
                .expect("the wake probe is installed");
        assert!(
            lock_released,
            "the wake follows the transition lock release"
        );
        assert!(same_owner, "completion wakes the same mailbox actor");
        let front = owner
            .v2_request(|slot| {
                let front = slot.as_ref().and_then(|owner| owner.front());
                *slot = None;
                Ok(front.map(|(_, item)| item.message_id))
            })
            .await
            .unwrap();
        assert_eq!(front, Some(2), "the same waiting input survives resume");
        assert!(wakes.try_recv().is_err(), "one owner wake per resume");
        drop(wake_probe);
    }
    let transition = crate::services::discord::try_intake_runtime_transition_after_redirect(
        &shared,
        channel_id,
        (None, false, old_cwd.clone()),
    )
    .await
    .expect("production intake acquires the released transition lock");
    assert_eq!(transition.state.0.as_deref(), Some(target_session_id));
    assert_eq!(transition.state.2, target_cwd);
    let blocked_resume_pool = pool.clone();
    let blocked_resume_registry = HealthRegistry::new();
    blocked_resume_registry
        .register("claude".to_string(), Arc::clone(&shared))
        .await;
    let blocked_target_cwd = target_cwd.clone();
    let blocked_resume = tokio::spawn(async move {
        perform_resume_rebind(
            &blocked_resume_pool,
            Some(&blocked_resume_registry),
            &session_key,
            Some(ProviderKind::Claude),
            Some(channel_id),
            tmux,
            &ResumePreviousOptions {
                session_id: Some(target_session_id.to_string()),
                cwd: Some(blocked_target_cwd),
            },
        )
        .await
    });
    let (claim_started_tx, claim_started_rx) = tokio::sync::oneshot::channel();
    let (claim_release_tx, claim_release_rx) = tokio::sync::oneshot::channel();
    let claim = tokio::spawn(async move {
        transition
            .complete_mailbox_claim(async move {
                let _ = claim_started_tx.send(());
                let _ = claim_release_rx.await;
            })
            .await;
    });
    claim_started_rx
        .await
        .expect("intake reaches the production claim span");
    tokio::task::yield_now().await;
    assert!(
        !blocked_resume.is_finished(),
        "resume must remain excluded until the intake claim completes"
    );
    let _ = claim_release_tx.send(());
    claim.await.expect("claim span completes");
    blocked_resume
        .await
        .expect("blocked resume task joins")
        .expect("blocked resume succeeds after the claim");

    pool.close().await;
    pg_db.drop().await;
}

/// The row's resume columns and its hosted-record lookup, for before/after comparison.
async fn durable_row(pool: &PgPool, key: &str) -> (Option<String>, Option<String>, String) {
    use crate::db::dispatched_sessions::hosted_execution as hosted;
    let context = crate::db::dispatched_sessions::load_session_rebind_context_pg(pool, key);
    let row = context.await.expect("load").expect("session row");
    let lookup = hosted::load_hosted_execution_pg(pool, hosted::HostedLookupKey::SessionKey(key));
    (
        row.cwd,
        row.claude_session_id,
        format!("{:?}", lookup.await),
    )
}

// A resume of a session the host guard keeps changes nothing: no durable rebind,
// teardown, tmux call, binding clear or in-memory rebind. A legacy row keeps main.
#[cfg(unix)]
#[test]
fn resume_refuses_what_the_host_guard_keeps_before_any_change_pg() {
    use crate::services::discord::host_defer_gate::tests::{Case, ScriptedTmux};
    use crate::services::discord::host_teardown_gate::test_support::{channel_key, runtime};
    let _root = crate::config::TestRuntimeRootGuard::new();
    crate::services::tui_prompt_dedupe::run_serial_test(async {
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let tmux = ScriptedTmux::install();
        let (db, pool) = crate::services::discord::host_defer_gate::tests::postgres().await;
        let (shared, registry) = runtime(&pool).await;
        let target_cwd = tempfile::tempdir().expect("target cwd");
        let opts = ResumePreviousOptions {
            session_id: Some("99999999-9999-9999-9999-999999999999".to_string()),
            cwd: Some(target_cwd.path().to_str().expect("utf8").to_string()),
        };
        let channel_of = |n: usize| ChannelId::new(1_479_671_301_387_066_000 + n as u64);
        let seeded = |n: usize, case: Case| {
            let (pool, shared) = (pool.clone(), shared.clone());
            async move {
                let name = format!("AgentDesk-claude-p4c1-resume-{n}");
                let key = channel_key(&shared, &name);
                case.seed(&pool, &key, &name, channel_of(n).get()).await;
                crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
                    &name,
                    runtime_binding("old-sid"),
                );
                (name, key)
            }
        };
        let kept = Case::ALL
            .into_iter()
            .filter(|c| c.has_row() && !c.admitted());
        for (n, case) in kept.enumerate() {
            let (name, key) = seeded(n, case).await;
            let before = durable_row(&pool, &key).await;
            let claude = Some(ProviderKind::Claude);
            let resumed = perform_resume_rebind(
                &pool,
                Some(&registry),
                &key,
                claude,
                Some(channel_of(n)),
                &name,
                &opts,
            )
            .await;
            assert!(
                matches!(resumed, Err(ResumeRebindError::HostUnsupported(_))),
                "{case:?}: {resumed:?}"
            );
            assert_eq!(
                durable_row(&pool, &key).await,
                before,
                "{case:?}: durable row"
            );
            assert_eq!(
                tmux.take_calls(),
                Vec::<String>::new(),
                "{case:?}: tmux untouched"
            );
            let bound = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&name);
            assert!(bound.is_some(), "{case:?}: runtime binding kept");
            let mirrored =
                crate::services::discord::resume_launch_state_for_tests(&shared, channel_of(n))
                    .await;
            assert_eq!(mirrored, None, "{case:?}: no in-memory rebind");
        }

        let hosted = Case::Stored(
            crate::services::discord::host_teardown_gate::test_support::Stored::Hosted,
        );
        let (name, key) = seeded(21, hosted).await;
        let before = durable_row(&pool, &key).await;
        let refused = perform_resume_rebind(
            &pool,
            None,
            &key,
            Some(ProviderKind::Claude),
            None,
            &name,
            &opts,
        )
        .await;
        assert!(
            matches!(
                &refused,
                Err(ResumeRebindError::HostUnsupported(reason))
                    if reason == "the session is not a legacy tmux session"
            ),
            "{refused:?}"
        );
        assert_eq!(durable_row(&pool, &key).await, before);
        assert!(tmux.take_calls().is_empty());

        let legacy = Case::Stored(
            crate::services::discord::host_teardown_gate::test_support::Stored::Legacy,
        );
        let (name, key) = seeded(20, legacy).await;
        let before = durable_row(&pool, &key).await;
        let resumed = perform_resume_rebind(
            &pool,
            Some(&registry),
            &key,
            None,
            Some(channel_of(20)),
            &name,
            &opts,
        )
        .await;
        assert!(
            matches!(resumed, Err(ResumeRebindError::HostUnsupported(_))),
            "{resumed:?}"
        );
        assert_eq!(
            durable_row(&pool, &key).await,
            before,
            "unknown provider: no rebind"
        );

        let _probe = crate::services::tmux_diagnostics::PaneLivenessOverrideGuard::set(
            &name,
            crate::services::platform::tmux::PaneLiveness::ProbeError,
        );
        let claude = Some(ProviderKind::Claude);
        let outcome = perform_resume_rebind(
            &pool,
            Some(&registry),
            &key,
            claude,
            Some(channel_of(20)),
            &name,
            &opts,
        )
        .await
        .expect("a legacy row resumes");
        assert_eq!(
            outcome.runtime_binding_clear,
            ResumeRuntimeBindingClearOutcome::RetainedLive {
                tmux_session: name.clone()
            },
            "a failed pane probe is not death"
        );
        let bound = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&name);
        assert!(bound.is_some(), "the live binding survives an unknown pane");
        let durable = durable_row(&pool, &key).await;
        assert_eq!(
            durable.1.as_deref(),
            Some("99999999-9999-9999-9999-999999999999")
        );
        assert_eq!(durable.2, before.2, "the hosted record is untouched");
        pool.close().await;
        db.drop().await;
    });
}
