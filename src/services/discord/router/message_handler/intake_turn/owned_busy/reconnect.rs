//! Resolve a failed Busy registration against the exact hosted execution.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedRecord, HostedState,
};
use crate::services::discord::herdr_terminate::root_replaced;
use crate::services::discord::recovery_engine::herdr_reattach::{VerifiedReattach, continue_owner};
use crate::services::discord::session_transition::SessionTransitionLease;
use crate::services::herdr_launch::retire::{self, RetireRefusal, Scope};
use crate::services::turn_host::HerdrTurnPlan;
use RecoveryFailure::{Refused, Reobserve, Retry};

pub(in crate::services::discord) const ROUTE_DIFFERS: &str =
    "local execution owner or endpoint differs";

pub(in crate::services::discord) fn route_matches(
    shared: &Shared,
    channel: ChannelId,
    session: &str,
    plan: &HerdrTurnPlan,
    record: &HostedExecution,
) -> Option<bool> {
    let root = crate::config::runtime_root()?;
    let node = crate::config::session_hosts::local_node()?;
    let at = record.location.as_ref()?;
    Some(
        record.state == HostedState::Bound
            && record.owner.provider == "claude"
            && record.owner.discord_token_hash == shared.token_hash
            && record.owner.channel_id == channel.get().to_string()
            && record.owner.logical_key == session
            && root == std::path::Path::new(&record.owner.runtime_root)
            && node == record.owner.owner_node
            && record.owner.owner_node == plan.endpoint.execution_node
            && at.execution_node == plan.endpoint.execution_node
            && at.endpoint_config_key == plan.endpoint.config_key
            && at.socket_addr == plan.endpoint.socket_addr
            && at.named_session == plan.endpoint.herdr_session,
    )
}

impl OwnerTurn {
    pub(in crate::services::discord) async fn discard_busy_recovery(
        self,
        shared: &Shared,
        channel: ChannelId,
        reason: String,
    ) {
        let failure = super::super::super::start_failure::Failure::owner(&self);
        let result = failure
            .discard(Some(shared.clone()), channel, reason)
            .await
            .map(|_| ());
        let _ = self.settled.send(result.map_err(effect::unsent));
    }

