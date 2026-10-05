//! Claude native `/clear` behind the `native_clear_enabled` runtime switch: the live clear keeps
//! its pane and the next admission settles a clear that a restart left unresolved.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use poise::serenity_prelude as serenity;
use tokio::sync::{OwnedMutexGuard, watch};
use tokio::time::Instant;

use super::super::super::SharedData;
use crate::db::session_transcripts::{self, NativeClearBoundary, NativeClearGeneration};
use crate::services::claude_tui::host_input::{InputTarget, NativeClearSubmission};
use crate::services::provider::ProviderKind;
use crate::services::session_host::ClearedHostSession;
use crate::services::tui_prompt_dedupe::native_clear::{
    CanonicalClearWaiter, ClearAdmission, ClearCommit, ClearDecision, ClearOutcome, ClearTicket,
    NativeClearHost, NativeClearRestart, judge_native_clear_admission, start_native_clear,
};

type Effect<'a> = Pin<Box<dyn Future<Output = bool> + Send + 'a>>;

/// The provider, pane and selector effects of a native clear; tests replace them per thread.
pub(super) trait NativeClearEffects: Send + Sync {
    fn clear_selector<'a>(&'a self, session_key: &'a str) -> Effect<'a>;
    fn save_selector<'a>(
        &'a self,
        key: &'a str,
        session: &'a str,
        ch: serenity::ChannelId,
    ) -> Effect<'a>;
    fn submit(&self, ticket: &ClearTicket, deadline: Instant) -> NativeClearSubmission;
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
    fn submit(&self, ticket: &ClearTicket, deadline: Instant) -> NativeClearSubmission {
        let target = InputTarget::legacy_tmux(&ticket.context.tmux_session);
        crate::services::claude_tui::input::submit_native_clear(&target, ticket, deadline)
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
    static TEST_EFFECTS: std::cell::RefCell<Option<Arc<dyn NativeClearEffects>>> =
        const { std::cell::RefCell::new(None) };
}

/// Installs `effects` as this thread's switched-on native clear until the guard drops.
#[cfg(test)]
pub(super) fn switch_on_for_tests(effects: Arc<dyn NativeClearEffects>) -> impl Drop {
    struct Off;
    impl Drop for Off {
        fn drop(&mut self) {
            TEST_EFFECTS.with(|cell| cell.borrow_mut().take());
        }
    }
    TEST_EFFECTS.with(|cell| *cell.borrow_mut() = Some(effects));
    Off
}

/// The switch, read before any other native-clear work; `None` keeps main's behavior.
fn switched_on() -> Option<Arc<dyn NativeClearEffects>> {
    #[cfg(test)]
    if let Some(effects) = TEST_EFFECTS.with(|cell| cell.borrow().clone()) {
        return Some(effects);
    }
    let config = crate::config_live_reload::current()?;
    config
        .runtime
        .native_clear_enabled
        .unwrap_or(false)
        .then(|| Arc::new(Production) as Arc<dyn NativeClearEffects>)
}

/// A clear whose target, host clearance and canonical baseline were all captured before its
/// boundary is written.
pub(super) enum NativeSelection {
    Tmux(TmuxSelection),
    /// A Herdr pane's planned clear and the session key whose selector it clears and saves.
    #[cfg(unix)]
    Herdr(Box<crate::services::session_host::HerdrClearPlan>, String),
}

impl NativeSelection {
    fn ticket(&self) -> &ClearTicket {
        match self {
            Self::Tmux(selection) => &selection.waiter.ticket,
            #[cfg(unix)]
            Self::Herdr(plan, _) => &plan.waiter().ticket,
        }
    }
}

/// A clear's target, judged before it changes anything: a host's planned clear, `None` for main's
/// tmux path, or main's refusal message.
pub(super) async fn target(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    explicit_session_key: Option<&str>,
) -> anyhow::Result<Option<NativeSelection>> {
    use super::super::super::admin_host_guard::{HostAdapter, ResetTarget, clear_reset_target};
    let key = || async {
        let resolved = super::resolve_session_key_for_clear(http, shared, channel_id, provider);
        super::choose_clear_session_key(explicit_session_key, resolved.await)
    };
    let judged = clear_reset_target(shared, provider, channel_id, explicit_session_key, key);
    match judged.await {
        ResetTarget::Refused(reason) => anyhow::bail!("세션을 초기화하지 못했어요: {reason}"),
        ResetTarget::LegacyTmux => Ok(None),
        ResetTarget::NativeClear(adapter) => match adapter {
            #[cfg(unix)]
            HostAdapter::Herdr(plan, key) => Ok(Some(NativeSelection::Herdr(plan, key))),
        },
    }
}

