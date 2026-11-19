//! Match a PATCH against the Codex conversation that can retain its context.
use anyhow::{Result, anyhow};
use sqlx::PgPool;

use super::{ProviderChoice, ProviderRoute};
use crate::domain::session_identity::SessionIdentity;
use crate::services::discord::org_schema::launch_account_is_named;
use crate::services::provider_auth_profile::DEFAULT_PROFILE_ID;
use crate::services::routines::agent_executor::routine_agent_session_name;
use crate::services::routines::store::RoutineRecord;
use crate::services::tmux_common::tmux_session_auth_profile;

pub(crate) struct PersistentModelPatch<'a> {
    pub routine: &'a RoutineRecord,
    pub previous: &'a ProviderChoice,
    pub thread: Option<&'a str>,
}

#[derive(sqlx::FromRow)]
struct Execution {
    channel: String,
    model: Option<String>,
    session_key: Option<String>,
    native: Option<String>,
    previous_native: Option<String>,
    is_dm: Option<bool>,
    model_matches_selector: bool,
}

pub(super) async fn refuses(
    pool: &PgPool,
    patch: &PersistentModelPatch<'_>,
    agent: &str,
    route: &ProviderRoute,
) -> Result<bool> {
    let executions = persistent_executions(pool, &patch.routine.id, agent).await?;
    let label = routine_agent_session_name(&patch.routine.name, agent);
    let labelled_pane = route.provider.build_tmux_session_name(&label);
    let local = crate::services::platform::hostname_short();
    let mut resumable = false;
    for execution in &executions {
        let channel = execution.channel.parse::<u64>().ok();
        let locator = execution
            .session_key
            .as_deref()
            .and_then(SessionIdentity::parse);
        let dm = execution.is_dm.unwrap_or_else(|| {
            locator
                .as_ref()
                .and_then(SessionIdentity::provider_and_channel)
                .and_then(|(_, segment)| segment.strip_prefix("dm-")?.parse::<u64>().ok())
                .is_some()
        });
        let pane = match locator.as_ref() {
            Some(key) if key.host != local => None,
            Some(key) if dm => Some(key.tmux_name.as_str()),
            None if dm => None,
            _ => Some(labelled_pane.as_str()),
        };
        let live_native = execution
            .native
            .is_none()
            .then(|| observed_native(pane?, channel?))
            .flatten();
        let Some(native) = execution.native.as_deref().or(live_native.as_deref()) else {
            continue;
        };
        let model_proof = pane.and_then(|tmux| {
            crate::services::tui_input::routine_model::requested_model_for_native(
                "codex", tmux, native,
            )
        });
        let pinned_model = match model_proof {
            Some(model) => model,
            None if execution.previous_native.as_deref() == Some(native)
                || execution.previous_native.is_none() && execution.model_matches_selector =>
            {
                execution.model.clone()
            }
            None => continue,
        };
        resumable = true;
        if route.model != pinned_model && named(route, channel, agent, pane, Some(native))? {
            return Ok(true);
        }
    }
    if resumable {
        return Ok(false);
    }

    let channel = executions
        .first()
        .map(|execution| execution.channel.as_str())
        .or(patch.thread)
        .and_then(|value| value.parse().ok());
    let claim = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT run_spec FROM routine_runs WHERE id = $1 AND status = 'running'
          AND run_spec->>'execution_strategy' = 'persistent'
          AND (run_spec->>'provider' = 'codex' OR run_spec->>'provider' IS NULL)
          AND ($2 = run_spec->>'agent_id' OR $2 = run_spec->>'fallback_agent_id')",
    )
    .bind(&patch.routine.in_flight_run_id)
    .bind(agent)
    .fetch_optional(pool)
    .await?;
    let previous = claim
        .as_ref()
        .map_or(patch.previous.model.as_deref(), |spec| {
            spec["model"].as_str()
        });
    Ok(route.model.as_deref() != previous
        && named(
            route,
            channel,
            agent,
            executions.is_empty().then_some(labelled_pane.as_str()),
            None,
        )?)
}

fn observed_native(pane: &str, channel: u64) -> Option<String> {
    use crate::services::tui_prompt_dedupe::binding_context::{
        SpawnNonceMarker, execution_context, observe_spawn_nonce_marker,
    };
    let SpawnNonceMarker::Known(nonce) = observe_spawn_nonce_marker(pane) else {
        return None;
    };
    let context = execution_context("codex", &nonce).ok()?;
    if context.tmux_session != pane || context.channel_id != Some(channel) {
        return None;
    }
    let bound = crate::services::tui_prompt_dedupe::peek_tmux_runtime_binding(pane)?.session_id?;
    context
        .expected_native_session_id
        .as_ref()
        .is_none_or(|id| id == &bound)
        .then_some(bound)
}

