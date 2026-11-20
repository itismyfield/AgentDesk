use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
pub(in crate::services::discord::turn_bridge) use crate::services::tui_o::cutover::BodyClaim;
use crate::services::tui_o::cutover::{self, IdentityError};

type Gate = fn(u64, Option<RuntimeHandoffKind>) -> Result<bool, IdentityError>;

/// TUI bodies follow destination membership on any gateway; uncertain selected identities are
/// held. Only read: a pending adoption stays pending, and a body claims at its transport instead.
pub(in crate::services::discord::turn_bridge) fn bridge_o_body_peek_decision(
    channel_id: ChannelId,
    inflight: &InflightTurnState,
    can_deliver_directly: bool,
) -> Result<bool, IdentityError> {
    let gate: Gate = cutover::peek_o_owns_tui_output_for_channel;
    decision(channel_id, inflight, can_deliver_directly, gate)
}

/// The claim a bridge body sends under, with the identity these decisions resolve.
pub(in crate::services::discord::turn_bridge) fn bridge_body_claim(
    channel_id: ChannelId,
    inflight: &InflightTurnState,
    can_deliver_directly: bool,
) -> BodyClaim<'static> {
    BodyClaim::new(channel_id.get(), kind(channel_id, inflight))
        .direct(can_deliver_directly)
        .for_turn(inflight.turn_nonce.as_deref())
}

fn kind(channel_id: ChannelId, inflight: &InflightTurnState) -> Option<RuntimeHandoffKind> {
    (inflight.channel_id == channel_id.get())
        .then_some(inflight.runtime_kind)
        .flatten()
}

fn decision(
    channel_id: ChannelId,
    inflight: &InflightTurnState,
    can_deliver_directly: bool,
    o_owns: Gate,
) -> Result<bool, IdentityError> {
    let owned = o_owns(channel_id.get(), kind(channel_id, inflight))?;
    Ok(cutover::o_keeps_body(
        channel_id.get(),
        owned,
        can_deliver_directly,
    ))
}

/// A bridge body sent under `claim`; O owning the channel or a held identity sent nothing, which
/// reads as a failed send.
pub(super) async fn claimed_send<T, F: std::future::Future<Output = Result<T, String>>>(
    claim: Option<BodyClaim<'_>>,
    send: impl FnOnce() -> F,
) -> Result<T, String> {
    cutover::BodySend::flatten(
        cutover::claim_then_send_at(cutover::BodySite::BridgeLongChunks, claim, send).await,
    )
}

/// The transport's own result, or `None` when O owns the channel or its identity is held and
/// nothing was sent.
pub(in crate::services::discord::turn_bridge) async fn sent_under<T, F>(
    claim: Option<BodyClaim<'_>>,
    send: impl FnOnce() -> F,
) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    match cutover::claim_then_send_at(cutover::BodySite::BridgeTerminal, claim, send).await {
        Ok(cutover::BodySend::Sent(sent)) => Some(sent),
        Ok(cutover::BodySend::OwnedByO) | Err(_) => None,
    }
}

#[cfg(test)]
mod first_admission_tests {
    use super::*;
    use crate::services::tui_o::channel_policy::{Adoption, BootChannels};
    use crate::services::tui_o::cutover::{IdentityError, claim_then_send, test_override};
    use crate::test_support::wait::{WAIT_CAP, await_within};