/// A host clear cannot stop a running turn, so it is refused under the transition guard before
/// the queue is touched; an unreadable mailbox counts as busy.
pub(super) async fn refuse_a_running_turn(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    hosted: Option<&NativeSelection>,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    if let Some(NativeSelection::Herdr(..)) = hosted {
        let probe = super::super::super::mailbox_probe::mailbox_has_active_turn_or_unreachable;
        if probe(shared, channel_id).await {
            let refusal = crate::services::session_host::HerdrClearRefusal::TurnInProgress;
            anyhow::bail!("세션을 초기화하지 못했어요: {refusal}");
        }
    }
    #[cfg(not(unix))]
    let _ = (shared, channel_id, hosted);
    Ok(())
}

pub(super) struct TmuxSelection {
    effects: Arc<dyn NativeClearEffects>,
    waiter: CanonicalClearWaiter,
    cleared: ClearedHostSession,
    session_key: String,
}

/// A selected clear whose ticket committed with its boundary under `generation`.
pub(super) struct ArmedClear(NativeSelection, NativeClearGeneration);

/// Commits the clear boundary; a selected clear also records its ticket in the same statement,
/// and anything else writes the boundary exactly as before.
pub(super) async fn finish_boundary(
    boundary: Option<sqlx::Transaction<'_, sqlx::Postgres>>,
    channel_key: &str,
    native: Option<NativeSelection>,
) -> anyhow::Result<Option<ArmedClear>> {
    match (boundary, native) {
        (Some(tx), Some(selection)) => {
            let ticket = serde_json::to_value(selection.ticket())?;
            let finish = session_transcripts::finish_native_channel_clear_boundary_tx;
            let generation = finish(tx, channel_key, &ticket).await?;
            Ok(Some(ArmedClear(selection, generation)))
        }
        (Some(tx), None) => session_transcripts::finish_channel_clear_boundary_tx(tx, channel_key)
            .await
            .map(|()| None),
        (None, _) => Ok(None),
    }
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

/// A host's planned clear as it is; otherwise native clear applies only to an O-owned Claude tmux
/// channel whose running execution is fully identified, and any miss keeps the managed reset.
pub(super) async fn select(
    hosted: Option<NativeSelection>,
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    tmux: Option<&str>,
    explicit_session_key: Option<&str>,
) -> Option<NativeSelection> {
    if hosted.is_some() {
        return hosted;
    }
    let effects = switched_on()?;
    let tmux = tmux?;
    let owned = crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel_tmux;
    if *provider != ProviderKind::Claude || owned(channel_id.get(), Some(tmux)) != Ok(true) {
        return None;
    }
    let resolved = super::resolve_session_key_for_clear(http, shared, channel_id, provider).await;
    let session_key = super::choose_clear_session_key(explicit_session_key, resolved)?;
    let clear = super::super::super::inflight::clear_channel_session;
    let pool = shared.pg_pool.as_ref();
    let caller = "native clear";
    let cleared = clear(
        pool,
        provider,
        channel_id.get(),
        Some(&session_key),
        tmux,
        caller,
    );
    let cleared = cleared.await?;
    let capture = crate::services::tui_prompt_dedupe::native_clear::capture_live_clear;
    let waiter = capture(channel_id.get(), tmux)?;
    Some(NativeSelection::Tmux(TmuxSelection {
        effects,
        waiter,
        cleared,
        session_key,
    }))
}

/// Runs the selected clear; the worker owns `guard` until the outcome and its completion mark are
/// durable, so a dropped caller neither releases admission nor skips the mark.
pub(super) async fn complete(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    ArmedClear(selection, generation): ArmedClear,
    guard: OwnedMutexGuard<()>,
) -> anyhow::Result<()> {
    let Some(pool) = shared.pg_pool.clone() else {
        anyhow::bail!("postgres pool is required to complete a native clear");
    };
    let outcome = match selection {
        NativeSelection::Tmux(selection) => {
            let host = LiveClear {
                shared: shared.clone(),
                pool,
                channel_id,
                generation,
                selection,
            };
            start_native_clear(host, ClearAdmission::Native, guard).await
        }
        #[cfg(unix)]
        NativeSelection::Herdr(plan, session_key) => {
            let session = HerdrSession {
                effects: switched_on().unwrap_or_else(|| Arc::new(Production)),
                shared: shared.clone(),
                pool,
                channel_id,
                generation,
                session_key,
            };
            let host = crate::services::session_host::HerdrClear::new(*plan, session);
            start_native_clear(host, ClearAdmission::Native, guard).await
        }
    };
    match outcome {
        Ok(ClearOutcome::Native(_) | ClearOutcome::Fallback) => Ok(()),
        outcome => anyhow::bail!(
            "세션을 초기화하지 못했어요: clear 결과를 확정하지 못해 다음 입력을 보류했어요 ({outcome:?})"
        ),
    }
}

struct LiveClear {
    shared: Arc<SharedData>,
    pool: sqlx::PgPool,
    channel_id: serenity::ChannelId,
    generation: NativeClearGeneration,
    selection: TmuxSelection,
}

/// A Herdr clear's channel session: the tmux clear's selector effects and settle.
#[cfg(unix)]
struct HerdrSession {
    effects: Arc<dyn NativeClearEffects>,
    shared: Arc<SharedData>,
    pool: sqlx::PgPool,
    channel_id: serenity::ChannelId,
    generation: NativeClearGeneration,
    session_key: String,
}

#[cfg(unix)]
impl crate::services::session_host::ClearSession for HerdrSession {
    fn clear_selector(&mut self) -> Effect<'_> {
        self.effects.clear_selector(&self.session_key)
    }
    fn save(&mut self, commit: ClearCommit) -> Effect<'_> {
        Box::pin(async move {
            let (key, channel_id) = (&self.session_key, self.channel_id);
            let saved = self.effects.save_selector(key, &commit.session, channel_id);
            let generation = self.generation;
            saved.await
                && settle(
                    &self.shared,
                    &self.pool,
                    channel_id,
                    generation,
                    Some(&commit),
                )
                .await
        })
    }
}

