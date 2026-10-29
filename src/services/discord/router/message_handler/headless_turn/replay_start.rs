//! Headless starts keep their reservation, source and prepared side inputs across an exact retry.
use super::super::provider_dispatch::replay_scope;
use super::*;
use crate::db::replay_disposition::write::CanonicalInput;
use crate::services::cluster::attachment_transfer::uploads::PendingUploads;
use crate::services::discord::gateway::{HeadlessExactGateway, TurnGateway};
use crate::services::discord::replay_policy::permit::EffectTarget;
use crate::services::discord::replay_policy::{self, ExactHandoff, ExactRetry};
use crate::services::discord::router::intake_dispatch::replay_admission::{
    self as admission, PreparedEffectProjection, ReplayAdmissionContext, ReplayPreflight,
    ReplayStart, StartStop,
};

tokio::task_local! { static REENTRY: Arc<OwnedRequest>; }

pub(super) struct Request<'a> {
    pub ctx: &'a serenity::Context,
    pub shared: &'a Arc<SharedData>,
    pub provider: &'a ProviderKind,
    pub agent: Option<&'a str>,
    pub channel: ChannelId,
    pub text: &'a str,
    pub owner: UserId,
    pub owner_name: &'a str,
    pub token: &'a str,
    pub source: Option<&'a str>,
    pub metadata: Option<&'a serde_json::Value>,
    pub channel_hint: Option<&'a str>,
    pub tmux_label: Option<&'a str>,
    pub dm_hint: Option<bool>,
    pub reservation: HeadlessTurnReservation,
}

pub(super) struct Prepared<'a> {
    pub prompt: &'a str,
    pub cwd: &'a str,
    pub reply: Option<&'a str>,
    pub uploads: &'a PendingUploads,
    pub session_key: Option<&'a str>,
    pub remote: bool,
    pub tmux: Option<&'a str>,
}

