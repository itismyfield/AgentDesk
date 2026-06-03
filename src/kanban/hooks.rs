//! Hook firing and side-effect draining for kanban transitions.

use super::audit::log_transition_audit_pg;
use super::github_sync::github_sync_on_transition_pg;
use super::review_tuning::record_true_negative_if_pass_with_backends;
use super::terminal_cleanup::sync_terminal_transition_followups_pg;
use crate::db::Db;
use crate::engine::PolicyEngine;
use serde_json::json;
use sqlx::Row as SqlxRow;

/// Fire hooks dynamically based on the effective pipeline's hooks section (#106 P5).
///
/// All hook bindings come from the YAML pipeline definition.
/// States without hook bindings simply fire no hooks.
pub(super) fn fire_dynamic_hooks(
    engine: &PolicyEngine,
    pipeline: &crate::pipeline::PipelineConfig,
    card_id: &str,
    old_status: &str,
    new_status: &str,
    source: Option<&str>,
) {
    let mut payload = json!({
        "card_id": card_id,
        "from": old_status,
        "to": new_status,
        "status": new_status,
    });
    if let Some(source) = source {
        payload["source"] = json!(source);
    }

    // Fire on_exit hooks for the state being LEFT
    if let Some(bindings) = pipeline.hooks_for_state(old_status) {
        for hook_name in &bindings.on_exit {
            let _ = engine.try_fire_hook_by_name(hook_name, payload.clone());
        }
    }
    // Fire on_enter hooks for the state being ENTERED
    if let Some(bindings) = pipeline.hooks_for_state(new_status) {
        for hook_name in &bindings.on_enter {
            let _ = engine.try_fire_hook_by_name(hook_name, payload.clone());
        }
    }
    // No fallback — YAML is the sole source of truth for hook bindings.
}

/// Drain deferred side-effects produced while hooks were executing.
///
/// Hooks cannot re-enter the engine, so transition requests and dispatch
/// creations are accumulated for post-hook replay.
// reason: pub kanban hook side-effect drainer; lib-build callers are cfg/test-gated. See #3034.
#[allow(dead_code)]
pub fn drain_hook_side_effects(db: &Db, engine: &PolicyEngine) {
    drain_hook_side_effects_with_backends(Some(db), engine);
}

pub fn drain_hook_side_effects_with_backends(db: Option<&Db>, engine: &PolicyEngine) {
    loop {
        let intent_result = engine.drain_pending_intents();
        let mut transitions = intent_result.transitions;
        transitions.extend(engine.drain_pending_transitions());

        if transitions.is_empty() {
            break;
        }

        for (card_id, old_status, new_status) in &transitions {
            fire_transition_hooks_with_backends(
                db,
                engine.pg_pool(),
                engine,
                card_id,
                old_status,
                new_status,
            );
        }
    }
}

/// Fire pipeline-defined event hooks for a lifecycle event (#134).
///
/// Looks up the `events` section of the effective pipeline and fires each
/// hook name via `try_fire_hook_by_name`. Falls back to firing the default
/// hook name if no pipeline config or no event binding is found.
// reason: pub kanban event-hook firer used by dispatched_sessions; lib-build callers are cfg/test-gated. See #3034.
#[allow(dead_code)]
pub fn fire_event_hooks(
    db: &Db,
    engine: &PolicyEngine,
    event: &str,
    default_hook: &str,
    payload: serde_json::Value,
) {
    fire_event_hooks_with_backends(Some(db), engine, event, default_hook, payload);
}

pub fn fire_event_hooks_with_backends(
    db: Option<&Db>,
    engine: &PolicyEngine,
    event: &str,
    default_hook: &str,
    payload: serde_json::Value,
) {
    crate::pipeline::ensure_loaded();
    let hooks: Vec<String> = crate::pipeline::try_get()
        .and_then(|p| p.event_hooks(event).cloned())
        .unwrap_or_else(|| vec![default_hook.to_string()]);
    for hook_name in &hooks {
        let _ = engine.try_fire_hook_by_name(hook_name, payload.clone());
    }
    // Event hook callers already own transition draining; only materialize
    // deferred dispatch intents here so follow-up notification queries can see them.
    let _ = db;
    let _ = engine.drain_pending_intents();
}