impl NativeClearHost for LiveClear {
    fn changes(&self) -> watch::Receiver<u64> {
        self.selection.waiter.changed.clone()
    }
    // The durable selector is cleared before `/clear` is typed, so a crash never resumes it.
    fn prepare(&mut self, _deadline: Instant) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        let selection = &self.selection;
        selection.effects.clear_selector(&selection.session_key)
    }
    fn submit(&mut self, deadline: Instant) -> NativeClearSubmission {
        let selection = &self.selection;
        selection.effects.submit(&selection.waiter.ticket, deadline)
    }
    fn decide(&mut self, allow_fallback: bool) -> ClearDecision {
        match allow_fallback {
            true => self.selection.waiter.fallback_cut(&self.selection.cleared),
            false => self.selection.waiter.probe(),
        }
    }
    fn composer_empty(&mut self, deadline: Instant) -> bool {
        let tmux = &self.selection.waiter.ticket.context.tmux_session;
        self.selection.effects.composer_empty(tmux, deadline)
    }
    fn save(
        &mut self,
        commit: ClearCommit,
        _deadline: Instant,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            let (shared, channel_id) = (&self.shared, self.channel_id);
            let selection = &self.selection;
            let key = &selection.session_key;
            selection
                .effects
                .save_selector(key, &commit.session, channel_id)
                .await
                && settle(
                    shared,
                    &self.pool,
                    channel_id,
                    self.generation,
                    Some(&commit),
                )
                .await
        })
    }
    fn fallback(&mut self, _deadline: Instant) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            let selection = &self.selection;
            if !selection
                .effects
                .clear_selector(&selection.session_key)
                .await
            {
                return false;
            }
            selection.effects.reset_process(selection.cleared.name());
            settle(
                &self.shared,
                &self.pool,
                self.channel_id,
                self.generation,
                None,
            )
            .await
        })
    }
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
) -> bool {
    // A Herdr channel's clear is native whatever the switch says, so its boundary settles here too.
    let herdr = || crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_some();
    let production = || Arc::new(Production) as Arc<dyn NativeClearEffects>;
    let Some(effects) = switched_on().or_else(|| herdr().then(production)) else {
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
    let NativeClearBoundary::Unresolved { generation, .. } = &boundary else {
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
        (NativeClearRestart::CompleteDurable(commit), Some(session_key), _) => {
            effects
                .save_selector(&session_key, &commit.session, channel_id)
                .await
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
        let data = shared.core.lock().await;
        if let Some(session) = data.sessions.get(&channel_id) {
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
mod tests;
