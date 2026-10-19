//! Admission settles a native `/clear` an earlier build left unresolved; a user clear itself ends
//! the session instead, and shares only the in-memory session reset kept here.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use poise::serenity_prelude as serenity;
use tokio::time::Instant;

use super::super::super::SharedData;
use crate::db::session_transcripts::{self, NativeClearBoundary, NativeClearGeneration};
use crate::services::provider::ProviderKind;
use crate::services::tui_prompt_dedupe::native_clear::{
    ClearCommit, NativeClearRestart, judge_native_clear_admission,
};

const NATIVE_ADMISSION_CAPTURE_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

type Effect<'a> = Pin<Box<dyn Future<Output = bool> + Send + 'a>>;

/// The pane and selector effects that settle an unresolved native clear; tests replace them.
pub(in crate::services::discord) trait NativeClearEffects:
    Send + Sync
{
    fn clear_selector<'a>(&'a self, session_key: &'a str) -> Effect<'a>;
    fn save_selector<'a>(
        &'a self,
        key: &'a str,
        session: &'a str,
        ch: serenity::ChannelId,
    ) -> Effect<'a>;
    fn composer_empty(&self, tmux: &str, deadline: Instant) -> bool;
    fn reset_process(&self, tmux: &str);
}

struct Production;

impl NativeClearEffects for Production {
    fn clear_selector<'a>(&'a self, session_key: &'a str) -> Effect<'a> {
        let clear = super::super::super::adk_session::clear_provider_session_id_checked;
        Box::pin(async move { clear(session_key).await.is_ok() })
    }
    fn save_selector<'a>(
        &'a self,
        key: &'a str,
        session: &'a str,
        ch: serenity::ChannelId,
    ) -> Effect<'a> {
        let save = super::super::super::adk_session::save_provider_session_id_checked;
        let provider = ProviderKind::Claude;
        Box::pin(async move {
            save(key, session, Some(session), &provider, ch)
                .await
                .is_ok()
        })
    }
    fn composer_empty(&self, tmux: &str, deadline: Instant) -> bool {
        let budget = deadline.saturating_duration_since(Instant::now());
        let capture = crate::services::platform::tmux::capture_pane_timeout(tmux, -80, budget);
        capture.is_some_and(|c| {
            crate::services::claude_tui::host_input::native_clear_composer_empty(&c)
        })
    }
    fn reset_process(&self, tmux: &str) {
        super::reset_managed_process_session(tmux);
    }
}

#[cfg(test)]
thread_local! {
    static TEST_CHANNELS: std::cell::RefCell<Option<Vec<u64>>> = const { std::cell::RefCell::new(None) };
    static TEST_EFFECTS: std::cell::RefCell<Option<Arc<dyn NativeClearEffects>>> =
        const { std::cell::RefCell::new(None) };
}

/// Installs `effects` as this thread's switched-on native clear until the guard drops.
#[cfg(test)]
pub(in crate::services::discord) fn switch_on_for_tests(
    effects: Arc<dyn NativeClearEffects>,
) -> impl Drop {
    struct Off;
    impl Drop for Off {
        fn drop(&mut self) {
            TEST_EFFECTS.with(|cell| cell.borrow_mut().take());
            TEST_CHANNELS.with(|cell| cell.borrow_mut().take());
        }
    }
    TEST_EFFECTS.with(|cell| *cell.borrow_mut() = Some(effects));
    Off
}

/// A Herdr channel's settle effects; they run whatever the native clear switch says.
fn host_effects() -> Arc<dyn NativeClearEffects> {
    Arc::new(Production)
}

/// Reads scope and effects together; disabled keeps the existing host behavior.
fn switched_on(channel_id: serenity::ChannelId) -> (bool, Option<Arc<dyn NativeClearEffects>>) {
    #[cfg(test)]
    if let Some(effects) = TEST_EFFECTS.with(|cell| cell.borrow().clone()) {
        let settings = crate::config::RuntimeSettingsConfig {
            native_clear_enabled: Some(true),
            native_clear_channels: TEST_CHANNELS.with(|cell| cell.borrow().clone()),
            ..Default::default()
        };
        let allowed = settings.native_clear_enabled_for_channel(channel_id.get());
        return (allowed, allowed.then_some(effects));
    }
    let Some(config) = crate::config_live_reload::current() else {
        return (true, None);
    };
    if !config.runtime.native_clear_enabled.unwrap_or(false) {
        return (true, None);
    }
    let allowed = config
        .runtime
        .native_clear_enabled_for_channel(channel_id.get());
    (
        allowed,
        allowed.then(|| Arc::new(Production) as Arc<dyn NativeClearEffects>),
    )
}

/// The cleared channel's in-memory provider session, history, uploads and role override.
pub(super) async fn clear_session_memory(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) {
    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            super::super::super::settings::cleanup_channel_uploads(channel_id);
            session.clear_provider_session();
            session.history.clear();
            session.pending_uploads.clear();
            session.cleared = true;
        }
    }
    shared.dispatch.role_overrides.remove(&channel_id);
}

