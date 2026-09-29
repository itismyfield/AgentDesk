//! Reaper backstop for `fresh` routine sessions. Each fresh run owns a distinct tmux
//! session; when completion teardown misses it, it lingers as a dead-pane orphan with no
//! channel mapping, which the periodic reaper otherwise leaves until the next restart.
//! These helpers let the reaper collect such orphans without ever targeting a
//! `persistent` routine, a DM-bound `dm-<user>` session, or a running turn.

use anyhow::{Result, anyhow};
use sqlx::PgPool;

use crate::services::platform::tmux::PaneLiveness;
use crate::services::provider::ProviderKind;

use super::agent_executor::routine_agent_session_name;
use super::store::RoutineRecord;

/// A `fresh` routine's own tmux session, a candidate for the reaper's dead-pane backstop.
#[derive(Debug, Clone)]
pub struct ReapableFreshRoutineSession {
    pub routine: RoutineRecord,
    pub tmux_session: String,
}

/// Lists the tmux sessions owned by `fresh` routines with no in-flight run, for
/// `provider`, under both the primary and fallback agent ids.
pub(crate) async fn reapable_fresh_routine_sessions(
    pool: &PgPool,
    provider: &ProviderKind,
) -> Result<Vec<ReapableFreshRoutineSession>> {
    let routines = load_reapable_fresh_routines(pool).await?;
    let mut out = Vec::new();
    for routine in routines {
        for tmux_session in fresh_routine_reapable_tmux_names(&routine, provider) {
            out.push(ReapableFreshRoutineSession {
                routine: routine.clone(),
                tmux_session,
            });
        }
    }
    Ok(out)
}

/// Loads `fresh` routines with a bound agent and no in-flight run.
async fn load_reapable_fresh_routines(pool: &PgPool) -> Result<Vec<RoutineRecord>> {
    sqlx::query_as(
        r#"
        SELECT id, agent_id, script_ref, name, status, execution_strategy,
               schedule, next_due_at, last_run_at, last_result, checkpoint,
               discord_thread_id, timeout_secs, fallback_agent_id, max_retries,
               in_flight_run_id, pause_reason,
               created_at, updated_at
        FROM routines
        WHERE execution_strategy = 'fresh'
          AND in_flight_run_id IS NULL
          AND agent_id IS NOT NULL
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("list reapable fresh routines: {error}"))
}

/// The tmux session a non-DM `fresh` run owns: the exact name the router builds from the
/// `routine <name> - <agent>` label, never the agent's shared channel session.
pub(crate) fn fresh_routine_owned_tmux_session_name(
    routine: &RoutineRecord,
    agent_id: &str,
    provider: &ProviderKind,
) -> String {
    provider.build_tmux_session_name(&routine_agent_session_name(&routine.name, agent_id))
}

