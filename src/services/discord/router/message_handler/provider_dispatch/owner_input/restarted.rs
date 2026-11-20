//! A restart observation follows the existing front into the owner's reserve and provider effect.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedObservation, HostedRecord, HostedState,
};
use crate::services::discord::herdr_terminate::root_replaced;
use crate::services::discord::recovery_engine::herdr_reattach::{VerifiedReattach, verify_owner};
use crate::services::discord::session_transition::SessionTransitionLease;
use crate::services::discord::turn_presence::activity::{Activity, activity_now};
use crate::services::herdr_launch::clear_epoch::{self, Epoch};
use crate::services::session_host::{HerdrPaneView, PaneProvider, PaneReading, herdr_endpoints};

#[derive(Default)]
pub(super) struct LateRead {
    pub(super) own_pane: bool,
    pub(super) clear_binding: bool,
    pub(super) restarted: Option<ExistingFront>,
}

impl LateRead {
    pub(super) fn set_headless(&mut self, own_pane: bool, clear_binding: bool) {
        self.own_pane = own_pane;
        self.clear_binding = clear_binding;
    }

    pub(super) fn awaiting_clear(&self) -> bool {
        self.restarted
            .as_ref()
            .is_some_and(|front| front.0.awaiting_clear)
    }
}

pub(super) struct ExistingFront(Box<Existing>);
struct Existing {
    pool: sqlx::PgPool,
    row: HostedObservation,
    pane: HerdrPaneView,
    epoch: Epoch,
    awaiting_clear: bool,
    cancel: Option<std::sync::Arc<CancelToken>>,
}

type Route<'a> = (
    &'a ProviderKind,
    ChannelId,
    Option<&'a str>,
    Option<&'a str>,
);

impl OwnerTurn {
    pub(in crate::services::discord) fn awaiting_restarted_clear(&self) -> bool {
        self.late.awaiting_clear()
    }

    /// Requires the turn's existing lease and claim. The caller supplies the lease only for a
    /// routed owner turn eligible for restart continuation.
    pub(in crate::services::discord) async fn prepare_restarted(
        mut self,
        shared: &Shared,
        (provider, channel, logical, key): Route<'_>,
        transition: &SessionTransitionLease<'_>,
    ) -> Result<Self, ()> {
        let host = crate::services::turn_host::for_observation(
            shared.pg_pool.as_ref(),
            provider,
            channel.get(),
            key,
        )
        .await;
        let row = match host {
            TurnHost::Tmux | TurnHost::Return(_) => return Ok(self),
            TurnHost::Herdr(plan) => match plan.row {
                None => return Ok(self),
                Some(row) if matches!(&row.record, HostedRecord::Legacy) => return Ok(self),
                Some(row) if matches!(&row.record, HostedRecord::Known(record) if record.state == HostedState::Retired) =>
                {
                    return Ok(self);
                }
                row => row,
            },
            TurnHost::Refused(_) => None,
        };
        let observed = row.as_ref().and_then(|row| {
            let HostedRecord::Known(record) = &row.record else {
                return None;
            };
            (Some(record.owner.logical_key.as_str()) == logical).then_some((row, record))
        });
        let candidate = match (shared.pg_pool.as_ref(), observed) {
            (Some(pool), Some((row, record)))
                if crate::services::herdr_admission::check().is_ok() =>
            {
                let epoch = epoch_for(record, channel);
                match verify_owner(shared, pool, transition, row).await {
                    // The normal provider path still has to adjudicate recovery.
                    VerifiedReattach::Ended => return Ok(self),
                    result @ (VerifiedReattach::Live | VerifiedReattach::AwaitingClear) => {
                        let cancel = self.mailbox.cancel_token().await.ok().flatten();
                        herdr_endpoints().view(record).map(|pane| {
                            ExistingFront(Box::new(Existing {
                                pool: pool.clone(),
                                row: row.clone(),
                                pane,
                                epoch,
                                awaiting_clear: result == VerifiedReattach::AwaitingClear,
                                cancel,
                            }))
                        })
                    }
                    VerifiedReattach::Held => None,
                }
            }
            _ => None,
        };

        // Claude's early path takes a current Busy grant for a person. The other starts wait for
        // Idle with the same front, while the drive's existing Busy path can take its own grant.
        let wait_for_idle = candidate.is_some();
        if let Some(candidate) = candidate
            && (candidate.0.awaiting_clear
                || person_claude(&self, provider)
                || activity_now(shared, provider, channel).await.activity == Activity::Idle)
        {
            self.late.restarted = Some(candidate);
            return Ok(self);
        }

        let probe = probe(
            shared.clone(),
            provider.clone(),
            channel,
            key.map(str::to_owned),
            (row, wait_for_idle),
        );
        hold_start(&self.mailbox, channel, self.front()).await;
        held_watch::arm(channel.get(), probe);
        held_watch::readiness::hold(drive(shared, channel), self.front()).await;
        self.defer_before_submit("herdr_restart_held");
        Err(())
    }
}

fn person_claude(owner: &OwnerTurn, provider: &ProviderKind) -> bool {
    *provider == ProviderKind::Claude && owner.origin.is_person()
}

fn exact_execution(record: &HostedExecution, reading: &PaneReading) -> bool {
    matches!((reading, &record.expected),
        (PaneReading::Present { root, provider: PaneProvider::Execution(process) }, Some(expected))
            if root == &expected.root && process == &expected.provider_process)
}

