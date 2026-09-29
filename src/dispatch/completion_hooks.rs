use anyhow::Result;
use serde_json::json;
use sqlx::PgPool;

use super::dispatch_query::query_dispatch_row_pg;
use super::dispatch_status::{
    auto_queue_review_disabled_for_dispatch_pg, block_on_dispatch_pg,
    card_needs_review_dispatch_pg, dispatch_exists_pg, maybe_inject_phase_gate_verdict_pg,
};
use crate::engine::PolicyEngine;

pub(super) fn fire_dispatch_completed_hooks(
    engine: &PolicyEngine,
    dispatch_id: &str,
    kanban_card_id: Option<&str>,
    result: serde_json::Value,
    needs_review_dispatch: bool,
) {
    crate::kanban::fire_event_hooks_with_backends(
        engine,
        "on_dispatch_completed",
        "OnDispatchCompleted",
        json!({
            "dispatch_id": dispatch_id,
            "kanban_card_id": kanban_card_id,
            "result": result,
        }),
    );

    crate::kanban::drain_hook_side_effects_with_backends(engine);

    if needs_review_dispatch {
        let cid = kanban_card_id.unwrap_or("unknown");
        tracing::warn!(
            "[dispatch] Card {} in review-like state but no review dispatch — re-firing OnReviewEnter with blocking lock (#220)",
            cid
        );
        let _ = engine.fire_hook_by_name_blocking("OnReviewEnter", json!({ "card_id": cid }));
        crate::kanban::drain_hook_side_effects_with_backends(engine);
    }
}

/// Fires completion hooks for a dispatch completed by direct DB write (the relay
/// fallback), which leaves a `reconcile_dispatch:*` marker. False means nothing to replay.
fn replay_dispatch_completed_hooks(engine: &PolicyEngine, dispatch_id: &str) -> Result<bool> {
    let Some(pool) = engine.pg_pool() else {
        return Err(anyhow::anyhow!(
            "Postgres pool required to replay dispatch {dispatch_id}"
        ));
    };
    let dispatch_id_owned = dispatch_id.to_string();
    let replay = block_on_dispatch_pg(pool, move |pool| async move {
        if !dispatch_exists_pg(&pool, &dispatch_id_owned).await? {
            return Ok(None);
        }
        let dispatch = query_dispatch_row_pg(&pool, &dispatch_id_owned).await?;
        if dispatch.get("status").and_then(|value| value.as_str()) != Some("completed") {
            return Ok(None);
        }
        let dispatch_type = dispatch
            .get("dispatch_type")
            .and_then(|value| value.as_str());
        // Same review_mode=disabled skip as the live path; the fallback already synced the entry.
        if matches!(dispatch_type, Some("implementation" | "rework"))
            && auto_queue_review_disabled_for_dispatch_pg(&pool, &dispatch_id_owned).await?
        {
            return Ok(None);
        }
        let kanban_card_id = dispatch
            .get("kanban_card_id")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let needs_review_dispatch = match kanban_card_id.as_deref() {
            Some(card_id) => card_needs_review_dispatch_pg(&pool, card_id).await?,
            None => false,
        };
        let result = dispatch.get("result").cloned().unwrap_or_default();
        let result = maybe_inject_phase_gate_verdict_pg(&pool, &dispatch_id_owned, &result)
            .await
            .unwrap_or(result);
        Ok(Some((kanban_card_id, result, needs_review_dispatch)))
    })?;
    let Some((kanban_card_id, result, needs_review_dispatch)) = replay else {
        return Ok(false);
    };
    fire_dispatch_completed_hooks(
        engine,
        dispatch_id,
        kanban_card_id.as_deref(),
        result,
        needs_review_dispatch,
    );
    Ok(true)
}

/// Drains `reconcile_dispatch:*` markers; claiming each by DELETE keeps nodes from double-firing.
pub(crate) async fn replay_marked_dispatch_completions_pg(engine: &PolicyEngine, pool: &PgPool) {
    let markers = match sqlx::query_as::<_, (String, String)>(
        "SELECT key, value FROM kv_meta WHERE key LIKE 'reconcile_dispatch:%'",
    )
    .fetch_all(pool)
    .await
    {
        Ok(markers) => markers,
        Err(error) => {
            tracing::warn!("[dispatch] load reconcile_dispatch markers failed: {error}");
            return;
        }
    };
    for (key, dispatch_id) in markers {
        match sqlx::query("DELETE FROM kv_meta WHERE key = $1")
            .bind(&key)
            .execute(pool)
            .await
        {
            Ok(done) if done.rows_affected() == 1 => {}
            Ok(_) => continue,
            Err(error) => {
                tracing::warn!("[dispatch] claim marker {key} failed: {error}");
                continue;
            }
        }
        match replay_dispatch_completed_hooks(engine, &dispatch_id) {
            Ok(true) => tracing::info!("[dispatch] replayed completion hooks for {dispatch_id}"),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    "[dispatch] replay completion hooks for {dispatch_id} failed: {error}"
                )
            }
        }
    }
}
