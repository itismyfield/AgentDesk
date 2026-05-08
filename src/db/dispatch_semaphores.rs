use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use sqlx::{Postgres, Transaction};

use crate::config::ClusterSemaphoreConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchSemaphoreAcquireOutcome {
    pub acquired: bool,
    pub reasons: Vec<String>,
}

impl DispatchSemaphoreAcquireOutcome {
    fn acquired() -> Self {
        Self {
            acquired: true,
            reasons: Vec::new(),
        }
    }

    fn rejected(reasons: Vec<String>) -> Self {
        Self {
            acquired: false,
            reasons,
        }
    }
}

pub fn required_semaphore_names(required_capabilities: Option<&Value>) -> Vec<String> {
    let Some(required_capabilities) = required_capabilities else {
        return Vec::new();
    };
    let hard_required = required_capabilities
        .get("required")
        .filter(|value| value.is_object())
        .unwrap_or(required_capabilities);

    let mut names = BTreeSet::new();
    collect_semaphore_names(hard_required.get("semaphores"), &mut names);
    collect_semaphore_names(hard_required.get("semaphore"), &mut names);
    names.into_iter().collect()
}

fn collect_semaphore_names(value: Option<&Value>, names: &mut BTreeSet<String>) {
    match value {
        Some(Value::String(name)) => {
            insert_name(names, name);
        }
        Some(Value::Array(values)) => {
            for value in values {
                if let Some(name) = value.as_str() {
                    insert_name(names, name);
                }
            }
        }
        Some(Value::Object(map)) => {
            for (name, enabled) in map {
                if enabled.as_bool() != Some(false) {
                    insert_name(names, name);
                }
            }
        }
        _ => {}
    }
}

fn insert_name(names: &mut BTreeSet<String>, name: &str) {
    let trimmed = name.trim();
    if !trimmed.is_empty() && trimmed.len() <= 128 {
        names.insert(trimmed.to_string());
    }
}

pub async fn reclaim_expired_dispatch_semaphores_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<u64, sqlx::Error> {
    sqlx::query("DELETE FROM dispatch_semaphore_holdings WHERE expires_at <= NOW()")
        .execute(&mut **tx)
        .await
        .map(|result| result.rows_affected())
}

pub async fn release_dispatch_semaphores_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    dispatch_id: &str,
) -> Result<u64, sqlx::Error> {
    sqlx::query("DELETE FROM dispatch_semaphore_holdings WHERE dispatch_id = $1")
        .bind(dispatch_id)
        .execute(&mut **tx)
        .await
        .map(|result| result.rows_affected())
}

pub async fn semaphore_unavailable_reasons_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    required_capabilities: Option<&Value>,
    configs: &BTreeMap<String, ClusterSemaphoreConfig>,
    instance_id: &str,
) -> Result<Vec<String>, sqlx::Error> {
    let mut reasons = Vec::new();
    for name in required_semaphore_names(required_capabilities) {
        let Some(config) = configs.get(&name) else {
            reasons.push(format!("semaphore '{name}' is not configured"));
            continue;
        };
        let scope = config.scope.as_str();
        let scope_key = config.scope.scope_key(instance_id);
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT
             FROM dispatch_semaphore_holdings
             WHERE semaphore_name = $1
               AND scope = $2
               AND scope_key = $3
               AND expires_at > NOW()",
        )
        .bind(&name)
        .bind(scope)
        .bind(&scope_key)
        .fetch_one(&mut **tx)
        .await?;
        let capacity = i64::from(config.effective_capacity());
        if active >= capacity {
            reasons.push(format!(
                "semaphore '{name}' exhausted for {scope}:{scope_key} ({active}/{capacity} active)"
            ));
        }
    }
    Ok(reasons)
}