fn epoch_for(record: &HostedExecution, channel: ChannelId) -> Epoch {
    clear_epoch::snapshot(
        &record.owner.provider,
        &record.owner.discord_token_hash,
        channel.get(),
    )
}

/// A watch reads the cause of the hold and wakes the same mailbox front. The next claim alone can
/// publish or submit. A changed row lets the owner reconsider a new placement or completed clear.
fn probe(
    shared: Shared,
    provider: ProviderKind,
    channel: ChannelId,
    key: Option<String>,
    (previous, wait_for_idle): (Option<HostedObservation>, bool),
) -> held_watch::ScreenProbe {
    let view = previous.as_ref().and_then(|row| match &row.record {
        HostedRecord::Known(record) => herdr_endpoints().view(record),
        _ => None,
    });
    let handle = tokio::runtime::Handle::current();
    Box::new(move || {
        if crate::services::herdr_admission::check().is_err() {
            return Screen::Blocked;
        }
        let host = handle.block_on(crate::services::turn_host::for_observation(
            shared.pg_pool.as_ref(),
            &provider,
            channel.get(),
            key.as_deref(),
        ));
        let TurnHost::Herdr(plan) = host else {
            return match host {
                TurnHost::Tmux | TurnHost::Return(_) => Screen::Ready,
                _ => Screen::Unknown,
            };
        };
        if plan.row != previous {
            return Screen::Ready;
        }
        let (Some(view), Some(row)) = (&view, &plan.row) else {
            return Screen::Unknown;
        };
        let HostedRecord::Known(record) = &row.record else {
            return Screen::Unknown;
        };
        if record.state != HostedState::Bound {
            return Screen::Unknown;
        }
        let reading = view.read_execution();
        if view.restart_ended(record, &reading) {
            return Screen::Ready;
        }
        if !exact_execution(record, &reading) {
            return Screen::Unknown;
        }
        if !wait_for_idle {
            let logical = &record.owner.logical_key;
            let owner = shared.tmux_watchers.owner_channel_for_tmux_session(logical);
            return if owner.is_none_or(|owner| owner == channel) {
                Screen::Ready
            } else {
                Screen::Blocked
            };
        }
        match handle
            .block_on(activity_now(&shared, &provider, channel))
            .activity
        {
            Activity::Idle => Screen::Ready,
            Activity::Busy => Screen::Blocked,
            Activity::Unknown => Screen::Unknown,
        }
    })
}

pub(super) fn register(
    late: &LateRead,
    turn: &StreamingTurn<'_>,
    origin: InputOrigin,
    mailbox: &ChannelMailboxHandle,
    front: &Front,
    legacy: impl FnOnce() -> Result<Registration, String>,
) -> Result<Registration, String> {
    if late.awaiting_clear() {
        return legacy();
    }
    owned_busy::late(turn, origin, mailbox, front, legacy)
}

pub(super) fn pass(
    existing: Option<ExistingFront>,
    turn: &StreamingTurn<'_>,
    recovery: super::super::start_failure::MissingRecovery,
    registration: &mut Registration,
) {
    registration.recovery = recovery;
    registration.restarted = existing.map(|mut front| {
        front.0.cancel = Some(turn.cancel.clone());
        front
    });
}

/// Called after reserve and before the provider's own atomic first-key gate. A replacement
/// launch cannot consume a continuation's old claim. The pump still owns every receipt.
pub(super) fn guard(
    front: Option<ExistingFront>,
    attempt: &Value,
    leaf: effect::Leaf,
) -> effect::Leaf {
    let Some(front) = front else { return leaf };
    let front = front.0;
    let target = &attempt["target"];
    let same_execution = match &front.row.record {
        HostedRecord::Known(record) => record.location.as_ref().is_some_and(|location| {
            target["host"] == "herdr"
                && target["owner"] == record.owner.logical_key
                && target["root"] == location.socket_addr
                && target["execution_nonce"] == record.execution_nonce
        }),
        _ => false,
    };
    Box::new(move |frame| {
        if !same_execution {
            return Ok(effect::refused("herdr_restart_target_changed".into(), true));
        }
        #[cfg(test)]
        if tests::omit_guard() {
            return leaf(frame);
        }
        let cancel = front.cancel.as_deref();
        if crate::services::provider::cancel_requested(cancel)
            || cancel.is_some_and(|token| token.native_stop.receipt_sink().stopped_before_key())
            || crate::services::herdr_admission::check().is_err()
        {
            return Ok(effect::refused(
                "herdr_restart_stopped_before_key".into(),
                true,
            ));
        }
        let current = tokio::runtime::Handle::current()
            .block_on(root_replaced::current(&front.pool, &front.row));
        let Some(permit) = front.epoch.begin_effect() else {
            return Ok(effect::refused("herdr_restart_epoch_changed".into(), true));
        };
        let still_exact = match &front.row.record {
            HostedRecord::Known(record) => front
                .pane
                .observe_execution()
                .is_some_and(|(_, reading)| exact_execution(record, &reading)),
            _ => false,
        };
        if current.is_err() || !still_exact {
            // A changed or dead predecessor will be adjudicated by a new owner pass.
            scope::record_preparation_watch(Box::new(|| Screen::Ready));
            return Ok(effect::refused(
                "herdr_restart_observation_changed".into(),
                true,
            ));
        }
        let result = leaf(frame);
        drop(permit);
        result
    })
}

#[cfg(test)]
#[path = "restarted_tests.rs"]
mod tests;