impl Request<'_> {
    pub(super) async fn identity_changed(&self) -> bool {
        if retrying()
            || !routine_metadata_agent_id(self.metadata)
                .zip(self.agent)
                .is_some_and(|(metadata_agent, role)| metadata_agent == role)
        {
            return false;
        }
        let Some(hint) = self.channel_hint.filter(|value| !value.trim().is_empty()) else {
            return false;
        };
        self.shared
            .core
            .lock()
            .await
            .sessions
            .get(&self.channel)
            .is_some_and(|session| session.channel_name.as_deref() != Some(hint))
    }

    pub(super) async fn bind_inflight(
        &self,
        state: &mut InflightTurnState,
        token: &CancelToken,
        session_key: Option<&str>,
        runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    ) {
        state.turn_nonce = token.turn_nonce().map(str::to_owned);
        apply_prelaunch_runtime_kind(state, runtime_kind);
        let (worktree_path, worktree_branch, base_commit) = {
            let data = self.shared.core.lock().await;
            data.sessions
                .get(&self.channel)
                .and_then(|session| session.worktree.as_ref())
                .map(|wt| {
                    (
                        Some(wt.worktree_path.clone()),
                        Some(wt.branch_name.clone()),
                        crate::services::platform::git_head_commit(&wt.original_path),
                    )
                })
                .unwrap_or((None, None, None))
        };
        state.set_worktree_context(worktree_path, worktree_branch, base_commit);
        state.logical_channel_id = Some(self.channel.get());
        state.session_key = session_key.map(str::to_owned);
        state.delivery_bot = metadata_delivery_bot(self.metadata);
        state.silent_turn = metadata_silent_flag(self.metadata);
        state.source = metadata_turn_source(self.source, self.metadata);
        // An exact retry starts again with this request's metadata, so it keeps the origin.
        state.input_origin_ids =
            crate::services::agents::input_result::metadata_origin_id(self.metadata)
                .into_iter()
                .collect();
    }

    pub(super) async fn early(
        &self,
    ) -> Result<Option<HeadlessTurnStartOutcome>, HeadlessTurnStartError> {
        match self.preflight().await {
            Ok(()) => Ok(None),
            Err(StartStop::Consumed) => Ok(Some(HeadlessTurnStartOutcome::consumed(
                self.reservation.turn_id(self.channel),
            ))),
            Err(StartStop::Deferred(reason)) => Err(HeadlessTurnStartError::Conflict(reason)),
        }
    }

    pub(super) async fn isolate(
        &self,
        current_path: &mut String,
        session_id: &mut Option<String>,
        memento_context_loaded: &mut bool,
        session_strategy_reason: &mut &'static str,
    ) {
        if !retrying() {
            let channel_name_for_isolation = {
                let data = self.shared.core.lock().await;
                data.sessions
                    .get(&self.channel)
                    .and_then(|session| session.channel_name.clone())
                    .or_else(|| self.channel_hint.map(str::to_owned))
            };
            let isolation_outcome = ensure_provider_worktree_isolation(
                self.shared,
                self.channel,
                current_path,
                self.provider,
                channel_name_for_isolation.as_deref(),
                None,
            )
            .await;
            reset_provider_session_after_worktree_isolation(
                self.shared,
                self.channel,
                self.provider,
                isolation_outcome,
                session_id,
                memento_context_loaded,
                session_strategy_reason,
            )
            .await;
        }
    }

    pub(super) async fn pending_reset(
        &self,
        fast_mode_channel_id: ChannelId,
        session_id: &mut Option<String>,
        memento_context_loaded: &mut bool,
        session_strategy_reason: &mut &'static str,
    ) {
        if !retrying() {
            super::super::super::super::commands::reset_provider_session_if_pending(
                &self.ctx.http,
                self.shared,
                self.provider,
                self.channel,
                fast_mode_channel_id,
            )
            .await;
            refresh_session_strategy_after_pending_reset(
                self.shared,
                self.channel,
                session_id,
                memento_context_loaded,
                session_strategy_reason,
            )
            .await;
        }
    }

    pub(super) async fn checked_start(
        &self,
        state: &mut InflightTurnState,
        prepared: Prepared<'_>,
        session_id: &mut Option<String>,
        token: &Arc<CancelToken>,
    ) -> Result<Option<ReplayStart>, HeadlessTurnStartError> {
        let reply = prepared.reply;
        match self.admit(state, prepared).await {
            Ok((start, fresh)) => {
                if fresh {
                    *session_id = None;
                }
                if start.receipt_id().is_some() {
                    state.followup_reply_context = reply.map(str::to_owned);
                }
                Ok(Some(start))
            }
            Err(stop) => {
                unwind_unstarted_turn(self.shared, self.channel, token).await;
                match stop {
                    StartStop::Consumed => Ok(None),
                    StartStop::Deferred(reason) => Err(HeadlessTurnStartError::Conflict(reason)),
                }
            }
        }
    }

    fn key(&self) -> String {
        format!(
            "headless:{}:{}:{}",
            self.provider.as_str(),
            self.channel,
            self.reservation.turn_id(self.channel)
        )
    }

    fn input(&self) -> CanonicalInput {
        CanonicalInput {
            request_key: self.key(),
            ..admission::discord_input(
                self.provider.as_str(),
                self.channel.get(),
                self.reservation.user_msg_id.get(),
                [],
                self.text,
            )
        }
    }

    async fn preflight(&self) -> Result<(), StartStop> {
        let channel = self.channel.to_string();
        let requested = !crate::services::discord::router::replay_producers_dormant(&channel);
        if retrying() {
            return if REENTRY
                .with(|old| old.provider == *self.provider && old.agent.as_deref() == self.agent)
            {
                Ok(())
            } else {
                Err(StartStop::Deferred("headless exact owner changed".into()))
            };
        }
        let (name, remote) = self
            .shared
            .core
            .lock()
            .await
            .sessions
            .get(&self.channel)
            .map(|session| (session.channel_name.clone(), false))
            .unwrap_or_default();
        let tmux = routine_tmux_session_name(
            self.provider,
            self.metadata,
            self.tmux_label,
            name.as_deref().or(self.channel_hint),
        );
        let scope =
            replay_scope::selected(self.provider, self.channel.get(), remote, tmux.as_deref());
        if scope.driver == crate::services::provider_hosting::ProviderSessionDriver::TuiHosting {
            return Ok(());
        }
        if self.shared.pg_pool.is_none()
            && (!requested
                || !replay_policy::activation::supported(
                    self.provider.as_str(),
                    scope.driver.as_str(),
                    scope.runtime,
                ))
        {
            return Ok(());
        }
        if requested
            && replay_policy::activation::supported(
                self.provider.as_str(),
                scope.driver.as_str(),
                scope.runtime,
            )
            && let Some(turn) = self.metadata.and_then(|value| value.get("turn_id"))
            && turn.as_str() != Some(self.reservation.turn_id(self.channel).as_str())
        {
            return Err(StartStop::Deferred(
                "headless reservation differs from its durable turn identity".into(),
            ));
        }
        let pool = self
            .shared
            .pg_pool
            .as_ref()
            .ok_or_else(|| StartStop::Deferred("no replay store".into()))?;
        let instance =
            crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
        let context = ReplayAdmissionContext::production(pool, &instance, admission::incarnation())
            .for_scope(
                &channel,
                self.provider.as_str(),
                scope.driver.as_str(),
                scope.runtime,
                false,
            );
        match admission::preflight_source(&context, &self.input()).await {
            ReplayPreflight::Admit(_) | ReplayPreflight::LegacyUnprotected => Ok(()),
            ReplayPreflight::ConsumeProtected(_) => Err(StartStop::Consumed),
            other => Err(StartStop::Deferred(format!(
                "{other:?}; retry with the original reservation"
            ))),
        }
    }

    pub(super) async fn admit(
        &self,
        inflight: &mut InflightTurnState,
        prepared: Prepared<'_>,
    ) -> Result<(ReplayStart, bool), StartStop> {
        let channel = self.channel.to_string();
        let continuation = replay_policy::take_continuation();
        let scope = replay_scope::selected(
            self.provider,
            self.channel.get(),
            prepared.remote,
            prepared.tmux,
        );
        let supported = replay_policy::activation::supported(
            self.provider.as_str(),
            scope.driver.as_str(),
            scope.runtime,
        );
        if !supported && continuation.is_some() {
            return Err(StartStop::Consumed);
        }
        if continuation.is_none() && (!supported || !replay_policy::activation::requested(&channel))
        {
            let Some(pool) = self.shared.pg_pool.as_ref() else {
                return Ok((
                    admission::dormant_start_for_scope(
                        &channel,
                        self.provider.as_str(),
                        scope.driver.as_str(),
                        scope.runtime,
                    )
                    .ok_or(StartStop::Consumed)?,
                    false,
                ));
            };
            let ctx = ReplayAdmissionContext::production(pool, "", admission::incarnation());
            return admission::admit_start(
                &ctx,
                &self.input(),
                PreparedEffectProjection {
                    target: EffectTarget::ProviderStart {
                        provider: self.provider.as_str().into(),
                        channel,
                    },
                    input_hash: admission::input_hash(prepared.prompt),
                    binding: serde_json::json!({}),
                    session_key: None,
                },
                None,
            )
            .await
            .map(|s| (s, false));
        }
        let session_key = prepared
            .session_key
            .ok_or_else(|| StartStop::Deferred("headless session is unbound".into()))?;
        if REENTRY
            .try_with(|old| old.session_key.as_str() != session_key || old.cwd != prepared.cwd)
            .unwrap_or(false)
        {
            return Err(StartStop::Deferred(
                "headless exact session routing or cwd changed".into(),
            ));
        }
        let pool = self
            .shared
            .pg_pool
            .as_ref()
            .ok_or_else(|| StartStop::Deferred("no replay store".into()))?;
        let bound: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE session_key=$1)")
                .bind(session_key)
                .fetch_one(pool)
                .await
                .map_err(|error| StartStop::Deferred(error.to_string()))?;
        if !bound {
            return Err(StartStop::Deferred(
                "headless session binding was not acknowledged".into(),
            ));
        }
        let uploads: Vec<String> = prepared
            .uploads
            .iter()
            .map(|upload| upload.history_record())
            .collect();
        let input = CanonicalInput {
            owner_id: self.owner.to_string(),
            agent_id: self.agent.unwrap_or_default().into(),
            attachments: serde_json::json!(uploads),
            reply_context: prepared.reply.map(str::to_owned),
            provenance: if continuation.is_some() {
                "reentry"
            } else {
                "new_input"
            },
            ..self.input()
        };
        let projection = PreparedEffectProjection {
            target: EffectTarget::ProviderStart {
                provider: self.provider.as_str().into(),
                channel,
            },
            input_hash: admission::input_hash(prepared.prompt),
            session_key: Some(session_key.into()),
            binding: serde_json::json!({"turn_nonce":inflight.turn_nonce, "user_msg_id":inflight.user_msg_id,
                "placeholder_msg_id":inflight.current_msg_id, "turn_id":self.reservation.turn_id(self.channel),
                "source":self.source, "metadata":self.metadata, "request_owner_name":self.owner_name,
                "channel_name_hint":self.channel_hint, "tmux_session_label":self.tmux_label,
                "is_dm_hint":self.dm_hint, "session_key":session_key, "cwd":prepared.cwd,
                "driver_scope":scope.driver.as_str(), "runtime_scope":scope.runtime}),
        };
        super::super::intake_turn::replay_start::common::begin(
            self.shared,
            self.provider,
            inflight,
            input,
            projection,
            continuation,
            scope,
        )
        .await
    }

    pub(super) fn gateway(
        &self,
        inflight: &InflightTurnState,
        uploads: &PendingUploads,
        cwd: &str,
    ) -> Arc<dyn TurnGateway> {
        let (Some(receipt), Some(session_key)) =
            (inflight.replay_receipt_id, inflight.session_key.as_ref())
        else {
            return Arc::new(HeadlessGateway);
        };
        let owned = Arc::new(OwnedRequest {
            ctx: self.ctx.clone(),
            shared: self.shared.clone(),
            provider: self.provider.clone(),
            agent: self.agent.map(str::to_owned),
            channel: self.channel,
            text: self.text.into(),
            owner: self.owner,
            owner_name: self.owner_name.into(),
            token: self.token.into(),
            source: self.source.map(str::to_owned),
            metadata: self.metadata.cloned(),
            channel_hint: self.channel_hint.map(str::to_owned),
            tmux_label: self.tmux_label.map(str::to_owned),
            dm_hint: self.dm_hint,
            reservation: self.reservation,
            receipt,
            session_key: session_key.clone(),
            cwd: cwd.into(),
            reply: inflight.followup_reply_context.clone(),
            uploads: uploads.clone(),
            sources: inflight.source_message_ids.clone(),
        });
        Arc::new(HeadlessExactGateway::new(move |channel, retry| {
            let owned = Arc::clone(&owned);
            Box::pin(async move { owned.retry(channel, retry).await })
        }))
    }
}