pub async fn try_acquire_dispatch_semaphores_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    dispatch_id: &str,
    holder_instance_id: &str,
    ttl_secs: i64,
    required_capabilities: Option<&Value>,
    configs: &BTreeMap<String, ClusterSemaphoreConfig>,
) -> Result<DispatchSemaphoreAcquireOutcome, sqlx::Error> {
    let names = required_semaphore_names(required_capabilities);
    if names.is_empty() {
        return Ok(DispatchSemaphoreAcquireOutcome::acquired());
    }

    release_dispatch_semaphores_on_pg_tx(tx, dispatch_id).await?;
    reclaim_expired_dispatch_semaphores_on_pg_tx(tx).await?;

    let ttl_secs = ttl_secs.clamp(1, 24 * 60 * 60);
    let mut reasons = Vec::new();
    for name in names {
        let Some(config) = configs.get(&name) else {
            reasons.push(format!("semaphore '{name}' is not configured"));
            break;
        };
        let scope = config.scope.as_str();
        let scope_key = config.scope.scope_key(holder_instance_id);
        let capacity = config.effective_capacity();
        let inserted = sqlx::query(
            "WITH slots AS (
                 SELECT generate_series(0, $5::INT - 1) AS slot_index
             ),
             free_slot AS (
                 SELECT slot_index
                 FROM slots
                 WHERE NOT EXISTS (
                     SELECT 1
                     FROM dispatch_semaphore_holdings h
                     WHERE h.semaphore_name = $1
                       AND h.scope = $2
                       AND h.scope_key = $3
                       AND h.slot_index = slots.slot_index
                       AND h.expires_at > NOW()
                 )
                 ORDER BY slot_index ASC
                 LIMIT 1
             ),
             acquired AS (
                 INSERT INTO dispatch_semaphore_holdings (
                     semaphore_name,
                     scope,
                     scope_key,
                     slot_index,
                     holder_instance_id,
                     dispatch_id,
                     acquired_at,
                     expires_at,
                     updated_at
                 )
                 SELECT $1, $2, $3, slot_index, $4, $6, NOW(),
                        NOW() + ($7::BIGINT * INTERVAL '1 second'),
                        NOW()
                 FROM free_slot
                 ON CONFLICT (semaphore_name, scope, scope_key, slot_index) DO UPDATE SET
                     holder_instance_id = EXCLUDED.holder_instance_id,
                     dispatch_id = EXCLUDED.dispatch_id,
                     acquired_at = NOW(),
                     expires_at = EXCLUDED.expires_at,
                     updated_at = NOW()
                 WHERE dispatch_semaphore_holdings.expires_at <= NOW()
                    OR dispatch_semaphore_holdings.dispatch_id = EXCLUDED.dispatch_id
                 RETURNING slot_index
             )
             SELECT slot_index FROM acquired",
        )
        .bind(&name)
        .bind(scope)
        .bind(&scope_key)
        .bind(holder_instance_id)
        .bind(capacity)
        .bind(dispatch_id)
        .bind(ttl_secs)
        .fetch_optional(&mut **tx)
        .await?;

        if inserted.is_none() {
            let active = active_holding_count(tx, &name, scope, &scope_key).await?;
            reasons.push(format!(
                "semaphore '{name}' exhausted for {scope}:{scope_key} ({active}/{capacity} active)"
            ));
            break;
        }
    }

    if reasons.is_empty() {
        Ok(DispatchSemaphoreAcquireOutcome::acquired())
    } else {
        release_dispatch_semaphores_on_pg_tx(tx, dispatch_id).await?;
        Ok(DispatchSemaphoreAcquireOutcome::rejected(reasons))
    }
}

async fn active_holding_count(
    tx: &mut Transaction<'_, Postgres>,
    name: &str,
    scope: &str,
    scope_key: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT
         FROM dispatch_semaphore_holdings
         WHERE semaphore_name = $1
           AND scope = $2
           AND scope_key = $3
           AND expires_at > NOW()",
    )
    .bind(name)
    .bind(scope)
    .bind(scope_key)
    .fetch_one(&mut **tx)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn required_semaphore_names_reads_required_namespace() {
        let required = json!({
            "required": {
                "semaphores": ["ue_editor", " ue_editor ", "gpu"]
            },
            "preferred": {"labels": ["mac-mini"]}
        });

        assert_eq!(
            required_semaphore_names(Some(&required)),
            vec!["gpu".to_string(), "ue_editor".to_string()]
        );
    }

    #[test]
    fn required_semaphore_names_accepts_object_and_singular_forms() {
        let required = json!({
            "semaphore": "camera",
            "semaphores": {"ue_editor": true, "disabled": false}
        });

        assert_eq!(
            required_semaphore_names(Some(&required)),
            vec!["camera".to_string(), "ue_editor".to_string()]
        );
    }
}
