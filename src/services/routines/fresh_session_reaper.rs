//! (#3877) Reaper-backstop support for `fresh` routine sessions.
//!
//! A `fresh` routine run owns a DISTINCT tmux session — the `routine <name> -
//! <agent>` label `start_turn` passes as the tmux session label (#3463). When
//! completion teardown misses it (e.g. a thread-less migrated-launchd run that
//! has no routine log thread), the session lingers as a dead-pane orphan with
//! no channel mapping. The periodic tmux reaper otherwise skips such orphans
//! ("handled by cleanup_orphan_tmux_sessions", which only runs at boot), so the
//! dead pane survives until the next dcserver restart.
//!
//! These helpers let the reaper identify and collect those orphans within one
//! reap cycle, while NEVER targeting a `persistent` routine, a DM-bound fresh
//! session (named `dm-<user>`, not the routine label), or a running turn.

use anyhow::{Result, anyhow};
use sqlx::PgPool;

use crate::services::platform::tmux::PaneLiveness;
use crate::services::provider::ProviderKind;

use super::agent_executor::routine_agent_session_name;
use super::store::RoutineRecord;

/// A `fresh` routine whose DISTINCT tmux session the periodic tmux reaper may
/// collect as a dead-pane orphan backstop when it escaped completion teardown
/// and has no channel mapping. `tmux_session` is the deterministic provider
/// session name the run created; `routine` carries the bindings the
/// positive-ownership teardown needs.
#[derive(Debug, Clone)]
pub struct ReapableFreshRoutineSession {
    pub routine: RoutineRecord,
    pub tmux_session: String,
}

/// Lists the deterministic tmux sessions owned by `fresh` routines that
/// currently have NO in-flight run, for `provider`. The periodic tmux reaper
/// uses this as a backstop to collect a completed fresh routine's orphan
/// without a dcserver restart.
///
/// Safety / scoping:
/// - Only `execution_strategy = 'fresh'` rows — `persistent` routine sessions
///   are never returned, so they survive.
/// - `in_flight_run_id IS NULL` — a routine with a LIVE turn is excluded, so the
///   reaper can never tear down a running session. (The reaper also gates on a
///   dead pane + no channel mapping, so this is defence-in-depth.)
/// - DM-bound fresh actions are naturally excluded: they create `dm-<user_id>`
///   sessions, never the `routine <name> - <agent>` label this derives, so an
///   awaiting-reply DM session can never match.
/// - Names are derived for both the primary and fallback agent ids so a
///   fallback-agent run's leaked session is still matchable.
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

/// Loads `fresh` routines with no in-flight run and a bound agent — the rows
/// whose owned tmux session the reaper backstop may collect.
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

/// The deterministic tmux session a non-DM `fresh` routine run owns, derived
/// from the `routine <name> - <agent>` label `start_turn` passes as the tmux
/// session label (#3463). This is the EXACT name the router builds at turn-start
/// (`build_tmux_session_name(label)`), so completion teardown and the reaper
/// backstop target the run's own session and never the shared primary agent
/// session (whose name is built from the real channel, not the label).
pub(crate) fn fresh_routine_owned_tmux_session_name(
    routine: &RoutineRecord,
    agent_id: &str,
    provider: &ProviderKind,
) -> String {
    provider.build_tmux_session_name(&routine_agent_session_name(&routine.name, agent_id))
}

/// The deterministic tmux session names a `fresh` routine could own across its
/// primary and fallback agent ids. Empty for a `persistent` routine or a
/// routine with no bound agent, so the reaper backstop never derives a name for
/// a session it must preserve.
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

/// (#3877 TOCTOU) Re-reads a single routine's CURRENT DB row by id, mirroring the
/// column selection of `load_reapable_fresh_routines` (the snapshot query) but
/// WITHOUT its reapability predicate — so the caller observes the row's live
/// `in_flight_run_id` / `execution_strategy` / `agent_id` even after a re-claim.
///
/// The periodic tmux reaper calls this immediately before killing a session the
/// snapshot matched: between the snapshot and the kill, a new claim can set
/// `in_flight_run_id` and re-launch a fresh pane under the SAME deterministic
/// tmux name. Re-reading here lets the reaper detect that re-trigger and skip the
/// kill instead of tearing down a live routine. Returns `None` when the row is
/// gone (the routine was deleted since the snapshot — also a skip).
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

