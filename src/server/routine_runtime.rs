use super::{Arc, HealthRegistry, PgPool, routine_script_audit};

pub(super) async fn routine_runtime_loop(
    pg_pool: Arc<PgPool>,
    health_registry: Option<Arc<HealthRegistry>>,
    routines_config: crate::config::RoutinesConfig,
    tick_interval_secs: u64,
) {
    use crate::services::routines::{
        RoutineAction, RoutineAgentExecutor, RoutineDiscordLogger, RoutineStore,
        after_startup_grace, poll_agent_turns, run_due_tick,
    };
    let boot = tokio::time::Instant::now();
    let Some(tick_interval_secs) = std::num::NonZeroU64::new(tick_interval_secs) else {
        tracing::warn!("routine runtime not started: tick_interval_secs must be greater than zero");
        return;
    };

    let _routine_action_validator: fn(serde_json::Value) -> anyhow::Result<RoutineAction> =
        crate::services::routines::validate_routine_action;

    let routine_script_dirs = routines_config.script_dirs();
    let initial_script_dirs = routine_script_dirs.clone();
    let script_loader = match tokio::task::spawn_blocking(move || {
        routine_script_audit::load_registry(&initial_script_dirs)
    })
    .await
    {
        Ok(Ok((loader, count))) => {
            tracing::info!(count, "routine script registry initialized");
            loader
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "routine runtime not started: script registry initialization failed");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "routine runtime not started: script registry blocking task failed");
            return;
        }
    };

    let store = RoutineStore::new_with_timezone_and_checkpoint_limit(
        pg_pool.clone(),
        routines_config.default_timezone.clone(),
        routines_config.max_checkpoint_bytes,
    );
    routine_script_audit::warn_once_unregistered(pg_pool.as_ref(), &routine_script_dirs).await;
    let discord_logger =
        RoutineDiscordLogger::new_with_health_registry(pg_pool.clone(), health_registry.clone());
    let agent_executor =
        RoutineAgentExecutor::new(pg_pool, health_registry, routines_config.agent_timeout_secs)
            .with_headless_fresh_agents(routines_config.headless_fresh_agents);
    match agent_executor.recover_stale_running_runs(&store).await {
        Ok(recovered) if !recovered.is_empty() => {
            for run in &recovered {
                // Reap owned orphaned fresh sessions before they look like abrupt exits.
                agent_executor
                    .teardown_recovered_fresh_session(&store, run)
                    .await;
                discord_logger.log_recovery(&store, run).await;
            }
            tracing::info!(
                recovered = recovered.len(),
                "routine boot recovery: stale runs marked interrupted"
            )
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "routine boot recovery failed"),
    }
    let mut interval =
        tokio::time::interval(std::time::Duration::from_secs(tick_interval_secs.get()));
    loop {
        interval.tick().await;
        // Per-tick tunables follow the live config; boot-bound values (script dirs, store
        // limits, agent timeout) keep their startup values in the objects built above.
        let routines_config = crate::config_live_reload::current()
            .map(|cfg| cfg.routines.clone())
            .unwrap_or_else(|| routines_config.clone());
        match agent_executor.recover_stale_running_runs(&store).await {
            Ok(recovered) if !recovered.is_empty() => {
                for run in &recovered {
                    // No fresh-session reap here: a concurrent re-claim can reuse the same tmux
                    // name, so the reap would race a live turn. Idle-kill collects any orphan.
                    discord_logger.log_recovery(&store, run).await;
                }
                tracing::info!(
                    recovered = recovered.len(),
                    "routine periodic recovery: expired-lease runs marked interrupted"
                )
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "routine periodic recovery failed"),
        }
        if routines_config.hot_reload {
            let reload_dirs = routine_script_dirs.clone();
            let loader = Arc::clone(&script_loader);
            match tokio::task::spawn_blocking(move || loader.load_dirs(&reload_dirs)).await {
                Ok(Ok(count)) if count > 0 => {
                    tracing::debug!(count, "routine script registry hot-reload pass complete")
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "routine script registry hot-reload failed")
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    "routine script registry hot-reload blocking task failed"
                ),
            }
        }
        // Paused routines are never claimed, so alert on ones paused past the threshold
        // instead of letting them silently never run again. 0 (the default) disables it.
        let stale_paused_alert_secs = routines_config.stale_paused_alert_secs;
        if stale_paused_alert_secs > 0 {
            let now = chrono::Utc::now();
            let cutoff = now - chrono::Duration::seconds(stale_paused_alert_secs as i64);
            match store.list_stale_paused_routines(cutoff).await {
                Ok(stale) => {
                    for routine in &stale {
                        let paused_for_secs = (now - routine.updated_at).num_seconds().max(0);
                        if paused_for_secs >= stale_paused_alert_secs as i64 {
                            discord_logger
                                .log_stale_paused(
                                    &store,
                                    routine,
                                    paused_for_secs,
                                    routines_config.stale_paused_alert_ttl_secs as i64,
                                )
                                .await;
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "routine stale-paused scan failed")
                }
            }
        }
        // Opt-in auto-resume, only for `pause_reason = 'failure'` routines; 0 (the default)
        // disables it, e.g. 3600 resumes after a 1-hour backoff.
        let auto_resume_secs = routines_config.failure_pause_auto_resume_secs;
        let pause_on_terminal_failure = routines_config.failure_pause_auto_resume_secs > 0;
        // Failure alerts are not tied to the pause knob: 0 there still alerts at this streak.
        let max_consecutive_failures = routines_config.max_consecutive_failures;
        if pause_on_terminal_failure {
            let now = chrono::Utc::now();
            let cutoff = now - chrono::Duration::seconds(auto_resume_secs as i64);
            match store.list_failure_paused_routines(cutoff).await {
                Ok(candidates) => {
                    for routine in &candidates {
                        match store
                            .auto_resume_failure_paused_routine(&routine.id, cutoff)
                            .await
                        {
                            Ok(true) => {
                                tracing::info!(
                                    routine_id = %routine.id,
                                    routine_name = %routine.name,
                                    "auto-resumed failure-paused routine"
                                );
                            }
                            Ok(false) => {
                                tracing::debug!(
                                    routine_id = %routine.id,
                                    "auto-resume: routine no longer eligible (already resumed or re-paused)"
                                );
                            }
                            Err(error) => {
                                tracing::warn!(
                                    routine_id = %routine.id,
                                    error = %error,
                                    "auto-resume failure-paused routine failed"
                                );
                            }
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "routine auto-resume scan failed")
                }
            }
        }
        match poll_agent_turns(
            &store,
            &agent_executor,
            routines_config.max_agent_polls_per_tick,
            pause_on_terminal_failure,
        )
        .await
        {
            Ok(outcomes) if !outcomes.is_empty() => {
                for outcome in &outcomes {
                    discord_logger.log_run_outcome(&store, outcome).await;
                    discord_logger
                        .alert_consecutive_failures(&store, outcome, max_consecutive_failures)
                        .await;
                }
                tracing::info!(count = outcomes.len(), "routine agent turns completed")
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "routine agent turn polling failed"),
        }
        let due_tick = run_due_tick(
            &store,
            &script_loader,
            &routine_script_dirs,
            Some(&agent_executor),
            Some(&discord_logger),
            routines_config.max_due_per_tick,
            pause_on_terminal_failure,
        );
        match after_startup_grace(boot.elapsed(), routines_config.startup_grace_secs, due_tick)
            .await
        {
            Ok(Some(outcomes)) if !outcomes.is_empty() => {
                for outcome in &outcomes {
                    discord_logger
                        .alert_consecutive_failures(&store, outcome, max_consecutive_failures)
                        .await;
                }
                tracing::info!(count = outcomes.len(), "routine due tick executed")
            }
            Ok(None) => tracing::debug!("routine due tick held by startup grace"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "routine due tick failed"),
        }
    }
}
