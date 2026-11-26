//! Idle-kill's end of a Bound Herdr execution: nothing closes while a turn, an inflight row or an
//! active dispatch holds the channel, and the provider session is kept for the next message.
use std::sync::{Arc, Mutex, PoisonError};

use sqlx::{PgPool, Postgres, Transaction};

use super::{
    ChannelId, HealthRegistry, NeverStartedTerminate, OperatorTerminate, SharedData,
    TerminationResult, close_unrecorded, load, retire_confirmed, terminate_enabled,
    terminate_scoped,
};
use crate::db::dispatched_sessions::hosted_execution::http_force::{
    active_dispatch_pg, disconnect_retired_pg,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedObservation, HostedRecord, HostedState,
};
use crate::services::provider::ProviderKind;
use crate::services::session_host::{HerdrTarget, TerminationFence};
use crate::services::turn_orchestrator::Expected;

/// An end no operator asked for; either kind refuses any turn holding the channel before a fence.
pub(super) enum AutoEnd<'a> {
    NeverStarted(&'a NeverStartedTerminate),
    Idle(IdleGate),
}

impl AutoEnd<'_> {
    pub(super) fn refuses(&self, record: &HostedExecution) -> Option<&'static str> {
        match self {
            Self::NeverStarted(asked) => {
                (!asked.admits(record)).then_some("execution is not a never-started pending")
            }
            Self::Idle(_) => {
                (record.state != HostedState::Bound).then_some("execution is not bound")
            }
        }
    }

    /// The idle end takes its row reservation and re-reads the inflight row under the transition
    /// lock, then shuts the fence.
    pub(super) async fn fence(
        &self,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel: ChannelId,
        record: &HostedExecution,
        target: &HerdrTarget,
    ) -> Result<TerminationFence, TerminationResult> {
        use TerminationResult::Refused;
        match self {
            Self::NeverStarted(asked) => asked.fence(shared, channel, record, target).await,
            Self::Idle(gate) => {
                gate.reserve().await?;
                if inflight_open(provider, channel) {
                    return Err(Refused(INFLIGHT_OPEN.into()));
                }
                target
                    .termination_fence()
                    .map_err(|error| Refused(format!("{error:?}")))
            }
        }
    }

    /// Lets a waiting dispatch write through once the pane's end was read, before the retire.
    pub(super) async fn release(&self) {
        if let Self::Idle(gate) = self {
            gate.release().await;
        }
    }
}

/// The sessions row lock an idle end holds from its last idle and dispatch read until the pane's
/// end is read: a dispatch written earlier refuses the end, one written later waits for it.
pub(super) struct IdleGate {
    pool: PgPool,
    row_id: i64,
    minutes: i32,
    held: Mutex<Option<Transaction<'static, Postgres>>>,
}

impl IdleGate {
    async fn reserve(&self) -> Result<(), TerminationResult> {
        use TerminationResult::Refused;
        let failed = |error: sqlx::Error| Refused(format!("row reservation: {error}"));
        let mut tx = self.pool.begin().await.map_err(failed)?;
        let read: Option<(Option<String>, bool)> = sqlx::query_as(
            "SELECT active_dispatch_id,
                    COALESCE(status = 'idle' AND COALESCE(last_heartbeat, created_at)
                             < NOW() - make_interval(mins => $2), FALSE)
             FROM sessions WHERE id = $1 FOR UPDATE",
        )
        .bind(self.row_id)
        .bind(self.minutes)
        .fetch_optional(&mut *tx)
        .await
        .map_err(failed)?;
        match read {
            Some((None, true)) => {}
            Some((Some(_), _)) => return Err(Refused("active dispatch".into())),
            _ => return Err(Refused("row no longer idle".into())),
        }
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
        Ok(())
    }

    async fn release(&self) {
        let held = self
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(tx) = held
            && let Err(error) = tx.commit().await
        {
            tracing::warn!(row_id = self.row_id, %error, "herdr idle end: reservation commit");
        }
    }
}

impl Drop for IdleGate {
    /// A run that stopped before its release gives the row back without leaving it locked.
    fn drop(&mut self) {
        let held = self.held.get_mut().unwrap_or_else(PoisonError::into_inner);
        if let Some(tx) = held.take() {
            tokio::spawn(async move { drop(tx.rollback().await) });
        }
    }
}

const INFLIGHT_OPEN: &str = "inflight row open";

/// How long after boot a row whose owning bot registered no runtime counts as orphaned.
const ORPHAN_AFTER_BOOT_SECS: i64 = 15 * 60;

/// An unreadable inflight row is an open one.
fn inflight_open(provider: &ProviderKind, channel: ChannelId) -> bool {
    let read = super::super::inflight::load_inflight_state_read_only_result;
    !matches!(read(provider, channel.get()), Ok(None))
}

/// What the idle end did: `closed` when it closed a pane, `disconnected` when the row left `idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IdleEnd {
    pub(crate) closed: bool,
    pub(crate) disconnected: bool,
}