/// Fire only the pipeline-defined on_enter/on_exit hooks for a transition.
///
/// Unlike `fire_transition_hooks`, this does NOT perform side-effects
/// (audit log, GitHub sync, terminal-state sync, dispatch notifications).
/// Use this when callers already handle those concerns separately
/// (e.g. dispatch creation, route handlers).
fn resolve_effective_pipeline_for_hooks(
    db: Option<&Db>,
    pg_pool: Option<&sqlx::PgPool>,
    card_id: &str,
) -> Option<crate::pipeline::PipelineConfig> {
    crate::pipeline::ensure_loaded();

    if let Some(pg_pool) = pg_pool {
        let card_id_owned = card_id.to_string();
        return match crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |bridge_pool| async move {
                let row = sqlx::query(
                    "SELECT repo_id, assigned_agent_id
                     FROM kanban_cards
                     WHERE id = $1",
                )
                .bind(&card_id_owned)
                .fetch_optional(&bridge_pool)
                .await
                .map_err(|error| {
                    format!("load postgres hook card context {card_id_owned}: {error}")
                })?;

                let (repo_id, agent_id) = if let Some(row) = row {
                    (
                        row.try_get::<Option<String>, _>("repo_id")
                            .map_err(|error| {
                                format!("decode postgres repo_id for {card_id_owned}: {error}")
                            })?,
                        row.try_get::<Option<String>, _>("assigned_agent_id")
                            .map_err(|error| {
                                format!(
                                    "decode postgres assigned_agent_id for {card_id_owned}: {error}"
                                )
                            })?,
                    )
                } else {
                    (None, None)
                };

                Ok(Some(
                    crate::pipeline::resolve_for_card_pg(
                        &bridge_pool,
                        repo_id.as_deref(),
                        agent_id.as_deref(),
                    )
                    .await,
                ))
            },
            |error| error,
        ) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!("failed to resolve postgres hook pipeline for {card_id}: {error}");
                None
            }
        };
    }

    {
        let _ = db;
        None
    }
}

// reason: pub kanban state-hook firer used by dispatch_create; lib-build callers are cfg/test-gated. See #3034.
#[allow(dead_code)]
pub fn fire_state_hooks(db: &Db, engine: &PolicyEngine, card_id: &str, from: &str, to: &str) {
    fire_state_hooks_with_backends(Some(db), engine, card_id, from, to);
}

pub fn fire_state_hooks_with_backends(
    db: Option<&Db>,
    engine: &PolicyEngine,
    card_id: &str,
    from: &str,
    to: &str,
) {
    if from == to {
        return;
    }
    let effective = resolve_effective_pipeline_for_hooks(db, engine.pg_pool(), card_id);
    if let Some(ref pipeline) = effective {
        fire_dynamic_hooks(engine, pipeline, card_id, from, to, None);
    }
    drain_hook_side_effects_with_backends(db, engine);
}

/// Fire only the on_enter hooks for a specific state, without requiring a transition.
///
/// Used when re-entering the same state (e.g., restarting review from awaiting_dod)
/// where `fire_state_hooks` would no-op because from == to.
// reason: pub kanban enter-hook firer; lib-build callers are cfg/test-gated. See #3034.
#[allow(dead_code)]
pub fn fire_enter_hooks(db: &Db, engine: &PolicyEngine, card_id: &str, state: &str) {
    fire_enter_hooks_with_backends(Some(db), engine, card_id, state);
}

pub fn fire_enter_hooks_with_backends(
    db: Option<&Db>,
    engine: &PolicyEngine,
    card_id: &str,
    state: &str,
) {
    let effective = resolve_effective_pipeline_for_hooks(db, engine.pg_pool(), card_id);
    if let Some(ref pipeline) = effective {
        if let Some(bindings) = pipeline.hooks_for_state(state) {
            let payload = json!({
                "card_id": card_id,
                "from": state,
                "to": state,
                "status": state,
            });
            for hook_name in &bindings.on_enter {
                let _ = engine.try_fire_hook_by_name(hook_name, payload.clone());
            }
        }
    }
    drain_hook_side_effects_with_backends(db, engine);
}

/// Fire hooks for a status transition that already happened in the DB.
/// Use this when the DB UPDATE was done elsewhere (e.g., update_card with mixed fields).
// reason: pub kanban transition-hook firer; lib-build callers are cfg/test-gated. See #3034.
#[allow(dead_code)]
pub fn fire_transition_hooks(db: &Db, engine: &PolicyEngine, card_id: &str, from: &str, to: &str) {
    fire_transition_hooks_with_backends(Some(db), engine.pg_pool(), engine, card_id, from, to);
}

