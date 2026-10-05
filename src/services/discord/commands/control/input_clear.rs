//! Production effects of a ledger `/clear`: the existing managed reset of the ticket's execution
//! and the boundary ticket in Postgres. `/clear` does not select this path yet.

use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::super::super::SharedData;
use super::super::super::input_runtime::clear::{ClearHost, Execution, Identity, Step};
use crate::db::session_transcripts::{
    self, NativeClearGeneration, NativeClearRecord, NativeClearResolve,
};
use crate::services::provider::ProviderKind;
use crate::services::session_host::ClearedHostSession;
use crate::services::tmux_common;
use crate::services::tui_prompt_dedupe::binding_context::{self, SpawnNonceMarker};
use crate::services::tui_prompt_dedupe::{binding_events, native_clear::ClearTicket};

/// Provider, selector and Discord effects; tests replace them.
pub(super) trait ClearEffects: Send + Sync {
    fn clear_selector<'a>(&'a self, session_key: &'a str) -> Step<'a, bool>;
    fn reset_process(&self, tmux: &str);
    // A process or session still under the name; a failed probe counts as alive.
    fn alive(&self, tmux: &str) -> bool;
    fn notice<'a>(&'a self, channel: serenity::ChannelId, text: &'a str) -> Step<'a, ()>;
}

struct Production(Arc<serenity::Http>);

impl ClearEffects for Production {
    fn clear_selector<'a>(&'a self, session_key: &'a str) -> Step<'a, bool> {
        let clear = super::super::super::adk_session::clear_provider_session_id_checked;
        Box::pin(async move { clear(session_key).await.is_ok() })
    }
    fn reset_process(&self, tmux: &str) {
        super::reset_managed_process_session(tmux);
    }
    fn alive(&self, tmux: &str) -> bool {
        use crate::services::platform::tmux::{SessionPresence, session_presence};
        crate::services::session_backend::process_session_pid(tmux).is_some()
            || session_presence(tmux) != SessionPresence::Missing
    }
    fn notice<'a>(&'a self, channel: serenity::ChannelId, text: &'a str) -> Step<'a, ()> {
        Box::pin(async move {
            if let Err(error) = channel.say(&*self.0, text).await {
                tracing::warn!(channel_id = channel.get(), %error, "ledger clear notice failed");
            }
        })
    }
}

pub(super) struct LedgerClear {
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: serenity::ChannelId,
    tmux: String,
    session_key: String,
    cleared: ClearedHostSession,
    effects: Arc<dyn ClearEffects>,
}

impl LedgerClear {
    pub(super) async fn admit(
        http: &Arc<serenity::Http>,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: serenity::ChannelId,
        explicit_session_key: Option<&str>,
    ) -> Result<Self, String> {
        let effects = Arc::new(Production(http.clone()));
        Self::admit_with(
            http,
            shared,
            provider,
            channel_id,
            explicit_session_key,
            effects,
        )
        .await
    }

    /// The host guard admits the managed reset before any ticket is captured.
    pub(super) async fn admit_with(
        http: &Arc<serenity::Http>,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: serenity::ChannelId,
        explicit_session_key: Option<&str>,
        effects: Arc<dyn ClearEffects>,
    ) -> Result<Self, String> {
        // The ticket lives in Postgres; without it nothing may be cut or reset.
        if shared.pg_pool.is_none() {
            return Err(super::super::super::input_runtime::clear::PG_RETRY_NOTICE.to_owned());
        }
        let refusal = super::super::super::admin_host_guard::managed_reset_refusal;
        let target = explicit_session_key;
        if let Some(reason) = refusal(shared, provider, channel_id, true, false, target).await {
            return Err(reason);
        }
        let tmux = {
            let data = shared.core.lock().await;
            let session = data.sessions.get(&channel_id);
            let name = session.and_then(|session| session.channel_name.as_deref());
            name.map(|name| provider.build_tmux_session_name(name))
        }
        .ok_or("채널 세션을 찾지 못했어요")?;
        let resolved = super::resolve_session_key_for_clear(http, shared, channel_id, provider);
        let session_key = super::choose_clear_session_key(target, resolved.await)
            .ok_or("세션 키를 찾지 못했어요")?;
        let clear = super::super::super::inflight::clear_channel_session;
        let pool = shared.pg_pool.as_ref();
        let id = channel_id.get();
        let cleared = clear(
            pool,
            provider,
            id,
            Some(&session_key),
            &tmux,
            "ledger clear",
        )
        .await
        .ok_or("호스트 가드가 세션 초기화를 허용하지 않았어요")?;
        Ok(Self {
            shared: shared.clone(),
            provider: provider.clone(),
            channel_id,
            tmux,
            session_key,
            cleared,
            effects,
        })
    }