/// `None` when the row records no Herdr execution. A Retired one only has its row disconnected.
/// `minimum_idle_minutes` is the policy's threshold, read again under the row reservation.
pub(crate) async fn expire_idle_herdr(
    registry: Option<&HealthRegistry>,
    pool: &PgPool,
    session_key: &str,
    minimum_idle_minutes: u64,
) -> Option<Result<IdleEnd, TerminationResult>> {
    use TerminationResult::Refused;
    let row = load(pool, session_key).await.ok()?;
    let HostedRecord::Known(record) = row.record.clone() else {
        return None;
    };
    if !terminate_enabled() {
        return Some(Err(TerminationResult::Disabled));
    }
    let channel = record
        .owner
        .channel_id
        .parse::<u64>()
        .ok()
        .filter(|c| *c != 0);
    let (Some(provider), Some(channel)) = (
        ProviderKind::from_str(&record.owner.provider),
        channel.map(ChannelId::new),
    ) else {
        return Some(Err(Refused("unknown provider or channel".into())));
    };
    if active_dispatch_pg(pool, row.session_id()).await != Ok(None) {
        return Some(Err(Refused("active dispatch".into())));
    }
    if inflight_open(&provider, channel) {
        return Some(Err(Refused(INFLIGHT_OPEN.into())));
    }
    if record.state == HostedState::Retired {
        let disconnected = disconnect(pool, &row, &record).await;
        return Some(Ok(IdleEnd {
            closed: false,
            disconnected,
        }));
    }
    let Some(registry) = registry else {
        return Some(Err(Refused("runtime unavailable".into())));
    };
    let gate = IdleGate {
        pool: pool.clone(),
        row_id: row.session_id(),
        minutes: i32::try_from(minimum_idle_minutes).unwrap_or(i32::MAX),
        held: Mutex::new(None),
    };
    let token = &record.owner.discord_token_hash;
    let runtimes = registry.all_shared_for_provider(&provider).await;
    let Some(shared) = runtimes
        .into_iter()
        .find(|shared| &shared.token_hash == token)
    else {
        let booted = chrono::Utc::now().timestamp() - registry.started_at_unix();
        return Some(match booted >= ORPHAN_AFTER_BOOT_SECS {
            true => end_orphan(pool, (session_key, channel), row, record, gate).await,
            false => Err(Refused("owner runtime not registered yet".into())),
        });
    };
    let resume = match provider {
        ProviderKind::Codex => {
            let record_resume = super::super::commands::effort_restart::record_idle_resume;
            match record_resume(&shared, &provider, channel, &record).await {
                Ok(operation) => Some(operation),
                Err(why) => return Some(Err(Refused(format!("codex thread not kept: {why}")))),
            }
        }
        _ => None,
    };
    let request = OperatorTerminate {
        session_key: session_key.into(),
        execution_nonce: record.execution_nonce.clone(),
    };
    let expected = Expected {
        owner: record.owner.clone(),
        dispatch: None,
        captured_dispatch: false,
    };
    let pool = pool.clone();
    // Detached so a dropped request cannot stop the run between its close and its settlement.
    let run = tokio::spawn(async move {
        let auto = Some(AutoEnd::Idle(gate));
        let scoped = terminate_scoped(
            request,
            shared.clone(),
            &pool,
            Some(expected),
            false,
            false,
            auto,
        );
        let (exclusion, wake) = match scoped.await {
            Ok(ended) => ended,
            Err(kept) => {
                if let Some(operation) = resume {
                    let cancel = super::super::commands::effort_restart::cancel_idle_resume;
                    cancel(&shared, channel, &operation).await;
                }
                return Err(kept);
            }
        };
        let disconnected = disconnect(&pool, &row, &record).await;
        drop(exclusion);
        wake.after_release().await;
        Ok::<_, TerminationResult>(IdleEnd {
            closed: true,
            disconnected,
        })
    });
    let cancelled = TerminationResult::Indeterminate("idle end task cancelled".into());
    Some(run.await.unwrap_or(Err(cancelled)))
}

/// Ends a Claude execution whose owning bot runs no runtime: the recorded local execution is
/// fenced, which shuts every pane write, and closed only while its row is still idle and free.
async fn end_orphan(
    pool: &PgPool,
    (session_key, channel): (&str, ChannelId),
    row: HostedObservation,
    record: HostedExecution,
    gate: IdleGate,
) -> Result<IdleEnd, TerminationResult> {
    use TerminationResult::Refused;
    // A Codex thread is kept only through its owner's input path.
    if record.owner.provider != ProviderKind::Claude.as_str() {
        return Err(Refused("no owner runtime keeps the codex thread".into()));
    }
    let local = crate::config::session_hosts::local_node();
    let executed_here =
        (record.location.as_ref()).is_some_and(|l| l.execution_node == record.owner.owner_node);
    if Some(record.owner.owner_node.clone()) != local || !executed_here {
        return Err(Refused("execution identity differs".into()));
    }
    let provider = ProviderKind::Claude;
    let holds = async {
        gate.reserve().await.is_ok()
            && load(pool, session_key).await.as_ref() == Ok(&row)
            && !inflight_open(&provider, channel)
    };
    let closed = close_unrecorded(&record, holds).await;
    gate.release().await;
    closed?;
    retire_confirmed(pool, &row, &record).await?;
    // The ended execution's composer hold has no turn left to release it.
    use crate::services::claude::herdr_turn::{HoldRelease, release_hold};
    if release_hold(&record.execution_nonce) != HoldRelease::Released {
        tracing::warn!(session_key, "herdr idle end: orphan hold not released");
    }
    let disconnected = disconnect(pool, &row, &record).await;
    Ok(IdleEnd {
        closed: true,
        disconnected,
    })
}

/// Leaves `idle` with the provider session kept, so idle-kill stops selecting the row.
async fn disconnect(pool: &PgPool, row: &HostedObservation, record: &HostedExecution) -> bool {
    match disconnect_retired_pg(pool, row.session_id(), record, None, true).await {
        Ok(disconnected) => disconnected,
        Err(error) => {
            tracing::warn!(row_id = row.session_id(), error, "herdr idle end: row kept");
            false
        }
    }
}