pub fn fire_transition_hooks_with_backends(
    db: Option<&Db>,
    pg_pool: Option<&sqlx::PgPool>,
    engine: &PolicyEngine,
    card_id: &str,
    from: &str,
    to: &str,
) {
    if from == to {
        return;
    }

    if let Some(pg_pool) = pg_pool {
        fire_transition_hooks_pg(db, pg_pool, engine, card_id, from, to);
        return;
    }

    {
        let _ = (db, engine, card_id, from, to);
        return;
    }
}

fn fire_transition_hooks_pg(
    db: Option<&Db>,
    pg_pool: &sqlx::PgPool,
    engine: &PolicyEngine,
    card_id: &str,
    from: &str,
    to: &str,
) {
    let card_id_owned = card_id.to_string();
    let from_owned = from.to_string();
    let to_owned = to.to_string();
    let effective = match crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |bridge_pool| async move {
            log_transition_audit_pg(
                &bridge_pool,
                &card_id_owned,
                &from_owned,
                &to_owned,
                "hook",
                "OK",
            )
            .await?;

            crate::pipeline::ensure_loaded();
            let row = sqlx::query(
                "SELECT repo_id, assigned_agent_id
                 FROM kanban_cards
                 WHERE id = $1",
            )
            .bind(&card_id_owned)
            .fetch_optional(&bridge_pool)
            .await
            .map_err(|error| {
                format!("load postgres card transition context {card_id_owned}: {error}")
            })?;
            let (repo_id, agent_id) = if let Some(row) = row {
                (
                    row.try_get::<Option<String>, _>("repo_id")
                        .map_err(|error| {
                            format!("decode postgres repo_id for {card_id_owned}: {error}")
                        })?,
                    row.try_get::<Option<String>, _>("assigned_agent_id")
                        .map_err(|error| {
                            format!(
                                "decode postgres assigned_agent_id for {card_id_owned}: {error}"
                            )
                        })?,
                )
            } else {
                (None, None)
            };
            Ok(Some(
                crate::pipeline::resolve_for_card_pg(
                    &bridge_pool,
                    repo_id.as_deref(),
                    agent_id.as_deref(),
                )
                .await,
            ))
        },
        |error| error,
    ) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!("failed to fire postgres transition hooks for {card_id}: {error}");
            None
        }
    };

    if let Some(ref pipeline) = effective {
        if pipeline.is_terminal(to) {
            let card_id_owned = card_id.to_string();
            let terminal_followup = crate::utils::async_bridge::block_on_pg_result(
                pg_pool,
                move |bridge_pool| async move {
                    let mut tx = bridge_pool.begin().await.map_err(|error| {
                        format!("begin postgres terminal follow-up tx: {error}")
                    })?;
                    sync_terminal_transition_followups_pg(&mut tx, &card_id_owned)
                        .await
                        .map_err(|error| format!("{error}"))?;
                    tx.commit().await.map_err(|error| {
                        format!("commit postgres terminal follow-up tx: {error}")
                    })?;
                    Ok(())
                },
                |error| error,
            );
            if let Err(error) = terminal_followup {
                tracing::warn!(
                    "[kanban] failed postgres terminal follow-up sync for {}: {}",
                    card_id,
                    error
                );
            }
        }

        let pg_pool_owned = pg_pool.clone();
        let pipeline_owned = pipeline.clone();
        let card_id_owned = card_id.to_string();
        let to_owned = to.to_string();
        let _ = crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |_bridge_pool| async move {
                github_sync_on_transition_pg(
                    &pg_pool_owned,
                    &pipeline_owned,
                    &card_id_owned,
                    &to_owned,
                )
                .await;
                Ok(())
            },
            |_error| (),
        );
        fire_dynamic_hooks(engine, pipeline, card_id, from, to, Some("hook"));

        if pipeline.is_terminal(to)
            && record_true_negative_if_pass_with_backends(db, Some(pg_pool), card_id)
        {
            crate::server::routes::review_verdict::spawn_aggregate_if_needed_with_pg(Some(
                pg_pool.clone(),
            ));
        }
    }
}