fn named(
    route: &ProviderRoute,
    channel: Option<u64>,
    agent: &str,
    pane: Option<&str>,
    native: Option<&str>,
) -> Result<bool> {
    if launch_account_is_named(&route.provider, channel, agent)
        .map_err(|_| anyhow!("could not determine the Codex account for this routine execution"))?
    {
        return Ok(true);
    }
    let Some(pane) = pane else {
        return Ok(false);
    };
    #[cfg(unix)]
    if let Some(native) = native
        && let Some(profile) = crate::services::tmux_common::claude_session_auth(pane, native)
            .map_err(|_| anyhow!("could not read the Codex account for this routine execution"))?
    {
        let profile = profile.trim();
        return Ok(!profile.is_empty() && profile != DEFAULT_PROFILE_ID);
    }
    // A legacy wrapper can have only its launch-account marker.
    if let Some(native) = native
        && crate::services::tui_prompt_dedupe::peek_tmux_runtime_binding(pane)
            .and_then(|binding| binding.session_id)
            .is_some_and(|bound| bound != native)
    {
        return Ok(false);
    }
    let Some(profile) = tmux_session_auth_profile(pane)
        .filter(|profile| !profile.is_empty() && profile != DEFAULT_PROFILE_ID)
    else {
        return Ok(false);
    };
    #[cfg(unix)]
    if let Some(native) = native {
        return crate::services::tmux_common::claude_session_auth_matches(pane, native, &profile)
            .map_err(|_| anyhow!("could not read the Codex account for this routine execution"));
    }
    let _ = profile;
    Ok(true)
}

async fn persistent_executions(pool: &PgPool, id: &str, agent: &str) -> Result<Vec<Execution>> {
    Ok(sqlx::query_as(
        r#"
        WITH routes AS (
            SELECT DISTINCT ON (attempt->>'channel_id')
                   attempt->>'channel_id' AS channel, rr.run_spec->>'model' AS model,
                   COALESCE(rr.finished_at, NOW()) AS last_seen, rr.started_at,
                   attempt->>'turn_id' AS turn_id,
                   CASE
                       WHEN jsonb_typeof(attempt->'is_dm') = 'boolean' THEN (attempt->>'is_dm')::boolean
                       WHEN rr.result_json->>'turn_id' = attempt->>'turn_id'
                            AND jsonb_typeof(rr.result_json->'is_dm') = 'boolean'
                            THEN (rr.result_json->>'is_dm')::boolean
                   END AS is_dm
            FROM routine_runs rr
            CROSS JOIN LATERAL jsonb_array_elements(rr.attempts) WITH ORDINALITY AS a(attempt, ord)
            WHERE rr.routine_id = $1
              AND (rr.run_spec IS NULL OR rr.run_spec->>'execution_strategy' = 'persistent')
              AND rr.result_json->>'fresh_context_guaranteed' IS DISTINCT FROM 'true'
              AND attempt->>'event' = 'started'
              AND attempt->>'agent_id' = $2 AND attempt->>'provider' = 'codex'
              AND attempt->>'channel_id' IS NOT NULL
            ORDER BY attempt->>'channel_id', rr.started_at DESC, rr.id DESC, ord DESC
        )
        SELECT routes.channel, routes.model, retained.session_key, retained.native,
               t.session_id AS previous_native, routes.is_dm,
               COALESCE(retained.model_matches_selector
                   AND (retained.candidates = 1 OR t.session_key = retained.session_key),
                   false) AS model_matches_selector
        FROM routes
        LEFT JOIN LATERAL (
            SELECT session_id, session_key FROM turns
            WHERE turn_id = routes.turn_id AND provider = 'codex' AND channel_id = routes.channel
            UNION ALL
            SELECT session_id, session_key FROM turns_archive
            WHERE turn_id = routes.turn_id AND provider = 'codex' AND channel_id = routes.channel
            LIMIT 1
        ) t ON TRUE
        LEFT JOIN LATERAL (
            SELECT s.session_key, selector.native, COUNT(*) OVER () AS candidates,
                   s.created_at <= routes.last_seen
                       AND COALESCE(s.claude_session_id_recorded_at, s.created_at) <= routes.last_seen
                       AND (t.session_key IS NULL OR t.session_key = s.session_key)
                       AS model_matches_selector
            FROM sessions s
            CROSS JOIN LATERAL (
                SELECT COALESCE(NULLIF(btrim(s.claude_session_id), ''),
                                NULLIF(btrim(s.raw_provider_session_id), '')) AS native
            ) selector
            WHERE (s.agent_id = $2 OR s.agent_id IS NULL AND t.session_key = s.session_key)
              AND s.provider = 'codex' AND s.closed_at IS NULL
              AND (s.channel_id = routes.channel
                   OR s.channel_id IS NULL AND s.thread_channel_id = routes.channel)
        ) retained ON TRUE
        ORDER BY routes.started_at DESC, routes.channel, retained.session_key
        "#,
    )
    .bind(id)
    .bind(agent)
    .fetch_all(pool)
    .await?)
}