struct OwnedRequest {
    ctx: serenity::Context,
    shared: Arc<SharedData>,
    provider: ProviderKind,
    agent: Option<String>,
    channel: ChannelId,
    text: String,
    owner: UserId,
    owner_name: String,
    token: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
    channel_hint: Option<String>,
    tmux_label: Option<String>,
    dm_hint: Option<bool>,
    reservation: HeadlessTurnReservation,
    receipt: i64,
    session_key: String,
    cwd: String,
    reply: Option<String>,
    uploads: PendingUploads,
    sources: Vec<u64>,
}

impl OwnedRequest {
    async fn retry(self: Arc<Self>, channel: ChannelId, retry: ExactRetry) -> ExactHandoff {
        let mut sources: Vec<_> = retry.sources.iter().map(|id| id.get()).collect();
        let mut expected = self.sources.clone();
        sources.sort_unstable();
        sources.dedup();
        expected.sort_unstable();
        expected.dedup();
        if channel != self.channel
            || retry.user_message_id != self.reservation.user_msg_id
            || retry.request_owner != self.owner
            || retry.text != self.text
            || retry.reply_context != self.reply
            || sources != expected
            || retry.permit.receipt_id() != self.receipt
        {
            return ExactHandoff::Refused;
        }
        if !wait_for_retry_owner(
            &self.shared,
            &self.provider,
            channel,
            self.receipt,
            self.reservation.user_msg_id.get(),
        )
        .await
        {
            return ExactHandoff::Refused;
        }
        let context = Arc::clone(&self);
        REENTRY
            .scope(context, async {
                let start = super::start_reserved_headless_turn_with_owner(
                    &self.ctx,
                    channel,
                    &self.text,
                    &self.owner_name,
                    self.owner,
                    &self.shared,
                    &self.token,
                    self.source.as_deref(),
                    self.metadata.clone(),
                    self.channel_hint.clone(),
                    self.tmux_label.clone(),
                    self.dm_hint,
                    self.reservation,
                );
                let (result, handoff) = replay_policy::hand_over(retry.permit, start).await;
                if let Err(error) = result {
                    tracing::warn!(%error, "headless exact start refused");
                }
                handoff
            })
            .await
    }
}