    pub(super) async fn recover_or_discard(
        self,
        shared: &Shared,
        route: (&ProviderKind, ChannelId, Option<&str>, &str),
        host: &TurnHost,
        (observation, lease): (&BusyObservation, Option<&SessionTransitionLease<'_>>),
    ) -> Result<(Option<Self>, bool), ()> {
        let (provider, channel, key, session) = route;
        let acquired = lease
            .is_none()
            .then(|| SessionTransitionLease::try_acquire(shared, channel).ok())
            .flatten();
        let lease = lease.or(acquired.as_ref());
        let candidate = async {
            let lease = lease.ok_or_else(|| Retry("channel transition unavailable".into()))?;
            let TurnHost::Herdr(plan) = host else {
                return Err(Retry("local Herdr execution unavailable".into()));
            };
            let row = plan
                .row
                .as_ref()
                .ok_or_else(|| Retry("hosted row unavailable".into()))?;
            let HostedRecord::Known(record) = &row.record else {
                return Err(Retry("hosted record unreadable".into()));
            };
            let pool = shared
                .pg_pool
                .as_ref()
                .ok_or_else(|| Retry("database unavailable".into()))?;
            match route_matches(shared, channel, session, plan, record) {
                Some(true) => Ok((lease, pool, row, record)),
                Some(false) => {
                    root_replaced::current(pool, row).await.map_err(Retry)?;
                    Err(Refused(ROUTE_DIFFERS.into()))
                }
                None => Err(Reobserve("local pane identity unavailable".into())),
            }
        }
        .await;
        let recovery = match candidate {
            Ok((lease, pool, row, record)) => {
                match continue_owner(shared, pool, lease, row).await.0 {
                    result @ (VerifiedReattach::Live | VerifiedReattach::AwaitingClear) => {
                        let owner = self
                            .prepare_restarted(
                                shared,
                                (provider, channel, Some(session), key),
                                lease,
                            )
                            .await?;
                        if result == VerifiedReattach::AwaitingClear
                            || owner.late.restarted.is_none()
                        {
                            return Ok((Some(owner), false));
                        }
                        return Box::pin(OwnerTurn::early(
                            shared,
                            (provider, channel, Some(session), key),
                            Some(owner),
                            false,
                            Some(lease),
                        ))
                        .await;
                    }
                    VerifiedReattach::Held => {
                        Err(Reobserve("pane identity or end unproven".into()))
                    }
                    VerifiedReattach::Ended => {
                        async {
                            root_replaced::current(pool, row).await.map_err(Retry)?;
                            let retired = retire::observed(pool, row, Scope::Missing)
                                .await
                                .map_err(|error| {
                                    let reason = format!("retire: {error:?}");
                                    match error {
                                        RetireRefusal::Unproven(_)
                                        | RetireRefusal::ProviderRunning => Reobserve(reason),
                                        RetireRefusal::Changed(_)
                                        | RetireRefusal::NoLocalEndpoint => Retry(reason),
                                        RetireRefusal::NoRow(_) => Refused(reason),
                                    }
                                })?;
                            if retired.hold
                                != crate::services::claude::herdr_turn::HoldRelease::Released
                            {
                                return Err(Refused(
                                    "retired execution input hold could not be released".into(),
                                ));
                            }
                            crate::services::tui_prompt_dedupe::withhold_herdr_execution(
                                session,
                                Some(&record.execution_nonce),
                            );
                            self.recovery.mark();
                            let mut expected = record.clone();
                            expected.state = HostedState::Retired;
                            match crate::services::turn_host::for_turn(
                                Some(pool),
                                provider,
                                channel.get(),
                                key,
                            )
                            .await
                            {
                                TurnHost::Herdr(next)
                                    if next.row.as_ref().is_some_and(|row| {
                                        row.record == HostedRecord::Known(expected.clone())
                                    }) =>
                                {
                                    Ok(())
                                }
                                _ => Err(Refused(
                                    "fresh Herdr execution is not admissible after retirement"
                                        .into(),
                                )),
                            }
                        }
                        .await
                    }
                }
            }
            Err(error) => Err(error),
        };
        let (reason, probe) = match recovery {
            Ok(()) => return Ok((Some(self.finish_restart_identity().await?), false)),
            Err(Refused(reason)) => {
                self.discard_busy_recovery(shared, channel, reason).await;
                return Err(());
            }
            Err(Reobserve(reason)) => (
                reason,
                recovery_probe(
                    host,
                    observation,
                    shared.pg_pool.as_ref(),
                    &self.mailbox,
                    self.front(),
                ),
            ),
            Err(Retry(reason)) => (reason, None),
        };
        // A changed observation schedules the usual input route, where continuation runs again.
        hold(
            (shared, HoldKind::Identity),
            &self.mailbox,
            channel,
            self.front(),
            Some(probe.unwrap_or_else(|| Box::new(|| Screen::Ready))),
        )
        .await;
        self.defer_before_submit(&reason);
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::recovery_engine::herdr_reattach::tests::{
        CHANNEL, named_registry, run, snapshot_labels,
    };
    use crate::services::herdr_launch::retire::{self, RetireRefusal, Scope};

    #[test]
    fn live_named_pane_survives_unproven_retirement_after_registration_failed_pg() {
        run(
            ProviderKind::Claude,
            "s2-unproven",
            false,
            |fixture, runtime| {
                let _named = named_registry(&fixture);
                snapshot_labels(
                    &fixture,
                    &[format!(
                        "{}#{}",
                        fixture.logical(),
                        fixture.record.execution_nonce
                    )],
                );
                runtime.block_on(async {
                    let shared = &fixture.shared;
                    let pool = shared.pg_pool.as_ref();
                    let channel = ChannelId::new(CHANNEL);
                    let key = Some(fixture.logical());
                    let host = crate::services::turn_host::for_observation(
                        pool,
                        &shared.provider,
                        CHANNEL,
                        key,
                    )
                    .await;
                    let TurnHost::Herdr(plan) = &host else {
                        panic!("expected the observed local host")
                    };
                    let row = plan.row.as_ref().unwrap();
                    assert!(matches!(
                        retire::observed(pool.unwrap(), row, Scope::Missing).await,
                        Err(RetireRefusal::Unproven(_))
                    ));
                    let (owner, _, _) =
                        super::super::super::restarted::tests::claimed(&fixture).await;
                    assert_eq!(owner.mailbox.has_active_turn().await, Ok(false));
                    let original = owner.front().clone();
                    let suspended = original.clone();
                    owner
                        .mailbox
                        .v2_request(move |slot| {
                            let owner = slot.as_mut().unwrap();
                            let at = chrono::Utc::now().timestamp_millis().max(0) as u64;
                            owner.hold_readiness(&suspended, "busy_readiness_unproven", at)?;
                            owner.hold_identity(&suspended)?;
                            assert!(
                                owner.json()?["readiness_holds"][0]["identity_held_at_ms"].is_u64()
                            );
                            Ok(())
                        })
                        .await
                        .unwrap();
                    let observation = claude_handoff::observe_busy(
                        CHANNEL,
                        fixture.logical(),
                        &host,
                        &shared.token_hash,
                    );
                    let (owner, _) = owner
                        .recover_or_discard(
                            shared,
                            (&shared.provider, channel, key, fixture.logical()),
                            &host,
                            (&observation, None),
                        )
                        .await
                        .expect("the live pane keeps the front in the ordinary input path");
                    let owner = owner.expect("an idle pane returns the original to its submitter");
                    assert!(owner.continues_herdr());
                    let snapshot = owner
                        .mailbox
                        .v2_request(|slot| slot.as_ref().unwrap().json())
                        .await
                        .unwrap();
                    assert!(snapshot["readiness_holds"].is_null());
                    assert_eq!(owner.front().0, original.0);
                    assert_eq!(owner.front().1.message_id, original.1.message_id);
                    assert_eq!(owner.front().1.text, original.1.text);
                    assert_eq!(
                        shared
                            .tmux_watchers
                            .owner_channel_for_tmux_session(fixture.logical()),
                        Some(channel)
                    );
                    assert!(
                        fixture.rig.sends().is_empty(),
                        "the observation does not send the input"
                    );
                });
                runtime.block_on(fixture.close());
            },
        );
    }
}
