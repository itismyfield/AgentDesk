//! Owns escalation key formats, cooldown decisions and cleanup for policies and transitions.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};

const PENDING_PREFIX: &str = "pm_pending:";
const SENT_PREFIX: &str = "pm_decision_sent:";
const COOLDOWN_SEC: i64 = 600;
const PENDING_TTL_SEC: i64 = 600;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Command {
    Enqueue {
        card_id: String,
        title: String,
        reason: String,
    },
    Pending,
    Prepare {
        card_id: String,
        value: Option<String>,
        fingerprint: Option<String>,
    },
    Sent {
        card_id: String,
        fingerprint: String,
    },
}

fn keys(card_id: &str) -> [String; 2] {
    [
        format!("{PENDING_PREFIX}{card_id}"),
        format!("{SENT_PREFIX}{card_id}"),
    ]
}

pub(crate) async fn clear_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    card_id: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM kv_meta WHERE key = ANY($1)")
        .bind(keys(card_id).to_vec())
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!("clear postgres escalation state for {card_id}: {error}")
        })?;
    Ok(())
}

async fn get(pool: &PgPool, key: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT value FROM kv_meta WHERE key = $1 AND (expires_at IS NULL OR expires_at > NOW())",
    )
    .bind(key)
    .fetch_optional(pool)
    .await?)
}

async fn set(pool: &PgPool, key: &str, value: Value, ttl: i64) -> Result<()> {
    sqlx::query("INSERT INTO kv_meta (key, value, expires_at) VALUES ($1, $2, NOW() + ($3 * INTERVAL '1 second')) ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, expires_at = EXCLUDED.expires_at")
        .bind(key).bind(value.to_string()).bind(ttl).execute(pool).await?;
    Ok(())
}

async fn delete(pool: &PgPool, key: &str) -> Result<()> {
    sqlx::query("DELETE FROM kv_meta WHERE key = $1")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

// Persisted cooldowns predate JSON records; retain parseInt's decimal-prefix compatibility.
fn decimal_prefix(raw: &str) -> i64 {
    let raw = raw.trim_start();
    let end = raw
        .char_indices()
        .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && matches!(c, '+' | '-')))
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    raw[..end].parse().unwrap_or(0)
}

fn cooldown_elapsed(raw: &str, fingerprint: &str, now: i64) -> Option<i64> {
    let parsed = serde_json::from_str::<Value>(raw).ok();
    let (sent_at, status) = match parsed.as_ref().filter(|v| v.is_object() || v.is_array()) {
        Some(record) => {
            let sent = record
                .get("sent_at")
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string())
                })
                .unwrap_or_default();
            (
                decimal_prefix(&sent),
                record
                    .get("status")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty()),
            )
        }
        None => (decimal_prefix(raw), None),
    };
    let elapsed = now.saturating_sub(sent_at);
    (status.is_none_or(|status| status == fingerprint) && elapsed < COOLDOWN_SEC).then_some(elapsed)
}

pub(crate) async fn execute(pool: &PgPool, command: Command) -> Result<Value> {
    match command {
        Command::Pending => {
            // The policy has always scanned the pending snapshot even when its TTL expired.
            let rows: Vec<(String, Option<String>)> =
                sqlx::query_as("SELECT key, value FROM kv_meta WHERE key LIKE $1")
                    .bind(format!("{PENDING_PREFIX}%"))
                    .fetch_all(pool)
                    .await?;
            Ok(Value::Array(rows.into_iter().map(|(key, value)| json!({"card_id": &key[PENDING_PREFIX.len()..], "value": value})).collect()))
        }
        Command::Enqueue {
            card_id,
            title,
            reason,
        } => {
            let [pending, _] = keys(&card_id);
            let mut entry = get(pool, &pending)
                .await
                .unwrap_or_default()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .filter(|v| !v.is_null() && *v != json!(false) && *v != json!(0) && *v != json!(""))
                .unwrap_or_else(|| json!({"title": title, "reasons": []}));
            let Some(object) = entry.as_object_mut() else {
                bail!("invalid escalation pending entry for {card_id}");
            };
            if object.get("title").is_none_or(|v| {
                v.is_null() || *v == json!("") || *v == json!(false) || *v == json!(0)
            }) {
                object.insert("title".into(), json!(title));
            }
            let reasons = object
                .get_mut("reasons")
                .and_then(Value::as_array_mut)
                .context("invalid escalation reasons")?;
            if !reasons.contains(&json!(reason)) {
                reasons.push(json!(reason));
            }
            // The policy ignored generic KV write errors; keep these statements best effort.
            let _ = set(pool, &pending, entry, PENDING_TTL_SEC).await;
            Ok(Value::Null)
        }
        Command::Prepare {
            card_id,
            value,
            fingerprint,
        } => {
            let [pending, sent] = keys(&card_id);
            let Some(fingerprint) = fingerprint else {
                let _ = delete(pool, &pending).await;
                let _ = delete(pool, &sent).await;
                return Ok(Value::Null);
            };
            let entry = match serde_json::from_str::<Value>(value.as_deref().unwrap_or("null")) {
                Ok(entry) => entry,
                Err(_) => {
                    let _ = delete(pool, &pending).await;
                    return Ok(Value::Null);
                }
            };
            if let Some(elapsed) = get(pool, &sent)
                .await
                .unwrap_or_default()
                .filter(|raw| !raw.is_empty())
                .and_then(|raw| {
                    cooldown_elapsed(&raw, &fingerprint, chrono::Utc::now().timestamp())
                })
            {
                tracing::info!("[escalation] cooldown skip for {card_id} ({elapsed}s)");
                return Ok(Value::Null);
            }
            Ok(
                json!({"reasons": entry.get("reasons").and_then(Value::as_array).cloned().unwrap_or_default()}),
            )
        }
        Command::Sent {
            card_id,
            fingerprint,
        } => {
            let [pending, sent] = keys(&card_id);
            // HTTP has already succeeded. Keep the existing stamp-then-delete statement boundary.
            let _ = set(
                pool,
                &sent,
                json!({"sent_at":chrono::Utc::now().timestamp(), "status":fingerprint}),
                COOLDOWN_SEC,
            )
            .await;
            let _ = delete(pool, &pending).await;
            Ok(Value::Null)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::cooldown_elapsed;

    #[test]
    fn cooldown_boundary_and_legacy_decimal_prefix() {
        for raw in [r#"{"sent_at":1000,"status":"same"}"#, "1000", "1000legacy"] {
            assert_eq!(cooldown_elapsed(raw, "same", 1599), Some(599));
            assert_eq!(cooldown_elapsed(raw, "same", 1600), None);
            assert_eq!(cooldown_elapsed(raw, "same", 1601), None);
        }
        assert_eq!(
            cooldown_elapsed(r#"{"sent_at":1000,"status":"old"}"#, "new", 1001),
            None
        );
        assert_eq!(
            cooldown_elapsed(r#"{"sent_at":"1000old"}"#, "any", 1001),
            Some(1)
        );
    }
}
