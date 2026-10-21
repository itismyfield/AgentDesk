//! What an HTTP session force-kill of a Herdr execution applies once the terminate retired it:
//! the disconnect and the dispatch failure or retry while the exclusion is held, then the drain
//! of the queue it kept and the auto-queue wake once the exclusion is gone.

use futures::FutureExt;
use futures::future::BoxFuture;
use sqlx::PgPool;

use super::super::force_kill::{activates_auto_queue, fail_or_retry};
use super::super::spawn_auto_queue_activate_for_agent;
use crate::app_state::AppState;
use crate::db::dispatched_sessions::prepare_force_kill_retry_pg;
use crate::services::discord::TmuxCleanupPolicy;
use crate::services::discord::herdr_terminate::RetiredExclusion;
use crate::services::discord::herdr_terminate::http_force::Released;
use crate::services::queue::schedule_post_cancel_queue_drain;
use crate::services::turn_cancel_finalizer::{
    FinalizeTurnCancelRequest, TurnCancelCorrelation, finalize_turn_cancel,
};
use crate::services::turn_lifecycle::{TurnLifecycleTarget, cleanup_policy_observability_surface};

pub(super) const HERDR_TERMINATE: &str = "herdr-terminate";

pub(super) struct KillInput {
    pub(super) state: AppState,
    pub(super) pool: PgPool,
    pub(super) session_key: String,
    pub(super) agent_id: Option<String>,
    pub(super) retry: bool,
    pub(super) reason: String,
}

/// What a forced Herdr session kill applied. `failed` names the first step that did not apply;
/// nothing after it ran, the drain and the auto-queue wake included.
pub(super) struct HerdrKillEffects {
    pub(super) failed: Option<String>,
    pub(super) inflight_cleared: bool,
    pub(super) retry_dispatch_id: Option<String>,
    pub(super) retry_skipped_reason: Option<&'static str>,
    pub(super) activation_requested: bool,
    pub(super) queued_remaining: Option<usize>,
    release: Option<Release>,
}

/// The released step's inputs: the channel whose kept queue drains and the agent it may wake.
struct Release {
    state: AppState,
    target: TurnLifecycleTarget,
    agent_id: Option<String>,
}

/// Disconnects the retired row, then fails the bound dispatch or creates its retry; the exclusion
/// is held throughout and the queue is left as it is.
pub(super) async fn herdr_force_kill_effects(
    exclusion: &RetiredExclusion,
    input: KillInput,
) -> HerdrKillEffects {
    let KillInput {
        state,
        pool,
        session_key,
        agent_id,
        retry,
        reason,
    } = input;
    let steps = kill_steps(exclusion, &pool, &session_key, retry).await;
    let turn_id = exclusion.settled_turn().and_then(|t| t.turn_nonce());
    finalize_turn_cancel(FinalizeTurnCancelRequest {
        correlation: TurnCancelCorrelation {
            provider: Some(exclusion.provider().clone()),
            channel_id: Some(exclusion.channel()),
            dispatch_id: exclusion.dispatch().map(str::to_string),
            session_key: Some(session_key),
            turn_id: turn_id.map(str::to_string),
        },
        reason,
        surface: cleanup_policy_observability_surface(TmuxCleanupPolicy::CleanupSession {
            termination_reason_code: None,
        })
        .into(),
        lifecycle_path: HERDR_TERMINATE.into(),
        tmux_killed: false,
        inflight_cleared: exclusion.settled_turn().is_some(),
        queue_depth: None,
        queue_preserved: Some(true),
        termination_recorded: true,
        completed_at: chrono::Utc::now(),
    });
    let inflight_cleared = exclusion.settled_turn().is_some();
    let (retry_dispatch_id, retry_skipped_reason) = match steps {
        Ok(outcome) => outcome,
        Err(failed) => {
            return HerdrKillEffects {
                failed: Some(failed),
                inflight_cleared,
                retry_dispatch_id: None,
                retry_skipped_reason: None,
                activation_requested: false,
                queued_remaining: None,
                release: None,
            };
        }
    };
    let wakes = activates_auto_queue(retry_dispatch_id.as_deref(), retry_skipped_reason);
    let agent_id = agent_id.filter(|_| wakes);
    let target = TurnLifecycleTarget {
        provider: Some(exclusion.provider().clone()),
        channel_id: Some(exclusion.channel()),
        tmux_name: String::new(),
    };
    HerdrKillEffects {
        failed: None,
        inflight_cleared,
        retry_dispatch_id,
        retry_skipped_reason,
        activation_requested: agent_id.is_some(),
        queued_remaining: None,
        release: Some(Release {
            state,
            target,
            agent_id,
        }),
    }
}

async fn kill_steps(
    exclusion: &RetiredExclusion,
    pool: &PgPool,
    session_key: &str,
    retry: bool,
) -> Result<(Option<String>, Option<&'static str>), String> {
    match exclusion.disconnect_session_keeping_provider(pool).await {
        Ok(true) => {}
        Ok(false) => return Err("session row moved on".into()),
        Err(error) => return Err(error),
    }
    let dispatch = exclusion.dispatch();
    let meta = prepare_force_kill_retry_pg(pool, session_key, dispatch, retry).await?;
    fail_or_retry(pool, session_key, meta).await
}

/// Once the channel is no longer excluded its kept queue drains now, not at the slow backstop,
/// and the agent's auto-queue wakes as the legacy force-kill wakes it.
impl Released for HerdrKillEffects {
    fn released(mut self) -> BoxFuture<'static, Self> {
        async move {
            let Some(Release {
                state,
                target,
                agent_id,
            }) = self.release.take()
            else {
                return self;
            };
            let registry = state.health_registry.as_ref();
            let reason = "force_kill_session_herdr";
            self.queued_remaining =
                schedule_post_cancel_queue_drain(registry, &target, reason).await;
            if let Some(agent_id) = agent_id {
                spawn_auto_queue_activate_for_agent(state, agent_id);
            }
            self
        }
        .boxed()
    }
}