    fn pool(&self) -> anyhow::Result<&sqlx::PgPool> {
        let pool = self.shared.pg_pool.as_ref();
        pool.ok_or_else(|| anyhow::anyhow!("postgres pool is required for a ledger clear"))
    }
}

impl ClearHost for LedgerClear {
    fn identity(&self) -> Identity {
        Identity {
            provider: self.provider.as_str().to_owned(),
            channel: self.channel_id.get(),
            tmux: self.tmux.clone(),
            host: binding_context::stable_host_identity(),
        }
    }

    fn capture(&mut self) -> Option<ClearTicket> {
        let SpawnNonceMarker::Known(nonce) =
            binding_context::observe_spawn_nonce_marker(&self.tmux)
        else {
            return None;
        };
        let provider = self.provider.as_str();
        let context = binding_context::execution_context(provider, &nonce).ok()?;
        let channel = self.channel_id.get();
        tmux_common::try_with_tmux_source_authority(&self.tmux, |_| {
            let old = binding_events::pinned_source(channel, &self.tmux).ok()??;
            let records = binding_events::records_strict(channel).ok()?.ok()?;
            let baseline = records.last().map_or(0, |event| event.seq);
            Some(ClearTicket {
                context,
                old,
                baseline,
                input: None,
            })
        })?
    }

    fn record(&mut self) -> Step<'_, anyhow::Result<Option<NativeClearRecord>>> {
        Box::pin(async move {
            let key = self.channel_id.get().to_string();
            session_transcripts::native_channel_clear_record(self.pool()?, &key).await
        })
    }

    fn commit<'a>(
        &'a mut self,
        ticket: &'a serde_json::Value,
    ) -> Step<'a, anyhow::Result<NativeClearGeneration>> {
        Box::pin(async move {
            let key = self.channel_id.get().to_string();
            let tx = session_transcripts::begin_channel_clear_boundary_tx(self.pool()?).await?;
            session_transcripts::finish_native_channel_clear_boundary_tx(tx, &key, ticket).await
        })
    }

    fn execution(&mut self, ticket: &ClearTicket) -> Execution {
        match binding_context::observe_spawn_nonce_marker(&self.tmux) {
            SpawnNonceMarker::Known(n) if n == ticket.context.execution_nonce => Execution::Current,
            SpawnNonceMarker::Known(_) => Execution::Replaced,
            SpawnNonceMarker::Absent if self.effects.alive(&self.tmux) => Execution::Unknown,
            SpawnNonceMarker::Absent => Execution::Retired,
            SpawnNonceMarker::Unreadable => Execution::Unknown,
        }
    }

    // The selector goes first so a crash before the kill retries a still-current execution.
    fn reset<'a>(&'a mut self, ticket: &'a ClearTicket) -> Step<'a, bool> {
        Box::pin(async move {
            if !self.effects.clear_selector(&self.session_key).await {
                return false;
            }
            let nonce = ticket.context.execution_nonce.as_str();
            let cut = tmux_common::try_with_tmux_source_authority(&self.tmux, |authority| {
                let marker = binding_context::observe_spawn_nonce_marker(&self.tmux);
                matches!(marker, SpawnNonceMarker::Known(n) if n == nonce)
                    && tmux_common::cleanup_native_clear_fallback_under_source_authority(
                        authority,
                        &self.cleared,
                    )
            });
            if cut != Some(true) {
                return false;
            }
            self.effects.reset_process(self.cleared.name());
            super::native::clear_session_memory(&self.shared, self.channel_id).await;
            super::native::clear_process_reset_pending(&self.shared, self.channel_id).await;
            true
        })
    }

    fn resolve(
        &mut self,
        generation: NativeClearGeneration,
    ) -> Step<'_, anyhow::Result<NativeClearResolve>> {
        Box::pin(async move {
            let key = self.channel_id.get().to_string();
            session_transcripts::resolve_native_channel_clear(self.pool()?, &key, generation).await
        })
    }

    fn notice<'a>(&'a mut self, text: &'a str) -> Step<'a, ()> {
        self.effects.notice(self.channel_id, text)
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
#[path = "input_clear_tests.rs"]
mod tests;