/// A process reset applies the pending fast-mode, goals and model resets, so their markers go.
pub(super) async fn clear_process_reset_pending(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) {
    super::super::config::clear_fast_mode_reset_pending_for_channel(shared, channel_id);
    super::super::config::clear_codex_goals_reset_pending_for_channel(shared, channel_id);
    let overrides = &shared.overrides;
    overrides.model_session_reset_pending.remove(&channel_id);
    overrides.session_reset_pending.remove(&channel_id);
    super::clear_all_fast_mode_reset_markers(shared, channel_id).await;
    super::persist_codex_goals_reset_marker(shared, channel_id, false).await;
}

/// Applies the clear's in-memory result and marks its generation complete; a native commit keeps
/// the reset-pending markers its unrestarted process still owes.
async fn settle(
    shared: &Arc<SharedData>,
    pool: &sqlx::PgPool,
    channel_id: serenity::ChannelId,
    generation: NativeClearGeneration,
    native: Option<&ClearCommit>,
) -> bool {
    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
            session.session_id = native.map(|commit| commit.session.clone());
            session.cleared = true;
        }
    }
    if native.is_none() {
        clear_process_reset_pending(shared, channel_id).await;
    }
    let key = channel_id.get().to_string();
    let resolved = session_transcripts::resolve_native_channel_clear(pool, &key, generation).await;
    matches!(
        resolved,
        Ok(session_transcripts::NativeClearResolve::Resolved)
    )
}

/// Settles an unresolved native clear of `channel_id` under the intake's transition guard and
/// refreshes `state` from it; false keeps the input queued while the clear cannot be settled.
pub(in crate::services::discord) async fn native_clear_admits(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    state: &mut (Option<String>, bool, String),
    recovered_fresh: &mut bool,
) -> bool {
    // A Herdr channel's clear is native whatever the switch says, so its boundary settles here too.
    let herdr = || crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_some();
    let (allowed, effects) = switched_on(channel_id);
    if !allowed {
        return true;
    }
    let Some(effects) = effects.or_else(|| herdr().then(host_effects)) else {
        return true;
    };
    let Some(pool) = shared
        .pg_pool
        .as_ref()
        .filter(|_| *provider == ProviderKind::Claude)
    else {
        return true;
    };
    let key = channel_id.get().to_string();
    let boundary = match session_transcripts::native_channel_clear_state(pool, &key).await {
        Ok(boundary) => boundary,
        Err(error) => {
            tracing::warn!(channel_id = channel_id.get(), %error, "native clear state unreadable; input held");
            return false;
        }
    };
    let NativeClearBoundary::Unresolved { generation, ticket } = &boundary else {
        return true;
    };
    let generation = *generation;
    let tmux = {
        let data = shared.core.lock().await;
        let session = data.sessions.get(&channel_id);
        let name = session.and_then(|session| session.channel_name.as_deref());
        name.map(|name| provider.build_tmux_session_name(name))
    };
    let host = crate::services::tui_prompt_dedupe::binding_context::stable_host_identity();
    let verdict = judge_native_clear_admission(
        &boundary,
        host.as_deref(),
        channel_id.get(),
        tmux.as_deref(),
    );
    if verdict == NativeClearRestart::Hold {
        tracing::warn!(
            channel_id = channel_id.get(),
            "native clear undecidable; input held"
        );
        return false;
    }
    let resolved = super::resolve_session_key_for_clear(http, shared, channel_id, provider).await;
    let settled = match (&verdict, resolved, tmux) {
        (NativeClearRestart::Preserve, _, _) => return true,
        (NativeClearRestart::CompleteDurable(commit), Some(session_key), Some(tmux)) => {
            let composer_empty = async {
                let deadline = Instant::now() + NATIVE_ADMISSION_CAPTURE_BUDGET;
                if herdr() {
                    #[cfg(unix)]
                    {
                        use crate::services::tui_prompt_dedupe::native_clear::ClearTicket;
                        let Ok(ticket) = serde_json::from_value::<ClearTicket>(ticket.clone())
                        else {
                            return false;
                        };
                        return crate::services::session_host::recovery_composer_empty(
                            pool,
                            &session_key,
                            channel_id.get(),
                            &ticket,
                            deadline,
                        )
                        .await;
                    }
                    #[cfg(not(unix))]
                    return false;
                }
                effects.composer_empty(&tmux, deadline)
            };
            effects
                .save_selector(&session_key, &commit.session, channel_id)
                .await
                && composer_empty.await
                && settle(shared, pool, channel_id, generation, Some(commit)).await
        }
        (NativeClearRestart::ResetUnresolved, Some(session_key), Some(tmux)) => {
            let refusal = super::super::super::admin_host_guard::managed_reset_refusal;
            refusal(
                shared,
                provider,
                channel_id,
                true,
                false,
                Some(&session_key),
            )
            .await
            .is_none()
                && effects.clear_selector(&session_key).await
                && {
                    effects.reset_process(&tmux);
                    settle(shared, pool, channel_id, generation, None).await
                }
        }
        _ => false,
    };
    if settled {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            *recovered_fresh = std::mem::take(&mut session.cleared);
            state.0 = session.session_id.clone();
            state.1 = session.memento_context_loaded;
        }
    } else {
        tracing::warn!(
            channel_id = channel_id.get(),
            ?verdict,
            "native clear unsettled; input held"
        );
    }
    settled
}

#[cfg(test)]
#[path = "native_tests.rs"]
pub(in crate::services::discord) mod tests;
