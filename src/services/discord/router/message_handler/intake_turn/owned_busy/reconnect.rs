//! An uncertain end keeps the original claim available for normal input after reattach.
use super::*;
use crate::services::discord::recovery_engine::herdr_reattach::{VerifiedReattach, continue_owner};
use crate::services::discord::session_transition::SessionTransitionLease;

impl OwnerTurn {
    pub(super) async fn recover_or_hold(
        self,
        shared: &Shared,
        (provider, channel, key, session): (&ProviderKind, ChannelId, Option<&str>, &str),
        host: &TurnHost,
        lease: Option<&SessionTransitionLease<'_>>,
    ) -> Result<(Option<Self>, bool), ()> {
        use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
        use crate::services::claude::herdr_turn::HoldRelease;
        use crate::services::herdr_launch::retire::{self, Scope};
        let acquired = lease
            .is_none()
            .then(|| SessionTransitionLease::try_acquire(shared, channel).ok())
            .flatten();
        let lease = lease.or(acquired.as_ref());
        let mut owner = self;
        let mut may_retire = false;
        if let (Some(lease), TurnHost::Herdr(plan), Some(pool)) =
            (lease, host, shared.pg_pool.as_ref())
            && let Some(row) = &plan.row
        {
            match continue_owner(shared, pool, lease, row).await.0 {
                result @ (VerifiedReattach::Live | VerifiedReattach::AwaitingClear) => {
                    owner = owner
                        .prepare_restarted(shared, (provider, channel, Some(session), key), lease)
                        .await?;
                    if result == VerifiedReattach::AwaitingClear || owner.late.restarted.is_none() {
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
                VerifiedReattach::Ended => may_retire = true,
                VerifiedReattach::Held => {}
            }
        }
        let recovery = async {
            if !may_retire {
                return Err("pane identity or end unproven".to_owned());
            }
            let TurnHost::Herdr(plan) = host else {
                return Err("host is not a local Herdr execution".to_string());
            };
            let row = plan.row.as_ref().ok_or("hosted row unavailable")?;
            let HostedRecord::Known(record) = &row.record else {
                return Err("hosted record unreadable".into());
            };
            // The reattach verdict already checked this Bound row's canonical owner and locality.
            if record.owner.logical_key != session {
                return Err("the observed execution belongs to another route".into());
            }
            let pool = shared.pg_pool.as_ref().ok_or("database unavailable")?;
            crate::services::discord::herdr_terminate::root_replaced::current(pool, row).await?;
            let retired = retire::observed(pool, row, Scope::Missing)
                .await
                .map_err(|e| format!("retire: {e:?}"))?;
            if retired.hold != HoldRelease::Released {
                return Err("retired execution input hold could not be released".into());
            }
            crate::services::tui_prompt_dedupe::withhold_herdr_execution(
                session,
                Some(&record.execution_nonce),
            );
            owner.recovery.mark();
            let mut expected = record.clone();
            expected.state = HostedState::Retired;
            match crate::services::turn_host::for_turn(Some(pool), provider, channel.get(), key)
                .await
            {
                TurnHost::Herdr(next)
                    if next
                        .row
                        .as_ref()
                        .is_some_and(|row| row.record == HostedRecord::Known(expected.clone())) =>
                {
                    Ok(())
                }
                _ => Err("fresh Herdr execution is not admissible after retirement".into()),
            }
        }
        .await;
        match recovery {
            Ok(()) => Ok((Some(owner), false)),
            Err(reason) => {
                let observation =
                    claude_handoff::observe_busy(channel.get(), session, host, &shared.token_hash);
                let watch = recoverable_probe(
                    host,
                    &observation,
                    shared.pg_pool.as_ref(),
                    &owner.mailbox,
                    Some(owner.front()),
                );
                hold(
                    (shared, observation.state() == BusyState::Held),
                    &owner.mailbox,
                    channel,
                    owner.front(),
                    watch,
                )
                .await;
                owner.defer_before_submit(&reason);
                Err(())
            }
        }
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
                    let (owner, _) = owner
                        .recover_or_hold(
                            shared,
                            (&shared.provider, channel, key, fixture.logical()),
                            &host,
                            None,
                        )
                        .await
                        .expect("the live pane keeps the front in the ordinary input path");
                    let owner = owner.expect("an idle pane returns the original to its submitter");
                    assert!(owner.continues_herdr());
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