    #[tokio::test]
    async fn bridge_factory_retains_only_its_own_admitted_turn_before_legacy_claim() {
        let channel = 8_165_301;
        let config = serde_json::from_value(serde_json::json!({"server":{}, "agents":[{"id":"first", "name":"first", "channels":{"codex":{"id":channel.to_string(), "runtime":"tui"}}}], "tui_o":{"writer":{"channels":[channel], "late_adopt":true}}})).unwrap();
        let boot = BootChannels::validate(&config)
            .unwrap()
            .seeded(&config, |_, _| {
                Ok(std::collections::BTreeMap::from([(
                    channel,
                    Adoption::Pending,
                )]))
            })
            .unwrap();
        let candidate = boot.candidate(channel).unwrap().clone();
        let boot_guard = test_override::force_boot(boot);
        let token = crate::services::provider::CancelToken::new();
        let admission = candidate
            .admit_first(channel, token.turn_nonce().unwrap(), "hi", "first", || {
                Ok(0)
            })
            .unwrap();
        let mut row = InflightTurnState::new(
            crate::services::provider::ProviderKind::Codex,
            channel,
            None,
            7,
            8,
            9,
            "hi".into(),
            None,
            None,
            None,
            None,
            0,
        );
        row.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        row.turn_nonce = Some("older-turn".into());
        // An unadopted TUI channel's body is held; the admission does not hold another turn's.
        let old = claim_then_send(
            Some(bridge_body_claim(ChannelId::new(channel), &row, true)),
            || async { 1 },
        );
        let old = await_within(old, WAIT_CAP).await;
        assert_eq!(old.unwrap_err(), IdentityError::AwaitingAdoption);
        row.turn_nonce = token.turn_nonce().map(str::to_owned);
        let mut own = Box::pin(claim_then_send(
            Some(bridge_body_claim(ChannelId::new(channel), &row, true)),
            || async { 2 },
        ));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(own.as_mut(), &mut context).is_pending());
        assert_eq!(candidate.sends(), (0, 0));
        candidate.invalidate_first(token.turn_nonce(), None);
        let own = await_within(own, WAIT_CAP).await;
        assert_eq!(own.unwrap_err(), IdentityError::AwaitingAdoption);
        assert_eq!(
            candidate.sends(),
            (0, 0),
            "Legacy sends no unadopted TUI body"
        );
        drop(admission);
        drop(boot_guard);
    }

    // Nothing adopts the admitted output within its one budget: the waiting body claim ends
    // refused with the deadline as its reason, and Legacy sends none of it.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn bridge_body_of_an_admission_nothing_adopts_is_refused_at_its_budget() {
        use crate::services::tui_o::channel_policy::output_disposition::{BUDGET, Reason};
        let channel = 8_165_302;
        let config = serde_json::from_value(serde_json::json!({"server":{}, "agents":[{"id":"first", "name":"first", "channels":{"codex":{"id":channel.to_string(), "runtime":"tui"}}}], "tui_o":{"writer":{"channels":[channel], "late_adopt":true}}})).unwrap();
        let pending = |_: &_, _| {
            Ok(std::collections::BTreeMap::from([(
                channel,
                Adoption::Pending,
            )]))
        };
        let boot = BootChannels::validate(&config).unwrap();
        let boot = boot.seeded(&config, pending).unwrap();
        let candidate = boot.candidate(channel).unwrap().clone();
        let boot_guard = test_override::force_boot(boot);
        let token = crate::services::provider::CancelToken::new();
        let nonce = token.turn_nonce().unwrap();
        let admitted = candidate.admit_first(channel, nonce, "hi", "first", || Ok(0));
        let admission = admitted.unwrap();
        let codex = crate::services::provider::ProviderKind::Codex;
        let mut row = InflightTurnState::new(
            codex,
            channel,
            None,
            7,
            8,
            9,
            "hi".into(),
            None,
            None,
            None,
            None,
            0,
        );
        row.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        row.turn_nonce = Some(nonce.to_owned());
        let claim = bridge_body_claim(ChannelId::new(channel), &row, true);
        let mut own = Box::pin(claim_then_send(Some(claim), || async { 2 }));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let second = std::time::Duration::from_secs(1);
        tokio::time::advance(BUDGET - second).await;
        let waiting = std::future::Future::poll(own.as_mut(), &mut context);
        assert!(waiting.is_pending(), "the body waits through its budget");
        tokio::time::advance(second).await;
        let refused = await_within(own, WAIT_CAP).await;
        assert!(matches!(refused, Err(IdentityError::AdoptionRefused)));
        assert_eq!(
            (candidate.peek(), candidate.sends()),
            (Adoption::Refused, (0, 0))
        );
        let reason = cutover::codex_first_rejection(channel, nonce);
        assert_eq!(reason, Some(Reason::Deadline));
        drop(admission);
        drop(boot_guard);
    }
}
