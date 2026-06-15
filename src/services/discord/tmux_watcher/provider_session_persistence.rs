//! #3479 item-2: provider-session persistence helpers for the tmux watcher.
//!
//! Verbatim extraction (zero logic change) of the watcher provider-session
//! selector resolution/persistence helpers from `tmux_watcher.rs`. Items are
//! `pub(super)` here and re-imported by the parent so the watcher loop's call
//! sites stay byte-identical.

use super::*;

/// Resolve the provider session selector to durably persist at turn end.
///
/// #3095: a TUI resume turn frequently does NOT re-emit the provider session id
/// in its pane output, so `observed_session_id` (`state.last_session_id`) is
/// `None` on most committed turns even though resume is working off the durable
/// in-memory selector. Falling back to the cached `session.session_id` keeps the
/// DB selector in sync on every committed turn so resume survives an in-memory
/// cache loss (idle-expiry / dcserver restart). The fallback is guarded against
/// empty values so a stale/blank selector never overwrites a good DB row.
pub(super) fn resolve_persistable_provider_session_id(
    observed_session_id: Option<&str>,
    cached_session_id: Option<&str>,
) -> Option<String> {
    let nonempty = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    nonempty(observed_session_id).or_else(|| nonempty(cached_session_id))
}

pub(super) async fn persist_watcher_provider_session_id(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    tmux_session_name: &str,
    session_id: Option<&str>,
) {
    // #3095: when the TUI did not re-emit a session id this turn, fall back to
    // the durable in-memory selector so the DB row is refreshed on every
    // committed turn — not only on the rare turns that print the id.
    let session_id = {
        let mut data = shared.core.lock().await;
        let session = data.sessions.get_mut(&channel_id).filter(|s| !s.cleared);
        let cached_session_id = session.as_ref().and_then(|s| s.session_id.clone());
        let Some(session_id) =
            resolve_persistable_provider_session_id(session_id, cached_session_id.as_deref())
        else {
            return;
        };
        if let Some(session) = session {
            session.restore_provider_session(Some(session_id.clone()));
        }
        session_id
    };

    let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
        &shared.token_hash,
        provider,
        tmux_session_name,
    );
    crate::services::discord::adk_session::save_provider_session_id(
        &session_key,
        &session_id,
        Some(&session_id),
        provider,
        channel_id,
        shared.api_port,
    )
    .await;

    // #3053: persisting a provider selector is live runtime activity — emit an
    // auditable heartbeat touch so idle-kill's COALESCE(last_heartbeat,
    // created_at) row is refreshed and the candidate-key match is logged.
    // (hook_session already sets last_heartbeat; this adds the audit trail and
    // covers any divergent/legacy session_key the upsert did not reach.)
    touch_session_activity(
        None::<&crate::db::Db>,
        shared.pg_pool.as_ref(),
        &shared.token_hash,
        provider,
        tmux_session_name,
        crate::services::discord::adk_session::parse_thread_channel_id_from_name(
            &crate::services::provider::parse_provider_and_channel_from_tmux_name(
                tmux_session_name,
            )
            .map(|(_, channel)| channel)
            .unwrap_or_default(),
        ),
        "provider_selector_persisted",
        "tmux_watcher.rs:persist_provider_session_selector",
    );

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 👁 watcher persisted provider session selector for {} channel {}",
        tmux_session_name,
        channel_id.get()
    );
}

#[cfg(test)]
mod tests {
    use super::resolve_persistable_provider_session_id;

    // #3095: a freshly observed TUI session id always wins so the DB tracks the
    // newest selector.
    #[test]
    fn persistable_provider_session_prefers_freshly_observed_id() {
        assert_eq!(
            resolve_persistable_provider_session_id(Some("fresh-sid"), Some("cached-sid")),
            Some("fresh-sid".to_string())
        );
    }

    // #3095 core fix: a resume turn whose TUI output did NOT re-emit a session id
    // must still persist the durable in-memory selector so the DB row is kept in
    // sync and resume survives idle-expiry / dcserver restart.
    #[test]
    fn persistable_provider_session_falls_back_to_cached_selector_on_resume_turn() {
        assert_eq!(
            resolve_persistable_provider_session_id(None, Some("cached-sid")),
            Some("cached-sid".to_string())
        );
    }

    // #3095 guard: never overwrite a good DB row with an empty/blank selector —
    // neither the observed nor the cached value is usable, so persist is skipped.
    #[test]
    fn persistable_provider_session_skips_when_no_usable_selector() {
        assert_eq!(
            resolve_persistable_provider_session_id(None, None),
            None,
            "no selector available -> skip persist"
        );
        assert_eq!(
            resolve_persistable_provider_session_id(Some("   "), Some("")),
            None,
            "blank observed + empty cached -> skip persist"
        );
        assert_eq!(
            resolve_persistable_provider_session_id(Some(""), Some("cached-sid")),
            Some("cached-sid".to_string()),
            "blank observed must fall through to the usable cached selector"
        );
    }
}