/// Waits for the previous owner to retire; beginning still needs the exact permit.
pub(super) async fn wait_for_retry_owner(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    receipt: i64,
    message: u64,
) -> bool {
    #[cfg(test)]
    replay_policy::activation::record_read(&format!("headless_retry_wait:{channel}"));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let busy = crate::services::discord::mailbox_snapshot(shared, channel)
            .await
            .cancel_token
            .is_some();
        let prior =
            crate::services::discord::inflight::load_inflight_state(provider, channel.get());
        if prior.as_ref().is_some_and(|prior| {
            prior.replay_receipt_id != Some(receipt) || prior.user_msg_id != message
        }) {
            return false;
        }
        if !busy && prior.is_none() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

pub(super) fn retrying() -> bool {
    REENTRY.try_with(|_| ()).is_ok()
}

pub(super) fn reply(current: Option<String>) -> Option<String> {
    REENTRY.try_with(|old| old.reply.clone()).unwrap_or(current)
}

pub(super) fn take_uploads(current: &mut PendingUploads) -> PendingUploads {
    REENTRY
        .try_with(|old| old.uploads.clone())
        .unwrap_or_else(|_| std::mem::take(current))
}

pub(super) fn take_session_retry_context(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    turn: Option<&str>,
) -> Option<super::super::super::turn_start::FormattedSessionRetryContext> {
    if retrying() {
        None
    } else {
        super::super::take_session_retry_context(shared, channel, turn)
    }
}
