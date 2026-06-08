//! Escalation-settings resolution (read path).
//!
//! These helpers previously lived in `crate::server::routes::escalation`, which
//! forced service-layer callers (dispatch delivery owner resolution) to reach
//! back up into the server layer via `crate::server::routes::escalation::*`
//! (#3037 service→server backflow). The settings-read logic depends only on
//! `crate::config` types/constants (already below services) and the
//! `crate::utils::async_bridge` PG bridge, so it belongs in the service layer.
//!
//! The escalation route module re-imports these symbols for its handlers, so
//! this stays the single source of truth and the dependency direction is now
//! routes -> services (not the reverse).

use crate::config::{
    Config, DEFAULT_ESCALATION_PM_HOURS, DEFAULT_ESCALATION_TIMEZONE, EscalationScheduleSettings,
    EscalationSettings,
};

/// Postgres `kv_meta` key holding the persisted escalation-settings override.
pub(crate) const ESCALATION_SETTINGS_OVERRIDE_KEY: &str = "escalation-settings-override";

/// Trim a string and treat empty results as absent.
pub(crate) fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
}

/// Compute the escalation settings implied by static config (no override).
pub(crate) fn escalation_defaults(config: &Config) -> EscalationSettings {
    EscalationSettings {
        mode: config.escalation.mode.clone(),
        owner_user_id: config.escalation.owner_user_id.or(config.discord.owner_id),
        pm_channel_id: normalize_optional_string(
            config
                .escalation
                .pm_channel_id
                .clone()
                .or_else(|| config.kanban.human_alert_channel_id.clone()),
        ),
        schedule: EscalationScheduleSettings {
            pm_hours: config
                .escalation
                .schedule
                .pm_hours
                .clone()
                .unwrap_or_else(|| DEFAULT_ESCALATION_PM_HOURS.to_string()),
            timezone: config
                .escalation
                .schedule
                .timezone
                .clone()
                .unwrap_or_else(|| DEFAULT_ESCALATION_TIMEZONE.to_string()),
        },
    }
}

/// Load the persisted escalation-settings override from Postgres, if present.
pub(crate) async fn load_override_pg_async(
    pool: &sqlx::PgPool,
) -> Result<Option<EscalationSettings>, String> {
    let raw = sqlx::query_scalar::<_, String>(
        "SELECT value
         FROM kv_meta
         WHERE key = $1
         LIMIT 1",
    )
    .bind(ESCALATION_SETTINGS_OVERRIDE_KEY)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        format!(
            "load postgres escalation settings override {ESCALATION_SETTINGS_OVERRIDE_KEY}: {error}"
        )
    })?;
    Ok(raw.and_then(|value| serde_json::from_str::<EscalationSettings>(&value).ok()))
}

/// Resolve effective escalation settings: config defaults overlaid with any
/// persisted Postgres override.
pub(crate) fn merged_settings_pg(
    pool: &sqlx::PgPool,
    config: &Config,
) -> Result<EscalationSettings, String> {
    let defaults = escalation_defaults(config);
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            Ok(load_override_pg_async(&bridge_pool)
                .await?
                .unwrap_or(defaults))
        },
        |error| error,
    )
}

/// Resolve the effective escalation owner user id across available backends.
pub(crate) fn effective_owner_user_id_with_backends(
    _db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    config: &Config,
) -> Option<u64> {
    if let Some(pool) = pg_pool {
        match merged_settings_pg(pool, config) {
            Ok(settings) => return settings.owner_user_id,
            Err(error) => {
                tracing::warn!(%error, "[escalation] failed to load postgres escalation settings override");
            }
        }
    }

    escalation_defaults(config).owner_user_id
}