/// Owned session names across the primary and fallback agent ids. Empty for a
/// non-`fresh` or agent-less routine, so the reaper never derives a name it must preserve.
pub(crate) fn fresh_routine_reapable_tmux_names(
    routine: &RoutineRecord,
    provider: &ProviderKind,
) -> Vec<String> {
    if routine.execution_strategy != "fresh" {
        return Vec::new();
    }
    let mut names = Vec::new();
    for agent_id in [
        routine.agent_id.as_deref(),
        routine.fallback_agent_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|agent_id| !agent_id.is_empty())
    {
        let name = fresh_routine_owned_tmux_session_name(routine, agent_id, provider);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Re-reads a routine's current row without the reapability filter, so the kill-time
/// re-check sees a re-claim. `None` means the row was deleted.
pub(crate) async fn reread_routine(
    pool: &PgPool,
    routine_id: &str,
) -> Result<Option<RoutineRecord>> {
    sqlx::query_as(
        r#"
        SELECT id, agent_id, script_ref, name, status, execution_strategy,
               schedule, next_due_at, last_run_at, last_result, checkpoint,
               discord_thread_id, timeout_secs, fallback_agent_id, max_retries,
               in_flight_run_id, pause_reason,
               created_at, updated_at
        FROM routines
        WHERE id = $1
        "#,
    )
    .bind(routine_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow!("re-read routine {routine_id}: {error}"))
}

/// Rust copy of the `load_reapable_fresh_routines` WHERE clause; keep the two identical
/// so the kill-time re-check enforces what the snapshot did.
pub(crate) fn routine_is_reapable_fresh_orphan(routine: &RoutineRecord) -> bool {
    routine.execution_strategy == "fresh"
        && routine.in_flight_run_id.is_none()
        && routine.agent_id.is_some()
}

/// Kill-time re-check: a claim after the snapshot can relaunch a pane under the same name.
/// `Ok(())` only if the re-read row is still a reapable orphan and the pane is `DeadOrAbsent`.
pub(crate) fn revalidate_fresh_orphan_before_kill(
    routine: Option<&RoutineRecord>,
    pane: PaneLiveness,
) -> Result<(), &'static str> {
    let Some(routine) = routine else {
        return Err("routine row gone since snapshot");
    };
    if routine.execution_strategy != "fresh" {
        return Err("routine no longer execution_strategy=fresh");
    }
    if routine.agent_id.is_none() {
        return Err("routine no longer has a bound agent");
    }
    if routine.in_flight_run_id.is_some() {
        return Err("routine re-triggered (in_flight_run_id set) since snapshot");
    }
    debug_assert!(routine_is_reapable_fresh_orphan(routine));
    match pane {
        PaneLiveness::DeadOrAbsent => Ok(()),
        PaneLiveness::Live => Err("tmux pane is live again (session recreated since snapshot)"),
        PaneLiveness::ProbeError => Err("tmux pane liveness probe failed (unknown — preserving)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;

    fn fresh_routine_named(
        name: &str,
        agent_id: Option<&str>,
        fallback_agent_id: Option<&str>,
    ) -> RoutineRecord {
        RoutineRecord {
            id: format!("routine-{name}"),
            agent_id: agent_id.map(ToOwned::to_owned),
            fallback_agent_id: fallback_agent_id.map(ToOwned::to_owned),
            max_retries: 0,
            script_ref: "script".to_string(),
            name: name.to_string(),
            status: "enabled".to_string(),
            execution_strategy: "fresh".to_string(),
            schedule: None,
            next_due_at: None,
            last_run_at: None,
            last_result: None,
            checkpoint: None,
            discord_thread_id: None,
            timeout_secs: None,
            in_flight_run_id: None,
            pause_reason: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn reapable_names_cover_fresh_with_fallback_and_exclude_persistent() {
        let provider = ProviderKind::Claude;
        let fresh = fresh_routine_named("memento-hygiene", Some("agent-a"), Some("agent-b"));
        let names = fresh_routine_reapable_tmux_names(&fresh, &provider);
        assert_eq!(names.len(), 2);
        assert!(names.contains(&fresh_routine_owned_tmux_session_name(
            &fresh, "agent-a", &provider
        )));
        assert!(names.contains(&fresh_routine_owned_tmux_session_name(
            &fresh, "agent-b", &provider
        )));

        let mut persistent = fresh_routine_named("always-on", Some("agent-a"), None);
        persistent.execution_strategy = "persistent".to_string();
        assert!(fresh_routine_reapable_tmux_names(&persistent, &provider).is_empty());

        let agentless = fresh_routine_named("orphan", None, None);
        assert!(fresh_routine_reapable_tmux_names(&agentless, &provider).is_empty());
    }

    // Mirrors the reaper's lookup map built from `reapable_fresh_routine_sessions`.
    #[test]
    fn reaper_backstop_matches_only_completed_fresh_orphan() {
        let provider = ProviderKind::Claude;
        let fresh = fresh_routine_named("dependency-update-watcher", Some("agent-a"), None);
        let mut persistent = fresh_routine_named("always-on", Some("agent-a"), None);
        persistent.execution_strategy = "persistent".to_string();

        let mut reapable: HashMap<String, RoutineRecord> = HashMap::new();
        for routine in [&fresh, &persistent] {
            for name in fresh_routine_reapable_tmux_names(routine, &provider) {
                reapable.insert(name, routine.clone());
            }
        }

        let fresh_orphan = fresh_routine_owned_tmux_session_name(&fresh, "agent-a", &provider);
        assert!(reapable.contains_key(&fresh_orphan));

        let persistent_session =
            provider.build_tmux_session_name(&routine_agent_session_name("always-on", "agent-a"));
        assert!(!reapable.contains_key(&persistent_session));

        // DM-bound fresh sessions are named `dm-<user>`, never the routine label.
        let dm_session = provider.build_tmux_session_name("dm-123456789");
        assert!(!reapable.contains_key(&dm_session));

        let work_session = provider.build_tmux_session_name("general");
        assert!(!reapable.contains_key(&work_session));
    }

    #[test]
    fn reapable_predicate_excludes_in_flight_and_non_fresh_rows() {
        let orphan = fresh_routine_named("memento-hygiene", Some("agent-a"), None);
        assert!(routine_is_reapable_fresh_orphan(&orphan));

        let mut in_flight = orphan.clone();
        in_flight.in_flight_run_id = Some("run-123".to_string());
        assert!(!routine_is_reapable_fresh_orphan(&in_flight));

        let mut persistent = orphan.clone();
        persistent.execution_strategy = "persistent".to_string();
        assert!(!routine_is_reapable_fresh_orphan(&persistent));

        let agentless = fresh_routine_named("orphan", None, None);
        assert!(!routine_is_reapable_fresh_orphan(&agentless));
    }

    #[test]
    fn revalidate_skips_kill_when_routine_retriggered_after_snapshot() {
        // A dead pane does not override the re-claim.
        let mut retriggered = fresh_routine_named("token-daily-report", Some("agent-a"), None);
        retriggered.in_flight_run_id = Some("run-789".to_string());
        let skip =
            revalidate_fresh_orphan_before_kill(Some(&retriggered), PaneLiveness::DeadOrAbsent);
        assert_eq!(
            skip,
            Err("routine re-triggered (in_flight_run_id set) since snapshot")
        );
    }

    #[test]
    fn revalidate_proceeds_only_for_dead_pane_genuine_orphan() {
        let orphan = fresh_routine_named("completed-fresh-orphan", Some("agent-a"), None);

        assert_eq!(
            revalidate_fresh_orphan_before_kill(Some(&orphan), PaneLiveness::DeadOrAbsent),
            Ok(())
        );

        assert_eq!(
            revalidate_fresh_orphan_before_kill(Some(&orphan), PaneLiveness::Live),
            Err("tmux pane is live again (session recreated since snapshot)")
        );

        assert_eq!(
            revalidate_fresh_orphan_before_kill(Some(&orphan), PaneLiveness::ProbeError),
            Err("tmux pane liveness probe failed (unknown — preserving)")
        );

        assert_eq!(
            revalidate_fresh_orphan_before_kill(None, PaneLiveness::DeadOrAbsent),
            Err("routine row gone since snapshot")
        );

        let mut persistent = orphan.clone();
        persistent.execution_strategy = "persistent".to_string();
        assert_eq!(
            revalidate_fresh_orphan_before_kill(Some(&persistent), PaneLiveness::DeadOrAbsent),
            Err("routine no longer execution_strategy=fresh")
        );
    }
}
