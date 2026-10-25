//! Recovery preserves delivery, never reconstructs a live attempt or continuation.

use super::*;
use crate::db::replay_disposition::{
    hold_notice::{self, NoticeRead, NoticeRouting},
    write,
};

pub(super) async fn resume(
    pool: Option<&sqlx::PgPool>,
    state: &inflight::InflightTurnState,
) -> bool {
    if crate::services::discord::router::replay_producers_dormant(&state.channel_id.to_string())
        || !state.replay_rerun_blocked()
        || matches!(
            state.runtime_kind,
            Some(
                crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
                    | crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
            )
        )
    {
        return true;
    }
    let (Some(pool), Some(id)) = (pool, state.replay_receipt_id) else {
        return false;
    };
    if state.runtime_kind_unknown_on_disk || state.version > inflight::inflight_state_version() {
        return false;
    }
    match hold_notice::read(pool, id).await {
        Ok(NoticeRead::Missing) => return false,
        Err(error) => {
            tracing::warn!(receipt=id, %error, "recovery notice unverified; keeping custody");
            return false;
        }
        Ok(NoticeRead::Notified(notice)) => {
            tracing::debug!(
                receipt = id,
                outbox = notice.outbox_id,
                "resuming existing replay notice"
            );
        }
        Ok(NoticeRead::NotNotified) => {}
    }
    let Ok(capture) = serde_json::to_value(state) else {
        return false;
    };
    write::recover_hold_notice(
        pool,
        id,
        &state.provider,
        &state.channel_id.to_string(),
        Some(&capture),
        NoticeRouting {
            bot: "notify",
            session_key: state.session_key.as_deref(),
        },
    )
    .await
    .inspect_err(|error| tracing::warn!(receipt=id, %error, "recovery hold not acknowledged"))
    .is_ok()
}

// The dormant build exercises enumeration at the test boot hook; production wiring follows activation.
#[cfg(test)]
pub(super) async fn resume_pg_only(pool: Option<&sqlx::PgPool>, provider: &str) {
    let Some(pool) = pool else {
        return;
    };
    let mut cursor = 0_i64;
    loop {
        let page: Result<Vec<(i64, String)>, _> = sqlx::query_as(
            "SELECT id, channel_id FROM intake_outbox WHERE id>$1
               AND lower(btrim(provider))=lower(btrim($2))
               AND replay_disposition IN ('withheld','started_unclassified','startup_failed_no_effect')
               AND replay_preserved -> 'stale_replay_notice' IS NULL ORDER BY id LIMIT 64",
        ).bind(cursor).bind(provider).fetch_all(pool).await;
        let page = match page {
            Ok(page) => page,
            Err(error) => {
                tracing::warn!(%error, "PG-only replay notice scan unverified");
                return;
            }
        };
        if page.is_empty() {
            return;
        }
        for (id, channel) in page {
            cursor = id;
            if crate::services::discord::router::replay_producers_dormant(&channel) {
                continue;
            }
            if let Err(error) = write::recover_hold_notice(
                pool,
                id,
                provider,
                &channel,
                None,
                NoticeRouting {
                    bot: "notify",
                    session_key: None,
                },
            )
            .await
            {
                tracing::warn!(receipt=id, %error, "PG-only replay hold not acknowledged");
            }
        }
    }
}

pub(super) fn observe_restore_inflight_snapshot(
    provider: &ProviderKind,
    states: &[inflight::InflightTurnState],
    boot_elapsed: std::time::Duration,
) {
    let current_generation = super::runtime_store::process_generation();
    tracing::info!(provider = %provider.as_str(), snapshot_rows = states.len(), boot_elapsed_ms = boot_elapsed.as_millis(), "restore_inflight snapshot loaded");
    for state in states
        .iter()
        .filter(|state| state.born_generation != 0 && state.born_generation == current_generation)
    {
        tracing::warn!(provider = %provider.as_str(), channel_id = state.channel_id, user_msg_id = state.user_msg_id, born_generation = state.born_generation, current_generation, boot_elapsed_ms = boot_elapsed.as_millis(), "restore_inflight snapshot contains a row authored by the running process");
    }
}