/// (#3877) Pure predicate matching, in Rust, the exact reapability filter
/// `load_reapable_fresh_routines` applies in SQL: `execution_strategy = 'fresh'
/// AND in_flight_run_id IS NULL AND agent_id IS NOT NULL`. Kept identical to that
/// WHERE clause so the kill-time re-check enforces the same condition the
/// snapshot did — a routine re-claimed since the snapshot (non-null
/// `in_flight_run_id`) is no longer a reapable orphan and must be preserved.
pub(crate) fn routine_is_reapable_fresh_orphan(routine: &RoutineRecord) -> bool {
    routine.execution_strategy == "fresh"
        && routine.in_flight_run_id.is_none()
        && routine.agent_id.is_some()
}

/// (#3877 TOCTOU) The re-validation decision the reaper backstop makes RIGHT
/// BEFORE killing a matched fresh-routine orphan, closing the window between the
/// snapshot and the kill.
///
/// Between the snapshot (`reapable_fresh_routine_sessions`) and the kill, a new
/// claim can set `in_flight_run_id` and re-launch a fresh pane under the SAME
/// deterministic tmux name. Killing then would tear down a just-re-triggered LIVE
/// routine — the one thing the reaper must never do.
///
/// Returns `Ok(())` (proceed to kill) ONLY when BOTH re-checks still indicate a
/// genuine completed orphan: the re-read row still satisfies
/// [`routine_is_reapable_fresh_orphan`] AND the pane is definitively dead.
/// Otherwise returns `Err(reason)` describing why the kill is SKIPPED:
/// - the row is gone, no longer `fresh`, or lost its agent binding;
/// - `in_flight_run_id` is now set (re-triggered since the snapshot);
/// - the pane is live again (recreated), or the probe failed (unknown ⇒ preserve).
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
        // Both the primary and fallback agent's owned sessions are reapable.
        assert_eq!(names.len(), 2);
        assert!(names.contains(&fresh_routine_owned_tmux_session_name(
            &fresh, "agent-a", &provider
        )));
        assert!(names.contains(&fresh_routine_owned_tmux_session_name(
            &fresh, "agent-b", &provider
        )));

        // A persistent routine is never reapable (its session must survive).
        let mut persistent = fresh_routine_named("always-on", Some("agent-a"), None);
        persistent.execution_strategy = "persistent".to_string();
        assert!(fresh_routine_reapable_tmux_names(&persistent, &provider).is_empty());

        // A fresh routine with no bound agent derives nothing.
        let agentless = fresh_routine_named("orphan", None, None);
        assert!(fresh_routine_reapable_tmux_names(&agentless, &provider).is_empty());
    }

    // #3877: the periodic reaper backstop reaps a completed fresh routine's
    // orphan (no channel mapping) while preserving persistent, DM-bound fresh,
    // and unrelated work sessions. Mirrors the reaper's HashMap lookup built
    // from `reapable_fresh_routine_sessions`.
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

        // The completed fresh routine's dead-pane orphan IS reaped.
        let fresh_orphan = fresh_routine_owned_tmux_session_name(&fresh, "agent-a", &provider);
        assert!(reapable.contains_key(&fresh_orphan));

        // The persistent routine's session is preserved.
        let persistent_session =
            provider.build_tmux_session_name(&routine_agent_session_name("always-on", "agent-a"));
        assert!(!reapable.contains_key(&persistent_session));

        // A DM-bound fresh session (`dm-<user>`) never matches — DM actions never
        // use the `routine <name> - <agent>` label.
        let dm_session = provider.build_tmux_session_name("dm-123456789");
        assert!(!reapable.contains_key(&dm_session));

        // An unrelated work session is untouched.
        let work_session = provider.build_tmux_session_name("general");
        assert!(!reapable.contains_key(&work_session));
    }

    // #3877 (ii): the reapability predicate the snapshot SQL applies excludes a
    // routine whose `in_flight_run_id IS NOT NULL` (a claimed/running turn) and
    // a `persistent` / agent-less routine, while admitting a completed fresh
    // orphan. `routine_is_reapable_fresh_orphan` is kept identical to the SQL
    // WHERE clause in `load_reapable_fresh_routines`, so this guards both.
    #[test]
    fn reapable_predicate_excludes_in_flight_and_non_fresh_rows() {
        // Completed fresh orphan with a bound agent and no in-flight run: reapable.
        let orphan = fresh_routine_named("memento-hygiene", Some("agent-a"), None);
        assert!(routine_is_reapable_fresh_orphan(&orphan));

        // Same routine but with an in-flight run (re-claimed): NOT reapable — the
        // SQL snapshot excludes `in_flight_run_id IS NOT NULL` rows.
        let mut in_flight = orphan.clone();
        in_flight.in_flight_run_id = Some("run-123".to_string());
        assert!(!routine_is_reapable_fresh_orphan(&in_flight));

        // A persistent routine is never reapable (its session must survive).
        let mut persistent = orphan.clone();
        persistent.execution_strategy = "persistent".to_string();
        assert!(!routine_is_reapable_fresh_orphan(&persistent));

        // A fresh routine with no bound agent is excluded (`agent_id IS NOT NULL`).
        let agentless = fresh_routine_named("orphan", None, None);
        assert!(!routine_is_reapable_fresh_orphan(&agentless));
    }

    // #3877 (i): the TOCTOU re-validation right before the kill. A routine the
    // snapshot saw as a completed fresh orphan, but whose `in_flight_run_id`
    // became NON-NULL before the kill (re-triggered), is NOT killed.
    #[test]
    fn revalidate_skips_kill_when_routine_retriggered_after_snapshot() {
        // Re-claimed before the kill: in_flight_run_id now set. Even with a dead
        // pane reading, the kill must be SKIPPED.
        let mut retriggered = fresh_routine_named("token-daily-report", Some("agent-a"), None);
        retriggered.in_flight_run_id = Some("run-789".to_string());
        let skip =
            revalidate_fresh_orphan_before_kill(Some(&retriggered), PaneLiveness::DeadOrAbsent);
        assert_eq!(
            skip,
            Err("routine re-triggered (in_flight_run_id set) since snapshot")
        );
    }

    // #3877 (i): the re-validation honours the second re-check — pane liveness.
    // If the deterministic-named session has a LIVE pane again (a re-claim
    // recreated it), or the probe is inconclusive, the kill is SKIPPED. Only a
    // still-reapable row with a definitively dead pane proceeds to the kill.
    #[test]
    fn revalidate_proceeds_only_for_dead_pane_genuine_orphan() {
        let orphan = fresh_routine_named("agent-feedback-briefing", Some("agent-a"), None);

        // Genuine completed orphan, pane still dead: proceed to kill.
        assert_eq!(
            revalidate_fresh_orphan_before_kill(Some(&orphan), PaneLiveness::DeadOrAbsent),
            Ok(())
        );

        // Pane recreated (live again) since the snapshot: preserve.
        assert_eq!(
            revalidate_fresh_orphan_before_kill(Some(&orphan), PaneLiveness::Live),
            Err("tmux pane is live again (session recreated since snapshot)")
        );

        // tmux probe failed: unknown ⇒ preserve, never kill on a transient hiccup.
        assert_eq!(
            revalidate_fresh_orphan_before_kill(Some(&orphan), PaneLiveness::ProbeError),
            Err("tmux pane liveness probe failed (unknown — preserving)")
        );

        // Row deleted since the snapshot: nothing to (and must not) kill.
        assert_eq!(
            revalidate_fresh_orphan_before_kill(None, PaneLiveness::DeadOrAbsent),
            Err("routine row gone since snapshot")
        );

        // Flipped to persistent since the snapshot: preserve.
        let mut persistent = orphan.clone();
        persistent.execution_strategy = "persistent".to_string();
        assert_eq!(
            revalidate_fresh_orphan_before_kill(Some(&persistent), PaneLiveness::DeadOrAbsent),
            Err("routine no longer execution_strategy=fresh")
        );
    }
}
