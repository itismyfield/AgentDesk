//! A read-only Missing watch can only ask the owner to judge the same front again.
use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    self, HostedLookup, HostedLookupKey, HostedRecord, HostedState,
};
use crate::services::session_host::{PaneReading, herdr_endpoints};

pub(in crate::services::discord) fn guard(
    pool: Option<&sqlx::PgPool>,
    host: &TurnHost,
    mailbox: ChannelMailboxHandle,
    front: Option<&Front>,
) -> Option<effect::ProbeGuard> {
    let TurnHost::Herdr(plan) = host else {
        return None;
    };
    let row = plan.row.as_ref()?.clone();
    let HostedRecord::Known(record) = &row.record else {
        return None;
    };
    if record.state != HostedState::Bound || record.owner.provider != "claude" {
        return None;
    }
    let view = herdr_endpoints().view(record)?;
    let expected = front?.clone();
    let pool = pool.cloned();
    Some(Box::new(move |mut read| {
        Box::new(move || {
            let leads = || {
                futures::executor::block_on(
                    mailbox.v2_request(|slot| Ok(slot.as_ref().and_then(|owner| owner.front()))),
                )
                .map(|front| same_front(&expected, front.as_ref()))
            };
            match leads() {
                Ok(true) => {}
                Ok(false) => return Screen::Gone,
                Err(_) => return Screen::Unknown,
            }
            if !matches!(view.read_execution(), PaneReading::Missing) {
                return read();
            }
            match futures::executor::block_on(mailbox.has_active_turn()) {
                Ok(true) => return Screen::Gone,
                Ok(false) => {}
                Err(_) => return Screen::Unknown,
            }
            if let Some(pool) = pool.as_ref() {
                let HostedRecord::Known(record) = &row.record else {
                    return Screen::Unknown;
                };
                let lookup = HostedLookupKey::Canonical {
                    provider: &record.owner.provider,
                    identity: CanonicalSessionIdentity {
                        kind: SessionIdentityKind::DiscordChannel,
                        discord_token_hash: &record.owner.discord_token_hash,
                        channel_id: &record.owner.channel_id,
                    },
                };
                let current = futures::executor::block_on(
                    hosted_execution::load_hosted_execution_pg(pool, lookup),
                );
                let mut retired = record.clone();
                retired.state = HostedState::Retired;
                match current {
                    HostedLookup::Found(current)
                        if current == row
                            || (current.session_id() == row.session_id()
                                && current.record == HostedRecord::Known(retired)) => {}
                    // An unavailable database leaves the decision to the existing owner/CAS path.
                    HostedLookup::Unknown(_) => {}
                    HostedLookup::Found(current)
                        if matches!(current.record, HostedRecord::Unknown(_)) => {}
                    _ => return Screen::Gone,
                }
            }
            // Ready wakes the existing claim/CAS path; this probe never retires, disposes or writes.
            match (
                leads(),
                futures::executor::block_on(mailbox.has_active_turn()),
            ) {
                (Ok(true), Ok(false)) => Screen::Ready,
                (Ok(false), _) | (_, Ok(true)) => Screen::Gone,
                _ => Screen::Unknown,
            }
        })
    }))
}
