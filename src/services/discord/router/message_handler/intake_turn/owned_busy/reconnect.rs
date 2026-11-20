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
                    if result == VerifiedReattach::AwaitingClear {
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
            let root = crate::config::runtime_root();
            let at = record
                .location
                .as_ref()
                .ok_or("recorded pane unavailable")?;
            if record.state != HostedState::Bound
                || record.owner.provider != "claude"
                || record.owner.discord_token_hash != shared.token_hash
                || record.owner.channel_id != channel.get().to_string()
                || record.owner.logical_key != session
                || root.as_deref() != Some(std::path::Path::new(&record.owner.runtime_root))
                || crate::config::session_hosts::local_node().as_deref()
                    != Some(record.owner.owner_node.as_str())
                || record.owner.owner_node != plan.endpoint.execution_node
                || at.execution_node != plan.endpoint.execution_node
                || at.endpoint_config_key != plan.endpoint.config_key
                || at.socket_addr != plan.endpoint.socket_addr
                || at.named_session != plan.endpoint.herdr_session
            {
                return Err("local execution owner or endpoint differs".into());
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
